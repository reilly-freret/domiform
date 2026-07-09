# Design: Northbound adapters & the Matter-device bridge

Status: in progress. This introduces a **northbound / southbound** distinction to
the adapter model and uses a Matter-device bridge (`type: matter_device`) — a
native Matter node embedded via `rs-matter` — as the first northbound adapter and
the proving ground for the concept.

> **Transport pivot (was: HAP / `hap-rs`).** The original plan embedded `hap-rs`
> to speak HomeKit Accessory Protocol directly. On investigation `hap-rs` proved
> abandoned (last release 2022, still `0.1.0-pre`, and it no longer even resolves
> on a current toolchain — a `links` conflict in its transitive mDNS stack). We
> pivoted to **`rs-matter`** (project-chip, Apache-2.0, `0.2.0` published 2026-06,
> pure-Rust with no `-sys`/openssl deps so the static musl binary is preserved).
> This is a strict *improvement*, not a workaround: domiform appears as a native
> **Matter device**, which Apple Home, Google Home and Alexa all commission
> directly — so "expose my devices to the Home app" generalizes to "expose them to
> any Matter controller." The northbound/southbound abstraction (Phases 0–1) was
> re-audited against `rs-matter`'s actual device/threading model and needed no
> rework; the two decisions we were least sure of (`RequestedChange` carrying
> `CapabilityState`, and the tick+observer combined trait) are the ones `rs-matter`
> *confirms* — a controller writes cluster *attribute values* (desired state), and
> the node runs its own `block_on(select…)` loop on a background thread exactly
> like z2m's network thread.

## 1. The concept: two polarities of one trait

The `Adapter` trait (`src/adapters/mod.rs`) already spans two data-flow shapes;
we're only *naming* a split that exists, not inventing one.

- **Southbound** (existing: `zigbee2mqtt`, `matter`, `zwavejs`): domiform is the
  *controller*; the protocol is *downstream*. The physical devices are the source
  of truth. `dispatch(Command)` sends outward; inbound protocol reports become
  `Event::StateReported` pulled in via `tick()`.
- **Northbound** (new: `homekit`, later a REST/web/voice frontend): domiform is
  the *source of truth*; the consumer is *upstream*. The adapter **exposes**
  devices already declared in the YAML and turns consumer input (a Home-app tap)
  back into canonical `Event`s.

The `ClockAdapter` is the existing proof that a non-southbound adapter is
legitimate: it binds no physical device, accepts no commands, and only pushes
state inward ("time is an adapter"). A northbound adapter is the *dual* of the
clock — it accepts no device commands of its own and only reads state outward +
originates events inward.

### Why not a separate `frontends/` module?

A natural instinct is to give northbound adapters their own module sibling to
`adapters/`, since they're "frontends," not device drivers. We deliberately do
**not**, for three reasons:

1. **`Adapter` is a lifecycle contract, not a data-flow role.** Everything that
   depends on it — `Engine::adapters: Vec<Box<dyn Adapter>>`, the `tick()` /
   `next_wake()` pump, the `Waker`/host loop, and the `AdapterPlugin`/`PLUGINS`
   registry that maps a config `type:` to a plugin — is direction-agnostic. A
   HomeKit bridge needs *all* of it (it registers via `type: homekit`, is built by
   `build_engine`, is `tick()`ed to drain pending HAP writes, participates in
   `next_wake`). Northbound and southbound share 100% of the lifecycle and differ
   only in data-flow direction. That is why the write path needs zero new engine
   plumbing.

2. **Direction doesn't partition cleanly — bidirectionality is the common case,
   already in the tree.** The southbound device adapters are themselves
   bidirectional: z2m `dispatch`es commands out *and* produces `StateReported` in.
   The clock is inbound-only. Split by direction and `zigbee2mqtt` has no obvious
   home — you get `adapters/` (bidirectional-ish), `frontends/` (northbound), and a
   fuzzy boundary that generates "where does this file go?" churn on every future
   PR. The module system would encode a distinction the type system doesn't
   enforce.

3. **The distinction belongs where it has teeth: the trait, not the directory.**
   `Polarity` on `AdapterPlugin` (§4) is the *one* place the compiler/builder
   actually branches on direction (~10 lines of `build_engine`: northbound gets the
   exposed-device set + `add_observer` wiring; southbound gets its bound devices). A
   bidirectional adapter is a `Polarity` variant, not a filing problem — the enum
   degrades gracefully where a module split does not.

**Revisit only if** northbound adapters stop sharing the `Adapter` lifecycle —
e.g. they become long-lived services that don't go through `tick()`, own their own
async runtime, and touch the engine only via a queue. Then they'd have earned a
sibling module because they'd no longer implement the same contract. Until that
evidence exists, splitting forces the taxonomy question prematurely; moving
`homekit.rs` into a `frontends/` module later is a cheap file move + second
registry, whereas starting split forks the "one registry, one line in `mod.rs`,
logic in the adapter's own file" contributor story that is domiform's best
architectural property.

### Why this must not compromise the tenets

- **Static / declarative single source of truth**: the bridge exposes *only*
  devices declared in the YAML; it invents nothing at runtime. It is a
  *projection* of the static device set into HAP accessories — no second device
  registry that can drift (this is the whole reason we embed `hap-rs` instead of
  feeding a separate Homebridge process; see `northbound-adapters-rationale`
  below).
- **Offline**: HAP is LAN-local mDNS; no cloud.
- **Arbitrary protocols**: a Zigbee bulb, a Z-Wave lock and a Matter plug appear
  as uniform HAP accessories *because they are already uniform `Device`s with
  `CapabilityKind`s*. `CapabilityKind::{Switch,Brightness,Color,ColorTemperature,
  Occupancy,Battery}` maps near-1:1 to HAP services — evidence the abstraction is
  right, not a coincidence.

## 2. The two seams the code lacks (everything else already exists)

Reading the runtime, the **write path is already fully supported** and only the
**read/fan-out path** needs a new (small) seam.

### 2a. Write path (Home app → engine): NO new engine plumbing

This reuses the *exact* mechanism `Zigbee2MqttAdapter` uses for an inbound button
press:

```
HAP thread: characteristic write ──▶ queue pending write + Waker::wake()
host loop: wakes ──▶ engine.advance() ──▶ tick_adapters()
homekit tick(): drains pending writes ──▶ returns Vec<Event> (canonical)
```

So a Home-app tap and a physical wall switch become **indistinguishable to the
engine** — both arrive as inbound `Event`s via `tick()`. The bridge never calls
`dispatch` on the engine.

**The one model decision:** what `Event` does a human-originated change become?
Today `Event` has only *reports* (`StateReported`, `OccupancyChanged`, `Action`),
no "a human requested this." A HAP write of `On=true` on a switch must turn into
the same thing a rule would emit (`Command::SetSwitch`) — but adapters speak
`Event`, not `Command`, and the engine's rule/dispatch machinery consumes
`Event`s. Options, to decide in Phase 0:

- **(A) New `Event::RequestedChange { device, desired: CapabilityState }`** — the
  engine folds/dispatches it as an intent. Cleanest and reusable by *every* future
  northbound frontend; costs a new `Event` variant + handling in `drain`.
- **(B) Reuse the `Action` path** — model each writable characteristic as a
  declared device event whose firing a rule maps to a command. Zero engine change
  but pushes per-device rule boilerplate onto the user; a poor fit for continuous
  values (brightness).

**Recommendation: (A).** It is the load-bearing choice the whole northbound
surface shares; getting it right once makes HomeKit mostly plumbing and makes a
future REST/web/voice frontend trivial. It keeps the invariant intact: the engine
still only reacts to `Event`s; adapters still only speak the canonical
vocabulary.

### 2b. Read / fan-out path (engine state → bridge): reuse `Observer`

The bridge must mirror *every* state change, not just echoes of commands aimed at
its own devices. The engine already emits exactly this signal:
`engine.rs:` `self.observer.state_folded(device, &state)` in `fold_state`.

**Decision (chosen): the bridge registers as an `Observer`.** A northbound adapter
is "an `Observer` that also happens to be an `Adapter`." This adds no new engine
concept. Two honest consequences to handle:

1. `Observer` is currently **single** (`set_observer` replaces). The engine must
   hold **multiple** observers so the bridge can coexist with `StderrObserver`.
   Change `observer: Box<dyn Observer>` → `observers: Vec<Box<dyn Observer>>`;
   `set_observer` keeps its meaning for the trace observer, add `add_observer`.
   Every `self.observer.foo(..)` call fans to all.
2. Widen the `observe.rs` module/trait doc: it is no longer *only* a debug/trace
   seam — `state_folded` is now also the northbound state-fan-out seam. Note this
   explicitly so the coupling is intentional and documented, not incidental.

The bridge, as an `Observer`, receives `state_folded(device, state)` on its own
thread-free path (engine is single-threaded), maps `(DeviceId, CapabilityState)`
→ the corresponding HAP characteristic value, and updates its `hap-rs`
accessory server. No polling, no store reference threaded through `Adapter`.

## 3. Config model (chosen: bridge references devices)

The northbound adapter binds **zero devices of its own** and instead *references*
devices declared under their real southbound adapters — preserving single source
of truth (no device re-declared, no per-device scatter):

```yaml
adapters:
  zigbee: { type: zigbee2mqtt, url: mqtt://localhost:1883 }
  home:
    type: homekit
    pin: "031-45-154"      # HAP setup code
    name: "Domiform"       # bridge name shown in Home app
    expose: all            # or an explicit list: [nightstand_l, hallway_light]
devices:
  nightstand_l: { adapter: zigbee, capabilities: [switch, brightness] }
  hallway_light: { adapter: zigbee, capabilities: [switch] }
```

This requires the compiler/build path (which currently assumes each device binds
to exactly one adapter) to allow an adapter that **references** other adapters'
devices. Concretely:

- `resolve`: validate that every name in `expose` (when not `all`) resolves to a
  declared device; emit a diagnostic otherwise. Do **not** re-bind those devices —
  they stay owned by their southbound adapter in `device_to_adapter`.
- `build_engine` (`compile/mod.rs`): the homekit adapter's `by_adapter[i]` slice is
  empty (it owns no devices). Its `AdapterPlugin::build` instead needs the
  *referenced* `DeviceDef`s (name, capabilities, metadata for HAP service/room) —
  so `build` for a northbound plugin takes the exposed device set, resolved from
  `expose`, rather than the "devices bound to me" set. This is the one build-path
  change; keep it additive so southbound plugins are unaffected.

## 4. Trait shape

Introduce a marker so the compiler and builder can treat the two polarities
differently without a per-adapter branch, mirroring the existing `AdapterPlugin`
registry philosophy (one line in `mod.rs`, logic in the adapter's own file):

```rust
// adapters/plugin.rs
pub enum Polarity { Southbound, Northbound }

pub trait AdapterPlugin: Sync + std::fmt::Debug {
    fn type_tag(&self) -> &'static str;
    fn polarity(&self) -> Polarity { Polarity::Southbound } // default: unchanged
    // ... existing validate_config / validate_device / build ...
}
```

- Southbound: `build(config, devices_bound_to_me, waker)` — unchanged.
- Northbound: receives the **exposed** device set + a `Waker` (for the write
  path) and returns a `Box<dyn Adapter>` that *also* is registered as an
  `Observer` (the builder wires `add_observer` for northbound adapters).

Keeping `build`'s signature stable and passing the resolved exposed-device set for
northbound adapters (rather than changing the trait shape per polarity) is
preferred if it stays clean; the plan validates that during Phase 1.

## 5. Implementation phases

Each phase compiles, is independently reviewable, and doesn't regress southbound
adapters or the deterministic core.

**Phase 0 — canonical inbound-command event (model only). ✅ DONE.**
- Added `Event::RequestedChange { device, desired: CapabilityState }`. **Decision
  resolved:** the variant carries a desired `CapabilityState` (not a `Command`) —
  a northbound adapter speaks pure *state* (a written characteristic = a desired
  value) and never constructs commands; the engine owns the state→command
  translation in one place (`Engine::command_for_requested_change`), reusable by
  every future northbound frontend.
- Handled in `engine.rs::drain`: intercepted before rule matching and before
  `fold_state`, translated to the same `Command` a rule would emit, dispatched one
  causal hop from the request. Deliberately *not* folded into the store (an intent,
  not a report — the device echo folds) and *not* a rule trigger. Non-writable
  desired states (`Occupancy`/`Battery`/`TimeOfDay`/`SunUp`) yield no command and
  are a harmless no-op.
- Tests (`tests/requested_change.rs`): a request drives the bound adapter exactly
  as the equivalent rule command does (asserted by driving both and comparing);
  brightness maps without transition; non-writable states no-op; the request does
  not fold before the echo; an unbound target fails like any command. No wall clock
  touched — replay/determinism unaffected.

**Phase 1 — multi-observer + `Polarity` + config plumbing (no HAP yet). ✅ DONE.**
- Engine: `observer: Box<dyn Observer>` → `observers: Vec<Box<dyn Observer>>`;
  `set_observer` → **`add_observer`** (all call sites, incl. `main.rs` and two
  tests, updated). Fan-out via a free `notify(&mut [Box<dyn Observer>], f)` helper
  so call sites keep a disjoint immutable borrow (`self.rules`/`self.state`) across
  the notification. `observe.rs` doc widened: `state_folded` is now also the
  northbound state-fan-out seam, not only tracing.
- **Decision — combined trait, separate list (refined from the doc's original
  "register as an Observer" sketch).** A northbound adapter must be *both* an
  `Adapter` (its `tick` drains consumer input; `next_wake` joins the host sleep)
  *and* an `Observer` (receives `state_folded`). Rather than a single `Observer`
  registration (which wouldn't get ticked) or `Rc<RefCell>` gymnastics to register
  one object twice, added `trait NorthboundAdapter: Adapter + Observer` with a
  blanket impl, and a dedicated `Engine::northbound: Vec<Box<dyn NorthboundAdapter>>`.
  The engine ticks that list in `tick_adapters`, includes it in `next_wake_delay`,
  and fans `state_folded` to it in `fold_state` (via `fan_state_folded`, which hits
  both `observers` and `northbound`). Northbound adapters bind no devices, so they
  are never a dispatch target and never enter `device_to_adapter`.
- `AdapterPlugin::polarity()` (default `Southbound`) + `expose_spec()` (parses the
  adapter's own `expose` syntax → `ExposeSpec::{All, Named}`) + `build_northbound()`
  (default `None`; `build()` defaulted to a loud `unreachable!` so a northbound
  plugin needn't supply an unused southbound builder).
- `resolve`: validates `expose` names against declared devices
  (`E_UNKNOWN_EXPOSED_DEVICE`), warns on empty exposure (`E_EMPTY_EXPOSE`), and
  **suppresses `E_UNUSED_ADAPTER` for northbound adapters** (binding no devices is
  correct for them). `build_engine`: dispatches on `polarity()`, resolves the
  exposed `DeviceDef`s via `exposed_devices`, and registers northbound adapters
  with `add_northbound` (no dispatch slot; a `NO_SLOT` sentinel + `debug_assert`
  guards that no device ever binds to one).
- **`type: mock_northbound`** adapter added (`src/adapters/mock_northbound.rs`,
  one line in the registry): an in-memory `MockNorthbound` that records every
  mirrored `state_folded` and drains test-queued writes into `RequestedChange` on
  `tick`. Proves the whole seam with zero HAP dependency.
- Tests (`tests/northbound.rs`, 7): folds reach the mirror; a command echo (not
  the request) is what's mirrored; a queued consumer "tap" drives the bound device
  on `tick`; northbound coexists with a second observer (Vec regression guard);
  a northbound adapter binds no devices and compiles without `E_UNUSED_ADAPTER`;
  `expose: [ghost]` and a bad `expose` keyword are compile errors. Verified a
  northbound config passes `--check` **offline** (no HAP contacted). Full suite +
  clippy green; southbound behavior unchanged.

**Phase 2 — the `matter_device` adapter (`rs-matter`), behind a transport seam.
✅ MAPPING + WIRING DONE; live node deferred to Phase 2b.**
- New `src/adapters/matter_device.rs` (`type: matter_device`) with `PLUGIN` + one
  line in the registry. Polarity `Northbound`; `expose_spec`/`build_northbound`
  implemented; it is `Adapter + Observer` → `NorthboundAdapter` via the blanket
  impl. `rs-matter` + `async-io`/`futures-lite` added to `Cargo.toml` (pure Rust,
  no `-sys` — static binary preserved; verified it resolves and compiles).
- **`MatterTransport` trait** is the seam (mirrors z2m's `MqttTransport`):
  `publish(device, &CapabilityState)` mirrors state outward into the node's
  attribute DB; `poll() -> Vec<(DeviceId, CapabilityState)>` returns controller
  attribute writes. The pure `CapabilityState` ↔ Matter mapping —
  `device_type_for` (capabilities → Matter device type), `capability_is_exposable`
  (which caps project to a cluster; time/sun/IR never do) — is unit-tested with
  `InMemoryMatter` (a cloneable shared-handle fake), **no node, no network**.
- Adapter facets: `state_folded` → `transport.publish` (outward mirror);
  `tick` → drains `transport.poll()` into `Event::RequestedChange` (a controller
  write and a physical wall switch become indistinguishable to the engine).
- Tests (`tests/matter_device.rs`, 11): device-type classification; exposability
  filter; fold→publish (through the shared handle); publish filters unexposable
  caps; controller-write→RequestedChange on tick (drained after); dispatch is a
  permanent failure (binds no devices); full end-to-end controller-write→bound
  device through the engine; config compiles/builds; `expose: [ghost]` errors.
  Full suite + clippy green; example `examples/matter_device.yaml` validates via
  `--check` **offline**; JSON schema + README updated.

**Phase 2b — the live `rs-matter` node. ✅ DONE.**
- `real_transport::connect` now spawns a **live `rs-matter` node on a background
  thread** (`src/adapters/matter_device/real_transport/`): a `Matter` object,
  `InteractionModel` + `DefaultResponder`, built-in mDNS, all under one
  `block_on(select4(…))`. The engine talks to it over channels: `publish` mirrors
  folded state into the hook cells; a controller attribute write
  (`OnOffHooks::set_on_off` / `LevelControlHooks::set_device_level`) is forwarded to
  the engine + a `Waker` nudge, drained by `tick` into `Event::RequestedChange`.
  Only the hook impls (bridging to the channels) are domiform-specific.
- **Bridge topology (`bridge.rs`).** The node is a **Matter bridge**: root (ep 0) +
  an **Aggregator** endpoint (ep 1, `DEV_TYPE_AGGREGATOR`) + one **bridged**
  endpoint per exposed device (ep 2+i, `DEV_TYPE_BRIDGED_NODE` + the light device
  type). This is what the crate's `bridge` example demonstrates. Two things fall out
  of it:
  - **Device names — served but NOT honoured by Apple Home.** Each bridged endpoint
    carries its own `BridgedDeviceBasicInformation` cluster, and `hooks::BridgedFacet`
    serves the domiform device name via `NodeLabel`, `ProductName`, and `VendorName`.
    Verified by logging that Apple Home reads these and we return the right values —
    yet Home displays its own device-type defaults ("Light", "Light 2", …) and shows
    the hub as "Matter Accessory". This is a **known controller-side limitation for
    bridged Matter devices** (widely reported across ecosystems), not something we
    can fix from the accessory: we serve the attributes because it is spec-correct,
    but the displayed name is the controller's call and the user renames manually.
    *(The root `BasicInformation.node_label` — a separate, per-fabric field Home
    might use for the hub — is private in `MatterState` with no public setter, so it
    is unreachable regardless.)* **Do not spend more time on this from our side.**
  - **N devices, fixed-depth handler chain.** See Phase 2c — this shipped too.
- **Persistence (model B) wired:** `DirKvBlobStore` at the resolved per-adapter
  state dir + `matter.load_persist` on boot — a commissioned controller survives a
  restart.
- **Commissioning verified end-to-end:** the node prints a scannable pairing
  code/QR (test creds: passcode `20202021`, discriminator `3840`), opens the
  commissioning window, and **advertises `_matterc._udp` on the LAN** (confirmed via
  macOS `dns-sd` browse) — i.e. it is discoverable by Apple Home.
- **Networking gotchas resolved** (real-run findings, all in `mdns.rs`):
  interface auto-selection **skips VPN/tunnel interfaces** (Tailscale `utun*`,
  WireGuard) that can't join multicast, with a `matter_device.interface` config
  override; the mDNS socket uses **`SO_REUSEPORT`** to share port 5353 with the OS
  responder; and it uses a **native IPv4 socket on v4-only interfaces** because
  macOS rejects IPv4 multicast on a dual-stack IPv6 socket (the common home-LAN
  case). Also: `main.rs` now initializes an `env_logger` backend so rs-matter's
  `log`-based output (the pairing QR in particular) reaches the terminal.

**Phase 2c — multi-device. ✅ DONE (dispatch-shim design).**
- **Metadata is runtime-sized.** `bridge::build_node` populates a
  `DynamicNode<'_, NODE_CAPACITY>` (heapless-vec-backed) with root + aggregator +
  one bridged endpoint per exposed device. `NodeMeta` adapts it to `Metadata`.
- **The handler chain is fixed-depth — this is the load-bearing decision.**
  `rs-matter`'s handler chain is compile-time-typed: every `.chain()` nests another
  `ChainedHandler<M, H, T>` layer, and its `OnOffHandler`/`LevelControlHandler` each
  bind to one endpoint. The *obvious* generalization — one `.chain()` per (endpoint,
  cluster) — makes the chain type ~5·N deep. **We tried that (a macro-unrolled,
  `MAX_MATTER_DEVICES`-capped chain) and it does not scale: at a few dozen slots the
  nested type is deep enough that rustc's layout computation for `InteractionModel`'s
  async body consumes unbounded memory (it OOM'd a dev machine at N=32 with
  `recursion_limit` raised). Abandoned.**
  - Instead we keep the chain depth **constant** and move per-device fan-out to
    runtime. For each stateful cluster (OnOff, LevelControl, BridgedBasicInfo) we
    register **one** *dispatch shim* — an `AsyncHandler` matched to that cluster on
    *any* endpoint (`EpClMatcher::new(None, cluster_id)`). The shim owns a
    `Vec<rs_matter::…Handler>` (one real per-endpoint handler per device) and routes
    each `read`/`write`/`invoke` to the instance selected by `ctx.endpt()`,
    delegating to `rs-matter`'s handler verbatim (no reimplementation of On/Off /
    Level logic). Its `run()` drives every device's background loop concurrently via
    `FuturesUnordered`. Stateless clusters (Descriptor, Groups) use a single shared
    handler matched on any endpoint. Net chain depth: ~6 links regardless of N.
  - `MAX_MATTER_DEVICES` (64) is now only a **soft** resolver cap
    (`E_TOO_MANY_EXPOSED`, replacing `E_MATTER_SINGLE_DEVICE`) + the `DynamicNode`
    capacity bound — *not* a compile-time chain size. No type explosion, no
    `build.rs`, no `recursion_limit` bump.
- **More capabilities:** Color→ColorControl Hue/Sat (reuse `color.rs`);
  ColorTemperature→ColorControl (mireds); Occupancy→OccupancySensing;
  Battery→PowerSource. (Phase 2 wired their *exposability*; the cluster handlers
  land here — Switch/Brightness are done.)
- **Production attestation:** replace test DAC/PAI with real device-attestation
  certificates for non-development use.

**Phase 3 — polish & docs.**
- ✅ `examples/matter_device.yaml`; ✅ README TODO updated; a short "northbound vs
  southbound" section in adapter docs (pending); ✅ `--check` validates a
  matter_device config offline.

## 6. Risks / open questions

- **`rs-matter` API churn** — the accepted bet (its README warns of
  backwards-incompatible changes, "blast radius more limited now"). The
  `MatterTransport` seam contains it: only Phase 2b's real-transport module pins to
  `rs-matter`'s API; the mapping, adapter, engine and config path do not.
- **Commissioning / fabric persistence** — *decided: model B (sidecar file).* The
  Matter fabric store holds the operational cert + keys a controller mints at
  commissioning — real, non-derivable runtime state (the one legitimate such bit a
  northbound adapter holds). It cannot live in the hand-authored YAML, so:
  - **Reproducibility is restated, not weakened:** the *configuration* is fully
    reproducible from the YAML; *commissioned identities* are runtime data, like a
    database's data directory. Documented in the README ("Configuration vs. runtime
    state") and here. Without persistence, a paired controller goes "No Response"
    and must be removed + re-paired on **every** restart — unusable; with it, pair
    once and restarts just work (this is exactly how Home Assistant's HomeKit
    bridge and z2m's `data/` dir already behave).
  - **Config surface (landed this phase):** `system.runtime_storage_path` — a
    directory for *all* runtime-state features, defaulting to the **config file's
    own directory** (resolved by the host, stable regardless of cwd; `.` would tie
    the location to launch dir, a footgun we avoided). Per-adapter override
    `matter_device.runtime_storage_file`, defaulting to
    `<runtime_storage_path>/homekit.<hash>.state` where `<hash>` is a deterministic
    FNV-1a of the adapter name (idempotent across restarts, collision-free across
    adapters). The resolved path is threaded to `build_northbound` via
    `NorthboundCtx` and on to `real_transport::connect`; the pure resolution
    (`SystemConfig::runtime_storage_dir`, `default_state_file`) is unit-tested.
  - **Remaining for Phase 2b:** the real `rs-matter` node `KvBlobStore`-persists
    the fabric range to that file and reloads it on boot. Until then the stub
    keeps everything compiling and tested; the path already flows through.
- **Multiple northbound adapters / a device exposed twice** — the `Vec<Observer>`
  fan-out handles N northbound adapters naturally; nothing special needed.
- **HAP identity/pairing state** — pairing keys are HAP-local state the YAML can't
  derive. Persist them beside the config (a small state file), analogous to how
  southbound stacks own their pairing tables. This is the *one* legitimate bit of
  non-derivable state a northbound adapter holds; call it out so it doesn't look
  like a tenet violation.
- **Write-loop safety** — a HAP write → `RequestedChange` → command → device echo
  → `state_folded` → HAP update must not oscillate. The existing causal-depth
  backstop (`engine.rs`) plus HAP's own value-equality suppression should prevent
  it; add a test that a settled write reaches a fixpoint.
