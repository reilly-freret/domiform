# Feature C: expanded capability definitions (valued sensors)

Status: proposed. Part of [config-language expansion](00-overview.md). **The
highest-risk, least-reversible change** in this plan — it edits the canonical
vocabulary. Land it before feature D so on-change triggers can include sensors.

## Problem

`CapabilityKind` is a closed enum of 8 (`Switch`, `Brightness`, `Color`,
`ColorTemperature`, `Occupancy`, `Battery`, `IrTransmitter`, `TimeOfDay`, `SunUp`).
There is no `Temperature`, `Humidity`, `Power`, `Illuminance`, `Contact`
(door/window open), `WaterLeak`, `Smoke`, etc. — the bread-and-butter of real
automations. Today domiform is effectively a switch/light/motion/button system; it
can't do climate, energy, or contact-based automation.

## Why this is the "least-reversible" change

Per `ARCHITECTURE.md` (Stop 4) and `model.rs`'s own opening comment, `Event` /
`Command` / `CapabilityState` are the vocabulary *everything* is shaped around.
Adding a `CapabilityKind`/`CapabilityState` variant ripples through every
exhaustive `match` on those enums. That's a feature of Rust's exhaustiveness, not a
bug: the compiler produces a **checklist** of every site that must handle the new
capability. But it's wide, so do it deliberately, one capability family at a time,
and let the compiler drive.

## Scope decision: which capabilities, and their shapes

Group the additions by their value-shape, because that determines how much
plumbing each needs:

| Capability | Shape | `as_i64`? | `as_bool`? | Notes |
|---|---|---|---|---|
| `Temperature` | numeric | yes | — | Store as centidegrees or a fixed unit (see below) |
| `Humidity` | numeric (0–100%) | yes | — | |
| `Illuminance` | numeric (lux) | yes | — | Range is large; `i64` fine |
| `Power` | numeric (watts) | yes | — | |
| `Energy` | numeric (Wh/kWh) | yes | — | Monotonic; consider separately |
| `Contact` | bool | — | yes | open/closed door/window |
| `WaterLeak` | bool | — | yes | |
| `Smoke` | bool | — | yes | |

**Numeric-shaped** sensors slot into the existing machinery almost for free once
added: `as_i64` makes them work with feature A's `compare` verb immediately.
**Bool-shaped** sensors slot into `as_bool` and can reuse a `BoolEquals`-style
condition verb. This is why features A and C compose so well — A gives you the
condition verb, C gives you the values it reads.

### Units — decide once, document forever

The engine stores raw `i64`/`u8`/etc.; it does not know units. Pick a **canonical
internal unit per capability** and make every adapter convert to it on fold (the
same way `TimeOfDay` is always "minutes since midnight" and brightness is always
0–100%). E.g. temperature as **centidegrees Celsius** (`2350` = 23.5°C) to keep
integer precision without floats. Document the canonical unit in the
`CapabilityState` variant's doc comment — it is a permanent contract every adapter
depends on. **Do not** let different adapters store different units for the same
capability; that silently breaks cross-adapter rules.

Config-facing values can be friendlier (`23.5`, `73°F`) and convert at compile
time (as `set_color_temperature` already converts kelvin→mireds in `lower.rs`).

## Implementation (per capability family)

Follow the compiler; the touch-points are always the same set:

1. **`src/model.rs`**: add the `CapabilityKind` variant and the matching
   `CapabilityState` variant (with a doc comment stating the canonical unit).
   Extend `CapabilityState::kind()`, and `as_i64` (numeric) or `as_bool` (bool).
   The compiler now lists every other match to fix.

2. **`src/compile/resolve.rs`**: extend `parse_capability` so the capability name
   is valid in a device's `capabilities:` list.

3. **Adapters** (`src/adapters/*.rs`): in each adapter's **pure** `*_to_events`
   translation (e.g. z2m's `json_to_device_events`), fold the protocol's native
   report into the new `CapabilityState`, converting to the canonical unit. Add
   startup **priming** where the protocol supports a read (z2m's `read_attr` /
   `/get`), so the capability isn't stuck `Unknown` — otherwise sensor-gated rules
   silently never fire (invariant #3). Sleepy/report-on-schedule sensors that can't
   answer a read stay `None` until their first report, which is correct.

4. **Conditions** come from feature A (numeric) and a bool verb (bool-shaped). If C
   lands before A, add at least a minimal condition path so the new capabilities
   are usable; otherwise they can only be *reported*, not *acted on*, until A/D.

5. **Commands**: most sensors are **read-only** — no `Command` variant, and
   `command_for_requested_change` returns `None` for them (like `Battery`/`TimeOfDay`
   today). Only add a `Command` for a capability that is actually writable.

## Tests

- Each new capability: an adapter fold test (protocol JSON → correct
  `CapabilityState` in canonical units), driven through `message_to_events` /
  the adapter's pure translation — no broker.
- A condition gate per capability (fires above/below threshold; **Unknown when
  unreported**).
- A round-trip unit test (config value → stored canonical → condition compares
  correctly), especially for temperature/kelvin-style conversions.
- Priming: first `tick` issues the read request for the new readable capability.

## Determinism / invariant notes

- The canonical-unit contract (invariant, and this doc's central rule) is what
  keeps cross-adapter rules correct. Enforce it in review: a fold that stores a
  non-canonical value is a bug even if its own tests pass.
- Read-only sensors must yield `None` from `command_for_requested_change` — a
  northbound write to a sensor is a harmless no-op, never an error (matches the
  existing `Battery`/`TimeOfDay` treatment).
- Adding variants is safe *because* of exhaustive matching; resist the urge to add
  a catch-all `_ =>` arm anywhere handling `CapabilityState`, which would defeat
  the compiler's checklist and let a future capability silently fall through.

## Sequencing note

This doc's value compounds with feature D: once sensors report *values*, D lets
rules trigger *on* those reports ("temperature crossed 25°C"). Landing C first
means D ships with sensor triggers from day one. See
[`04-on-change-triggers.md`](04-on-change-triggers.md).
