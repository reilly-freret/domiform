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

/// One protocol adapter, as seen by the compiler. Implemented by a zero-sized
/// marker type per adapter (each adapter file exposes a `PLUGIN` static of its
/// type). `Sync` so the registry can be a `static`; `Debug` so a resolved
/// adapter — which carries `&dyn AdapterPlugin` — stays `Debug`.
pub trait AdapterPlugin: Sync + std::fmt::Debug {
    /// The `type:` discriminator in config (e.g. `"zwavejs"`). Must be unique
    /// across the registry; the resolver maps a config `type` to a plugin by it.
    fn type_tag(&self) -> &'static str;

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

    /// Build the runtime adapter for its device set. Called only after
    /// compilation succeeds, so `config` is already validated: re-read it with
    /// the same step `validate_config` used and treat failure as unreachable —
    /// drop the offending item rather than panic, as the rest of the builder
    /// does for values `resolve` already checked.
    fn build(
        &self,
        config: &Value,
        devices: &[&DeviceDef],
        waker: Option<Waker>,
    ) -> Box<dyn Adapter>;
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
