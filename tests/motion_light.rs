//! End-to-end proof of the core model against the nastiest MVP automation:
//! a motion light with re-trigger. This single test exercises events,
//! conditions, commands, scene-free dispatch, the scheduler adapter, and —
//! crucially — timer *cancellation* on re-trigger.
//!
//! Wiring (what the compiler will eventually emit):
//!   rule A: Occupancy(true)  -> SetSwitch(on)  + CancelTimer("hallway_off")
//!   rule B: Occupancy(false) -> ScheduleTimer("hallway_off", 10m)
//!   rule C: Timer("hallway_off") -> SetSwitch(off)

use domiform::ids::DeviceId;
use domiform::{
    CapabilityKind, CapabilityState, Command, Condition, Engine, Event, MockDeviceAdapter, Rule,
    RuleId, TimerKey, Trigger,
};

const TEN_MIN: u64 = 10 * 60 * 1000;

fn build() -> (Engine, DeviceId) {
    let motion = DeviceId(1);
    let light = DeviceId(2);
    let key = TimerKey::new("hallway_off");

    let mut engine = Engine::new();
    let device_adapter = engine.add_adapter(Box::new(MockDeviceAdapter));
    engine.bind_device(light, device_adapter);
    // motion is a pure event source; no commands are sent to it.

    // rule A
    engine.add_rule(Rule::new(
        RuleId(1),
        Trigger::Changed {
            device: motion,
            kind: CapabilityKind::Occupancy,
            to: true,
        },
        Condition::Always,
        vec![
            Command::SetSwitch {
                device: light,
                on: true,
            },
            Command::CancelTimer { key: key.clone() },
        ],
    ));
    // rule B
    engine.add_rule(Rule::new(
        RuleId(2),
        Trigger::Changed {
            device: motion,
            kind: CapabilityKind::Occupancy,
            to: false,
        },
        Condition::Always,
        vec![Command::ScheduleTimer {
            key: key.clone(),
            after: TEN_MIN,
        }],
    ));
    // rule C
    engine.add_rule(Rule::new(
        RuleId(3),
        Trigger::Timer { key: key.clone() },
        Condition::Always,
        vec![Command::SetSwitch {
            device: light,
            on: false,
        }],
    ));

    (engine, light)
}

#[test]
fn motion_turns_light_on() {
    let (mut engine, light) = build();
    engine.inject(Event::StateReported {
        device: DeviceId(1),
        state: CapabilityState::Occupancy(true),
    });
    assert_eq!(engine.switch_state(light), Some(true));
}

#[test]
fn re_trigger_cancels_pending_off() {
    let (mut engine, light) = build();

    // Motion, then clear: a 10-minute off-timer is now pending.
    engine.inject(Event::StateReported {
        device: DeviceId(1),
        state: CapabilityState::Occupancy(true),
    });
    engine.inject(Event::StateReported {
        device: DeviceId(1),
        state: CapabilityState::Occupancy(false),
    });

    // Five minutes later, motion again — this must CANCEL the pending timer.
    engine.advance(5 * 60 * 1000);
    engine.inject(Event::StateReported {
        device: DeviceId(1),
        state: CapabilityState::Occupancy(true),
    });

    // Advance well past the original 10-minute deadline. Because the timer was
    // cancelled, the light must still be ON.
    engine.advance(TEN_MIN);
    assert_eq!(
        engine.switch_state(light),
        Some(true),
        "re-trigger should have cancelled the off-timer"
    );
}

#[test]
fn timer_fires_when_not_re_triggered() {
    let (mut engine, light) = build();

    engine.inject(Event::StateReported {
        device: DeviceId(1),
        state: CapabilityState::Occupancy(true),
    });
    engine.inject(Event::StateReported {
        device: DeviceId(1),
        state: CapabilityState::Occupancy(false),
    });

    // No further motion. Just before the deadline: still on.
    engine.advance(TEN_MIN - 1);
    assert_eq!(engine.switch_state(light), Some(true));

    // At the deadline: the timer fires and the light turns off.
    engine.advance(1);
    assert_eq!(engine.switch_state(light), Some(false));
}
