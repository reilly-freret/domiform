//! The clock as an adapter, backing a synthetic device. On every tick it
//! publishes the current `TimeOfDay` and `SunUp` as ordinary `StateReported`
//! events, so conditions read "is it dark?" exactly like any other state.
//!
//! Skeleton simplifications, to be replaced without touching the model:
//! * `now` millis are interpreted as time since local midnight (virtual time
//!   starts at 00:00). A real clock maps a wall-clock timestamp instead.
//! * Sun state is a fixed sunrise/sunset threshold rather than a real solar
//!   ephemeris from latitude/longitude/date.

use super::{Adapter, DispatchOutcome};
use crate::ids::DeviceId;
use crate::model::{CapabilityState, Command, Event, Millis};

pub struct ClockAdapter {
    device: DeviceId,
    sunrise_min: u16,
    sunset_min: u16,
    last_emit_min: Option<u16>,
}

impl ClockAdapter {
    pub fn new(device: DeviceId, sunrise_min: u16, sunset_min: u16) -> Self {
        ClockAdapter {
            device,
            sunrise_min,
            sunset_min,
            last_emit_min: None,
        }
    }

    fn minute_of_day(now: Millis) -> u16 {
        ((now / 60_000) % 1440) as u16
    }
}

impl Adapter for ClockAdapter {
    fn dispatch(&mut self, _cmd: &Command, _now: Millis) -> DispatchOutcome {
        // Read-only device: nothing is ever commanded to the clock. Treat a
        // stray command as a permanent failure rather than a silent no-op.
        DispatchOutcome::Permanent("clock device accepts no commands".into())
    }

    fn tick(&mut self, now: Millis) -> Vec<Event> {
        let minute = Self::minute_of_day(now);
        if self.last_emit_min == Some(minute) {
            return Vec::new(); // only republish when the minute actually changes
        }
        self.last_emit_min = Some(minute);
        let sun_up = minute >= self.sunrise_min && minute < self.sunset_min;
        vec![
            Event::StateReported {
                device: self.device,
                state: CapabilityState::TimeOfDay(minute),
            },
            Event::StateReported {
                device: self.device,
                state: CapabilityState::SunUp(sun_up),
            },
        ]
    }

    fn next_wake(&self, now: Millis) -> Option<Millis> {
        // State is republished only when the minute changes, so ask to be woken
        // at the next minute boundary — no sooner, no later.
        Some(60_000 - (now % 60_000))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_wake_targets_the_next_minute_boundary() {
        let c = ClockAdapter::new(DeviceId(0), 0, 0);
        assert_eq!(c.next_wake(0), Some(60_000)); // at :00, next change is +60s
        assert_eq!(c.next_wake(90_000), Some(30_000)); // 1m30s in → 30s to 2m
    }
}
