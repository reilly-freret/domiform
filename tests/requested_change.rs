//! `Event::RequestedChange`: the canonical inbound path for a northbound adapter
//! (HomeKit tap, REST call, web toggle). A requested *desired state* becomes the
//! same `Command` a rule would emit and is dispatched — so a Home-app tap and a
//! physical wall switch are indistinguishable to the engine. The request itself
//! is an intent, not a report: it does not fold into the store; the device's own
//! echo does. Non-writable states (battery, occupancy, time) are harmless no-ops.

use std::cell::RefCell;
use std::rc::Rc;

use domiform::ids::DeviceId;
use domiform::model::{CapabilityState, Millis};
use domiform::{
    Adapter, Command, Condition, DispatchOutcome, Engine, Event, Rule, RuleId, Trigger,
};

const LIGHT: DeviceId = DeviceId(1);
const UNBOUND: DeviceId = DeviceId(2);

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
        desired: CapabilityState::Switch(true),
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
        desired: CapabilityState::Brightness(40),
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
        Trigger::Occupancy {
            device: BUTTON,
            occupied: true,
        },
        Condition::Always,
        vec![Command::SetSwitch {
            device: LIGHT,
            on: true,
        }],
    ));
    rule_engine.inject(Event::OccupancyChanged {
        device: BUTTON,
        occupied: true,
    });

    // Path 2: a northbound request expressing the same desired state.
    let (mut req_engine, req_rec) = build();
    req_engine.inject(Event::RequestedChange {
        device: LIGHT,
        desired: CapabilityState::Switch(true),
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
        desired: CapabilityState::Battery(50),
    });
    engine.inject(Event::RequestedChange {
        device: LIGHT,
        desired: CapabilityState::Occupancy(true),
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
        desired: CapabilityState::Switch(true),
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
        desired: CapabilityState::Switch(true),
    });

    // Nothing reached the bound recorder, and the engine survived.
    assert!(recorder.commands().is_empty());
    assert_eq!(engine.switch_state(UNBOUND), None);
}
