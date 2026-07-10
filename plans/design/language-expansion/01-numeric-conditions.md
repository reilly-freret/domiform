# Feature A: generic numeric condition verb

Status: proposed. Part of [config-language expansion](00-overview.md). **Ship
first** — no vocabulary or engine change, highest value-to-effort ratio, ideal
first contribution.

## Problem

`Condition::Compare { device, kind, op, value: i64 }` (`src/rule.rs`) already
supports comparing *any* numeric capability against a constant. The engine
evaluates it; the store holds the values (`num_value` via `CapabilityState::as_i64`
covers `Brightness`, `Battery`, `ColorTemperature`, `TimeOfDay`, and any future
sensor from feature C). But `src/compile/lower.rs` only emits `Compare` for
**time** (`time_after` / `time_before`). There is no config verb to write, e.g.:

- "if battery below 15%" → `Compare { kind: Battery, op: Lt, value: 15 }`
- "if brightness at or above 80%" → `Compare { kind: Brightness, op: Ge, value: 80 }`

So a whole class of conditions the engine can already evaluate is unreachable from
config. This is a pure "expose what exists" gap.

## Design

Add **one** general condition verb, `compare`. The general form:

```yaml
if:
  compare: { device: hallway_lamp, capability: brightness, op: ">=", value: 80 }
```

`op` is one of `<`, `<=`, `==`, `!=`, `>=`, `>` mapping to `CmpOp`
(`Lt/Le/Eq/Ne/Ge/Gt`). `capability` is a capability *name* string reusing the
existing `parse_capability`-style mapping, restricted to numeric-shaped kinds.

### On ergonomic sugar — deliberately deferred

A tempting follow-on is per-capability sugar verbs (`battery_below`,
`brightness_is`, …) that lower to the same `Condition::Compare`. **Ship v1 with
only the general `compare` verb** and no sugar. Rationale, in service of a
consistent surface: sugar aliases create two ways to write one condition, which is
exactly the kind of redundancy that makes a config language harder to learn and
document ("do I write `battery_below` or `compare`?"). One canonical verb is more
intuitive than a general verb plus a partial, arbitrary set of shortcuts.

If real configs later show a specific pattern is painfully verbose, a *single*
well-chosen sugar verb can be added then as a conscious ergonomics decision — but
never a full `<cap>_<op>` matrix, and never "to be safe." Start canonical.

## Implementation

Everything is in `src/compile/lower.rs` (plus a small AST payload struct).

1. **AST payload** (`src/compile/ast.rs`): a `RawCompare` struct with
   `device: String`, `capability: String`, `op: String`, `value: i64`, with
   `#[serde(deny_unknown_fields)]`.

2. **Op parsing** in `lower.rs`: a helper `parse_cmp_op(&str) -> Option<CmpOp>`
   mapping the six operator strings. Emit `E_BAD_OP` on anything else.

3. **Numeric-capability parsing**: a helper that maps a capability name to a
   `CapabilityKind` **and rejects non-numeric kinds** (a `compare` on `Switch`
   makes no sense — that's what the `switch` bool verb is for). Reuse/extend the
   `parse_capability` mapping from `resolve.rs`; emit `E_BAD_CAPABILITY` /
   `E_NON_NUMERIC_CAPABILITY` as appropriate.

4. **New arm(s)** in `Lowerer::condition`'s match (`lower.rs` ~line 248), e.g.:

   ```rust
   "compare" => {
       let c: RawCompare = self.payload(payload, "compare", at)?;
       let device = self.resolve_device(&c.device, at)?;
       let (kind, op) = (self.numeric_capability(&c.capability, at)?,
                         self.parse_cmp_op(&c.op, at)?);
       self.require_cap(device, &c.device, kind, "compare", at);
       Condition::Compare { device, kind, op, value: c.value }
   }
   ```

5. **`require_cap`** already exists and does exactly the right check (device
   declares the capability), and already exempts the synthetic clock device. Reuse
   it — no change needed.

## Tests

Add to `tests/state_conditions.rs` (or a new `tests/numeric_conditions.rs`),
following the existing pattern of driving a compiled config through the engine:

- `battery_below` gates a rule: with battery reported at 10, rule fires; at 50, it
  doesn't; **never reported → `Unknown`, does not fire** (the critical invariant).
- Each of the six operators lowers and evaluates correctly.
- `compare` on a non-numeric capability (`switch`) is a compile error.
- `compare` on a capability the device doesn't declare is a compile error
  (`require_cap`).
- `compare` on the clock device's `time_of_day` works (exempt from `require_cap`)
  — this is what `time_after`/`time_before` already do under the hood; the general
  verb should reach it too.

## Determinism / invariant notes

- **No engine change**, so no determinism risk. `Condition::eval` already yields
  `Truth::Unknown` for a never-reported capability (`num_value` returns `None`).
  Verify the "never reported → Unknown" test explicitly — it's the whole safety
  story.

## Future (not in scope)

- **Cross-device comparison** ("A brighter than B"): would require a `Compare`
  variant whose RHS is another `(device, capability)` read rather than a constant.
  That *is* an engine change (`rule.rs`) and a real design fork (what if the RHS is
  `Unknown`?). Defer until there's demand; note it here so it isn't reinvented.
- **Hysteresis** (two-threshold on/off) is expressible today with two rules and
  two constants once this verb exists — no special support needed.
