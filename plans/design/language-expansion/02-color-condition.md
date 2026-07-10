# Feature B: `color_is` condition

Status: proposed. Part of [config-language expansion](00-overview.md). Cheap,
independent — ship anytime. Closes a documented half-built path.

## Problem

`CapabilityState::Color { r, g, b }` is fully plumbed on the *write* and *fold*
paths:

- Every adapter folds inbound color reports into `CapabilityState::Color` (sRGB) —
  `model.rs` `CapabilityKind::Color` explicitly notes this.
- `set_color` exists as a command (`lower.rs`, `Command::SetColor`).

But there is **no condition that reads color**. `model.rs` says so directly:
*"Conditions that read color (e.g. `color_is`) are not implemented yet, but every
adapter already folds inbound reports... so the canonical read path exists when a
condition form is added."* So "if the light is currently red, do X" is unstateable
despite the entire read path existing.

## Why this needs an engine change (unlike feature A)

Color is neither bool-shaped nor numeric-shaped: `CapabilityState::as_bool` and
`as_i64` both return `None` for it. So the two existing condition leaves
(`BoolEquals`, `Compare`) can't express it. This feature adds **one new leaf** to
`Condition` and **one eval arm** — a small, contained vocabulary addition, not a
wide one.

## Design

### Config surface

```yaml
if: { color_is: { device: strip, color: "#FF0000" } }        # exact sRGB match
if: { color_is: { device: strip, color: { r: 255, g: 0, b: 0 } } }
```

Reuse the existing `parse_color` helper in `lower.rs` (handles `#RRGGBB` and
`{ r, g, b }`) — the color-parsing is already written and shared with `set_color`.

### Exact match vs. tolerance — decide explicitly

Exact `(r,g,b)` equality is the simplest semantics and the right **v1**. But real
devices round-trip color through HSV/XY color spaces, so a light *set* to
`#FF0000` may *report* `#FE0100`. Exact match will surprise users.

**Recommendation:** ship v1 as exact equality (simplest, deterministic, matches the
store's representation), and document the caveat. If demand appears, add an
optional `tolerance` (e.g. max per-channel delta, or Euclidean distance in sRGB)
as a follow-up — a `ColorEquals { .., tolerance: u8 }` field. Do **not** silently
add fuzzy matching in v1; explicit is better.

## Implementation

1. **`Condition` leaf** (`src/rule.rs`): add
   ```rust
   ColorEquals { device: DeviceId, r: u8, g: u8, b: u8 },
   ```
   (Add a `tolerance` field later if pursued; v1 omits it.)

2. **Eval arm** (`Condition::eval`, `rule.rs`): read the stored color and compare.
   Critically, yield `Truth::Unknown` when no color has been reported:
   ```rust
   Condition::ColorEquals { device, r, g, b } =>
       match state.get(*device, CapabilityKind::Color) {
           Some(CapabilityState::Color { r: cr, g: cg, b: cb }) =>
               Truth::from_bool(cr == r && cg == g && cb == b),
           _ => Truth::Unknown,
       },
   ```
   Note: `StateStore` already has `get(device, kind)`; no new store method needed.
   (`as_bool`/`as_i64` don't apply to color, which is exactly why this reads `get`
   directly.)

3. **Lowering arm** (`Lowerer::condition`, `lower.rs`): add `"color_is"`, resolve
   device, `require_cap(.., CapabilityKind::Color, "color_is", ..)`, parse color
   via the existing `parse_color`, emit `Condition::ColorEquals`.

4. The `Condition` enum gains a variant, so the compiler will flag any exhaustive
   `match` on `Condition` that needs the new arm — follow the compiler.

## Tests

Add to `tests/state_conditions.rs` (or `tests/color.rs`):

- Light reports `#FF0000`; `color_is #FF0000` fires, `color_is #00FF00` doesn't.
- **Color never reported → `Unknown`, does not fire** (the invariant).
- Both config color forms (`#RRGGBB` and `{ r, g, b }`) lower identically.
- `color_is` on a device that doesn't declare `color` is a compile error.

## Determinism / invariant notes

- Pure engine addition, no time, no I/O — deterministic by construction.
- The `Unknown`-when-unreported arm is mandatory (invariant #3). Test it.
