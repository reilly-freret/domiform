//! The `matter_device` **northbound** adapter: domiform exposes its own devices
//! as a single native Matter node on the LAN. Apple Home, Google Home and Alexa
//! all commission Matter devices directly, so this is how a polished app on any
//! of those ecosystems drives domiform — without domiform re-declaring anything
//! (the devices are already in the YAML; this is a *projection* of them, the same
//! single-source-of-truth argument as `docs/design/northbound-adapters.md`).
//!
//! It is the dual of the southbound [`MatterAdapter`](super::MatterAdapter): that
//! one *controls* Matter devices; this one *is* one.
//!
//! ## The seam
//!
//! Like every network adapter here, the async protocol runtime is confined behind
//! a trait — [`MatterTransport`] — so the interesting, bug-prone part (mapping
//! `CapabilityState` ↔ Matter cluster attributes) is pure and unit-tested with no
//! node, no radio, no mDNS. The real transport (a `rs-matter` node on a background
//! thread, nudging the host via a [`Waker`]) sits behind the same trait.
//!
//! ```text
//!   state_folded(dev, state) ─▶ MatterTransport::publish ─▶ node attribute DB
//!   controller writes attr   ─▶ MatterTransport::poll   ─▶ Event::RequestedChange
//! ```
//!
//! The write path is exactly the z2m/​device inbound path in reverse: a controller
//! writes `OnOff.OnOff` or `LevelControl.CurrentLevel`, the transport queues the
//! resulting `(DeviceId, CapabilityState)`, and `tick` drains it into a
//! `RequestedChange` — so an app tap and a physical wall switch are
//! indistinguishable to the engine.

use serde::Deserialize;
use serde_yaml::Value;

use super::plugin::{AdapterPlugin, ExposeSpec, Polarity};
use super::{config_of, Adapter, DispatchOutcome, NorthboundAdapter};
use crate::compile::diagnostic::Diagnostic;
use crate::compile::resolve::DeviceDef;
use crate::ids::DeviceId;
use crate::model::{CapabilityKind, CapabilityState, Command, Event, Millis};
use crate::observe::Observer;
use crate::wake::Waker;

/// The seam between the synchronous engine and the asynchronous Matter node.
///
/// The adapter only ever *publishes* domiform state outward (so the node's
/// attribute database mirrors reality) and *polls* for inbound controller writes.
/// A real implementation runs the `rs-matter` node on its own thread and hands
/// writes back via `poll`; tests use an in-memory transport. Either way the
/// `CapabilityState` ↔ cluster mapping — the part worth testing — is pure.
///
/// Not required to be `Send`: the engine is single-threaded, and a real transport
/// keeps its own runtime thread internally, exposing only this surface.
pub trait MatterTransport {
    /// Reflect a device's current capability state into the node's attribute
    /// database, so a controller reading the endpoint sees domiform's truth. A
    /// capability the node doesn't model is silently ignored.
    fn publish(&mut self, device: DeviceId, state: &CapabilityState);

    /// Every controller-originated attribute write since the last call, already
    /// translated to canonical `(device, desired state)` pairs (non-blocking).
    fn poll(&mut self) -> Vec<(DeviceId, CapabilityState)>;

    /// Whether the underlying node is still running. The real transport owns a
    /// background thread that can die (fatal `rs-matter` error or panic); when it
    /// does, the bridge is offline until domiform restarts and the adapter must be
    /// able to say so. Transports with no such thread (in-memory, no-op) are always
    /// healthy — hence the default.
    fn is_healthy(&self) -> bool {
        true
    }
}

/// One domiform device projected as a Matter endpoint: which capabilities it
/// exposes (→ which clusters the endpoint carries) and a human label.
#[derive(Clone, Debug)]
pub struct ExposedDevice {
    pub id: DeviceId,
    pub label: String,
    pub capabilities: Vec<CapabilityKind>,
}

/// The Matter *device type* an endpoint should advertise, derived from a device's
/// declared capabilities. This is the coarse classification a controller uses to
/// pick an icon/affordance; the precise behavior comes from the clusters.
///
/// Kept as a pure mapping (no `rs-matter` types) so it is unit-testable and the
/// real transport is the only thing that needs the concrete device-type ids.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatterDeviceType {
    /// Has color (hue/sat) and/or color temperature — an extended color light
    /// (0x010D). Always carries OnOff + LevelControl + ColorControl.
    ExtendedColorLight,
    /// Has brightness (dimmable light, 0x0101).
    DimmableLight,
    /// On/off only (on/off light or plug-in unit, 0x0100).
    OnOffLight,
    /// An occupancy sensor (0x0107) — read-only to the controller.
    OccupancySensor,
    /// Nothing this adapter models yet; the endpoint is skipped.
    Unsupported,
}

/// Classify a device's capabilities into the Matter device type its endpoint
/// advertises. Color or color-temperature makes an extended color light (which
/// subsumes dimming and on/off); brightness alone is a dimmable light; a bare
/// switch is an on/off light; occupancy alone is a sensor.
pub fn device_type_for(caps: &[CapabilityKind]) -> MatterDeviceType {
    let has = |k: CapabilityKind| caps.contains(&k);
    if has(CapabilityKind::Color) || has(CapabilityKind::ColorTemperature) {
        MatterDeviceType::ExtendedColorLight
    } else if has(CapabilityKind::Brightness) {
        MatterDeviceType::DimmableLight
    } else if has(CapabilityKind::Switch) {
        MatterDeviceType::OnOffLight
    } else if has(CapabilityKind::Occupancy) {
        MatterDeviceType::OccupancySensor
    } else {
        MatterDeviceType::Unsupported
    }
}

/// Whether this adapter can currently project a given capability onto a Matter
/// cluster attribute. Drives both endpoint construction (which clusters to add)
/// and the publish/poll mapping.
///
/// Switch, Brightness, Color, and ColorTemperature are wired in the live node.
/// Occupancy and Battery are deliberately *not* admitted here — admitting them
/// without their sensor/power clusters would advertise the wrong device type
/// (e.g. an occupancy sensor as an On/Off light). Engine-internal capabilities
/// (IR, time-of-day, sun) are never exposed.
pub fn capability_is_exposable(kind: CapabilityKind) -> bool {
    matches!(
        kind,
        CapabilityKind::Switch
            | CapabilityKind::Brightness
            | CapabilityKind::Color
            | CapabilityKind::ColorTemperature
    )
}

/// Adapter that projects a set of domiform devices as a Matter node.
pub struct MatterDeviceAdapter {
    /// Exposed devices, in a stable order (used to assign Matter endpoint ids in
    /// the real transport; kept here so the mapping is deterministic).
    exposed: Vec<ExposedDevice>,
    transport: Box<dyn MatterTransport>,
    /// Set once the transport first reports unhealthy, so the "node died" error is
    /// logged exactly once rather than on every tick.
    degraded: bool,
}

impl MatterDeviceAdapter {
    pub fn new(exposed: Vec<ExposedDevice>, transport: Box<dyn MatterTransport>) -> Self {
        MatterDeviceAdapter {
            exposed,
            transport,
            degraded: false,
        }
    }

    /// The devices this adapter projects (test/introspection aid).
    pub fn exposed(&self) -> &[ExposedDevice] {
        &self.exposed
    }
}

impl Adapter for MatterDeviceAdapter {
    /// A northbound adapter binds no devices, so no command is ever routed here.
    fn dispatch(&mut self, _cmd: &Command, _now: Millis) -> DispatchOutcome {
        DispatchOutcome::Permanent(
            "matter_device is a northbound adapter, not a dispatch target".into(),
        )
    }

    /// Drain controller-originated attribute writes into `RequestedChange`s, and
    /// surface node-thread death once (a fatal `rs-matter` error or panic kills the
    /// background node; the engine keeps ticking us regardless).
    fn tick(&mut self, _now: Millis) -> Vec<Event> {
        if !self.degraded && !self.transport.is_healthy() {
            self.degraded = true;
            log::error!(
                "[matter_device] the Matter node thread has exited; the bridge is \
                 OFFLINE (no controller writes or state updates will flow) until \
                 domiform is restarted"
            );
        }
        self.transport
            .poll()
            .into_iter()
            .map(|(device, desired)| Event::RequestedChange { device, desired })
            .collect()
    }
}

impl Observer for MatterDeviceAdapter {
    /// Every folded state change is mirrored into the node so a controller sees
    /// domiform's current truth. `publish` ignores capabilities the node doesn't
    /// model, so unexposed folds are harmless.
    fn state_folded(&mut self, device: DeviceId, state: &CapabilityState) {
        self.transport.publish(device, state);
    }
}

// `MatterDeviceAdapter` is `Adapter + Observer`, so the blanket impl in `mod.rs`
// makes it a `NorthboundAdapter`.

/// The inspectable inner state of an [`InMemoryMatter`], shared between the
/// transport the adapter owns and a handle a test holds.
pub struct InMemoryMatterState {
    /// Every `(device, state)` the adapter mirrored outward (already filtered to
    /// exposable capabilities, as a real node would).
    pub published: Vec<(DeviceId, CapabilityState)>,
    /// Controller writes queued by a test, drained on `poll` (FIFO).
    pub inbound: Vec<(DeviceId, CapabilityState)>,
    /// Node health, mirroring the real transport's liveness flag. Starts healthy;
    /// a test flips it via [`InMemoryMatter::kill`] to exercise the death path.
    pub healthy: bool,
}

impl Default for InMemoryMatterState {
    fn default() -> Self {
        Self {
            published: Vec::new(),
            inbound: Vec::new(),
            healthy: true,
        }
    }
}

/// An in-memory [`MatterTransport`] for tests: records published state and lets a
/// test inject controller writes. No `rs-matter`, no network — this is what makes
/// the mapping testable in isolation, mirroring z2m's in-memory MQTT transport.
///
/// Cloneable: clones share one inner state (via `Rc<RefCell<_>>`), so a test can
/// hold a handle to read what the adapter published and enqueue inbound writes
/// after handing the transport to the adapter.
#[derive(Clone, Default)]
pub struct InMemoryMatter(std::rc::Rc<std::cell::RefCell<InMemoryMatterState>>);

impl InMemoryMatter {
    pub fn new() -> Self {
        Self::default()
    }

    /// What the adapter has mirrored outward so far (a snapshot copy).
    pub fn published(&self) -> Vec<(DeviceId, CapabilityState)> {
        self.0.borrow().published.clone()
    }

    /// Queue a controller-originated attribute write, delivered on the next
    /// `poll` (i.e. the adapter's next `tick`).
    pub fn queue_inbound(&self, device: DeviceId, desired: CapabilityState) {
        self.0.borrow_mut().inbound.push((device, desired));
    }

    /// Simulate the node thread dying, so a subsequent `is_healthy` reports
    /// unhealthy (as the real transport does when `run_node` exits).
    pub fn kill(&self) {
        self.0.borrow_mut().healthy = false;
    }
}

impl MatterTransport for InMemoryMatter {
    fn publish(&mut self, device: DeviceId, state: &CapabilityState) {
        // Only exposable capabilities reach a real node; model that here so tests
        // assert the same filtering the real transport applies.
        if capability_is_exposable(state.kind()) {
            self.0.borrow_mut().published.push((device, state.clone()));
        }
    }

    fn poll(&mut self) -> Vec<(DeviceId, CapabilityState)> {
        std::mem::take(&mut self.0.borrow_mut().inbound)
    }

    fn is_healthy(&self) -> bool {
        self.0.borrow().healthy
    }
}

/// Registers the `matter_device` northbound adapter.
#[derive(Debug)]
pub struct Plugin;
pub static PLUGIN: Plugin = Plugin;

/// `matter_device` config. `expose` selects the devices to project;
/// `runtime_storage_file` optionally overrides where this adapter keeps its Matter
/// commissioning/fabric store.
///
/// That store is **runtime data, not configuration** (see the reproducibility note
/// in `docs/design/northbound-adapters.md`): it holds the operational certificate
/// and keys a controller mints when it commissions this node, which cannot exist
/// in the hand-authored YAML. Persisting it is what lets a paired controller
/// survive a domiform restart instead of going "No Response" until re-paired.
///
/// When unset, it defaults to `<system.runtime_storage_path>/matter.<hash>.state`,
/// where `<hash>` is derived from the adapter's config name — stable across
/// restarts (idempotent) and distinct per adapter (no collisions). The path is a
/// *directory* (`DirKvBlobStore`), despite the historical `_file` config key name.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MatterDeviceConfig {
    #[serde(default)]
    expose: ExposeConfig,
    #[serde(default)]
    runtime_storage_file: Option<String>,
    /// Force the network interface used for Matter mDNS announcement (e.g. `en0`).
    /// When unset, an interface is auto-selected, skipping VPN/tunnel interfaces
    /// (Tailscale `utun*`, WireGuard) that can't join multicast. Set this if
    /// auto-selection picks the wrong NIC and the device isn't discoverable.
    #[serde(default)]
    interface: Option<String>,
}

/// The default fabric-store path for a `matter_device` adapter: a stable,
/// per-adapter directory under the host's runtime storage directory. The name
/// hash is derived from the adapter name so it is deterministic (same path every
/// run) yet collision-free across multiple `matter_device` adapters.
pub fn default_state_file(runtime_dir: &std::path::Path, adapter_name: &str) -> std::path::PathBuf {
    // A small, dependency-free FNV-1a hash of the adapter name → hex. We only need
    // stable uniqueness for a filename, not cryptographic strength.
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in adapter_name.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    runtime_dir.join(format!("matter.{hash:016x}.state"))
}

/// This adapter's effective state-file path: the explicit `runtime_storage_file`
/// if set, else the per-adapter default under the host runtime dir.
fn state_file_for(cfg: &MatterDeviceConfig, ctx: &super::NorthboundCtx) -> std::path::PathBuf {
    match &cfg.runtime_storage_file {
        Some(f) => std::path::PathBuf::from(f),
        None => default_state_file(ctx.runtime_storage_dir, ctx.adapter_name),
    }
}

/// `expose: all` | `expose: [a, b]`, defaulting to all declared devices.
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
        "matter_device"
    }

    fn polarity(&self) -> Polarity {
        Polarity::Northbound
    }

    fn validate_config(&self, config: &Value, at: &str, diags: &mut Vec<Diagnostic>) {
        if let Some(cfg) = config_of::<MatterDeviceConfig>(config, at, diags) {
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
        let cfg: MatterDeviceConfig = serde_yaml::from_value(config.clone()).ok()?;
        Some(match cfg.expose {
            ExposeConfig::Unset | ExposeConfig::Keyword(_) => ExposeSpec::All,
            ExposeConfig::Names(names) => ExposeSpec::Named(names),
        })
    }

    fn build_northbound(
        &self,
        config: &Value,
        exposed: &[&DeviceDef],
        waker: Option<Waker>,
        ctx: &super::NorthboundCtx,
    ) -> Option<Box<dyn NorthboundAdapter>> {
        // Project each exposed device that carries at least one exposable
        // capability. A device with none (an events-only remote, occupancy-only
        // sensor, …) contributes no endpoint and is skipped — with a warning so
        // `expose: [motion]` doesn't silently vanish from the Matter bridge.
        let exposed: Vec<ExposedDevice> = exposed
            .iter()
            .filter_map(|d| {
                let caps: Vec<CapabilityKind> = d
                    .capabilities
                    .iter()
                    .copied()
                    .filter(|c| capability_is_exposable(*c))
                    .collect();
                if caps.is_empty() {
                    log::warn!(
                        "[matter_device] skipping '{}': no Switch/Brightness capability \
                         (only those are projected onto Matter endpoints today)",
                        d.name
                    );
                    return None;
                }
                Some(ExposedDevice {
                    id: d.id,
                    label: d.name.clone(),
                    capabilities: caps,
                })
            })
            .collect();

        // Resolve where this adapter's Matter fabric store lives (validated shape;
        // fall back to the default rather than panic, as other adapters' builds do).
        let cfg: MatterDeviceConfig =
            serde_yaml::from_value(config.clone()).unwrap_or(MatterDeviceConfig {
                expose: ExposeConfig::Unset,
                runtime_storage_file: None,
                interface: None,
            });
        let state_file = state_file_for(&cfg, ctx);

        let transport =
            real_transport::connect(&exposed, &state_file, cfg.interface.clone(), waker);
        Some(Box::new(MatterDeviceAdapter::new(exposed, transport)))
    }
}

/// The real `rs-matter`-backed transport: a live Matter node (attribute DB +
/// responder + built-in mDNS) run on a background thread, persisting its fabric
/// store to the resolved state directory. See [`real_transport`].
mod real_transport;

/// Soft cap on devices a single `matter_device` adapter can expose, re-exported
/// for the resolver's `E_TOO_MANY_EXPOSED` check (`DynamicNode` capacity — the
/// handler chain itself is fixed-depth regardless of N).
pub use real_transport::MAX_MATTER_DEVICES;
