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
//! Unlike `matter_device`, this adapter is **not** in `adapters::PLUGINS`. It is
//! built from the `system.rest_api` stanza, exactly as [`ClockAdapter`] is built
//! from `system` values. That is also the right semantic call: `matter_device` is
//! a projection of a device *subset* and earns its `expose:` spec, whereas the
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
use crate::ids::{DeviceId, RuleId, SceneId};
use crate::model::{CapabilityKind, CapabilityState, Command, Desired, Event, Millis};
use crate::observe::Observer;
use crate::rule::Truth;
use crate::wake::Waker;

use super::{Adapter, DispatchOutcome};

pub mod http;
pub mod json;
pub mod routes;

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
    Device { device: DeviceId, desired: Desired },
    Scene { scene: SceneId },
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
}

impl Observer for RestApiObserver {
    /// Stamp the rule's outcome with the `now` recorded on the most recent
    /// `tick`. This is *exact*, not approximate: [`Engine::advance`] sets
    /// `self.now`, then ticks adapters (which records it), then drains — so the
    /// `now` we hold is the same `now` under which these rules are considered.
    ///
    /// [`Engine::advance`]: crate::engine::Engine::advance
    fn rule_considered(&mut self, rule: RuleId, truth: Truth, fired: bool) {
        let mut mirror = lock(&self.shared);
        let now = mirror.now;
        let entry = mirror.rules.entry(rule).or_default();
        entry.last_considered_ms = Some(now);
        entry.last_truth = Some(truth);
        if fired {
            entry.last_fired_ms = Some(now);
            entry.fire_count += 1;
        }
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
    /// True for the synthetic clock device, which has no adapter or address.
    pub synthetic: bool,
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
    pub rules: Vec<(RuleId, String)>,
    device_by_name: HashMap<String, DeviceId>,
    scene_by_name: HashMap<String, SceneId>,
}

/// One scene as the API presents it. `commands` is the member count, which is
/// all `GET /scenes` reports.
#[derive(Clone, Debug)]
pub struct SceneEntry {
    pub id: SceneId,
    pub name: String,
    pub commands: usize,
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
            synthetic: true,
        });

        let scenes: Vec<SceneEntry> = cfg
            .scenes
            .iter()
            .map(|s| SceneEntry {
                id: s.id,
                name: s.name.clone(),
                commands: s.commands.len(),
            })
            .collect();

        let device_by_name = devices.iter().map(|d| (d.name.clone(), d.id)).collect();
        let scene_by_name = scenes.iter().map(|s| (s.name.clone(), s.id)).collect();

        Directory {
            system_name: cfg.system.name.clone(),
            timezone: cfg.system.timezone.clone(),
            devices,
            scenes,
            rules: cfg.rules.iter().map(|r| (r.id, r.name.clone())).collect(),
            device_by_name,
            scene_by_name,
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
}
