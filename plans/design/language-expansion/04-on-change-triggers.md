# Feature D: on-change / state-report triggers

Status: proposed. Part of [config-language expansion](00-overview.md). **The
keystone.** Land feature C (expanded capabilities) first so this ships with sensor
triggers. This is the most design-heavy feature; read it fully before implementing.

## Problem

The trigger surface (`src/rule.rs` `Trigger`, dispatched in `lower.rs`) is:
`event`, `occupancy` / `occupancy_clear`, `timer`, `schedule`, `command_failed`.
There is **no way to trigger on a device reporting a state/value**. So:

- "if it gets too hot, turn on the fan" — unstateable.
- "when the door opens, turn on the porch light" — unstateable (needs `Contact`
  from feature C *and* a report trigger).
- "when brightness drops below 20%, ..." — unstateable.

Numeric/color/sensor capabilities can be *read in conditions* but can never *wake*
a rule. The engine already folds `StateReported` into the store and matches every
rule against every event — but there's no `Trigger` variant meaning "device X
reported capability Y." This feature adds it.

## The central design fork: edge vs. level

This is the decision that must be made explicitly; getting it wrong makes triggers
either spammy or laggy.

- **Level trigger**: fires *every time* a matching report arrives while a predicate
  holds. "Temperature reported and temp ≥ 25" fires on *every* temperature report
  above 25 — which for a sensor reporting every 30s is a flood. Bounded by causal
  depth, but semantically wrong for "turn on the fan once."
- **Edge trigger**: fires only on the *transition* — when the predicate flips from
  not-holding to holding (or a value crosses a threshold). "Fire when temp *crosses*
  25 upward." This is what users almost always mean, and what the existing
  `occupancy` trigger effectively is (it fires on the *change* to occupied).

**Recommendation: edge semantics as the default**, matching occupancy's existing
behavior and user intuition. A level/"on every report" mode can be an explicit
opt-in later if needed, but should not be the default.

### The ordering subtlety (important — read `engine.rs` `drain` first)

Edge detection needs the **previous** value to compare against the new one. But the
drain loop currently **folds state *before* matching rules** (`fold_state` then the
rule loop — see `ARCHITECTURE.md` Stop 3, and the comment in `engine.rs` about
conditions reading the post-fold store). By the time a trigger is evaluated, the
store already holds the *new* value; the old one is gone.

Three viable approaches, in order of preference:

1. **Pass the prior value to `Trigger::matches`.** Before folding, capture the
   store's current value for the reported `(device, capability)`; fold; then when
   matching an on-change trigger, hand it `(prior, new)` so it can detect the edge.
   This is a localized change to the drain loop (capture-before-fold) and keeps the
   trigger pure. **Preferred.**

2. **Fold after matching for report events.** Reorder so on-change triggers see the
   pre-fold store as "previous" and the event payload as "new." Riskier: it changes
   the invariant that *conditions* read post-fold state (rules rely on "the event
   that triggered me is already reflected in the store"). Do not do this without
   auditing every condition-reads-triggering-event assumption.

3. **Store keeps a one-deep history** (`prev` alongside `current` per key). More
   memory and a broader `StateStore` change; only worth it if multiple features
   need history. Overkill for now.

Approach **1** is the recommendation: capture the prior value in `drain` for
`StateReported` / `OccupancyChanged` events, fold, then evaluate on-change triggers
with both values. Keep the deterministic contract intact — this is all synchronous,
in-loop, no time or I/O.

## Config surface

```yaml
# fire when a boolean capability changes to a value (edge)
when: { changed: { device: front_door, capability: contact, to: open } }

# fire when a numeric capability crosses a threshold (edge, directional)
when: { crosses: { device: thermostat, capability: temperature, above: 25.0 } }
when: { crosses: { device: thermostat, capability: temperature, below: 18.0 } }

# fire on any report of a capability (level / opt-in)
when: { reports: { device: power_meter, capability: power } }
```

- `changed ... to` — bool-shaped edge (subsumes occupancy; see below).
- `crosses ... above/below` — numeric edge, directional. Fires when the value moves
  from not-satisfying to satisfying the threshold. This is the fan/climate case.
- `reports` — explicit level trigger (every report), for metering/logging-style
  rules. Opt-in, clearly named so its spamminess is a choice.

Exact verb names are a bikeshed; the *semantics* (edge default, explicit level
opt-in, directional numeric crossing) are what matter.

## Vocabulary changes

1. **`Event`** (`model.rs`): on-change triggers match against `StateReported` and
   `OccupancyChanged`, which already exist. **No new `Event` variant needed** —
   this is the elegant part. The trigger matches on events already flowing.

2. **`Trigger`** (`rule.rs`): add variant(s), e.g.
   ```rust
   Changed  { device: DeviceId, kind: CapabilityKind, to: BoolOrValue },   // edge, bool
   Crosses  { device: DeviceId, kind: CapabilityKind, bound: i64, dir: CrossDir }, // edge, numeric
   Reports  { device: DeviceId, kind: CapabilityKind },                    // level
   ```
   `Trigger::matches` gains access to the prior value (per the ordering fix). For
   edge triggers, `matches` returns true only when `prior` did not satisfy and
   `new` does.

## Deleting the occupancy special case (resolves the side-question)

`Trigger::Occupancy` / `Event::OccupancyChanged` are a **historical special case** —
occupancy is the only stateful capability with its own dedicated trigger *and* its
own event type, because "motion → light" predated any general report-trigger. This
feature is the general mechanism occupancy prefigured, so occupancy should stop
being special. There is a single user; there is no compat to preserve, so **do the
clean break**, not a sugar-preserving shim.

**Required path — fully collapse occupancy into the general model:**

1. **Delete `Event::OccupancyChanged`.** Occupancy reports ride the same
   `Event::StateReported { state: CapabilityState::Occupancy(bool) }` as every other
   stateful capability. Update every adapter's fold path (`*_to_events`) that
   currently emits `OccupancyChanged` to emit `StateReported` instead. `fold_state`
   in `engine.rs` loses its dedicated `OccupancyChanged` arm — occupancy folds
   through the ordinary `StateReported` arm.
2. **Delete `Trigger::Occupancy`.** Occupancy triggers become the general
   `changed { device, capability: occupancy, to: true/false }`.
3. **Delete the `occupancy` / `occupancy_clear` config verbs.** Replace them in
   config with the general `changed` verb. Update `examples/*.yaml` and every test
   config to the new form. This is a deliberate, breaking config change — the whole
   point.
4. **Result:** occupancy is now indistinguishable from `contact`, `water_leak`, or
   any other bool-shaped capability (feature C) — one report event, one trigger
   verb, zero special cases. The data model is strictly more consistent.

Do not add a compatibility alias. If the old `occupancy:` verb is desired *later*
purely for brevity, it can be reintroduced as sugar then — but ship the clean model
first so the general path is the canonical one, not an afterthought bolted beside a
preserved special case.

## Implementation order

1. Drain-loop prior-value capture (approach 1 above), with a test proving edge
   detection sees the transition.
2. `Trigger` variants + `matches` logic (pure, testable in isolation like the
   existing `Trigger::matches` unit tests).
3. `lower.rs` arms for `changed` / `crosses` / `reports`, with `require_cap` and
   numeric/bool capability validation (reuse feature A's op/capability helpers).
4. Delete `Trigger::Occupancy`, `Event::OccupancyChanged`, and the `occupancy` /
   `occupancy_clear` verbs; migrate every example and test config to `changed`.

## Tests

- Edge: temperature reports 20, 24, 26 → `crosses above 25` fires **once** (on the
  24→26 report), not on subsequent reports of 27, 28.
- Directional: `crosses below 18` does not fire on an upward crossing.
- Level: `reports power` fires on every power report.
- Bool edge: `changed contact to open` fires on closed→open, not on repeated open
  reports.
- **Occupancy migration:** the motion-light tests, rewritten to use `changed
  { capability: occupancy }`, behave identically to the old `occupancy` trigger
  (same firings on the same report sequence). The old `occupancy` verb and
  `Event::OccupancyChanged` no longer exist and referencing either is a compile
  error / build error.
- Determinism: same report sequence → same trigger firings, driven by
  `inject`/`advance` (no time or threads).

## Determinism / invariant notes

- All edge detection is synchronous and in-loop; no wall-clock, no threads — the
  contract holds.
- Missing prior value (first-ever report): define the edge semantics explicitly.
  Recommended — a first report that *satisfies* the predicate **does** count as an
  edge (not-known → satisfied is a transition), so "if it's already hot at boot,
  turn on the fan" works. Document and test this boundary; it's the analogue of the
  `Unknown` question for triggers.
