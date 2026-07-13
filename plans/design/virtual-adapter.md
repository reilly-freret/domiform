# Design: the `virtual` adapter

Status: **IMPLEMENTED** on `language_improvements`. Scope: a small, first-class
adapter for **domiform-owned stateful devices with no physical backing** — the
missing noun that lets a stateless appliance (an IR-only air conditioner) be
surfaced as a tappable, stateful device in Apple Home via `matter_device`.

## Motivation (the real use case)

A "dumb" air conditioner is controllable only by an IR remote. In domiform today
you model this southbound with two real devices and a rule:

- a wall button — an **event-only** device (`events: [press]`, no capabilities);
- an IR blaster — a **command sink** (`capabilities: [ir_transmitter]`);
- a rule: `when press of wall_button → send_ir_code(toggle) via ir_blaster`.

This works great on the wall. But you can't expose it to Apple Home the same way,
because:

- `matter_device` projects **capabilities** onto Matter clusters; it cannot project
  a stateless **event**. There is no "expose a button press" path.
- Apple Home has no satisfying UI for a stateless button anyway. What gives a good
  tappable tile is a **stateful On/Off switch** — and the AC *has* a power state; it
  just lives only in domiform's head, because write-only IR can't report it back.

## Decision

Don't try to expose the button. Expose a **virtual stateful switch** that fronts the
AC, and let a rule translate its state changes into IR. This needs exactly one new
thing: an adapter that **owns state for a device with no physical backing**.

That adapter is `type: virtual`: on any state-setting command
(`SetSwitch`/`SetBrightness`/`SetColor`/`SetColorTemperature`) it echoes the
commanded value straight back as `Event::StateReported`, so the engine folds it into
truth. This is *exactly* what the internal `mock` adapter already does — `virtual` is
`mock` promoted to a documented, config-facing feature with an honest name (`mock`
stays what it is: a test/fallback stand-in).

**`matter_device` is unchanged.** It already exposes any switch-capable device, so
`expose: [ac_power]` surfaces the virtual switch as a real On/Off tile.

### Why a new type rather than reusing `mock`

`mock` reads as a test artifact in a real config and conflates "test/fallback
stand-in" with "intentional virtual device." A distinct `virtual` tag states intent.
The two share an implementation shape but not a meaning.

## The end-to-end shape

```yaml
adapters:
  z2m:     { type: zigbee2mqtt, url: mqtt://... }
  virtual: { type: virtual }
  home:    { type: matter_device, expose: [ac_power] }

devices:
  wall_button:      { adapter: z2m, address: wall_1, events: [press] }
  ir_blaster:       { adapter: z2m, address: ir_1, capabilities: [ir_transmitter] }
  ac_power:         { adapter: virtual, capabilities: [switch] }  # virtual switch

rules:
  # Apple-Home tap OR wall button → the switch changes → fire IR.
  - when: { changed: ac_power }
    then:
      - send_ir_code: { device: ir_blaster, code: "<ac-toggle>" }
  # Keep one source of truth: the button toggles the same virtual switch.
  - when: { press: wall_button }
    then:
      - toggle: ac_power
```

Data flow for an Apple-Home tap (all existing machinery):

```
Home tap → matter_device → Event::RequestedChange{ac_power, Switch(on)}
  → engine lowers to Command::SetSwitch          (command_for_requested_change)
  → virtual adapter echoes StateReported{ac_power, Switch(on)}
  → fold_state → store truth + mirror back to the Matter node (tile stays correct)
  → rule `changed: ac_power` fires → send_ir_code → z2m IR blaster
```

Nothing about IR semantics (toggle vs. discrete on/off codes) lives in the adapter:
that is purely a **rule-authoring** choice, and `then:` already supports arbitrary
commands. The adapter only owns switch state.

## What shipped

- **`src/adapters/virtual_device.rs`** — the plugin (`type: virtual`,
  `VirtualDeviceAdapter`), echo-on-dispatch identical to `mock`, with a doc comment
  framing the feature. ~40 lines.
- **`src/adapters/mod.rs`** — `mod virtual_device;`, `&virtual_device::PLUGIN`
  appended to `PLUGINS`, and a `pub use`. The only registry edit (the "one line a new
  adapter adds" contract).
- **`schema/domiform.schema.json`** — `virtual` added to the adapter `type` oneOf.
- **Docs/example** — README section + `examples/virtual_ac.yaml` demonstrating the
  three-device pattern.
- **Tests (`tests/virtual_device.rs`)** — the echo contract, and an end-to-end
  "Apple-Home tap on a virtual switch drives an IR rule" through the engine with the
  in-memory Matter transport.

## Known limitation (documented, not a bug)

Because write-only IR can't report the AC's real state, `ac_power` is domiform's
**belief**, not ground truth. If the OEM remote is used, belief and reality diverge,
and a subsequent toggle flips the AC "backwards" relative to the tile. Inherent to
IR; documented in the README so it isn't mistaken for a domiform bug. Discrete
on/off IR codes (a rule per direction) mitigate it where the AC supports them.

## Explicitly out of scope

- Exposing device **events** (button presses) to Matter — no good Apple-Home UX;
  the virtual-switch pattern is the intended answer.
- Any change to `matter_device` — it already does its part.
- Persisting virtual state across restarts — virtual devices boot to their default
  (off / unset) like any un-reported device; a startup rule can seed them if needed.
  Revisit only if a concrete need appears.
