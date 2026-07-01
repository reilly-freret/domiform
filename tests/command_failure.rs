//! Graceful degradation on command failure: transient failures retry with
//! backoff (expressed as scheduled timers, so they advance with virtual time),
//! permanent failures give up immediately, and exhausted retries surface a
//! `CommandFailed` event plus an `Observer` notification.

use std::cell::RefCell;
use std::rc::Rc;

use domiform::ids::{ActionId, DeviceId};
use domiform::{
    Adapter, CapabilityState, Command, Condition, DispatchOutcome, Engine, Event, Millis, Observer,
    RetryPolicy, Rule, RuleId, Trigger,
};

const SWITCH: DeviceId = DeviceId(10); // pure event source
const LIGHT: DeviceId = DeviceId(11);
const PRESS: ActionId = ActionId(0);

// --- test adapters -----------------------------------------------------------

/// Fails transiently `fail_remaining` times, then succeeds (echoing state).
struct FlakyAdapter {
    fail_remaining: u32,
}

impl Adapter for FlakyAdapter {
    fn dispatch(&mut self, cmd: &Command, _now: Millis) -> DispatchOutcome {
        if self.fail_remaining > 0 {
            self.fail_remaining -= 1;
            return DispatchOutcome::Transient("network blip".into());
        }
        match cmd {
            Command::SetSwitch { device, on } => DispatchOutcome::Ok(vec![Event::StateReported {
                device: *device,
                state: CapabilityState::Switch(*on),
            }]),
            _ => DispatchOutcome::ok(),
        }
    }
}

/// Always fails permanently.
struct DeadAdapter;

impl Adapter for DeadAdapter {
    fn dispatch(&mut self, _cmd: &Command, _now: Millis) -> DispatchOutcome {
        DispatchOutcome::Permanent("device unreachable".into())
    }
}

// --- recording observer (shared via Rc<RefCell>) -----------------------------

#[derive(Default)]
struct Recorder {
    dispatch_failed: u32,
    retry_scheduled: u32,
    command_failed: u32,
}

/// Local newtype so we can implement the foreign `Observer` trait (orphan rule);
/// cloning shares the same inner counters with the engine.
#[derive(Clone, Default)]
struct SharedRecorder(Rc<RefCell<Recorder>>);

impl Observer for SharedRecorder {
    fn dispatch_failed(&mut self, _c: &Command, _r: &str, _t: bool, _a: u32) {
        self.0.borrow_mut().dispatch_failed += 1;
    }
    fn retry_scheduled(&mut self, _c: &Command, _next: u32, _delay: Millis) {
        self.0.borrow_mut().retry_scheduled += 1;
    }
    fn command_failed(&mut self, _c: &Command, _r: &str, _a: u32) {
        self.0.borrow_mut().command_failed += 1;
    }
}

// --- harness -----------------------------------------------------------------

fn press() -> Event {
    Event::Action {
        device: SWITCH,
        action: PRESS,
    }
}

fn turn_on_rule() -> Rule {
    Rule::new(
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
    )
}

fn engine_with(adapter: Box<dyn Adapter>, rec: &SharedRecorder) -> Engine {
    let mut engine = Engine::new();
    let idx = engine.add_adapter(adapter);
    engine.bind_device(LIGHT, idx);
    engine.set_retry_policy(RetryPolicy {
        max_attempts: 3,
        base_backoff: 1000,
    });
    engine.set_observer(Box::new(rec.clone()));
    engine.add_rule(turn_on_rule());
    engine
}

// --- tests -------------------------------------------------------------------

#[test]
fn transient_failure_retries_then_succeeds() {
    let rec = SharedRecorder::default();
    let mut engine = engine_with(Box::new(FlakyAdapter { fail_remaining: 2 }), &rec);

    // Attempt 1 fails transiently; a retry is scheduled 1000ms out.
    engine.inject(press());
    assert_eq!(engine.switch_state(LIGHT), None, "not applied yet");

    // Attempt 2 fires at +1000ms, fails, schedules attempt 3 at +2000ms.
    engine.advance(1000);
    assert_eq!(engine.switch_state(LIGHT), None, "still retrying");

    // Attempt 3 fires and succeeds.
    engine.advance(2000);
    assert_eq!(engine.switch_state(LIGHT), Some(true), "eventually applied");

    let r = rec.0.borrow();
    assert_eq!(r.dispatch_failed, 2);
    assert_eq!(r.retry_scheduled, 2);
    assert_eq!(r.command_failed, 0);
}

#[test]
fn permanent_failure_does_not_retry() {
    let rec = SharedRecorder::default();
    let mut engine = engine_with(Box::new(DeadAdapter), &rec);

    engine.inject(press());
    assert_eq!(engine.switch_state(LIGHT), None);

    let r = rec.0.borrow();
    assert_eq!(r.retry_scheduled, 0, "permanent failures are not retried");
    assert_eq!(r.command_failed, 1, "given up on immediately");
}

#[test]
fn exhausted_retries_emit_command_failed() {
    let rec = SharedRecorder::default();
    let mut engine = engine_with(Box::new(FlakyAdapter { fail_remaining: 99 }), &rec);

    engine.inject(press()); // attempt 1 fails -> retry
    engine.advance(1000); // attempt 2 fails -> retry
    engine.advance(2000); // attempt 3 fails -> give up

    assert_eq!(engine.switch_state(LIGHT), None);
    let r = rec.0.borrow();
    assert_eq!(r.dispatch_failed, 3, "every attempt logged");
    assert_eq!(r.retry_scheduled, 2, "retried after attempts 1 and 2");
    assert_eq!(r.command_failed, 1, "gave up after attempt 3");
}
