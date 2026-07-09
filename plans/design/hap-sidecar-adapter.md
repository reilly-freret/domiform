# Design note: a HAP sidecar northbound adapter

Status: **proposed, not started.** Research + architecture only. Date: 2026-07-09.

## TL;DR for a new agent

domiform already has a `matter_device` northbound adapter (see
[`northbound-adapters.md`](./northbound-adapters.md) and the
[handoff](../handoff/2026-07-09-matter-device-northbound.md)) that exposes
domiform devices to Apple Home / Google / Alexa as a native **Matter bridge**
(rs-matter, in-process). It works — devices pair and are controllable — **but Apple
Home ignores our bridged device names** and shows generic "Light" / "Matter
Accessory" labels. That is a well-documented *controller-side* limitation for
bridged Matter devices, not a bug we can fix from the accessory side (see the
naming caveat in the handoff).

This doc proposes an **alternative northbound adapter, `hap_bridge`**, that speaks
Apple's older, proprietary **HAP** (HomeKit Accessory Protocol) via a **HAP-python
`HttpBridge` sidecar process**. HAP naming *is* honored and auto-updated by Apple
Home (this is how Home Assistant's native HomeKit integration and Homebridge get
correct, live-updating names). The cost: HAP is **Apple-only** (no Google/Alexa).

The two adapters coexist behind the same seam: pick `matter_device` for
cross-ecosystem reach, `hap_bridge` for Apple-first naming fidelity.

---

## Why HAP, and why a sidecar (the research)

### The naming problem is HAP-vs-Matter, not us

Home Assistant's native HomeKit integration and Homebridge speak **HAP** — Apple's
original protocol, which predates Matter. In HAP each accessory has a `Name`
characteristic that Apple Home **displays verbatim and re-reads on reconnect**, so
renaming upstream and reloading "just works." Our `matter_device` adapter speaks
**Matter bridging**, where the name lives in `BridgedDeviceBasicInformation`
(`NodeLabel` / `ProductName`); Apple Home's Matter implementation treats those as a
commissioning-time seed / user-owned local state and does **not** keep them in sync.
We verified by logging that Home reads our attributes and we return the right
values — Home just ignores them. Corroborated by user reports across ecosystems
(e.g. Aqara forum: "NodeLabel is not taken into account for bridges EndPoints").

### There is no usable Rust HAP accessory library

Surveyed 2026-07-09:

- **`hap-rs` / `hap`** (ewilken) — latest release **Feb 2020**, ~4+ years stale, 17
  open issues, no bridge docs. This is the crate the first session tried and
  abandoned ("won't compile"). Not viable.
- **`hap-controller`** — the *controller* side (what a hub does), wrong direction.
- **`fmckeogh/homekit`** — embedded / `no_std`, niche, not a bridge server.

Building a HAP accessory bridge from scratch in Rust is a large, security-sensitive
project (SRP pairing, session crypto, TLV8, the accessory HTTP server + mDNS). Out
of scope. **Every mature HAP accessory implementation is another-language,
another-process** — so real HAP naming means running a sidecar regardless.

### The mature HAP servers (all sidecar-shaped)

- **HAP-python** (ikalchev) — Python. **Has a purpose-built `HttpBridge`**: remote
  processes drive accessories over HTTP with JSON. This is the recommended target.
- **HAP-NodeJS** (homebridge) — Node; the engine under Homebridge. Very mature, but
  no first-class "drive from another process over HTTP" surface — you'd write a thin
  Node shim.
- **brutella/hap** (Go) — supports multi-accessory bridges + `FsStore` pairing
  persistence; would also need a small driver shim.

HAP-python's `HttpBridge` is the least-glue option: it exists precisely to let an
external process own the accessory logic.

---

## Why this does NOT duplicate the source of truth

The early objection to a Homebridge sidecar was "duplicate sources of truth." That
concern applies to **southbound** sidecars (zwave-js-ui, otbr+matter-js): those own
physical devices, so a second declaration there is a real second source of truth.

A HAP bridge sidecar is **northbound** — the mirror image. It owns **nothing**. It
is a pure projection surface: domiform declares the accessories, their names, and
pushes every state change; the sidecar just renders them to Apple Home and relays
taps back. This is structurally identical to what `matter_device` already is — it
only moves the rendering out of process. domiform remains the sole source of truth
because the sidecar declares no devices of its own.

---

## Architecture: reuse the existing northbound seam

The `matter_device` adapter is built around a transport trait that is already the
right abstraction for "some northbound renderer":

```rust
// src/adapters/matter_device/mod.rs
pub trait MatterTransport {
    fn publish(&mut self, device: DeviceId, state: &CapabilityState); // domiform state -> renderer
    fn poll(&mut self) -> Vec<(DeviceId, CapabilityState)>;           // controller writes -> RequestedChange
}
```

A `hap_bridge` adapter is the same shape with the protocol swapped. Concretely:

1. **New adapter `type: hap_bridge`** — mirror `matter_device`'s plugin
   (`Polarity::Northbound`, `expose: all | [names]`, no dispatch slot,
   `E_UNUSED_ADAPTER` suppressed). Most of the plugin/resolver wiring is copy-adapt
   from `matter_device`. The `ExposedDevice` computation (id, label, exposable
   capabilities) is reusable as-is.
2. **A `HapSidecarTransport`** implementing the same publish/poll contract, talking
   HTTP to the HAP-python `HttpBridge`:
   - **On build / config:** tell the sidecar the accessory set + **names** (the
     `expose` list — same data `matter_device` already computes). This is where HAP
     naming fidelity comes from: the name is an accessory field the sidecar
     publishes and Apple Home honors + keeps in sync.
   - **On `state_folded` (publish):** POST the changed characteristic values
     (HAP-python's format: `{ "aid": N, "services": { "<Service>": { "<Char>":
     value } } }`). Same hook point as `MatterTransport::publish`.
   - **On a HomeKit write (poll):** the sidecar POSTs the desired state back to a
     small HTTP endpoint domiform exposes (or domiform long-polls); domiform lowers
     it to `Event::RequestedChange` — **the exact inbound path Phase 0 already
     built**. An app tap and a wall switch stay indistinguishable to the engine.
3. **Capability ↔ HAP service/characteristic mapping** — the pure, unit-testable
   part (mirror `matter_device`'s `CapabilityState` ↔ cluster mapping):
   - Switch → `Switch`/`Lightbulb.On`
   - Brightness → `Lightbulb.Brightness`
   - Color / ColorTemperature → `Lightbulb` Hue/Sat / ColorTemperature (reuse
     [`src/color.rs`](../../src/color.rs))
   - Occupancy → `OccupancySensor`; Battery → `BatteryService`
4. **Runtime / persistence** — the sidecar owns HAP pairing state (a directory,
   like `matter_device`'s fabric store). Same "config is reproducible; commissioned
   identity is runtime data" posture (README "Configuration vs. runtime state").
   Threads through `NorthboundCtx.runtime_storage_dir` exactly like the Matter one.

Reused as-is: `Event::RequestedChange` + `command_for_requested_change`
(`engine.rs`), the multi-observer fan-out (`observe.rs`), the northbound plugin hooks
(`AdapterPlugin::{polarity, expose_spec, build_northbound}`, `NorthboundCtx`), and
the resolver's `expose` validation. Only the transport implementation + the
capability→HAP mapping are new.

### Sidecar lifecycle & contract (to be specified)

Open questions for the implementer to pin down in a spike:

- **How is the sidecar launched?** Options: domiform spawns/supervises the
  HAP-python process (like a managed child), or the user runs it (docker-compose,
  like zwave-js-ui) and domiform just connects. Managed child is friendlier; user-run
  matches the existing sidecar pattern. Decide based on how the other sidecars are
  deployed.
- **The wire contract** between domiform and the sidecar: accessory declaration
  (set + names + which services), state push (domiform → sidecar), and control
  relay (sidecar → domiform). HAP-python's `HttpBridge` defines the push half;
  the declaration + relay half is a thin protocol we design (a small JSON over
  HTTP, or a tiny Python driver script on top of HAP-python that domiform talks to).
- **Naming refresh:** confirm the sidecar re-publishes accessory names on config
  reload and Apple Home picks them up live (this is the whole point — verify it in
  the spike before building the adapter).

---

## Recommended first step: a de-risking spike

Before writing any Rust, prove the mechanism end to end (a couple of hours):

1. Stand up HAP-python's `HttpBridge` with two static accessories (a switch + a
   dimmable light).
2. Pair it into Apple Home. Confirm both show with the **names you set**.
3. Change an accessory's name in the driver, reload, and confirm Apple Home updates
   the displayed name live — **no manual rename**. This is the behavior
   `matter_device` cannot deliver; confirming it here validates the entire
   direction.
4. Drive a characteristic (turn the switch on) from the driver and confirm Home
   reflects it; tap in Home and confirm the driver observes the write.

If the spike confirms live naming + bidirectional control, wire the
`HapSidecarTransport` against the existing seam per the architecture above.

---

## Tradeoffs to keep in view

| | `matter_device` (current, in-process) | `hap_bridge` (proposed, sidecar) |
|---|---|---|
| Naming in Apple Home | broken (controller quirk) | **works, auto-updates** |
| Ecosystems | Apple + Google + Alexa | **Apple only** |
| Deployment | single static Rust binary | +1 sidecar process (like zwave-js-ui) |
| Single source of truth | clean (northbound) | clean (northbound; sidecar owns nothing) |
| New work | done | `HapSidecarTransport` + capability→HAP map + sidecar contract |

These are **complementary**, not competing: keep `matter_device` for cross-ecosystem
reach; add `hap_bridge` for Apple-first fidelity. Both are northbound adapters behind
the same seam; a user picks per their setup.

---

## Sources

- hap-rs (stale): <https://github.com/ewilken/hap-rs>
- HAP-python + HttpBridge: <https://github.com/ikalchev/HAP-python> ·
  <https://hap-python.readthedocs.io/en/latest/>
- HAP-NodeJS (Homebridge engine): <https://github.com/homebridge/HAP-NodeJS>
- brutella/hap (Go): <https://github.com/brutella/hap>
- Matter bridged-name limitation (representative report): Aqara forum, "NodeLabel is
  not taken into account for bridges EndPoints"
