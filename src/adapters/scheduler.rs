//! Time as an adapter: holds pending timers keyed by name with absolute fire
//! times, fires them on `tick`, and creates/cancels them via `dispatch`. Crons,
//! relative timers, debounce, and time conditions all fall out of this one
//! mechanism — "time is an adapter."

use std::collections::HashMap;

use super::{Adapter, DispatchOutcome};
use crate::model::{Command, Event, Millis, TimerKey};

#[derive(Default)]
pub struct SchedulerAdapter {
    pending: HashMap<TimerKey, Millis>,
}

impl Adapter for SchedulerAdapter {
    fn dispatch(&mut self, cmd: &Command, now: Millis) -> DispatchOutcome {
        match cmd {
            Command::ScheduleTimer { key, after } => {
                self.pending.insert(key.clone(), now + after);
            }
            Command::CancelTimer { key } => {
                self.pending.remove(key);
            }
            _ => {}
        }
        DispatchOutcome::ok()
    }

    fn tick(&mut self, now: Millis) -> Vec<Event> {
        // Collect due timers deterministically (sorted by fire time, then key).
        let mut due: Vec<(Millis, TimerKey)> = self
            .pending
            .iter()
            .filter(|(_, &fire_at)| fire_at <= now)
            .map(|(k, &t)| (t, k.clone()))
            .collect();
        due.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1 .0.cmp(&b.1 .0)));
        for (_, key) in &due {
            self.pending.remove(key);
        }
        due.into_iter()
            .map(|(_, key)| Event::TimerElapsed { key })
            .collect()
    }

    fn next_wake(&self, now: Millis) -> Option<Millis> {
        // The soonest pending timer. `saturating_sub` so an already-due timer
        // reports 0 (wake immediately) rather than underflowing.
        self.pending
            .values()
            .min()
            .map(|&fire_at| fire_at.saturating_sub(now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_wake_is_delay_to_soonest_timer() {
        let mut s = SchedulerAdapter::default();
        assert_eq!(s.next_wake(0), None, "no timers, nothing to wake for");

        s.dispatch(
            &Command::ScheduleTimer {
                key: TimerKey::new("a"),
                after: 5000,
            },
            1000,
        );
        s.dispatch(
            &Command::ScheduleTimer {
                key: TimerKey::new("b"),
                after: 1000,
            },
            1000,
        );
        // b fires at 2000, a at 6000; soonest is b.
        assert_eq!(s.next_wake(1500), Some(500));
        // Past every fire time → 0, never an underflow.
        assert_eq!(s.next_wake(9999), Some(0));
    }
}
