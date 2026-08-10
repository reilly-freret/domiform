//! `Event::RequestedChange` / `Event::RequestedScene`: the canonical inbound path
//! for a northbound adapter (HomeKit tap, REST call, web toggle). A requested
//! *intent* becomes the same `Command` a rule would emit and is dispatched — so a
//! Home-app tap and a physical wall switch are indistinguishable to the engine.
//! The request itself is an intent, not a report: it does not fold into the store;
//! the device's own echo does. Non-writable states (battery, occupancy, time) are
//! harmless no-ops.
//!
//! The payload is a [`Desired`], which also carries the *relative* intents
//! (`Toggle`, `AdjustBrightness`) that a rule can express but a bare
//! `CapabilityState` cannot. These are deliberately **not** resolved by the
//! adapter: they lower to relative `Command`s and are resolved against the store
//! at dispatch time, so they can't race with in-flight state.

use std::cell::RefCell;
use std::rc::Rc;

use domiform::ids::{DeviceId, SceneId};
use domiform::model::{CapabilityState, Desired, Millis};
use domiform::{
    Adapter, CapabilityKind, Command, Condition, DispatchOutcome, Engine, Event, MockNorthbound,
    Rule, RuleId, Trigger,
};

const LIGHT: DeviceId = DeviceId(1);
const UNBOUND: DeviceId = DeviceId(2);
const EVENING: SceneId = SceneId(0);

/// Records every command it's handed, and echoes state back like a real device
/// so the store stays live (mirrors `MockDeviceAdapter` / the toggle test).
#[derive(Clone, Default)]
struct Recorder(Rc<RefCell<Vec<Command>>>);

impl Recorder {
    fn commands(&self) -> Vec<Command> {
        self.0.borrow().clone()
    }
}

impl Adapter for Recorder {
    fn dispatch(&mut self, cmd: &Command, _now: Millis) -> DispatchOutcome {
        self.0.borrow_mut().push(cmd.clone());
        match cmd {
            Command::SetSwitch { device, on } => DispatchOutcome::Ok(vec![Event::StateReported {
                device: *device,
                state: CapabilityState::Switch(*on),
            }]),
            Command::SetBrightness { device, value, .. } => {
                DispatchOutcome::Ok(vec![Event::StateReported {
                    device: *device,
                    state: CapabilityState::Brightness(*value),
                }])
            }
            _ => DispatchOutcome::ok(),
        }
    }
}

/// An engine with one device (`LIGHT`) bound to a recorder, and no rules — so we
/// observe *only* the requested-change path, not any rule reaction.
fn build() -> (Engine, Recorder) {
    let recorder = Recorder::default();
    let mut engine = Engine::new();
    let idx = engine.add_adapter(Box::new(recorder.clone()));
    engine.bind_device(LIGHT, idx);
    (engine, recorder)
}

#[test]
fn requested_switch_dispatches_setswitch_and_echo_folds() {
    let (mut engine, recorder) = build();

    engine.inject(Event::RequestedChange {
        device: LIGHT,
        desired: Desired::Set(CapabilityState::Switch(true)),
    });

    // The adapter saw exactly the command a rule emitting SetSwitch would produce.
    assert_eq!(
        recorder.commands(),
        vec![Command::SetSwitch {
            device: LIGHT,
            on: true
        }]
    );
    // The store reflects the device's *echo*, not the request itself.
    assert_eq!(engine.switch_state(LIGHT), Some(true));
}

#[test]
fn requested_brightness_maps_to_setbrightness_without_transition() {
    let (mut engine, recorder) = build();

    engine.inject(Event::RequestedChange {
        device: LIGHT,
        desired: Desired::Set(CapabilityState::Brightness(40)),
    });

    assert_eq!(
        recorder.commands(),
        vec![Command::SetBrightness {
            device: LIGHT,
            value: 40,
            transition: None,
        }]
    );
}

#[test]
fn requesting_a_tap_matches_the_equivalent_rule_command() {
    // A RequestedChange(Switch(true)) must be indistinguishable, at the adapter,
    // from a rule firing `SetSwitch { on: true }`. Drive each in its own engine
    // and assert the adapter saw the same command and the store settled the same.
    const BUTTON: DeviceId = DeviceId(0);

    // Path 1: a physical button press fires a rule that sets the light on.
    let rule_rec = Recorder::default();
    let mut rule_engine = Engine::new();
    let ridx = rule_engine.add_adapter(Box::new(rule_rec.clone()));
    rule_engine.bind_device(LIGHT, ridx);
    rule_engine.add_rule(Rule::new(
        RuleId(0),
        Trigger::Changed {
            device: BUTTON,
            kind: CapabilityKind::Occupancy,
            to: true,
        },
        Condition::Always,
        vec![Command::SetSwitch {
            device: LIGHT,
            on: true,
        }],
    ));
    rule_engine.inject(Event::StateReported {
        device: BUTTON,
        state: CapabilityState::Occupancy(true),
    });

    // Path 2: a northbound request expressing the same desired state.
    let (mut req_engine, req_rec) = build();
    req_engine.inject(Event::RequestedChange {
        device: LIGHT,
        desired: Desired::Set(CapabilityState::Switch(true)),
    });

    // Identical at the adapter, and identical settled state.
    assert_eq!(req_rec.commands(), rule_rec.commands());
    assert_eq!(
        req_engine.switch_state(LIGHT),
        rule_engine.switch_state(LIGHT)
    );
}

#[test]
fn non_writable_desired_state_is_a_noop() {
    let (mut engine, recorder) = build();

    // Battery / occupancy / time have no write command: requesting them does
    // nothing rather than erroring.
    engine.inject(Event::RequestedChange {
        device: LIGHT,
        desired: Desired::Set(CapabilityState::Battery(50)),
    });
    engine.inject(Event::RequestedChange {
        device: LIGHT,
        desired: Desired::Set(CapabilityState::Occupancy(true)),
    });

    assert!(recorder.commands().is_empty());
}

#[test]
fn request_is_not_folded_into_the_store_before_the_echo() {
    // An adapter that accepts the command but produces NO echo. The store must
    // stay Unknown: a request is an intent, not a report — only a device echo
    // (StateReported) updates state.
    #[derive(Clone, Default)]
    struct Silent;
    impl Adapter for Silent {
        fn dispatch(&mut self, _cmd: &Command, _now: Millis) -> DispatchOutcome {
            DispatchOutcome::ok()
        }
    }

    let mut engine = Engine::new();
    let idx = engine.add_adapter(Box::new(Silent));
    engine.bind_device(LIGHT, idx);

    engine.inject(Event::RequestedChange {
        device: LIGHT,
        desired: Desired::Set(CapabilityState::Switch(true)),
    });

    // No echo arrived, so the request left the store untouched.
    assert_eq!(engine.switch_state(LIGHT), None);
}

#[test]
fn request_to_unbound_device_fails_like_any_command() {
    // A request targeting a device with no adapter is a permanent misconfig,
    // handled by the same path as any unroutable command (no panic, no state).
    let (mut engine, recorder) = build();

    engine.inject(Event::RequestedChange {
        device: UNBOUND,
        desired: Desired::Set(CapabilityState::Switch(true)),
    });

    // Nothing reached the bound recorder, and the engine survived.
    assert!(recorder.commands().is_empty());
    assert_eq!(engine.switch_state(UNBOUND), None);
}

// --- relative intents --------------------------------------------------------
//
// `Toggle` and `AdjustBrightness` are the reason the payload is a `Desired` and
// not a `CapabilityState`: they name an intent the network could not otherwise
// express, and the engine — not the adapter — resolves them against the store.

#[test]
fn toggle_with_known_state_resolves_against_the_store() {
    let (mut engine, recorder) = build();

    // Establish a known value the way a real device would: an echoed report.
    engine.inject(Event::StateReported {
        device: LIGHT,
        state: CapabilityState::Switch(true),
    });
    engine.inject(Event::RequestedChange {
        device: LIGHT,
        desired: Desired::Toggle,
    });

    // The adapter sees a concrete SetSwitch, not an ambiguous toggle — identical
    // to what `toggle:` in a rule produces (see tests/toggle.rs).
    assert_eq!(
        recorder.commands(),
        vec![Command::SetSwitch {
            device: LIGHT,
            on: false
        }]
    );
    assert_eq!(engine.switch_state(LIGHT), Some(false));
}

#[test]
fn toggle_with_unknown_state_reaches_the_adapter_raw() {
    // Nothing has ever been reported for LIGHT, so the engine cannot pick a
    // direction and defers to the device — matching existing toggle semantics.
    let (mut engine, recorder) = build();

    engine.inject(Event::RequestedChange {
        device: LIGHT,
        desired: Desired::Toggle,
    });

    assert_eq!(
        recorder.commands(),
        vec![Command::ToggleSwitch { device: LIGHT }]
    );
}

#[test]
fn adjust_brightness_clamps_at_both_ends_and_ignores_zero() {
    // Upward, clamped to 100.
    let (mut engine, recorder) = build();
    engine.inject(Event::StateReported {
        device: LIGHT,
        state: CapabilityState::Brightness(95),
    });
    engine.inject(Event::RequestedChange {
        device: LIGHT,
        desired: Desired::AdjustBrightness(10),
    });
    assert_eq!(
        recorder.commands(),
        vec![Command::SetBrightness {
            device: LIGHT,
            value: 100,
            transition: None,
        }]
    );

    // Downward, clamped to 0.
    let (mut engine, recorder) = build();
    engine.inject(Event::StateReported {
        device: LIGHT,
        state: CapabilityState::Brightness(5),
    });
    engine.inject(Event::RequestedChange {
        device: LIGHT,
        desired: Desired::AdjustBrightness(-10),
    });
    assert_eq!(
        recorder.commands(),
        vec![Command::SetBrightness {
            device: LIGHT,
            value: 0,
            transition: None,
        }]
    );

    // A zero delta asks for nothing, so no command is dispatched at all.
    let (mut engine, recorder) = build();
    engine.inject(Event::StateReported {
        device: LIGHT,
        state: CapabilityState::Brightness(50),
    });
    engine.inject(Event::RequestedChange {
        device: LIGHT,
        desired: Desired::AdjustBrightness(0),
    });
    assert!(recorder.commands().is_empty());
}

#[test]
fn send_ir_becomes_a_send_ir_code_command() {
    // `ir_transmitter` is write-only and has no CapabilityState, so this intent
    // is the network's only route to `Command::SendIrCode` — config reaches it by
    // compile-time lowering, a path an adapter's `tick` cannot take.
    let (mut engine, recorder) = build();

    engine.inject(Event::RequestedChange {
        device: LIGHT,
        desired: Desired::SendIr("aGVsbG8=".to_string()),
    });

    assert_eq!(
        recorder.commands(),
        vec![Command::SendIrCode {
            device: LIGHT,
            code: "aGVsbG8=".to_string(),
        }]
    );
}

#[test]
fn a_relative_intent_still_refans_the_settled_value_to_northbound_mirrors() {
    // Regression guard: the `RequestedChange` arm ends by re-fanning the store's
    // current value for the requested capability, so an optimistic northbound
    // cell (a Matter attribute) snaps back to truth after a rejected or
    // unconfirmed write. That re-fan keys off `Desired::kind()`; a relative
    // intent must resolve to the right capability just as `Set` does.
    //
    // Use an adapter that ACCEPTS but never echoes, so the only thing that could
    // reach the mirror is the re-fan itself.
    #[derive(Clone, Default)]
    struct Silent;
    impl Adapter for Silent {
        fn dispatch(&mut self, _cmd: &Command, _now: Millis) -> DispatchOutcome {
            DispatchOutcome::ok()
        }
    }

    let bridge = MockNorthbound::new();
    let mut engine = Engine::new();
    let idx = engine.add_adapter(Box::new(Silent));
    engine.bind_device(LIGHT, idx);
    engine.add_northbound(Box::new(bridge.clone()));

    // Settle a known switch value. That fold is the mirror's first entry.
    engine.inject(Event::StateReported {
        device: LIGHT,
        state: CapabilityState::Switch(false),
    });
    let before = bridge.mirrored();
    assert_eq!(before, vec![(LIGHT, CapabilityState::Switch(false))]);

    // A toggle the device silently drops. The store still says `false`, and that
    // truth must be re-fanned to the mirror rather than leaving it asserting the
    // optimistic `true` a real bridge would have flipped to.
    engine.inject(Event::RequestedChange {
        device: LIGHT,
        desired: Desired::Toggle,
    });

    // Assert on the *additional* fan, not just the final value: the store never
    // changed, so a missing re-fan would leave `latest()` looking correct while
    // a real bridge's optimistic cell stayed wrong. The new entry is the signal.
    let after = bridge.mirrored();
    assert_eq!(
        after.len(),
        before.len() + 1,
        "the request must produce exactly one re-fan of the settled value; \
         got {after:?}"
    );
    assert_eq!(after.last(), Some(&(LIGHT, CapabilityState::Switch(false))));
}

// --- scene activation --------------------------------------------------------

#[test]
fn requested_scene_expands_without_folding_or_matching_rules() {
    let recorder = Recorder::default();
    let mut engine = Engine::new();
    let idx = engine.add_adapter(Box::new(recorder.clone()));
    engine.bind_device(LIGHT, idx);
    engine.add_scene(
        EVENING,
        vec![
            Command::SetSwitch {
                device: LIGHT,
                on: true,
            },
            Command::SetBrightness {
                device: LIGHT,
                value: 30,
                transition: None,
            },
        ],
    );

    // A rule that would fire if the request were run through rule matching. It
    // triggers on the switch report, which only the device's *echo* produces —
    // so it may fire from the echo, but never from the request itself.
    let spy = Recorder::default();
    let spy_idx = engine.add_adapter(Box::new(spy.clone()));
    engine.bind_device(UNBOUND, spy_idx);
    engine.add_rule(Rule::new(
        RuleId(0),
        Trigger::Changed {
            device: LIGHT,
            kind: CapabilityKind::Occupancy,
            to: true,
        },
        Condition::Always,
        vec![Command::SetSwitch {
            device: UNBOUND,
            on: true,
        }],
    ));

    engine.inject(Event::RequestedScene { scene: EVENING });

    // The scene's member commands reached the device, in order.
    assert_eq!(
        recorder.commands(),
        vec![
            Command::SetSwitch {
                device: LIGHT,
                on: true
            },
            Command::SetBrightness {
                device: LIGHT,
                value: 30,
                transition: None,
            },
        ]
    );
    // The request folded nothing of its own and matched no rule against itself.
    assert!(spy.commands().is_empty());
}

#[test]
fn requesting_an_unknown_scene_is_a_harmless_noop() {
    // An id no scene is registered under: `dispatch_at` finds nothing to expand.
    let (mut engine, recorder) = build();

    engine.inject(Event::RequestedScene { scene: SceneId(99) });

    assert!(recorder.commands().is_empty());
}

#[test]
fn a_northbound_adapter_can_activate_a_scene_on_tick() {
    // The end-to-end outward→inward path for scenes, mirroring
    // `a_consumer_tap_drives_the_bound_device_on_tick` in tests/northbound.rs.
    let recorder = Recorder::default();
    let bridge = MockNorthbound::new();
    let mut engine = Engine::new();
    let idx = engine.add_adapter(Box::new(recorder.clone()));
    engine.bind_device(LIGHT, idx);
    engine.add_northbound(Box::new(bridge.clone()));
    engine.add_scene(
        EVENING,
        vec![Command::SetSwitch {
            device: LIGHT,
            on: true,
        }],
    );

    bridge.queue_scene(EVENING);
    engine.advance(1);

    assert_eq!(
        recorder.commands(),
        vec![Command::SetSwitch {
            device: LIGHT,
            on: true
        }]
    );
}
