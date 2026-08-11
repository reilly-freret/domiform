# Design: REST API v2 — events, introspection, streaming, auth

Status: **IMPLEMENTED** (see §12 for the as-built notes). Successor to
`plans/design/rest-api.md` (v1, implemented).
Scope: make the API expressive enough that a dashboard can treat the domiform
config as the single source of truth, and stop polling.

This document assumes v1 is in place and only describes what changes. Read §1 of
the v1 doc for the constraint that shapes everything (the API cannot hold the
`Engine`); nothing here relaxes it.

---

## 1. Motivation

Two consumers, with different needs:

1. **A SvelteKit dashboard**, same homelab, same Docker network. It is the
   primary consumer and drives every requirement below.
2. **Ad-hoc clients** — iOS Shortcuts POSTing to integrate with Siri and
   widgets. These want one-shot, single-request actions and cannot hold a
   connection or run a handshake.

Three concrete gaps in v1:

### 1a. Rules are unreachable, so config stops being the source of truth

The real case: air-conditioner control via an IR blaster. The homelab config has

```yaml
knob_a_toggle_ac:
  when:
    event: knob_a.click
  then:
    - send_ir_code: { device: bedroom_ir_blaster, code: *ac-toggle }
    - schedule_timer: { key: bedroom_ac_mode_timer, after: 2s }

bedroom_ac_mode:
  when:
    timer: bedroom_ac_mode_timer
  then:
    - send_ir_code: { device: bedroom_ir_blaster, code: *ac-mode }
```

v1 exposes `Desired::SendIr(String)`, so a dashboard *can* send IR — but only by
holding its own copy of the base64 codes. That duplicates a long, opaque,
easy-to-desync blob into a second repository, and it drops the second half of the
behavior: the toggle is a two-stage sequence (toggle, wait 2s, set mode) that
exists only as rules. A client replaying stage one alone gets a working power
toggle and a silently wrong mode.

### 1b. The dashboard polls

v1 answers a write with `202` and never the resulting state, correctly (§7 of v1:
returning a state would mean inventing one). The consequence is that the
SvelteKit spike POSTs, sleeps ~500 ms, then GETs, hoping the device echo has
landed. That is both slow and unreliable — the echo latency is a property of the
device and the protocol, not a constant.

### 1c. The API is unauthenticated on an off-loopback bind

The homelab config binds `0.0.0.0:8020`. v1 shipped with no auth by explicit
decision (v1 §2, §9), warning on non-loopback binds. Anything on the network can
currently control the house.

---

## 2. Decisions made

Settled with the owner in the design conversation. Do not re-litigate without
asking; do flag it if implementation reveals one is unworkable.

| Decision | Choice | Rationale |
|---|---|---|
| Rule invocation | **Device events, not rule triggering** | §3 |
| Rules over HTTP | Read-only, with structured detail | §3, §5 |
| Event write scope | Any event the device **declared** | §4 |
| Live updates | **SSE** at `GET /stream` | §6 |
| Stream contents | State, rule fires, actions, command failures | §6b |
| Stream priming | Snapshot on connect, then deltas | §6c |
| Rule/scene detail | Structured JSON, resolved to names | §5 |
| Auth | Optional bearer token; when set, required everywhere | §7 |
| Synthetic API events | A **config idiom**, not a code change | §8 |
| Batching, keep-alive | Out of scope; SSE removes most of the motivation | §10 |

---

## 3. Why device events, and not `POST /rules/{name}/trigger`

Both were considered. They are not equivalent, and the difference is decisive.

**`POST /devices/{name}/events/{event}`** injects `Event::Action { device, action }`
at depth 0 — byte-identical to what the zigbee2mqtt adapter produces when the
physical button is pressed. It runs the whole drain loop: rule matching,
condition evaluation, `for:` timers, cascades. For the AC case above, one POST to
`knob_a.click` fires `knob_a_toggle_ac`, which sends the toggle *and* arms
`bedroom_ac_mode_timer`, so `bedroom_ac_mode` fires 2 s later with the mode code.
The client gets the complete behavior and never sees a base64 string.

**`POST /rules/{name}/trigger`** — "spoof the when clause" — is ambiguous about
the `if:` clause, and the homelab config shows why that is fatal rather than
academic. These two rules share a trigger and are disambiguated *only* by their
conditions:

```yaml
grid_floor_lamp_on:
  when: { event: grid_a.top_left_single }
  if:   { switch: { device: living_room_lamp_trunk, is_on: false } }
  then: [ ... turn on ... ]

grid_floor_lamp_off:
  when: { event: grid_a.top_left_single }
  if:   { switch: { device: living_room_lamp_trunk, is_on: true } }
  then: [ ... turn off ... ]
```

A rule-trigger endpoint must either

* **evaluate the condition** — in which case triggering `grid_floor_lamp_on`
  while the lamp is on does nothing, and the client must know to try the sibling.
  That leaks the rule-pair encoding into the dashboard, which is exactly the
  duplication this design exists to remove; or
* **bypass the condition** — in which case a client can force contradictory
  states, and "rules are the source of truth" is gone.

The event endpoint has no such ambiguity: one POST to `grid_a.top_left_single`,
and the engine selects the correct rule exactly as the physical button does.

It is also the smaller change. `events:` is already a declared, named, interned
part of the device model (`DeviceDef::events: Vec<DeviceEvent>`), and
`CompiledConfig::action_id(device, event)` already exists. The `Directory` simply
does not surface it yet.

**Therefore:** rules stay read-only over HTTP. The write surface is device
events, which is the surface the config already models.

---

## 4. Device events

### 4a. `POST /devices/{name}/events/{event}`

Empty body. Fires the named declared event.

* `202 Accepted` → `{"accepted": true}`
* `404 unknown_device`
* `404 unknown_event` — the device declares no event by that name

Semantics are the same `202`-not-`200` contract as `/intent` (v1 §7): the event
is queued, the engine matches rules on the next loop iteration, and any resulting
state arrives later as the device's own echo.

### 4b. Validation is against `events:` **only**

Do not gate this on `capabilities:`. A device may declare events and no
capabilities at all — that is exactly the synthetic-event idiom in §8, and it is
the common case for an API-driven virtual device. `require_capability` must not
be reached on this path.

### 4c. Plumbing

Three small additions:

```rust
// mod.rs — DeviceEntry gains:
/// Declared stateless events, in config order. Name → interned identity.
pub events: Vec<(String, ActionId)>,
```

populated in `Directory::from_config` from `d.events` (the clock device gets an
empty vec), plus a lookup:

```rust
impl DeviceEntry {
    pub fn action(&self, name: &str) -> Option<ActionId> {
        self.events.iter().find(|(n, _)| n == name).map(|(_, id)| *id)
    }
}
```

`Inbound` gains a variant, and `RestApiAdapter::tick` a matching arm:

```rust
pub enum Inbound {
    Device { device: DeviceId, desired: Desired },
    Scene  { scene: SceneId },
    Action { device: DeviceId, action: ActionId },   // new
}

// tick():
Inbound::Action { device, action } => Event::Action { device, action },
```

That is the entire engine-side cost. `Event::Action` already flows through
`drain` with full rule matching; **no engine change is required**.

Two properties worth confirming, because they are what make this safe:

* `fold_state` is a no-op for anything that is not `StateReported`, so a POSTed
  action never touches the state store directly. It is inert unless a rule
  matches it — exactly like the physical press.
* The action is dispatched through the normal `tick` → queue-at-depth-0 path, so
  cascade-depth limits, `for:` timers and retry semantics all apply unchanged.
  An API client cannot bypass the backstop.

### 4e. `Directory` also needs schedules and rules

Rendering `Trigger::Time { schedule }` (§5a) requires resolving a `ScheduleId`
back to its config name, which the v1 `Directory` does not carry. Add it, and
carry the full `Rule` rather than just `(RuleId, String)` so `/rules` can render
trigger, condition and commands:

```rust
pub struct Directory {
    // ... v1 fields ...
    pub schedules: Vec<(ScheduleId, String)>,   // new — from cfg.schedules
    pub rules: Vec<Rule>,                       // was: Vec<(RuleId, String)>
    rule_by_name: HashMap<String, RuleId>,      // new — for GET /rules/{name}
    scene_commands: HashMap<SceneId, Vec<Command>>, // new — for GET /scenes detail
}
```

`Rule` is `Clone + Debug` and `CompiledScene` already carries `commands`, so both
are straight clones from `CompiledConfig` at startup. The `Directory` remains
immutable after construction and needs no lock (v1 §4e).

`TimerKey` is a plain `String` and so is self-describing — no lookup needed for
`Trigger::Timer`.

### 4d. `GET /devices` exposes events

Add an `events` array to the device object:

```json
{
  "name": "knob_a",
  "room": null,
  "synthetic": false,
  "capabilities": {},
  "events": ["cw", "ccw", "click"]
}
```

Config order, names only — the `raw:` protocol string is an implementation
detail of the southbound adapter and must not leak.

---

## 5. Rule and scene introspection

### 5a. `GET /rules`

v1 reports runtime status only. Add the rule's *shape*, so a dashboard can build
UI from config instead of hardcoding it — the whole point of §1a.

```json
{
  "rules": [
    {
      "name": "knob_a_toggle_ac",
      "trigger": { "type": "event", "device": "knob_a", "event": "click" },
      "condition": true,
      "then": [
        { "type": "send_ir_code", "device": "bedroom_ir_blaster" },
        { "type": "schedule_timer", "key": "bedroom_ac_mode_timer" }
      ],
      "for_ms": null,
      "last_considered_ms": 1842000,
      "last_truth": "true",
      "last_fired_ms": 1842000,
      "fire_count": 7
    }
  ]
}
```

Structured, with every id resolved to its config name. The dashboard's real query
— "find every rule triggered by an event, and render a button for it" — is a
filter on `trigger.type == "event"`.

**Trigger rendering**, one object per `Trigger` variant:

```json
{"type":"event","device":"knob_a","event":"click"}
{"type":"changed","device":"front_door","capability":"contact","to":true}
{"type":"crosses","device":"office","capability":"temperature","bound":2600,"dir":"above"}
{"type":"reports","device":"meter","capability":"power"}
{"type":"timer","key":"bedroom_ac_mode_timer"}
{"type":"time","schedule":"sunset"}
{"type":"command_failed","device":"kitchen_lamp"}
```

`TimerKey` is a plain `String`, so it is self-describing. `ScheduleId` resolves
via `cfg.schedules`, which the `Directory` must now carry (it does not today).
`command_failed` renders `"device": null` for the match-any form.

**Command rendering** is deliberately lossy in one place: `SendIrCode` reports
its device but **not** its `code`. The codes are 100+ character base64 blobs that
would dominate the payload, and a client never needs one — reaching IR without
handling the code is the entire feature. Everything else renders its scalars:

```json
{"type":"set_switch","device":"lamp","on":true}
{"type":"toggle_switch","device":"lamp"}
{"type":"set_brightness","device":"lamp","value":100,"transition_ms":null}
{"type":"increase_brightness","device":"lamp","by":20}
{"type":"decrease_brightness","device":"lamp","by":20}
{"type":"set_color","device":"lamp","color":{"r":255,"g":170,"b":0}}
{"type":"set_color_temperature","device":"lamp","mireds":370}
{"type":"activate_scene","scene":"all_off"}
{"type":"schedule_timer","key":"k","after_ms":2000}
{"type":"cancel_timer","key":"k"}
{"type":"send_ir_code","device":"bedroom_ir_blaster"}
```

`condition` renders `true` for the always-true condition and a nested structured
object otherwise. Keep this rendering in one place — a new `describe.rs` in the
`rest_api` module — so `Trigger`, `Condition` and `Command` have exactly one wire
spelling shared by `/rules`, `/scenes` and `/stream`.

### 5b. `GET /scenes`

Replace the bare count with the member commands, same rendering:

```json
{
  "scenes": [
    {
      "name": "just_got_home",
      "commands": 6,
      "then": [
        { "type": "send_ir_code", "device": "living_room_ir_blaster" },
        { "type": "set_brightness", "device": "living_room_lamp_trunk",
          "value": 100, "transition_ms": null }
      ]
    }
  ]
}
```

`commands` is retained so existing clients do not break.

### 5c. `GET /rules/{name}` and `GET /scenes/{name}`

Single-object forms, for symmetry with `/devices/{name}`. `404 unknown_rule` /
`404 unknown_scene`.

---

## 6. `GET /stream` — server-sent events

### 6a. Why SSE, and why the name is `/stream`

SSE over WebSocket: the handlers need one-way, server→client push. Writes over
`POST` already work and are what Shortcuts can speak. A WebSocket would need a
handshake, frame masking, ping/pong and close semantics hand-written against
`std::net` — a great deal of protocol code for duplex the design does not need.
SSE is `text/event-stream` over ordinary HTTP/1.1: a status line, a header block,
and `data:` lines. `EventSource` consumes it natively in the browser.

Over long-polling: long-polling holds a connection thread per client *and*
re-establishes constantly, against a 16-connection cap.

**Naming:** the stream is `/stream`, not `/events`, because `events` already
means "declared device actions" in the config vocabulary
(`POST /devices/{name}/events/{event}`, and the `events:` array in §4d). The
config vocabulary wins; the whole design leans on it being the source of truth.

### 6b. What it carries

Four event types. All four are reachable from the **existing `RestApiObserver`**,
which is registered on the ordinary observer list and therefore sees the full
`Observer` surface — `event_received`, `rule_considered`, `command_failed` and
`state_folded` all exist on the trait today.

This is worth stating explicitly because it is a live trap: the engine fans only
`state_folded` to the `northbound` list (`Engine::fan_state_folded`), so anything
implemented on `RestApiAdapter` other than `state_folded` is silently never
called. That is the v1 §13b bug. **Implement all stream sourcing on
`RestApiObserver`, never on `RestApiAdapter`.** No engine change is required.

```
event: state
data: {"device":"reilly_nightstand","capability":"switch","value":true}

event: rule
data: {"rule":"kennedy_toggle","truth":"true","fired":true}

event: action
data: {"device":"knob_a","event":"click","depth":0}

event: command_failed
data: {"command":{"type":"set_switch","device":"kitchen_lamp","on":true},
       "reason":"mqtt timeout","attempts":3}
```

* **state** — from `state_folded`. The one that kills the 500 ms poll.
* **rule** — from `rule_considered`. The best available signal for "why won't
  this rule fire", and the data is already collected for `GET /rules`.
* **action** — from `event_received`, filtered to `Event::Action`. Carries
  `depth` so a client can tell a press that started a causal chain (`depth: 0`)
  from one produced by a cascade.

  Note precisely what `depth: 0` does **not** mean: it does not distinguish a
  physical button press from an API-injected one. Both enter the queue at depth 0
  — that identity is the entire point of §3, and nothing downstream should try to
  undo it. A client that needs provenance must track its own POSTs. Do not add a
  `source` field to `Event::Action` to recover this; it would put a
  frontend-shaped concern into the core vocabulary, and the engine's
  indistinguishability property is load-bearing.
* **command_failed** — from `command_failed`. Surfaces a device that went offline,
  which the state stream alone would show only as "nothing happened".

Unknown/other `Observer` callbacks are not streamed.

### 6c. Snapshot on connect

On connect, before any delta, emit the full current mirror:

```
event: snapshot
data: {"devices":[{"name":"reilly_nightstand",
        "capabilities":{"switch":true,"brightness":80},
        "events":[]}, ...]}
```

Without it there is a genuine lost-update window: a client that GETs `/devices`
and *then* subscribes misses anything that folded in between. With it, one
connection is sufficient and the client is correct from its first frame.

**Build the snapshot on the subscriber's own thread, not the engine's.** The
subscribe path is: register the subscriber (engine-visible from that instant, so
no delta is missed), then read the mirror and serialize the snapshot on the HTTP
thread. Deltas that arrive during serialization queue behind the snapshot and are
delivered after it, which is correct — a duplicate or superseded value is
harmless, a missing one is not. Doing it in the other order (serialize, then
register) reopens the very window the snapshot exists to close.

This ordering also keeps the engine thread out of it entirely: registration is a
mutex push, and the O(devices) serialization cost lands on the connection thread.

### 6d. Backpressure — the constraint that matters

**`Observer` callbacks run on the engine thread, inside `drain`.** A blocking
write to a slow SSE client would stall the run loop, which is precisely the
failure mode the v1 architecture is built to prevent (v1 §1a.3).

Therefore:

* The observer never touches a socket. It pushes onto a **bounded per-subscriber
  queue** (`VecDeque`, cap ~256) behind the subscriber registry's mutex, and
  returns immediately.
* On overflow, **drop the oldest** and set a `lagged` flag. When the writer next
  runs it emits `event: lagged` so the client knows to re-sync with
  `GET /devices` rather than silently believing a stale view.
* Each subscriber has its **own writer thread**, blocking on its queue and doing
  the socket I/O. A slow client stalls only its own thread.
* Registry lock discipline is the mirror's (v1 §4d): take it, push, drop it.
  Never hold it across socket I/O.

A `Waker`-style condvar wakes the writer; a periodic `: keepalive` comment
(every 30 s) keeps intermediaries from reaping an idle connection and detects a
dead peer via write failure.

### 6d-bis. The engine must survive a broken API

An optional adapter must not be able to affect the core engine. v2 adds
substantially more observer code than v1 had, so this needs stating as a
requirement rather than assumed.

**`notify` has no panic isolation.** It is a plain loop:

```rust
fn notify(observers: &mut [Box<dyn Observer>], f: impl Fn(&mut dyn Observer)) {
    for obs in observers.iter_mut() {
        f(obs.as_mut());
    }
}
```

A panic inside any `Observer` unwinds through `drain`, through `Engine::advance`,
and out of the run loop in `main.rs` — killing the whole process. Today that risk
is small (the tracing observer and v1's mirror writes are simple). With a
subscriber registry, per-subscriber queues and JSON serialization on the observer
path, it grows.

Requirements, in order of preference:

1. **The observer path must not panic.** No `unwrap`, no indexing, no slicing, no
   arithmetic that can overflow. Mutexes use the existing `lock()` helper, which
   already recovers from poisoning (v1 §4d) rather than propagating. Serialization
   uses `serde_json::to_vec(...)` with a fallback on `Err`, never `.unwrap()`.
   Queue pushes are bounded and infallible by construction (§6d).
2. **Registry poisoning is survivable.** If a writer thread panics while holding
   the registry lock, `lock()` recovers the guard. The invariant is that the
   registry is never left structurally inconsistent across a panic point — push
   and pop are single operations, so it is not.
3. A `catch_unwind` boundary inside `notify` was considered and is **not**
   proposed here: it changes core engine behavior for every observer including
   the tracing one, and `&mut dyn Observer` is not `UnwindSafe` without further
   ceremony. Note it as future work if the observer surface keeps growing.

**The subscriber registry must never be locked across socket I/O**, per v1 §4d.
Concretely, the writer thread must clone or drain what it needs out of the queue,
drop the guard, and *then* write. A writer that holds the registry lock while
blocked on a slow socket would block the engine thread's next `state_folded`
push — converting a slow HTTP client into a stalled run loop, which is exactly
the failure §6d exists to prevent.

**A dead subscriber must be reaped without engine involvement.** When a write
fails, the writer thread deregisters itself and releases its `MAX_STREAMS` slot
via a drop guard (the `ConnectionGuard` pattern already in `http.rs`). The engine
never learns a subscriber existed.

**Test the invariant, do not just assert it** — see §9 tests 12 and 16.

### 6e. Connection budget

SSE breaks two v1 invariants in `http.rs`, both of which must be handled
explicitly:

1. **`Connection: close` is hardcoded** and `serve` handles exactly one request
   per connection. A stream needs its own write path: `200`,
   `Content-Type: text/event-stream`, `Cache-Control: no-cache`,
   `Connection: keep-alive`, then an open-ended body. It must **not** send
   `Content-Length`.
2. **`MAX_CONNECTIONS: usize = 16`** counts every accepted socket. Streams are
   long-lived, so a handful of open dashboard tabs would exhaust the cap and
   `503` the entire API — including the request-response endpoints.

Give streams a **separate budget**: `MAX_STREAMS: usize = 8`, tracked by its own
`AtomicUsize` and *not* charged against `MAX_CONNECTIONS`. Over the stream limit,
answer `503 too_many_streams` and close. Streams also opt out of the 5 s
`IO_TIMEOUT` read deadline (there is no further request to read) but keep a write
timeout, so a wedged peer is eventually reaped.

---

## 7. Authentication

A bearer token that is **optional to configure and mandatory to send once
configured**. `RawRestApi` gains `token: Option<String>`; the schema gains the
field.

* **Token absent from config** — every route open, exactly as v1, including the
  existing non-loopback `warn!`. This preserves local development and every
  existing config unchanged.
* **Token present in config** — *every* route requires it, reads included. One
  rule, no carve-outs. Device state is itself sensitive: it reveals whether
  anyone is home.

**Header only:**

```
Authorization: Bearer <token>
```

A `?token=` query parameter was considered and **rejected**. iOS Shortcuts sets
request headers directly in the *Get Contents of URL* action, so there is no
capability gap to work around, and a credential in a query string leaks into
proxy access logs, browser history and `Referer` headers — a durable exposure for
a secret that controls a house.

The one genuine limitation is `EventSource`, which has no API for setting
headers, so a *browser-side* SSE client cannot authenticate by header. That is
not a blocker here: the SvelteKit app should proxy `/stream` through its own
server routes, which is the natural shape for that framework anyway and keeps the
token out of the browser entirely. If this ever does need revisiting, add the
query form **scoped to `/stream` alone** rather than as a global escape hatch.

Consequences:

* `401 unauthorized` when the header is absent or malformed, `403 forbidden` on
  a well-formed token that does not match. Both use the v1 error shape.
* Compare in **constant time** — a plain `==` on a secret is a timing oracle. A
  byte-wise fold over both slices is sufficient; no new dependency. Compare
  lengths without early-return.
* Because auth is header-only, `http.rs` keeps stripping and discarding the query
  string and `routes::handle` keeps its current signature. `read_request` gains
  an `authorization: Option<String>` field parsed from the header block, passed
  to `handle` alongside the body. Strictly less change than the query form would
  have required.
* The startup warning distinguishes "non-loopback **and** no token" (loud, the
  current text) from "non-loopback with a token" (quiet — a bind that is
  legitimate behind a token).

Still out of scope: TLS and CORS. Omitting CORS headers remains what stops a
browser page on another origin from driving the house.

---

## 8. Synthetic API events — a config idiom

The question was whether a rule can trigger on an arbitrary API event rather than
a physical button. **It already can, with no code change**, and it falls out of §3.

`events:` is not tied to physical hardware. It is a declared name that interns to
an `ActionId`; a southbound adapter resolves a raw protocol string to it, but
nothing requires that any adapter ever produce one. A device on the `virtual`
adapter with an `events:` map is therefore a pure API surface:

```yaml
devices:
  api:
    adapter: virtual
    events:
      movie_mode: movie_mode
      goodnight: goodnight

rules:
  movie_mode:
    when: { event: api.movie_mode }
    then:
      - set_brightness: { device: living_room_lamp_trunk, value: 15 }
      - turn_off: hallway_desk_lamp
```

`POST /devices/api/events/movie_mode` fires it. The `raw:` value is dead weight —
nothing southbound will ever match it — but it is required by the schema and
harmless.

Two things this implies, both already specified above:

* The event route must validate against `events:` alone (§4b) — `api` declares no
  capabilities.
* Rejecting events that no rule listens to was considered and **declined**: it
  would couple the write surface to rule config and make the API change shape
  whenever rules are edited.

Ship `examples/api_events.yaml` demonstrating this, and document it in the README
as the supported way to build an API-driven automation. It is deliberately *not*
a new adapter type — a config idiom that costs nothing is better than a feature.

---

## 9. Testing

Extend `tests/rest_api.rs`, keeping the v1 pattern: drive `routes::handle`
directly where possible; use a real `Engine` with a recording southbound adapter
for anything asserting engine behavior.

**Events (§4)**

1. `POST /devices/knob_a/events/click` → `202`, and the rule's commands reach the
   recorder — including the second-stage IR after the timer elapses. This is the
   §1a case end to end and is the single most valuable test here.
2. A device with `events:` and **no** `capabilities:` accepts an event (§8).
3. `404 unknown_event` for an undeclared event; `404 unknown_device` for both.
4. Condition-disambiguated rule pairs: with the lamp on, one POST to
   `grid_a.top_left_single` fires the *off* rule only (§3).
5. `GET /devices` lists declared events in config order.

**Introspection (§5)**

6. `GET /rules` renders each `Trigger` variant with names resolved.
7. `send_ir_code` renders its device and **omits** the code.
8. `GET /scenes` renders member commands.

**Stream (§6)**

9. A folded state reaches a subscriber as `event: state`.
10. A fired rule, a device action, and a command failure each reach a subscriber.
11. A new subscriber receives `event: snapshot` before any delta.
12. A subscriber whose queue overflows gets `event: lagged` and the **engine is
    not blocked** — assert the run loop still advances. This is the §6d
    invariant; it is the one that would be catastrophic to get wrong.
13. Streams do not consume the `MAX_CONNECTIONS` budget: open `MAX_STREAMS`
    streams, then assert a normal `GET /system` still answers `200` (§6e).

**Auth (§7)**

14. Token unset → open. Token set → `401` without the header, `401` on a
    malformed header (`Basic ...`, missing `Bearer`), `403` on mismatch, `200`
    with the correct token.
15. Auth is enforced on **every** route, including `GET /system` and `/stream` —
    parameterize over the route table so a newly added route cannot silently
    default to open.

**Engine isolation (§6d-bis)** — the requirement that an optional adapter cannot
affect core operation:

16. With `MAX_STREAMS` subscribers attached and none of them reading, the engine
    still advances and southbound commands still dispatch. This is test 12's
    invariant from the opposite direction: 12 proves overflow does not block, 16
    proves saturation does not either.
17. Dropping a subscriber's socket mid-stream deregisters it and frees its slot,
    with no engine involvement — open `MAX_STREAMS`, drop them all, confirm a new
    stream is accepted.
18. The REST API disabled in config allocates no thread and no socket, and the
    engine behaves identically to a build without it (v1 §6b's property, worth a
    regression guard now that the surface is larger).

Plus: `cargo clippy --all-targets` green, and `cargo run -- --check examples/*.yaml`
passing offline for the new example.

---

## 10. Explicitly out of scope

* **Batching / multi-capability intents.** Considered and declined. SSE removes
  most of the motivation (the round-trip cost was the real complaint), and
  one-capability-per-request keeps validation and error reporting unambiguous.
* **HTTP keep-alive** for the request/response endpoints. Same reasoning: with a
  stream open, the poll traffic that made per-request TCP handshakes matter
  largely disappears.
* **`POST /rules/{name}/trigger`.** Rejected on the merits in §3.
* **Mutating config at runtime.** Unchanged from v1 §11 — it breaks the static,
  declarative single-source-of-truth tenet.
* **TLS, CORS** (§7). **WebSocket** (§6a).
* **Per-adapter health.** Unchanged from v1 §11; there is still no adapter-health
  surface on the `Adapter` trait.

---

## 11. Definition of done

- [x] `DeviceEntry.events`, `Inbound::Action`, and the `tick` arm.
- [x] `Directory` carries `schedules`, full `Rule`s, `rule_by_name` and scene
      commands (§4e); still immutable, still lock-free.
- [x] `POST /devices/{name}/events/{event}`, validated against `events:` alone.
- [x] `describe.rs`: one wire spelling for `Trigger`, `Condition`, `Command`,
      shared by `/rules`, `/scenes` and `/stream`.
- [x] `GET /rules`, `GET /rules/{name}`, `GET /scenes`, `GET /scenes/{name}`
      render structured detail; `send_ir_code` omits its code.
- [x] `GET /stream`: SSE, snapshot-then-deltas, four event types, all sourced
      from `RestApiObserver` (never `RestApiAdapter` — v1 §13b).
- [x] Subscriber registered *before* the snapshot is serialized, and the snapshot
      built on the connection thread (§6c).
- [x] Bounded per-subscriber queues with drop-oldest and a `lagged` event; a
      writer thread per subscriber; the engine thread never blocks on a socket.
- [x] The observer path is panic-free: no `unwrap`/index/overflow, `lock()`
      everywhere, fallible serialization handled (§6d-bis).
- [x] Separate `MAX_STREAMS` budget; streaming write path without
      `Content-Length`; dead subscribers reaped by drop guard.
- [x] `token: Option<String>` on `RawRestApi` + schema; constant-time compare;
      **header only**; startup warning distinguishes token/no-token.
- [x] `examples/api_events.yaml` passes `--check`.
- [x] Tests per §9 — including 12, 16, 17, 18, the engine-isolation guards.
- [x] `cargo test` and `cargo clippy --all-targets` green.
- [x] **No diff in `src/engine.rs`, `src/model.rs`, or `src/observe.rs`.** If one
      becomes necessary, stop and confirm — v2 is designed to require none, so a
      required core change means an assumption here is wrong.
- [x] README (endpoints, SSE, auth, the §8 idiom), ARCHITECTURE, and a v2 note
      appended to `plans/design/rest-api.md`.

---

## 12. What changed during implementation

Everything landed as specified. Three notes for a future reader.

### 12a. The core-engine guard held

The §11 requirement — no diff in `src/engine.rs`, `src/model.rs`,
`src/observe.rs` — was met exactly. `git diff --stat` over those files is empty.
Every change is confined to `src/adapters/rest_api/`, plus one optional field on
`RawRestApi` and the host wiring in `main.rs`.

This is the strongest evidence the §3 argument was right: reaching rules through
the *existing* event vocabulary required no new engine concept, whereas a
rule-trigger endpoint would have needed one.

### 12b. `RestApiObserver` gained the `Directory`, via late attachment

§6b called for sourcing all four stream types from `RestApiObserver`, which is
correct, but the spec did not say how that type learns *names* — a frame carries
`"device": "kitchen_lamp"`, not `DeviceId(2)`.

The constraint is ordering: `rest_api::channel()` is called before the
`Directory` exists (the adapter must be registered before `engine.start()`, per
v1 §6b). Rather than reorder the host's startup — which v1 documents as
load-bearing — the observer takes the broadcaster and directory through a
separate `with_stream(broadcaster, directory)` call after construction. A host
that does not serve the stream never calls it and pays nothing; `GET /rules`
works either way, since the mirror is maintained independently.

### 12c. Stream budget: accepted under one cap, held under another

§6e specifies a separate `MAX_STREAMS` budget. As built, a stream request is
*accepted* under `MAX_CONNECTIONS` (the request must be read before it can be
known to be a stream at all), then checked against `MAX_STREAMS` on upgrade. The
connection slot is released when `serve` returns.

The net effect is what §6e wanted — long-lived streams do not permanently consume
request/response slots — but the transient overlap means a burst of simultaneous
stream *connects* can briefly occupy connection slots. With `MAX_STREAMS: 8`
against `MAX_CONNECTIONS: 16` this cannot starve the API, and the test at §9.13
pins the property.

### 12d. A test-fixture finding worth keeping

The `examples/api_events.yaml` smoke test surfaced real three-valued-logic
behavior worth documenting rather than working around: a `toggle` against an
*unknown* switch reaches the adapter as a raw `ToggleSwitch` (the engine resolves
it to a concrete `SetSwitch` only when it knows the current value), and the
`mock` adapter echoes concrete sets only. So toggling never bootstraps state,
and a condition pair reading that switch reports `last_truth: "unknown"` with
neither rule firing.

This is correct on every layer, and `GET /rules/{name}` diagnosed it in one
request — a good demonstration that the §5 rule detail earns its place. The
example now uses a concrete `turn_on` to establish state, with a comment
explaining why.
