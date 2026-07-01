//! Observability hooks for the dispatch/retry/failure path — and, behind the
//! `-v` verbose flag, a full trace of the event loop.
//!
//! This is deliberately a tiny trait rather than a hard dependency on `log` or
//! `tracing`: the engine calls these methods, and a host wires whatever logging
//! backend it likes behind them. `NoopObserver` keeps tests quiet;
//! `StderrObserver` is the zero-dependency "it at least prints" implementation.
//!
//! The trace hooks (`event_received`, `state_folded`, `rule_considered`,
//! `command_dispatched`, `scene_expanded`) are how you debug a rule that won't
//! fire: they show every event entering the loop, what it folded into the state
//! store, and — crucially — the three-valued `Truth` of each matched rule's
//! condition. A rule whose trigger matches but whose condition logs `Unknown`
//! is reading state that has never been reported (see `rule.rs`).
//!
//! Names, not ids: the runtime speaks only interned ids, but a host can hand the
//! observer the compiler's name tables (`with_names`) so the trace prints
//! `nightstands_left` instead of `DeviceId(1)`. The engine stays name-free.

use std::collections::HashMap;
use std::time::Instant;

use crate::ids::{DeviceId, RuleId, SceneId};
use crate::model::{CapabilityState, Command, Event, Millis};
use crate::rule::Truth;

#[allow(unused_variables)]
pub trait Observer {
    // --- failure path (printed regardless of verbosity) ---------------------

    /// A single dispatch attempt failed. `transient` distinguishes a retryable
    /// failure from a permanent one; `attempt` is 1-based.
    fn dispatch_failed(&mut self, command: &Command, reason: &str, transient: bool, attempt: u32) {}

    /// A retry has been scheduled for `delay` ms from now, as `next_attempt`.
    fn retry_scheduled(&mut self, command: &Command, next_attempt: u32, delay: Millis) {}

    /// A command was given up on (retries exhausted, or a permanent failure).
    /// A `CommandFailed` event is also placed on the bus.
    fn command_failed(&mut self, command: &Command, reason: &str, attempts: u32) {}

    /// An event was dropped because its causal cascade exceeded the configured
    /// depth — the runtime backstop against feedback loops. Seeing this in
    /// practice points at a config cycle the compiler should have flagged.
    fn cascade_dropped(&mut self, event: &Event, depth: u32) {}

    // --- trace path (verbose `-v` only) -------------------------------------

    /// An event was popped off the queue and is about to be processed.
    fn event_received(&mut self, event: &Event, depth: u32) {}

    /// Device/sensor feedback was folded into the state store. This is what a
    /// later `rule_considered` reads — if you never see a `state_folded` for the
    /// device a condition names, that condition is permanently `Unknown`.
    fn state_folded(&mut self, device: DeviceId, state: &CapabilityState) {}

    /// A rule's trigger matched the current event, so its condition was
    /// evaluated. `truth` is the three-valued result and `fired` is whether the
    /// rule's commands were emitted (only on `Truth::True`).
    fn rule_considered(&mut self, rule: RuleId, truth: Truth, fired: bool) {}

    /// A command is about to be dispatched to an adapter (or expanded, for a
    /// scene). `depth` is the causal depth of the event that produced it.
    fn command_dispatched(&mut self, command: &Command, depth: u32) {}

    /// A scene command expanded into its `count` member commands.
    fn scene_expanded(&mut self, scene: SceneId, count: usize) {}
}

/// Discards everything. The engine's default.
pub struct NoopObserver;
impl Observer for NoopObserver {}

/// Prints to stderr. A stand-in until a real `log`/`tracing` adapter exists.
///
/// Failures are always printed. The verbose trace (`event_received`,
/// `rule_considered`, …) is printed only when constructed with [`verbose`].
///
/// [`verbose`]: StderrObserver::verbose
pub struct StderrObserver {
    verbose: bool,
    /// Captured at construction; every line is stamped with elapsed time since
    /// then. The engine's virtual clock is useless for wall-clock timing (it
    /// starts at 0 and only advances on `tick`), so real I/O latency — e.g. the
    /// gap between dispatching a command and its device echo — only shows up
    /// against this monotonic origin.
    start: Instant,
    device_names: HashMap<DeviceId, String>,
    rule_names: HashMap<RuleId, String>,
    scene_names: HashMap<SceneId, String>,
}

impl Default for StderrObserver {
    fn default() -> Self {
        Self {
            verbose: false,
            start: Instant::now(),
            device_names: HashMap::new(),
            rule_names: HashMap::new(),
            scene_names: HashMap::new(),
        }
    }
}

impl StderrObserver {
    /// Failure logging only (the historical behavior).
    pub fn new() -> Self {
        Self::default()
    }

    /// Failure logging *plus* a full per-event trace of the loop.
    pub fn verbose() -> Self {
        Self {
            verbose: true,
            ..Self::default()
        }
    }

    /// Seconds.millis elapsed since construction, for line stamps. Right-padded
    /// so columns line up: `   0.000`, `  12.345`.
    fn ts(&self) -> String {
        let e = self.start.elapsed();
        format!("{:>4}.{:03}", e.as_secs(), e.subsec_millis())
    }

    /// Attach the compiler's name tables so the trace prints names instead of
    /// raw ids. Pass `cfg.devices`/`cfg.scenes`/rule names from the host.
    pub fn with_names(
        mut self,
        devices: impl IntoIterator<Item = (DeviceId, String)>,
        rules: impl IntoIterator<Item = (RuleId, String)>,
        scenes: impl IntoIterator<Item = (SceneId, String)>,
    ) -> Self {
        self.device_names = devices.into_iter().collect();
        self.rule_names = rules.into_iter().collect();
        self.scene_names = scenes.into_iter().collect();
        self
    }

    fn device(&self, id: DeviceId) -> String {
        self.device_names
            .get(&id)
            .cloned()
            .unwrap_or_else(|| format!("{id:?}"))
    }

    fn rule(&self, id: RuleId) -> String {
        self.rule_names
            .get(&id)
            .cloned()
            .unwrap_or_else(|| format!("{id:?}"))
    }

    fn scene(&self, id: SceneId) -> String {
        self.scene_names
            .get(&id)
            .cloned()
            .unwrap_or_else(|| format!("{id:?}"))
    }
}

impl Observer for StderrObserver {
    fn dispatch_failed(&mut self, command: &Command, reason: &str, transient: bool, attempt: u32) {
        let kind = if transient { "transient" } else { "permanent" };
        eprintln!(
            "[{}] [domiform] dispatch failed ({kind}, attempt {attempt}): {command:?} — {reason}",
            self.ts()
        );
    }

    fn retry_scheduled(&mut self, command: &Command, next_attempt: u32, delay: Millis) {
        eprintln!(
            "[{}] [domiform] scheduling attempt {next_attempt} for {command:?} in {delay}ms",
            self.ts()
        );
    }

    fn command_failed(&mut self, command: &Command, reason: &str, attempts: u32) {
        eprintln!(
            "[{}] [domiform] COMMAND FAILED after {attempts} attempt(s): {command:?} — {reason}",
            self.ts()
        );
    }

    fn cascade_dropped(&mut self, event: &Event, depth: u32) {
        eprintln!("[{}] [domiform] CASCADE ABORTED at depth {depth}, dropping {event:?} (likely a config cycle)", self.ts());
    }

    fn event_received(&mut self, event: &Event, depth: u32) {
        if self.verbose {
            eprintln!("[{}] [v] event  @{depth}  {event:?}", self.ts());
        }
    }

    fn state_folded(&mut self, device: DeviceId, state: &CapabilityState) {
        if self.verbose {
            eprintln!(
                "[{}] [v]   state  {} := {state:?}",
                self.ts(),
                self.device(device)
            );
        }
    }

    fn rule_considered(&mut self, rule: RuleId, truth: Truth, fired: bool) {
        if !self.verbose {
            return;
        }
        let name = self.rule(rule);
        if fired {
            eprintln!(
                "[{}] [v]   rule '{name}': trigger matched, condition True -> FIRING",
                self.ts()
            );
        } else if truth == Truth::Unknown {
            eprintln!(
                "[{}] [v]   rule '{name}': trigger matched, but condition is UNKNOWN \
                 (a capability it reads has never been reported) -> not firing",
                self.ts()
            );
        } else {
            eprintln!(
                "[{}] [v]   rule '{name}': trigger matched, condition {truth:?} -> not firing",
                self.ts()
            );
        }
    }

    fn command_dispatched(&mut self, command: &Command, depth: u32) {
        if self.verbose {
            eprintln!("[{}] [v]   dispatch @{depth}  {command:?}", self.ts());
        }
    }

    fn scene_expanded(&mut self, scene: SceneId, count: usize) {
        if self.verbose {
            eprintln!(
                "[{}] [v]   scene '{}' -> {count} command(s)",
                self.ts(),
                self.scene(scene)
            );
        }
    }
}
