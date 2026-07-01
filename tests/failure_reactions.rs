//! Closing two runtime seams:
//!  1. `Trigger::CommandFailed` — rules can react to a failed command (the basis
//!     for "device offline → fall back / notify" once real adapters exist).
//!  2. The cascade depth-guard — a rule that reacts to a failure by re-issuing a
//!     command that fails again is bounded instead of hanging the loop.

use std::cell::RefCell;
use std::rc::Rc;

use domiform::ids::{ActionId, DeviceId};
use domiform::{
    Adapter, Command, Condition, DispatchOutcome, Engine, Event, Millis, MockDeviceAdapter,
    Observer, Rule, RuleId, Trigger,
};

const SWITCH: DeviceId = DeviceId(10);
const LIGHT: DeviceId = DeviceId(11); // bound to a dead adapter
const BACKUP: DeviceId = DeviceId(12); // bound to a working adapter
const PRESS: ActionId = ActionId(0); // the switch's one declared event

/// Always fails permanently — stands in for an offline/unreachable device.
struct DeadAdapter;
impl Adapter for DeadAdapter {
    fn dispatch(&mut self, _cmd: &Command, _now: Millis) -> DispatchOutcome {
        DispatchOutcome::Permanent("device unreachable".into())
    }
}

fn press() -> Event {
    Event::Action {
        device: SWITCH,
        action: PRESS,
    }
}

#[test]
fn rule_falls_back_when_a_command_fails() {
    let mut engine = Engine::new();
    let dead = engine.add_adapter(Box::new(DeadAdapter));
    let working = engine.add_adapter(Box::new(MockDeviceAdapter));
    engine.bind_device(LIGHT, dead);
    engine.bind_device(BACKUP, working);

    // Primary: button turns on the (dead) main light.
    engine.add_rule(Rule::new(
        RuleId(1),
        Trigger::Action {
            device: SWITCH,
            action: PRESS,
        },
        Condition::Always,
        vec![Command::SetSwitch {
            device: LIGHT,
            on: true,
        }],
    ));
    // Fallback: if a command to the main light fails, light the backup instead.
    engine.add_rule(Rule::new(
        RuleId(2),
        Trigger::CommandFailed {
            device: Some(LIGHT),
        },
        Condition::Always,
        vec![Command::SetSwitch {
            device: BACKUP,
            on: true,
        }],
    ));

    engine.inject(press());

    assert_eq!(engine.switch_state(LIGHT), None, "main light never applied");
    assert_eq!(
        engine.switch_state(BACKUP),
        Some(true),
        "fallback fired off the CommandFailed event"
    );
}

// --- cascade guard -----------------------------------------------------------

#[derive(Default)]
struct Recorder {
    command_failed: u32,
    cascade_dropped: u32,
}

#[derive(Clone, Default)]
struct SharedRecorder(Rc<RefCell<Recorder>>);

impl Observer for SharedRecorder {
    fn command_failed(&mut self, _c: &Command, _r: &str, _a: u32) {
        self.0.borrow_mut().command_failed += 1;
    }
    fn cascade_dropped(&mut self, _e: &Event, _d: u32) {
        self.0.borrow_mut().cascade_dropped += 1;
    }
}

#[test]
fn cascade_guard_terminates_a_failure_storm() {
    let rec = SharedRecorder::default();

    let mut engine = Engine::new();
    let dead = engine.add_adapter(Box::new(DeadAdapter));
    engine.bind_device(LIGHT, dead);
    engine.set_observer(Box::new(rec.clone()));
    engine.set_max_cascade_depth(4);

    // Kick it off.
    engine.add_rule(Rule::new(
        RuleId(1),
        Trigger::Action {
            device: SWITCH,
            action: PRESS,
        },
        Condition::Always,
        vec![Command::SetSwitch {
            device: LIGHT,
            on: true,
        }],
    ));
    // The pathological rule: react to the failure by re-issuing the same doomed
    // command, which fails again — an unbounded loop without the depth-guard.
    engine.add_rule(Rule::new(
        RuleId(2),
        Trigger::CommandFailed {
            device: Some(LIGHT),
        },
        Condition::Always,
        vec![Command::SetSwitch {
            device: LIGHT,
            on: true,
        }],
    ));

    // If the guard works, this returns; if it doesn't, the test hangs.
    engine.inject(press());

    let r = rec.0.borrow();
    assert_eq!(
        r.cascade_dropped, 1,
        "exactly one event past the depth limit"
    );
    // depth 0 (button) + depths 1..=4 reacted to = 5 failures, then depth 5 dropped.
    assert_eq!(
        r.command_failed, 5,
        "the storm is bounded by the depth limit"
    );
}
