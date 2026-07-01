//! The Matter adapter: canonical events/commands ↔ a Matter controller.
//!
//! Structurally this is the zigbee2mqtt adapter with different nouns. The
//! deterministic engine core is synchronous, but a Matter controller
//! (`python-matter-server`) is asynchronous and network-driven. The seam between
//! them is [`MatterController`]: the adapter only ever *invokes* cluster commands
//! and *polls* for attribute reports through it. A real implementation
//! ([`MatterServerWs`], behind the `matter` feature) runs a WebSocket on a
//! background thread and hands inbound reports back via `poll`; tests use an
//! in-memory fake. Either way the protocol translation — the interesting,
//! bug-prone part — is pure and exercised without a controller.
//!
//! ```text
//!   attribute report ─▶ MatterController::poll ─▶ report_to_events ─▶ Event (tick)
//!   Command ─▶ command_to_cluster ─▶ MatterController::invoke ─▶ device_command
//! ```
//!
//! Note we talk to a *controller*, not to an OTBR. "Matter-over-Thread" only
//! means the device's IPv6 packets ride the Thread mesh; controlling a device
//! still needs a process that speaks the Matter Interaction Model (CASE sessions,
//! cluster reads/invokes). See `docs/matter.md`.

use std::collections::HashMap;

use serde_json::Value;

use super::{level_to_pct, pct_to_level, Adapter, DispatchOutcome};
use crate::ids::DeviceId;
use crate::model::{CapabilityState, Command, Event, Millis};

/// A commissioned Matter node id (decimal, assigned at commissioning time —
/// the `address` in config). A newtype, not a bare `u64`, so it can't be
/// transposed with an [`EndpointId`] and so it can key a `HashMap`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NodeId(pub u64);

/// One endpoint of a node (a multi-endpoint device — e.g. a power strip — is one
/// domiform device per endpoint). Defaults to 1 in config.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EndpointId(pub u16);

/// One controller command, already lowered from a canonical [`Command`]. Small
/// and protocol-shaped — the analogue of the JSON body `command_to_publish`
/// builds for z2m.
#[derive(Clone, Debug, PartialEq)]
pub enum ClusterCommand {
    /// OnOff cluster: On (`true`) / Off (`false`).
    OnOff(bool),
    /// OnOff cluster: Toggle.
    Toggle,
    /// LevelControl: MoveToLevelWithOnOff. `level` is 0..=254; `transition_ds` is
    /// the fade time in tenths of a second.
    MoveToLevel { level: u8, transition_ds: u16 },
}

/// One inbound attribute report (the unit of [`MatterController::poll`]). `value`
/// stays a raw JSON value; turning it into a typed [`CapabilityState`] is
/// [`report_to_events`]' job.
#[derive(Clone, Debug)]
pub struct AttrReport {
    pub node: NodeId,
    pub endpoint: EndpointId,
    pub cluster: u32,   // e.g. 0x0006 OnOff
    pub attribute: u32, // e.g. 0x0000 OnOff
    pub value: Value,
}

/// The seam between the synchronous engine and an asynchronous Matter
/// controller. Compare [`MqttTransport`](super::MqttTransport): act outward,
/// drain inward, nothing else.
///
/// Not required to be `Send`: the engine is single-threaded, and a real
/// controller keeps its own network thread internally, exposing only this
/// non-blocking invoke/poll surface.
pub trait MatterController {
    /// Invoke a cluster command on a node's endpoint. `Err` signals a transient
    /// failure (the adapter turns it into a retryable `DispatchOutcome::Transient`).
    fn invoke(
        &mut self,
        node: NodeId,
        endpoint: EndpointId,
        cmd: &ClusterCommand,
    ) -> Result<(), String>;

    /// Return every attribute report received since the last call (non-blocking).
    fn poll(&mut self) -> Vec<AttrReport>;
}

/// Adapter that bridges a set of commissioned Matter nodes to the engine.
pub struct MatterAdapter {
    /// address (node + endpoint) for an outbound command's target device.
    by_id: HashMap<DeviceId, (NodeId, EndpointId)>,
    /// reverse lookup for an inbound attribute report.
    by_node: HashMap<(NodeId, EndpointId), DeviceId>,
    controller: Box<dyn MatterController>,
}

impl MatterAdapter {
    /// `devices` maps each `DeviceId` to its commissioned `(NodeId, EndpointId)`
    /// (the `address` + `endpoint` from config).
    pub fn new(
        devices: impl IntoIterator<Item = (DeviceId, NodeId, EndpointId)>,
        controller: Box<dyn MatterController>,
    ) -> Self {
        let mut by_id = HashMap::new();
        let mut by_node = HashMap::new();
        for (id, node, endpoint) in devices {
            by_id.insert(id, (node, endpoint));
            by_node.insert((node, endpoint), id);
        }
        MatterAdapter {
            by_id,
            by_node,
            controller,
        }
    }
}

impl Adapter for MatterAdapter {
    fn dispatch(&mut self, cmd: &Command, _now: Millis) -> DispatchOutcome {
        let Some(device) = cmd.target_device() else {
            return DispatchOutcome::Permanent("command has no target device".into());
        };
        let Some(&(node, endpoint)) = self.by_id.get(&device) else {
            return DispatchOutcome::Permanent("device is not managed by this adapter".into());
        };
        let Some(cluster_cmd) = command_to_cluster(cmd) else {
            return DispatchOutcome::Permanent("command unsupported by matter".into());
        };
        match self.controller.invoke(node, endpoint, &cluster_cmd) {
            Ok(()) => DispatchOutcome::ok(),
            Err(e) => DispatchOutcome::Transient(e),
        }
    }

    fn tick(&mut self, _now: Millis) -> Vec<Event> {
        // Unlike z2m, no explicit `/get` priming is needed: a real controller's
        // `start_listening` snapshot is replayed as attribute reports on connect,
        // so state-gated conditions start from real state through this same path.
        let reports = self.controller.poll();
        let mut events = Vec::new();
        for r in &reports {
            events.extend(report_to_events(&self.by_node, r));
        }
        events
    }
}

// --- pure translation (no controller, no engine) ----------------------------

/// Canonical command → Matter cluster command. `None` = not a device command
/// (scenes/timers never reach an adapter; the engine handles those).
pub fn command_to_cluster(cmd: &Command) -> Option<ClusterCommand> {
    match cmd {
        Command::SetSwitch { on, .. } => Some(ClusterCommand::OnOff(*on)),
        Command::ToggleSwitch { .. } => Some(ClusterCommand::Toggle),
        Command::SetBrightness {
            value, transition, ..
        } => Some(ClusterCommand::MoveToLevel {
            level: pct_to_level(*value) as u8, // 0..=100 → 0..=254
            transition_ds: transition.map_or(0, |ms| (ms / 100) as u16), // ms → 1/10 s
        }),
        // Scenes/timers aren't device commands; the caller turns this `None` into
        // a `Permanent` outcome (surfaced to the `Observer`, not printed here).
        _ => None,
    }
}

/// Inbound attribute report → canonical events (the mirror of `message_to_events`).
/// Unknown `(node, endpoint)` or unmapped `(cluster, attribute)` ⇒ no event.
pub fn report_to_events(
    by_node: &HashMap<(NodeId, EndpointId), DeviceId>,
    r: &AttrReport,
) -> Vec<Event> {
    let Some(&device) = by_node.get(&(r.node, r.endpoint)) else {
        return Vec::new();
    };
    let event = match (r.cluster, r.attribute) {
        // OnOff cluster, OnOff attribute (bool).
        (0x0006, 0x0000) => r.value.as_bool().map(|on| Event::StateReported {
            device,
            state: CapabilityState::Switch(on),
        }),
        // LevelControl, CurrentLevel (0..=254 → 0..=100).
        (0x0008, 0x0000) => r.value.as_u64().map(|lvl| Event::StateReported {
            device,
            state: CapabilityState::Brightness(level_to_pct(lvl)),
        }),
        // ColorControl, ColorTemperatureMireds (mireds).
        (0x0300, 0x0007) => r.value.as_u64().map(|mireds| Event::StateReported {
            device,
            state: CapabilityState::ColorTemperature(mireds.min(u16::MAX as u64) as u16),
        }),
        // OccupancySensing, Occupancy (bit 0 = occupied).
        (0x0406, 0x0000) => r.value.as_u64().map(|bits| Event::OccupancyChanged {
            device,
            occupied: bits & 1 == 1,
        }),
        // PowerSource, BatPercentRemaining (½-percent → percent).
        (0x002F, 0x000C) => r.value.as_u64().map(|half| Event::StateReported {
            device,
            state: CapabilityState::Battery((half / 2).min(100) as u8),
        }),
        _ => None,
    };
    event.into_iter().collect()
}

// --- real transport ---------------------------------------------------------

mod matter_ws {
    use std::sync::mpsc::{self, Receiver};
    use std::thread;
    use std::time::Duration;

    use futures_util::{SinkExt, StreamExt};
    use serde_json::{json, Value};
    use tokio::sync::mpsc as tokio_mpsc;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message;

    use super::{AttrReport, ClusterCommand, EndpointId, MatterController, NodeId};
    use crate::wake::Waker;

    /// Delay before retrying a failed or dropped controller connection.
    const RECONNECT_DELAY: Duration = Duration::from_secs(3);

    /// A live `python-matter-server` WebSocket connection.
    ///
    /// All async is confined here. A background thread runs a single-threaded tokio
    /// runtime that owns the socket and `split`s it into independent read and write
    /// halves; a `select!` loop drives both — inbound frames become [`AttrReport`]s
    /// on `inbound` (and wake the host), outbound `device_command`s arrive on
    /// `outbound` and are written. Because the halves are separate objects, reads
    /// never delay writes — no lock, and no read-timeout polling.
    ///
    /// `invoke` only pushes a frame onto `outbound` (a non-blocking sync send on a
    /// tokio unbounded channel); `poll` drains `inbound`. The engine thread never
    /// touches the runtime, so the deterministic core stays single-threaded.
    pub struct MatterServerWs {
        outbound: tokio_mpsc::UnboundedSender<String>,
        inbound: Receiver<AttrReport>,
        next_id: u64,
    }

    impl MatterServerWs {
        /// Connect to `url` (e.g. `ws://host:5580/ws`) and spawn the background
        /// runtime. Returns immediately — like the z2m transport, (re)connection
        /// happens in the background, so a controller that's down at startup isn't
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
                        eprintln!("[matter] failed to build runtime: {e}");
                        return;
                    }
                };
                rt.block_on(run_connection(url, outbound_rx, inbound_tx, waker));
            });

            MatterServerWs {
                outbound,
                inbound,
                next_id: 1,
            }
        }
    }

    /// The background async loop: (re)connect, send `start_listening`, then pump the
    /// socket until it drops — forever. Returns only when the host drops the
    /// outbound sender (engine shutdown).
    async fn run_connection(
        url: String,
        mut outbound_rx: tokio_mpsc::UnboundedReceiver<String>,
        inbound_tx: mpsc::Sender<AttrReport>,
        waker: Option<Waker>,
    ) {
        // `start_listening` both subscribes to updates and returns a snapshot of
        // every node's current attributes — folding that snapshot gives us free
        // state priming (the z2m `/get`-on-connect equivalent), on every (re)connect.
        let start =
            json!({ "message_id": "start_listening", "command": "start_listening" }).to_string();
        loop {
            let stream = match connect_async(url.clone()).await {
                Ok((stream, _resp)) => stream,
                Err(e) => {
                    eprintln!(
                        "[matter] connect to {url} failed: {e}; retrying in {RECONNECT_DELAY:?}"
                    );
                    tokio::time::sleep(RECONNECT_DELAY).await;
                    continue;
                }
            };
            let (mut write, mut read) = stream.split();
            if let Err(e) = write.send(Message::Text(start.clone())).await {
                eprintln!("[matter] start_listening failed: {e}; reconnecting");
                tokio::time::sleep(RECONNECT_DELAY).await;
                continue;
            }
            // Pump until the connection drops, then fall through to reconnect.
            loop {
                tokio::select! {
                    inbound_frame = read.next() => match inbound_frame {
                        Some(Ok(Message::Text(txt))) => {
                            for report in parse_inbound(&txt) {
                                let _ = inbound_tx.send(report);
                                if let Some(w) = &waker {
                                    w.wake();
                                }
                            }
                        }
                        Some(Ok(Message::Close(_))) | None => {
                            eprintln!("[matter] connection closed; reconnecting");
                            break;
                        }
                        Some(Ok(_)) => {}
                        Some(Err(e)) => {
                            eprintln!("[matter] read error: {e}; reconnecting");
                            break;
                        }
                    },
                    outgoing = outbound_rx.recv() => match outgoing {
                        Some(frame) => {
                            if let Err(e) = write.send(Message::Text(frame)).await {
                                eprintln!("[matter] write error: {e}; reconnecting");
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

    impl MatterController for MatterServerWs {
        fn invoke(
            &mut self,
            node: NodeId,
            endpoint: EndpointId,
            cmd: &ClusterCommand,
        ) -> Result<(), String> {
            let (cluster_id, command_name, payload): (u32, &str, Value) = match cmd {
                ClusterCommand::OnOff(true) => (0x0006, "On", json!({})),
                ClusterCommand::OnOff(false) => (0x0006, "Off", json!({})),
                ClusterCommand::Toggle => (0x0006, "Toggle", json!({})),
                ClusterCommand::MoveToLevel {
                    level,
                    transition_ds,
                } => (
                    0x0008,
                    "MoveToLevelWithOnOff",
                    json!({ "level": level, "transitionTime": transition_ds }),
                ),
            };
            let message_id = self.next_id.to_string();
            self.next_id += 1;
            let req = json!({
                "message_id": message_id,
                "command": "device_command",
                "args": {
                    "node_id": node.0,
                    "endpoint_id": endpoint.0,
                    "cluster_id": cluster_id,
                    "command_name": command_name,
                    "payload": payload,
                }
            });
            // Hand the frame to the socket-owning thread. `Err` only if that thread
            // has died (controller gone) — surfaced as a retryable `Transient`.
            self.outbound
                .send(req.to_string())
                .map_err(|_| "matter transport thread is gone".to_string())
        }

        fn poll(&mut self) -> Vec<AttrReport> {
            self.inbound.try_iter().collect()
        }
    }

    /// Parse one inbound server text frame into zero or more attribute reports.
    /// Handles the unsolicited `attribute_updated` event and the
    /// `start_listening` snapshot result; everything else is ignored.
    fn parse_inbound(txt: &str) -> Vec<AttrReport> {
        let Ok(v) = serde_json::from_str::<Value>(txt) else {
            return Vec::new();
        };
        if v.get("event").and_then(Value::as_str) == Some("attribute_updated") {
            return v
                .get("data")
                .and_then(Value::as_array)
                .and_then(|d| parse_attribute_updated(d))
                .into_iter()
                .collect();
        }
        // A command result carrying the node snapshot (the `start_listening` reply).
        if let Some(nodes) = v.get("result").and_then(Value::as_array) {
            return nodes.iter().flat_map(parse_node_snapshot).collect();
        }
        Vec::new()
    }

    /// `[node_id, "endpoint/cluster/attribute", value]` → one report.
    fn parse_attribute_updated(data: &[Value]) -> Option<AttrReport> {
        let node = data.first()?.as_u64()?;
        let (endpoint, cluster, attribute) = parse_path(data.get(1)?.as_str()?)?;
        Some(AttrReport {
            node: NodeId(node),
            endpoint: EndpointId(endpoint),
            cluster,
            attribute,
            value: data.get(2)?.clone(),
        })
    }

    /// One node object from the `start_listening` snapshot → its attribute reports.
    fn parse_node_snapshot(node: &Value) -> Vec<AttrReport> {
        let Some(node_id) = node.get("node_id").and_then(Value::as_u64) else {
            return Vec::new();
        };
        let Some(attrs) = node.get("attributes").and_then(Value::as_object) else {
            return Vec::new();
        };
        attrs
            .iter()
            .filter_map(|(path, value)| {
                let (endpoint, cluster, attribute) = parse_path(path)?;
                Some(AttrReport {
                    node: NodeId(node_id),
                    endpoint: EndpointId(endpoint),
                    cluster,
                    attribute,
                    value: value.clone(),
                })
            })
            .collect()
    }

    /// `"endpoint/cluster/attribute"` (decimal ids) → `(endpoint, cluster, attribute)`.
    fn parse_path(path: &str) -> Option<(u16, u32, u32)> {
        let mut parts = path.split('/');
        let endpoint = parts.next()?.parse().ok()?;
        let cluster = parts.next()?.parse().ok()?;
        let attribute = parts.next()?.parse().ok()?;
        Some((endpoint, cluster, attribute))
    }
}

pub use matter_ws::MatterServerWs;
