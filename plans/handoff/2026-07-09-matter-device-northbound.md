# Handoff: Northbound adapters & the live Matter-device bridge

Date: 2026-07-09. Branch: `matter_homekit` (all work **uncommitted** — see [Git state](#git-state)).

This session introduced a **northbound / southbound** distinction to domiform's
adapter model and built, end to end, a `matter_device` adapter that exposes a
domiform device as a **live Matter accessory** on the LAN — successfully paired
into Apple Home from an iPhone. This doc is the map for continuing that work.

The authoritative design record is
[`docs/design/northbound-adapters.md`](../design/northbound-adapters.md); read it
first. This handoff summarizes what shipped, why, and — importantly — the
**current gaps**. It references files rather than restating code.

---

## 1. The concept (why this exists)

domiform already had **southbound** adapters (zigbee2mqtt, matter, zwavejs): domiform
is the controller, the physical devices are the source of truth. This session added
the dual — **northbound** adapters: domiform is the source of truth, and a consumer
(Apple Home / Google / Alexa via Matter) is upstream. A northbound adapter *exposes*
devices already declared in the YAML; it invents nothing, so the single-source-of-truth
tenet holds.

The distinction lives on the trait (`Polarity`), **not** in a separate module —
see §1 of the design doc for the full "why not a `frontends/` module" rationale.
The `ClockAdapter` was the existing proof that a non-southbound adapter is
legitimate; a northbound adapter is its mirror.

Transport choice: we first tried HomeKit/HAP via `hap-rs` (abandoned, won't
compile) and pivoted to **`rs-matter`** — domiform appears as a native Matter
device, which *all* major ecosystems commission. See the design doc's "Transport
pivot" note.

---

## 2. What shipped, by phase

Phases 0/1/2/2b are all **done**. The design doc has a detailed per-phase log
(search `✅`); condensed here:

- **Phase 0 — `Event::RequestedChange`** ([`src/model.rs`](../../src/model.rs)):
  the canonical inbound path for *any* northbound frontend. A consumer's desired
  *state* (`CapabilityState`, not a `Command`) that the engine lowers to the same
  command a rule would emit. Handling + the `command_for_requested_change`
  state→command map are in [`src/engine.rs`](../../src/engine.rs) (in `drain`,
  intercepted before rule matching / fold — it's an intent, not a report). Tests:
  [`tests/requested_change.rs`](../../tests/requested_change.rs).

- **Phase 1 — the northbound seam** (no protocol yet):
  - Engine went multi-observer (`observers: Vec<Box<dyn Observer>>`, `add_observer`,
    `notify` helper) so a northbound adapter can watch every `state_folded`.
    [`src/engine.rs`](../../src/engine.rs), [`src/observe.rs`](../../src/observe.rs).
  - `trait NorthboundAdapter: Adapter + Observer` (blanket impl) held in a
    dedicated `Engine::northbound` list — ticked *and* fed `state_folded`.
    [`src/adapters/mod.rs`](../../src/adapters/mod.rs).
  - `AdapterPlugin` gained `polarity()`, `expose_spec()`, `build_northbound()`,
    and a `NorthboundCtx`. [`src/adapters/plugin.rs`](../../src/adapters/plugin.rs).
  - Config: adapters *reference* devices via `expose: all | [names]`; the resolver
    validates names, suppresses `E_UNUSED_ADAPTER` for northbound adapters, and the
    builder wires them with no dispatch slot.
    [`src/compile/resolve.rs`](../../src/compile/resolve.rs),
    [`src/compile/mod.rs`](../../src/compile/mod.rs).
  - `type: mock_northbound` proves the whole seam with **zero protocol dependency**.
    [`src/adapters/mock_northbound.rs`](../../src/adapters/mock_northbound.rs),
    tests [`tests/northbound.rs`](../../tests/northbound.rs).

- **Phase 2 — the `matter_device` adapter + mapping**
  ([`src/adapters/matter_device/mod.rs`](../../src/adapters/matter_device/mod.rs)):
  `type: matter_device`, `Polarity::Northbound`, a `MatterTransport` seam
  (mirroring z2m's `MqttTransport`) so the pure `CapabilityState`↔cluster mapping is
  unit-tested with an in-memory fake. Tests:
  [`tests/matter_device.rs`](../../tests/matter_device.rs).

- **Phase 2b — the live rs-matter node**
  ([`src/adapters/matter_device/real_transport/`](../../src/adapters/matter_device/real_transport/)):
  a real Matter node on a background thread; **paired into Apple Home this session.**
  Detailed below.

---

## 3. How the live node works (the load-bearing part)

Directory [`src/adapters/matter_device/real_transport/`](../../src/adapters/matter_device/real_transport/):

- **`mod.rs`** — spawns a background thread that owns the entire (non-`Send`)
  rs-matter object graph and runs it under one `block_on(select4(...))`, exactly
  like z2m's network thread. The engine talks to it over `std::mpsc` channels
  (`ChannelTransport`). `publish` mirrors folded engine state into the node; a
  controller write becomes an `Event::RequestedChange` drained by `tick`, with a
  `Waker` nudge. Structure follows rs-matter's own v0.2 `dimmable_light` /
  `onoff_light` examples closely (the README example is **stale** — trust the repo
  examples, cloned during the session).
- **`hooks.rs`** — the domiform-specific part: impls of rs-matter's `OnOffHooks` /
  `LevelControlHooks` that bridge controller reads/writes to the channels + shared
  cells.
- **`mdns.rs`** — LAN discovery glue, ported from rs-matter's example `common/mdns.rs`.

Key facts a new session must know:

- **rs-matter is `no_std`/async-first, v0.2.0, pure-Rust** (no `-sys`/openssl — the
  static-musl story survives). Its API diverges from the README; use the cloned
  repo examples as ground truth.
- **Commissioning uses rs-matter's *test* credentials** (passcode `20202021`,
  discriminator `3840`, test DAC). Correct for dev/bring-up; **production needs real
  device-attestation certs** (gap #4 below).
- **Persistence = model B (sidecar).** `DirKvBlobStore` at the resolved runtime
  storage path + `matter.load_persist` on boot → a paired controller survives
  restarts. The "config is reproducible; commissioned identity is runtime data"
  philosophy is documented in [`README.md`](../../README.md) ("Configuration vs.
  runtime state") and the design doc §6.

### Config surface (all landed)

See [`examples/matter_device.yaml`](../../examples/matter_device.yaml) and
[`schema/domiform.schema.json`](../../schema/domiform.schema.json):

- `system.runtime_storage_path` — dir for runtime state; defaults to the **config
  file's directory** (resolved by the host, not cwd). Threaded via `NorthboundCtx`.
- `matter_device.expose` — `all` or `[names]`.
- `matter_device.runtime_storage_file` — optional fabric-store path override;
  default `<runtime_storage_path>/homekit.<hash>.state` (stable FNV-1a of adapter
  name).
- `matter_device.interface` — optional mDNS interface override (e.g. `en0`).

---

## 4. Real-world gotchas already solved (don't re-debug these)

All discovered by actually running it; fixes are in the code with comments:

- **Logger**: `main.rs` now inits `env_logger` so rs-matter's `log`-based output
  (the pairing QR!) is visible. It also silences `rs_matter::im::invoker=off` by
  default — that target logs *benign* `UnsupportedCluster`/`UnsupportedAttribute`
  errors when a controller probes optional clusters during commissioning. Those are
  expected, not bugs.
- **mDNS on macOS** ([`mdns.rs`](../../src/adapters/matter_device/real_transport/mdns.rs)):
  (a) auto-selection **skips VPN/tunnel interfaces** (Tailscale `utun*`) that can't
  join multicast; (b) `SO_REUSEPORT` to share port 5353 with the OS mDNS responder;
  (c) a **native IPv4 socket on v4-only interfaces** because macOS rejects IPv4
  multicast on a dual-stack IPv6 socket. Also: macOS Local Network privacy must be
  granted to the terminal app.
- **MQTT reconnect storm**: the z2m background thread busy-spun on connection-refused
  (thousands of lines/sec). Now throttled with a 5s backoff + dedup —
  [`src/adapters/zigbee2mqtt.rs`](../../src/adapters/zigbee2mqtt.rs) (real transport
  module, the `connection.iter()` loop).
- **LevelControl↔OnOff coupling error**: a fresh light with no brightness set made
  rs-matter's coupling read `current_level() == None` and raise `Error::Failure` on
  toggle. Fixed by seeding the level cell to full brightness (254) —
  [`hooks.rs`](../../src/adapters/matter_device/real_transport/hooks.rs) (`LightCells::default`).

---

## 5. Status update (later same-day session) — gaps #1 and #2 resolved

The two headline gaps below were **closed** in a follow-up session; the design
notes here are corrected. See design doc Phase 2b/2c for the authoritative record.

- **✅ Multi-device (was gap #1, "single device only").** Shipped, but **not** the
  way this handoff sketched. `DynamicNode<'_, N>` does exist and is used for the
  *metadata*, but the "macro/recursion-generated N-slot handler chain with a
  `MAX_MATTER_DEVICES` cap" was **tried and abandoned**: a chain nesting ~5·N
  `ChainedHandler` layers makes rustc's layout pass for `InteractionModel`'s async
  body blow up (OOM at N=32 even with `recursion_limit` raised). The working design
  is **dispatch shims** (`bridge.rs`): one `AsyncHandler` per stateful cluster,
  matched on *any* endpoint, owning a `Vec` of rs-matter's real per-endpoint
  handlers and routing by `ctx.endpt()`. Fixed chain depth (~6), no cap-driven type
  explosion, full reuse of rs-matter's OnOff/Level logic. `E_MATTER_SINGLE_DEVICE`
  is gone; `E_TOO_MANY_EXPOSED` guards a *soft* cap (`MAX_MATTER_DEVICES = 64`).

- **⚠️ Device names — attempted, NOT working with Apple Home (was gap #2).** We do
  everything correctly on the accessory side: each bridged endpoint has its own
  `BridgedDeviceBasicInformation` cluster and `hooks::BridgedFacet` serves the
  domiform device name via **all** the naming attributes — `NodeLabel`,
  `ProductName`, and `VendorName` (verified by logging: Apple Home *does* read them
  and we *do* return e.g. "living_room_lamp"). **Apple Home ignores them anyway** and
  displays its own device-type defaults ("Light", "Light 2", "Light 3"); the bridge
  hub shows as "Matter Accessory" (the root `BasicInformation.node_label` is private
  in `MatterState` with no public setter, so that one is genuinely unfixable from our
  side). This is a **known controller-side limitation for bridged Matter devices**,
  not a bug we can fix — corroborated by user reports across ecosystems (e.g. the
  Aqara forum thread "NodeLabel is not taken into account for bridges EndPoints").
  The user renames accessories manually in Home. **Do not sink more time into this
  from the accessory side** — the fix, if any, is Apple's. Left as-is; the naming
  attributes are served because it's spec-correct and a better-behaved controller
  (or a future Home update) would use them.

### Remaining open items

1. **Capability coverage.** Only Switch→OnOff and Brightness→LevelControl are wired
   in the live node. `capability_is_exposable` deliberately admits **only** those
   two — Occupancy/Battery/Color/CT are *not* projected until cluster handlers
   exist (admitting them earlier advertised sensors as On/Off lights). To add:
   Color→ColorControl Hue/Sat (reuse [`src/color.rs`](../../src/color.rs)),
   ColorTemperature→ColorControl (mireds), Occupancy→OccupancySensing,
   Battery→PowerSource — and wire `device_type_for` into `build_node` when doing so.

2. **Production attestation (low priority / not a coding task).** We use rs-matter's
   *test* Device Attestation Certificate (`TEST_DEV_ATT`), which chains to a test PAA
   not in the real CSA trust store. The **only** user-facing effect is the
   "uncertified accessory" warning during pairing, with an **"Add Anyway"** button;
   after that the device is fully functional. This is the standard posture for
   open-source Matter projects (Home Assistant, ESPHome, etc.) and "click Add Anyway"
   is a fine long-term default. A *real* DAC is not a code change — it requires **CSA
   membership, a real Vendor ID, and per-product certification**, i.e. a
   business/compliance step. Don't spend a coding session on this.

3. **Housekeeping.** A stray `runtime/` dir (a fabric store from local test runs)
   is untracked at repo root (gitignored). Default fabric-store path is now
   `matter.<hash>.state` (was `homekit.*`).

4. **Known consistency gaps (not yet fixed).** Controller writes update Matter
   cells optimistically before southbound echo; no startup state sync into the
   node; node-thread death is log-only. See audit notes in session that abandoned
   the HAP sidecar initiative.

---

## 6. Verification & how to run

- Full suite + clippy are **green** (17 test groups, 0 warnings). Run: `cargo test`,
  `cargo clippy --all-targets`.
- To run the bridge: `cargo run -- -c examples/matter_device.yaml`, scan the printed
  QR (or enter the pairing code) into Apple Home. Each exposed device appears as a
  bridged accessory under its domiform name. The example points z2m at a broker that
  may be down — that single `[mqtt] connection error` line is expected and harmless;
  the Matter side is independent.
- **Note on `MAX_MATTER_DEVICES`:** the chain is fixed-depth, so this is a *soft*
  cap only — safe to raise. Do **not** revive the per-device `.chain()` unroll
  approach; it OOMs rustc (see Phase 2c). If a chain-depth change is ever needed,
  build with a memory watchdog — a runaway monomorphization can exhaust host RAM.
- The pure state↔level conversions and mapping are unit-tested
  ([`tests/matter_device.rs`](../../tests/matter_device.rs)); the live node itself
  is only testable by pairing a real controller.

---

## 7. Git state

Nothing is committed. New files: the `matter_device/` module, `mock_northbound.rs`,
three test files, `examples/matter_device.yaml`, the `docs/` tree (design +
this handoff). Modified: engine/model/observe/main/plugin/resolve/compile,
`Cargo.toml` (new deps: `rs-matter`, `async-io`, `futures-lite`, `embassy-futures`,
`embassy-sync`, `embassy-time-queue-utils`, `socket2`, `if-addrs`, `rand` 0.8,
`log`, `env_logger`), README, schema, and two existing tests (updated for
`add_observer`). Before committing: handle the `runtime/` dir (gap #5) and decide
whether the new pre-release `rs-matter` dependency is acceptable to pin.
