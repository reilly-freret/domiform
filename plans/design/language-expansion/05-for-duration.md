# Feature E: for-duration (sustained) conditions/triggers

Status: proposed. Part of [config-language expansion](00-overview.md). **Last in
sequence** — depends on feature D's edge/level trigger model. This is an engine
feature (per-rule timer state), not just a config verb.

## Problem

The single most common automation after "motion → light" is **"motion clear *for*
5 minutes → turn off."** Today this is only expressible as a hand-rolled timer
choreography:

```yaml
# rule 1: on occupancy_clear, schedule a timer
when: { occupancy_clear: hallway_motion }
then: [ { schedule_timer: { key: hallway_off, after: 5m } } ]
# rule 2: on that timer, turn off
when: { timer: hallway_off }
then: [ { turn_off: hallway_light } ]
# rule 3: on new motion, cancel the timer
when: { occupancy: hallway_motion }
then: [ { cancel_timer: hallway_off } ]
```

(`examples/hallway.yaml` likely does exactly this.) It works, but:

- It's three rules and a named timer for one conceptual idea.
- The cancel-on-retrigger is easy to forget → the light turns off even though
  motion resumed (a classic footgun). The `E_DANGLING_TIMER` lint catches a
  *missing* schedule but not a *missing cancel*.
- It doesn't generalize: "temperature above 25 *for* 10 minutes" needs the same
  dance around a numeric condition.

A first-class "condition/trigger has held for duration T" primitive collapses this
to one declaration and removes the footgun.

## Design

Two possible surfaces; they're related but distinct:

### E1. `for`-qualified trigger (sustained edge)

```yaml
when:
  changed: { device: hallway_motion, capability: occupancy, to: false }
  for: 5m
then: [ { turn_off: hallway_light } ]
```

Semantics: when the trigger's edge fires, start a per-rule timer for `5m`; if the
condition still holds when it elapses, fire the rule's commands; if the state
*reverts* before then (new motion), cancel automatically. This is the occupancy
case, generalized and made safe (auto-cancel is built in).

### E2. `for`-qualified condition (sustained guard)

```yaml
when: { reports: { device: thermostat, capability: temperature } }
if: { compare: { device: thermostat, capability: temperature, op: ">=", value: 25 }, for: 10m }
then: [ { turn_on: attic_fan } ]
```

Semantics: the condition is only `True` if it has *continuously* held for `10m`.

E1 (sustained trigger) is the higher-value, more common case and the recommended
**v1**. E2 (sustained condition) is more general but needs the engine to track
"how long has this condition been continuously true," which is a bigger lift.
**Ship E1 first.**

## Why this is an engine feature

Unlike features A/B (compiler-only) and even C/D, a `for` qualifier requires the
engine to hold **per-rule pending-timer state** and manage the schedule/cancel
lifecycle *automatically* — exactly the choreography users do by hand today, moved
into the engine. This is new mutable state on the `Engine`.

Crucially, it must reuse the **existing scheduler adapter** (`ScheduleTimer` /
`CancelTimer` / `TimerElapsed`), not a new timing mechanism — so it stays in
virtual time and fully replayable (invariant #1). A `for`-trigger internally:

1. On edge-fire, dispatches a `ScheduleTimer` with an engine-generated key (like
   the retry mechanism's `__retry:` keys — see `engine.rs` `schedule_retry`).
2. On the auto-cancel condition (state reverts), dispatches `CancelTimer`.
3. On `TimerElapsed` for that key, re-checks the condition still holds, then fires
   the rule's commands.

Model it closely on the **existing retry machinery**, which is the proof that
"engine-managed future events keyed by generated keys, routed through the
scheduler" already works and stays deterministic. Reuse that pattern; don't invent
a parallel one.

## Implementation sketch

1. **Engine state**: a map from a generated timer key to "pending sustained fire"
   (which rule, what commands, what condition to re-verify). Analogous to
   `retries: HashMap<TimerKey, PendingRetry>`.

2. **Key namespace**: reserve a prefix (e.g. `__for:`) like `__retry:`, and
   intercept those `TimerElapsed` events in `drain` *before* rule matching (exactly
   as retry timers are intercepted), so they re-verify + fire rather than acting as
   a user-visible trigger.

3. **Auto-cancel**: when a subsequent event changes the watched state such that the
   sustained predicate no longer holds, cancel the pending timer. This requires the
   engine to know *what to watch* — tie it to the trigger's `(device, capability)`.

4. **Compiler** (`lower.rs`): parse the optional `for:` field on a trigger (E1) /
   condition (E2), lower to a duration `Millis` via the existing `parse_duration`.
   The `RawRule` AST already has `when` and `if` as `Value`; `for` is a sibling
   field on the rule or nested in the trigger node — decide and keep it consistent.

5. **Re-verification on elapse**: when the timer fires, re-evaluate the condition
   against the *current* store before firing (state may have changed in a way that
   didn't trigger an explicit cancel). Belt-and-suspenders; keeps semantics honest.

## Tests

- Motion-clear-for-5m: occupancy goes false, advance 5m with no new motion → light
  turns off. New motion at 3m → advance past 5m → light stays on (auto-cancel).
- Re-verification: condition satisfied at schedule time but reverted by elapse
  (via a path that didn't cancel) → does **not** fire.
- Determinism: the whole sequence driven by `advance` in virtual time; identical
  advances → identical firings. No `Instant::now()` anywhere.
- The generated `__for:` keys don't collide with user timer keys or `__retry:`
  keys, and don't leak into user-visible timer triggers.

## Determinism / invariant notes

- **This is the feature most at risk of violating invariant #1** (no wall-clock in
  the engine). It must route *all* timing through the scheduler adapter in virtual
  time, exactly like retries. A `std::thread::sleep` or `Instant::now()` here would
  break replay — reject any such implementation.
- Because it reuses the scheduler, `next_wake_delay` already accounts for pending
  `for` timers (they're normal scheduled timers), so the host loop wakes correctly
  with no `main.rs` change.

## Relationship to feature D

E1's "sustained edge" is literally "feature D's edge trigger, plus a debounce
timer, plus auto-cancel." Building E before D would mean inventing D's edge
semantics ad hoc. This is why the sequence is D → E. Once D exists, E is
"D + timer lifecycle," a clean increment.
