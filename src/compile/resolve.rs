//! Semantic analysis: raw AST → resolved object graph.
//!
//! This is where the compiler earns its name. Names become interned ids,
//! `adapter: "zigbee"` becomes a resolved `AdapterIdx`, capability strings
//! become `CapabilityKind`s, rule/scene bodies are lowered to runtime types, and
//! anything that does not line up becomes a `Diagnostic`. Every problem is
//! collected — the pass never stops at the first error.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use crate::compile::ast::{RawAdapter, RawConfig, RawSchedule};
use crate::compile::diagnostic::{CompileErrors, Diagnostic};
use crate::compile::lower::Lowerer;
use crate::ids::{ActionId, AdapterIdx, DeviceId, RuleId, ScheduleId, SceneId};
use crate::model::{CapabilityKind, Command};
use crate::rule::Rule;

/// Global, single-valued configuration. Connection details live on adapters;
/// only genuinely site-wide values live here.
#[derive(Clone, Debug)]
pub struct SystemConfig {
    pub name: Option<String>,
    pub timezone: String,
    pub latitude: f64,
    pub longitude: f64,
}

#[derive(Clone, Debug)]
pub enum AdapterKind {
    Zigbee2Mqtt {
        host: String,
        port: u16,
        base_topic: String,
    },
    /// A Matter controller addressed by its WebSocket URL (e.g.
    /// `ws://host:5580/ws`). Parsing the URL is left to the transport.
    Matter {
        url: String,
    },
    Mock,
}

/// Parse `mqtt://host:port` (scheme optional, port defaults to 1883).
fn parse_mqtt_url(url: &str) -> Option<(String, u16)> {
    let rest = url
        .strip_prefix("mqtt://")
        .or_else(|| url.strip_prefix("tcp://"))
        .unwrap_or(url)
        .trim_end_matches('/');
    let (host, port) = match rest.rsplit_once(':') {
        Some((host, port)) => (host, port.parse::<u16>().ok()?),
        None => (rest, 1883),
    };
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), port))
}

#[derive(Clone, Debug)]
pub struct AdapterDef {
    pub name: String,
    pub kind: AdapterKind,
}

#[derive(Clone, Debug, Default)]
pub struct DeviceMetadata {
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub room: Option<String>,
}

/// One declared device event: a local name, the raw protocol string the adapter
/// matches, and the interned identity rules trigger on.
#[derive(Clone, Debug)]
pub struct DeviceEvent {
    pub id: ActionId,
    pub name: String,
    pub raw: String,
}

#[derive(Clone, Debug)]
pub struct DeviceDef {
    pub id: DeviceId,
    pub name: String,
    /// Resolved reference (a config-space adapter index), not a string.
    pub adapter: AdapterIdx,
    pub address: Option<String>,
    /// Matter endpoint (1 by default); ignored by protocols that don't use it.
    pub endpoint: u16,
    pub capabilities: Vec<CapabilityKind>,
    /// Stateless events this device can emit (button presses, knob turns, …).
    pub events: Vec<DeviceEvent>,
    pub metadata: DeviceMetadata,
}

#[derive(Clone, Debug)]
pub struct CompiledScene {
    pub id: SceneId,
    pub name: String,
    pub commands: Vec<Command>,
}

/// A resolved wall-clock schedule: its interned id and the (validated, desugared)
/// 5-field cron expression the clock adapter fires on. Kept as a string so the
/// compiler output stays plain data; the engine builder parses it back to a
/// `croner::Cron`.
#[derive(Clone, Debug)]
pub struct CompiledSchedule {
    pub id: ScheduleId,
    pub name: String,
    pub cron: String,
}

/// The compiled, fully-resolved configuration: the object graph the plan calls
/// for, with strings already turned into references — including runtime-ready
/// `Rule`s and scene command lists. `warnings` are surfaced on success.
#[derive(Clone, Debug)]
pub struct CompiledConfig {
    pub system: SystemConfig,
    pub adapters: Vec<AdapterDef>,
    pub devices: Vec<DeviceDef>,
    pub scenes: Vec<CompiledScene>,
    pub schedules: Vec<CompiledSchedule>,
    pub rules: Vec<Rule>,
    pub warnings: Vec<Diagnostic>,
    adapter_index: HashMap<String, AdapterIdx>,
    device_index: HashMap<String, DeviceId>,
    scene_index: HashMap<String, SceneId>,
    schedule_index: HashMap<String, ScheduleId>,
    clock_device: DeviceId,
}

impl CompiledConfig {
    pub fn device_id(&self, name: &str) -> Option<DeviceId> {
        self.device_index.get(name).copied()
    }

    pub fn adapter_idx(&self, name: &str) -> Option<AdapterIdx> {
        self.adapter_index.get(name).copied()
    }

    pub fn scene_id(&self, name: &str) -> Option<SceneId> {
        self.scene_index.get(name).copied()
    }

    pub fn schedule_id(&self, name: &str) -> Option<ScheduleId> {
        self.schedule_index.get(name).copied()
    }

    pub fn device(&self, id: DeviceId) -> Option<&DeviceDef> {
        self.devices.get(id.0 as usize)
    }

    /// The interned `ActionId` for a device's declared event, by local name.
    pub fn action_id(&self, device: DeviceId, event: &str) -> Option<ActionId> {
        self.device(device)?
            .events
            .iter()
            .find(|e| e.name == event)
            .map(|e| e.id)
    }

    /// The synthetic device backing `sun_up` / `time_*` conditions, which the
    /// engine builder wires to a clock adapter.
    pub fn clock_device(&self) -> DeviceId {
        self.clock_device
    }
}

/// Map a capability string to its kind. Synthetic capabilities (`time_of_day`,
/// `sun_up`) are deliberately absent: the clock adapter produces them, so naming
/// one on a physical device is an error.
fn parse_capability(s: &str) -> Option<CapabilityKind> {
    Some(match s {
        "switch" => CapabilityKind::Switch,
        "brightness" => CapabilityKind::Brightness,
        "color_temperature" => CapabilityKind::ColorTemperature,
        "occupancy" => CapabilityKind::Occupancy,
        "battery" => CapabilityKind::Battery,
        _ => return None,
    })
}

fn adapter_kind(raw: &RawAdapter, diags: &mut Vec<Diagnostic>, at: &str) -> AdapterKind {
    match raw {
        RawAdapter::Zigbee2mqtt { url, base_topic } => {
            let (host, port) = parse_mqtt_url(url).unwrap_or_else(|| {
                diags.push(
                    Diagnostic::error("E_BAD_URL", format!("invalid broker url '{url}'"))
                        .at(at.to_string()),
                );
                (String::new(), 0)
            });
            AdapterKind::Zigbee2Mqtt {
                host,
                port,
                base_topic: base_topic.clone(),
            }
        }
        RawAdapter::Matter { url } => {
            if !(url.starts_with("ws://") || url.starts_with("wss://")) {
                diags.push(
                    Diagnostic::error(
                        "E_BAD_URL",
                        format!("matter controller url must be ws:// or wss:// — got '{url}'"),
                    )
                    .at(at.to_string()),
                );
            }
            AdapterKind::Matter { url: url.clone() }
        }
        RawAdapter::Mock => AdapterKind::Mock,
    }
}

pub fn resolve(raw: RawConfig) -> Result<CompiledConfig, CompileErrors> {
    let mut diags: Vec<Diagnostic> = Vec::new();

    // --- adapters: assign indices in name-sorted order ----------------------
    let mut adapters: Vec<AdapterDef> = Vec::new();
    let mut adapter_index: HashMap<String, AdapterIdx> = HashMap::new();
    for (name, raw_adapter) in &raw.adapters {
        adapter_index.insert(name.clone(), adapters.len());
        let kind = adapter_kind(raw_adapter, &mut diags, &format!("adapter '{name}'"));
        adapters.push(AdapterDef {
            name: name.clone(),
            kind,
        });
    }

    // --- devices: resolve adapter refs + capabilities + events --------------
    let mut devices: Vec<DeviceDef> = Vec::new();
    let mut device_index: HashMap<String, DeviceId> = HashMap::new();
    let mut used_adapters: HashSet<AdapterIdx> = HashSet::new();
    // Interns every declared device event in (device, event) name-sorted order,
    // so ActionIds are stable across runs.
    let mut next_action: u32 = 0;

    for (name, raw_device) in &raw.devices {
        let at = format!("device '{name}'");

        let adapter = match adapter_index.get(&raw_device.adapter) {
            Some(&idx) => {
                used_adapters.insert(idx);
                Some(idx)
            }
            None => {
                diags.push(
                    Diagnostic::error(
                        "E_UNKNOWN_ADAPTER",
                        format!("references unknown adapter '{}'", raw_device.adapter),
                    )
                    .at(at.clone()),
                );
                None
            }
        };

        let mut capabilities = Vec::new();
        for cap in &raw_device.capabilities {
            match parse_capability(cap) {
                Some(kind) => capabilities.push(kind),
                None => diags.push(
                    Diagnostic::error(
                        "E_UNKNOWN_CAPABILITY",
                        format!("unknown capability '{cap}'"),
                    )
                    .at(at.clone()),
                ),
            }
        }
        if raw_device.capabilities.is_empty() {
            diags.push(
                Diagnostic::warning("E_NO_CAPABILITIES", "device declares no capabilities")
                    .at(at.clone()),
            );
        }

        // Intern declared events (name-sorted via the BTreeMap). Two names may
        // map to the same raw string; that's allowed (aliases).
        let mut events = Vec::new();
        for (event_name, raw) in &raw_device.events {
            events.push(DeviceEvent {
                id: ActionId(next_action),
                name: event_name.clone(),
                raw: raw.clone(),
            });
            next_action += 1;
        }

        if let Some(adapter) = adapter {
            let id = DeviceId(devices.len() as u32);
            device_index.insert(name.clone(), id);
            devices.push(DeviceDef {
                id,
                name: name.clone(),
                adapter,
                address: raw_device.address.clone(),
                endpoint: raw_device.endpoint.unwrap_or(1),
                capabilities,
                events,
                metadata: DeviceMetadata {
                    manufacturer: raw_device.manufacturer.clone(),
                    model: raw_device.model.clone(),
                    room: raw_device.room.clone(),
                },
            });
        }
    }

    // Reserve the synthetic clock device id just past the user devices.
    let clock_device = DeviceId(devices.len() as u32);

    // --- scenes: assign ids before lowering so rules can reference them ------
    let mut scene_index: HashMap<String, SceneId> = HashMap::new();
    for name in raw.scenes.keys() {
        scene_index.insert(name.clone(), SceneId(scene_index.len() as u32));
    }

    // --- schedules: assign ids + desugar/validate to cron -------------------
    let mut schedule_index: HashMap<String, ScheduleId> = HashMap::new();
    let mut schedules: Vec<CompiledSchedule> = Vec::new();
    for (name, raw_schedule) in &raw.schedules {
        let id = ScheduleId(schedule_index.len() as u32);
        schedule_index.insert(name.clone(), id);
        if let Some(cron) = resolve_schedule(name, raw_schedule, &mut diags) {
            schedules.push(CompiledSchedule {
                id,
                name: name.clone(),
                cron,
            });
        }
    }

    // --- lower scenes and rules ---------------------------------------------
    let mut scenes: Vec<CompiledScene> = Vec::new();
    let mut rules: Vec<Rule> = Vec::new();
    let mut scheduled_keys: HashSet<String> = HashSet::new();
    let mut referenced_keys: HashSet<String> = HashSet::new();
    let mut used_scenes: HashSet<SceneId> = HashSet::new();
    let mut referenced_schedules: HashSet<ScheduleId> = HashSet::new();

    {
        let mut lw = Lowerer {
            devices: &devices,
            device_index: &device_index,
            scene_index: &scene_index,
            schedule_index: &schedule_index,
            clock_device,
            diags: &mut diags,
            scheduled_keys: &mut scheduled_keys,
            referenced_keys: &mut referenced_keys,
            used_scenes: &mut used_scenes,
            referenced_schedules: &mut referenced_schedules,
        };

        for (name, raw_commands) in &raw.scenes {
            let at = format!("scene '{name}'");
            let commands = lw.commands(raw_commands, &at);
            scenes.push(CompiledScene {
                id: scene_index[name],
                name: name.clone(),
                commands,
            });
        }

        for (i, (name, raw_rule)) in raw.rules.iter().enumerate() {
            let at = format!("rule '{name}'");
            let trigger = lw.trigger(&raw_rule.when, &at);
            let condition = match &raw_rule.condition {
                Some(c) => lw.condition(c, &at),
                None => Some(crate::rule::Condition::Always),
            };
            let commands = lw.commands(&raw_rule.then, &at);
            if let (Some(trigger), Some(condition)) = (trigger, condition) {
                rules.push(
                    Rule::new(RuleId(i as u32), trigger, condition, commands)
                        .with_name(name.clone()),
                );
            }
        }
    }

    // --- whole-program lints ------------------------------------------------
    for (idx, adapter) in adapters.iter().enumerate() {
        if !used_adapters.contains(&idx) {
            diags.push(
                Diagnostic::warning("E_UNUSED_ADAPTER", "adapter is not used by any device")
                    .at(format!("adapter '{}'", adapter.name)),
            );
        }
    }
    // Protocol adapters address devices by an adapter-specific `address`, so one
    // is required (and, for Matter, must be a numeric node_id).
    for device in &devices {
        let at = format!("device '{}'", device.name);
        match adapters.get(device.adapter).map(|a| &a.kind) {
            // zigbee2mqtt addresses devices by friendly_name.
            Some(AdapterKind::Zigbee2Mqtt { .. }) if device.address.is_none() => {
                diags.push(
                    Diagnostic::error(
                        "E_MISSING_ADDRESS",
                        "zigbee2mqtt devices need an `address` (the z2m friendly_name)",
                    )
                    .at(at),
                );
            }
            // Matter addresses devices by the decimal node_id from commissioning.
            Some(AdapterKind::Matter { .. }) => match &device.address {
                None => diags.push(
                    Diagnostic::error(
                        "E_MISSING_ADDRESS",
                        "matter devices need an `address` (the decimal node_id from commissioning)",
                    )
                    .at(at),
                ),
                Some(addr) if addr.parse::<u64>().is_err() => diags.push(
                    Diagnostic::error(
                        "E_BAD_ADDRESS",
                        format!("matter `address` must be a decimal node_id — got '{addr}'"),
                    )
                    .at(at),
                ),
                _ => {}
            },
            _ => {}
        }
    }
    for scene in &scenes {
        if !used_scenes.contains(&scene.id) {
            diags.push(
                Diagnostic::warning("E_UNUSED_SCENE", "scene is never activated")
                    .at(format!("scene '{}'", scene.name)),
            );
        }
    }
    for schedule in &schedules {
        if !referenced_schedules.contains(&schedule.id) {
            diags.push(
                Diagnostic::warning("E_UNUSED_SCHEDULE", "schedule triggers no rule")
                    .at(format!("schedule '{}'", schedule.name)),
            );
        }
    }
    for key in referenced_keys.difference(&scheduled_keys) {
        diags.push(Diagnostic::warning(
            "E_DANGLING_TIMER",
            format!("timer key '{key}' is referenced but never scheduled"),
        ));
    }
    if raw.devices.is_empty() {
        diags.push(Diagnostic::warning(
            "E_NO_DEVICES",
            "configuration defines no devices",
        ));
    }

    let system = system_config(&raw, &mut diags);

    if diags.iter().any(Diagnostic::is_error) {
        return Err(CompileErrors(diags));
    }

    Ok(CompiledConfig {
        system,
        adapters,
        devices,
        scenes,
        schedules,
        rules,
        warnings: diags, // only warnings remain
        adapter_index,
        device_index,
        scene_index,
        schedule_index,
        clock_device,
    })
}

fn system_config(raw: &RawConfig, diags: &mut Vec<Diagnostic>) -> SystemConfig {
    let timezone = raw.system.timezone.clone().unwrap_or_else(|| "UTC".into());
    // Validate against the IANA database now, so a typo like `America/New_Yrok`
    // fails at compile time rather than silently falling back at runtime.
    if chrono_tz::Tz::from_str(&timezone).is_err() {
        diags.push(Diagnostic::error(
            "E_BAD_TIMEZONE",
            format!("unknown timezone '{timezone}' (expected an IANA name like America/New_York)"),
        ));
    }
    SystemConfig {
        name: raw.system.name.clone(),
        timezone,
        latitude: raw.system.latitude.unwrap_or(0.0),
        longitude: raw.system.longitude.unwrap_or(0.0),
    }
}

/// Desugar one schedule entry to a validated 5-field cron string. Exactly one of
/// `cron`/`daily`/`weekday`/`weekend` must be set; the sugar forms expand to cron,
/// and the result is validated with `croner`.
fn resolve_schedule(
    name: &str,
    raw: &RawSchedule,
    diags: &mut Vec<Diagnostic>,
) -> Option<String> {
    let at = format!("schedule '{name}'");
    // Collect whichever field is populated, as (label, desugared-cron).
    let mut forms: Vec<String> = Vec::new();
    if let Some(expr) = &raw.cron {
        forms.push(expr.clone());
    }
    for (field, dow) in [
        (&raw.daily, "*"),
        (&raw.weekday, "1-5"),
        (&raw.weekend, "0,6"),
    ] {
        if let Some(hhmm) = field {
            forms.push(desugar_time(hhmm, dow, &at, diags)?);
        }
    }

    let cron = match forms.len() {
        1 => forms.pop().unwrap(),
        0 => {
            diags.push(
                Diagnostic::error(
                    "E_BAD_SCHEDULE",
                    "schedule needs one of: cron, daily, weekday, weekend",
                )
                .at(at),
            );
            return None;
        }
        _ => {
            diags.push(
                Diagnostic::error(
                    "E_BAD_SCHEDULE",
                    "schedule must set exactly one of: cron, daily, weekday, weekend",
                )
                .at(at),
            );
            return None;
        }
    };

    // Validate the final expression (raw or desugared) against the cron parser.
    if let Err(e) = croner::Cron::from_str(&cron) {
        diags.push(Diagnostic::error("E_BAD_CRON", format!("invalid cron '{cron}': {e}")).at(at));
        return None;
    }
    Some(cron)
}

/// `"HH:MM"` + a day-of-week field → a `"M H * * <dow>"` cron expression.
fn desugar_time(hhmm: &str, dow: &str, at: &str, diags: &mut Vec<Diagnostic>) -> Option<String> {
    if let Some((h, m)) = hhmm.trim().split_once(':') {
        if let (Ok(h), Ok(m)) = (h.trim().parse::<u16>(), m.trim().parse::<u16>()) {
            if h < 24 && m < 60 {
                return Some(format!("{m} {h} * * {dow}"));
            }
        }
    }
    diags.push(
        Diagnostic::error("E_BAD_TIME", format!("invalid time '{hhmm}' (expected HH:MM)"))
            .at(at.to_string()),
    );
    None
}
