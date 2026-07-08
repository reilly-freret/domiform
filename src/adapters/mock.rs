//! A stand-in device adapter that echoes the commanded state back as a
//! `StateReported` event, modeling the device-confirmation feedback path. Used
//! by tests and as the fallback when a protocol adapter isn't compiled in.

use serde_yaml::Value;

use super::plugin::AdapterPlugin;
use super::{Adapter, DispatchOutcome};
use crate::compile::resolve::DeviceDef;
use crate::model::{CapabilityState, Command, Event, Millis};
use crate::wake::Waker;

#[derive(Default)]
pub struct MockDeviceAdapter;

/// Registers the in-memory mock adapter (`type: mock`). It takes no config and
/// no addresses, so both validation hooks accept anything.
#[derive(Debug)]
pub struct Plugin;
pub static PLUGIN: Plugin = Plugin;

impl AdapterPlugin for Plugin {
    fn type_tag(&self) -> &'static str {
        "mock"
    }

    fn build(
        &self,
        _config: &Value,
        _devices: &[&DeviceDef],
        _waker: Option<Waker>,
    ) -> Box<dyn Adapter> {
        Box::new(MockDeviceAdapter)
    }
}

impl Adapter for MockDeviceAdapter {
    fn dispatch(&mut self, cmd: &Command, _now: Millis) -> DispatchOutcome {
        match cmd {
            Command::SetSwitch { device, on } => DispatchOutcome::Ok(vec![Event::StateReported {
                device: *device,
                state: CapabilityState::Switch(*on),
            }]),
            Command::SetBrightness { device, value, .. } => {
                DispatchOutcome::Ok(vec![Event::StateReported {
                    device: *device,
                    state: CapabilityState::Brightness(*value),
                }])
            }
            Command::SetColor {
                device, r, g, b, ..
            } => DispatchOutcome::Ok(vec![Event::StateReported {
                device: *device,
                state: CapabilityState::Color {
                    r: *r,
                    g: *g,
                    b: *b,
                },
            }]),
            Command::SetColorTemperature { device, mireds, .. } => {
                DispatchOutcome::Ok(vec![Event::StateReported {
                    device: *device,
                    state: CapabilityState::ColorTemperature(*mireds),
                }])
            }
            _ => DispatchOutcome::ok(),
        }
    }
}
