//! The REST API: a read/write HTTP surface over the running engine.
//!
//! ```text
//!   HTTP threads                              engine thread (the run loop)
//!   ────────────                              ────────────────────────────
//!   GET  ── read ──▶ Arc<Mutex<Mirror>>  ◀── write ── state_folded / rule_considered
//!   POST ── send ──▶ mpsc::channel       ──  drain ──▶ tick() → RequestedChange
//!                    + Waker::wake()                            RequestedScene
//! ```
//!
//! # Why the API cannot hold the engine
//!
//! The obvious implementation — hand the HTTP server an `Arc<Engine>` — is
//! impossible, and that constraint shapes everything here. [`Engine::advance`]
//! and [`Engine::inject`] take `&mut self`, and the host's run loop calls
//! `advance` every iteration, while an HTTP server needs its own thread (it
//! blocks on `accept`). So:
//!
//! 1. **`Arc<T>` only ever yields `&T`.** `Arc::get_mut` returns `None` the
//!    moment a second clone exists, which is exactly this situation.
//! 2. **`Engine` is neither `Send` nor `Sync`.** It holds `Vec<Box<dyn Adapter>>`,
//!    `Vec<Box<dyn Observer>>` and `Vec<Box<dyn NorthboundAdapter>>`; none of
//!    those trait objects carry auto-trait bounds, so the engine cannot cross a
//!    thread boundary at all. That is deliberate — a real transport keeps its own
//!    runtime thread internally, behind its own seam.
//! 3. **`Arc<Mutex<Engine>>` would be wrong even if it compiled.** It would hold a
//!    lock across [`Engine::drain`], which runs the queue to quiescence and
//!    dispatches into adapters doing real network I/O. A `GET` would block behind
//!    an MQTT round-trip, a slow client could stall the run loop, and two threads
//!    injecting events would destroy the determinism the architecture rests on.
//!
//! **Therefore the REST API is a northbound adapter.** The engine owns it; the
//! HTTP threads talk to it over a shared state [`Mirror`] (for reads) and an
//! `mpsc` channel plus a [`Waker`] (for writes) — the same shape the Matter
//! node's background thread already uses.
//!
//! # Why it is configured from `system`, not the plugin registry
//!
//! Unlike the protocol adapters, this adapter is **not** in `adapters::PLUGINS`.
//! It is built from the `system.rest_api` stanza, exactly as [`ClockAdapter`] is
//! built from `system` values. That is also the right semantic call: an
//! `expose:`-style adapter is a projection of a device *subset*, whereas the
//! REST API is an instance-level control surface that lists everything and
//! activates scenes — something `expose:` does not model.
//!
//! [`Engine::advance`]: crate::engine::Engine::advance
//! [`Engine::inject`]: crate::engine::Engine::inject
//! [`Engine::drain`]: crate::engine::Engine
//! [`ClockAdapter`]: crate::adapters::ClockAdapter

use std::collections::HashMap;
use std::sync::mpsc::{channel as mpsc_channel, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::compile::resolve::CompiledConfig;
use crate::ids::{ActionId, DeviceId, RuleId, SceneId, ScheduleId};
use crate::model::{CapabilityKind, CapabilityState, Command, Desired, Event, Millis};
use crate::observe::Observer;
use crate::rule::{Rule, Truth};
use crate::wake::Waker;

use super::{Adapter, DispatchOutcome};

pub mod describe;
pub mod http;
pub mod json;
pub mod routes;
pub mod stream;

pub use http::RestApiServer;
pub use routes::{handle, Response};

/// What the engine thread publishes for HTTP threads to read. Never a source of
/// truth — a projection of the store, exactly like a Matter attribute cell.
#[derive(Default)]
pub struct Mirror {
    states: HashMap<(DeviceId, CapabilityKind), CapabilityState>,
    rules: HashMap<RuleId, RuleStatus>,
    /// The engine's virtual `now`, recorded on each `tick`.
    now: Millis,
}

/// What `GET /rules` reports for one rule. `last_truth` is the single most useful
/// signal when debugging a rule that will not fire.
///
/// Note the semantics precisely: [`Observer::rule_considered`] fires only when
/// the rule's **trigger matched**, so `last_considered_ms` means "the last time
/// this rule's trigger matched an event", not "the last time the engine looked
/// at it".
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RuleStatus {
    pub last_considered_ms: Option<Millis>,
    pub last_truth: Option<Truth>,
    pub last_fired_ms: Option<Millis>,
    pub fire_count: u64,
}

/// A consumer request crossing from an HTTP thread into the engine.
#[derive(Clone, Debug, PartialEq)]
pub enum Inbound {
    Device {
        device: DeviceId,
        desired: Desired,
    },
    Scene {
        scene: SceneId,
    },
    /// Fire one of a device's declared events — indistinguishable, once it
    /// reaches the queue, from the physical button press that normally produces
    /// it. This is how the API reaches *rules*: rather than exposing a
    /// rule-trigger endpoint (which would be ambiguous about the `if:` clause
    /// for rules disambiguated solely by their conditions), a consumer fires the
    /// event and the engine selects the matching rule exactly as it would for
    /// the real button.
    Action {
        device: DeviceId,
        action: ActionId,
    },
}

/// Take a mutex guard, recovering from poisoning rather than propagating a panic
/// from one connection thread into every other one. A poisoned `Mirror` is still
/// perfectly readable: it is a projection, and the worst case is one stale entry.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// --- the adapter -------------------------------------------------------------

/// The engine-side half: an `Adapter + Observer`, and so a
/// [`NorthboundAdapter`](super::NorthboundAdapter) via the blanket impl — no new
/// trait, no registry entry.
pub struct RestApiAdapter {
    shared: Arc<Mutex<Mirror>>,
    inbound: Receiver<Inbound>,
}

impl Observer for RestApiAdapter {
    fn state_folded(&mut self, device: DeviceId, state: &CapabilityState) {
        lock(&self.shared)
            .states
            .insert((device, state.kind()), state.clone());
    }

    // NOTE: `rule_considered` is deliberately *not* implemented here — it would
    // never be called. The engine fans only `state_folded` to its `northbound`
    // list (`Engine::fan_state_folded`); every other `Observer` callback goes to
    // the `observers` list alone. Rule status is collected by the sibling
    // [`RestApiObserver`], which the host registers with `add_observer`.
}

/// The observer half, registered separately with
/// [`Engine::add_observer`](crate::engine::Engine::add_observer).
///
/// It exists because a `NorthboundAdapter` does *not* receive the full `Observer`
/// surface: the engine fans only `state_folded` to its northbound list, so an
/// adapter that implemented `rule_considered` would simply never hear it. This
/// type joins the ordinary observer list to pick that up, writing into the very
/// same [`Mirror`] behind the shared `Arc<Mutex<_>>` — so `GET /devices` and
/// `GET /rules` read one consistent projection regardless of which registration
/// delivered the update.
pub struct RestApiObserver {
    shared: Arc<Mutex<Mirror>>,
    /// The SSE fan-out. Also the reason this type resolves names: a frame carries
    /// `"device": "kitchen_lamp"`, not `DeviceId(2)`.
    ///
    /// `None` until [`RestApiObserver::with_stream`] is called, so a host that
    /// does not serve the stream pays nothing — and, importantly, so the
    /// `Directory` (built from the config after the channel is constructed) can
    /// be attached later without reordering the host's startup sequence.
    stream: Option<(stream::Broadcaster, Arc<Directory>)>,
}

impl RestApiObserver {
    /// Attach the SSE fan-out. Called by the host once the `Directory` exists.
    /// Without this the observer still maintains the mirror for `GET /rules`;
    /// it simply broadcasts nothing.
    pub fn with_stream(&mut self, broadcaster: stream::Broadcaster, directory: Arc<Directory>) {
        self.stream = Some((broadcaster, directory));
    }

    /// Render and queue one frame, if the stream is attached.
    ///
    /// Runs on the **engine thread**: the closure does plain JSON assembly and
    /// the broadcast is a mutex push. Nothing here may panic or block on I/O.
    fn emit(&self, frame: impl FnOnce(&Directory) -> stream::Frame) {
        if let Some((broadcaster, directory)) = &self.stream {
            broadcaster.broadcast(frame(directory));
        }
    }
}

impl Observer for RestApiObserver {
    /// Stamp the rule's outcome with the `now` recorded on the most recent
    /// `tick`. This is *exact*, not approximate: [`Engine::advance`] sets
    /// `self.now`, then ticks adapters (which records it), then drains — so the
    /// `now` we hold is the same `now` under which these rules are considered.
    ///
    /// [`Engine::advance`]: crate::engine::Engine::advance
    fn rule_considered(&mut self, rule: RuleId, truth: Truth, fired: bool) {
        {
            let mut mirror = lock(&self.shared);
            let now = mirror.now;
            let entry = mirror.rules.entry(rule).or_default();
            entry.last_considered_ms = Some(now);
            entry.last_truth = Some(truth);
            if fired {
                entry.last_fired_ms = Some(now);
                // Saturating: a `u64` fire count cannot realistically overflow,
                // but this path runs on the engine thread and must not panic
                // even in a debug build.
                entry.fire_count = entry.fire_count.saturating_add(1);
            }
        }
        self.emit(|directory| {
            stream::rule_frame(&directory.rule_name(rule), truth_name(truth), fired)
        });
    }

    /// The stream's `state` events. This mirrors what [`RestApiAdapter`] does for
    /// the read endpoints, but is fanned to the *observer* list — both see every
    /// fold, and both write a projection, so there is no divergence.
    fn state_folded(&mut self, device: DeviceId, state: &CapabilityState) {
        self.emit(|directory| {
            stream::state_frame(
                &directory.device_name(device),
                state.kind().name(),
                json::state_to_json(state),
            )
        });
    }

    /// The stream's `action` events, filtered out of the full event feed.
    ///
    /// `depth` distinguishes a chain-starting event from a cascade-produced one.
    /// It deliberately does *not* distinguish a physical press from an
    /// API-injected one: both arrive at depth 0, which is the whole point of the
    /// events surface.
    fn event_received(&mut self, event: &Event, depth: u32) {
        if let Event::Action { device, action } = event {
            let (device, action) = (*device, *action);
            self.emit(|directory| {
                stream::action_frame(
                    &directory.device_name(device),
                    &directory.action_name(device, action),
                    depth,
                )
            });
        }
    }

    /// The stream's `command_failed` events — a device that went offline or
    /// rejected a command, which the state stream alone would show only as
    /// "nothing happened".
    fn command_failed(&mut self, command: &Command, reason: &str, attempts: u32) {
        self.emit(|directory| {
            stream::command_failed_frame(describe::command(directory, command), reason, attempts)
        });
    }
}

/// The wire spelling of the three-valued `Truth`. Shared by `GET /rules` and the
/// stream, so the two can never disagree.
pub fn truth_name(truth: Truth) -> &'static str {
    match truth {
        Truth::True => "true",
        Truth::False => "false",
        Truth::Unknown => "unknown",
    }
}

impl Adapter for RestApiAdapter {
    /// A northbound adapter binds no devices, so no command is ever routed here.
    fn dispatch(&mut self, _cmd: &Command, _now: Millis) -> DispatchOutcome {
        DispatchOutcome::Permanent("rest_api is a northbound adapter, not a dispatch target".into())
    }

    /// Record `now` for rule stamping, then drain everything the HTTP threads
    /// have queued into inbound intents — the same pull-after-`Waker` path a
    /// Matter controller's attribute write takes.
    fn tick(&mut self, now: Millis) -> Vec<Event> {
        lock(&self.shared).now = now;
        self.inbound
            .try_iter()
            .map(|inbound| match inbound {
                Inbound::Device { device, desired } => Event::RequestedChange { device, desired },
                Inbound::Scene { scene } => Event::RequestedScene { scene },
                // Enters the queue at depth 0 like any adapter-produced event, so
                // cascade limits, `for:` timers and retries all apply unchanged —
                // an API client cannot bypass the engine's backstops.
                Inbound::Action { device, action } => Event::Action { device, action },
            })
            .collect()
    }

    // `next_wake` keeps its default `None`: this adapter has no scheduled work of
    // its own. Inbound requests arrive with a `Waker` nudge instead.
}

// --- the handle --------------------------------------------------------------

/// What the HTTP threads hold: a cloneable, `Send` view of the mirror plus the
/// channel into the engine. Deliberately exposes no way to reach the `Engine`.
///
/// **Lock discipline:** every method takes the mutex, copies what it needs, and
/// drops the guard before returning. A guard is never held across socket I/O.
#[derive(Clone)]
pub struct RestApiHandle {
    shared: Arc<Mutex<Mirror>>,
    outbound: Sender<Inbound>,
    waker: Option<Waker>,
}

impl RestApiHandle {
    /// The mirrored value of one capability, or `None` if the engine has never
    /// folded it — the explicit "we have never heard about this" that the API
    /// renders as JSON `null`.
    pub fn state(&self, device: DeviceId, kind: CapabilityKind) -> Option<CapabilityState> {
        lock(&self.shared).states.get(&(device, kind)).cloned()
    }

    /// The recorded status of one rule. A rule never yet considered reports the
    /// default: `None` timestamps and `fire_count: 0`.
    pub fn rule_status(&self, rule: RuleId) -> RuleStatus {
        lock(&self.shared)
            .rules
            .get(&rule)
            .copied()
            .unwrap_or_default()
    }

    /// The engine's virtual time as of its most recent tick. Virtual ms since
    /// boot — *not* a Unix timestamp.
    pub fn engine_now(&self) -> Millis {
        lock(&self.shared).now
    }

    /// Queue a request and nudge the run loop so it drains promptly instead of
    /// waiting out its sleep. Returns `false` if the engine is gone (the receiver
    /// was dropped), which is how a request during shutdown is reported.
    pub fn request(&self, inbound: Inbound) -> bool {
        if self.outbound.send(inbound).is_err() {
            return false;
        }
        if let Some(waker) = &self.waker {
            waker.wake();
        }
        true
    }
}

/// Construct the linked trio, all sharing one [`Mirror`]:
///
/// * the **adapter** goes to the engine via
///   [`add_northbound`](crate::engine::Engine::add_northbound) — it ticks
///   (draining requests) and receives `state_folded`;
/// * the **observer** goes to the engine via
///   [`add_observer`](crate::engine::Engine::add_observer) — it receives
///   `rule_considered`, which the northbound list never sees;
/// * the **handle** goes to the HTTP server — and, in tests, straight to the
///   assertions.
///
/// Both registrations are needed for a complete `GET /rules`; registering only
/// the adapter yields device state but leaves every rule reporting `fire_count:
/// 0`. Cheap enough to call unconditionally: when the API is disabled in config,
/// nothing here allocates a thread or a socket.
pub fn channel(waker: Option<Waker>) -> (RestApiAdapter, RestApiObserver, RestApiHandle) {
    let shared = Arc::new(Mutex::new(Mirror::default()));
    let (outbound, inbound) = mpsc_channel();
    (
        RestApiAdapter {
            shared: Arc::clone(&shared),
            inbound,
        },
        RestApiObserver {
            shared: Arc::clone(&shared),
            stream: None,
        },
        RestApiHandle {
            shared,
            outbound,
            waker,
        },
    )
}

// --- the directory -----------------------------------------------------------

/// One device as the API presents it.
#[derive(Clone, Debug)]
pub struct DeviceEntry {
    pub id: DeviceId,
    pub name: String,
    pub room: Option<String>,
    pub capabilities: Vec<CapabilityKind>,
    /// Declared stateless events, in config order: the local name paired with its
    /// interned identity. The `raw:` protocol string is deliberately **not**
    /// carried — it is an implementation detail of the southbound adapter and
    /// must not leak onto the wire.
    ///
    /// A device may declare events and no capabilities at all; that is the
    /// supported idiom for an API-driven automation surface (a `virtual` device
    /// whose events exist only to be POSTed), so nothing here may assume a
    /// non-empty `capabilities`.
    pub events: Vec<(String, ActionId)>,
    /// True for the synthetic clock device, which has no adapter or address.
    pub synthetic: bool,
}

impl DeviceEntry {
    /// The interned identity of a declared event, by local name.
    pub fn action(&self, name: &str) -> Option<ActionId> {
        self.events
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, id)| *id)
    }
}

/// The name↔id translation table, built once at startup from [`CompiledConfig`].
///
/// The runtime speaks only interned ids; the API speaks names. This is immutable
/// after construction, so it is wrapped in an `Arc` and cloned per connection
/// thread with no lock.
pub struct Directory {
    pub system_name: Option<String>,
    pub timezone: String,
    /// In config order.
    pub devices: Vec<DeviceEntry>,
    pub scenes: Vec<SceneEntry>,
    /// The full compiled rules, not merely their names: `GET /rules` renders each
    /// rule's trigger, condition and commands so a client can build UI from the
    /// config rather than hardcoding it.
    pub rules: Vec<Rule>,
    /// Wall-clock schedule names, needed to render `Trigger::Time` back to the
    /// name it was written under. (`TimerKey` needs no such table: it is a plain
    /// `String` and so is already self-describing.)
    pub schedules: Vec<(ScheduleId, String)>,
    device_by_name: HashMap<String, DeviceId>,
    scene_by_name: HashMap<String, SceneId>,
    rule_by_name: HashMap<String, RuleId>,
}

/// One scene as the API presents it.
#[derive(Clone, Debug)]
pub struct SceneEntry {
    pub id: SceneId,
    pub name: String,
    /// The scene's member commands, rendered in full by `GET /scenes`.
    pub commands: Vec<Command>,
}

/// The name the synthetic clock device is exposed under. It is not in
/// `cfg.devices`; `main.rs` already special-cases it the same way when naming the
/// observer's tables.
pub const CLOCK_DEVICE_NAME: &str = "clock";

impl Directory {
    pub fn from_config(cfg: &CompiledConfig) -> Self {
        let mut devices: Vec<DeviceEntry> = cfg
            .devices
            .iter()
            .map(|d| DeviceEntry {
                id: d.id,
                name: d.name.clone(),
                room: d.metadata.room.clone(),
                capabilities: d.capabilities.clone(),
                events: d.events.iter().map(|e| (e.name.clone(), e.id)).collect(),
                synthetic: false,
            })
            .collect();

        // The synthetic clock device backs `sun_up` / `time_of_day` conditions and
        // is a real, readable device from the API's point of view — it just has no
        // adapter or address behind it.
        devices.push(DeviceEntry {
            id: cfg.clock_device(),
            name: CLOCK_DEVICE_NAME.to_string(),
            room: None,
            capabilities: vec![CapabilityKind::TimeOfDay, CapabilityKind::SunUp],
            events: Vec::new(),
            synthetic: true,
        });

        let scenes: Vec<SceneEntry> = cfg
            .scenes
            .iter()
            .map(|s| SceneEntry {
                id: s.id,
                name: s.name.clone(),
                commands: s.commands.clone(),
            })
            .collect();

        let device_by_name = devices.iter().map(|d| (d.name.clone(), d.id)).collect();
        let scene_by_name = scenes.iter().map(|s| (s.name.clone(), s.id)).collect();
        let rule_by_name = cfg.rules.iter().map(|r| (r.name.clone(), r.id)).collect();

        Directory {
            system_name: cfg.system.name.clone(),
            timezone: cfg.system.timezone.clone(),
            devices,
            scenes,
            rules: cfg.rules.clone(),
            schedules: cfg
                .schedules
                .iter()
                .map(|s| (s.id, s.name.clone()))
                .collect(),
            device_by_name,
            scene_by_name,
            rule_by_name,
        }
    }

    pub fn device(&self, name: &str) -> Option<&DeviceEntry> {
        let id = self.device_by_name.get(name)?;
        self.devices.iter().find(|d| d.id == *id)
    }

    pub fn scene(&self, name: &str) -> Option<&SceneEntry> {
        let id = self.scene_by_name.get(name)?;
        self.scenes.iter().find(|s| s.id == *id)
    }

    pub fn rule(&self, name: &str) -> Option<&Rule> {
        let id = self.rule_by_name.get(name)?;
        self.rules.iter().find(|r| r.id == *id)
    }

    /// The config name of a device, for rendering an id back onto the wire.
    /// Falls back to a synthetic `device#N` rather than failing: a describe path
    /// must never panic (it runs on the engine thread when the stream renders).
    pub fn device_name(&self, id: DeviceId) -> String {
        self.devices
            .iter()
            .find(|d| d.id == id)
            .map(|d| d.name.clone())
            .unwrap_or_else(|| format!("device#{}", id.0))
    }

    pub fn scene_name(&self, id: SceneId) -> String {
        self.scenes
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.name.clone())
            .unwrap_or_else(|| format!("scene#{}", id.0))
    }

    pub fn rule_name(&self, id: RuleId) -> String {
        self.rules
            .iter()
            .find(|r| r.id == id)
            .map(|r| r.name.clone())
            .unwrap_or_else(|| format!("rule#{}", id.0))
    }

    pub fn schedule_name(&self, id: ScheduleId) -> String {
        self.schedules
            .iter()
            .find(|(sid, _)| *sid == id)
            .map(|(_, name)| name.clone())
            .unwrap_or_else(|| format!("schedule#{}", id.0))
    }

    /// The local name of a device's declared event, for rendering
    /// `Trigger::Action` and streamed actions back onto the wire.
    pub fn action_name(&self, device: DeviceId, action: ActionId) -> String {
        self.devices
            .iter()
            .find(|d| d.id == device)
            .and_then(|d| {
                d.events
                    .iter()
                    .find(|(_, id)| *id == action)
                    .map(|(name, _)| name.clone())
            })
            .unwrap_or_else(|| format!("action#{}", action.0))
    }
}
