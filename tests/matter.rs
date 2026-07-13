//! Matter adapter tests, all driven by an in-memory fake controller — no
//! python-matter-server, no WebSocket, no network. Covers protocol translation
//! in both directions and the full attribute-report → rule → cluster-invoke loop
//! through the engine. The structural twin of `tests/zigbee2mqtt.rs`.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use serde_json::{json, Value};

use domiform::ids::{DeviceId, RuleId};
use domiform::rule::{Condition, Rule, Trigger};
use domiform::{
    Adapter, AttrReport, CapabilityKind, CapabilityState, ClusterCommand, Command, EndpointId,
    Engine, Event, MatterAdapter, MatterController, NodeId,
};

const MOTION: DeviceId = DeviceId(0);
const LAMP: DeviceId = DeviceId(1);

// Commissioned Matter targets (node_id + endpoint), as the compiler would carry
// them from each device's `address` / `endpoint`.
const MOTION_NODE: NodeId = NodeId(10);
const LAMP_NODE: NodeId = NodeId(11);
const EP: EndpointId = EndpointId(1);

// Cluster / attribute ids the tests exercise (decimal — see docs/matter.md §5).
const ONOFF: u32 = 0x0006;
const LEVEL: u32 = 0x0008;
const OCCUPANCY: u32 = 0x0406;
const POWER_SOURCE: u32 = 0x002F;
const BAT_PERCENT: u32 = 0x000C;
const COLOR: u32 = 0x0300;
const CURRENT_HUE: u32 = 0x0008;
const CURRENT_SAT: u32 = 0x0009;

/// Shared in-memory controller: tests feed inbound attribute reports and read
/// back what the adapter invoked, while the adapter owns a clone as its seam.
#[derive(Clone, Default)]
struct FakeController(Rc<RefCell<ControllerState>>);

#[derive(Default)]
struct ControllerState {
    invoked: Vec<(NodeId, EndpointId, ClusterCommand)>,
    inbound: VecDeque<AttrReport>,
}

impl FakeController {
    fn report(
        &self,
        node: NodeId,
        endpoint: EndpointId,
        cluster: u32,
        attribute: u32,
        value: Value,
    ) {
        self.0.borrow_mut().inbound.push_back(AttrReport {
            node,
            endpoint,
            cluster,
            attribute,
            value,
        });
    }
    fn invoked(&self) -> Vec<(NodeId, EndpointId, ClusterCommand)> {
        self.0.borrow().invoked.clone()
    }
}

impl MatterController for FakeController {
    fn invoke(
        &mut self,
        node: NodeId,
        endpoint: EndpointId,
        cmd: &ClusterCommand,
    ) -> Result<(), String> {
        self.0
            .borrow_mut()
            .invoked
            .push((node, endpoint, cmd.clone()));
        Ok(())
    }
    fn poll(&mut self) -> Vec<AttrReport> {
        self.0.borrow_mut().inbound.drain(..).collect()
    }
}

fn adapter(controller: &FakeController) -> MatterAdapter {
    MatterAdapter::new(
        [(MOTION, MOTION_NODE, EP), (LAMP, LAMP_NODE, EP)],
        Box::new(controller.clone()),
    )
}

#[test]
fn inbound_onoff_and_level_become_events() {
    let c = FakeController::default();
    let mut a = adapter(&c);

    // OnOff true, CurrentLevel 254 (max) → Switch(true), Brightness 100%.
    c.report(LAMP_NODE, EP, ONOFF, 0x0000, json!(true));
    c.report(LAMP_NODE, EP, LEVEL, 0x0000, json!(254));
    let events = a.tick(0);

    assert!(events.contains(&Event::StateReported {
        device: LAMP,
        state: domiform::CapabilityState::Switch(true),
    }));
    assert!(events.contains(&Event::StateReported {
        device: LAMP,
        state: domiform::CapabilityState::Brightness(100),
    }));
}

#[test]
fn inbound_hue_and_saturation_fold_into_a_color_event() {
    let c = FakeController::default();
    let mut a = adapter(&c);

    // Hue alone is not enough — Matter reports the two attributes separately,
    // so no Color event fires until saturation arrives.
    c.report(LAMP_NODE, EP, COLOR, CURRENT_HUE, json!(0));
    let events = a.tick(0);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::StateReported { state, .. } if format!("{state:?}").contains("Color"))),
        "hue alone should not emit a Color event, got {events:?}"
    );

    // Saturation completes the pair: hue 0 / sat 254 → pure red at full value.
    c.report(LAMP_NODE, EP, COLOR, CURRENT_SAT, json!(254));
    let events = a.tick(0);
    assert!(
        events.contains(&Event::StateReported {
            device: LAMP,
            state: domiform::CapabilityState::Color { r: 255, g: 0, b: 0 },
        }),
        "got {events:?}"
    );
}

#[test]
fn inbound_occupancy_and_battery_become_events() {
    let c = FakeController::default();
    let mut a = adapter(&c);

    // Occupancy bit 0 set → occupied. BatPercentRemaining is half-percent: 160 → 80%.
    c.report(MOTION_NODE, EP, OCCUPANCY, 0x0000, json!(1));
    c.report(MOTION_NODE, EP, POWER_SOURCE, BAT_PERCENT, json!(160));
    let events = a.tick(0);

    assert!(events.contains(&Event::StateReported {
        device: MOTION,
        state: CapabilityState::Occupancy(true),
    }));
    assert!(events.iter().any(|e| matches!(
        e,
        Event::StateReported { device: MOTION, state } if format!("{state:?}").contains("Battery(80)")
    )));
}

#[test]
fn inbound_report_from_unknown_node_is_ignored() {
    let c = FakeController::default();
    let mut a = adapter(&c);

    c.report(NodeId(999), EP, ONOFF, 0x0000, json!(true)); // unmapped node
    c.report(LAMP_NODE, EndpointId(7), ONOFF, 0x0000, json!(true)); // unmapped endpoint
    assert!(a.tick(0).is_empty());
}

#[test]
fn inbound_unmapped_cluster_is_ignored() {
    let c = FakeController::default();
    let mut a = adapter(&c);

    c.report(LAMP_NODE, EP, 0x1234, 0x0000, json!(true)); // no such cluster mapping
    assert!(a.tick(0).is_empty());
}

#[test]
fn outbound_switch_and_toggle_invoke_onoff() {
    let c = FakeController::default();
    let mut a = adapter(&c);

    a.dispatch(
        &Command::SetSwitch {
            device: LAMP,
            on: true,
        },
        0,
    );
    a.dispatch(&Command::ToggleSwitch { device: LAMP }, 0);

    let invoked = c.invoked();
    assert_eq!(invoked[0], (LAMP_NODE, EP, ClusterCommand::OnOff(true)));
    assert_eq!(invoked[1], (LAMP_NODE, EP, ClusterCommand::Toggle));
}

#[test]
fn outbound_brightness_scales_to_level() {
    let c = FakeController::default();
    let mut a = adapter(&c);

    // 50% → level ~127; a 2s transition → 20 tenths of a second.
    a.dispatch(
        &Command::SetBrightness {
            device: LAMP,
            value: 50,
            transition: Some(2000),
        },
        0,
    );

    let invoked = c.invoked();
    assert_eq!(
        invoked[0],
        (
            LAMP_NODE,
            EP,
            ClusterCommand::MoveToLevel {
                level: 127,
                transition_ds: 20
            }
        )
    );
}

#[test]
fn outbound_color_invokes_hue_and_saturation() {
    let c = FakeController::default();
    let mut a = adapter(&c);

    a.dispatch(
        &Command::SetColor {
            device: LAMP,
            r: 255,
            g: 0,
            b: 0,
            transition: Some(1000),
        },
        0,
    );

    let invoked = c.invoked();
    assert!(matches!(
        invoked[0],
        (
            LAMP_NODE,
            EP,
            ClusterCommand::MoveToHueAndSaturation {
                hue: 0,
                saturation: 254,
                transition_ds: 10
            }
        )
    ));
}

#[test]
fn outbound_color_temperature_invokes_move_to_color_temp() {
    let c = FakeController::default();
    let mut a = adapter(&c);

    a.dispatch(
        &Command::SetColorTemperature {
            device: LAMP,
            mireds: 370,
            transition: None,
        },
        0,
    );

    let invoked = c.invoked();
    assert_eq!(
        invoked[0],
        (
            LAMP_NODE,
            EP,
            ClusterCommand::MoveToColorTemperature {
                mireds: 370,
                transition_ds: 0
            }
        )
    );
}

#[test]
fn commanding_an_unmanaged_device_is_permanent_failure() {
    let c = FakeController::default();
    let mut a = adapter(&c);

    let outcome = a.dispatch(
        &Command::SetSwitch {
            device: DeviceId(999),
            on: true,
        },
        0,
    );
    assert!(matches!(outcome, domiform::DispatchOutcome::Permanent(_)));
    assert!(c.invoked().is_empty());
}

#[test]
fn full_loop_attribute_report_to_cluster_invoke() {
    // A real attribute report arrives, a rule fires, and the resulting command is
    // invoked back out — the whole adapter↔engine round trip, no controller.
    let c = FakeController::default();

    let mut engine = Engine::new();
    let idx = engine.add_adapter(Box::new(adapter(&c)));
    engine.bind_device(MOTION, idx);
    engine.bind_device(LAMP, idx);
    engine.add_rule(Rule::new(
        RuleId(0),
        Trigger::Changed {
            device: MOTION,
            kind: CapabilityKind::Occupancy,
            to: true,
        },
        Condition::Always,
        vec![Command::SetSwitch {
            device: LAMP,
            on: true,
        }],
    ));

    // The motion sensor reports occupancy; advancing pumps tick → event → rule.
    c.report(MOTION_NODE, EP, OCCUPANCY, 0x0000, json!(1));
    engine.advance(0);

    let invoked = c.invoked();
    assert_eq!(invoked.len(), 1);
    assert_eq!(invoked[0], (LAMP_NODE, EP, ClusterCommand::OnOff(true)));
}
