//! The capstone: a complete automation authored in YAML, compiled to runtime
//! types, and executed — plus the rule-level static checks that make config
//! feel like a compiled program.

use chrono::{TimeZone, Utc};
use domiform::{build_engine, build_engine_at, compile_str, Event};

/// A fixed UTC-midnight boot epoch (equator/UTC config → sunrise ~06:00, sunset
/// ~18:00), so `sun_up` advances are deterministic instead of tracking the wall
/// clock the test happens to run at.
fn boot_midnight() -> i64 {
    Utc.with_ymd_and_hms(2024, 6, 1, 0, 0, 0)
        .unwrap()
        .timestamp_millis()
}

/// A dark-aware motion light with a re-triggerable off-timer — the exact
/// automation we pressure-tested by hand, now written entirely in config.
const MOTION_LIGHT: &str = r#"
system:
  timezone: UTC

adapters:
  z:
    type: mock

devices:
  hallway_motion:
    adapter: z
    capabilities: [occupancy]
  hallway_light:
    adapter: z
    capabilities: [switch]

rules:
  on_motion:
    when: { occupancy: hallway_motion }
    if: { sun_up: false }
    then:
      - turn_on: hallway_light
      - cancel_timer: hallway_off
  arm_off:
    when: { occupancy_clear: hallway_motion }
    then:
      - schedule_timer: { key: hallway_off, after: 10m }
  do_off:
    when: { timer: hallway_off }
    then:
      - turn_off: hallway_light
"#;

const TEN_MIN: u64 = 10 * 60 * 1000;

#[test]
fn full_automation_runs_from_yaml() {
    let cfg = compile_str(MOTION_LIGHT).expect("should compile clean");
    assert!(
        cfg.warnings.is_empty(),
        "no warnings expected: {:?}",
        cfg.warnings
    );

    let motion = cfg.device_id("hallway_motion").unwrap();
    let light = cfg.device_id("hallway_light").unwrap();

    let mut engine = build_engine_at(&cfg, None, boot_midnight());
    engine.start(); // boot at 00:00 → clock reports SunUp(false), i.e. dark

    // Motion while dark → light on (the `sun_up: false` guard passed).
    engine.inject(Event::OccupancyChanged {
        device: motion,
        occupied: true,
    });
    assert_eq!(engine.switch_state(light), Some(true));

    // Motion clears → off-timer armed.
    engine.inject(Event::OccupancyChanged {
        device: motion,
        occupied: false,
    });

    // Re-trigger before the deadline cancels the timer; light stays on past it.
    engine.advance(5 * 60 * 1000);
    engine.inject(Event::OccupancyChanged {
        device: motion,
        occupied: true,
    });
    engine.advance(TEN_MIN);
    assert_eq!(
        engine.switch_state(light),
        Some(true),
        "re-trigger cancelled the off-timer"
    );

    // Clear again and let the timer run out → light off.
    engine.inject(Event::OccupancyChanged {
        device: motion,
        occupied: false,
    });
    engine.advance(TEN_MIN);
    assert_eq!(engine.switch_state(light), Some(false));
}

#[test]
fn daylight_guard_suppresses_the_rule() {
    let cfg = compile_str(MOTION_LIGHT).unwrap();
    let motion = cfg.device_id("hallway_motion").unwrap();
    let light = cfg.device_id("hallway_light").unwrap();

    let mut engine = build_engine_at(&cfg, None, boot_midnight());
    engine.start();
    engine.advance(12 * 60 * 60 * 1000); // noon → SunUp(true)

    engine.inject(Event::OccupancyChanged {
        device: motion,
        occupied: true,
    });
    assert_ne!(
        engine.switch_state(light),
        Some(true),
        "sun_up: false guard blocks it"
    );
}

#[test]
fn rule_references_are_checked() {
    // Every kind of bad reference, reported together.
    let bad = r#"
adapters:
  z: { type: mock }
devices:
  lamp: { adapter: z, capabilities: [switch] }
rules:
  oops:
    when: { occupancy: lamp }          # lamp has no occupancy capability
    then:
      - set_brightness: { device: lamp, value: 50 }  # nor brightness
      - turn_on: ghost                 # unknown device
      - activate_scene: nope           # unknown scene
"#;
    let errs = compile_str(bad).expect_err("should fail");
    let codes: Vec<_> = errs.errors().map(|d| d.code).collect();

    assert!(codes.contains(&"E_UNKNOWN_DEVICE"));
    assert!(codes.contains(&"E_UNKNOWN_SCENE"));
    assert_eq!(
        codes
            .iter()
            .filter(|c| **c == "E_MISSING_CAPABILITY")
            .count(),
        2,
        "missing occupancy (trigger) and brightness (command)"
    );
}

#[test]
fn dangling_timer_and_unused_scene_are_warnings() {
    let cfg = compile_str(
        r#"
adapters:
  z: { type: mock }
devices:
  lamp: { adapter: z, capabilities: [switch] }
scenes:
  never_used:
    - turn_off: lamp
rules:
  r:
    when: { timer: phantom }   # nothing ever schedules 'phantom'
    then:
      - turn_on: lamp
"#,
    )
    .expect("warnings do not fail compilation");

    let codes: Vec<_> = cfg.warnings.iter().map(|d| d.code).collect();
    assert!(codes.contains(&"E_DANGLING_TIMER"));
    assert!(codes.contains(&"E_UNUSED_SCENE"));
}

#[test]
fn scenes_compile_and_expand() {
    let cfg = compile_str(
        r#"
adapters:
  z: { type: mock }
devices:
  a:      { adapter: z, capabilities: [switch] }
  b:      { adapter: z, capabilities: [switch, brightness] }
  remote: { adapter: z, capabilities: [battery], events: { tap: single, dbl: double } }
scenes:
  movie:
    - turn_off: a
    - set_brightness: { device: b, value: 20 }
rules:
  go:
    when: { event: remote.dbl }
    then:
      - activate_scene: movie
"#,
    )
    .expect("should compile");

    let mut engine = build_engine(&cfg);
    let a = cfg.device_id("a").unwrap();
    let remote = cfg.device_id("remote").unwrap();
    let tap = cfg.action_id(remote, "tap").unwrap();
    let dbl = cfg.action_id(remote, "dbl").unwrap();

    // The wrong event — the rule wants `dbl`.
    engine.inject(Event::Action {
        device: remote,
        action: tap,
    });
    assert_eq!(engine.switch_state(a), None, "tap should not match");

    // `dbl` fires the rule, expanding the scene to its commands.
    engine.inject(Event::Action {
        device: remote,
        action: dbl,
    });
    assert_eq!(
        engine.switch_state(a),
        Some(false),
        "scene's turn_off applied"
    );
}

#[test]
fn event_names_disambiguate_buttons_on_one_device() {
    // One 3-gang switch driving two lights from two different paddles, proving
    // `event: <device>.<name>` addresses an individual declared event.
    let cfg = compile_str(
        r#"
adapters:
  z: { type: mock }
devices:
  wall:  { adapter: z, capabilities: [], events: { top: toggle_l1, bottom: toggle_l3 } }
  lamp1: { adapter: z, capabilities: [switch] }
  lamp2: { adapter: z, capabilities: [switch] }
rules:
  on1:  { when: { event: wall.top },    then: [ { turn_on:  lamp1 } ] }
  off2: { when: { event: wall.bottom }, then: [ { turn_off: lamp2 } ] }
"#,
    )
    .expect("should compile");

    let mut engine = build_engine(&cfg);
    let wall = cfg.device_id("wall").unwrap();
    let lamp1 = cfg.device_id("lamp1").unwrap();
    let lamp2 = cfg.device_id("lamp2").unwrap();
    let top = cfg.action_id(wall, "top").unwrap();
    let bottom = cfg.action_id(wall, "bottom").unwrap();

    // `bottom` drives off2 (lamp2), not on1 (lamp1).
    engine.inject(Event::Action {
        device: wall,
        action: bottom,
    });
    assert_eq!(engine.switch_state(lamp1), None, "bottom ≠ top");
    assert_eq!(engine.switch_state(lamp2), Some(false));

    // `top` turns on lamp1.
    engine.inject(Event::Action {
        device: wall,
        action: top,
    });
    assert_eq!(engine.switch_state(lamp1), Some(true));
}

#[test]
fn triggering_an_undeclared_event_is_a_compile_error() {
    let errs = compile_str(
        r#"
adapters:
  z: { type: mock }
devices:
  wall: { adapter: z, capabilities: [], events: { top: toggle_l1 } }
  lamp: { adapter: z, capabilities: [switch] }
rules:
  go: { when: { event: wall.bottom }, then: [ { turn_on: lamp } ] }
"#,
    )
    .expect_err("wall declares no `bottom` event");
    assert!(format!("{errs}").contains("E_UNKNOWN_EVENT"), "got: {errs}");
}

#[test]
fn color_commands_require_declared_capabilities() {
    let errs = compile_str(
        r##"
adapters:
  z: { type: mock }
devices:
  rgb:  { adapter: z, capabilities: [switch] }
  ct:   { adapter: z, capabilities: [switch] }
rules:
  bad_rgb:
    when: { timer: t }
    then:
      - set_color: { device: rgb, color: "#ff0000" }
  bad_ct:
    when: { timer: t }
    then:
      - set_color_temperature: { device: ct, kelvin: 2700 }
"##,
    )
    .expect_err("missing color capabilities");
    let codes: Vec<_> = errs.errors().map(|d| d.code).collect();
    assert_eq!(
        codes
            .iter()
            .filter(|c| **c == "E_MISSING_CAPABILITY")
            .count(),
        2
    );
}

#[test]
fn color_formats_compile_to_runtime_commands() {
    let cfg = compile_str(
        r#"
adapters:
  z: { type: mock }
devices:
  lamp: { adapter: z, capabilities: [color, color_temperature] }
rules:
  go:
    when: { timer: t }
    then:
      - set_color: { device: lamp, color: { r: 255, g: 128, b: 0 } }
      - set_color_temperature: { device: lamp, kelvin: 4000 }
"#,
    )
    .expect("should compile");

    let lamp = cfg.device_id("lamp").unwrap();
    let rule = &cfg.rules[0];
    assert_eq!(
        rule.commands[0],
        domiform::Command::SetColor {
            device: lamp,
            r: 255,
            g: 128,
            b: 0,
            transition: None,
        }
    );
    // 1_000_000 / 4000 = 250 mireds
    assert_eq!(
        rule.commands[1],
        domiform::Command::SetColorTemperature {
            device: lamp,
            mireds: 250,
            transition: None,
        }
    );
}

#[test]
fn send_ir_code_requires_ir_transmitter_capability() {
    let errs = compile_str(
        r#"
adapters:
  z: { type: mock }
devices:
  blaster: { adapter: z, capabilities: [switch] }
rules:
  bad:
    when: { timer: t }
    then:
      - send_ir_code: { device: blaster, code: "abc123" }
"#,
    )
    .expect_err("missing ir_transmitter capability");
    assert!(format!("{errs}").contains("E_MISSING_CAPABILITY"));
}

#[test]
fn send_ir_code_compiles_to_runtime_command() {
    let cfg = compile_str(
        r#"
adapters:
  z: { type: mock }
devices:
  ac_blaster: { adapter: z, capabilities: [ir_transmitter] }
rules:
  turn_ac_off:
    when: { timer: t }
    then:
      - send_ir_code: { device: ac_blaster, code: "BW4jahFCAuAXAQGMBsADAHLgAgvAE4AH4BcBwCeAB+AFRw8vm24jqwhCAv//biOrCEIC" }
"#,
    )
    .expect("should compile");

    let blaster = cfg.device_id("ac_blaster").unwrap();
    assert_eq!(
        cfg.rules[0].commands[0],
        domiform::Command::SendIrCode {
            device: blaster,
            code: "BW4jahFCAuAXAQGMBsADAHLgAgvAE4AH4BcBwCeAB+AFRw8vm24jqwhCAv//biOrCEIC".into(),
        }
    );
}
