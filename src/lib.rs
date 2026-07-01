//! Domiform core runtime skeleton.
//!
//! This is the *runtime* half of the design — it deliberately knows nothing
//! about YAML, parsing, or compilation. The compiler's job will be to produce
//! the very types defined here (`Rule`, `Command`, bound `DeviceId`s, etc.)
//! with all string references already resolved to ids.
//!
//! Key design decisions baked into this skeleton:
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
pub mod compile;
pub mod engine;
pub mod ids;
pub mod model;
pub mod observe;
pub mod rule;
pub mod state;
pub mod wake;

pub use adapters::{
    Adapter, AttrReport, ClockAdapter, ClusterCommand, DispatchOutcome, EndpointId, MatterAdapter,
    MatterController, MockDeviceAdapter, MqttMessage, MqttTransport, NodeId, SchedulerAdapter,
    Zigbee2MqttAdapter,
};
pub use compile::{
    build_engine, build_engine_with_waker, compile_str, CompileErrors, CompiledConfig, Diagnostic,
};
pub use engine::{Engine, RetryPolicy};
pub use ids::{ActionId, AdapterIdx, DeviceId, RuleId, SceneId, ScheduleId};
pub use model::{CapabilityKind, CapabilityState, Command, Event, Millis, TimerKey};
pub use observe::{NoopObserver, Observer, StderrObserver};
pub use rule::{CmpOp, Condition, Rule, Trigger, Truth};
pub use state::StateStore;
pub use wake::{wake_channel, WakeListener, WakeReason, Waker};
