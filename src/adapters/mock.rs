//! A stand-in device adapter that echoes the commanded state back as a
//! `StateReported` event, modeling the device-confirmation feedback path. Used
//! by tests and as the fallback when a protocol adapter isn't compiled in.

use super::{Adapter, DispatchOutcome};
use crate::model::{CapabilityState, Command, Event, Millis};

#[derive(Default)]
pub struct MockDeviceAdapter;

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
            _ => DispatchOutcome::ok(),
        }
    }
}
