//! The canonical vocabulary: capabilities, events, and commands.
//!
//! This is the highest-risk, least-reversible part of the design — everything
//! else is shaped around these enums. Adapters translate protocol messages into
//! `Event`s and translate `Command`s back into protocol messages; rules only
//! ever speak this vocabulary.

use crate::ids::{ActionId, DeviceId, SceneId, ScheduleId};

/// Logical milliseconds. In tests this is virtual time advanced by hand; in
/// production it is fed by the clock adapter. Rules and adapters must never call
/// the wall clock directly — that is what keeps replay deterministic.
pub type Millis = u64;

/// The *kind* of a capability, used as a key into the state store.
///
/// Note `TimeOfDay` / `SunUp`: time is modeled as a synthetic device with
/// ordinary capabilities, so a condition that reads "is it dark?" looks exactly
/// like one that reads "is the switch on?".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CapabilityKind {
    Switch,
    Brightness,
    ColorTemperature,
    Occupancy,
    Battery,
    TimeOfDay,
    SunUp,
}

/// State lives on capabilities, not devices — this avoids one giant
/// "device state" blob and lets a device be an arbitrary bag of capabilities.
#[derive(Clone, Debug, PartialEq)]
pub enum CapabilityState {
    Switch(bool),
    Brightness(u8),        // 0..=100
    ColorTemperature(u16), // mireds
    Occupancy(bool),
    Battery(u8),
    TimeOfDay(u16), // minutes since local midnight, 0..1440
    SunUp(bool),    // false = after sunset / before sunrise (real solar ephemeris)
}

impl CapabilityState {
    pub fn kind(&self) -> CapabilityKind {
        match self {
            CapabilityState::Switch(_) => CapabilityKind::Switch,
            CapabilityState::Brightness(_) => CapabilityKind::Brightness,
            CapabilityState::ColorTemperature(_) => CapabilityKind::ColorTemperature,
            CapabilityState::Occupancy(_) => CapabilityKind::Occupancy,
            CapabilityState::Battery(_) => CapabilityKind::Battery,
            CapabilityState::TimeOfDay(_) => CapabilityKind::TimeOfDay,
            CapabilityState::SunUp(_) => CapabilityKind::SunUp,
        }
    }

    /// Extract a boolean view, if this capability is boolean-shaped. Lets the
    /// condition evaluator treat switch / occupancy / sun-up uniformly.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            CapabilityState::Switch(b)
            | CapabilityState::Occupancy(b)
            | CapabilityState::SunUp(b) => Some(*b),
            _ => None,
        }
    }

    /// Extract a numeric view, if this capability is scalar-shaped. Lets the
    /// condition evaluator compare brightness / battery / time-of-day uniformly.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            CapabilityState::Brightness(v) | CapabilityState::Battery(v) => Some(*v as i64),
            CapabilityState::ColorTemperature(v) | CapabilityState::TimeOfDay(v) => Some(*v as i64),
            _ => None,
        }
    }
}

/// A timer's identity. Named so that one rule can cancel a timer another rule
/// scheduled, and so the compiler can lint that every cancel matches a schedule.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TimerKey(pub String);

impl TimerKey {
    pub fn new(s: impl Into<String>) -> Self {
        TimerKey(s.into())
    }
}

/// Canonical, protocol-independent events. Everything that can wake the engine
/// — a button, a sensor, *or the passage of time* — arrives in this one shape.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// A stateless device event fired — a button press, knob turn, scene button.
    /// `action` is the interned identity of one of the device's declared
    /// `events`; the adapter resolved it from the raw protocol string.
    Action {
        device: DeviceId,
        action: ActionId,
    },
    OccupancyChanged {
        device: DeviceId,
        occupied: bool,
    },
    /// A device reporting its own state back (the feedback path). Updates the
    /// state store; rules may also trigger on it. The clock adapter uses this
    /// same event to publish time-of-day and sun state.
    StateReported {
        device: DeviceId,
        state: CapabilityState,
    },
    /// Emitted by the scheduler when a wall-clock schedule comes due.
    TimeReached {
        schedule: ScheduleId,
    },
    /// Emitted by the scheduler when a relative timer elapses.
    TimerElapsed {
        key: TimerKey,
    },
    /// A command that could not be dispatched — emitted after retries are
    /// exhausted, or immediately for a permanent failure. It rides the same bus
    /// so rules can react (notify, fall back); it is also surfaced to the
    /// `Observer` for logging. Nothing matches it as a trigger yet.
    CommandFailed {
        command: Command,
        reason: String,
        attempts: u32,
    },
}

/// Canonical, protocol-independent commands. Note `ScheduleTimer`/`CancelTimer`:
/// relative timing is expressed as a command to the scheduler adapter that
/// produces a future event — the same shape as everything else.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    SetSwitch {
        device: DeviceId,
        on: bool,
    },
    /// Flip a switch. The engine resolves this to an explicit `SetSwitch` against
    /// the known state before dispatch (so adapters send well-supported On/Off,
    /// not the flaky protocol `Toggle`); it only reaches an adapter as a raw
    /// toggle when the state is `Unknown`. See `Engine::resolve_toggle`.
    ToggleSwitch {
        device: DeviceId,
    },
    /// `transition` lets fades push down to the adapter (pass-through or
    /// emulated) instead of forcing a rule to sequence many steps.
    SetBrightness {
        device: DeviceId,
        value: u8,
        transition: Option<Millis>,
    },
    DecreaseBrightness {
        device: DeviceId,
        value: u8,
    },
    IncreaseBrightness {
        device: DeviceId,
        value: u8,
    },
    ActivateScene {
        scene: SceneId,
    },
    ScheduleTimer {
        key: TimerKey,
        after: Millis,
    },
    CancelTimer {
        key: TimerKey,
    },
}

impl Command {
    /// The device a command targets, if any. Scheduler/scene commands return
    /// `None` because they are routed specially rather than to a device adapter.
    pub fn target_device(&self) -> Option<DeviceId> {
        match self {
            Command::SetSwitch { device, .. } => Some(*device),
            Command::ToggleSwitch { device, .. } => Some(*device),
            Command::SetBrightness { device, .. } => Some(*device),
            Command::DecreaseBrightness { device, .. } => Some(*device),
            Command::IncreaseBrightness { device, .. } => Some(*device),
            Command::ActivateScene { .. }
            | Command::ScheduleTimer { .. }
            | Command::CancelTimer { .. } => None,
        }
    }
}
