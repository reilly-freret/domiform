//! Wall-clock schedules: the `schedules:` config surface (sugar + raw cron), its
//! static checks, and an end-to-end fire — a cron schedule driving a rule through
//! the clock adapter, deterministically, on a fixed boot epoch.

use chrono::{TimeZone, Utc};
use domiform::{build_engine_at, compile_str, CapabilityState, Event};

/// Boot epoch for a given UTC wall-clock time, so schedule fires are replayable.
fn epoch(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
    Utc.with_ymd_and_hms(y, mo, d, h, mi, 0)
        .unwrap()
        .timestamp_millis()
}

const E2E: &str = r#"
system:
  timezone: UTC

adapters:
  z: { type: mock }

devices:
  lamp: { adapter: z, capabilities: [switch] }

schedules:
  wake: { daily: "06:40" }

rules:
  morning:
    when: { schedule: wake }
    then: [ { turn_on: lamp } ]
"#;

#[test]
fn a_daily_schedule_fires_a_rule_through_the_clock() {
    let cfg = compile_str(E2E).expect("compiles clean");
    assert!(cfg.warnings.is_empty(), "no warnings: {:?}", cfg.warnings);
    let lamp = cfg.device_id("lamp").unwrap();

    // Boot at 06:39 UTC; the 06:40 daily schedule is 60s away.
    let mut engine = build_engine_at(&cfg, None, epoch(2024, 6, 1, 6, 39));
    engine.start();
    assert_eq!(engine.switch_state(lamp), None, "nothing has fired yet");

    engine.advance(60 * 1000); // cross 06:40
    assert_eq!(
        engine.switch_state(lamp),
        Some(true),
        "the schedule fired and the rule turned the lamp on"
    );
}

#[test]
fn sugar_desugars_to_the_expected_cron() {
    let cfg = compile_str(
        r#"
adapters: { z: { type: mock } }
devices:  { lamp: { adapter: z, capabilities: [switch] } }
schedules:
  a: { daily: "06:40" }
  b: { weekday: "08:00" }
  c: { weekend: "09:30" }
  d: { cron: "*/15 9-17 * * *" }
rules:
  ra: { when: { schedule: a }, then: [ { turn_on: lamp } ] }
  rb: { when: { schedule: b }, then: [ { turn_on: lamp } ] }
  rc: { when: { schedule: c }, then: [ { turn_on: lamp } ] }
  rd: { when: { schedule: d }, then: [ { turn_on: lamp } ] }
"#,
    )
    .expect("compiles");

    let cron_of = |name: &str| {
        cfg.schedules
            .iter()
            .find(|s| s.name == name)
            .map(|s| s.cron.as_str())
            .unwrap()
    };
    assert_eq!(cron_of("a"), "40 6 * * *");
    assert_eq!(cron_of("b"), "0 8 * * 1-5");
    assert_eq!(cron_of("c"), "30 9 * * 0,6");
    assert_eq!(cron_of("d"), "*/15 9-17 * * *"); // raw cron passes through
}

#[test]
fn bad_schedules_are_compile_errors() {
    let bad = r#"
adapters: { z: { type: mock } }
devices:  { lamp: { adapter: z, capabilities: [switch] } }
schedules:
  no_fields:   {}
  two_fields:  { daily: "06:40", cron: "0 8 * * *" }
  bad_cron:    { cron: "not a cron" }
  bad_time:    { daily: "26:99" }
rules:
  r: { when: { schedule: missing }, then: [ { turn_on: lamp } ] }
"#;
    let errs = compile_str(bad).expect_err("should fail");
    let codes: Vec<_> = errs.errors().map(|d| d.code).collect();

    assert!(codes.contains(&"E_BAD_SCHEDULE"), "got: {codes:?}");
    assert!(codes.contains(&"E_BAD_CRON"), "got: {codes:?}");
    assert!(codes.contains(&"E_BAD_TIME"), "got: {codes:?}");
    assert!(codes.contains(&"E_UNKNOWN_SCHEDULE"), "got: {codes:?}");
}

#[test]
fn an_unreferenced_schedule_is_a_warning() {
    let cfg = compile_str(
        r#"
adapters: { z: { type: mock } }
devices:  { lamp: { adapter: z, capabilities: [switch] } }
schedules:
  orphan: { daily: "06:40" }
"#,
    )
    .expect("warnings don't fail compilation");
    let codes: Vec<_> = cfg.warnings.iter().map(|d| d.code).collect();
    assert!(codes.contains(&"E_UNUSED_SCHEDULE"), "got: {codes:?}");
}

#[test]
fn an_unknown_timezone_is_a_compile_error() {
    let errs = compile_str(
        r#"
system: { timezone: "America/New_Yrok" }
adapters: { z: { type: mock } }
devices:  { lamp: { adapter: z, capabilities: [switch] } }
"#,
    )
    .expect_err("typo'd timezone should fail");
    let codes: Vec<_> = errs.errors().map(|d| d.code).collect();
    assert!(codes.contains(&"E_BAD_TIMEZONE"), "got: {codes:?}");
}

/// A `schedule:` trigger naming a schedule that exists but a real event flowing
/// through must not accidentally match unrelated triggers — sanity that the
/// `TimeReached` path is isolated. (Also documents the `Event` API.)
#[test]
fn time_reached_only_matches_schedule_triggers() {
    let cfg = compile_str(E2E).unwrap();
    let lamp = cfg.device_id("lamp").unwrap();
    let mut engine = build_engine_at(&cfg, None, epoch(2024, 6, 1, 12, 0));
    engine.start();

    // An unrelated occupancy event must not fire the schedule rule.
    engine.inject(Event::StateReported {
        device: lamp,
        state: CapabilityState::Occupancy(true),
    });
    assert_eq!(engine.switch_state(lamp), None);
}
