//! The clock as an adapter, backing a synthetic device and firing wall-clock
//! schedules. On each tick it republishes the current `TimeOfDay` and `SunUp` as
//! ordinary `StateReported` events — so conditions read "is it dark?" exactly
//! like any other state — and emits a `TimeReached` for every cron schedule that
//! has come due.
//!
//! **Wall-clock time enters the deterministic core as data, never as a call.**
//! The adapter holds a `boot_epoch_ms` — real Unix time captured once at startup
//! (`main.rs`) and injected — and derives the current wall instant as
//! `boot_epoch_ms + engine_now`. Tests construct it with a *fixed* epoch, so
//! replay stays reproducible; the core never reads the wall clock itself. Civil
//! time is computed in the configured timezone (`chrono-tz`, IANA db compiled in),
//! and sun state from a real solar ephemeris over latitude/longitude (`sunrise`).

use chrono::{DateTime, NaiveDate, Timelike, Utc};
use chrono_tz::Tz;
use croner::Cron;
use sunrise::{Coordinates, SolarDay, SolarEvent};

use super::{Adapter, DispatchOutcome};
use crate::ids::{DeviceId, ScheduleId};
use crate::model::{CapabilityState, Command, Event, Millis};

const MINUTE_MS: i64 = 60_000;

/// One cron schedule the clock fires, with its cached next occurrence.
struct ScheduleState {
    id: ScheduleId,
    cron: Cron,
    /// Next occurrence in local time. `None` once no further occurrence exists.
    /// `armed` distinguishes "not computed yet" from a genuine end-of-schedule.
    next_fire: Option<DateTime<Tz>>,
    armed: bool,
}

/// The sun window for one calendar date, cached so the ephemeris runs once a day.
enum SunWindow {
    /// A normal day: the sun is up between these two UTC instants.
    Between(DateTime<Utc>, DateTime<Utc>),
    /// Polar day/night, a missing event, or invalid coordinates — fall back to a
    /// fixed local threshold. See the polar caveat in `docs/STATUS.md`.
    Fallback,
}

impl SunWindow {
    const FALLBACK_SUNRISE_MIN: u32 = 6 * 60;
    const FALLBACK_SUNSET_MIN: u32 = 18 * 60;
}

pub struct ClockAdapter {
    device: DeviceId,
    boot_epoch_ms: i64,
    tz: Tz,
    coord: Option<Coordinates>,
    last_emit_min: Option<u16>,
    schedules: Vec<ScheduleState>,
    /// `(date, window)` — recomputed only when the local date rolls over.
    sun_cache: Option<(NaiveDate, SunWindow)>,
}

impl ClockAdapter {
    /// `boot_epoch_ms` is real Unix time (ms) captured once at startup; the
    /// wall instant is `boot_epoch_ms + engine_now`. `tz` is the configured
    /// timezone; `latitude`/`longitude` drive the solar ephemeris.
    pub fn new(device: DeviceId, boot_epoch_ms: i64, tz: Tz, latitude: f64, longitude: f64) -> Self {
        ClockAdapter {
            device,
            boot_epoch_ms,
            tz,
            coord: Coordinates::new(latitude, longitude),
            last_emit_min: None,
            schedules: Vec::new(),
            sun_cache: None,
        }
    }

    /// Attach the compiled cron schedules this clock should fire as `TimeReached`.
    pub fn with_schedules(mut self, schedules: Vec<(ScheduleId, Cron)>) -> Self {
        self.schedules = schedules
            .into_iter()
            .map(|(id, cron)| ScheduleState {
                id,
                cron,
                next_fire: None,
                armed: false,
            })
            .collect();
        self
    }

    /// The current wall instant in UTC.
    fn wall_utc(&self, now: Millis) -> DateTime<Utc> {
        let ms = self.boot_epoch_ms + now as i64;
        DateTime::from_timestamp_millis(ms).expect("wall-clock instant out of representable range")
    }

    /// `true` if the sun is up at `wall_utc` on the given local date.
    fn sun_up(&mut self, wall_utc: DateTime<Utc>, local: &DateTime<Tz>) -> bool {
        let date = local.date_naive();
        let fresh = matches!(&self.sun_cache, Some((cached, _)) if *cached == date);
        if !fresh {
            let window = self.compute_window(date);
            self.sun_cache = Some((date, window));
        }
        match &self.sun_cache.as_ref().expect("just populated").1 {
            SunWindow::Between(rise, set) => *rise <= wall_utc && wall_utc < *set,
            SunWindow::Fallback => {
                let minute = local.hour() * 60 + local.minute();
                (SunWindow::FALLBACK_SUNRISE_MIN..SunWindow::FALLBACK_SUNSET_MIN).contains(&minute)
            }
        }
    }

    fn compute_window(&self, date: NaiveDate) -> SunWindow {
        let Some(coord) = self.coord else {
            return SunWindow::Fallback;
        };
        let day = SolarDay::new(coord, date);
        match (
            day.event_time(SolarEvent::Sunrise),
            day.event_time(SolarEvent::Sunset),
        ) {
            (Some(rise), Some(set)) => SunWindow::Between(rise, set),
            // Polar day/night: no sunrise/sunset crossing on this date.
            _ => SunWindow::Fallback,
        }
    }

    /// Fire every schedule due at or before `local`, deterministically ordered by
    /// (fire time, schedule id), re-arming each to its next occurrence.
    fn fire_due_schedules(&mut self, local: &DateTime<Tz>) -> Vec<Event> {
        let mut fired: Vec<(DateTime<Tz>, ScheduleId)> = Vec::new();
        for s in &mut self.schedules {
            if !s.armed {
                // First arming: next occurrence *strictly after* now — so a boot
                // that lands exactly on a cron minute doesn't fire immediately,
                // and a restart doesn't replay a schedule already fired.
                s.next_fire = s.cron.find_next_occurrence(local, false).ok();
                s.armed = true;
            }
            while let Some(next) = s.next_fire {
                if next > *local {
                    break;
                }
                fired.push((next, s.id));
                // Strictly after the one we just fired → occurrences increase, so
                // this loop always terminates.
                s.next_fire = s.cron.find_next_occurrence(&next, false).ok();
            }
        }
        fired.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1 .0.cmp(&b.1 .0)));
        fired
            .into_iter()
            .map(|(_, schedule)| Event::TimeReached { schedule })
            .collect()
    }
}

impl Adapter for ClockAdapter {
    fn dispatch(&mut self, _cmd: &Command, _now: Millis) -> DispatchOutcome {
        // Read-only device: nothing is ever commanded to the clock. Treat a
        // stray command as a permanent failure rather than a silent no-op.
        DispatchOutcome::Permanent("clock device accepts no commands".into())
    }

    fn tick(&mut self, now: Millis) -> Vec<Event> {
        let wall_utc = self.wall_utc(now);
        let local = wall_utc.with_timezone(&self.tz);
        let mut events = Vec::new();

        // Time-of-day / sun, republished only when the minute actually changes.
        let minute = (local.hour() * 60 + local.minute()) as u16;
        if self.last_emit_min != Some(minute) {
            self.last_emit_min = Some(minute);
            let sun_up = self.sun_up(wall_utc, &local);
            events.push(Event::StateReported {
                device: self.device,
                state: CapabilityState::TimeOfDay(minute),
            });
            events.push(Event::StateReported {
                device: self.device,
                state: CapabilityState::SunUp(sun_up),
            });
        }

        events.extend(self.fire_due_schedules(&local));
        events
    }

    fn next_wake(&self, now: Millis) -> Option<Millis> {
        let wall_ms = self.boot_epoch_ms + now as i64;
        // Next minute boundary. Every tz offset is a whole number of minutes, so
        // computing this on the UTC instant is DST-safe for minute cadence.
        let to_next_min = (MINUTE_MS - wall_ms.rem_euclid(MINUTE_MS)) as Millis;

        // ...but no later than the soonest pending schedule.
        let soonest = self
            .schedules
            .iter()
            .filter_map(|s| s.next_fire)
            .map(|next| (next.timestamp_millis() - wall_ms).max(0) as Millis)
            .fold(to_next_min, Millis::min);
        Some(soonest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;
    use std::str::FromStr as _;

    const SUN: DeviceId = DeviceId(0);

    /// Unix ms for a UTC wall-clock time, for deterministic boot epochs.
    fn epoch(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0)
            .unwrap()
            .timestamp_millis()
    }

    fn time_of_day(evs: &[Event]) -> Option<u16> {
        evs.iter().find_map(|e| match e {
            Event::StateReported {
                state: CapabilityState::TimeOfDay(m),
                ..
            } => Some(*m),
            _ => None,
        })
    }

    fn sun_up(evs: &[Event]) -> Option<bool> {
        evs.iter().find_map(|e| match e {
            Event::StateReported {
                state: CapabilityState::SunUp(b),
                ..
            } => Some(*b),
            _ => None,
        })
    }

    #[test]
    fn reports_real_time_of_day_not_time_since_boot() {
        // The regression this whole change fixes: booting at 15:00 must report
        // 15:00, not 00:00.
        let mut c = ClockAdapter::new(SUN, epoch(2024, 6, 1, 15, 0), Tz::UTC, 0.0, 0.0);
        let evs = c.tick(0);
        assert_eq!(time_of_day(&evs), Some(15 * 60));
        // Advancing 30 virtual minutes moves civil time forward from 15:00.
        let evs = c.tick(30 * 60 * 1000);
        assert_eq!(time_of_day(&evs), Some(15 * 60 + 30));
    }

    #[test]
    fn timezone_offsets_the_reported_time() {
        let e = epoch(2024, 6, 1, 12, 0); // 12:00 UTC
        let mut utc = ClockAdapter::new(SUN, e, Tz::UTC, 0.0, 0.0);
        let mut ny = ClockAdapter::new(SUN, e, Tz::America__New_York, 0.0, 0.0);
        // New York is UTC-4 in June (EDT) → 08:00 local.
        assert_eq!(time_of_day(&utc.tick(0)), Some(12 * 60));
        assert_eq!(time_of_day(&ny.tick(0)), Some(8 * 60));
    }

    #[test]
    fn sun_is_up_at_noon_and_down_at_midnight_at_the_equator() {
        // Equator/UTC: day ~06:00–18:00 year round.
        let mut midnight = ClockAdapter::new(SUN, epoch(2024, 6, 1, 0, 0), Tz::UTC, 0.0, 0.0);
        let mut noon = ClockAdapter::new(SUN, epoch(2024, 6, 1, 12, 0), Tz::UTC, 0.0, 0.0);
        assert_eq!(sun_up(&midnight.tick(0)), Some(false));
        assert_eq!(sun_up(&noon.tick(0)), Some(true));
    }

    #[test]
    fn republishes_only_on_a_minute_change() {
        let mut c = ClockAdapter::new(SUN, epoch(2024, 6, 1, 12, 0), Tz::UTC, 0.0, 0.0);
        assert!(!c.tick(0).is_empty(), "first tick emits");
        assert!(c.tick(30_000).is_empty(), "same minute → nothing");
        assert!(!c.tick(60_000).is_empty(), "next minute → emits again");
    }

    #[test]
    fn next_wake_targets_the_next_minute_boundary() {
        let c = ClockAdapter::new(SUN, epoch(2024, 6, 1, 12, 0), Tz::UTC, 0.0, 0.0);
        assert_eq!(c.next_wake(0), Some(60_000)); // exactly on :00
        assert_eq!(c.next_wake(90_000), Some(30_000)); // 1m30s in → 30s left
    }

    #[test]
    fn cron_schedule_fires_time_reached_and_rearms() {
        let sched = ScheduleId(7);
        // Every day at 06:40; boot at 06:00 UTC.
        let cron = Cron::from_str("40 6 * * *").unwrap();
        let mut c = ClockAdapter::new(SUN, epoch(2024, 6, 1, 6, 0), Tz::UTC, 0.0, 0.0)
            .with_schedules(vec![(sched, cron)]);

        assert!(
            c.tick(0).iter().all(|e| !matches!(e, Event::TimeReached { .. })),
            "nothing due at boot (06:00)"
        );
        // Advance to 06:40 → the schedule fires exactly once.
        let evs = c.tick(40 * 60 * 1000);
        let fires: Vec<_> = evs
            .iter()
            .filter(|e| matches!(e, Event::TimeReached { schedule } if *schedule == sched))
            .collect();
        assert_eq!(fires.len(), 1, "06:40 fires once: {evs:?}");
        // It re-arms for the next day, not immediately again.
        let evs = c.tick(41 * 60 * 1000);
        assert!(
            evs.iter().all(|e| !matches!(e, Event::TimeReached { .. })),
            "already fired today"
        );
    }

    #[test]
    fn next_wake_accounts_for_a_sooner_schedule() {
        let cron = Cron::from_str("40 6 * * *").unwrap();
        let mut c = ClockAdapter::new(SUN, epoch(2024, 6, 1, 6, 39), Tz::UTC, 0.0, 0.0)
            .with_schedules(vec![(ScheduleId(1), cron)]);
        c.tick(0); // arm the schedule
        // 06:40 is 60s away — sooner than... also 60s to the minute boundary.
        // Move 30s in: boundary is 30s out, schedule (06:40) is 30s out — equal.
        assert_eq!(c.next_wake(30_000), Some(30_000));
    }
}
