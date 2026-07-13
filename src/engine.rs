//! The runtime: a single-threaded, ordered event loop.
//!
//! ```text
//! inject event -> queue (depth 0)
//!   while queue non-empty:
//!     pop (event, depth)
//!     if depth > max_cascade_depth -> drop + notify Observer, continue  (backstop)
//!     if it is an internal retry timer -> re-dispatch that command, continue
//!     fold device/state feedback into the state store
//!     for each rule: trigger.matches() && condition.eval().is_true() -> commands
//!     dispatch commands (device adapters, scheduler, or scene expansion)
//!       Ok        -> produced events re-enter at depth+1
//!       Transient -> schedule a retry (a future timer), or give up if exhausted
//!       Permanent -> give up immediately
//!       give up   -> notify Observer and emit a CommandFailed event (depth+1)
//! ```
//!
//! **Causal depth** is how the loop fails safe against feedback cascades: each
//! event carries the number of dispatch hops that produced it. Externally
//! injected events and timer fires are depth 0; anything a command produces is
//! one deeper. A genuine loop (e.g. a rule that reacts to `CommandFailed` by
//! re-issuing a command that fails again) is bounded instead of hanging the
//! single-threaded loop. Detecting such cycles *statically* is the compiler's
//! job; this is the runtime backstop for the cases static analysis cannot see
//! (state-dependent cycles, misbehaving physical devices).

use std::collections::{HashMap, VecDeque};

use crate::adapters::{Adapter, DispatchOutcome, SchedulerAdapter};
use crate::ids::{AdapterIdx, DeviceId, RuleId, SceneId};
use crate::model::{CapabilityState, Command, Event, Millis, TimerKey};
use crate::observe::Observer;
use crate::rule::Rule;
use crate::state::StateStore;
use crate::CapabilityKind;

const SCHEDULER_IDX: AdapterIdx = 0;
const RETRY_KEY_PREFIX: &str = "__retry:";
/// Key namespace for engine-managed `for:` (sustained-edge) timers. Like
/// `__retry:` keys, these are intercepted in `drain` before rule matching so
/// they re-verify + fire rather than acting as user-visible timer triggers.
const FOR_KEY_PREFIX: &str = "__for:";
const DEFAULT_MAX_CASCADE_DEPTH: u32 = 32;

/// How transient failures are retried. Backoff is exponential in the base delay.
#[derive(Clone, Copy, Debug)]
pub struct RetryPolicy {
    /// Total dispatch attempts before giving up (including the first).
    pub max_attempts: u32,
    /// Delay before the 2nd attempt; doubles each subsequent attempt.
    pub base_backoff: Millis,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            max_attempts: 3,
            base_backoff: 1000,
        }
    }
}

impl RetryPolicy {
    /// Backoff after the given (1-based) attempt has failed.
    fn backoff(&self, failed_attempt: u32) -> Millis {
        let shift = failed_attempt.min(16).saturating_sub(1);
        self.base_backoff.saturating_mul(1u64 << shift)
    }
}

struct PendingRetry {
    command: Command,
    attempt: u32,
}

/// A rule waiting out its `for:` duration. Keyed by `RuleId` in `pending_for`;
/// holds the generated timer key so a state revert can cancel it and an elapse
/// can identify which rule to re-verify and fire.
struct PendingFor {
    key: TimerKey,
}

/// Fan one observer notification out to every registered observer, in order.
/// A free function taking the slice directly (not `&mut self`) so call sites can
/// hold a disjoint immutable borrow of another field (`self.rules`, `self.state`)
/// across the notification — the borrow checker permits `&self.rules` +
/// `&mut self.observers` but not `&self.rules` + a `&mut self` method call.
fn notify(observers: &mut [Box<dyn Observer>], f: impl Fn(&mut dyn Observer)) {
    for obs in observers.iter_mut() {
        f(obs.as_mut());
    }
}

pub struct Engine {
    now: Millis,
    /// Each entry is `(event, causal_depth)`.
    queue: VecDeque<(Event, u32)>,
    state: StateStore,
    rules: Vec<Rule>,
    adapters: Vec<Box<dyn Adapter>>,
    /// Northbound adapters (`matter_device`, …). Held separately from `adapters` because
    /// they are driven on both paths: `tick`/`next_wake` like an adapter (to
    /// drain consumer input and schedule wakes) *and* `state_folded` like an
    /// observer (to mirror engine state outward). They bind no devices, so they
    /// are never a `dispatch` target and never appear in `device_to_adapter`.
    northbound: Vec<Box<dyn crate::adapters::NorthboundAdapter>>,
    device_to_adapter: HashMap<DeviceId, AdapterIdx>,
    scenes: HashMap<SceneId, Vec<Command>>,
    /// Every registered observer, notified in registration order. Multiple so a
    /// trace/logging observer (`StderrObserver`) can coexist with **northbound
    /// adapters**, which register here to receive `state_folded` — the engine's
    /// state fan-out seam (see `observe.rs`). Was a single `Box` when observation
    /// meant only tracing.
    observers: Vec<Box<dyn Observer>>,
    retry: RetryPolicy,
    retries: HashMap<TimerKey, PendingRetry>,
    retry_counter: u64,
    /// Live `for:` timers, keyed by rule so at most one is pending per rule (a
    /// fresh edge restarts the wait) and a state revert can find + cancel it.
    pending_for: HashMap<RuleId, PendingFor>,
    for_counter: u64,
    max_cascade_depth: u32,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    pub fn new() -> Self {
        // Adapter 0 is always the scheduler — "time is an adapter."
        let adapters: Vec<Box<dyn Adapter>> = vec![Box::new(SchedulerAdapter::default())];
        Engine {
            now: 0,
            queue: VecDeque::new(),
            state: StateStore::default(),
            rules: Vec::new(),
            adapters,
            northbound: Vec::new(),
            device_to_adapter: HashMap::new(),
            scenes: HashMap::new(),
            observers: Vec::new(),
            retry: RetryPolicy::default(),
            retries: HashMap::new(),
            retry_counter: 0,
            pending_for: HashMap::new(),
            for_counter: 0,
            max_cascade_depth: DEFAULT_MAX_CASCADE_DEPTH,
        }
    }

    // --- wiring (in the real system, produced by the compiler) ---------------

    pub fn add_adapter(&mut self, adapter: Box<dyn Adapter>) -> AdapterIdx {
        self.adapters.push(adapter);
        self.adapters.len() - 1
    }

    /// Register a northbound adapter. It is ticked (and its `next_wake` honored)
    /// like an adapter and fed `state_folded` like an observer, but binds no
    /// devices and is never a dispatch target. See [`NorthboundAdapter`].
    ///
    /// [`NorthboundAdapter`]: crate::adapters::NorthboundAdapter
    pub fn add_northbound(&mut self, adapter: Box<dyn crate::adapters::NorthboundAdapter>) {
        self.northbound.push(adapter);
    }

    pub fn bind_device(&mut self, device: DeviceId, adapter: AdapterIdx) {
        self.device_to_adapter.insert(device, adapter);
    }

    pub fn add_rule(&mut self, rule: Rule) {
        self.rules.push(rule);
    }

    pub fn add_scene(&mut self, scene: SceneId, commands: Vec<Command>) {
        self.scenes.insert(scene, commands);
    }

    /// Register an observer. Multiple may coexist and are notified in
    /// registration order: a host's trace/logging observer plus any northbound
    /// adapters that observe `state_folded` to mirror engine state (see
    /// `observe.rs`). Replaces the old single-observer `set_observer`.
    pub fn add_observer(&mut self, observer: Box<dyn Observer>) {
        self.observers.push(observer);
    }

    pub fn set_retry_policy(&mut self, policy: RetryPolicy) {
        self.retry = policy;
    }

    /// Maximum causal cascade depth before an event is dropped as a runaway
    /// feedback loop. The default (32) is far above any legitimate chain.
    pub fn set_max_cascade_depth(&mut self, depth: u32) {
        self.max_cascade_depth = depth;
    }

    // --- driving the loop ----------------------------------------------------

    pub fn now(&self) -> Millis {
        self.now
    }

    pub fn switch_state(&self, device: DeviceId) -> Option<bool> {
        self.state.switch_is(device)
    }

    /// Smallest delay (ms from the current virtual `now`) after which some adapter
    /// needs a `tick` on its own initiative — the next due timer, the next clock
    /// minute. A real-time host blocks at most this long (or until a `Waker`
    /// signals inbound I/O) before calling `advance`, so it sleeps exactly until
    /// the next due event instead of polling. `None` means no adapter has any
    /// scheduled work, so the host should wait solely on external I/O.
    pub fn next_wake_delay(&self) -> Option<Millis> {
        let adapters = self.adapters.iter().filter_map(|a| a.next_wake(self.now));
        let northbound = self.northbound.iter().filter_map(|a| a.next_wake(self.now));
        adapters.chain(northbound).min()
    }

    /// Boot phase: tick every adapter once so initial state (notably the clock's
    /// time/sun snapshot) is in the store before any event is processed. Call
    /// after wiring is complete.
    pub fn start(&mut self) {
        self.tick_adapters();
        self.drain();
        self.sync_northbound_startup_state();
    }

    /// Replay the current state store into every northbound adapter, so a freshly
    /// built projection (a Matter node's attribute cells) reflects engine truth at
    /// boot instead of protocol defaults. Without this, a controller reading an
    /// endpoint immediately after pairing sees the node's defaults (off / full
    /// brightness) until the next fold arrives. Idempotent: `state_folded` on a
    /// northbound adapter just overwrites its mirror.
    ///
    /// Only northbound adapters are synced — ordinary observers already witnessed
    /// each fold as it happened and a replay would double-report to them.
    fn sync_northbound_startup_state(&mut self) {
        if self.northbound.is_empty() {
            return;
        }
        let snapshot: Vec<(DeviceId, CapabilityState)> = self
            .state
            .iter()
            .map(|(device, state)| (device, state.clone()))
            .collect();
        for (device, state) in &snapshot {
            for nb in &mut self.northbound {
                nb.state_folded(*device, state);
            }
        }
    }

    /// Inject an inbound event (as an adapter would) and run to quiescence.
    pub fn inject(&mut self, event: Event) {
        self.queue.push_back((event, 0));
        self.drain();
    }

    /// Advance virtual time, firing any due timers, then run to quiescence.
    pub fn advance(&mut self, dt: Millis) {
        self.now += dt;
        self.tick_adapters();
        self.drain();
    }

    /// Tick every adapter and enqueue what they produce at depth 0. Collected
    /// first to avoid borrowing `self.adapters` and `self.queue` simultaneously.
    fn tick_adapters(&mut self) {
        let mut produced = Vec::new();
        for adapter in &mut self.adapters {
            produced.extend(adapter.tick(self.now));
        }
        // Northbound adapters tick too: a controller write (e.g. Matter attribute
        // write) drains into inbound `Event`s — the same pull-after-`Waker` path
        // a southbound device report takes.
        for adapter in &mut self.northbound {
            produced.extend(adapter.tick(self.now));
        }
        for ev in produced {
            self.queue.push_back((ev, 0));
        }
    }

    fn drain(&mut self) {
        while let Some((ev, depth)) = self.queue.pop_front() {
            // Backstop: refuse to follow a cascade past the configured depth.
            if depth > self.max_cascade_depth {
                notify(&mut self.observers, |o| o.cascade_dropped(&ev, depth));
                continue;
            }

            notify(&mut self.observers, |o| o.event_received(&ev, depth));

            // Intercept internal retry timers before rule matching: they re-issue
            // a command rather than acting as a user-visible trigger. A retry is
            // time-gated, so it restarts the causal chain at depth 0.
            if let Event::TimerElapsed { key } = &ev {
                if let Some(pending) = self.retries.remove(key) {
                    self.dispatch_at(pending.command, pending.attempt, 0);
                    continue;
                }
            }

            // Intercept engine-managed `for:` timers, the same way: a sustained
            // edge has waited out its duration. Re-verify the trigger's predicate
            // still holds against the *current* store (belt-and-suspenders: state
            // may have changed via a path that didn't cancel), then fire the
            // rule's commands. Time-gated, so it starts a fresh causal chain.
            if let Event::TimerElapsed { key } = &ev {
                if key.0.starts_with(FOR_KEY_PREFIX) {
                    self.fire_elapsed_for(key);
                    continue;
                }
            }

            // A consumer-requested change (northbound inbound: a Matter controller
            // attribute write, a REST call, a web toggle) becomes the same command
            // a rule would emit and is dispatched one causal hop from the request.
            // It is deliberately *not* folded into the store and *not* run through
            // rule matching — it is an intent to act, not a report of reality, and
            // reality arrives later as the device's own echo. A non-writable
            // desired state (battery, time) yields no command and is a harmless
            // no-op.
            if let Event::RequestedChange { device, desired } = &ev {
                if let Some(cmd) = Self::command_for_requested_change(*device, desired) {
                    self.dispatch_at(cmd, 1, depth);
                }
                continue;
            }

            // Capture the *prior* value of a reported capability before folding,
            // so on-change (edge) triggers can see the transition. The store is
            // about to be overwritten with the new value in `fold_state`, so this
            // is the only moment the old value is available. Cheap: one clone of a
            // small enum, only for `StateReported`.
            let prior: Option<CapabilityState> = match &ev {
                Event::StateReported { device, state } => {
                    self.state.get(*device, state.kind()).cloned()
                }
                _ => None,
            };

            self.fold_state(&ev);

            // On any state report, auto-cancel a pending `for:` timer whose
            // sustained predicate no longer holds (e.g. new motion arrived, so
            // "motion clear for 5m" must not fire). Done against the freshly
            // folded store, before matching, so a report that both cancels one
            // rule's timer and (re)starts another's is handled in order.
            if matches!(ev, Event::StateReported { .. }) {
                self.cancel_reverted_for_timers();
            }

            // Collect commands from every rule that matches. Distinct field
            // borrows (rules + state read-only, observer mutable) — fine for the
            // borrow checker. Evaluating the condition for *every* matched rule
            // (not short-circuiting on `is_true`) lets the observer report the
            // three-valued `Truth` — the key signal when a rule won't fire.
            //
            // A `for:`-qualified rule that fires does not dispatch immediately —
            // it (re)starts a sustained timer; its commands run only if the
            // predicate survives the duration (see `fire_elapsed_for`).
            let mut commands: Vec<Command> = Vec::new();
            let mut arm_for: Vec<(RuleId, Millis)> = Vec::new();
            for rule in &self.rules {
                if !rule.trigger.matches(&ev, prior.as_ref()) {
                    continue;
                }
                let truth = rule.condition.eval(&self.state);
                let fired = truth.is_true();
                notify(&mut self.observers, |o| {
                    o.rule_considered(rule.id, truth, fired)
                });
                if fired {
                    match rule.for_duration {
                        Some(d) => arm_for.push((rule.id, d)),
                        None => commands.extend(rule.commands.iter().cloned()),
                    }
                }
            }
            for (rule, d) in arm_for {
                self.arm_for_timer(rule, d);
            }
            for cmd in commands {
                self.dispatch_at(cmd, 1, depth);
            }
        }
    }

    /// (Re)start a rule's `for:` timer: cancel any timer already pending for this
    /// rule (a fresh edge restarts the wait), then schedule a new one through the
    /// scheduler adapter under a generated `__for:` key.
    fn arm_for_timer(&mut self, rule: RuleId, after: Millis) {
        if let Some(prev) = self.pending_for.remove(&rule) {
            let _ = self.adapters[SCHEDULER_IDX]
                .dispatch(&Command::CancelTimer { key: prev.key }, self.now);
        }
        self.for_counter += 1;
        let key = TimerKey(format!("{FOR_KEY_PREFIX}{}", self.for_counter));
        self.pending_for
            .insert(rule, PendingFor { key: key.clone() });
        let _ =
            self.adapters[SCHEDULER_IDX].dispatch(&Command::ScheduleTimer { key, after }, self.now);
    }

    /// Cancel every pending `for:` timer whose trigger predicate no longer holds
    /// against the current store. Called after each state fold — the auto-cancel
    /// that makes "sustained" safe (state reverts → the pending fire is dropped).
    fn cancel_reverted_for_timers(&mut self) {
        let reverted: Vec<RuleId> = self
            .pending_for
            .keys()
            .copied()
            .filter(|rule| {
                self.rules
                    .iter()
                    .find(|r| r.id == *rule)
                    .map(|r| !r.trigger.predicate_holds(&self.state))
                    .unwrap_or(true)
            })
            .collect();
        for rule in reverted {
            if let Some(pending) = self.pending_for.remove(&rule) {
                let _ = self.adapters[SCHEDULER_IDX]
                    .dispatch(&Command::CancelTimer { key: pending.key }, self.now);
            }
        }
    }

    /// A `for:` timer elapsed: re-verify the rule's predicate still holds, then
    /// dispatch its commands. The timer fired means the duration passed without an
    /// auto-cancel, but re-verifying keeps semantics honest against any state
    /// change that slipped through.
    fn fire_elapsed_for(&mut self, key: &TimerKey) {
        // Find (and clear) the pending entry this key belongs to.
        let Some(rule_id) = self
            .pending_for
            .iter()
            .find(|(_, p)| &p.key == key)
            .map(|(id, _)| *id)
        else {
            return; // stale timer (already cancelled/superseded)
        };
        self.pending_for.remove(&rule_id);

        let Some(rule) = self.rules.iter().find(|r| r.id == rule_id) else {
            return;
        };
        if rule.trigger.predicate_holds(&self.state) {
            let commands = rule.commands.clone();
            for cmd in commands {
                self.dispatch_at(cmd, 1, 0);
            }
        }
    }

    /// Fold device feedback / sensor reports into the disposable state store, and
    /// fan the change to observers *and* northbound adapters (the state mirror).
    fn fold_state(&mut self, ev: &Event) {
        if let Event::StateReported { device, state } = ev {
            self.fan_state_folded(*device, state);
            self.state.set(*device, state.clone());
        }
    }

    /// Deliver a folded state change to every observer *and* every northbound
    /// adapter. Northbound adapters are `Observer`s too, but live in their own
    /// list (they also tick); this is the single seam that keeps their outward
    /// mirror in sync with the store. Kept separate from `notify` so the two
    /// disjoint field borrows (`observers`, `northbound`) are explicit.
    fn fan_state_folded(&mut self, device: DeviceId, state: &CapabilityState) {
        notify(&mut self.observers, |o| o.state_folded(device, state));
        for nb in &mut self.northbound {
            nb.state_folded(device, state);
        }
    }

    /// Dispatch one command. `depth` is the causal depth of the event that
    /// produced it; events this command yields enter the queue at `depth + 1`.
    fn dispatch_at(&mut self, cmd: Command, attempt: u32, depth: u32) {
        // Resolve a toggle against known state before anything else, so the trace
        // and the adapter both see the concrete `SetSwitch` (see `resolve_toggle`).
        let cmd = self.resolve_implicit_state_command(cmd);

        notify(&mut self.observers, |o| o.command_dispatched(&cmd, depth));

        // Scenes have no special runtime semantics: expand to fresh commands at
        // the same causal depth (one hop from the activating event).
        if let Command::ActivateScene { scene } = &cmd {
            let scene = *scene;
            if let Some(cmds) = self.scenes.get(&scene).cloned() {
                notify(&mut self.observers, |o| o.scene_expanded(scene, cmds.len()));
                for c in cmds {
                    self.dispatch_at(c, 1, depth);
                }
            }
            return;
        }

        // Scheduler commands are routed internally and never fail.
        if matches!(
            &cmd,
            Command::ScheduleTimer { .. } | Command::CancelTimer { .. }
        ) {
            if let DispatchOutcome::Ok(evs) = self.adapters[SCHEDULER_IDX].dispatch(&cmd, self.now)
            {
                self.enqueue_all(evs, depth + 1);
            }
            return;
        }

        // Device-targeted command: route to the bound adapter.
        let target = cmd
            .target_device()
            .and_then(|d| self.device_to_adapter.get(&d).copied());
        match target {
            Some(idx) => {
                let outcome = self.adapters[idx].dispatch(&cmd, self.now);
                self.handle_outcome(outcome, cmd, attempt, depth);
            }
            None => {
                // Device command with no bound adapter: a permanent misconfig.
                // The compiler should reject this; the runtime fails it loudly.
                let outcome =
                    DispatchOutcome::Permanent("no adapter bound for target device".into());
                self.handle_outcome(outcome, cmd, attempt, depth);
            }
        }
    }

    /// Translate a consumer-requested *desired state* into the `Command` that
    /// achieves it — the canonical "northbound intent → action" mapping, kept in
    /// one place so every northbound adapter (HomeKit, REST, web) speaks pure
    /// state and never constructs commands itself. Requests carry no transition
    /// (a tap is instantaneous intent), so brightness/color use `None`.
    ///
    /// Returns `None` for capability states that are *reports only* and have no
    /// corresponding write command (`Occupancy`, `Battery`, `TimeOfDay`,
    /// `SunUp`) — requesting those is a harmless no-op rather than an error.
    fn command_for_requested_change(
        device: DeviceId,
        desired: &CapabilityState,
    ) -> Option<Command> {
        use CapabilityState as S;
        match *desired {
            S::Switch(on) => Some(Command::SetSwitch { device, on }),
            S::Brightness(value) => Some(Command::SetBrightness {
                device,
                value,
                transition: None,
            }),
            S::Color { r, g, b } => Some(Command::SetColor {
                device,
                r,
                g,
                b,
                transition: None,
            }),
            S::ColorTemperature(mireds) => Some(Command::SetColorTemperature {
                device,
                mireds,
                transition: None,
            }),
            // Read-only capabilities: a northbound write to a sensor has no
            // command and is a harmless no-op (matches Battery/TimeOfDay).
            S::Occupancy(_)
            | S::Battery(_)
            | S::Temperature(_)
            | S::Humidity(_)
            | S::Illuminance(_)
            | S::Power(_)
            | S::Contact(_)
            | S::WaterLeak(_)
            | S::Smoke(_)
            | S::TimeOfDay(_)
            | S::SunUp(_) => None,
        }
    }

    /// Perform state-aware transformations for commands that have implicit effects based on current device state.
    ///
    /// This resolves "implicit" commands into explicit, state-driven actions. For example:
    /// - Resolves a `ToggleSwitch` into a concrete `SetSwitch { on: !current }` if the switch state is known,
    ///   ensuring adapters only receive explicit On/Off commands, rather than ambiguous toggles that may be
    ///   mishandled by device firmware or become opaque in traces. If the state is unknown, leaves the toggle
    ///   command as-is to be interpreted by the device.
    /// - Resolves `DecreaseBrightness` and `IncreaseBrightness` into concrete `SetBrightness` commands, adjusting
    ///   the value relative to the currently known brightness. If the brightness is unknown, leaves the original
    ///   increment/decrement command intact.
    ///
    /// Any command type not specifically handled is passed through unchanged.
    fn resolve_implicit_state_command(&self, cmd: Command) -> Command {
        match cmd {
            Command::ToggleSwitch { device } => {
                if let Some(on) = self.state.switch_is(device) {
                    return Command::SetSwitch { device, on: !on };
                }
                cmd
            }
            Command::DecreaseBrightness { device, value } => {
                if let Some(brightness) = self.state.num_value(device, CapabilityKind::Brightness) {
                    let level = (brightness - i64::from(value)).clamp(0, 100) as u8;
                    return Command::SetBrightness {
                        device,
                        value: level,
                        transition: None,
                    };
                }
                cmd
            }
            Command::IncreaseBrightness { device, value } => {
                if let Some(brightness) = self.state.num_value(device, CapabilityKind::Brightness) {
                    let level = (brightness + i64::from(value)).clamp(0, 100) as u8;
                    return Command::SetBrightness {
                        device,
                        value: level,
                        transition: None,
                    };
                }
                cmd
            }
            _ => cmd,
        }
    }

    fn handle_outcome(&mut self, outcome: DispatchOutcome, cmd: Command, attempt: u32, depth: u32) {
        match outcome {
            DispatchOutcome::Ok(evs) => self.enqueue_all(evs, depth + 1),
            DispatchOutcome::Transient(reason) => {
                notify(&mut self.observers, |o| {
                    o.dispatch_failed(&cmd, &reason, true, attempt)
                });
                if attempt < self.retry.max_attempts {
                    self.schedule_retry(cmd, attempt);
                } else {
                    self.give_up(cmd, reason, attempt, depth);
                }
            }
            DispatchOutcome::Permanent(reason) => {
                notify(&mut self.observers, |o| {
                    o.dispatch_failed(&cmd, &reason, false, attempt)
                });
                self.give_up(cmd, reason, attempt, depth);
            }
        }
    }

    fn schedule_retry(&mut self, cmd: Command, failed_attempt: u32) {
        let next_attempt = failed_attempt + 1;
        let delay = self.retry.backoff(failed_attempt);
        self.retry_counter += 1;
        let key = TimerKey(format!("{RETRY_KEY_PREFIX}{}", self.retry_counter));

        notify(&mut self.observers, |o| {
            o.retry_scheduled(&cmd, next_attempt, delay)
        });
        self.retries.insert(
            key.clone(),
            PendingRetry {
                command: cmd,
                attempt: next_attempt,
            },
        );
        // Route through the scheduler so the retry is a normal future event.
        let sched = Command::ScheduleTimer { key, after: delay };
        let _ = self.adapters[SCHEDULER_IDX].dispatch(&sched, self.now);
    }

    fn give_up(&mut self, cmd: Command, reason: String, attempts: u32, depth: u32) {
        notify(&mut self.observers, |o| {
            o.command_failed(&cmd, &reason, attempts)
        });
        self.enqueue(
            Event::CommandFailed {
                command: cmd,
                reason,
                attempts,
            },
            depth + 1,
        );
    }

    fn enqueue(&mut self, ev: Event, depth: u32) {
        self.queue.push_back((ev, depth));
    }

    fn enqueue_all(&mut self, evs: Vec<Event>, depth: u32) {
        for ev in evs {
            self.queue.push_back((ev, depth));
        }
    }
}
