# Design: `matter_device` reliability hardening

Status: **IMPLEMENTED** (Tasks A–D landed on `language_improvements`; full suite +
clippy green). See the "Implementation notes" section at the end for exactly what
shipped and where. The body below is the original plan, retained as the design
record. Scope: close the **consistency / robustness gaps** in the live
`matter_device` northbound bridge — the "flakiness," distinct from the bridged-name
display problem (which is a controller-side Apple limitation we have accepted; see
[`plans/design/northbound-adapters.md`](./northbound-adapters.md) §2b and the
handoff naming caveat). Nothing here touches naming.

This doc is written to be handed to a fresh implementation agent. It references
files and line anchors as of the `language_improvements` branch; verify them before
editing (the module may have shifted).

## Context an implementer needs first

Read, in order:
1. [`plans/handoff/2026-07-09-matter-device-northbound.md`](../handoff/2026-07-09-matter-device-northbound.md)
   — the map of the whole feature. §4 lists gotchas already solved; §5 item 4 is
   the "known consistency gaps (not yet fixed)" list this doc addresses.
2. [`plans/design/northbound-adapters.md`](./northbound-adapters.md) — the design
   record; §6 "Write-loop safety" is directly relevant.
3. The module: [`src/adapters/matter_device/mod.rs`](../../src/adapters/matter_device/mod.rs)
   (adapter facet + `MatterTransport` seam), and
   [`src/adapters/matter_device/real_transport/`](../../src/adapters/matter_device/real_transport/)
   (`mod.rs` = node thread + channels, `hooks.rs` = cluster hooks ↔ channels,
   `bridge.rs` = node/handler construction, `mdns.rs` = discovery).

### Architecture recap (so the gaps make sense)

The engine is single-threaded and deterministic. The live Matter node is **not
`Send`** and runs on its own background thread (`real_transport::run_node`,
[`mod.rs:184`](../../src/adapters/matter_device/real_transport/mod.rs)), owning the
entire rs-matter object graph under one `block_on(select4(...))`. The engine and
the node thread communicate over two `std::mpsc` channels wrapped in
`ChannelTransport` ([`mod.rs:113`](../../src/adapters/matter_device/real_transport/mod.rs)):

```
engine thread                                  node thread (block_on)
─────────────                                  ──────────────────────
Observer::state_folded(dev,state)              mirror_engine_state task
  → transport.publish(dev,state)                 → hooks[i].apply_engine_state → cells
  → to_node.send((dev,state)) ───[chan]────────▶ (mod.rs:287)

Adapter::tick()  ◀──[chan]──── from_node ◀──── OnOffFacet::set_on_off / …
  → Event::RequestedChange                       → emit() → to_engine.send + Waker
      (mod.rs:159)                               (hooks.rs:174)
```

Per-device shared state lives in `LightCells` (`on`, `level`) behind `Rc<Cell<_>>`
on the node thread ([`hooks.rs:45`](../../src/adapters/matter_device/real_transport/hooks.rs)).

## Status of the four originally-listed gaps

While scoping this doc, **gap #1 was found already fixed** — do not re-implement it:

- **✅ Startup state sync — ALREADY DONE.** `Engine::sync_northbound_startup_state`
  ([`src/engine.rs:240`](../../src/engine.rs)) replays the full state store into
  every northbound adapter from `Engine::start` ([`engine.rs:225`](../../src/engine.rs))
  after the boot tick+drain. So a controller reading an endpoint post-boot sees
  engine truth, not the node's seeded defaults. Verify this still holds (a test for
  it is recommended below under Task D) but write no new sync path.

The remaining three, plus a documentation-only item, are the work:

- **Gap A — node-thread death is silent.** `connect` spawns the node thread and, on
  error, only `log::error!`s ([`mod.rs:156`](../../src/adapters/matter_device/real_transport/mod.rs)).
  The engine keeps `publish`ing into `to_node` (a now-dead `Receiver`) forever; from
  the user's side the bridge simply stops responding with no signal.
- **Gap B — optimistic write echo can lie.** A controller write flips the local
  `LightCells` immediately (`set_on_off` sets `cells.on` *before* emitting;
  [`hooks.rs:219`](../../src/adapters/matter_device/real_transport/hooks.rs)) and
  emits a `RequestedChange`. If the southbound device rejects/never confirms, the
  Matter cell is now out of sync with reality until (if ever) a fold corrects it.
- **Gap C — write→echo loop has no explicit fixpoint guarantee/test.** Design doc
  §6 calls for a test that a settled write reaches a fixpoint and does not
  oscillate. The causal-depth backstop exists; the *guarantee* is untested.
- **Doc item D — color is controller-authoritative only.** `apply_engine_state`
  no-ops on `Color`/`ColorTemperature` ([`hooks.rs:134`](../../src/adapters/matter_device/real_transport/hooks.rs))
  because rs-matter 0.2's ColorControl handler owns attribute state with no
  engine→handler write-back. This is a real desync (a southbound color change is
  invisible to a controller reading the node) that we cannot fix in 0.2. It must be
  documented as a known limitation, not left as an inline comment only.

---

## Task A — surface node-thread death to the engine

**Goal:** when the node thread exits (fatal rs-matter error, panic, or the
`block_on` returning), the adapter learns and the failure is visible — not a silent
black hole that keeps accepting `publish`es.

**Why it matters:** this is the single most likely cause of "it was working, then it
just stopped" flakiness. Today the only evidence is one `log::error!` line that a
user running without `RUST_LOG` never sees, and the engine's behavior is unchanged
(it keeps mirroring into a dead channel).

**Design:**

1. Add a **liveness flag** the node thread clears on exit and the adapter reads.
   The simplest `Send`-safe shape: an `Arc<AtomicBool>` (`alive`), set `true` before
   spawn, set `false` in the thread's closure after `run_node` returns (whether `Ok`
   or `Err`). `connect` returns it alongside the transport (store it on
   `ChannelTransport`, or return a small struct). A panic in the thread won't run
   normal code after `run_node`, so also install the clear via a guard whose `Drop`
   fires on unwind — e.g. a tiny `struct AliveGuard(Arc<AtomicBool>)` whose `Drop`
   stores `false`, created at the top of the closure. That covers both the error
   return and a panic.

2. `ChannelTransport::poll` (or a new `MatterTransport::is_healthy`) checks `alive`.
   On the first observed death, emit a **one-shot** loud log at `error` *and* — the
   important part — make the state visible to the operator through a channel they
   actually watch. Options, in preference order:
   - (preferred) add an optional `Observer`-style health signal: the adapter, on
     detecting death in `tick`, returns nothing but sets an internal
     `degraded: bool` and logs once at `error!` with actionable text ("matter_device
     node thread exited; the Matter bridge is offline until domiform restarts").
     Keep it dead-simple; do **not** attempt auto-restart in this task (see
     "Explicitly out of scope").
   - Threading a structured health event all the way to the host loop is a larger
     change; only do it if the trivial log-once proves insufficient. Prefer the
     small version.

3. Stop pointless work once dead: after detecting death, `publish` becomes a no-op
   (the channel send already silently fails via `let _ =`, but skip the clone/send
   to avoid unbounded queue growth if the `Receiver` is dropped — an mpsc `Sender`
   whose `Receiver` is gone errors on `send`, so this is a latent slow leak only if
   the receiver is somehow still alive but idle; guarding on `alive` is the clean
   fix).

**Files:** `real_transport/mod.rs` (`connect`, `run_node` closure, `ChannelTransport`),
`matter_device/mod.rs` (`MatterTransport` trait if you add `is_healthy`;
`MatterDeviceAdapter::tick` for the detect-and-log-once).

**Tests:** unit-testable via the `InMemoryMatter` fake — add a "dead transport"
variant (or a flag on `InMemoryMatter`) that reports not-healthy, and assert the
adapter logs once and no-ops publishes thereafter. The real thread-death path is
only exercisable by an integration run; note that in the test comment.

**Explicitly out of scope for Task A:** automatic node-thread restart /
resurrection. It's a legitimate follow-up but has its own hazards (fabric-store
reopen, mDNS re-announce, re-commission window) and must not ride in on this change.
Leave a `// TODO(reliability): auto-restart` marker referencing this doc.

---

## Task B — don't let an optimistic echo lie

**Goal:** a controller write must not leave the Matter attribute cells asserting a
value the real device never accepted.

**Current behavior:** `OnOffFacet::set_on_off` sets `cells.on` then emits
([`hooks.rs:219`](../../src/adapters/matter_device/real_transport/hooks.rs));
`LevelFacet::set_device_level` likewise ([`hooks.rs:282`](../../src/adapters/matter_device/real_transport/hooks.rs)).
The optimistic local set is actually **required** by rs-matter's OnOff↔Level
coupling (a read during command handling must return a concrete value — this is why
`level` is seeded to 254, see the `LightCells` doc comment). So we cannot simply
*remove* the optimistic set.

**The real fix is convergence, not removal:** ensure the authoritative engine echo
always re-asserts truth over the optimistic guess, and that a *rejected* command
visibly reverts.

1. **Confirm the echo path already corrects an accepted write.** Trace it: controller
   write → `RequestedChange` → `command_for_requested_change` → dispatch → device →
   `StateReported` → `fold_state` → `state_folded` → `publish` → `apply_engine_state`
   → cells. If the device *accepts*, the cell is rewritten with the true value; if
   the true value equals the optimistic one, it's a no-op. Add a test asserting the
   cell matches the folded value after a full round-trip (this may already pass —
   confirm, don't assume).

2. **Handle rejection / no-confirm.** If the dispatch fails permanently or the device
   never echoes, the optimistic cell stays wrong. Decide the policy:
   - **Minimum viable:** when `command_for_requested_change` yields a command whose
     dispatch returns `DispatchOutcome::Permanent`, the engine already knows the
     write went nowhere. The Matter cell should revert to the last *folded* value.
     The clean seam is: the northbound adapter, on the next `state_folded` for that
     device, overwrites the optimistic cell anyway — so the missing piece is only
     the *no-echo* case (device silently drops it). For that, rely on the fact that
     the engine's store still holds the old value; a periodic re-assert (Task A's
     mirror task already runs every 50ms — see `mirror_engine_state`,
     [`mod.rs:287`](../../src/adapters/matter_device/real_transport/mod.rs)) would
     re-push truth *if* the engine re-folded. It does not re-fold unchanged state, so
     the optimistic cell can persist.
   - **Recommended:** after emitting a `RequestedChange`, do **not** trust the
     optimistic cell as ground truth beyond the coupling read. Concretely: keep the
     optimistic set (coupling needs it) but ensure every engine fold for the device
     re-publishes even when the value is unchanged from the engine's view, so the
     node reconverges. The lowest-risk implementation is a **startup-style targeted
     re-sync**: when a `RequestedChange` is produced, record the device id; on the
     next engine quiescence, re-`state_folded` that device's current store value
     into the northbound adapter unconditionally. This guarantees the cell equals
     engine truth one hop after any write, accepted or not. Evaluate doing this in
     the engine (a "post-requested-change reconcile" that re-fans the affected
     device's current state to northbound adapters) vs. in the adapter. **Prefer the
     engine**, mirroring `sync_northbound_startup_state` — it already has the store
     and the fan-out helper (`fan_state_folded`, [`engine.rs:474`](../../src/engine.rs)).

**Files:** likely `src/engine.rs` (a targeted reconcile after a `RequestedChange`
is handled — find where `RequestedChange` is intercepted in `drain`), plus a test.
Keep southbound adapters unaffected (they don't observe `state_folded`).

**Tests (`tests/matter_device.rs` or `tests/requested_change.rs`):**
- accepted write: cell converges to the device-echoed value.
- rejected write (bind to an adapter whose `dispatch` returns `Permanent`): the
  node's mirrored state reverts to the pre-write engine value within one hop.

---

## Task C — prove write→echo reaches a fixpoint (design doc §6)

**Goal:** a settled controller write does not oscillate; the loop
write → `RequestedChange` → command → device echo → `state_folded` → publish
converges and stops.

This is mostly a **test** task — the machinery (causal-depth backstop
`max_cascade_depth`, [`engine.rs:196`](../../src/engine.rs)) exists; the guarantee
is unverified.

**Tests (`tests/matter_device.rs`, end-to-end through the engine with
`InMemoryMatter`):**
1. Queue a controller write of `Switch(true)` on an exposed, bound device; drive the
   engine to quiescence; assert (a) the bound device received exactly one
   `SetSwitch(true)`, (b) the published mirror ends at `Switch(true)`, (c) no
   further events are produced on an additional `advance` with no input (fixpoint).
2. Same for `Brightness` — assert value equality suppresses re-emission (a fold of
   the same brightness must not re-trigger a write).
3. Regression: assert the cascade-depth backstop is *not* hit in the settled case
   (i.e. convergence is natural, not merely truncated by the backstop) — check the
   observer's `cascade_dropped` was never called.

No production code should be needed for C unless a test reveals an actual
oscillation; if it does, that finding reopens B.

---

## Task D — document the color desync limitation (docs only)

**Goal:** promote the inline `apply_engine_state` comment
([`hooks.rs:134`](../../src/adapters/matter_device/real_transport/hooks.rs)) to a
visible, user-facing known-limitation note.

rs-matter 0.2's `ColorControlHandler` owns its attribute state and exposes no
engine→handler write-back (`OutOfBandMessage::Update` is a no-op), so a **southbound**
color/CT change cannot be pushed into the node. A controller therefore sees only the
last *controller-set* color, never a color changed by a domiform rule or another
adapter. Controller-driven color still works.

**Where to document:**
- README, in the same "Configuration vs. runtime state" / Matter section that
  already carries the naming caveat — a short "Known limitations" bullet.
- `examples/matter_device.yaml` — a comment near any color-capable exposed device.
- Keep the `hooks.rs` inline comment; add a one-line pointer to the README bullet so
  the two don't drift.

**Revisit trigger:** when rs-matter adds an engine→handler color-sync path (track the
crate version; we are pinned to `0.2.0` per
[`memory/rs-matter-version-pinning.md`]). Note it in the doc so a future upgrade
picks it up.

---

## Verification (all tasks)

- `cargo test` and `cargo clippy --all-targets` must stay green (handoff §6 baseline:
  17 test groups, 0 warnings).
- Live smoke test (cannot be unit-tested): `cargo run -- -c examples/matter_device.yaml`,
  pair into Apple Home, then exercise:
  - toggle from Home → domiform device reacts (write path);
  - change the device from domiform's side (or a rule) → Home reflects it (mirror);
  - **Task A:** kill/panic the node (or simulate) → the error is loud and the adapter
    stops silently accepting work;
  - **Task B:** point an exposed device at an adapter that rejects the command →
    Home's tile reverts instead of staying wrong.
- Determinism: none of these may touch the wall clock or make the engine core
  non-deterministic. All new logic lives on the adapter/engine seam already used by
  `RequestedChange`; replay/determinism must be unaffected (assert by keeping the
  existing replay tests green).

## Ordering recommendation

A (death visibility) → C (fixpoint test, cheap, may surface B issues) → B (echo
convergence) → D (docs). A is the highest-value reliability win and independent; C
is cheap and de-risks B; B is the subtlest; D is trivial and can land anytime.

## Explicitly out of scope (do not scope-creep into these)

- Bridged-device **naming** in Apple Home — accepted controller limitation, not a bug
  (see northbound design doc §2b). No work here.
- Node-thread **auto-restart** — a legitimate follow-up to Task A but separately
  scoped (fabric-store reopen + mDNS re-announce hazards).
- **Production attestation** (real DAC) — a business/compliance step, not code
  (handoff §5 item 2).
- New **capabilities** (Occupancy/Battery clusters) — orthogonal feature work.
- Anything on the **rs-matter version** — stay pinned at `0.2.0`.

## Implementation notes (what actually shipped)

- **Gap #1 (startup sync):** confirmed already present; no code added.
- **Task A (node death):** `AliveGuard` + `Arc<AtomicBool>` liveness in
  `real_transport/mod.rs` (cleared on `run_node` return *or* panic-unwind);
  `MatterTransport::is_healthy` (default `true`) read in
  `MatterDeviceAdapter::tick`, which logs once (a `degraded` flag) and makes
  `publish` a no-op once dead. `InMemoryMatter::kill` added for tests. Auto-restart
  left as a `TODO(reliability)` marker by `connect`.
- **Task B (echo convergence):** implemented **engine-side**, not in the adapter —
  after handling an `Event::RequestedChange` in `Engine::drain`, the engine re-fans
  the store's current value for that `(device, kind)` to northbound adapters via the
  existing `fan_state_folded`. So the optimistic mirror snaps back to truth one hop
  after any write, accepted or rejected. Southbound adapters are unaffected.
  - **Known edge (accepted, documented):** if the store holds *no prior value* for
    the capability (a device that has never reported and rejects its very first
    write), `state.get` is `None` and the mirror keeps the optimistic value. Forcing
    a revert-to-unknown would be worse; this degenerate case is left as-is.
- **Task C (fixpoint):** test-only; `an_accepted_controller_write_converges_and_settles`
  asserts convergence + no oscillation + zero cascade drops. No production change
  needed (no oscillation was found).
- **Task D (color docs):** README "Known limitations of the Matter bridge" (covers
  both the naming caveat and color-desync), example YAML note, and a README pointer
  added to the `hooks.rs` inline comment.
- **Tests:** `tests/matter_device.rs` gained `a_dead_node_is_reported_once_…`,
  `a_rejected_controller_write_reverts_the_mirror_to_engine_truth`, and
  `an_accepted_controller_write_converges_and_settles` (23 tests total in that file,
  all green).
