//! Feature A: the general `compare` condition verb.
//!
//! `Condition::Compare` already existed in the engine (time conditions lower to
//! it), but no config verb reached it for arbitrary numeric capabilities. These
//! tests drive a compiled config through the engine: a button press fires a rule
//! guarded by `compare`, and the guard reads state injected as a `StateReported`
//! sensor report — exactly how a real adapter would fold a battery/brightness
//! level into the store.

use domiform::{build_engine, compile_str, CapabilityState, Event};

/// A lamp (switch + battery) plus a button. The rule turns the lamp on when the
/// button is pressed *and* the battery `compare`s per the given op/value.
fn config(op: &str, value: i64) -> String {
    format!(
        r#"
adapters:
  z: {{ type: mock }}
devices:
  lamp:
    adapter: z
    capabilities: [switch, battery]
  btn:
    adapter: z
    events: {{ press: p }}
rules:
  low_batt:
    when: {{ event: btn.press }}
    if: {{ compare: {{ device: lamp, capability: battery, op: "{op}", value: {value} }} }}
    then:
      - turn_on: lamp
"#
    )
}

/// Find the interned `DeviceId` / `ActionId` and drive the engine: report a
/// battery level, press the button, and read back whether the lamp turned on.
fn fires_with_battery(cfg_src: &str, battery: u8) -> bool {
    let cfg = compile_str(cfg_src).expect("should compile");
    let lamp = cfg.device_id("lamp").unwrap();
    let btn = cfg.device_id("btn").unwrap();
    // The button declares exactly one event, `press`; grab its ActionId.
    let press = cfg.device(btn).unwrap().events[0].id;

    let mut engine = build_engine(&cfg);
    engine.start();
    // Report the battery level (folds into the store like a real sensor report).
    engine.inject(Event::StateReported {
        device: lamp,
        state: CapabilityState::Battery(battery),
    });
    engine.inject(Event::Action {
        device: btn,
        action: press,
    });
    engine.switch_state(lamp) == Some(true)
}

#[test]
fn battery_below_gate_fires_only_under_threshold() {
    let cfg = config("<", 15);
    assert!(fires_with_battery(&cfg, 10), "10 < 15 → fires");
    assert!(!fires_with_battery(&cfg, 50), "50 < 15 is false → no fire");
}

#[test]
fn never_reported_is_unknown_and_does_not_fire() {
    // The critical invariant: a capability that was never reported reads Unknown,
    // so the rule declines rather than acting on absent data.
    let cfg = compile_str(&config("<", 15)).expect("should compile");
    let lamp = cfg.device_id("lamp").unwrap();
    let btn = cfg.device_id("btn").unwrap();
    let press = cfg.device(btn).unwrap().events[0].id;

    let mut engine = build_engine(&cfg);
    engine.start();
    // No battery report at all — press the button straight away.
    engine.inject(Event::Action {
        device: btn,
        action: press,
    });
    assert_ne!(
        engine.switch_state(lamp),
        Some(true),
        "battery never reported → Unknown → rule must not fire"
    );
}

#[test]
fn every_operator_lowers_and_evaluates() {
    // (op, value, battery, expected-fire)
    let cases = [
        ("<", 50, 40, true),
        ("<", 50, 60, false),
        ("<=", 50, 50, true),
        ("<=", 50, 51, false),
        ("==", 42, 42, true),
        ("==", 42, 43, false),
        ("!=", 42, 43, true),
        ("!=", 42, 42, false),
        (">=", 50, 50, true),
        (">=", 50, 49, false),
        (">", 50, 51, true),
        (">", 50, 50, false),
    ];
    for (op, value, battery, expected) in cases {
        let cfg = config(op, value);
        assert_eq!(
            fires_with_battery(&cfg, battery),
            expected,
            "op {op} value {value} battery {battery} should fire={expected}"
        );
    }
}

#[test]
fn compare_on_non_numeric_capability_is_an_error() {
    let src = r#"
adapters:
  z: { type: mock }
devices:
  lamp: { adapter: z, capabilities: [switch] }
  btn: { adapter: z, events: { press: p } }
rules:
  r:
    when: { event: btn.press }
    if: { compare: { device: lamp, capability: switch, op: "==", value: 1 } }
    then: [ { turn_on: lamp } ]
"#;
    let errs = compile_str(src).expect_err("compare on switch should fail");
    assert!(errs.errors().any(|d| d.code == "E_NON_NUMERIC_CAPABILITY"));
}

#[test]
fn compare_on_undeclared_capability_is_an_error() {
    // The device is numeric-shaped-capable in the language but doesn't declare
    // `battery`, so require_cap rejects it.
    let src = r#"
adapters:
  z: { type: mock }
devices:
  lamp: { adapter: z, capabilities: [switch] }
  btn: { adapter: z, events: { press: p } }
rules:
  r:
    when: { event: btn.press }
    if: { compare: { device: lamp, capability: battery, op: "<", value: 15 } }
    then: [ { turn_on: lamp } ]
"#;
    let errs = compile_str(src).expect_err("compare on undeclared cap should fail");
    assert!(errs.errors().any(|d| d.code == "E_MISSING_CAPABILITY"));
}

#[test]
fn bad_operator_is_an_error() {
    let src = r#"
adapters:
  z: { type: mock }
devices:
  lamp: { adapter: z, capabilities: [switch, battery] }
  btn: { adapter: z, events: { press: p } }
rules:
  r:
    when: { event: btn.press }
    if: { compare: { device: lamp, capability: battery, op: "=<", value: 15 } }
    then: [ { turn_on: lamp } ]
"#;
    let errs = compile_str(src).expect_err("bad op should fail");
    assert!(errs.errors().any(|d| d.code == "E_BAD_OP"));
}
