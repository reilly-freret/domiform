//! Feature D: on-change / state-report triggers.
//!
//! The new trigger family (`changed`, `crosses`, `reports`) all ride
//! `Event::StateReported` — no per-capability event type. Edge triggers
//! (`changed`, `crosses`) fire only on the *transition* into the predicate; the
//! level trigger (`reports`) fires on every report. These tests drive compiled
//! configs and count firings by how many commands reach the mock device.

use std::cell::RefCell;
use std::rc::Rc;

use domiform::ids::DeviceId;
use domiform::{
    build_engine, compile_str, Adapter, CapabilityState, Command, CompiledConfig, DispatchOutcome,
    Engine, Event, Millis,
};

/// A device adapter that counts every SetSwitch(on:true) it's told to dispatch,
/// so a test can assert exactly how many times a rule fired.
#[derive(Clone, Default)]
struct Counter(Rc<RefCell<u32>>);

impl Counter {
    fn count(&self) -> u32 {
        *self.0.borrow()
    }
}

impl Adapter for Counter {
    fn dispatch(&mut self, cmd: &Command, _now: Millis) -> DispatchOutcome {
        if let Command::SetSwitch { device, on: true } = cmd {
            *self.0.borrow_mut() += 1;
            return DispatchOutcome::Ok(vec![Event::StateReported {
                device: *device,
                state: CapabilityState::Switch(true),
            }]);
        }
        DispatchOutcome::ok()
    }
}

/// Build an engine from a config, binding a fresh `Counter` to the `fan` device
/// (the one every rule here commands). Returns the engine, the counter, and the
/// compiled config (for device-id lookup).
fn build(cfg_src: &str) -> (Engine, Counter, CompiledConfig) {
    let cfg = compile_str(cfg_src).expect("should compile");
    // We don't use the plugin-built adapter for `fan`; instead wire our Counter
    // directly so we can observe firings. Build the engine, then rebind.
    let mut engine = build_engine(&cfg);
    let counter = Counter::default();
    let idx = engine.add_adapter(Box::new(counter.clone()));
    let fan = cfg.device_id("fan").unwrap();
    engine.bind_device(fan, idx);
    engine.start();
    (engine, counter, cfg)
}

fn report(engine: &mut Engine, device: DeviceId, state: CapabilityState) {
    engine.inject(Event::StateReported { device, state });
}

const CROSS_ABOVE: &str = r#"
adapters:
  z: { type: mock }
devices:
  thermostat: { adapter: z, capabilities: [temperature] }
  fan: { adapter: z, capabilities: [switch] }
rules:
  hot:
    when: { crosses: { device: thermostat, capability: temperature, above: 2500 } }
    then: [ { turn_on: fan } ]
"#;

#[test]
fn crosses_above_fires_once_on_the_upward_edge() {
    let (mut engine, counter, cfg) = build(CROSS_ABOVE);
    let t = cfg.device_id("thermostat").unwrap();

    report(&mut engine, t, CapabilityState::Temperature(2000)); // 20°C: below
    assert_eq!(counter.count(), 0);
    report(&mut engine, t, CapabilityState::Temperature(2400)); // 24°C: still below
    assert_eq!(counter.count(), 0);
    report(&mut engine, t, CapabilityState::Temperature(2600)); // 26°C: crosses up
    assert_eq!(counter.count(), 1, "fires on the crossing");
    report(&mut engine, t, CapabilityState::Temperature(2700)); // stays above
    report(&mut engine, t, CapabilityState::Temperature(2800)); // stays above
    assert_eq!(counter.count(), 1, "does not re-fire while it stays above");
}

#[test]
fn crosses_above_does_not_fire_on_a_downward_move() {
    let (mut engine, counter, cfg) = build(CROSS_ABOVE);
    let t = cfg.device_id("thermostat").unwrap();
    // Start above (first-ever report already satisfies → counts as an edge).
    report(&mut engine, t, CapabilityState::Temperature(2600));
    assert_eq!(
        counter.count(),
        1,
        "first report already-hot counts as edge"
    );
    // Drop below, then a smaller drop — no upward crossing.
    report(&mut engine, t, CapabilityState::Temperature(2000));
    report(&mut engine, t, CapabilityState::Temperature(1000));
    assert_eq!(counter.count(), 1);
}

const CROSS_BELOW: &str = r#"
adapters:
  z: { type: mock }
devices:
  thermostat: { adapter: z, capabilities: [temperature] }
  fan: { adapter: z, capabilities: [switch] }
rules:
  cold:
    when: { crosses: { device: thermostat, capability: temperature, below: 1800 } }
    then: [ { turn_on: fan } ]
"#;

#[test]
fn crosses_below_is_directional() {
    let (mut engine, counter, cfg) = build(CROSS_BELOW);
    let t = cfg.device_id("thermostat").unwrap();
    report(&mut engine, t, CapabilityState::Temperature(2000)); // above bound, no fire
    assert_eq!(counter.count(), 0);
    report(&mut engine, t, CapabilityState::Temperature(1700)); // crosses down
    assert_eq!(counter.count(), 1);
    report(&mut engine, t, CapabilityState::Temperature(1600)); // stays below
    assert_eq!(counter.count(), 1, "no re-fire while below");
    report(&mut engine, t, CapabilityState::Temperature(2000)); // back up — not a downward crossing
    assert_eq!(counter.count(), 1);
}

const REPORTS: &str = r#"
adapters:
  z: { type: mock }
devices:
  meter: { adapter: z, capabilities: [power] }
  fan: { adapter: z, capabilities: [switch] }
rules:
  log:
    when: { reports: { device: meter, capability: power } }
    then: [ { turn_on: fan } ]
"#;

#[test]
fn reports_fires_on_every_report() {
    let (mut engine, counter, cfg) = build(REPORTS);
    let m = cfg.device_id("meter").unwrap();
    report(&mut engine, m, CapabilityState::Power(100));
    report(&mut engine, m, CapabilityState::Power(100)); // same value: still fires
    report(&mut engine, m, CapabilityState::Power(120));
    assert_eq!(counter.count(), 3, "level trigger fires on every report");
}

const CHANGED: &str = r#"
adapters:
  z: { type: mock }
devices:
  door: { adapter: z, capabilities: [contact] }
  fan: { adapter: z, capabilities: [switch] }
rules:
  opened:
    when: { changed: { device: door, capability: contact, to: true } }
    then: [ { turn_on: fan } ]
"#;

#[test]
fn changed_bool_fires_on_edge_only() {
    let (mut engine, counter, cfg) = build(CHANGED);
    let d = cfg.device_id("door").unwrap();
    report(&mut engine, d, CapabilityState::Contact(false)); // closed
    assert_eq!(counter.count(), 0);
    report(&mut engine, d, CapabilityState::Contact(true)); // opens
    assert_eq!(counter.count(), 1);
    report(&mut engine, d, CapabilityState::Contact(true)); // repeated open report
    assert_eq!(counter.count(), 1, "no re-fire on repeated open");
    report(&mut engine, d, CapabilityState::Contact(false)); // closes
    report(&mut engine, d, CapabilityState::Contact(true)); // opens again
    assert_eq!(counter.count(), 2);
}

#[test]
fn changed_first_report_that_satisfies_counts_as_edge() {
    // A first-ever report matching the target value is a not-known → satisfied
    // transition, so it fires ("already open at boot → act").
    let (mut engine, counter, cfg) = build(CHANGED);
    let d = cfg.device_id("door").unwrap();
    report(&mut engine, d, CapabilityState::Contact(true));
    assert_eq!(counter.count(), 1);
}

// --- occupancy migration equivalence ---------------------------------------

#[test]
fn occupancy_migrates_to_changed_with_identical_firings() {
    // The founding motion-light case, now expressed with the general `changed`
    // verb over occupancy. Same report sequence → same firings the old dedicated
    // occupancy trigger produced.
    let src = r#"
adapters:
  z: { type: mock }
devices:
  motion: { adapter: z, capabilities: [occupancy] }
  fan: { adapter: z, capabilities: [switch] }
rules:
  on_motion:
    when: { changed: { device: motion, capability: occupancy, to: true } }
    then: [ { turn_on: fan } ]
"#;
    let (mut engine, counter, cfg) = build(src);
    let m = cfg.device_id("motion").unwrap();
    report(&mut engine, m, CapabilityState::Occupancy(true)); // detect → fire
    report(&mut engine, m, CapabilityState::Occupancy(false)); // clear
    report(&mut engine, m, CapabilityState::Occupancy(true)); // detect → fire
    assert_eq!(counter.count(), 2);
}

// --- validation ------------------------------------------------------------

#[test]
fn changed_on_numeric_capability_is_an_error() {
    let src = r#"
adapters:
  z: { type: mock }
devices:
  meter: { adapter: z, capabilities: [power] }
  fan: { adapter: z, capabilities: [switch] }
rules:
  r:
    when: { changed: { device: meter, capability: power, to: true } }
    then: [ { turn_on: fan } ]
"#;
    let errs = compile_str(src).expect_err("changed on numeric should fail");
    assert!(errs.errors().any(|d| d.code == "E_NON_BOOL_CAPABILITY"));
}

#[test]
fn crosses_needs_exactly_one_bound() {
    let both = r#"
adapters:
  z: { type: mock }
devices:
  t: { adapter: z, capabilities: [temperature] }
  fan: { adapter: z, capabilities: [switch] }
rules:
  r:
    when: { crosses: { device: t, capability: temperature, above: 2500, below: 1800 } }
    then: [ { turn_on: fan } ]
"#;
    let errs = compile_str(both).expect_err("both bounds should fail");
    assert!(errs.errors().any(|d| d.code == "E_BAD_CROSS"));
}
