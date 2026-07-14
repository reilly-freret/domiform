# domiform architecture

A map of how domiform is built, organized around the lifetime of the program:
run the binary → parse config → build the runtime → drive it in real time. Read
this before diving into the source; it exists so you (or an agent) don't have to
re-derive the architecture from 13k lines every time.

## The one idea everything bends toward

domiform treats a smart home as a **program** and its rules as **source code**.
You write YAML declaring (a) your devices and (b) rules/scenes relating them;
domiform *compiles* that into an object graph and *runs* it on a deterministic
engine.

Two properties drive nearly every design decision:

- **Declarative** — you describe *what* ("dark + motion → lamp on"), never *how*
  (MQTT topics, Matter clusters). The "how" is the adapters' problem.
- **Deterministic & replayable** — the engine is a single-threaded loop that only
  advances on explicit `inject`/`advance` calls. Same events in the same order ⇒
  identical behavior, every time. This is what makes the whole system testable
  with zero hardware and zero threads.

**The load-bearing principle:** *keep the deterministic core deterministic, and
push every side effect — wall-clock time, threads, networks — to the edges.*
Almost every "why is it built this way?" bottoms out here.

A signature consequence: **time is just an adapter.** There's no special "if it's
after sunset" machinery. A synthetic clock *device* has ordinary capabilities
(`TimeOfDay`, `SunUp`), so "is it dark?" is literally the same shape as "is the
switch on?" — `BoolEquals { device: sun, kind: SunUp, value: false }`.

## Layer map (the whole arc in five lines)

| Layer | Files | Role |
| --- | --- | --- |
| Host | `src/main.rs`, `src/wake.rs` | Owns wall-clock time + the real-time pump. The **only** impure, thread-aware part. Feeds the engine pure time-deltas. |
| Compiler | `src/compile/` | Frontloads YAML into a stable, id-interned object graph, then never speaks again. Protocol-agnostic via a plugin registry. |
| Engine | `src/engine.rs` | Single-threaded, FIFO, run-to-quiescence state machine. Loop-proofed by causal depth; retries are ordinary future timers. |
| Vocabulary | `src/model.rs`, `src/rule.rs`, `src/state.rs` | Small enum lingua franca (`Event`/`Command`/`CapabilityState`) + a three-valued (Kleene) condition algebra. |
| Adapters | `src/adapters/` | Trap every protocol and every thread behind a narrow seam. Bug-prone logic lives in *pure* functions; the one background thread just "buffers a message, rings a bell." |

---

## Stop 1 — the host (`main.rs` + `wake.rs`)

`main.rs` is **not** the engine — it's the *host* that drives the engine. It lives
in the real world (reads the clock, sleeps) and keeps wall-clock time on its side
of the fence. The engine never learns what "now" is in any absolute sense.

**Three-way exit fork** (`main`): usage error → `ExitCode::from(2)`; work failed
(config didn't compile) → `ExitCode::FAILURE` (1); success → `0`. Note it returns
values / exit codes rather than panicking — *panics are for bugs, `Result`/exit
codes for expected failure*. `--check` validates one-or-many configs offline
(touches no hardware), because compilation is a pure function of the text.

**The real-time pump** (`run_engine`, the core loop):

```rust
let mut last = Instant::now();
loop {
    let timeout = engine.next_wake_delay()            // soonest an adapter needs a tick
        .map(Duration::from_millis).unwrap_or(MAX_SLEEP).min(MAX_SLEEP);
    wakes.wait(timeout);                              // block until timeout OR a Waker fires
    let now = Instant::now();
    let elapsed = now.duration_since(last).as_millis() as u64;
    last = now;
    engine.advance(elapsed);                          // push virtual time forward by real elapsed
}
```

Wall-clock time exists **only** on the host side (`Instant::now()`), and crosses
into the engine only as a plain `u64` of elapsed ms (`advance(elapsed)`). That's
the entire mechanism behind "the engine stays agreement-free of wall time." Swap
`Instant::now()` for a hand-fed delta sequence and the engine can't tell — which
is exactly what tests do.

**Two clocks, kept separate:**

- `Instant::now()` — monotonic, host-only, used solely to measure *deltas*. Never
  stored in the engine.
- `boot_epoch_ms` — wall-clock Unix time, captured **once** at startup and handed
  to the `ClockAdapter` so the synthetic clock knows the absolute time/sun state.

The engine's own time is `now: Millis`, a virtual counter starting at 0 that only
moves when `advance` adds a delta. Any engine moment's wall time is
`boot_epoch_ms + engine.now()`. **Determinism story:** inject the boot epoch once
(an input), feed deltas thereafter (inputs); the engine's entire relationship with
time is a pure accumulation of inputs. `build_engine_at` fixes the epoch for
replay tests.

**`wake.rs` — the cross-thread wakeup.** `next_wake_delay()` tells the host the
soonest a *scheduled* wake is due (next timer, next clock minute). But *inbound
I/O* (a button press) arrives at unpredictable times on background threads.
Without a signal, the host would have to poll on a short interval (burning CPU
when idle, adding latency). The `Waker`/`WakeListener` pair (an `mpsc` channel)
closes that gap: a transport thread calls `Waker::wake()` after buffering inbound
work, and `WakeListener::wait()` — blocked until the next scheduled wake — returns
immediately. So the loop sleeps until the *earlier* of "next scheduled wake" or
"inbound arrived," and never spins. The `Waker` carries **no data** (`Sender<()>`)
— just "there's work, go drain." It lives entirely outside the deterministic core;
tests never touch it.

`MAX_SLEEP` (5s) is a safety-net cap so virtual time roughly tracks real time even
when a config schedules no timers. The pump loop has no shutdown path yet — a
deliberate omission (the `Waker`'s `wake()` is explicitly best-effort for a
dropped listener), not an oversight.

---

## Stop 2 — the compiler (`compile/`)

A real three-phase compiler pipeline:

```text
YAML ──parse──▶ AST ──resolve──▶ CompiledConfig ──build_engine──▶ Engine
      (ast)          (resolve)                     (adapters via plugin registry)
```

The key sentence: **"the runtime never consults config text after startup."** Once
`compile_str` produces a `CompiledConfig`, the YAML is gone — the engine runs a
graph of integer ids and enums, no strings on the hot path. That *is* "a compiled
representation of a ruleset, not a pile of runtime ephemera."

### Phase 1 — `ast.rs`: serde mirrors, dumb on purpose

The AST types hold strings and raw values and do **no validation** — all judgment
lives in phase 2. serde mechanics doing real work:

- `#[serde(deny_unknown_fields)]` on nearly every struct → a typo is a hard error,
  not a silent default. The config-language equivalent of `-Werror` on typos.
- `#[serde(flatten)]` on `RawAdapter.config` → captures `type` explicitly and dumps
  *all other keys* into an opaque `Value` blob. That blob is the *adapter's*
  business, not the compiler's. This is why a new adapter edits **zero** lines of
  `ast.rs`.
- Rule triggers/conditions/commands stay as raw `serde_yaml::Value` (not typed
  enums): their natural terse single-key-map form (`{ turn_on: hallway_light }`)
  doesn't deserialize cleanly via serde's tagged enums, so `lower.rs` hand-parses
  them for full diagnostic control.

**`BTreeMap` everywhere, not `HashMap`** (critical): `BTreeMap` iterates in sorted
key order; the resolver assigns ids *in iteration order*, so `DeviceId(3)` is
always the same device across runs. Using `HashMap` would shuffle ids run-to-run
(its hasher is randomly seeded per instance) and **break replay, persisted state,
and reproducible diffs** — the compiled artifact must be a pure function of the
config text, with no per-run entropy. This is the same purity principle as
`boot_epoch_ms`, reaching into the parse layer.

`parse_raw_config` strips top-level `x-*` keys before deserializing, so YAML
anchors work without tripping `deny_unknown_fields`.

### Phase 2 — `resolve.rs`: earns the name "compiler"

Two big ideas:

**Interning — strings become arena indices.** Cross-references are modeled by
*index*, not pointer: `DeviceId(u32)`, `AdapterIdx`, etc. are just positions in a
`Vec`. This is the idiomatic Rust answer to "an object graph with direct
references" without fighting `Rc<RefCell<_>>` cycles (see `ids.rs`). Each newtype
(`DeviceId(pub u32)`, `SceneId(pub u32)`, …) is a zero-cost wrapper that stops you
passing a `SceneId` where a `DeviceId` is expected — compile-time safety, no
runtime cost. After resolve, `DeviceDef.adapter` is an `AdapterIdx`, not a string;
the name is dead.

**Diagnostics accumulate, they don't abort.** One `diags: Vec<Diagnostic>`
collects *every* problem in a single pass; the function only bails at the end
(`if diags.iter().any(is_error) { return Err(...) }`). Warnings survive into
`CompiledConfig.warnings`. This is `rustc`-quality UX: see all errors at once.
Whole-program lints a mere schema validator couldn't do: `E_UNUSED_ADAPTER`,
`E_UNUSED_SCENE`, `E_UNUSED_SCHEDULE`, `E_INERT_DEVICE`, and especially
`E_DANGLING_TIMER` (a set-difference `referenced_keys - scheduled_keys` catching a
rule that waits on a timer nobody schedules — a statically-dead rule).

**The plugin registry — validating protocols the compiler has never heard of.**
The resolver must validate adapter-specific config (broker URLs, device addresses)
*without knowing about* any protocol. Solution: `trait AdapterPlugin`
(`adapters/plugin.rs`). Each adapter provides a zero-sized `PLUGIN` static,
collected in one slice (`adapters/mod.rs`):

```rust
static PLUGINS: &[&dyn AdapterPlugin] = &[
    &zigbee2mqtt::PLUGIN, &matter::PLUGIN, &zwavejs::PLUGIN,
    &matter_device::PLUGIN, &mock::PLUGIN, &mock_northbound::PLUGIN,
];
```

- `dyn AdapterPlugin` — a **trait object**: concrete type erased, dispatch through
  a vtable. The resolver calls `plugin.validate_config(...)` with genuinely no idea
  which protocol it hit.
- `&'static` — each `PLUGIN` is a zero-sized `static` baked into the binary, so no
  allocation, no lifetime juggling; the registry can be a `static`. `Sync` bound
  makes that safe.
- Defaulted trait methods do double duty: `polarity()` defaults to `Southbound`
  (so existing adapters are unaffected by the northbound concept); `build` vs
  `build_northbound` default to `unreachable!` / `None` so each adapter implements
  only its half and the builder picks by `polarity()`.

**Adding a protocol adapter = one new file + one line in `mod.rs`.** The compiler
never names a concrete adapter.

### Phase 3 — `build_engine` (`compile/mod.rs`)

Turns the static `CompiledConfig` into a live `Engine`: group devices by adapter,
build each adapter **through the trait** (branching *once* on `polarity()` —
southbound gets a dispatch slot via `add_adapter`; northbound gets `NO_SLOT` via
`add_northbound` because nothing routes commands *to* it), then wire the synthetic
clock device (seeded with `boot_epoch_ms`, tz, lat/long). Scheduler is always
engine slot 0 ("time is an adapter," physically arranged).

Note the division of error handling: user errors are `Diagnostic`s (phase 2);
internal invariant violations the resolver already guaranteed are `.expect(...)` /
`debug_assert!` (phase 3). Same posture as `main.rs`: `Result` for expected
failure, panic for "impossible."

---

## Stop 3 — the engine core (`engine.rs`)

The deterministic heart. **Ruthlessly single-threaded**: no `async`, no `Mutex`,
no `spawn`. That's the point, not an oversight.

**Shape:** an `Engine` struct bundling `now`, a `queue: VecDeque<(Event, u32)>`
(FIFO work queue; the `u32` is *causal depth*), the `StateStore`, `rules`,
`adapters: Vec<Box<dyn Adapter>>` (owned trait objects; slot 0 = scheduler),
`northbound`, `device_to_adapter` routing, `scenes`, `observers`, and retry
bookkeeping. The engine speaks only `Command`/`Event`; the `Box<dyn Adapter>`
vtable translates to/from protocols, so the core stays protocol-ignorant.

**Three (and only three) ways to drive it**, each ending in `drain()` (run to
quiescence — the queue is always empty when the call returns):

- `start()` — boot: tick every adapter once (so the clock's initial snapshot lands
  before any event), drain, replay startup state into northbound adapters.
- `inject(event)` — "an adapter reported something": push at depth 0, drain.
- `advance(dt)` — "time passed": bump `now`, tick adapters (firing due timers),
  drain.

Because `inject`/`advance` are the *only* inputs and neither reads a clock or a
socket, replay is guaranteed. (The `Waker` only influences *when the host calls*
`advance` — it never feeds data in.)

**The drain loop** (`drain`) per event: (A) drop if `depth > max_cascade_depth`;
(B) intercept internal retry timers (re-dispatch at depth 0); (C) `RequestedChange`
(northbound intent) → translate to a `Command`, dispatch, **do not fold, do not
match rules**; (D) `fold_state` — update the store; (E) match every rule
(`trigger.matches` then `condition.eval`), always computing the full three-valued
`Truth` and reporting it (no short-circuit — debuggability); (F) dispatch commands
of fired rules.

Two subtleties worth internalizing:

- **fold-before-match (D before E)** implements the design's "pressure-test
  finding": rules are `trigger + condition + commands`, **not** `event → commands`.
  Conditions read the *current state store*, not the triggering event. The trigger
  says *when*; the condition says *whether*, against live state.
- **`RequestedChange` is an intent, not a report** — it's dispatched but never
  folded and never matched. Reality arrives later as the device's own echo
  (`StateReported`), which is what folds. So an app tap and a physical wall switch
  are indistinguishable to the engine, and the store only ever reflects *reported
  reality*.

**Causal depth — the cleverest idea in the file.** The `u32` riding each queued
event counts dispatch hops. External events + timer fires = depth 0; anything a
command produces = depth+1. If depth exceeds `max_cascade_depth` (default 32) the
event is dropped (`cascade_dropped`). This makes the single-threaded loop
**provably terminating**: each iteration either drains toward empty or increments
depth toward the cap. It's the runtime backstop against feedback cascades
(misbehaving devices, state-dependent cycles) that static analysis can't see;
static cycle detection is the compiler's job. Retries reset to depth 0 (they're
time-gated, so they legitimately start a fresh causal chain).

**Dispatch & retry** (`dispatch_at` → `handle_outcome`): resolve implicit commands
first (a `ToggleSwitch` becomes concrete `SetSwitch { on: !current }` against known
state, so adapters get well-supported On/Off, not the flaky protocol `Toggle`);
expand scenes inline (no special runtime semantics — just a named command list);
route scheduler commands to slot 0; route device commands via `device_to_adapter`.
Then branch on `DispatchOutcome`: `Ok(evs)` → enqueue at depth+1; `Transient` →
`schedule_retry` (routed as a normal `ScheduleTimer` through the scheduler,
exponential backoff — retries are just future events, fully replayable in virtual
time); `Permanent` → give up. Giving up emits a `CommandFailed` **event** back on
the bus, so a *rule* can react ("device offline → notify") — failure is
first-class, not an exception.

**The borrow-checker dance — `notify` as a free function.** `notify` takes
`&mut [Box<dyn Observer>]` (just the observers slice), **not** `&mut self`. Why:
in the rule loop you hold `&self.rules` (iterating) and `&self.state` (reading)
while needing `&mut self.observers` (to notify). Rust allows simultaneous borrows
of *disjoint fields* but a `&mut self` method borrows *all* of `self` — including
`self.rules`, which you're mid-iteration on. The fix is to **narrow the borrow to
the field**: pass `&mut self.observers` directly. `notify`/`fan_state_folded`
aren't for reuse — they're borrow-narrowing devices. This one pattern removes half
your borrow-checker fights in stateful Rust.

---

## Stop 4 — vocabulary & the Kleene truth algebra (`model.rs`, `rule.rs`, `state.rs`)

`model.rs` opens by calling itself *"the highest-risk, least-reversible part of the
design — everything else is shaped around these enums."* True: the engine,
adapters, and lowering are all organized around three enums.

**The narrow waist.** `Event` (everything that can *wake* the engine, one shape),
`Command` (everything the engine can *ask a device to do*), `CapabilityState` (the
*state* a capability can be in). Adapters translate protocol↔these enums; rules
speak *only* these enums. N protocols on one side, M rule-forms on the other, small
fixed vocabulary in the middle ⇒ N+M translations, not N×M. Enums specifically,
because Rust's **exhaustive `match`** turns any vocabulary change into a
compiler-enforced checklist across every call site — which is *why* it's safe to
call this the least-reversible part.

**State lives on capabilities, not devices.** The store keys on
`(DeviceId, CapabilityKind)`, so a device is an arbitrary *bag of capabilities*
(bulb = `{Switch, Brightness, Color}`; sensor = `{Occupancy, Battery}`; button =
none). Adding a device "type" usually needs no code — it's a different subset of
the same capability atoms. `CapabilityKind` (tag only, used as a key / in
conditions) mirrors `CapabilityState` (tag + value); `CapabilityState::kind()`
projects one to the other.

**`as_bool` / `as_i64`** collapse the eight `CapabilityState` variants into two
shapes ("boolean-ish": switch/occupancy/sun-up; "numeric-ish":
brightness/battery/color-temp/time-of-day). This is why the condition evaluator
needs only two leaf kinds (`BoolEquals`, `Compare`) instead of one per capability.

**"Time is an adapter," embodied:** `TimeOfDay(u16)` and `SunUp(bool)` are ordinary
`CapabilityState` variants in the same store. "After sunset" is
`BoolEquals { device: sun, kind: SunUp, value: false }` — the *same* leaf a light
condition uses. Because time state is *in the vocabulary*, the whole condition
algebra reaches it for free; no parallel time evaluator.

**Three-valued (Kleene) truth — the star.** `Truth` is `{ True, False, Unknown }`,
not `bool`. The problem: a condition may read state *no adapter has ever reported*.
With plain `bool`, missing→`false` is a trap: `Not(sun_is_up)` with missing sun
state → `Not(false)` → `true` → **your outdoor light fires in daylight** at boot.
So "never heard about this" is a first-class value: the store distinguishes
`Some(false)` from `None` (`state.rs`), a leaf reading `None` yields
`Truth::Unknown`, and `Unknown` **propagates** through the operators (`rule.rs`):

- `Not(Unknown) = Unknown` — the daylight bug is structurally impossible.
- `False AND Unknown = False`, `True OR Unknown = True` — classical logic still
  applies where a definite value settles the result (mirrors SQL `NULL`).
- **A rule fires only on definite `True`** (`is_true()`): `Unknown` and `False`
  both decline. "When unsure, do nothing" — the correct default for physical
  control — encoded in one line.

This is *why* the drain loop reports the full `Truth` to observers (Stop 3): a
non-firing rule is either `False` (genuinely not met — expected) or `Unknown`
(reading state nobody reported — usually a bug: dead leaf, offline device). The
`-v` trace shows which; a two-valued world can't tell "correctly not firing" from
"silently broken." The compiler *should* warn on a leaf that's *permanently*
`Unknown` (references a capability no adapter reports) — static analysis for the
always-unknown case, three-valued runtime logic for the transiently-unknown case.

**Why the tests live at the bottom of `rule.rs`:** `Condition::eval` is a *pure*
function of `(condition, state)` — no clock, no I/O, no `self`. So the entire
condition algebra is testable as arithmetic on hand-built `StateStore`s, no engine,
no threads, no flakiness. Purity is the whole payoff.

---

## Stop 5 — adapters (`adapters/`): where protocols and threads live

Adapters are the **only** place protocols (or time) exist. Everything inbound of an
adapter is canonical `Event`/`Command`; everything outbound is
MQTT/Matter/Z-Wave. `zigbee2mqtt.rs` is the canonical example, and its internal
geography *is* the architecture in miniature — three sections:

1. **The `Adapter` impl** — thin, stateful, engine-facing.
2. **Pure translation** (`command_to_publish` / `message_to_events`) — explicitly
   *"no transport, no engine."* Free functions, plain data in/out, no `self`. All
   the bug-prone logic (JSON shape-matching, brightness scaling, multi-gang channel
   rules) lives here and is unit-testable with hand-built inputs.
3. **The real transport** (`mod rumqttc_transport`) — the *one* place with a thread
   and a network.

**The transport seam.** `trait MqttTransport` is just two methods: `publish`
(outbound) and `poll` (inbound, non-blocking, "every message since last call"). The
adapter holds a `Box<dyn MqttTransport>`, so it can't tell a real broker from an
in-memory test double — same adapter code, no broker in tests. The trait's own doc
notes it's *"not required to be `Send`: the engine is single-threaded, and a real
transport keeps its own network thread internally."* The thread lives *inside* the
transport, below this seam.

**The `Adapter` impl is pull-based** (the concrete realization of the Stop-3
threading model):

- `dispatch(cmd)` — translate via the pure `command_to_publish`, `transport.publish`,
  map the result to a `DispatchOutcome` (`Ok`→`ok()`, `Err`→`Transient`; unknown
  device / unsupported command → `Permanent`). The adapter makes *no* retry
  decisions — it classifies the failure and lets the engine's retry machinery
  decide.
- `tick()` — **the engine calls this**; the adapter drains the transport's buffer
  (`transport.poll()`), translates via the pure `message_to_events`, and **returns**
  `Vec<Event>`. It never pushes to the engine; no `Event` ever crosses a thread
  boundary. Also does first-tick `/get` **priming** so device state isn't stuck at
  `Truth::Unknown` at boot (the priming and the three-valued logic are two halves of
  one story: logic makes unknown *safe*, priming makes it *rare*).

**The one background thread — the whole concurrency model in one `thread::spawn`.**
In `RumqttcTransport::connect`:

```rust
let (tx, inbound) = mpsc::channel();          // THE thread boundary: a channel
thread::spawn(move || {
    for event in connection.iter() {          // blocking network loop
        // on an inbound publish:
        let _ = tx.send(MqttMessage { .. });  // (1) buffer the raw message
        if let Some(w) = &waker { w.wake(); }  // (2) ring the bell: "go poll"
    }
});
// poll(): self.inbound.try_iter().collect()  // (3) host drains, non-blocking
```

Inbound lifecycle, every hand-off:

1. **Background thread** blocks on the socket; on a publish it does exactly two
   things — buffer the raw `MqttMessage` into an `mpsc::channel` (the *data* path)
   and call `waker.wake()` (the *signal* path). It never sees an `Event` or the
   `Engine`.
2. The **channel** is the thread boundary; `MqttMessage` is `Send`. `Sync` is
   required of the *channel*, **not** the engine.
3. The **`Waker`** sends a contentless `()` on the wake channel; the host's
   `WakeListener::wait()` (the pump) returns. Data waits in the mpsc buffer.
4. Back on the **host thread**: pump → `engine.advance` → `adapter.tick` →
   `transport.poll` drains the buffer → translate on the host thread → return
   `Vec<Event>` → engine enqueues. Deterministic ordering, single-threaded, no
   locks.

So the only `Send`/`Sync` types are the two channels (data buffer + `Waker`'s
`Sender<()>`); the `Engine`, `Adapter`, and `Event`s stay on one thread. The thread
lives *below the transport seam*, and the seam is a channel + a wake signal, so
upstream determinism is never at risk.

Production-hardening details (all confined to the transport, never leaking up): the
reconnect loop throttles/de-dupes broker errors so a down broker doesn't busy-spin
(and isn't fatal — the engine runs on while it retries); per-device topic
subscription instead of `<base>/#` avoids z2m's oversized retained `bridge/*`
messages that would destabilize the connection.

**The plugin closes the loop to the compiler.** The `AdapterPlugin` impl at the
bottom is the zero-sized `PLUGIN` static from Stop 2: `type_tag()`,
`validate_config` (broker URL), `validate_device` (requires a friendly_name
`address`), and `build` (threads the `Waker` clone down into the transport's
background thread — `main.rs` → `build_engine_with_waker_in` → `plugin.build` →
`RumqttcTransport::connect`, the whole wake plumbing end to end in one parameter).

Every adapter (`zwavejs.rs`, `matter.rs`, `clock.rs`, the `matter_device/`
northbound bridge) is a variation on this template.

---

## Polarity: southbound vs northbound

Adapters carry a `Polarity` (`adapters/plugin.rs`):

- **Southbound** (default; zigbee2mqtt, matter, zwavejs) — domiform is the
  controller; owns/commands the devices *bound* to it. Gets an engine dispatch slot.
- **Northbound** (`matter_device`, later REST/web/voice) — domiform is the source of
  truth; the consumer is upstream. Exposes devices declared *elsewhere*, binds none,
  gets **no** dispatch slot. Registered in a separate `northbound` list because it's
  driven on *both* paths: `tick`/`next_wake` like an adapter (drain consumer input,
  schedule wakes) **and** `state_folded` like an `Observer` (mirror engine state
  outward). Consumer input arrives as `Event::RequestedChange` (an intent — see
  Stop 3). `NorthboundAdapter` is a blanket impl for any `Adapter + Observer`.

---

## Where to look for what

- Add a **protocol adapter** → one new file in `src/adapters/` implementing
  `Adapter` + `AdapterPlugin`, plus one line in `adapters/mod.rs` `PLUGINS`. Touch
  *nothing* in `compile/`.
- Add a **rule trigger/condition/command form** → usually a dispatch arm in
  `compile/lower.rs`; often no AST change.
- Add a **global config constraint** → step 1 of `resolve.rs` + `SystemConfig`.
- Add a **shared device field** → `RawDevice` (`ast.rs`) + `DeviceDef`
  (`resolve.rs`).
- Change the **canonical vocabulary** (`Event`/`Command`/`CapabilityState`) → the
  highest-blast-radius change; the compiler will march you through every `match`.

## Invariants to preserve (the determinism contract)

1. The engine never reads wall-clock time or any socket directly — only `inject` /
   `advance` inputs (time enters as `boot_epoch_ms` once + deltas thereafter).
2. Config sections iterate via `BTreeMap` so id interning is stable across runs.
3. No `Event` crosses a thread boundary; transports buffer raw protocol messages
   into a channel and signal readiness via the `Waker`. The engine pulls on `tick`.
4. Missing state is `Truth::Unknown`, never `False`; rules fire only on definite
   `True`.
5. The bug-prone protocol translation stays in pure free functions, separate from
   the transport (I/O) and the `Adapter` impl (engine-facing state).
