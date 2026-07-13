//! Proof that conditions can gate behavior on *state read from a synthetic
//! device the clock adapter maintains*. The exact same rule fires at night and
//! is suppressed in daylight — the only difference is time advanced on the bus.
//!
//! This is the load-bearing claim for "rules are pure and testable": the rule
//! never calls a clock; it reads `SunUp` from the store like any other state.

use chrono::{TimeZone, Utc};
use chrono_tz::Tz;
use domiform::ids::DeviceId;
use domiform::{
    CapabilityKind, CapabilityState, ClockAdapter, Command, Condition, Engine, Event,
    MockDeviceAdapter, Rule, RuleId, Trigger,
};

const MOTION: DeviceId = DeviceId(1);
const LIGHT: DeviceId = DeviceId(2);
const SUN: DeviceId = DeviceId(3);

const MINUTE_MS: u64 = 60 * 1000;

/// Boot at a UTC midnight on the equator: the clock's real ephemeris then puts
/// sunrise ~06:00 and sunset ~18:00, so the test's noon/19:00 advances land
/// squarely in day and night. Fixed epoch → deterministic replay.
fn boot_epoch_ms() -> i64 {
    Utc.with_ymd_and_hms(2024, 6, 1, 0, 0, 0)
        .unwrap()
        .timestamp_millis()
}

/// Hallway light comes on with motion — but only while the sun is down.
fn build() -> Engine {
    let mut engine = Engine::new();

    let device_adapter = engine.add_adapter(Box::new(MockDeviceAdapter));
    engine.bind_device(LIGHT, device_adapter);

    // The clock adapter backs the synthetic SUN device. It is never bound for
    // commands — it is a read-only state source.
    engine.add_adapter(Box::new(ClockAdapter::new(
        SUN,
        boot_epoch_ms(),
        Tz::UTC,
        0.0,
        0.0,
    )));

    engine.add_rule(Rule::new(
        RuleId(1),
        Trigger::Changed {
            device: MOTION,
            kind: CapabilityKind::Occupancy,
            to: true,
        },
        Condition::BoolEquals {
            device: SUN,
            kind: CapabilityKind::SunUp,
            value: false, // i.e. "after sunset / before sunrise"
        },
        vec![Command::SetSwitch {
            device: LIGHT,
            on: true,
        }],
    ));

    engine
}

fn motion() -> Event {
    Event::StateReported {
        device: MOTION,
        state: CapabilityState::Occupancy(true),
    }
}

/// The sensor going idle — needed between two `motion()`s so the second is a
/// real occupancy *edge* (the `changed ... to: true` trigger fires only on the
/// transition into occupied, not on a repeated occupied report).
fn motion_clear() -> Event {
    Event::StateReported {
        device: MOTION,
        state: CapabilityState::Occupancy(false),
    }
}

#[test]
fn motion_turns_on_light_at_night() {
    let mut engine = build();
    engine.start(); // boot at 00:00 → clock publishes SunUp(false)
    engine.inject(motion());
    assert_eq!(
        engine.switch_state(LIGHT),
        Some(true),
        "after sunset the motion rule should fire"
    );
}

#[test]
fn motion_is_ignored_in_daylight() {
    let mut engine = build();
    engine.start();
    engine.advance(12 * 60 * MINUTE_MS); // advance to 12:00 → SunUp(true)
    engine.inject(motion());
    assert_ne!(
        engine.switch_state(LIGHT),
        Some(true),
        "in daylight the condition is false, so the light must stay off"
    );
}

#[test]
fn condition_re_evaluates_as_time_passes() {
    // One engine, same rule, crossing sunset: suppressed before, fires after.
    let mut engine = build();
    engine.start();

    engine.advance(12 * 60 * MINUTE_MS); // noon
    engine.inject(motion());
    assert_ne!(engine.switch_state(LIGHT), Some(true), "noon: suppressed");
    engine.inject(motion_clear()); // sensor returns to idle

    engine.advance(7 * 60 * MINUTE_MS); // 19:00, past sunset
    engine.inject(motion()); // a fresh occupancy edge
    assert_eq!(engine.switch_state(LIGHT), Some(true), "evening: fires");
}
