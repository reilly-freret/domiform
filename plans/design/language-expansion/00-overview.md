# Design: config-language expansion (sensors, on-change, durations)

Status: proposed. This is an umbrella plan for five related additions to the
config language and, where required, the engine vocabulary. It sequences the work
and records the dependencies; each feature has its own focused doc.

Read `ARCHITECTURE.md` at the repo root first — this plan assumes the layer model
(host / compiler / engine / vocabulary / adapters), the determinism contract, and
the "time is an adapter" and three-valued-`Truth` ideas it describes.

## The through-line

Two observations from an architecture review motivate all five features:

1. **The engine is more capable than the config language exposes.** `Condition::Compare`
   already handles any numeric capability; the store already folds `Color`; but no
   config *verb* reaches them. These are near-free "expose what exists" wins.
2. **The vocabulary is narrower than real homes.** `CapabilityKind` is a closed
   enum of 8, with no valued sensors (temperature, humidity, power, illuminance,
   contact). And no `Trigger` can fire on a device *reporting a value* — so
   "if it gets too hot, turn on the fan" is unstateable. These are structural.

Together they move domiform from "a switch/light/motion/button controller" toward
"a home automation system." The cheap wins are worth doing first because they
unlock value immediately and with near-zero core risk; the structural work
(sensors + on-change triggers) is the real roadmap decision.

## The five features

| # | Feature | Blast radius | Doc |
|---|---|---|---|
| A | Generic numeric condition verb | compiler only (engine already supports it) | [`01-numeric-conditions.md`](01-numeric-conditions.md) |
| B | `color_is` condition | compiler + one engine leaf/eval arm | [`02-color-condition.md`](02-color-condition.md) |
| C | Expanded capability definitions (valued sensors) | **vocabulary** (`CapabilityKind`/`CapabilityState`) + every adapter | [`03-expanded-capabilities.md`](03-expanded-capabilities.md) |
| D | On-change / state-report triggers | **vocabulary** (`Trigger`/`Event`) + engine drain + compiler | [`04-on-change-triggers.md`](04-on-change-triggers.md) |
| E | For-duration (sustained) conditions/triggers | engine (per-rule timer state) + compiler | [`05-for-duration.md`](05-for-duration.md) |

## Dependency graph & recommended order

```
A (numeric conditions)  ── independent, cheap ──▶ ship first
B (color_is)            ── independent, cheap ──▶ ship anytime
C (expanded caps)       ── unlocks the *values* D triggers on
        │
        ▼
D (on-change triggers)  ── the keystone; subsumes the special-cased occupancy trigger
        │
        ▼
E (for-duration)        ── builds on D's edge/level semantics + timer choreography
```

**Recommended sequence: A → B → C → D → E.**

- **A and B first.** No vocabulary change, no determinism risk, immediate value,
  and they're the ideal "learn the add-a-config-verb loop" tasks. A is nearly
  pure-compiler; B adds exactly one engine leaf.
- **C before D.** On-change triggers are far more useful once there are *valued*
  sensors to trigger on. C is a mechanical-but-wide change (the "highest-risk,
  least-reversible" vocabulary edit — the compiler will march you through every
  `match`). Landing it first means D can include sensor triggers from day one.
- **D before E.** For-duration semantics ("motion clear *for* 5m") depend on the
  edge-vs-level trigger model D must settle. Building E on an un-decided trigger
  model would mean reworking it.

A and B can be done in parallel with C by different agents; D and E are a chain.

## Cross-cutting invariants every feature must preserve

These come straight from the determinism contract (`ARCHITECTURE.md` §"Invariants
to preserve"). Any implementation that violates one is wrong regardless of tests:

1. **No wall-clock reads in the engine.** Durations (feature E) are virtual-time
   deltas driven by `advance`, scheduled through the scheduler adapter — never
   `Instant::now()`.
2. **`BTreeMap` iteration for id stability.** Any new config section keeps the
   `BTreeMap` pattern so interned ids stay stable across runs.
3. **Missing state is `Truth::Unknown`, never `False`.** New numeric/color/sensor
   conditions must yield `Unknown` when the capability has never been reported
   (features A, B, C, D), so a rule declines to act rather than acting on absent
   data. New adapters must **prime** sensor state on connect where the protocol
   allows (as z2m does today) so conditions aren't stuck `Unknown`.
4. **The bug-prone protocol translation stays in pure free functions.** New
   capability folds (feature C) live in each adapter's pure `*_to_events`
   translation, not in the transport or the `Adapter` impl.
5. **Compiler catches statically what the engine assumes.** Every new capability
   reference is checked against what the device declares (`require_cap`); a
   condition/trigger on a never-reported capability should ideally warn (the
   permanently-`Unknown` dead-rule lint noted in `rule.rs`).

## Clean-break posture: no backwards compatibility

domiform has a **single user**, so there is no compat to preserve and no migration
window to honor. Every feature in this plan should make the **most destructive API
change that yields a better, more consistent, more intuitive config language and
data model** — delete special cases outright rather than wrapping them in
compatibility shims, and rewrite `examples/*.yaml` and test configs to the new
form as part of the same change. A breaking config change is a *feature* here, not
a cost. Do not add aliases "to be safe"; a cleaner canonical surface beats a
familiar one. (This posture is distinct from the determinism invariants below,
which are permanent contracts and must be preserved.)

## A note on the occupancy trigger (why it exists, and its fate)

`Trigger::Occupancy` / `Event::OccupancyChanged` are a **historical special case**,
not a principled design: "motion → light" was the founding use case, so occupancy
got a first-class trigger and its *own dedicated event type* before any general
"trigger on a state report" mechanism existed. Every *other* stateful capability
rides `Event::StateReported`; occupancy is the lone exception.

Feature D (on-change triggers) is the general mechanism occupancy prefigures.
Per the clean-break posture above, D **deletes** the occupancy special case
entirely — `Trigger::Occupancy`, `Event::OccupancyChanged`, and the `occupancy` /
`occupancy_clear` config verbs all go, replaced by the general `changed` verb over
`CapabilityState::Occupancy` riding `StateReported`. Occupancy becomes just another
bool-shaped capability. See [`04-on-change-triggers.md`](04-on-change-triggers.md)
§"Deleting the occupancy special case".

## What is deliberately *not* here

- **Notifications / webhooks / HTTP-out.** There's no `notify` command and this
  plan doesn't add one; the intended path is a future northbound/notification
  adapter reacting to the event bus (`CommandFailed` already rides it), not a new
  `Command` variant. Out of scope.
- **Cross-device relative conditions** ("A brighter than B"). Real but niche;
  noted in feature A's "future" section, not planned here.
- **Scene state snapshot/restore.** Out of scope by the "scenes are just command
  lists" design.
