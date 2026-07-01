//! Engine-resolved toggle: `Command::ToggleSwitch` is turned into an explicit
//! `SetSwitch { on: !current }` using the state store, so adapters send the
//! well-supported On/Off commands instead of the protocol `Toggle` primitive
//! (which cheap device firmware often botches). When the switch state is
//! `Unknown`, the raw toggle passes through for the device to resolve itself.

use std::cell::RefCell;
use std::rc::Rc;

use domiform::ids::DeviceId;
use domiform::model::{CapabilityState, Millis};
use domiform::{
    Adapter, Command, Condition, DispatchOutcome, Engine, Event, Rule, RuleId, Trigger,
};

const MOTION: DeviceId = DeviceId(0);
const LIGHT: DeviceId = DeviceId(1);

/// An adapter that records every command it's handed, and echoes `SetSwitch`
/// back as a state report (like `MockDeviceAdapter`) so the store stays live.
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
            _ => DispatchOutcome::ok(),
        }
    }
}

/// An engine with one rule: `Occupancy(MOTION, true)` toggles `LIGHT`.
fn build() -> (Engine, Recorder) {
    let recorder = Recorder::default();
    let mut engine = Engine::new();
    let idx = engine.add_adapter(Box::new(recorder.clone()));
    engine.bind_device(LIGHT, idx);
    engine.add_rule(Rule::new(
        RuleId(0),
        Trigger::Occupancy {
            device: MOTION,
            occupied: true,
        },
        Condition::Always,
        vec![Command::ToggleSwitch { device: LIGHT }],
    ));
    (engine, recorder)
}

#[test]
fn toggle_with_known_on_state_dispatches_explicit_off() {
    let (mut engine, recorder) = build();
    engine.inject(Event::StateReported {
        device: LIGHT,
        state: CapabilityState::Switch(true),
    });

    engine.inject(Event::OccupancyChanged {
        device: MOTION,
        occupied: true,
    });

    // The adapter saw an explicit SetSwitch(off), never a ToggleSwitch.
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
fn toggle_with_known_off_state_dispatches_explicit_on() {
    let (mut engine, recorder) = build();
    engine.inject(Event::StateReported {
        device: LIGHT,
        state: CapabilityState::Switch(false),
    });

    engine.inject(Event::OccupancyChanged {
        device: MOTION,
        occupied: true,
    });

    assert_eq!(
        recorder.commands(),
        vec![Command::SetSwitch {
            device: LIGHT,
            on: true
        }]
    );
    assert_eq!(engine.switch_state(LIGHT), Some(true));
}

#[test]
fn repeated_toggles_alternate() {
    let (mut engine, recorder) = build();
    engine.inject(Event::StateReported {
        device: LIGHT,
        state: CapabilityState::Switch(false),
    });

    engine.inject(Event::OccupancyChanged {
        device: MOTION,
        occupied: true,
    }); // -> on
    engine.inject(Event::OccupancyChanged {
        device: MOTION,
        occupied: true,
    }); // -> off
    engine.inject(Event::OccupancyChanged {
        device: MOTION,
        occupied: true,
    }); // -> on

    assert_eq!(
        recorder.commands(),
        vec![
            Command::SetSwitch {
                device: LIGHT,
                on: true
            },
            Command::SetSwitch {
                device: LIGHT,
                on: false
            },
            Command::SetSwitch {
                device: LIGHT,
                on: true
            },
        ]
    );
    assert_eq!(engine.switch_state(LIGHT), Some(true));
}

#[test]
fn toggle_with_unknown_state_passes_raw_toggle_through() {
    // No prior state report for LIGHT: the store is Unknown, so the engine can't
    // resolve a direction and hands the device the raw toggle to decide itself.
    let (mut engine, recorder) = build();

    engine.inject(Event::OccupancyChanged {
        device: MOTION,
        occupied: true,
    });

    assert_eq!(
        recorder.commands(),
        vec![Command::ToggleSwitch { device: LIGHT }]
    );
}
