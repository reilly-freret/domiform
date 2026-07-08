//! Domiform: declarative smart-home orchestration compiled to a deterministic
//! event engine.
//!
//! **Users** write YAML configs (see [`examples/`](https://github.com/reilly-freret/domiform/tree/main/examples)
//! in the repository) and run them with the `domiform` binary. For Docker setup
//! and quickstart, see the
//! [README](https://github.com/reilly-freret/domiform/blob/main/README.md).
//!
//! **Contributors** extend the system in two main ways:
//!
//! * **New protocol adapter** — one file under [`adapters`] implementing
//!   [`Adapter`] and [`adapters::AdapterPlugin`], plus one line in
//!   [`adapters::plugins`]. See [`adapters::AdapterPlugin`] and any existing adapter
//!   (e.g. [`ZwaveAdapter`]) for the pattern.
//! * **Config language / compiler** — serde mirrors in [`compile::ast`],
//!   semantic checks in [`compile::resolve`], rule lowering in
//!   [`compile::lower`]. Adapter-specific config validation stays in the
//!   adapter's `PLUGIN`, not in the compiler core.
//!
//! Build API docs locally with `cargo doc --open`. Contribution guidelines live
//! in `CONTRIBUTING.md` at the repository root.
//!
//! # Architecture
//!
//! The library splits cleanly into a **compiler** ([`compile`]) and a **runtime**
//! (everything else). The runtime deliberately knows nothing about YAML: the
//! compiler produces the types defined here (`Rule`, `Command`, bound
//! `DeviceId`s, etc.) with all string references already resolved to ids.
//!
//! Key design decisions:
//!
//! * A **single-threaded, ordered event loop** processes one event at a time.
//!   Given the same sequence of injected events and clock advances, behavior is
//!   fully deterministic and replayable.
//! * **Time is just an adapter.** The scheduler emits events (`TimerElapsed`),
//!   accepts commands (`ScheduleTimer` / `CancelTimer`), and — in the full
//!   design — backs a synthetic clock/sun device whose state conditions can
//!   read. Nothing in the rule engine knows time is special.
//! * **Rules are `trigger + condition + commands`**, not `event -> commands`.
//!   Conditions read the current state store (the pressure-test finding).
//! * **Commands carry an optional transition**, so fades push down to the
//!   adapter (pass-through or emulated) instead of forcing rules to sequence.

pub mod adapters;
pub mod color;
pub mod compile;
pub mod engine;
pub mod ids;
pub mod model;
pub mod observe;
pub mod rule;
pub mod state;
pub mod wake;

pub use adapters::{
    Adapter, AttrReport, ClockAdapter, ClusterCommand, DeviceKind, DispatchOutcome, EndpointId,
    MatterAdapter, MatterController, MockDeviceAdapter, MqttMessage, MqttTransport, NodeId,
    SchedulerAdapter, SetValue, ValueUpdate, Zigbee2MqttAdapter, ZwaveAdapter, ZwaveClient,
};
pub use compile::{
    build_engine, build_engine_at, build_engine_with_waker, compile_str, CompileErrors,
    CompiledConfig, Diagnostic,
};
pub use engine::{Engine, RetryPolicy};
pub use ids::{ActionId, AdapterIdx, DeviceId, RuleId, SceneId, ScheduleId};
pub use model::{CapabilityKind, CapabilityState, Command, Event, Millis, TimerKey};
pub use observe::{NoopObserver, Observer, StderrObserver};
pub use rule::{CmpOp, Condition, Rule, Trigger, Truth};
pub use state::StateStore;
pub use wake::{wake_channel, WakeListener, WakeReason, Waker};
