//! The `virtual` adapter: domiform-owned **stateful devices with no physical
//! backing**. A virtual device holds state that lives only in domiform — e.g. a
//! switch fronting an IR-only air conditioner, so the appliance becomes a real,
//! foldable, exposable device even though it can't report its own state.
//!
//! It works by echoing every state-setting command straight back as a
//! `StateReported`: commanding `SetSwitch(on)` makes the device *be* `Switch(on)`
//! in the store. That is all it does — the device's *behavior* (fire an IR toggle
//! when the switch changes) lives in rules, not here, keeping domiform's "logic is
//! declarative" tenet intact.
//!
//! Paired with `matter_device`, this is how a stateless appliance gets a real,
//! tappable On/Off tile in Apple Home: expose a `virtual` switch, and a rule
//! translates its changes into IR (see `examples/virtual_ac.yaml`).
//!
//! Implementation note: this is the same echo-on-dispatch shape as the internal
//! [`mock`](super::mock) adapter, but `mock` is a test/fallback stand-in whereas
//! `virtual` is an intentional, user-facing feature — hence the distinct type.

use serde_yaml::Value;

use super::plugin::AdapterPlugin;
use super::{Adapter, DispatchOutcome};
use crate::compile::resolve::DeviceDef;
use crate::model::{CapabilityState, Command, Event, Millis};
use crate::wake::Waker;

/// An adapter whose devices have no physical backing: it echoes each commanded
/// state back as a report, so the engine folds it into truth.
#[derive(Default)]
pub struct VirtualDeviceAdapter;

/// Registers the `virtual` adapter. Like `mock`, it takes no config and no
/// addresses, so both validation hooks accept anything.
#[derive(Debug)]
pub struct Plugin;
pub static PLUGIN: Plugin = Plugin;

impl AdapterPlugin for Plugin {
    fn type_tag(&self) -> &'static str {
        "virtual"
    }

    fn build(
        &self,
        _config: &Value,
        _devices: &[&DeviceDef],
        _waker: Option<Waker>,
    ) -> Box<dyn Adapter> {
        Box::new(VirtualDeviceAdapter)
    }
}

impl Adapter for VirtualDeviceAdapter {
    /// Echo the commanded state back as a report, so the store reflects it. A
    /// virtual device *is* whatever it was last set to.
    fn dispatch(&mut self, cmd: &Command, _now: Millis) -> DispatchOutcome {
        let state = match cmd {
            Command::SetSwitch { on, .. } => CapabilityState::Switch(*on),
            Command::SetBrightness { value, .. } => CapabilityState::Brightness(*value),
            Command::SetColor { r, g, b, .. } => CapabilityState::Color {
                r: *r,
                g: *g,
                b: *b,
            },
            Command::SetColorTemperature { mireds, .. } => CapabilityState::ColorTemperature(*mireds),
            // No state to echo (a virtual device wouldn't normally be an IR sink,
            // but a no-op keeps it harmless).
            _ => return DispatchOutcome::ok(),
        };
        let device = cmd
            .target_device()
            .expect("state-setting command has a target");
        DispatchOutcome::Ok(vec![Event::StateReported { device, state }])
    }
}
