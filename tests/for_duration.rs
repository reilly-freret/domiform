//! Feature E: `for:`-qualified (sustained-edge) triggers.
//!
//! A rule with `for: <duration>` fires only if its edge trigger's predicate has
//! held *continuously* for that long: on the edge the engine schedules a timer
//! through the scheduler adapter (virtual time); if the state reverts before it
//! elapses the timer is auto-cancelled; on elapse the predicate is re-verified
//! before the commands fire. All timing is driven by `advance` — no wall clock,
//! fully replayable.

use std::cell::RefCell;
use std::rc::Rc;

use domiform::ids::DeviceId;
use domiform::{
    build_engine, compile_str, Adapter, CapabilityState, Command, CompiledConfig, DispatchOutcome,
    Engine, Event, Millis,
};

const FIVE_MIN: Millis = 5 * 60 * 1000;

#[derive(Clone, Default)]
struct Counter(Rc<RefCell<u32>>);
impl Counter {
    fn count(&self) -> u32 {
        *self.0.borrow()
    }
}
impl Adapter for Counter {
    fn dispatch(&mut self, cmd: &Command, _now: Millis) -> DispatchOutcome {
        if let Command::SetSwitch { device, on } = cmd {
            *self.0.borrow_mut() += 1;
            return DispatchOutcome::Ok(vec![Event::StateReported {
                device: *device,
                state: CapabilityState::Switch(*on),
            }]);
        }
        DispatchOutcome::ok()
    }
}

/// Wire a `Counter` to the `light` device so we can count command dispatches.
fn build(cfg_src: &str) -> (Engine, Counter, CompiledConfig) {
    let cfg = compile_str(cfg_src).expect("should compile");
    let mut engine = build_engine(&cfg);
    let counter = Counter::default();
    let idx = engine.add_adapter(Box::new(counter.clone()));
    let light = cfg.device_id("light").unwrap();
    engine.bind_device(light, idx);
    engine.start();
    (engine, counter, cfg)
}

fn report(engine: &mut Engine, device: DeviceId, state: CapabilityState) {
    engine.inject(Event::StateReported { device, state });
}

/// "Motion clear *for* 5 minutes → turn the light off." Occupancy going false is
/// the edge; the rule fires only if it stays false for 5m.
const MOTION_CLEAR: &str = r#"
adapters:
  z: { type: mock }
devices:
  motion: { adapter: z, capabilities: [occupancy] }
  light: { adapter: z, capabilities: [switch] }
rules:
  auto_off:
    when: { changed: { device: motion, capability: occupancy, to: false } }
    for: 5m
    then: [ { turn_off: light } ]
"#;

#[test]
fn fires_after_sustained_duration() {
    let (mut engine, counter, cfg) = build(MOTION_CLEAR);
    let m = cfg.device_id("motion").unwrap();
    report(&mut engine, m, CapabilityState::Occupancy(true)); // occupied
    report(&mut engine, m, CapabilityState::Occupancy(false)); // clears → arm 5m timer
    assert_eq!(counter.count(), 0, "does not fire immediately");
    engine.advance(FIVE_MIN);
    assert_eq!(
        counter.count(),
        1,
        "fires once the 5m elapses with no motion"
    );
}

#[test]
fn new_motion_auto_cancels_the_pending_off() {
    let (mut engine, counter, cfg) = build(MOTION_CLEAR);
    let m = cfg.device_id("motion").unwrap();
    report(&mut engine, m, CapabilityState::Occupancy(true));
    report(&mut engine, m, CapabilityState::Occupancy(false)); // arm timer
    engine.advance(3 * 60 * 1000); // 3m in
    report(&mut engine, m, CapabilityState::Occupancy(true)); // motion resumes → cancel
    engine.advance(FIVE_MIN); // well past the original deadline
    assert_eq!(
        counter.count(),
        0,
        "auto-cancel: the light must not turn off because motion resumed"
    );
}

#[test]
fn re_arm_on_second_clear_uses_a_fresh_duration() {
    let (mut engine, counter, cfg) = build(MOTION_CLEAR);
    let m = cfg.device_id("motion").unwrap();
    // Clear, resume before deadline, clear again: the second clear must start a
    // fresh 5m from *that* moment, not fire on the first clear's schedule.
    report(&mut engine, m, CapabilityState::Occupancy(false)); // arm #1
    engine.advance(3 * 60 * 1000);
    report(&mut engine, m, CapabilityState::Occupancy(true)); // cancel #1
    report(&mut engine, m, CapabilityState::Occupancy(false)); // arm #2
    engine.advance(3 * 60 * 1000); // only 3m into arm #2
    assert_eq!(counter.count(), 0, "not yet — arm #2 needs a full 5m");
    engine.advance(2 * 60 * 1000); // now 5m into arm #2
    assert_eq!(counter.count(), 1);
}

/// Numeric sustained case: "temperature above 25°C (2500cc) for 5m → fan on."
const HOT_FOR: &str = r#"
adapters:
  z: { type: mock }
devices:
  thermostat: { adapter: z, capabilities: [temperature] }
  light: { adapter: z, capabilities: [switch] }
rules:
  hot:
    when: { crosses: { device: thermostat, capability: temperature, above: 2500 } }
    for: 5m
    then: [ { turn_on: light } ]
"#;

#[test]
fn numeric_crossing_reverting_before_elapse_does_not_fire() {
    // Re-verification: the value crosses up (arming the timer), then drops back
    // below before the timer elapses — the auto-cancel drops the pending fire.
    let (mut engine, counter, cfg) = build(HOT_FOR);
    let t = cfg.device_id("thermostat").unwrap();
    report(&mut engine, t, CapabilityState::Temperature(2600)); // crosses up → arm
    engine.advance(2 * 60 * 1000);
    report(&mut engine, t, CapabilityState::Temperature(2000)); // drops below → cancel
    engine.advance(FIVE_MIN);
    assert_eq!(counter.count(), 0, "reverted before elapse → no fire");
}

#[test]
fn numeric_crossing_sustained_fires() {
    let (mut engine, counter, cfg) = build(HOT_FOR);
    let t = cfg.device_id("thermostat").unwrap();
    report(&mut engine, t, CapabilityState::Temperature(2600)); // arm
    report(&mut engine, t, CapabilityState::Temperature(2700)); // still above (no re-arm needed)
    engine.advance(FIVE_MIN);
    assert_eq!(counter.count(), 1);
}

#[test]
fn is_deterministic_under_identical_advances() {
    // Same report + advance sequence → same firing count, twice.
    let run = || {
        let (mut engine, counter, cfg) = build(MOTION_CLEAR);
        let m = cfg.device_id("motion").unwrap();
        report(&mut engine, m, CapabilityState::Occupancy(false));
        engine.advance(2 * 60 * 1000);
        report(&mut engine, m, CapabilityState::Occupancy(true));
        report(&mut engine, m, CapabilityState::Occupancy(false));
        engine.advance(FIVE_MIN);
        counter.count()
    };
    assert_eq!(run(), run());
    assert_eq!(run(), 1);
}

#[test]
fn for_on_a_non_edge_trigger_is_a_compile_error() {
    let src = r#"
adapters:
  z: { type: mock }
devices:
  meter: { adapter: z, capabilities: [power] }
  light: { adapter: z, capabilities: [switch] }
rules:
  r:
    when: { reports: { device: meter, capability: power } }
    for: 5m
    then: [ { turn_on: light } ]
"#;
    let errs = compile_str(src).expect_err("for on reports should fail");
    assert!(errs.errors().any(|d| d.code == "E_FOR_UNSUPPORTED"));
}
