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
use crate::ids::{AdapterIdx, DeviceId, SceneId};
use crate::model::{Command, Event, Millis, TimerKey};
use crate::observe::{NoopObserver, Observer};
use crate::rule::Rule;
use crate::state::StateStore;
use crate::CapabilityKind;

const SCHEDULER_IDX: AdapterIdx = 0;
const RETRY_KEY_PREFIX: &str = "__retry:";
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

pub struct Engine {
    now: Millis,
    /// Each entry is `(event, causal_depth)`.
    queue: VecDeque<(Event, u32)>,
    state: StateStore,
    rules: Vec<Rule>,
    adapters: Vec<Box<dyn Adapter>>,
    device_to_adapter: HashMap<DeviceId, AdapterIdx>,
    scenes: HashMap<SceneId, Vec<Command>>,
    observer: Box<dyn Observer>,
    retry: RetryPolicy,
    retries: HashMap<TimerKey, PendingRetry>,
    retry_counter: u64,
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
            device_to_adapter: HashMap::new(),
            scenes: HashMap::new(),
            observer: Box::new(NoopObserver),
            retry: RetryPolicy::default(),
            retries: HashMap::new(),
            retry_counter: 0,
            max_cascade_depth: DEFAULT_MAX_CASCADE_DEPTH,
        }
    }

    // --- wiring (in the real system, produced by the compiler) ---------------

    pub fn add_adapter(&mut self, adapter: Box<dyn Adapter>) -> AdapterIdx {
        self.adapters.push(adapter);
        self.adapters.len() - 1
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

    pub fn set_observer(&mut self, observer: Box<dyn Observer>) {
        self.observer = observer;
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
        self.adapters
            .iter()
            .filter_map(|a| a.next_wake(self.now))
            .min()
    }

    /// Boot phase: tick every adapter once so initial state (notably the clock's
    /// time/sun snapshot) is in the store before any event is processed. Call
    /// after wiring is complete.
    pub fn start(&mut self) {
        self.tick_adapters();
        self.drain();
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
        for ev in produced {
            self.queue.push_back((ev, 0));
        }
    }

    fn drain(&mut self) {
        while let Some((ev, depth)) = self.queue.pop_front() {
            // Backstop: refuse to follow a cascade past the configured depth.
            if depth > self.max_cascade_depth {
                self.observer.cascade_dropped(&ev, depth);
                continue;
            }

            self.observer.event_received(&ev, depth);

            // Intercept internal retry timers before rule matching: they re-issue
            // a command rather than acting as a user-visible trigger. A retry is
            // time-gated, so it restarts the causal chain at depth 0.
            if let Event::TimerElapsed { key } = &ev {
                if let Some(pending) = self.retries.remove(key) {
                    self.dispatch_at(pending.command, pending.attempt, 0);
                    continue;
                }
            }

            self.fold_state(&ev);

            // Collect commands from every rule that matches. Distinct field
            // borrows (rules + state read-only, observer mutable) — fine for the
            // borrow checker. Evaluating the condition for *every* matched rule
            // (not short-circuiting on `is_true`) lets the observer report the
            // three-valued `Truth` — the key signal when a rule won't fire.
            let mut commands: Vec<Command> = Vec::new();
            for rule in &self.rules {
                if !rule.trigger.matches(&ev) {
                    continue;
                }
                let truth = rule.condition.eval(&self.state);
                let fired = truth.is_true();
                self.observer.rule_considered(rule.id, truth, fired);
                if fired {
                    commands.extend(rule.commands.iter().cloned());
                }
            }
            for cmd in commands {
                self.dispatch_at(cmd, 1, depth);
            }
        }
    }

    /// Fold device feedback / sensor reports into the disposable state store.
    fn fold_state(&mut self, ev: &Event) {
        match ev {
            Event::StateReported { device, state } => {
                self.observer.state_folded(*device, state);
                self.state.set(*device, state.clone());
            }
            Event::OccupancyChanged { device, occupied } => {
                let state = crate::model::CapabilityState::Occupancy(*occupied);
                self.observer.state_folded(*device, &state);
                self.state.set(*device, state);
            }
            _ => {}
        }
    }

    /// Dispatch one command. `depth` is the causal depth of the event that
    /// produced it; events this command yields enter the queue at `depth + 1`.
    fn dispatch_at(&mut self, cmd: Command, attempt: u32, depth: u32) {
        // Resolve a toggle against known state before anything else, so the trace
        // and the adapter both see the concrete `SetSwitch` (see `resolve_toggle`).
        let cmd = self.resolve_implicit_state_command(cmd);

        self.observer.command_dispatched(&cmd, depth);

        // Scenes have no special runtime semantics: expand to fresh commands at
        // the same causal depth (one hop from the activating event).
        if let Command::ActivateScene { scene } = &cmd {
            let scene = *scene;
            if let Some(cmds) = self.scenes.get(&scene).cloned() {
                self.observer.scene_expanded(scene, cmds.len());
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
                self.observer.dispatch_failed(&cmd, &reason, true, attempt);
                if attempt < self.retry.max_attempts {
                    self.schedule_retry(cmd, attempt);
                } else {
                    self.give_up(cmd, reason, attempt, depth);
                }
            }
            DispatchOutcome::Permanent(reason) => {
                self.observer.dispatch_failed(&cmd, &reason, false, attempt);
                self.give_up(cmd, reason, attempt, depth);
            }
        }
    }

    fn schedule_retry(&mut self, cmd: Command, failed_attempt: u32) {
        let next_attempt = failed_attempt + 1;
        let delay = self.retry.backoff(failed_attempt);
        self.retry_counter += 1;
        let key = TimerKey(format!("{RETRY_KEY_PREFIX}{}", self.retry_counter));

        self.observer.retry_scheduled(&cmd, next_attempt, delay);
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
        self.observer.command_failed(&cmd, &reason, attempts);
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
