//! The runtime state store.
//!
//! This is the *disposable* half of the system: ephemeral capability state
//! keyed by `(device, capability)`. Deleting it and replaying the event log
//! should reconstruct it exactly. Conditions read from here at evaluation time.

use std::collections::HashMap;

use crate::ids::DeviceId;
use crate::model::{CapabilityKind, CapabilityState};

#[derive(Default)]
pub struct StateStore {
    map: HashMap<(DeviceId, CapabilityKind), CapabilityState>,
}

impl StateStore {
    pub fn set(&mut self, device: DeviceId, state: CapabilityState) {
        self.map.insert((device, state.kind()), state);
    }

    pub fn get(&self, device: DeviceId, kind: CapabilityKind) -> Option<&CapabilityState> {
        self.map.get(&(device, kind))
    }

    /// Read a boolean capability. `None` means "we have never heard about this
    /// capability" — distinct from `Some(false)`. Conditions treat `None` as
    /// unsatisfied (see `Condition::eval`).
    pub fn bool_value(&self, device: DeviceId, kind: CapabilityKind) -> Option<bool> {
        self.get(device, kind).and_then(CapabilityState::as_bool)
    }

    /// Read a numeric capability. `None` means unknown (see above).
    pub fn num_value(&self, device: DeviceId, kind: CapabilityKind) -> Option<i64> {
        self.get(device, kind).and_then(CapabilityState::as_i64)
    }

    /// Convenience used by the engine's public `switch_state` accessor.
    pub fn switch_is(&self, device: DeviceId) -> Option<bool> {
        self.bool_value(device, CapabilityKind::Switch)
    }
}
