//! zigbee2mqtt adapter tests, all driven by an in-memory transport — no broker,
//! no z2m, no network. Covers protocol translation in both directions and the
//! full inbound-message → rule → outbound-publish loop through the engine.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use domiform::ids::{ActionId, DeviceId};
use domiform::rule::{Condition, Rule, Trigger};
use domiform::{
    Adapter, CapabilityKind, Command, Engine, Event, MqttMessage, MqttTransport, Zigbee2MqttAdapter,
};

const MOTION: DeviceId = DeviceId(0);
const LIGHT: DeviceId = DeviceId(1);
const GANG_L1: DeviceId = DeviceId(2);
const GANG_EVENTS: DeviceId = DeviceId(3);

const GANG: &str = "gang_switch_01";

// MOTION's declared events (local name → raw z2m `action` string), as the
// compiler would intern them.
const DBL: ActionId = ActionId(0); // raw "double"
const TOP: ActionId = ActionId(1); // raw "toggle_l1"
const BOTTOM: ActionId = ActionId(2); // raw "toggle_l3"
const B2: ActionId = ActionId(3); // raw "2_double"

/// Shared in-memory broker: tests inject inbound messages and read what the
/// adapter published, while the adapter owns a clone as its transport.
#[derive(Clone, Default)]
struct TestBroker(Rc<RefCell<BrokerState>>);

#[derive(Default)]
struct BrokerState {
    published: Vec<(String, String)>,
    inbound: VecDeque<MqttMessage>,
}

impl TestBroker {
    fn receive(&self, topic: &str, payload: &str) {
        self.0.borrow_mut().inbound.push_back(MqttMessage {
            topic: topic.to_string(),
            payload: payload.as_bytes().to_vec(),
        });
    }
    fn published(&self) -> Vec<(String, String)> {
        self.0.borrow().published.clone()
    }
}

impl MqttTransport for TestBroker {
    fn publish(&mut self, topic: &str, payload: &[u8]) -> Result<(), String> {
        self.0.borrow_mut().published.push((
            topic.to_string(),
            String::from_utf8_lossy(payload).into_owned(),
        ));
        Ok(())
    }
    fn poll(&mut self) -> Vec<MqttMessage> {
        self.0.borrow_mut().inbound.drain(..).collect()
    }
}

fn adapter(broker: &TestBroker) -> Zigbee2MqttAdapter {
    // No capabilities → no startup `/get` priming, so these tests observe only
    // the publishes they trigger. Priming is covered by its own test below.
    Zigbee2MqttAdapter::new(
        "zigbee2mqtt",
        [
            (MOTION, "motion_01".to_string(), None),
            (LIGHT, "light_01".to_string(), None),
        ],
        [
            (MOTION, "double".to_string(), DBL),
            (MOTION, "toggle_l1".to_string(), TOP),
            (MOTION, "toggle_l3".to_string(), BOTTOM),
            (MOTION, "2_double".to_string(), B2),
        ],
        Vec::<(DeviceId, Vec<CapabilityKind>)>::new(),
        Box::new(broker.clone()),
    )
}

#[test]
fn primes_device_state_with_get_on_first_tick() {
    let broker = TestBroker::default();
    let mut a = Zigbee2MqttAdapter::new(
        "zigbee2mqtt",
        [
            (LIGHT, "light_01".to_string(), None),
            (MOTION, "motion_01".to_string(), None),
        ],
        std::iter::empty::<(DeviceId, String, ActionId)>(),
        [
            // A light: switch + brightness are readable → one `/get`.
            (
                LIGHT,
                vec![CapabilityKind::Switch, CapabilityKind::Brightness],
            ),
            // A sleepy sensor: occupancy/battery aren't primed → no `/get`.
            (
                MOTION,
                vec![CapabilityKind::Occupancy, CapabilityKind::Battery],
            ),
        ],
        Box::new(broker.clone()),
    );

    // First tick fires the priming publishes; later ticks must not repeat them.
    a.tick(0);
    a.tick(0);

    let published = broker.published();
    assert_eq!(published.len(), 1, "exactly one /get, only for the light");
    assert_eq!(published[0].0, "zigbee2mqtt/light_01/get");
    assert!(
        published[0].1.contains("\"state\":\"\""),
        "got {}",
        published[0].1
    );
    assert!(
        published[0].1.contains("\"brightness\":\"\""),
        "got {}",
        published[0].1
    );
}

#[test]
fn inbound_occupancy_and_battery_become_events() {
    let broker = TestBroker::default();
    let mut a = adapter(&broker);

    broker.receive(
        "zigbee2mqtt/motion_01",
        r#"{"occupancy":true,"battery":80}"#,
    );
    let events = a.tick(0);

    assert!(events.contains(&Event::StateReported {
        device: MOTION,
        state: domiform::CapabilityState::Occupancy(true),
    }));
    assert!(events.iter().any(|e| matches!(
        e,
        Event::StateReported { device: MOTION, state } if format!("{state:?}").contains("Battery(80)")
    )));
}

#[test]
fn inbound_light_state_scales_brightness() {
    let broker = TestBroker::default();
    let mut a = adapter(&broker);

    // z2m brightness 254 (max) → our 100%.
    broker.receive("zigbee2mqtt/light_01", r#"{"state":"ON","brightness":254}"#);
    let events = a.tick(0);

    assert!(events.contains(&Event::StateReported {
        device: LIGHT,
        state: domiform::CapabilityState::Switch(true),
    }));
    assert!(events.contains(&Event::StateReported {
        device: LIGHT,
        state: domiform::CapabilityState::Brightness(100),
    }));
}

#[test]
fn inbound_declared_action_becomes_event() {
    let broker = TestBroker::default();
    let mut a = adapter(&broker);

    broker.receive("zigbee2mqtt/motion_01", r#"{"action":"double"}"#);
    assert!(a.tick(0).contains(&Event::Action {
        device: MOTION,
        action: DBL
    }));
}

#[test]
fn gang_switch_paddles_map_to_distinct_declared_events() {
    // Sonoff "type 120" 3-gang decora emits per-paddle `toggle_lN`. Each declared
    // paddle resolves to its own ActionId; an *undeclared* paddle is ignored.
    let broker = TestBroker::default();
    let mut a = adapter(&broker);

    broker.receive("zigbee2mqtt/motion_01", r#"{"action":"toggle_l1"}"#);
    broker.receive("zigbee2mqtt/motion_01", r#"{"action":"toggle_l3"}"#);
    broker.receive("zigbee2mqtt/motion_01", r#"{"action":"toggle_l2"}"#); // not declared
    let events = a.tick(0);
    assert!(events.contains(&Event::Action {
        device: MOTION,
        action: TOP
    }));
    assert!(events.contains(&Event::Action {
        device: MOTION,
        action: BOTTOM
    }));
    assert_eq!(events.len(), 2, "undeclared toggle_l2 must be ignored");
}

#[test]
fn multi_button_remote_action_resolves_by_exact_string() {
    let broker = TestBroker::default();
    let mut a = adapter(&broker);

    broker.receive("zigbee2mqtt/motion_01", r#"{"action":"2_double"}"#);
    assert!(a.tick(0).contains(&Event::Action {
        device: MOTION,
        action: B2
    }));
}

#[test]
fn undeclared_action_is_ignored() {
    let broker = TestBroker::default();
    let mut a = adapter(&broker);

    broker.receive("zigbee2mqtt/motion_01", r#"{"action":"single"}"#); // not declared
    broker.receive("zigbee2mqtt/motion_01", r#"{"action":""}"#); // z2m's empty filler
    assert!(a.tick(0).is_empty());
}

#[test]
fn unknown_topics_are_ignored() {
    let broker = TestBroker::default();
    let mut a = adapter(&broker);

    broker.receive("zigbee2mqtt/light_01/set", r#"{"state":"ON"}"#); // our own echo
    broker.receive("zigbee2mqtt/bridge/state", "online"); // bridge topic
    broker.receive("zigbee2mqtt/unknown_device", r#"{"state":"ON"}"#); // not in registry
    assert!(a.tick(0).is_empty());
}

#[test]
fn outbound_commands_publish_to_set_topic() {
    let broker = TestBroker::default();
    let mut a = adapter(&broker);

    a.dispatch(
        &Command::SetSwitch {
            device: LIGHT,
            on: true,
        },
        0,
    );
    a.dispatch(
        &Command::SetBrightness {
            device: LIGHT,
            value: 50,
            transition: None,
        },
        0,
    );

    let published = broker.published();
    assert_eq!(published[0].0, "zigbee2mqtt/light_01/set");
    assert!(published[0].1.contains("\"state\":\"ON\""));
    // 50% → z2m ~127.
    assert!(
        published[1].1.contains("\"brightness\":127"),
        "got {}",
        published[1].1
    );
}

#[test]
fn multi_channel_switch_commands_and_state() {
    let broker = TestBroker::default();
    let mut a = Zigbee2MqttAdapter::new(
        "zigbee2mqtt",
        [
            (GANG_L1, GANG.to_string(), Some(1)),
            (GANG_EVENTS, GANG.to_string(), None),
        ],
        [(GANG_EVENTS, "toggle_l3".to_string(), BOTTOM)],
        [(GANG_L1, vec![CapabilityKind::Switch])],
        Box::new(broker.clone()),
    );

    a.dispatch(
        &Command::SetSwitch {
            device: GANG_L1,
            on: false,
        },
        0,
    );
    let published = broker.published();
    assert_eq!(published[0].0, format!("zigbee2mqtt/{GANG}/set"));
    assert!(
        published[0].1.contains("\"state_l1\":\"OFF\""),
        "got {}",
        published[0].1
    );

    broker.receive(
        &format!("zigbee2mqtt/{GANG}"),
        r#"{"state_l1":"ON","state_l2":"OFF","action":"toggle_l3"}"#,
    );
    let events = a.tick(0);

    assert!(events.contains(&Event::StateReported {
        device: GANG_L1,
        state: domiform::CapabilityState::Switch(true),
    }));
    assert!(events.contains(&Event::Action {
        device: GANG_EVENTS,
        action: BOTTOM,
    }));
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(
                e,
                Event::StateReported {
                    device: GANG_EVENTS,
                    ..
                }
            ))
            .count(),
        0,
        "events-only device must not inherit state_l1"
    );
}

#[test]
fn multi_channel_priming_merges_get_on_shared_topic() {
    let broker = TestBroker::default();
    let mut a = Zigbee2MqttAdapter::new(
        "zigbee2mqtt",
        [
            (GANG_L1, GANG.to_string(), Some(1)),
            (DeviceId(4), GANG.to_string(), Some(2)),
        ],
        std::iter::empty::<(DeviceId, String, ActionId)>(),
        [
            (GANG_L1, vec![CapabilityKind::Switch]),
            (DeviceId(4), vec![CapabilityKind::Switch]),
        ],
        Box::new(broker.clone()),
    );

    a.tick(0);
    let published = broker.published();
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].0, format!("zigbee2mqtt/{GANG}/get"));
    assert!(
        published[0].1.contains("\"state_l1\":\"\""),
        "got {}",
        published[0].1
    );
    assert!(
        published[0].1.contains("\"state_l2\":\"\""),
        "got {}",
        published[0].1
    );
}

#[test]
fn outbound_color_and_color_temp_publish() {
    let broker = TestBroker::default();
    let mut a = adapter(&broker);

    a.dispatch(
        &Command::SetColor {
            device: LIGHT,
            r: 255,
            g: 107,
            b: 71,
            // Sub-second fade: must survive as a fractional-second transition,
            // not truncate to 0 (z2m accepts floats).
            transition: Some(500),
        },
        0,
    );
    a.dispatch(
        &Command::SetColorTemperature {
            device: LIGHT,
            mireds: 370,
            transition: None,
        },
        0,
    );

    let published = broker.published();
    assert!(
        published[0].1.contains("\"r\":255")
            && published[0].1.contains("\"g\":107")
            && published[0].1.contains("\"b\":71"),
        "got {}",
        published[0].1
    );
    assert!(
        published[0].1.contains("\"transition\":0.5"),
        "got {}",
        published[0].1
    );
    assert!(
        published[1].1.contains("\"color_temp\":370"),
        "got {}",
        published[1].1
    );
}

#[test]
fn outbound_send_ir_code_publish() {
    let broker = TestBroker::default();
    let mut a = adapter(&broker);

    a.dispatch(
        &Command::SendIrCode {
            device: LIGHT,
            code: "BW4jahFCAuAXAQGMBsADAHLgAgvAE4AH4BcBwCeAB+AFRw8vm24jqwhCAv//biOrCEIC".into(),
        },
        0,
    );

    let published = broker.published();
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].0, "zigbee2mqtt/light_01/set");
    assert!(
        published[0].1.contains("\"ir_code_to_send\":\"BW4jahFCAuAXAQGMBsADAHLgAgvAE4AH4BcBwCeAB+AFRw8vm24jqwhCAv//biOrCEIC\""),
        "got {}",
        published[0].1
    );
}

#[test]
fn inbound_color_and_color_temp_become_events() {
    let broker = TestBroker::default();
    let mut a = adapter(&broker);

    broker.receive(
        "zigbee2mqtt/light_01",
        r#"{"color":{"r":46,"g":102,"b":150},"color_temp":325}"#,
    );
    let events = a.tick(0);

    assert!(events.contains(&Event::StateReported {
        device: LIGHT,
        state: domiform::CapabilityState::Color {
            r: 46,
            g: 102,
            b: 150
        },
    }));
    assert!(events.contains(&Event::StateReported {
        device: LIGHT,
        state: domiform::CapabilityState::ColorTemperature(325),
    }));
}

#[test]
fn commanding_an_unmanaged_device_is_permanent_failure() {
    let broker = TestBroker::default();
    let mut a = adapter(&broker);

    let outcome = a.dispatch(
        &Command::SetSwitch {
            device: DeviceId(999),
            on: true,
        },
        0,
    );
    assert!(matches!(outcome, domiform::DispatchOutcome::Permanent(_)));
    assert!(broker.published().is_empty());
}

#[test]
fn full_loop_inbound_message_to_outbound_publish() {
    // A real z2m message arrives, a rule fires, and the resulting command is
    // published back out — the whole adapter↔engine round trip, no broker.
    let broker = TestBroker::default();

    let mut engine = Engine::new();
    let idx = engine.add_adapter(Box::new(adapter(&broker)));
    engine.bind_device(MOTION, idx);
    engine.bind_device(LIGHT, idx);
    engine.add_rule(Rule::new(
        domiform::RuleId(0),
        Trigger::Changed {
            device: MOTION,
            kind: CapabilityKind::Occupancy,
            to: true,
        },
        Condition::Always,
        vec![Command::SetSwitch {
            device: LIGHT,
            on: true,
        }],
    ));

    // z2m reports motion; advancing pumps the adapter's tick → event → rule.
    broker.receive("zigbee2mqtt/motion_01", r#"{"occupancy":true}"#);
    engine.advance(0);

    let published = broker.published();
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].0, "zigbee2mqtt/light_01/set");
    assert!(published[0].1.contains("\"state\":\"ON\""));
}
