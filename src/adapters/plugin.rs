//! The adapter registration seam: how a protocol adapter tells the compiler it
//! exists, without the compiler knowing any concrete adapter.
//!
//! Adding a protocol used to mean editing the parser (`compile::ast`), the
//! resolver (`compile::resolve`), *and* the engine builder (`compile`), on top
//! of the adapter file itself — so a community adapter PR sprawled across the
//! compiler. Instead, each adapter implements [`AdapterPlugin`] in its own file
//! and is listed once in [`plugins`](super::plugins). The compiler drives every
//! protocol through this trait and never names a concrete adapter, so adding one
//! is a new file in this directory plus a single line in `mod.rs`.
//!
//! The three hooks mirror the compiler's three per-protocol needs, which used to
//! be three scattered `match` arms:
//!
//! * [`validate_config`](AdapterPlugin::validate_config) — the adapter's own
//!   config block (a URL's scheme, a broker's port),
//! * [`validate_device`](AdapterPlugin::validate_device) — each device bound to
//!   it (its `address` shape), and
//! * [`build`](AdapterPlugin::build) — the runtime adapter for its device set.

use serde_yaml::Value;

use crate::compile::diagnostic::Diagnostic;
use crate::compile::resolve::DeviceDef;
use crate::wake::Waker;

use super::Adapter;

/// Which way an adapter's data flows — the northbound/southbound distinction.
/// The trait carries it (not a module split) because it is the *one* place the
/// builder branches: a southbound adapter is built with the devices *bound* to
/// it and drives them; a northbound adapter is built with the devices it
/// *exposes* (declared under other adapters) and mirrors them outward + turns
/// consumer input into inbound events. See `docs/design/northbound-adapters.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Polarity {
    /// domiform is the controller; the protocol is downstream (zigbee2mqtt,
    /// matter, zwavejs). Owns/commands the devices bound to it.
    Southbound,
    /// domiform is the source of truth; the consumer is upstream (homekit, and
    /// later REST/web/voice). Exposes devices declared elsewhere; binds none of
    /// its own. Registers as an `Observer` to mirror engine state.
    Northbound,
}

/// Host-provided context for building a northbound adapter — things the compiler
/// can't know but the running host can. Extensible: future stateful northbound
/// features read what they need from here.
pub struct NorthboundCtx<'a> {
    /// The adapter's config name (e.g. `"home"`). Used to derive a stable,
    /// collision-free default filename for this adapter's runtime state.
    pub adapter_name: &'a str,
    /// The effective directory for runtime state (`system.runtime_storage_path`,
    /// or the config file's directory by default) — already resolved by the host.
    /// A feature that requires runtime state places its files under here.
    pub runtime_storage_dir: &'a std::path::Path,
}

/// Which devices a northbound adapter exposes, as declared in its config. The
/// plugin parses this from its own `expose` syntax; the resolver turns it into a
/// concrete set of `DeviceId`s and validates every named device exists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExposeSpec {
    /// Every device declared in the config (`expose: all`).
    All,
    /// An explicit list of device names (`expose: [nightstand_l, hallway]`).
    Named(Vec<String>),
}

/// One protocol adapter, as seen by the compiler. Implemented by a zero-sized
/// marker type per adapter (each adapter file exposes a `PLUGIN` static of its
/// type). `Sync` so the registry can be a `static`; `Debug` so a resolved
/// adapter — which carries `&dyn AdapterPlugin` — stays `Debug`.
pub trait AdapterPlugin: Sync + std::fmt::Debug {
    /// The `type:` discriminator in config (e.g. `"zwavejs"`). Must be unique
    /// across the registry; the resolver maps a config `type` to a plugin by it.
    fn type_tag(&self) -> &'static str;

    /// This adapter's data-flow polarity. Defaults to [`Polarity::Southbound`],
    /// so every existing protocol adapter is unaffected; a northbound adapter
    /// (homekit, …) overrides it. The resolver and builder use it to decide
    /// whether the adapter is built with its *bound* devices (southbound) or the
    /// devices it *exposes* (northbound), and whether to register it as an
    /// `Observer` for state fan-out.
    fn polarity(&self) -> Polarity {
        Polarity::Southbound
    }

    /// For a northbound adapter, which devices it exposes to its consumer,
    /// parsed from config (`expose: all` or `expose: [a, b]`). Returned by the
    /// plugin because the *syntax* of `expose` is the adapter's own concern; the
    /// resolver turns the [`ExposeSpec`] into concrete `DeviceId`s and validates
    /// that each named device exists. `None` (the default) is correct for every
    /// southbound adapter — it exposes nothing by this mechanism.
    fn expose_spec(&self, config: &Value) -> Option<ExposeSpec> {
        let _ = config;
        None
    }

    /// Validate this adapter's own config block — the fields under
    /// `adapters.<name>` besides `type`. Push a [`Diagnostic`] for anything
    /// wrong; the default accepts any block. Most adapters deserialize `config`
    /// with [`config_of`] (which reports a malformed block for them) and then
    /// check semantics the type system can't, like a URL scheme.
    fn validate_config(&self, config: &Value, at: &str, diags: &mut Vec<Diagnostic>) {
        let _ = (config, at, diags);
    }

    /// Validate one device bound to this adapter — typically its `address` (a
    /// numeric node id, a friendly_name, …). `at` is the device's diagnostic
    /// context. The default accepts any device (e.g. the mock adapter).
    fn validate_device(
        &self,
        config: &Value,
        device: &DeviceDef,
        at: &str,
        diags: &mut Vec<Diagnostic>,
    ) {
        let _ = (config, device, at, diags);
    }

    /// Build the runtime adapter for its bound device set (the **southbound**
    /// path). Called only after compilation succeeds, so `config` is already
    /// validated: re-read it with the same step `validate_config` used and treat
    /// failure as unreachable — drop the offending item rather than panic, as the
    /// rest of the builder does for values `resolve` already checked.
    ///
    /// Defaulted to a loud `unreachable!` so a **northbound** plugin (which
    /// implements [`build_northbound`](Self::build_northbound) instead) need not
    /// supply an unused southbound builder. The builder dispatches on
    /// [`polarity`](Self::polarity), so a southbound plugin's `build` is the only
    /// one ever called here; a southbound plugin that forgets to implement it
    /// fails loudly rather than silently.
    fn build(
        &self,
        config: &Value,
        devices: &[&DeviceDef],
        waker: Option<Waker>,
    ) -> Box<dyn Adapter> {
        let _ = (config, devices, waker);
        unreachable!(
            "southbound plugin '{}' must implement build()",
            self.type_tag()
        )
    }

    /// Build the runtime adapter for a **northbound** plugin. `exposed` is the
    /// resolved set of devices this adapter mirrors — declared under other
    /// adapters and validated by the resolver against [`expose_spec`]. The
    /// returned object is both an [`Adapter`] (its `tick` drains consumer input;
    /// `next_wake` participates in the host loop) and an
    /// [`Observer`](crate::observe::Observer) (it receives `state_folded` to keep
    /// its mirror fresh) — i.e. a [`NorthboundAdapter`](super::NorthboundAdapter).
    ///
    /// Defaults to `None`: an adapter that isn't northbound never builds one, and
    /// the builder uses [`build`](Self::build) for it instead. A plugin whose
    /// [`polarity`](Self::polarity) is [`Northbound`](Polarity::Northbound) must
    /// override this and return `Some`.
    fn build_northbound(
        &self,
        config: &Value,
        exposed: &[&DeviceDef],
        waker: Option<Waker>,
        ctx: &NorthboundCtx,
    ) -> Option<Box<dyn super::NorthboundAdapter>> {
        let _ = (config, exposed, waker, ctx);
        None
    }
}

/// Deserialize an adapter's config block into its typed, `deny_unknown_fields`
/// form, reporting a malformed block as a single `E_ADAPTER_CONFIG` diagnostic.
/// Adapters call this from [`validate_config`](AdapterPlugin::validate_config);
/// [`build`](AdapterPlugin::build) uses the infallible `serde_yaml::from_value(…)
/// .ok()` twin, since by then the block is known good.
pub fn config_of<T: serde::de::DeserializeOwned>(
    config: &Value,
    at: &str,
    diags: &mut Vec<Diagnostic>,
) -> Option<T> {
    match serde_yaml::from_value(config.clone()) {
        Ok(v) => Some(v),
        Err(e) => {
            diags.push(Diagnostic::error("E_ADAPTER_CONFIG", e.to_string()).at(at.to_string()));
            None
        }
    }
}
