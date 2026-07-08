//! Adapters: the only place protocols (or time) exist.
//!
//! An adapter translates `Command`s outward and produces `Event`s inward. The
//! scheduler and the clock are *just adapters* — the design's central bet. For
//! the skeleton, adapters return the events they produce rather than holding a
//! bus reference, which keeps ownership simple and dispatch deterministic.
//!
//! `mod.rs` holds only the contract every adapter implements ([`Adapter`] +
//! [`DispatchOutcome`]); each adapter lives in its own file in this directory:
//!
//! | File | Adapter |
//! |---|---|
//! | `mock.rs` | [`MockDeviceAdapter`] — echoes commanded state (tests/bring-up) |
//! | `scheduler.rs` | [`SchedulerAdapter`] — timers; "time is an adapter" |
//! | `clock.rs` | [`ClockAdapter`] — synthetic time-of-day / sun device |
//! | `zigbee2mqtt.rs` | [`Zigbee2MqttAdapter`] — zigbee2mqtt over MQTT |
//! | `matter.rs` | [`MatterAdapter`] — Matter via `python-matter-server` |
//! | `zwavejs.rs` | [`ZwaveAdapter`] — Z-Wave via `zwave-js-server` |
//!
//! **To add a protocol adapter** (Z-Wave, Matter, ESPHome, …): add a module
//! here, implement [`Adapter`] for its runtime type and [`AdapterPlugin`] for a
//! zero-sized `PLUGIN` marker (config validation + construction — see any
//! existing adapter), then add one line to [`plugins`]. The compiler discovers
//! it through the registry, so nothing in `compile/` changes.

use crate::model::{Command, Event, Millis};

mod clock;
pub mod matter;
mod mock;
mod plugin;
mod scheduler;
pub mod zigbee2mqtt;
pub mod zwavejs;

pub use clock::ClockAdapter;
pub use matter::{AttrReport, ClusterCommand, EndpointId, MatterAdapter, MatterController, NodeId};
pub use mock::MockDeviceAdapter;
pub use plugin::{config_of, AdapterPlugin};
pub use scheduler::SchedulerAdapter;
pub use zigbee2mqtt::{MqttMessage, MqttTransport, Zigbee2MqttAdapter};
pub use zwavejs::{DeviceKind, SetValue, ValueUpdate, ZwaveAdapter, ZwaveClient};

/// Every protocol adapter the compiler knows about, in one place. This is the
/// **only** line a new adapter adds outside its own file: append its `PLUGIN`.
/// The resolver looks a config `type` up here, and the engine builder builds
/// through the trait, so neither has a per-adapter branch anymore.
static PLUGINS: &[&dyn AdapterPlugin] = &[
    &zigbee2mqtt::PLUGIN,
    &matter::PLUGIN,
    &zwavejs::PLUGIN,
    &mock::PLUGIN,
];

/// The registered adapters (see [`PLUGINS`]).
pub fn plugins() -> &'static [&'static dyn AdapterPlugin] {
    PLUGINS
}

/// The plugin whose [`type_tag`](AdapterPlugin::type_tag) matches a config
/// `type`, or `None` if no adapter claims it (an unknown-type error).
pub fn plugin_for(type_tag: &str) -> Option<&'static dyn AdapterPlugin> {
    PLUGINS.iter().copied().find(|p| p.type_tag() == type_tag)
}

/// Scale a 0..=100 percentage to the 0..=254 "level" that both zigbee2mqtt
/// brightness and Matter LevelControl use (identical ranges). Rounds to nearest
/// and clamps the input to 100. Shared so both adapters agree on the conversion.
pub(crate) fn pct_to_level(pct: u8) -> u64 {
    (pct.min(100) as u64 * 254 + 50) / 100
}

/// Inverse of [`pct_to_level`]: a 0..=254 level back to a 0..=100 percentage.
pub(crate) fn level_to_pct(raw: u64) -> u8 {
    ((raw.min(254) * 100 + 127) / 254) as u8
}

/// The result of attempting to dispatch one command. Splitting failure into
/// transient vs. permanent is what lets the engine retry a network blip but give
/// up immediately on a misconfiguration.
#[derive(Clone, Debug)]
pub enum DispatchOutcome {
    /// Applied; here are any events produced synchronously (e.g. a state echo).
    Ok(Vec<Event>),
    /// Failed, but retrying may help (radio busy, momentary network loss).
    Transient(String),
    /// Failed in a way retrying cannot fix (unsupported command, no route).
    Permanent(String),
}

impl DispatchOutcome {
    /// Convenience for "succeeded, produced no events."
    pub fn ok() -> Self {
        DispatchOutcome::Ok(Vec::new())
    }
}

pub trait Adapter {
    /// Translate a command into protocol action. Real network adapters will
    /// confirm asynchronously; the skeleton resolves synchronously, same shape.
    fn dispatch(&mut self, cmd: &Command, now: Millis) -> DispatchOutcome;

    /// Called when virtual time advances (and once at boot). The scheduler fires
    /// due timers here; the clock publishes the current time/sun state. Ticks
    /// cannot fail, so this returns events directly.
    fn tick(&mut self, now: Millis) -> Vec<Event> {
        let _ = now;
        Vec::new()
    }

    /// When, in ms from `now`, this adapter will next need a `tick` for reasons of
    /// its own — the scheduler's soonest due timer, the clock's next minute. The
    /// host takes the minimum across adapters and sleeps exactly that long (or
    /// until a `Waker` fires), instead of polling on a fixed interval.
    ///
    /// `None` (the default) means "I have no scheduled work; only external I/O
    /// will produce events from me" — true of device/protocol adapters, whose
    /// inbound path is driven by a background thread and a `Waker`, not by ticks.
    fn next_wake(&self, now: Millis) -> Option<Millis> {
        let _ = now;
        None
    }
}
