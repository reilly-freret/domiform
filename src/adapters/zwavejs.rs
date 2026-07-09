//! The Z-Wave JS adapter: canonical events/commands ↔ a `zwave-js-server`.
//!
//! Structurally this is the Matter adapter with Z-Wave nouns. The deterministic
//! engine core is synchronous, but `zwave-js-server` is asynchronous and
//! network-driven. The seam between them is [`ZwaveClient`]: the adapter only
//! ever *sets values* and *polls* for value reports through it. A real
//! implementation ([`ZwaveServerWs`], behind no feature — the network sits behind
//! this trait, always compiled) runs a WebSocket on a background thread and hands
//! inbound reports back via `poll`; tests use an in-memory fake. Either way the
//! protocol translation — the interesting, bug-prone part — is pure and exercised
//! without a server.
//!
//! ```text
//!   value report ─▶ ZwaveClient::poll ─▶ update_to_events ─▶ Event (tick)
//!   Command ─▶ command_to_set_value ─▶ ZwaveClient::set_value ─▶ node.set_value
//! ```
//!
//! Z-Wave has no cluster/attribute model: everything is a *value*, addressed by
//! `(command class, property)` on a node (see the CC ids in [`update_to_events`]).
//! Two shapes matter here:
//!
//! * **stateful values** (`currentValue`, battery `level`) arrive as zwave-js
//!   `value updated` events and are folded into the state store, and
//! * **stateless notifications** (a Central Scene button press) arrive as `value
//!   notification` events. The [`ValueUpdate::notification`] flag keeps the two
//!   apart so a scene button only fires on a real press — never on the
//!   `value updated`/`value added` that replays on (re)connect.

use std::collections::HashMap;

use serde_json::{json, Value};

use super::{Adapter, DispatchOutcome};
use crate::ids::{ActionId, DeviceId};
use crate::model::{CapabilityKind, CapabilityState, Command, Event, Millis};

/// A Z-Wave node id (assigned when the device is included — the `address` in
/// config). A newtype, not a bare `u32`, so it can't be transposed with an
/// [`EndpointId`] and so it can key a `HashMap`. Compare [`NodeId`](super::NodeId)
/// for Matter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NodeId(pub u32);

/// One endpoint of a node (the `endpoint` in config, default `0` = the root).
///
/// Most Z-Wave devices are a single load on the root endpoint, but a multi-load
/// module — a multi-relay, a dual dimmer, a metered power strip — exposes each
/// independent load on its *own* endpoint under one node id. Each such load is a
/// separate domiform device: same `address`, different `endpoint`. Central Scene
/// controllers (the Zooz ZEN32, most wall remotes) are *not* multi-endpoint —
/// their buttons are scene numbers on the root, disambiguated by `events:`, not
/// by endpoints.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EndpointId(pub u16);

/// Whether a device is driven through the Binary Switch CC or the Multilevel
/// Switch CC — the one thing an outbound `SetSwitch` needs that the command
/// itself doesn't carry. Derived from the device's declared capabilities:
/// anything with `brightness` is a [`DeviceKind::Dimmer`].
///
/// Z-Wave, unlike Matter, has no single "OnOff" that a controller routes for us:
/// a plain relay speaks Binary Switch (CC 0x25), a dimmer speaks Multilevel
/// Switch (CC 0x26), and turning either "on" is a *different value* on a
/// *different command class*. So the kind has to travel with the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceKind {
    Switch,
    Dimmer,
}

impl DeviceKind {
    /// A device is a dimmer iff it declares the `brightness` capability.
    pub fn from_capabilities(caps: &[CapabilityKind]) -> DeviceKind {
        if caps.contains(&CapabilityKind::Brightness) {
            DeviceKind::Dimmer
        } else {
            DeviceKind::Switch
        }
    }
}

/// One inbound value report (the unit of [`ZwaveClient::poll`]). `value` stays a
/// raw JSON value; turning it into a typed [`CapabilityState`] or an
/// [`Event::Action`] is [`update_to_events`]' job. The analogue of Matter's
/// `AttrReport`, but carrying Z-Wave's `(command_class, property, property_key)`
/// value address instead of `(cluster, attribute)`.
#[derive(Clone, Debug)]
pub struct ValueUpdate {
    pub node: NodeId,
    /// The endpoint the value lives on (`0` = root). Reports that omit an
    /// endpoint are treated as root by the transport.
    pub endpoint: EndpointId,
    pub command_class: u16,
    /// e.g. `"currentValue"`, `"targetValue"`, `"scene"`, `"level"`.
    pub property: String,
    /// The value's sub-key, when it has one — for Central Scene this is the
    /// button number zwave-js reports (`"001"`). `None` for simple values.
    pub property_key: Option<String>,
    pub value: Value,
    /// `true` for a stateless zwave-js `value notification` (a Central Scene
    /// press); `false` for a stateful `value updated` / snapshot value. Central
    /// Scene events fire only when this is `true`, so the value-init that replays
    /// on (re)connect can't spuriously re-fire a button.
    pub notification: bool,
}

/// One lowered `node.set_value`, ready for [`ZwaveClient::set_value`]. Small and
/// protocol-shaped — the analogue of Matter's `ClusterCommand`. Every command we
/// send writes `targetValue` on a command class; only the class and value differ.
#[derive(Clone, Debug, PartialEq)]
pub struct SetValue {
    pub command_class: u16,
    /// Always `"targetValue"` for the commands we lower, but named so the real
    /// transport builds the value id generically.
    pub property: String,
    pub value: Value,
    /// Fade duration, dimmers only. The transport passes it as the Multilevel
    /// Switch `transitionDuration` option; `None` means the device's default.
    pub transition: Option<Millis>,
}

/// The seam between the synchronous engine and an asynchronous `zwave-js-server`.
/// Compare [`MatterController`](super::MatterController): act outward, drain
/// inward, nothing else.
///
/// Not required to be `Send`: the engine is single-threaded, and a real client
/// keeps its own network thread internally, exposing only this non-blocking
/// set/poll surface.
pub trait ZwaveClient {
    /// Write a value on a node's endpoint. `Err` signals a transient failure (the
    /// adapter turns it into a retryable `DispatchOutcome::Transient`).
    fn set_value(
        &mut self,
        node: NodeId,
        endpoint: EndpointId,
        value: &SetValue,
    ) -> Result<(), String>;

    /// Return every value report received since the last call (non-blocking).
    fn poll(&mut self) -> Vec<ValueUpdate>;
}

/// Adapter that bridges a set of Z-Wave nodes to the engine.
pub struct ZwaveAdapter {
    /// node + endpoint + kind for an outbound command's target device.
    by_id: HashMap<DeviceId, (NodeId, EndpointId, DeviceKind)>,
    /// reverse lookup for an inbound value report, by `(node, endpoint)`. A
    /// single-load device is one entry on the root endpoint; a multi-load module
    /// is several entries under one node, one per endpoint.
    by_node: HashMap<(NodeId, EndpointId), DeviceId>,
    /// Per device, a Central Scene raw event string (`"<button>:<attribute>"`,
    /// e.g. `"1:KeyPressed"`) → the declared event's `ActionId`.
    actions: HashMap<DeviceId, HashMap<String, ActionId>>,
    client: Box<dyn ZwaveClient>,
}

impl ZwaveAdapter {
    /// `devices` maps each `DeviceId` to its included `NodeId` (the `address`),
    /// its [`EndpointId`] (the `endpoint`, default `0` = root), and its
    /// [`DeviceKind`] (from its capabilities). `events` declares the raw Central
    /// Scene strings each device can emit and the `ActionId` each resolves to
    /// (from the device's `events:` config).
    pub fn new(
        devices: impl IntoIterator<Item = (DeviceId, NodeId, EndpointId, DeviceKind)>,
        events: impl IntoIterator<Item = (DeviceId, String, ActionId)>,
        client: Box<dyn ZwaveClient>,
    ) -> Self {
        let mut by_id = HashMap::new();
        let mut by_node = HashMap::new();
        for (id, node, endpoint, kind) in devices {
            by_id.insert(id, (node, endpoint, kind));
            by_node.insert((node, endpoint), id);
        }
        let mut actions: HashMap<DeviceId, HashMap<String, ActionId>> = HashMap::new();
        for (id, raw, action) in events {
            actions.entry(id).or_default().insert(raw, action);
        }
        ZwaveAdapter {
            by_id,
            by_node,
            actions,
            client,
        }
    }
}

impl Adapter for ZwaveAdapter {
    fn dispatch(&mut self, cmd: &Command, _now: Millis) -> DispatchOutcome {
        let Some(device) = cmd.target_device() else {
            return DispatchOutcome::Permanent("command has no target device".into());
        };
        let Some(&(node, endpoint, kind)) = self.by_id.get(&device) else {
            return DispatchOutcome::Permanent("device is not managed by this adapter".into());
        };
        let Some(set) = command_to_set_value(kind, cmd) else {
            return DispatchOutcome::Permanent("command unsupported by z-wave".into());
        };
        match self.client.set_value(node, endpoint, &set) {
            Ok(()) => DispatchOutcome::ok(),
            Err(e) => DispatchOutcome::Transient(e),
        }
    }

    fn tick(&mut self, _now: Millis) -> Vec<Event> {
        // Like Matter, no explicit priming: `start_listening` returns a snapshot
        // of every node's values on connect, replayed as `value updated`s through
        // this same path, so state-gated conditions start from real state.
        let updates = self.client.poll();
        let mut events = Vec::new();
        for u in &updates {
            events.extend(update_to_events(&self.by_node, &self.actions, u));
        }
        events
    }
}

// --- pure translation (no client, no engine) --------------------------------

// Z-Wave's Multilevel Switch level is 0..=99 (99 = full); our model is a 0..=100
// percentage. Distinct from the z2m/Matter 0..=254 range, so these conversions
// are local rather than shared with `adapters::{pct_to_level, level_to_pct}`.

/// A 0..=100 percentage → a 0..=99 Multilevel Switch level. Rounds to nearest.
fn pct_to_zwave(pct: u8) -> u8 {
    ((pct.min(100) as u16 * 99 + 50) / 100) as u8
}

/// Inverse of [`pct_to_zwave`]: a 0..=99 level back to a 0..=100 percentage.
fn zwave_to_pct(level: u64) -> u8 {
    ((level.min(99) * 100 + 49) / 99) as u8
}

/// "On" for a Multilevel Switch `targetValue`: `255` means "restore the last
/// known level" (the Z-Wave idiom for a plain on), so turning a dimmer on
/// returns it to its previous brightness rather than forcing 100%.
const MULTILEVEL_ON: u16 = 255;

// Command classes this adapter speaks (decimal). Named so the match arms read.
const CC_BINARY_SWITCH: u16 = 0x25; // 37
const CC_MULTILEVEL_SWITCH: u16 = 0x26; // 38
const CC_COLOR_SWITCH: u16 = 0x33; // 51
const CC_CENTRAL_SCENE: u16 = 0x5B; // 91
const CC_NOTIFICATION: u16 = 0x71; // 113
const CC_BATTERY: u16 = 0x80; // 128

/// The Central Scene "key attribute" (what happened to the button), rendered as
/// the canonical name used in a device's raw `events:` string. Values follow the
/// Z-Wave Central Scene CC; `None` for any reserved/unknown attribute.
fn key_attribute_name(value: u64) -> Option<&'static str> {
    Some(match value {
        0 => "KeyPressed",   // single tap
        1 => "KeyReleased",  // release after a hold
        2 => "KeyHeldDown",  // held
        3 => "KeyPressed2x", // double tap
        4 => "KeyPressed3x", // triple tap
        5 => "KeyPressed4x",
        6 => "KeyPressed5x",
        _ => return None,
    })
}

/// Canonical command → a `node.set_value` payload. `None` = not a device command
/// (scenes/timers never reach an adapter) or one Z-Wave can't express (a raw
/// `Toggle`, which only reaches here when switch state is unknown — the engine
/// resolves it to an explicit `SetSwitch` otherwise). `kind` selects the command
/// class an on/off targets: a relay's Binary Switch vs. a dimmer's Multilevel.
pub fn command_to_set_value(kind: DeviceKind, cmd: &Command) -> Option<SetValue> {
    match (kind, cmd) {
        // A dimmer's on/off is a Multilevel level: 255 = restore last, 0 = off.
        (DeviceKind::Dimmer, Command::SetSwitch { on, .. }) => Some(SetValue {
            command_class: CC_MULTILEVEL_SWITCH,
            property: "targetValue".into(),
            value: json!(if *on { MULTILEVEL_ON } else { 0 }),
            transition: None,
        }),
        // A relay's on/off is a Binary Switch boolean.
        (DeviceKind::Switch, Command::SetSwitch { on, .. }) => Some(SetValue {
            command_class: CC_BINARY_SWITCH,
            property: "targetValue".into(),
            value: json!(*on),
            transition: None,
        }),
        (
            _,
            Command::SetBrightness {
                value, transition, ..
            },
        ) => Some(SetValue {
            command_class: CC_MULTILEVEL_SWITCH,
            property: "targetValue".into(),
            value: json!(pct_to_zwave(*value)),
            transition: *transition,
        }),
        (
            _,
            Command::SetColor {
                r,
                g,
                b,
                transition,
                ..
            },
        ) => Some(SetValue {
            command_class: CC_COLOR_SWITCH,
            property: "targetColor".into(),
            value: json!({ "red": r, "green": g, "blue": b }),
            transition: *transition,
        }),
        (
            _,
            Command::SetColorTemperature {
                mireds, transition, ..
            },
        ) => {
            // Z-Wave Color Switch tunable-white spans two channels, so a color
            // temperature is rendered as a warm/cold mix over 2700–6500 K. The
            // config schema advertises a wider range (1000–10000 K); values
            // outside the hardware band are clamped to the nearest bound by
            // `color::mireds_to_warm_cold` — the bulb cannot render them.
            let (warm, cold) = crate::color::mireds_to_warm_cold(*mireds);
            Some(SetValue {
                command_class: CC_COLOR_SWITCH,
                property: "targetColor".into(),
                value: json!({ "warmWhite": warm, "coldWhite": cold }),
                transition: *transition,
            })
        }
        // Scenes/timers aren't device commands, and Z-Wave has no toggle — the
        // caller turns this `None` into a `Permanent` outcome (surfaced to the
        // `Observer`, not printed here).
        _ => None,
    }
}

/// Inbound value report → canonical events (the mirror of `command_to_set_value`,
/// and the twin of Matter's `report_to_events`). Unknown node or unmapped
/// `(command_class, property)` ⇒ no event.
pub fn update_to_events(
    by_node: &HashMap<(NodeId, EndpointId), DeviceId>,
    actions: &HashMap<DeviceId, HashMap<String, ActionId>>,
    u: &ValueUpdate,
) -> Vec<Event> {
    let Some(&device) = by_node.get(&(u.node, u.endpoint)) else {
        return Vec::new();
    };

    // A Central Scene press is stateless: fire only on a real `value notification`,
    // and only for a button+attribute this device declared (exact match).
    if u.command_class == CC_CENTRAL_SCENE && u.property == "scene" {
        if !u.notification {
            return Vec::new(); // value-init replay on (re)connect, not a press
        }
        let (Some(button), Some(attr)) = (
            u.property_key.as_deref().map(normalize_button),
            u.value.as_u64().and_then(key_attribute_name),
        ) else {
            return Vec::new();
        };
        let raw = format!("{button}:{attr}");
        return actions
            .get(&device)
            .and_then(|m| m.get(&raw))
            .map(|&action| Event::Action { device, action })
            .into_iter()
            .collect();
    }

    // Everything else is a stateful value. We fold only `currentValue` (the
    // device's actual state), never `targetValue` (our own commanded intent
    // echoed back), so state can't briefly lie between command and confirmation.
    let event = match (u.command_class, u.property.as_str()) {
        (CC_BINARY_SWITCH, "currentValue") => u.value.as_bool().map(|on| Event::StateReported {
            device,
            state: CapabilityState::Switch(on),
        }),
        // A dimmer's level implies both brightness *and* on/off (level 0 = off),
        // so a `switch` condition works on a dimmer that has no Binary Switch.
        (CC_MULTILEVEL_SWITCH, "currentValue") => {
            return u
                .value
                .as_u64()
                .map(|lvl| {
                    vec![
                        Event::StateReported {
                            device,
                            state: CapabilityState::Brightness(zwave_to_pct(lvl)),
                        },
                        Event::StateReported {
                            device,
                            state: CapabilityState::Switch(lvl > 0),
                        },
                    ]
                })
                .unwrap_or_default();
        }
        (CC_BATTERY, "level") => u.value.as_u64().map(|pct| Event::StateReported {
            device,
            state: CapabilityState::Battery(pct.min(100) as u8),
        }),
        (CC_COLOR_SWITCH, "currentColor") => {
            if let Some((r, g, b)) = parse_zwave_color(&u.value) {
                return vec![Event::StateReported {
                    device,
                    state: CapabilityState::Color { r, g, b },
                }];
            }
            if let Some(mireds) = parse_zwave_color_temp(&u.value) {
                return vec![Event::StateReported {
                    device,
                    state: CapabilityState::ColorTemperature(mireds),
                }];
            }
            return Vec::new();
        }
        // Notification CC, Home Security: motion detection (7/8) → occupied,
        // idle (0) → clear. Other notification kinds (tamper, …) are ignored.
        (CC_NOTIFICATION, "Home Security") => match u.value.as_u64() {
            Some(0) => Some(Event::OccupancyChanged {
                device,
                occupied: false,
            }),
            Some(7) | Some(8) => Some(Event::OccupancyChanged {
                device,
                occupied: true,
            }),
            _ => None,
        },
        _ => None,
    };
    event.into_iter().collect()
}

/// zwave-js reports the Central Scene button as a zero-padded string (`"001"`);
/// normalize it to a plain number (`"1"`) so a device's `events:` config reads
/// naturally. A non-numeric key is passed through unchanged.
fn normalize_button(key: &str) -> String {
    key.parse::<u32>()
        .map(|n| n.to_string())
        .unwrap_or_else(|_| key.to_string())
}

/// Extract sRGB from a Color Switch `currentColor` / `targetColor` object.
fn parse_zwave_color(value: &Value) -> Option<(u8, u8, u8)> {
    let obj = value.as_object()?;
    let r = obj.get("red").and_then(Value::as_u64)?;
    let g = obj.get("green").and_then(Value::as_u64)?;
    let b = obj.get("blue").and_then(Value::as_u64)?;
    if r <= 255 && g <= 255 && b <= 255 {
        Some((r as u8, g as u8, b as u8))
    } else {
        None
    }
}

/// Approximate mireds from a Color Switch warm/cold-white mix. Used when a
/// device reports tunable-white channels instead of a direct mireds value.
fn parse_zwave_color_temp(value: &Value) -> Option<u16> {
    let obj = value.as_object()?;
    let warm = obj.get("warmWhite").and_then(Value::as_u64)?;
    let cold = obj.get("coldWhite").and_then(Value::as_u64)?;
    crate::color::warm_cold_to_mireds(warm, cold)
}

// --- real transport ---------------------------------------------------------

mod zwave_ws {
    use std::sync::mpsc::{self, Receiver};
    use std::thread;
    use std::time::Duration;

    use futures_util::{SinkExt, StreamExt};
    use serde_json::{json, Value};
    use tokio::sync::mpsc as tokio_mpsc;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message;

    use super::{EndpointId, NodeId, SetValue, ValueUpdate, ZwaveClient};
    use crate::wake::Waker;

    /// Delay before retrying a failed or dropped server connection.
    const RECONNECT_DELAY: Duration = Duration::from_secs(3);

    /// The newest zwave-js-server API schema version this adapter understands. We
    /// negotiate `min(this, server maxSchemaVersion)` so a newer *or* older server
    /// is fine; the value fields we parse are stable across these versions.
    const MAX_SCHEMA_VERSION: u64 = 41;

    /// A live `zwave-js-server` WebSocket connection.
    ///
    /// All async is confined here, exactly as in the Matter transport: a
    /// background thread runs a single-threaded tokio runtime that owns the socket
    /// and `split`s it into independent read and write halves; a `select!` loop
    /// drives both — inbound frames become [`ValueUpdate`]s on `inbound` (and wake
    /// the host), outbound `node.set_value`s arrive on `outbound` and are written.
    ///
    /// `set_value` only pushes a frame onto `outbound`; `poll` drains `inbound`.
    /// The engine thread never touches the runtime, so the deterministic core
    /// stays single-threaded.
    pub struct ZwaveServerWs {
        outbound: tokio_mpsc::UnboundedSender<String>,
        inbound: Receiver<ValueUpdate>,
        next_id: u64,
    }

    impl ZwaveServerWs {
        /// Connect to `url` (e.g. `ws://host:3000`) and spawn the background
        /// runtime. Returns immediately — like the other transports, (re)connection
        /// happens in the background, so a server that's down at startup isn't
        /// fatal: queued commands flush once it's reachable.
        ///
        /// `waker`, if present, is signaled whenever an inbound report is queued,
        /// so a real-time host blocked on a [`Waker`] wakes promptly instead of
        /// polling. `None` (e.g. a one-shot tool) means no one is notified.
        pub fn connect(url: &str, waker: Option<Waker>) -> Self {
            let (outbound, outbound_rx) = tokio_mpsc::unbounded_channel::<String>();
            let (inbound_tx, inbound) = mpsc::channel();
            let url = url.to_string();
            thread::spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        eprintln!("[zwave] failed to build runtime: {e}");
                        return;
                    }
                };
                rt.block_on(run_connection(url, outbound_rx, inbound_tx, waker));
            });

            ZwaveServerWs {
                outbound,
                inbound,
                next_id: 1,
            }
        }
    }

    /// The background async loop: (re)connect, negotiate the schema, send
    /// `start_listening`, then pump the socket until it drops — forever. Returns
    /// only when the host drops the outbound sender (engine shutdown).
    async fn run_connection(
        url: String,
        mut outbound_rx: tokio_mpsc::UnboundedReceiver<String>,
        inbound_tx: mpsc::Sender<ValueUpdate>,
        waker: Option<Waker>,
    ) {
        loop {
            let stream = match connect_async(url.clone()).await {
                Ok((stream, _resp)) => stream,
                Err(e) => {
                    eprintln!(
                        "[zwave] connect to {url} failed: {e}; retrying in {RECONNECT_DELAY:?}"
                    );
                    tokio::time::sleep(RECONNECT_DELAY).await;
                    continue;
                }
            };
            let (mut write, mut read) = stream.split();

            // Handshake: the server sends a `version` frame first; reply with
            // `set_api_schema` (negotiated) and then `start_listening`, whose
            // result carries the full node snapshot (our free state priming).
            let schema = negotiate_schema(&mut read).await;
            let set_schema =
                json!({ "messageId": "set_api_schema", "command": "set_api_schema", "schemaVersion": schema })
                    .to_string();
            let start =
                json!({ "messageId": "start_listening", "command": "start_listening" }).to_string();
            if write.send(Message::Text(set_schema)).await.is_err()
                || write.send(Message::Text(start)).await.is_err()
            {
                eprintln!("[zwave] handshake failed; reconnecting");
                tokio::time::sleep(RECONNECT_DELAY).await;
                continue;
            }

            // Pump until the connection drops, then fall through to reconnect.
            loop {
                tokio::select! {
                    inbound_frame = read.next() => match inbound_frame {
                        Some(Ok(Message::Text(txt))) => {
                            for update in parse_inbound(&txt) {
                                let _ = inbound_tx.send(update);
                                if let Some(w) = &waker {
                                    w.wake();
                                }
                            }
                        }
                        Some(Ok(Message::Close(_))) | None => {
                            eprintln!("[zwave] connection closed; reconnecting");
                            break;
                        }
                        Some(Ok(_)) => {}
                        Some(Err(e)) => {
                            eprintln!("[zwave] read error: {e}; reconnecting");
                            break;
                        }
                    },
                    outgoing = outbound_rx.recv() => match outgoing {
                        Some(frame) => {
                            if let Err(e) = write.send(Message::Text(frame)).await {
                                eprintln!("[zwave] write error: {e}; reconnecting");
                                break;
                            }
                        }
                        // Host dropped the sender: the engine is shutting down.
                        None => return,
                    }
                }
            }
            tokio::time::sleep(RECONNECT_DELAY).await;
        }
    }

    /// Read the server's opening `version` frame and pick the schema version to
    /// request: the server's `maxSchemaVersion`, capped at ours. Falls back to a
    /// conservative version if the frame is missing or unparseable.
    async fn negotiate_schema(
        read: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
                  + Unpin),
    ) -> u64 {
        const FALLBACK_SCHEMA: u64 = 33;
        if let Some(Ok(Message::Text(txt))) = read.next().await {
            if let Ok(v) = serde_json::from_str::<Value>(&txt) {
                if let Some(server_max) = v.get("maxSchemaVersion").and_then(Value::as_u64) {
                    return server_max.min(MAX_SCHEMA_VERSION);
                }
            }
        }
        FALLBACK_SCHEMA
    }

    impl ZwaveClient for ZwaveServerWs {
        fn set_value(
            &mut self,
            node: NodeId,
            endpoint: EndpointId,
            value: &SetValue,
        ) -> Result<(), String> {
            let message_id = self.next_id.to_string();
            self.next_id += 1;
            let mut req = json!({
                "messageId": message_id,
                "command": "node.set_value",
                "nodeId": node.0,
                "valueId": {
                    "commandClass": value.command_class,
                    "endpoint": endpoint.0,
                    "property": value.property,
                },
                "value": value.value,
            });
            // A fade pushes down as the Multilevel Switch `transitionDuration`
            // option, in whole seconds (zwave-js accepts a `"<n>s"` duration).
            if let Some(ms) = value.transition {
                req["options"] = json!({ "transitionDuration": format!("{}s", ms / 1000) });
            }
            // Hand the frame to the socket-owning thread. `Err` only if that thread
            // has died (server gone) — surfaced as a retryable `Transient`.
            self.outbound
                .send(req.to_string())
                .map_err(|_| "zwave transport thread is gone".to_string())
        }

        fn poll(&mut self) -> Vec<ValueUpdate> {
            self.inbound.try_iter().collect()
        }
    }

    /// Parse one inbound server text frame into zero or more value updates.
    /// Handles the unsolicited `value updated` / `value notification` events and
    /// the `start_listening` snapshot result; everything else is ignored.
    fn parse_inbound(txt: &str) -> Vec<ValueUpdate> {
        let Ok(v) = serde_json::from_str::<Value>(txt) else {
            return Vec::new();
        };

        // An event frame: `{ type: "event", event: { source, event, nodeId, args } }`.
        if v.get("type").and_then(Value::as_str) == Some("event") {
            return parse_event(v.get("event")).into_iter().collect();
        }

        // The `start_listening` result carrying the node snapshot.
        if let Some(nodes) = v
            .get("result")
            .and_then(|r| r.get("state"))
            .and_then(|s| s.get("nodes"))
            .and_then(Value::as_array)
        {
            return nodes.iter().flat_map(parse_node_snapshot).collect();
        }
        Vec::new()
    }

    /// A single `event` object → an update, for the two event kinds we fold. A
    /// `value notification` is stateless (Central Scene); a `value updated` is
    /// stateful and carries the new value under `newValue`.
    fn parse_event(event: Option<&Value>) -> Option<ValueUpdate> {
        let event = event?;
        if event.get("source").and_then(Value::as_str) != Some("node") {
            return None;
        }
        let notification = match event.get("event").and_then(Value::as_str)? {
            "value notification" => true,
            "value updated" => false,
            _ => return None,
        };
        let node = event.get("nodeId").and_then(Value::as_u64)?;
        let args = event.get("args")?;
        // A `value updated` reports the fresh reading under `newValue`; a
        // `value notification` carries it inline as `value`.
        let value = if notification {
            args.get("value")?.clone()
        } else {
            args.get("newValue").or_else(|| args.get("value"))?.clone()
        };
        Some(ValueUpdate {
            node: NodeId(node as u32),
            endpoint: endpoint_id(args.get("endpoint")),
            command_class: args.get("commandClass").and_then(Value::as_u64)? as u16,
            property: property_string(args.get("property"))?,
            property_key: args.get("propertyKey").and_then(property_key_string),
            value,
            notification,
        })
    }

    /// One node object from the `start_listening` snapshot → its stateful value
    /// updates. Each entry of `node.values` is a value id plus its current
    /// `value`; snapshots are never stateless notifications.
    fn parse_node_snapshot(node: &Value) -> Vec<ValueUpdate> {
        let Some(node_id) = node.get("nodeId").and_then(Value::as_u64) else {
            return Vec::new();
        };
        let Some(values) = node.get("values").and_then(Value::as_array) else {
            return Vec::new();
        };
        values
            .iter()
            .filter_map(|val| {
                Some(ValueUpdate {
                    node: NodeId(node_id as u32),
                    endpoint: endpoint_id(val.get("endpoint")),
                    command_class: val.get("commandClass").and_then(Value::as_u64)? as u16,
                    property: property_string(val.get("property"))?,
                    property_key: val.get("propertyKey").and_then(property_key_string),
                    value: val.get("value")?.clone(),
                    notification: false,
                })
            })
            .collect()
    }

    /// A zwave-js value id `endpoint` is optional; a missing (or non-numeric)
    /// endpoint means the root, `0`.
    fn endpoint_id(v: Option<&Value>) -> EndpointId {
        EndpointId(v.and_then(Value::as_u64).unwrap_or(0) as u16)
    }

    /// A zwave-js `property` is a string *or* a number; render either as a string.
    fn property_string(v: Option<&Value>) -> Option<String> {
        match v? {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        }
    }

    /// Same for `propertyKey` (Central Scene's button is a string like `"001"`,
    /// but other CCs use a number).
    fn property_key_string(v: &Value) -> Option<String> {
        match v {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        }
    }
}

pub use zwave_ws::ZwaveServerWs;

// --- compiler registration --------------------------------------------------

use super::plugin::{config_of, AdapterPlugin};
use crate::compile::diagnostic::Diagnostic;
use crate::compile::resolve::DeviceDef;

/// Registers the Z-Wave JS adapter (`type: zwavejs`) with the compiler.
#[derive(Debug)]
pub struct Plugin;
pub static PLUGIN: Plugin = Plugin;

/// The `adapters.<name>` block for a Z-Wave JS adapter, minus `type`.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    /// Server WebSocket URL, e.g. `ws://host:3000`.
    url: String,
}

impl AdapterPlugin for Plugin {
    fn type_tag(&self) -> &'static str {
        "zwavejs"
    }

    fn validate_config(&self, config: &serde_yaml::Value, at: &str, diags: &mut Vec<Diagnostic>) {
        let Some(cfg) = config_of::<Config>(config, at, diags) else {
            return;
        };
        if !(cfg.url.starts_with("ws://") || cfg.url.starts_with("wss://")) {
            diags.push(
                Diagnostic::error(
                    "E_BAD_URL",
                    format!(
                        "zwavejs server url must be ws:// or wss:// — got '{}'",
                        cfg.url
                    ),
                )
                .at(at.to_string()),
            );
        }
    }

    fn validate_device(
        &self,
        _config: &serde_yaml::Value,
        device: &DeviceDef,
        at: &str,
        diags: &mut Vec<Diagnostic>,
    ) {
        // Z-Wave addresses devices by the decimal node_id from inclusion.
        match &device.address {
            None => diags.push(
                Diagnostic::error(
                    "E_MISSING_ADDRESS",
                    "zwavejs devices need an `address` (the decimal node_id from inclusion)",
                )
                .at(at.to_string()),
            ),
            Some(addr) if addr.parse::<u32>().is_err() => diags.push(
                Diagnostic::error(
                    "E_BAD_ADDRESS",
                    format!("zwavejs `address` must be a decimal node_id — got '{addr}'"),
                )
                .at(at.to_string()),
            ),
            _ => {}
        }
    }

    fn build(
        &self,
        config: &serde_yaml::Value,
        devices: &[&DeviceDef],
        waker: Option<crate::wake::Waker>,
    ) -> Box<dyn Adapter> {
        let cfg: Config = serde_yaml::from_value(config.clone())
            .unwrap_or_else(|_| Config { url: String::new() });
        // `address` is the decimal node_id (already validated numeric); a device
        // whose address somehow doesn't parse is dropped rather than panicking.
        // The kind (relay vs. dimmer) is derived from the declared capabilities.
        let targets: Vec<_> = devices
            .iter()
            .filter_map(|d| {
                let node = d.address.as_ref()?.parse::<u32>().ok()?;
                // Z-Wave's endpoint default is the root, 0: the single load of an
                // ordinary node. A multi-load module sets `endpoint:` per device.
                Some((
                    d.id,
                    NodeId(node),
                    EndpointId(d.endpoint.unwrap_or(0)),
                    DeviceKind::from_capabilities(&d.capabilities),
                ))
            })
            .collect();
        // Per-device (raw Central Scene string → ActionId) for inbound translation.
        let mut events = Vec::new();
        for d in devices {
            for e in &d.events {
                events.push((d.id, e.raw.clone(), e.id));
            }
        }
        let client = ZwaveServerWs::connect(&cfg.url, waker);
        Box::new(ZwaveAdapter::new(targets, events, Box::new(client)))
    }
}
