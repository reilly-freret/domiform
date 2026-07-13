//! Feature C: expanded valued-sensor capabilities.
//!
//! Two halves: (1) the z2m *fold* path translates each protocol-native sensor
//! report into the canonical-unit `CapabilityState`, driven through the adapter's
//! `tick` with an in-memory transport — no broker; (2) the compiled-config path
//! proves a rule can gate on a folded sensor value (numeric via `compare`,
//! bool via `switch`-style `BoolEquals` — here through the general `compare`
//! and a bool condition), including the "never reported → Unknown" invariant.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use domiform::ids::{ActionId, DeviceId};
use domiform::{
    build_engine, compile_str, Adapter, CapabilityKind, CapabilityState, Event, MqttMessage,
    MqttTransport, Zigbee2MqttAdapter,
};

const SENSOR: DeviceId = DeviceId(0);

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

/// An adapter with one multi-sensor device on topic `sensor_01`, no priming.
fn adapter(broker: &TestBroker) -> Zigbee2MqttAdapter {
    Zigbee2MqttAdapter::new(
        "zigbee2mqtt",
        [(SENSOR, "sensor_01".to_string(), None)],
        std::iter::empty::<(DeviceId, String, ActionId)>(),
        Vec::<(DeviceId, Vec<CapabilityKind>)>::new(),
        Box::new(broker.clone()),
    )
}

fn fold(json: &str) -> Vec<Event> {
    let broker = TestBroker::default();
    let mut a = adapter(&broker);
    broker.receive("zigbee2mqtt/sensor_01", json);
    a.tick(0)
}

fn reported(events: &[Event]) -> Vec<CapabilityState> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::StateReported { device, state } if *device == SENSOR => Some(state.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn temperature_folds_to_centidegrees() {
    // z2m reports °C as a float; canonical is centidegrees Celsius.
    let states = reported(&fold(r#"{"temperature":23.5}"#));
    assert!(states.contains(&CapabilityState::Temperature(2350)));
}

#[test]
fn subzero_temperature_is_signed() {
    let states = reported(&fold(r#"{"temperature":-4.25}"#));
    assert!(states.contains(&CapabilityState::Temperature(-425)));
}

#[test]
fn humidity_and_power_fold() {
    let states = reported(&fold(r#"{"humidity":47.8,"power":12.3}"#));
    assert!(states.contains(&CapabilityState::Humidity(48)));
    assert!(states.contains(&CapabilityState::Power(12)));
}

#[test]
fn illuminance_prefers_lux_field() {
    let states = reported(&fold(r#"{"illuminance":153,"illuminance_lux":220}"#));
    assert!(states.contains(&CapabilityState::Illuminance(220)));
}

#[test]
fn contact_is_inverted_from_z2m() {
    // z2m `contact: true` means the sensor is *closed*; canonical Contact(true)
    // means *open*. So closed→false, open→true.
    let closed = reported(&fold(r#"{"contact":true}"#));
    assert!(closed.contains(&CapabilityState::Contact(false)));
    let open = reported(&fold(r#"{"contact":false}"#));
    assert!(open.contains(&CapabilityState::Contact(true)));
}

#[test]
fn water_leak_and_smoke_fold() {
    let states = reported(&fold(r#"{"water_leak":true,"smoke":false}"#));
    assert!(states.contains(&CapabilityState::WaterLeak(true)));
    assert!(states.contains(&CapabilityState::Smoke(false)));
}

#[test]
fn power_is_primed_but_environmental_sensors_are_not() {
    let broker = TestBroker::default();
    let mut a = Zigbee2MqttAdapter::new(
        "zigbee2mqtt",
        [(SENSOR, "sensor_01".to_string(), None)],
        std::iter::empty::<(DeviceId, String, ActionId)>(),
        [(
            SENSOR,
            vec![
                CapabilityKind::Power,
                CapabilityKind::Temperature,
                CapabilityKind::Contact,
            ],
        )],
        Box::new(broker.clone()),
    );
    a.tick(0);
    let published = broker.published();
    assert_eq!(published.len(), 1, "only the mains-powered meter is primed");
    assert_eq!(published[0].0, "zigbee2mqtt/sensor_01/get");
    assert!(
        published[0].1.contains("\"power\":\"\""),
        "{}",
        published[0].1
    );
    assert!(!published[0].1.contains("temperature"));
    assert!(!published[0].1.contains("contact"));
}

// --- compiled-config gate --------------------------------------------------

/// A fan (switch) turns on when a button is pressed and temperature ≥ 25.0 °C
/// (2500 centidegrees).
const HOT: &str = r#"
adapters:
  z: { type: mock }
devices:
  thermostat: { adapter: z, capabilities: [temperature] }
  fan: { adapter: z, capabilities: [switch] }
  btn: { adapter: z, events: { press: p } }
rules:
  too_hot:
    when: { event: btn.press }
    if: { compare: { device: thermostat, capability: temperature, op: ">=", value: 2500 } }
    then: [ { turn_on: fan } ]
"#;

fn fan_fires_at(temp_centi: Option<i16>) -> bool {
    let cfg = compile_str(HOT).expect("should compile");
    let thermostat = cfg.device_id("thermostat").unwrap();
    let fan = cfg.device_id("fan").unwrap();
    let btn = cfg.device_id("btn").unwrap();
    let press = cfg.device(btn).unwrap().events[0].id;

    let mut engine = build_engine(&cfg);
    engine.start();
    if let Some(t) = temp_centi {
        engine.inject(Event::StateReported {
            device: thermostat,
            state: CapabilityState::Temperature(t),
        });
    }
    engine.inject(Event::Action {
        device: btn,
        action: press,
    });
    engine.switch_state(fan) == Some(true)
}

#[test]
fn temperature_gate_fires_above_threshold() {
    assert!(fan_fires_at(Some(2600)), "26.0°C ≥ 25.0 → fires");
    assert!(!fan_fires_at(Some(2000)), "20.0°C < 25.0 → no fire");
}

#[test]
fn temperature_gate_unknown_when_unreported() {
    assert!(
        !fan_fires_at(None),
        "temperature never reported → Unknown → must not fire"
    );
}
