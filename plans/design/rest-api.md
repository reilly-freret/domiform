# Design: the REST API (`system.rest_api`)

Status: **IMPLEMENTED** (see §13 for where the build deviated from this plan).
Scope: an optional, read/write HTTP API over the running engine — list device
state by config name, and submit commands (set/toggle/adjust a device, activate a
scene) as structured JSON.

This document is written to be handed to an implementer with no prior context on
the conversation that produced it. Every decision that was made deliberately is
recorded with its rationale, so a reader can tell "this is load-bearing" from
"this was arbitrary."

---

## 1. Motivation and the one hard constraint

domiform can already be driven from Apple Home / Google / Alexa through the
`matter_device` northbound adapter. What it lacks is a *programmable* surface: a
script, a dashboard, a Home Assistant `rest_command`, or a `curl` in a shell has
no way to ask "what is the kitchen lamp doing?" or to say "turn it off."

### 1a. The API cannot hold a reference to the `Engine`

This is the constraint that shapes the entire design, and it is worth stating up
front because the obvious implementation is impossible.

`Engine::advance` and `Engine::inject` take `&mut self`, and the host's run loop
in `main.rs` calls `advance` on every iteration. An HTTP server needs its own
thread (it blocks on `accept`). So the naive shape — hand the server an
`Arc<Engine>` — fails three times over:

1. **`Arc<T>` only ever yields `&T`.** `Arc::get_mut` returns `None` whenever a
   second clone exists, which is precisely the case here.
2. **`Engine` is neither `Send` nor `Sync`.** It holds `Vec<Box<dyn Adapter>>`,
   `Vec<Box<dyn Observer>>`, and `Vec<Box<dyn NorthboundAdapter>>`; none of those
   trait objects carry auto-trait bounds, so the engine cannot cross a thread
   boundary at all. This is deliberate — see the comment on `MatterTransport` in
   `src/adapters/matter_device/mod.rs`: *"Not required to be `Send`: the engine is
   single-threaded, and a real transport keeps its own runtime thread internally."*
3. **`Arc<Mutex<Engine>>` would be wrong even if it compiled.** It would require
   adding `+ Send` to four traits, and it would hold a lock across `Engine::drain`,
   which runs the queue to quiescence and dispatches into adapters that perform
   real network I/O. A `GET` would block behind an MQTT round-trip, a slow client
   could stall the run loop, and two threads injecting events would destroy the
   determinism the whole architecture rests on (`src/engine.rs`: *"a
   single-threaded, ordered event loop"*).

**Therefore: the REST API is a northbound adapter.** The engine owns it; the HTTP
threads talk to it over a shared state mirror (for reads) and an `mpsc` channel
plus a `Waker` (for writes). This is exactly the shape `real_transport` already
uses for the Matter node's background thread (`ChannelTransport` in
`src/adapters/matter_device/real_transport/mod.rs`).

```text
  HTTP threads                              engine thread (the run loop)
  ────────────                              ────────────────────────────
  GET  ── read ──▶ Arc<Mutex<Mirror>>  ◀── write ── state_folded / rule_considered
  POST ── send ──▶ mpsc::channel       ──  drain ──▶ tick() → RequestedChange
                   + Waker::wake()                            RequestedScene
```

### 1b. Why "toggle" needs a model change even though config supports it

A reasonable objection: `toggle: desk_lamp` already works in config, so why is
toggling over HTTP a new problem?

Because they travel different paths. In config, `toggle` is lowered **at compile
time** by `src/compile/lower.rs` into `Command::ToggleSwitch`, and rules hold
`Vec<Command>` directly. Adapters cannot do that — an adapter's `tick` returns
`Vec<Event>`, and the only northbound intent event is:

```rust
RequestedChange { device: DeviceId, desired: CapabilityState }
```

which carries a *state*. There is no `CapabilityState` that means "toggle,"
"brightness plus ten," or "activate a scene." Phase 0 of
`plans/design/northbound-adapters.md` chose pure state on purpose (a Matter
attribute write really is a desired value), and for Matter that remains exactly
right. But it means the network→engine vocabulary is strictly narrower than the
rule vocabulary, and the gap is structural, not an oversight.

Resolving toggle inside the HTTP handler by reading the mirror and sending
`Switch(!current)` is **not acceptable**: it races with in-flight engine state and
it duplicates `Engine::resolve_implicit_state_command`, which already performs
exactly this resolution correctly, against the authoritative store, at dispatch
time.

So Phase 0 of this plan widens the intent vocabulary once, for every present and
future northbound frontend.

---

## 2. Decisions already made

These were settled with the project owner. Do not re-litigate them without
asking; do flag it if implementation reveals one of them is unworkable.

| Decision | Choice | Note |
|---|---|---|
| Intent vocabulary | Widen `RequestedChange`'s payload into a `Desired` enum | One path shared by REST and Matter, rather than two overlapping events |
| Authentication | **None in v1** | Document loopback-binding + reverse proxy. See §9 |
| Code location | Library: `src/adapters/rest_api/` | Configured from `system.rest_api`, **not** in the `PLUGINS` registry |
| Endpoints | devices, scenes, rules, system | See §7 |
| Unreported capability | Explicit JSON `null` | Mirrors the engine's `Truth::Unknown` distinction |
| Healthcheck | **Stays a separate server** | One may be enabled without the other; no code sharing beyond convention |

### 2a. One refinement to flag before you start

The owner selected "widen `RequestedChange` … (Set/Toggle/AdjustBrightness/
ActivateScene)". Putting `ActivateScene` inside that enum does not typecheck
cleanly: `RequestedChange` carries `device: DeviceId`, and a scene has no device,
so every scene request would need a bogus device field.

**This plan therefore splits it:** `Desired` holds the three device-scoped intents,
and scene activation becomes a sibling `Event::RequestedScene { scene: SceneId }`.
Both variants are honest and neither carries a dead field. If the owner prefers a
single `Event::Requested { intent: Intent }` wrapper instead, that is a mechanical
change to Phase 0 only — confirm before deviating.

---

## 3. Phase 0 — widen the northbound intent vocabulary

**Goal:** the network can express every intent a rule can, without adapters ever
constructing a `Command`.

### 3a. `src/model.rs`

Add next to `CapabilityState`:

```rust
/// What a consumer *asked for* — the northbound intent vocabulary.
///
/// Northbound adapters speak this and never construct a `Command`: the engine
/// keeps sole ownership of the intent→command translation
/// (`Engine::command_for_requested_change`), so REST, Matter, and any future
/// frontend cannot drift in how they interpret a tap.
///
/// `Toggle` and `AdjustBrightness` are *relative* intents: they are lowered to
/// the corresponding relative `Command` and resolved against the store at
/// dispatch time by `Engine::resolve_implicit_state_command`. Resolving them in
/// the adapter would race with in-flight state.
#[derive(Clone, Debug, PartialEq)]
pub enum Desired {
    /// A concrete value for one capability.
    Set(CapabilityState),
    /// Flip a switch.
    Toggle,
    /// Nudge brightness by a signed delta in percentage points.
    AdjustBrightness(i8),
}

impl Desired {
    /// The capability this intent concerns. Used by the engine to re-fan the
    /// store's settled value to northbound mirrors after a request.
    pub fn kind(&self) -> CapabilityKind {
        match self {
            Desired::Set(state) => state.kind(),
            Desired::Toggle => CapabilityKind::Switch,
            Desired::AdjustBrightness(_) => CapabilityKind::Brightness,
        }
    }
}
```

Change the event and add its sibling:

```rust
RequestedChange {
    device: DeviceId,
    desired: Desired,          // was: CapabilityState
},
/// A consumer requested a scene activation. Scenes are not device-scoped, so
/// this is a sibling of `RequestedChange` rather than a `Desired` variant.
RequestedScene {
    scene: SceneId,
},
```

Also add to `CapabilityKind` (removes a duplicated table and gives the REST layer
a public name mapping in both directions):

```rust
impl CapabilityKind {
    /// The canonical config/wire name (`switch`, `color_temperature`, …).
    pub fn name(self) -> &'static str { /* exhaustive match */ }

    /// Inverse of `name`. Includes the synthetic clock capabilities
    /// (`time_of_day`, `sun_up`); callers that must reject those — the config
    /// resolver — filter them out themselves.
    pub fn from_name(s: &str) -> Option<Self> { /* exhaustive match */ }
}
```

Then rewrite `resolve::parse_capability` to delegate to `from_name` and reject
`TimeOfDay`/`SunUp`, preserving its current behavior and doc comment exactly.

### 3b. `src/engine.rs`

`command_for_requested_change` becomes:

```rust
fn command_for_requested_change(device: DeviceId, desired: &Desired) -> Option<Command> {
    match desired {
        Desired::Set(state) => Self::command_for_state(device, state),   // today's body, extracted
        Desired::Toggle => Some(Command::ToggleSwitch { device }),
        Desired::AdjustBrightness(delta) if *delta > 0 =>
            Some(Command::IncreaseBrightness { device, value: delta.unsigned_abs() }),
        Desired::AdjustBrightness(delta) if *delta < 0 =>
            Some(Command::DecreaseBrightness { device, value: delta.unsigned_abs() }),
        Desired::AdjustBrightness(_) => None,   // zero delta: harmless no-op
    }
}
```

No new dispatch logic is needed: `resolve_implicit_state_command` already lowers
`ToggleSwitch`, `IncreaseBrightness`, and `DecreaseBrightness` to concrete
commands against the store, with clamping to `0..=100`.

**Do not miss this:** the `RequestedChange` arm of `drain` ends with a re-fan of
the store's current value to northbound mirrors (around `src/engine.rs:345`). It
currently calls `desired.kind()` on a `CapabilityState`; it must now call
`Desired::kind()`. This block is the fix for "Gap B / Task B" in
`plans/design/matter-device-reliability.md` — silently breaking it would let the
Matter bridge's optimistic attribute cell drift from truth after a rejected write.

Add a `RequestedScene` arm alongside `RequestedChange` in `drain`, with identical
semantics — dispatch `Command::ActivateScene { scene }` at `depth`, do **not**
fold, do **not** run rule matching. Scene expansion already happens inside
`dispatch_at`.

### 3c. Call sites to update (complete list)

- `src/adapters/mock_northbound.rs` — `pending_writes` becomes
  `Vec<(DeviceId, Desired)>`; add `queue_scene`. Keep `queue_write(device, state)`
  as a convenience that wraps in `Desired::Set` so existing tests read unchanged.
- `src/adapters/matter_device/mod.rs` — `tick` wraps its polled
  `(DeviceId, CapabilityState)` pairs in `Desired::Set`. The `MatterTransport`
  trait itself is **unchanged**: Matter only ever expresses concrete values.
- `tests/requested_change.rs`, `tests/northbound.rs`, `tests/virtual_device.rs`,
  `tests/matter_device.rs` — wrap literals in `Desired::Set`.
- `src/lib.rs` — re-export `Desired` from the `model` group.
- `ARCHITECTURE.md` — the drain-loop walkthrough (step C) and the "`RequestedChange`
  is an intent, not a report" bullet both name the payload type.
- `plans/design/northbound-adapters.md` — append a short "Phase 4" note recording
  that the Phase 0 vocabulary was widened, and why.

### 3d. Phase 0 tests

Extend `tests/requested_change.rs`:

- `Desired::Toggle` with a known store value dispatches `SetSwitch { on: !prior }`.
- `Desired::Toggle` with an unknown store value reaches the adapter as a raw
  `ToggleSwitch` (matching existing toggle semantics — see `tests/toggle.rs`).
- `AdjustBrightness(+10)` from 95 clamps to 100; `AdjustBrightness(-10)` from 5
  clamps to 0; `AdjustBrightness(0)` produces no command.
- `RequestedScene` expands to the scene's commands and does not fold or match rules.
- A `RequestedChange` for a device with a relative intent still re-fans the
  settled value to northbound mirrors (regression guard for §3b).

Phase 0 must land green on its own, with no REST code present.

---

## 4. Phase 1 — the engine-side adapter

**New directory:** `src/adapters/rest_api/`

```
mod.rs      RestApiAdapter, Mirror, RestApiHandle, Directory, Inbound
routes.rs   pure request → response routing (the main testable surface)
json.rs     CapabilityState ↔ JSON, in both directions
http.rs     TcpListener, connection threads, HTTP/1.1 parsing
```

### 4a. Why the library and not the binary, and why not a plugin

It goes in the **library** so `tests/rest_api.rs` can reach it. A module under
`src/rest/` in the binary crate is unreachable from `tests/`, which would leave
the routing and JSON layers — the parts most likely to have bugs — untestable.

It is **not** registered in `adapters::PLUGINS`, because it is configured from the
`system` stanza rather than the `adapters` map. There is direct precedent:
`ClockAdapter` is a library adapter constructed by `build_engine_full` from
`system` values (timezone, lat/long), with no plugin entry.

This is also the right *semantic* call. `matter_device` is a projection of a
device subset and earns its `expose:` spec; the REST API is an instance-level
control surface that lists everything and activates scenes, which `expose:` does
not model.

### 4b. Shared types

```rust
/// What the engine thread publishes for HTTP threads to read. Never a source of
/// truth — a projection of the store, exactly like a Matter attribute cell.
#[derive(Default)]
pub struct Mirror {
    states: HashMap<(DeviceId, CapabilityKind), CapabilityState>,
    rules: HashMap<RuleId, RuleStatus>,
    /// The engine's virtual `now`, recorded on each `tick`.
    now: Millis,
}

#[derive(Clone, Copy, Default)]
pub struct RuleStatus {
    pub last_considered_ms: Option<Millis>,
    pub last_truth: Option<Truth>,
    pub last_fired_ms: Option<Millis>,
    pub fire_count: u64,
}

/// A consumer request crossing from an HTTP thread into the engine.
pub enum Inbound {
    Device { device: DeviceId, desired: Desired },
    Scene { scene: SceneId },
}
```

### 4c. The adapter

```rust
pub struct RestApiAdapter {
    shared: Arc<Mutex<Mirror>>,
    inbound: Receiver<Inbound>,
}

impl Observer for RestApiAdapter {
    fn state_folded(&mut self, device: DeviceId, state: &CapabilityState) {
        // insert into shared.states
    }
    fn rule_considered(&mut self, rule: RuleId, truth: Truth, fired: bool) {
        // update shared.rules, stamped with shared.now
    }
}

impl Adapter for RestApiAdapter {
    fn dispatch(&mut self, _cmd: &Command, _now: Millis) -> DispatchOutcome {
        DispatchOutcome::Permanent("rest_api is a northbound adapter, not a dispatch target".into())
    }
    fn tick(&mut self, now: Millis) -> Vec<Event> {
        // record `now` into the mirror, then drain `inbound.try_iter()` into
        // Event::RequestedChange / Event::RequestedScene
    }
    // `next_wake` keeps its default `None`: no scheduled work of its own.
}
```

`Adapter + Observer` makes it a `NorthboundAdapter` via the blanket impl in
`src/adapters/mod.rs` — no new trait, no registry entry.

**On rule timestamps:** `Observer::rule_considered` carries no time, so the
adapter stamps it with the `now` recorded on the most recent `tick`. This is
*exact*, not approximate: `Engine::advance` sets `self.now`, then calls
`tick_adapters`, then `drain` — so the `now` the adapter recorded is the same
`now` under which rules are being considered in that cycle.

### 4d. The handle (what HTTP threads hold)

```rust
#[derive(Clone)]
pub struct RestApiHandle {
    shared: Arc<Mutex<Mirror>>,
    outbound: Sender<Inbound>,
    waker: Option<Waker>,
}

impl RestApiHandle {
    pub fn state(&self, device: DeviceId, kind: CapabilityKind) -> Option<CapabilityState>;
    pub fn rule_status(&self, rule: RuleId) -> RuleStatus;
    pub fn engine_now(&self) -> Millis;
    /// Queue a request and nudge the run loop. `false` if the engine is gone.
    pub fn request(&self, inbound: Inbound) -> bool;
}

/// Construct the linked pair. The adapter goes to the engine; the handle goes to
/// the HTTP server (and, in tests, straight to the assertions).
pub fn channel(waker: Option<Waker>) -> (RestApiAdapter, RestApiHandle);
```

Lock discipline: every method takes the mutex, copies what it needs, and drops
the guard before returning. **Never hold the guard across socket I/O.** Recover
from poisoning with `unwrap_or_else(|e| e.into_inner())` rather than propagating a
panic from one connection thread into all others.

### 4e. The directory (names)

The runtime speaks only interned ids; the API speaks names. Build the translation
table once, at startup, from `CompiledConfig`:

```rust
pub struct Directory {
    pub system_name: Option<String>,
    pub timezone: String,
    pub devices: Vec<DeviceEntry>,          // config order
    pub scenes: Vec<(SceneId, String)>,
    pub rules: Vec<(RuleId, String)>,
    device_by_name: HashMap<String, DeviceId>,
    scene_by_name: HashMap<String, SceneId>,
}

pub struct DeviceEntry {
    pub id: DeviceId,
    pub name: String,
    pub room: Option<String>,
    pub capabilities: Vec<CapabilityKind>,
    /// True for the synthetic clock device, which has no adapter or address.
    pub synthetic: bool,
}

impl Directory {
    pub fn from_config(cfg: &CompiledConfig) -> Self;
}
```

Include the synthetic clock device (`cfg.clock_device()`) as a device named
`clock`, with capabilities `[time_of_day, sun_up]` and `synthetic: true`. It is
not in `cfg.devices`; `main.rs` already special-cases it the same way when naming
the observer's tables.

The directory is immutable after construction — wrap in `Arc<Directory>` and
clone the `Arc` per connection thread. No lock needed.

### 4f. Capability validation at the edge

`DeviceEntry::capabilities` lets the API reject `POST /devices/motion_sensor/intent`
with `{"set": {"switch": true}}` at the boundary, returning `422`, instead of
letting the engine dispatch a switch command at an occupancy sensor and fail
opaquely three hops later. Do this — it is the main user-facing benefit of having
a directory at all.

---

## 5. Phase 2 — the HTTP server

### 5a. Use `std::net`, not tokio

`tokio` is already a dependency but only with `rt, macros, sync, time, net`, and
there is **no HTTP server anywhere in the tree**. Adding `axum`/`hyper` pulls in
hyper, tower, http, http-body and their transitive graph — a real build-time and
binary-size cost against a project that deliberately keeps a static musl binary.

More importantly, the handlers do no async work: a `GET` is a hashmap read under a
mutex and a `POST` is a channel send. There is nothing to await. Use
`std::net::TcpListener` with a thread per connection.

### 5b. Server shape

Model it on `src/healthcheck.rs` — the same `Disabled | Enabled` enum, the same
named-thread accept loop, the same `self_connect` shutdown trick — with four
changes that matter for an API that does real work:

```rust
pub enum RestApiServer {
    Disabled,
    Enabled {
        host: String,
        port: u16,
        directory: Arc<Directory>,
        handle: RestApiHandle,
        shutdown_signal: Arc<AtomicBool>,
        /// Set by `start`, so `self_connect` works when the config asked for
        /// port 0 (tests).
        bound: Arc<OnceLock<SocketAddr>>,
        live_connections: Arc<AtomicUsize>,
    },
}

impl RestApiServer {
    pub fn new(
        config: Option<RawRestApi>,
        directory: Arc<Directory>,
        handle: RestApiHandle,
        shutdown_signal: Arc<AtomicBool>,
    ) -> Self;

    /// Binds and spawns the accept thread. Returns the bound address, or `None`
    /// when disabled.
    pub fn start(&self) -> std::io::Result<Option<SocketAddr>>;

    pub fn self_connect(&self);
}
```

1. **Thread per connection.** `healthcheck.rs` handles connections serially, which
   is fine for a liveness probe and unacceptable for an API — one slow client
   would block every other request. Spawn a detached thread per accepted stream.
2. **Bounded concurrency.** `const MAX_CONNECTIONS: usize = 16;` guarded by an
   `AtomicUsize` incremented on accept and decremented by a drop guard. Over the
   limit, write `503` and close. Unbounded thread spawning is a trivial DoS.
3. **Timeouts.** `set_read_timeout` and `set_write_timeout` of 5s on every stream,
   so a client that opens a socket and never sends cannot pin a thread.
4. **Bounded request size.** Refuse a `Content-Length` over `64 * 1024` with `413`,
   and cap the header section at 64 lines / 8 KiB. Read exactly `Content-Length`
   bytes; do not read to EOF.

### 5c. HTTP parsing scope

Implement only what is needed, and reject the rest explicitly:

- HTTP/1.1 request line, headers, and a `Content-Length`-delimited body.
- **No keep-alive.** Always respond with `Connection: close` and close the socket.
- **No chunked transfer encoding** — respond `411 Length Required` if a body
  arrives without `Content-Length`.
- Ignore `Transfer-Encoding`, `Expect: 100-continue`, and query strings in v1.
- Split the path on `/` and match on the segments; no regex, no router crate.

### 5d. Routing as a pure function

The connection thread should do I/O and nothing else. All decision-making lives in:

```rust
// routes.rs
pub fn handle(
    directory: &Directory,
    handle: &RestApiHandle,
    method: &str,
    path: &str,
    body: &[u8],
) -> Response;

pub struct Response { pub status: u16, pub body: Vec<u8> }  // body is always JSON
```

This is the seam that makes the API testable without a socket, and it is where
the bulk of the tests should live.

---

## 6. Phase 3 — config, schema, and host wiring

### 6a. Config

`RawRestApi { host, port }` already exists in `src/compile/ast.rs` and is already
threaded through `SystemConfig` — no compiler change is required. Leave it exactly
as it is (no `token` field; auth is out of scope per §2).

**`schema/domiform.schema.json` must be updated** — `rest_api` is currently absent
from `$defs.system.properties`, and that object is `additionalProperties: false`,
so every editor with the schema attached flags a valid config as an error today.
Add it next to `healthcheck`, mirroring its shape and prose style:

```json
"rest_api": {
  "type": "object",
  "additionalProperties": false,
  "required": ["host", "port"],
  "properties": {
    "host": { "type": "string", "description": "host address of the REST API server." },
    "port": { "type": "integer", "minimum": 2000, "maximum": 65535, "description": "port of the REST API server." }
  },
  "description": "if provided, serve a read/write HTTP API on the given host and port. Unauthenticated: bind to 127.0.0.1 unless it sits behind an authenticating proxy."
}
```

Add `examples/rest_api.yaml`, modeled on `examples/healthcheck.yaml` but with a
device and a scene so the endpoints have something to return, and with
`host: "127.0.0.1"` to model the safe default. Verify it passes
`cargo run -- --check examples/rest_api.yaml`.

### 6b. `src/main.rs`

Current state, for orientation: lines 318–326 create the `shutdown` atomic, start
the healthcheck, and contain a half-finished `Arc::new(engine)` spike. **Delete
`src/rest/` entirely** (`mod.rs` and `api_core.rs`) and remove `mod rest;` — it is
superseded by the library module.

The required ordering, which is easy to get wrong:

1. Hoist the `shutdown` atomic **above** `engine.start()` (it is currently created
   after it).
2. `let (rest_adapter, rest_handle) = rest_api::channel(Some(waker.clone()));`
3. `engine.add_northbound(Box::new(rest_adapter));` — **before `engine.start()`**.
   `start()` calls `sync_northbound_startup_state`, which replays the current
   store into every northbound adapter. Register after `start()` and the mirror
   silently begins empty, so an early `GET` reports `null` for values the engine
   already knows.
4. `engine.add_observer(...)` and `engine.start()` as today.
5. Build `Arc<Directory>` from `cfg`, construct `RestApiServer`, call `start()`,
   and treat a bind error as `ExitCode::FAILURE` (matching the healthcheck's
   handling — a configured-but-unbindable port is a misconfiguration, not a
   degraded mode).
6. Add `rest_api.self_connect()` to the signal-handler thread beside
   `healthcheck.self_connect()`.
7. Leave `engine` as a plain `let mut engine` — no `Arc` anywhere — so the
   existing `engine.advance(elapsed)` in the run loop is untouched.

If the REST API is disabled in config, none of this allocates a thread or a
socket, and `channel()` is cheap enough to construct unconditionally.

---

## 7. API surface

All responses are JSON. All errors use one shape:

```json
{ "error": { "code": "unknown_device", "message": "no device named 'kitchen_lmap'" } }
```

Error codes: `unknown_device`, `unknown_scene`, `unknown_route`,
`unsupported_capability`, `malformed_body`, `method_not_allowed`,
`payload_too_large`, `too_many_connections`.

### `GET /system`

```json
{
  "name": "home",
  "timezone": "America/New_York",
  "engine_now_ms": 1843000,
  "version": "0.0.0",
  "devices": 12,
  "scenes": 3,
  "rules": 9
}
```

`engine_now_ms` is *virtual* engine time since boot (which, for the real-time
host, tracks wall-clock elapsed), not a Unix timestamp. Label it clearly in the
README so nobody mistakes it for one. `version` comes from `env!("CARGO_PKG_VERSION")`.

Adapter health is deliberately **not** included: there is no general adapter-health
API on the `Adapter` trait today (only `MatterTransport::is_healthy`, which is
internal to that adapter). Adding one is out of scope; see §11.

### `GET /devices`

```json
{
  "devices": [
    {
      "name": "kitchen_lamp",
      "room": "kitchen",
      "synthetic": false,
      "capabilities": { "switch": true, "brightness": 40, "color_temperature": null }
    },
    {
      "name": "clock",
      "room": null,
      "synthetic": true,
      "capabilities": { "time_of_day": 745, "sun_up": true }
    }
  ]
}
```

Every capability the device **declared** appears as a key. A capability the engine
has never folded is `null` — the explicit representation of "we have never heard
about this," which is a real and meaningful state in this system (see
`StateStore::bool_value`'s doc and `Truth::Unknown`). `ir_transmitter` is
write-only and will always be `null`; that is expected.

Value encoding follows the canonical units in `src/model.rs` verbatim — brightness
and battery and humidity are `0..=100`, temperature is centidegrees Celsius,
color_temperature is mireds, illuminance is lux, power is watts, time_of_day is
minutes since local midnight. Color is `{"r": 255, "g": 170, "b": 0}`. Do not
convert units at this layer; the canonical-unit contract is the whole point.

### `GET /devices/{name}`

One device object, same shape. `404` with `unknown_device` if the name is not
declared.

### `POST /devices/{name}/intent`

Body is exactly one of:

```json
{ "set": { "switch": true } }
{ "set": { "brightness": 40 } }
{ "set": { "color": { "r": 255, "g": 0, "b": 0 } } }
{ "set": { "color_temperature": 370 } }
{ "toggle": {} }
{ "adjust_brightness": -10 }
```

Responses:

- `202 Accepted` → `{ "accepted": true }`
- `400 malformed_body` — unparseable JSON, zero or multiple intent keys, wrong
  value type, or out-of-range scalar
- `404 unknown_device`
- `422 unsupported_capability` — the device did not declare that capability, or a
  read-only capability was targeted (`occupancy`, `battery`, `temperature`,
  `humidity`, `illuminance`, `power`, `contact`, `water_leak`, `smoke`,
  `time_of_day`, `sun_up`)

**`202`, not `200`, and never the resulting state.** The request is queued; the
engine dispatches on the next loop iteration and the device's own echo folds some
time after that. Returning a state here would mean inventing one. A client that
needs confirmation polls `GET /devices/{name}`.

### `GET /scenes`

```json
{ "scenes": [ { "name": "evening", "commands": 4 } ] }
```

### `POST /scenes/{name}/activate`

Empty body. `202` on success, `404 unknown_scene` otherwise.

### `GET /rules`

```json
{
  "rules": [
    {
      "name": "motion_lights",
      "last_considered_ms": 1842000,
      "last_truth": "true",
      "last_fired_ms": 1842000,
      "fire_count": 7
    }
  ]
}
```

`last_truth` is `"true" | "false" | "unknown"`, the three-valued `Truth` — the
single most useful signal for debugging a rule that will not fire, and the reason
this endpoint is worth having. A rule never yet considered has `null` timestamps
and `fire_count: 0`.

Note the semantics precisely: `Observer::rule_considered` fires only when the
rule's **trigger matched**, so `last_considered_ms` means "last time this rule's
trigger matched an event," not "last time the engine looked at it."

### Anything else

`404` with `unknown_route`. A known path with a wrong method gets `405` with
`method_not_allowed`.

---

## 8. Testing

### `tests/rest_api.rs` (new)

Most tests should drive `routes::handle` directly with a `Directory` and a
`RestApiHandle` — no socket, no threads, fully deterministic.

Read path:
1. A declared-but-unreported capability serializes as `null`.
2. After the engine folds a value, `GET /devices` reflects it.
3. The synthetic clock device appears with `time_of_day` / `sun_up`.
4. `GET /devices/{unknown}` → `404 unknown_device`.
5. `GET /system` reports the config name, timezone, and counts.
6. `GET /rules` reflects a rule that fired: `fire_count` 1, `last_truth` `"true"`.

Write path — these should run through a real `Engine` with a recording southbound
adapter (copy the `Recorder` pattern from `tests/northbound.rs`), asserting the
command that actually reached the device:
7. `{"set":{"switch":true}}` → `engine.advance(1)` → `SetSwitch { on: true }`.
8. `{"toggle":{}}` with the store at `on` → `SetSwitch { on: false }`.
9. `{"adjust_brightness":-10}` from 5 → `SetBrightness { value: 0 }`.
10. `POST /scenes/evening/activate` → the scene's commands reach their devices.
11. `422` for an intent naming a capability the device did not declare.
12. `400` for an empty body, two intent keys, and a non-numeric brightness.
13. `404` for an unknown scene.

Socket layer — one test, to prove the wiring:
14. Start a server on `127.0.0.1:0`, take the bound address from `start()`, issue a
    real `GET /system` over `TcpStream`, and assert a `200` with parseable JSON.

### Regression coverage elsewhere

- The Phase 0 tests in §3d.
- Confirm the full suite plus `cargo clippy --all-targets` is green, and that
  `cargo run -- --check examples/*.yaml` still passes offline.

---

## 9. Security posture (read this before shipping)

v1 has **no authentication**. Every endpoint is reachable by anyone who can open a
TCP connection to the port, and the write endpoints control physical devices in a
home. Treat this as the defining constraint on how it is documented:

- `examples/rest_api.yaml` binds `127.0.0.1`.
- On startup, if the configured host is not a loopback address, log a `warn!` that
  states plainly that the API is unauthenticated and anyone on the network can
  control the devices.
- The README section must say the same thing, and point at a reverse proxy
  (Caddy/nginx with basic auth or mTLS) as the supported way to expose it.
- No CORS headers. A browser page on another origin must not be able to drive
  someone's house; omitting the headers is what prevents that.

Adding a bearer token later is a small change — one optional field on `RawRestApi`,
one constant-time comparison in `routes::handle` — and the design leaves room for
it. It is out of scope only because it was explicitly deferred.

---

## 10. Documentation to update

- `README.md` — a "REST API" section: enabling it, the endpoint table, the
  `202`-not-`200` semantics, the `null`-means-unknown convention, and the security
  warning from §9.
- `ARCHITECTURE.md` — the `Desired` widening in the drain-loop walkthrough, and
  `rest_api` in the northbound adapter discussion.
- `plans/design/northbound-adapters.md` — a Phase 4 entry recording that REST
  landed and that the Phase 0 "pure state" vocabulary was widened to `Desired`,
  with the reasoning from §1b.
- `src/adapters/rest_api/mod.rs` — a module doc in the house style: the ASCII
  data-flow diagram from §1a, why the API cannot hold the engine, and why it is
  configured from `system` rather than the plugin registry.

---

## 11. Explicitly out of scope

Do not build these; note them as future work if you touch adjacent code.

- Authentication, TLS, CORS (§9).
- HTTP keep-alive, chunked encoding, compression, HTTP/2.
- Streaming state changes (SSE/WebSocket). The mirror already receives every fold,
  so this is a natural v2, but it needs a subscriber registry and backpressure
  design that v1 does not.
- Mutating config at runtime — creating devices, editing rules. This would break
  the "static, declarative single source of truth" tenet; changes go through the
  YAML and a restart.
- Per-adapter health in `GET /system`. There is no adapter-health surface on the
  `Adapter` trait today. Adding `fn health(&self) -> Health` with a default is a
  reasonable separate change, and `matter_device` already tracks the underlying
  state internally, but it is not this task.
- Merging the healthcheck server into the API. Explicitly rejected: the two must be
  independently enableable.

## 12. Definition of done

- [ ] Phase 0 lands separately and green: `Desired`, `RequestedScene`, engine
      translation, all call sites, new tests in `tests/requested_change.rs`.
- [ ] `src/adapters/rest_api/` implements the adapter, handle, directory, routing,
      JSON, and HTTP server; `src/rest/` is deleted.
- [ ] `main.rs` wires it with the ordering in §6b; `engine` is never wrapped in an `Arc`.
- [ ] `schema/domiform.schema.json` accepts `system.rest_api`; `examples/rest_api.yaml`
      passes `--check`.
- [ ] `tests/rest_api.rs` covers §8, including one real-socket test.
- [ ] `cargo test` and `cargo clippy --all-targets` are green.
- [ ] README, ARCHITECTURE, and the northbound design doc are updated.
- [ ] A manual smoke test: run `examples/rest_api.yaml`, `curl` the read endpoints,
      `curl -X POST` a toggle, and confirm the device state changes and the trace
      shows the request arriving one hop from the call.

---

## 13. What changed during implementation

Three deviations from the plan above, each agreed with the owner. Everything else
landed as written.

### 13a. `Desired::SendIr(String)` — a fourth intent variant

§3a specifies three variants. A fourth was added so the API can reach
`Command::SendIrCode`. The reasoning is the same one §1b uses for `toggle`:
config's `send_ir_code:` is lowered at **compile time**, a path an adapter's
`tick` cannot take, so without a variant there is no way to trigger IR over HTTP
short of the frontend constructing a `Command` itself — the thing this design
exists to prevent. Wire form: `{"send_ir_code": "<base64>"}`.

`ir_transmitter` remains write-only and always reads as `null` (§7), because it
has no `CapabilityState`. `Desired::kind()` returns `CapabilityKind::IrTransmitter`
for this variant, so the engine's post-request re-fan (§3b) finds nothing in the
store and no-ops — correct, not a bug.

### 13b. `GET /rules` needs a second registration

**§4c is wrong as written.** It has `RestApiAdapter` implement
`Observer::rule_considered`, but the engine never calls it: `Engine::drain` fans
only `state_folded` to the `northbound` list (via `fan_state_folded`), while every
other `Observer` callback goes to `self.observers` alone. An adapter implementing
`rule_considered` is silently never invoked, and `GET /rules` reports
`fire_count: 0` forever. This was caught by the §8 test for a fired rule.

The fix keeps the engine untouched: `channel()` returns a **trio** —
`(RestApiAdapter, RestApiObserver, RestApiHandle)` — and the host registers the
adapter with `add_northbound` *and* the observer with `add_observer`. Both write
the same `Arc<Mutex<Mirror>>`, so the two endpoints read one consistent
projection. The alternative — fanning every `Observer` call to `northbound` — was
rejected as out of scope: it changes engine behavior for `matter_device` too.

### 13c. Read-only capabilities are rejected only at the REST edge

§7 requires `422` for read-only capabilities, but `Engine::command_for_state`
treats such a request as a silent no-op, and `tests/requested_change.rs`
asserts that. Both behaviors are correct for their layer and both were kept: the
engine's no-op is right for Matter (where the alternative is dropping a controller
mid-write), and the REST layer rejects at the boundary via
`CapabilityKind::is_writable`, which is where a client can actually be told why.
The engine is unchanged.
