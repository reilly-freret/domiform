//! A test-only **northbound** adapter, the northbound analogue of
//! [`MockDeviceAdapter`](super::MockDeviceAdapter). It proves the entire
//! northbound seam — state fan-out inward, consumer input outward — with zero HAP
//! dependency and full determinism, so Phase 1 can be exercised before the real
//! `hap-rs` adapter (Phase 2) exists.
//!
//! Two facets, matching a real northbound adapter:
//! * **Observer**: every `state_folded` the engine delivers is recorded (this is
//!   the mirror a real bridge would push to HomeKit).
//! * **Adapter**: `tick` drains any queued *consumer writes* into inbound
//!   `Event::RequestedChange`s — the same pull-after-`Waker` path a Matter
//!   controller's attribute write (from Apple Home, Google, Alexa, …) would take.
//!
//! Tests share the adapter's inner state via an [`Rc<RefCell<_>>`] handle (the
//! same pattern the integration tests use for recorders), so a test can read what
//! the adapter mirrored and enqueue a write for its next tick.

use std::cell::RefCell;
use std::rc::Rc;

use serde::Deserialize;
use serde_yaml::Value;

use super::plugin::{AdapterPlugin, ExposeSpec, Polarity};
use super::{config_of, Adapter, DispatchOutcome, NorthboundAdapter};
use crate::compile::diagnostic::Diagnostic;
use crate::compile::resolve::DeviceDef;
use crate::ids::DeviceId;
use crate::model::{CapabilityState, Command, Event, Millis};
use crate::observe::Observer;
use crate::wake::Waker;

/// The observable, test-inspectable inner state, shared between the adapter the
/// engine owns and the handle a test holds.
#[derive(Default)]
pub struct MockNorthboundState {
    /// Every `(device, state)` the engine folded and fanned to this adapter, in
    /// order — the outward mirror a real bridge keeps.
    pub mirrored: Vec<(DeviceId, CapabilityState)>,
    /// Consumer writes queued by a test, drained into `RequestedChange`s on the
    /// next `tick` (FIFO). Models input arriving on a bridge's own thread.
    pub pending_writes: Vec<(DeviceId, CapabilityState)>,
}

/// A cloneable handle to a mock northbound adapter's shared state. Clone it before
/// handing the adapter to the engine; the clone reads/writes the same inner state.
#[derive(Clone, Default)]
pub struct MockNorthbound(Rc<RefCell<MockNorthboundState>>);

impl MockNorthbound {
    pub fn new() -> Self {
        Self::default()
    }

    /// What this adapter has mirrored so far (a snapshot copy).
    pub fn mirrored(&self) -> Vec<(DeviceId, CapabilityState)> {
        self.0.borrow().mirrored.clone()
    }

    /// The most recent state mirrored for `device`, if any.
    pub fn latest(&self, device: DeviceId) -> Option<CapabilityState> {
        self.0
            .borrow()
            .mirrored
            .iter()
            .rev()
            .find(|(d, _)| *d == device)
            .map(|(_, s)| s.clone())
    }

    /// Queue a consumer write (a "tap"), delivered as a `RequestedChange` on the
    /// adapter's next `tick`.
    pub fn queue_write(&self, device: DeviceId, desired: CapabilityState) {
        self.0.borrow_mut().pending_writes.push((device, desired));
    }
}

impl Observer for MockNorthbound {
    fn state_folded(&mut self, device: DeviceId, state: &CapabilityState) {
        self.0.borrow_mut().mirrored.push((device, state.clone()));
    }
}

impl Adapter for MockNorthbound {
    /// A northbound adapter binds no devices, so no command is ever routed here.
    fn dispatch(&mut self, _cmd: &Command, _now: Millis) -> DispatchOutcome {
        DispatchOutcome::Permanent("northbound adapter is not a dispatch target".into())
    }

    /// Drain queued consumer writes into inbound `RequestedChange` events.
    fn tick(&mut self, _now: Millis) -> Vec<Event> {
        let writes = std::mem::take(&mut self.0.borrow_mut().pending_writes);
        writes
            .into_iter()
            .map(|(device, desired)| Event::RequestedChange { device, desired })
            .collect()
    }
}

// `MockNorthbound` is `Adapter + Observer`, so the blanket impl makes it a
// `NorthboundAdapter` automatically.

/// Registers the mock northbound adapter (`type: mock_northbound`). Its config is
/// just an `expose` spec, so the config/resolve/build path can be tested without
/// HAP. The engine-level shared handle is only available to tests that construct
/// the adapter directly; the config path builds a fresh (unobserved) instance.
#[derive(Debug)]
pub struct Plugin;
pub static PLUGIN: Plugin = Plugin;

/// The mock northbound adapter's config: which devices it exposes.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MockNorthboundConfig {
    /// `all` or a list of device names. Defaults to exposing everything.
    #[serde(default)]
    expose: ExposeConfig,
}

/// `expose: all` | `expose: [a, b]`, defaulting to all.
#[derive(Debug, Default, Deserialize)]
#[serde(untagged)]
enum ExposeConfig {
    #[default]
    Unset,
    Keyword(String),
    Names(Vec<String>),
}

impl AdapterPlugin for Plugin {
    fn type_tag(&self) -> &'static str {
        "mock_northbound"
    }

    fn polarity(&self) -> Polarity {
        Polarity::Northbound
    }

    fn validate_config(&self, config: &Value, at: &str, diags: &mut Vec<Diagnostic>) {
        // Deserialize to report a malformed block; the keyword form is checked here
        // so a typo like `expose: everything` fails at compile time, not silently.
        if let Some(cfg) = config_of::<MockNorthboundConfig>(config, at, diags) {
            if let ExposeConfig::Keyword(k) = &cfg.expose {
                if k != "all" {
                    diags.push(
                        Diagnostic::error(
                            "E_ADAPTER_CONFIG",
                            format!("`expose` keyword must be 'all', got '{k}'"),
                        )
                        .at(at.to_string()),
                    );
                }
            }
        }
    }

    fn expose_spec(&self, config: &Value) -> Option<ExposeSpec> {
        let cfg: MockNorthboundConfig = serde_yaml::from_value(config.clone()).ok()?;
        Some(match cfg.expose {
            // Unset defaults to exposing everything, matching the doc above.
            ExposeConfig::Unset | ExposeConfig::Keyword(_) => ExposeSpec::All,
            ExposeConfig::Names(names) => ExposeSpec::Named(names),
        })
    }

    fn build_northbound(
        &self,
        _config: &Value,
        _exposed: &[&DeviceDef],
        _waker: Option<Waker>,
        _ctx: &super::NorthboundCtx,
    ) -> Option<Box<dyn NorthboundAdapter>> {
        // The config path builds a fresh instance with no external handle. Tests
        // that need to inspect the mirror construct `MockNorthbound` directly and
        // `add_northbound` it. A real northbound adapter would use `_exposed` to
        // register its accessories, `_waker` to nudge the host on input, and
        // `_ctx.runtime_storage_dir` for any state it must persist.
        Some(Box::new(MockNorthbound::new()))
    }
}
