//! Z-Wave JS adapter tests, all driven by an in-memory fake client — no
//! zwave-js-server, no WebSocket, no network. Covers protocol translation in
//! both directions (including multi-endpoint nodes) and the full value-report →
//! rule → set-value loop through the engine. The structural twin of
//! `tests/matter.rs`.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use serde_json::{json, Value};

use domiform::adapters::zwavejs::{EndpointId, NodeId};
use domiform::ids::{ActionId, DeviceId, RuleId};
use domiform::rule::{Condition, Rule, Trigger};
use domiform::{
    Adapter, CapabilityState, Command, DeviceKind, Engine, Event, SetValue, ValueUpdate,
    ZwaveAdapter, ZwaveClient,
};

const REMOTE: DeviceId = DeviceId(0); // a Central Scene wall dimmer / scene controller
const BULB: DeviceId = DeviceId(1); // a dimmable bulb
const RELAY: DeviceId = DeviceId(2); // a non-dimmable on/off switch
const SENSOR: DeviceId = DeviceId(3); // a motion + battery sensor
const DUAL_A: DeviceId = DeviceId(4); // relay 1 of a multi-relay module (one node)
const DUAL_B: DeviceId = DeviceId(5); // relay 2 of the same module (same node)

// Included Z-Wave node ids (the `address` from config).
const REMOTE_NODE: NodeId = NodeId(5);
const BULB_NODE: NodeId = NodeId(8);
const RELAY_NODE: NodeId = NodeId(9);
const SENSOR_NODE: NodeId = NodeId(12);
const DUAL_NODE: NodeId = NodeId(15); // one node, two endpoints → two devices

// Endpoints. Ordinary single-load devices live on the root; a multi-load module
// puts each independent load on its own endpoint under one node.
const ROOT: EndpointId = EndpointId(0);
const EP1: EndpointId = EndpointId(1);
const EP2: EndpointId = EndpointId(2);

// REMOTE's declared Central Scene events (local name → raw "<button>:<attr>"),
// as the compiler would intern them.
const UP_SINGLE: ActionId = ActionId(0); // raw "1:KeyPressed"
const UP_DOUBLE: ActionId = ActionId(1); // raw "1:KeyPressed2x"
const DOWN_HOLD: ActionId = ActionId(2); // raw "2:KeyHeldDown"

// Command classes the tests exercise (decimal).
const BINARY_SWITCH: u16 = 0x25;
const MULTILEVEL_SWITCH: u16 = 0x26;
const CENTRAL_SCENE: u16 = 0x5B;
const NOTIFICATION: u16 = 0x71;
const BATTERY: u16 = 0x80;

/// Shared in-memory client: tests feed inbound value updates and read back what
/// the adapter set, while the adapter owns a clone as its seam.
#[derive(Clone, Default)]
struct FakeClient(Rc<RefCell<ClientState>>);

#[derive(Default)]
struct ClientState {
    set: Vec<(NodeId, EndpointId, SetValue)>,
    inbound: VecDeque<ValueUpdate>,
}

impl FakeClient {
    /// Feed a stateful `value updated` on a specific endpoint.
    fn update_at(
        &self,
        node: NodeId,
        endpoint: EndpointId,
        command_class: u16,
        property: &str,
        value: Value,
    ) {
        self.0.borrow_mut().inbound.push_back(ValueUpdate {
            node,
            endpoint,
            command_class,
            property: property.to_string(),
            property_key: None,
            value,
            notification: false,
        });
    }

    /// Feed a stateful `value updated` on the root endpoint (the common case).
    fn update(&self, node: NodeId, command_class: u16, property: &str, value: Value) {
        self.update_at(node, ROOT, command_class, property, value);
    }

    /// Feed a Central Scene `value notification` (a stateless button press). Scene
    /// controllers report on the root endpoint.
    fn scene(&self, node: NodeId, button: &str, key_attribute: u64) {
        self.0.borrow_mut().inbound.push_back(ValueUpdate {
            node,
            endpoint: ROOT,
            command_class: CENTRAL_SCENE,
            property: "scene".to_string(),
            property_key: Some(button.to_string()),
            value: json!(key_attribute),
            notification: true,
        });
    }

    fn set(&self) -> Vec<(NodeId, EndpointId, SetValue)> {
        self.0.borrow().set.clone()
    }
}

impl ZwaveClient for FakeClient {
    fn set_value(
        &mut self,
        node: NodeId,
        endpoint: EndpointId,
        value: &SetValue,
    ) -> Result<(), String> {
        self.0
            .borrow_mut()
            .set
            .push((node, endpoint, value.clone()));
        Ok(())
    }
    fn poll(&mut self) -> Vec<ValueUpdate> {
        self.0.borrow_mut().inbound.drain(..).collect()
    }
}

fn adapter(client: &FakeClient) -> ZwaveAdapter {
    ZwaveAdapter::new(
        [
            (REMOTE, REMOTE_NODE, ROOT, DeviceKind::Switch),
            (BULB, BULB_NODE, ROOT, DeviceKind::Dimmer),
            (RELAY, RELAY_NODE, ROOT, DeviceKind::Switch),
            (SENSOR, SENSOR_NODE, ROOT, DeviceKind::Switch),
            // One physical multi-relay module, two loads on two endpoints.
            (DUAL_A, DUAL_NODE, EP1, DeviceKind::Switch),
            (DUAL_B, DUAL_NODE, EP2, DeviceKind::Switch),
        ],
        [
            (REMOTE, "1:KeyPressed".to_string(), UP_SINGLE),
            (REMOTE, "1:KeyPressed2x".to_string(), UP_DOUBLE),
            (REMOTE, "2:KeyHeldDown".to_string(), DOWN_HOLD),
        ],
        Box::new(client.clone()),
    )
}

#[test]
fn inbound_binary_switch_current_value_becomes_switch_event() {
    let c = FakeClient::default();
    let mut a = adapter(&c);

    c.update(RELAY_NODE, BINARY_SWITCH, "currentValue", json!(true));
    let events = a.tick(0);

    assert!(events.contains(&Event::StateReported {
        device: RELAY,
        state: domiform::CapabilityState::Switch(true),
    }));
}

#[test]
fn inbound_multilevel_current_value_scales_and_implies_switch() {
    let c = FakeClient::default();
    let mut a = adapter(&c);

    // Z-Wave level 99 (max) → our 100%, and a non-zero level implies "on".
    c.update(BULB_NODE, MULTILEVEL_SWITCH, "currentValue", json!(99));
    let events = a.tick(0);

    assert!(events.contains(&Event::StateReported {
        device: BULB,
        state: domiform::CapabilityState::Brightness(100),
    }));
    assert!(events.contains(&Event::StateReported {
        device: BULB,
        state: domiform::CapabilityState::Switch(true),
    }));

    // Level 0 → 0% and "off".
    c.update(BULB_NODE, MULTILEVEL_SWITCH, "currentValue", json!(0));
    let events = a.tick(0);
    assert!(events.contains(&Event::StateReported {
        device: BULB,
        state: domiform::CapabilityState::Switch(false),
    }));
}

#[test]
fn inbound_battery_and_occupancy_become_events() {
    let c = FakeClient::default();
    let mut a = adapter(&c);

    c.update(SENSOR_NODE, BATTERY, "level", json!(80));
    // Notification CC, Home Security: 8 = motion detected → occupied.
    c.update(SENSOR_NODE, NOTIFICATION, "Home Security", json!(8));
    let events = a.tick(0);

    assert!(events.iter().any(|e| matches!(
        e,
        Event::StateReported { device: SENSOR, state } if format!("{state:?}").contains("Battery(80)")
    )));
    assert!(events.contains(&Event::StateReported {
        device: SENSOR,
        state: CapabilityState::Occupancy(true),
    }));

    // Home Security idle (0) clears occupancy.
    c.update(SENSOR_NODE, NOTIFICATION, "Home Security", json!(0));
    assert!(a.tick(0).contains(&Event::StateReported {
        device: SENSOR,
        state: CapabilityState::Occupancy(false),
    }));
}

#[test]
fn inbound_target_value_is_ignored() {
    // Only `currentValue` (real state) folds; `targetValue` (our own commanded
    // intent echoed back) must not, or state briefly lies before confirmation.
    let c = FakeClient::default();
    let mut a = adapter(&c);

    c.update(RELAY_NODE, BINARY_SWITCH, "targetValue", json!(true));
    c.update(BULB_NODE, MULTILEVEL_SWITCH, "targetValue", json!(50));
    assert!(a.tick(0).is_empty());
}

#[test]
fn central_scene_notification_becomes_declared_event() {
    let c = FakeClient::default();
    let mut a = adapter(&c);

    // Button 1 single tap; zwave-js reports the button zero-padded ("001").
    c.scene(REMOTE_NODE, "001", 0);
    assert!(a.tick(0).contains(&Event::Action {
        device: REMOTE,
        action: UP_SINGLE
    }));

    // Button 1 double tap, button 2 held — each resolves to its own action. This
    // is how a many-buttoned scene controller (e.g. a Zooz ZEN32: 5 buttons ×
    // several gestures) is one device with many addressable events, no endpoints.
    c.scene(REMOTE_NODE, "001", 3);
    c.scene(REMOTE_NODE, "002", 2);
    let events = a.tick(0);
    assert!(events.contains(&Event::Action {
        device: REMOTE,
        action: UP_DOUBLE
    }));
    assert!(events.contains(&Event::Action {
        device: REMOTE,
        action: DOWN_HOLD
    }));
}

#[test]
fn undeclared_scene_and_non_notification_are_ignored() {
    let c = FakeClient::default();
    let mut a = adapter(&c);

    // Button 3 was never declared on REMOTE.
    c.scene(REMOTE_NODE, "003", 0);
    // A scene value that arrives as a stateful `value updated` (the init/replay
    // on reconnect) must NOT fire — only real notifications do.
    c.0.borrow_mut().inbound.push_back(ValueUpdate {
        node: REMOTE_NODE,
        endpoint: ROOT,
        command_class: CENTRAL_SCENE,
        property: "scene".to_string(),
        property_key: Some("001".to_string()),
        value: json!(0),
        notification: false,
    });
    assert!(a.tick(0).is_empty());
}

#[test]
fn inbound_from_unknown_node_is_ignored() {
    let c = FakeClient::default();
    let mut a = adapter(&c);

    c.update(NodeId(999), BINARY_SWITCH, "currentValue", json!(true));
    c.scene(NodeId(999), "001", 0);
    assert!(a.tick(0).is_empty());
}

#[test]
fn multi_endpoint_reports_route_to_distinct_devices() {
    // Two loads on one node, one per endpoint, must fold onto their own device —
    // never collide because they share a node id.
    let c = FakeClient::default();
    let mut a = adapter(&c);

    c.update_at(DUAL_NODE, EP1, BINARY_SWITCH, "currentValue", json!(true));
    c.update_at(DUAL_NODE, EP2, BINARY_SWITCH, "currentValue", json!(false));
    let events = a.tick(0);

    assert!(events.contains(&Event::StateReported {
        device: DUAL_A,
        state: domiform::CapabilityState::Switch(true),
    }));
    assert!(events.contains(&Event::StateReported {
        device: DUAL_B,
        state: domiform::CapabilityState::Switch(false),
    }));

    // A report on an endpoint no device claims (the module's root) is ignored.
    c.update_at(DUAL_NODE, ROOT, BINARY_SWITCH, "currentValue", json!(true));
    assert!(a.tick(0).is_empty());
}

#[test]
fn multi_endpoint_commands_carry_the_endpoint() {
    // Commanding each load targets the same node but its own endpoint.
    let c = FakeClient::default();
    let mut a = adapter(&c);

    a.dispatch(
        &Command::SetSwitch {
            device: DUAL_A,
            on: true,
        },
        0,
    );
    a.dispatch(
        &Command::SetSwitch {
            device: DUAL_B,
            on: false,
        },
        0,
    );

    let set = c.set();
    assert_eq!(set[0].0, DUAL_NODE);
    assert_eq!(set[0].1, EP1);
    assert_eq!(set[0].2.value, json!(true));
    assert_eq!(set[1].0, DUAL_NODE);
    assert_eq!(set[1].1, EP2);
    assert_eq!(set[1].2.value, json!(false));
}

#[test]
fn outbound_switch_targets_the_right_command_class() {
    let c = FakeClient::default();
    let mut a = adapter(&c);

    // A dimmer's on is a Multilevel level (255 = restore last), off is 0.
    a.dispatch(
        &Command::SetSwitch {
            device: BULB,
            on: true,
        },
        0,
    );
    a.dispatch(
        &Command::SetSwitch {
            device: BULB,
            on: false,
        },
        0,
    );
    // A relay's on/off is a Binary Switch boolean.
    a.dispatch(
        &Command::SetSwitch {
            device: RELAY,
            on: true,
        },
        0,
    );

    let set = c.set();
    assert_eq!(
        set[0],
        (
            BULB_NODE,
            ROOT,
            SetValue {
                command_class: MULTILEVEL_SWITCH,
                property: "targetValue".into(),
                value: json!(255),
                transition: None,
            }
        )
    );
    assert_eq!(
        set[1],
        (
            BULB_NODE,
            ROOT,
            SetValue {
                command_class: MULTILEVEL_SWITCH,
                property: "targetValue".into(),
                value: json!(0),
                transition: None,
            }
        )
    );
    assert_eq!(
        set[2],
        (
            RELAY_NODE,
            ROOT,
            SetValue {
                command_class: BINARY_SWITCH,
                property: "targetValue".into(),
                value: json!(true),
                transition: None,
            }
        )
    );
}

#[test]
fn outbound_brightness_scales_to_multilevel_with_transition() {
    let c = FakeClient::default();
    let mut a = adapter(&c);

    // 100% → Z-Wave level 99; the fade is carried through for the transport.
    a.dispatch(
        &Command::SetBrightness {
            device: BULB,
            value: 100,
            transition: Some(2000),
        },
        0,
    );

    let set = c.set();
    assert_eq!(
        set[0],
        (
            BULB_NODE,
            ROOT,
            SetValue {
                command_class: MULTILEVEL_SWITCH,
                property: "targetValue".into(),
                value: json!(99),
                transition: Some(2000),
            }
        )
    );
}

#[test]
fn outbound_color_targets_color_switch() {
    let c = FakeClient::default();
    let mut a = adapter(&c);

    a.dispatch(
        &Command::SetColor {
            device: BULB,
            r: 255,
            g: 0,
            b: 128,
            transition: None,
        },
        0,
    );

    let set = c.set();
    assert_eq!(
        set[0],
        (
            BULB_NODE,
            ROOT,
            SetValue {
                command_class: 0x33,
                property: "targetColor".into(),
                value: json!({ "red": 255, "green": 0, "blue": 128 }),
                transition: None,
            }
        )
    );
}

#[test]
fn outbound_color_temperature_targets_warm_cold_mix() {
    let c = FakeClient::default();
    let mut a = adapter(&c);

    // 2700 K ≈ 370 mireds → warm white, no cold.
    a.dispatch(
        &Command::SetColorTemperature {
            device: BULB,
            mireds: 370,
            transition: None,
        },
        0,
    );

    let set = c.set();
    assert_eq!(set[0].2.value, json!({ "warmWhite": 255, "coldWhite": 0 }));
}

#[test]
fn inbound_current_color_becomes_color_event() {
    let c = FakeClient::default();
    let mut a = adapter(&c);

    c.update(
        BULB_NODE,
        0x33,
        "currentColor",
        json!({ "red": 10, "green": 20, "blue": 30 }),
    );
    let events = a.tick(0);
    assert!(events.contains(&Event::StateReported {
        device: BULB,
        state: domiform::CapabilityState::Color {
            r: 10,
            g: 20,
            b: 30
        },
    }));
}

#[test]
fn raw_toggle_is_unsupported() {
    // Z-Wave has no toggle; a raw toggle only reaches an adapter when switch
    // state is unknown (the engine resolves it to SetSwitch otherwise).
    let c = FakeClient::default();
    let mut a = adapter(&c);

    let outcome = a.dispatch(&Command::ToggleSwitch { device: RELAY }, 0);
    assert!(matches!(outcome, domiform::DispatchOutcome::Permanent(_)));
    assert!(c.set().is_empty());
}

#[test]
fn commanding_an_unmanaged_device_is_permanent_failure() {
    let c = FakeClient::default();
    let mut a = adapter(&c);

    let outcome = a.dispatch(
        &Command::SetSwitch {
            device: DeviceId(999),
            on: true,
        },
        0,
    );
    assert!(matches!(outcome, domiform::DispatchOutcome::Permanent(_)));
    assert!(c.set().is_empty());
}

#[test]
fn full_loop_scene_press_to_set_value() {
    // A real Central Scene press arrives, a rule fires, and the resulting command
    // is set back out — the whole adapter↔engine round trip, no server.
    let c = FakeClient::default();

    let mut engine = Engine::new();
    let idx = engine.add_adapter(Box::new(adapter(&c)));
    engine.bind_device(REMOTE, idx);
    engine.bind_device(BULB, idx);
    engine.add_rule(Rule::new(
        RuleId(0),
        Trigger::Action {
            device: REMOTE,
            action: UP_SINGLE,
        },
        Condition::Always,
        vec![Command::SetSwitch {
            device: BULB,
            on: true,
        }],
    ));

    // The scene controller reports a button press; advancing pumps the adapter's
    // tick → event → rule.
    c.scene(REMOTE_NODE, "001", 0);
    engine.advance(0);

    let set = c.set();
    assert_eq!(set.len(), 1);
    assert_eq!(set[0].0, BULB_NODE);
    assert_eq!(set[0].1, ROOT);
    // BULB is a dimmer, so "on" is Multilevel 255 (restore last level).
    assert_eq!(set[0].2.value, json!(255));
}
