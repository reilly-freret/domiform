//! The zigbee2mqtt adapter: canonical events/commands ↔ z2m MQTT JSON.
//!
//! The deterministic engine core is synchronous, but MQTT is asynchronous and
//! network-driven. The seam between them is [`MqttTransport`]: the adapter only
//! ever *publishes* and *polls* through it. A real implementation
//! ([`RumqttcTransport`], behind the `mqtt` feature) runs the network on a
//! background thread and hands inbound messages back via `poll`; tests use an
//! in-memory transport. Either way the protocol translation — the interesting,
//! bug-prone part — is pure and exercised without a broker.
//!
//! ```text
//!   z2m publish ─▶ MqttTransport::poll ─▶ message_to_events ─▶ Event (tick)
//!   Command ─▶ command_to_publish ─▶ MqttTransport::publish ─▶ z2m /set
//! ```

use std::collections::HashMap;

use serde_json::{json, Value};

use super::{level_to_pct, pct_to_level, Adapter, DispatchOutcome};
use crate::ids::{ActionId, DeviceId};
use crate::model::{CapabilityKind, CapabilityState, Command, Event, Millis};

/// One MQTT message, transport-agnostic.
#[derive(Clone, Debug)]
pub struct MqttMessage {
    pub topic: String,
    pub payload: Vec<u8>,
}

/// The seam between the synchronous engine and an asynchronous MQTT client.
///
/// Not required to be `Send`: the engine is single-threaded, and a real
/// transport keeps its own network thread internally, exposing only this
/// non-blocking publish/poll surface.
pub trait MqttTransport {
    /// Publish a retained-or-not message. `Err` signals a transient failure
    /// (the adapter turns it into a retryable `DispatchOutcome::Transient`).
    fn publish(&mut self, topic: &str, payload: &[u8]) -> Result<(), String>;

    /// Return every message received since the last call (non-blocking).
    fn poll(&mut self) -> Vec<MqttMessage>;
}

/// The z2m attribute name to read for a capability, for startup state priming.
/// `None` capabilities are either synthetic (clock) or event/sleepy attributes
/// that don't answer a `/get` reliably (occupancy/battery are reported on the
/// device's own schedule, so priming them just produces broker-side warnings).
fn read_attr(cap: CapabilityKind) -> Option<&'static str> {
    match cap {
        CapabilityKind::Switch => Some("state"),
        CapabilityKind::Brightness => Some("brightness"),
        CapabilityKind::ColorTemperature => Some("color_temp"),
        CapabilityKind::Occupancy
        | CapabilityKind::Battery
        | CapabilityKind::TimeOfDay
        | CapabilityKind::SunUp => None,
    }
}

/// Adapter that bridges a set of zigbee2mqtt devices to the engine.
pub struct Zigbee2MqttAdapter {
    base_topic: String,
    by_id: HashMap<DeviceId, String>,
    by_name: HashMap<String, DeviceId>,
    /// Per device, the raw z2m `action` string → the declared event's `ActionId`.
    actions: HashMap<DeviceId, HashMap<String, ActionId>>,
    transport: Box<dyn MqttTransport>,
    /// `<base>/<friendly>/get` publishes that prompt z2m to report each device's
    /// current state on connect, sent once on the first `tick`. Without this, a
    /// condition that reads a device's state stays `Unknown` until the device
    /// happens to change on its own — so state-gated rules silently never fire.
    prime_requests: Vec<(String, Vec<u8>)>,
    primed: bool,
}

impl Zigbee2MqttAdapter {
    /// `devices` maps each `DeviceId` to its z2m friendly_name (the `address`).
    /// `events` declares the raw `action` strings each device can emit and the
    /// `ActionId` each resolves to (from the device's `events:` config).
    /// `capabilities` is each device's declared capabilities, used to build the
    /// startup `/get` requests that prime device state (see `prime_requests`).
    pub fn new(
        base_topic: impl Into<String>,
        devices: impl IntoIterator<Item = (DeviceId, String)>,
        events: impl IntoIterator<Item = (DeviceId, String, ActionId)>,
        capabilities: impl IntoIterator<Item = (DeviceId, Vec<CapabilityKind>)>,
        transport: Box<dyn MqttTransport>,
    ) -> Self {
        let base_topic = base_topic.into();
        let mut by_id = HashMap::new();
        let mut by_name = HashMap::new();
        for (id, name) in devices {
            by_name.insert(name.clone(), id);
            by_id.insert(id, name);
        }
        let mut actions: HashMap<DeviceId, HashMap<String, ActionId>> = HashMap::new();
        for (id, raw, action) in events {
            actions.entry(id).or_default().insert(raw, action);
        }

        // Precompute one `/get` per device that has at least one readable
        // capability. The payload reads all of them at once: `{"state":"", ...}`.
        let mut prime_requests = Vec::new();
        for (id, caps) in capabilities {
            let Some(friendly) = by_id.get(&id) else {
                continue;
            };
            let attrs: Vec<&str> = caps.iter().copied().filter_map(read_attr).collect();
            if attrs.is_empty() {
                continue;
            }
            let body: Value = attrs
                .into_iter()
                .map(|a| (a.to_string(), json!("")))
                .collect();
            let topic = format!("{base_topic}/{friendly}/get");
            if let Ok(payload) = serde_json::to_vec(&body) {
                prime_requests.push((topic, payload));
            }
        }

        Zigbee2MqttAdapter {
            base_topic,
            by_id,
            by_name,
            actions,
            transport,
            prime_requests,
            primed: false,
        }
    }
}

impl Adapter for Zigbee2MqttAdapter {
    fn dispatch(&mut self, cmd: &Command, _now: Millis) -> DispatchOutcome {
        let Some(device) = cmd.target_device() else {
            return DispatchOutcome::Permanent("command has no target device".into());
        };
        let Some(friendly) = self.by_id.get(&device) else {
            return DispatchOutcome::Permanent("device is not managed by this adapter".into());
        };
        let Some((topic, payload)) = command_to_publish(&self.base_topic, friendly, cmd) else {
            return DispatchOutcome::Permanent("command unsupported by zigbee2mqtt".into());
        };
        match self.transport.publish(&topic, &payload) {
            Ok(()) => DispatchOutcome::ok(),
            Err(e) => DispatchOutcome::Transient(e),
        }
    }

    fn tick(&mut self, _now: Millis) -> Vec<Event> {
        // On the first tick (engine boot), prompt every device to report its
        // current state so conditions don't start out `Unknown`. Best-effort: a
        // failed publish just means that device stays unknown until it next
        // reports, exactly as before this priming existed.
        if !self.primed {
            self.primed = true;
            for (topic, payload) in &self.prime_requests {
                let _ = self.transport.publish(topic, payload);
            }
        }

        let messages = self.transport.poll();
        let mut events = Vec::new();
        for msg in messages {
            events.extend(message_to_events(
                &self.base_topic,
                &self.by_name,
                &self.actions,
                &msg,
            ));
        }
        events
    }
}

// --- pure translation (no transport, no engine) -----------------------------

// z2m brightness is 0..=254; our model is a 0..=100 percentage. The scaling is
// shared with the Matter adapter (same range) — see `adapters::{pct_to_level,
// level_to_pct}`.

/// Translate a canonical command into a z2m `<base>/<friendly>/set` publish.
/// Returns `None` for commands not addressed to a device (scenes, timers).
pub fn command_to_publish(base: &str, friendly: &str, cmd: &Command) -> Option<(String, Vec<u8>)> {
    let body: Value = match cmd {
        Command::SetSwitch { on, .. } => json!({ "state": if *on { "ON" } else { "OFF" } }),
        Command::SetBrightness {
            value, transition, ..
        } => {
            let mut o = json!({ "brightness": pct_to_level(*value) });
            if let Some(ms) = transition {
                // z2m transition is in seconds.
                o["transition"] = json!(ms / 1000);
            }
            o
        }
        Command::ToggleSwitch { .. } => {
            json!({ "state": "TOGGLE"})
        }
        // Not addressed to a z2m device (scenes/timers). The caller turns this
        // `None` into a `Permanent` outcome, which the engine surfaces to the
        // `Observer` — so the failure is reported there, not printed here.
        _ => return None,
    };
    let topic = format!("{base}/{friendly}/set");
    Some((topic, serde_json::to_vec(&body).ok()?))
}

/// Translate an inbound z2m state publish into canonical events. Messages for
/// unknown devices, sub-topics (`/set`, `/get`), or the bridge are ignored.
pub fn message_to_events(
    base: &str,
    by_name: &HashMap<String, DeviceId>,
    actions: &HashMap<DeviceId, HashMap<String, ActionId>>,
    msg: &MqttMessage,
) -> Vec<Event> {
    let prefix = format!("{base}/");
    let Some(friendly) = msg.topic.strip_prefix(&prefix) else {
        return Vec::new();
    };
    if friendly.contains('/') {
        return Vec::new(); // e.g. <friendly>/set echoes, or bridge/... topics
    }
    let Some(&device) = by_name.get(friendly) else {
        return Vec::new();
    };
    let Ok(json) = serde_json::from_slice::<Value>(&msg.payload) else {
        return Vec::new();
    };

    let mut events = Vec::new();
    if let Some(state) = json.get("state").and_then(Value::as_str) {
        events.push(Event::StateReported {
            device,
            state: CapabilityState::Switch(state.eq_ignore_ascii_case("ON")),
        });
    }
    if let Some(b) = json.get("brightness").and_then(Value::as_u64) {
        events.push(Event::StateReported {
            device,
            state: CapabilityState::Brightness(level_to_pct(b)),
        });
    }
    if let Some(occupied) = json.get("occupancy").and_then(Value::as_bool) {
        events.push(Event::OccupancyChanged { device, occupied });
    }
    if let Some(bat) = json.get("battery").and_then(Value::as_u64) {
        events.push(Event::StateReported {
            device,
            state: CapabilityState::Battery(bat.min(100) as u8),
        });
    }
    // A stateless event fires only if the device declared this raw `action`
    // string. Exact match — unknown actions (z2m's empty `""` between presses,
    // undeclared buttons) are ignored.
    if let Some(action) = json.get("action").and_then(Value::as_str) {
        if let Some(&id) = actions.get(&device).and_then(|m| m.get(action)) {
            events.push(Event::Action { device, action: id });
        }
    }
    events
}

// --- real transport ---------------------------------------------------------

mod rumqttc_transport {
    use std::sync::mpsc::{self, Receiver};
    use std::thread;
    use std::time::Duration;

    use rumqttc::{Client, Event as MqttEvent, MqttOptions, Packet, QoS};

    use super::{MqttMessage, MqttTransport};
    use crate::wake::Waker;

    /// A live MQTT connection. The network event loop runs on a background
    /// thread, funneling inbound publishes into a channel that `poll` drains;
    /// `publish` uses the (thread-safe) client handle directly.
    pub struct RumqttcTransport {
        client: Client,
        inbound: Receiver<MqttMessage>,
    }

    impl RumqttcTransport {
        /// Connect to `host:port` and subscribe to exactly the given topics
        /// (one per managed device, `<base_topic>/<friendly_name>`).
        ///
        /// We deliberately do *not* subscribe to `<base_topic>/#`: that makes
        /// the broker replay every retained topic, including z2m's large
        /// `bridge/*` messages (`bridge/devices` is tens of KB). Those oversized
        /// retained packets destabilize the connection into a reconnect loop that
        /// never delivers a device event. Subscribing per-device avoids them and
        /// loses nothing — events for unmanaged devices were ignored anyway.
        ///
        /// `waker`, if present, is signaled whenever an inbound message is queued,
        /// so a real-time host blocked on a [`Waker`] wakes promptly instead of
        /// polling. `None` (e.g. a one-shot tool) just means no one is notified.
        pub fn connect(host: &str, port: u16, topics: &[String], waker: Option<Waker>) -> Self {
            // Unique client id per process so a stale/duplicate connection can't
            // trigger broker-side takeover flapping (same id ⇒ mosquitto kicks
            // the older connection).
            let client_id = format!("domiform-{}", std::process::id());
            let mut opts = MqttOptions::new(client_id, host, port);
            opts.set_keep_alive(Duration::from_secs(15));
            // Headroom over the largest retained device payload we expect.
            opts.set_max_packet_size(1024 * 1024, 1024 * 1024);

            let (client, mut connection) = Client::new(opts, 64);
            for topic in topics {
                let _ = client.subscribe(topic, QoS::AtMostOnce);
            }

            let (tx, inbound) = mpsc::channel();
            thread::spawn(move || {
                for event in connection.iter() {
                    match event {
                        Ok(MqttEvent::Incoming(Packet::Publish(p))) => {
                            let _ = tx.send(MqttMessage {
                                topic: p.topic,
                                payload: p.payload.to_vec(),
                            });
                            // Nudge the host: there's a message waiting for `poll`.
                            if let Some(w) = &waker {
                                w.wake();
                            }
                        }
                        // The connection self-reconnects; surface errors so a
                        // broker problem isn't silently swallowed.
                        Err(e) => eprintln!("[mqtt] connection error: {e:?}"),
                        Ok(_) => {}
                    }
                }
            });

            RumqttcTransport { client, inbound }
        }
    }

    impl MqttTransport for RumqttcTransport {
        fn publish(&mut self, topic: &str, payload: &[u8]) -> Result<(), String> {
            self.client
                .try_publish(topic, QoS::AtLeastOnce, false, payload)
                .map_err(|e| e.to_string())
        }

        fn poll(&mut self) -> Vec<MqttMessage> {
            self.inbound.try_iter().collect()
        }
    }
}

pub use rumqttc_transport::RumqttcTransport;
