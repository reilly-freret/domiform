//! Lowering: raw rule/scene values → runtime `Trigger` / `Condition` / `Command`.
//!
//! Each node is the terse single-key map (`{ occupancy: motion }`,
//! `{ set_brightness: { device, value } }`). We dispatch on the key by hand —
//! serde's externally-tagged enums can't read this form from YAML — and use
//! `serde_yaml::from_value` only for the multi-field payloads.
//!
//! Every device name resolves to a `DeviceId`, every scene to a `SceneId`, every
//! capability reference is checked against what the device declares, and
//! durations/times are parsed. Failures become diagnostics; the pass keeps going
//! so one compile reports them all. These static checks are exactly the runtime
//! invariants the engine assumes (missing capability → silent no-op; condition
//! on a never-reported capability → permanently `Unknown`; cancel of an
//! unscheduled timer → dangling reference).

use std::collections::{HashMap, HashSet};

use serde::de::DeserializeOwned;
use serde_yaml::Value;

use crate::compile::ast::{
    RawDecreaseBrightness, RawIncreaseBrightness, RawOccupancyIs, RawScheduleTimer,
    RawSetBrightness, RawSetColor, RawSetColorTemperature, RawSwitchIs,
};
use crate::compile::diagnostic::Diagnostic;
use crate::compile::resolve::DeviceDef;
use crate::ids::{ActionId, DeviceId, SceneId, ScheduleId};
use crate::model::{CapabilityKind, Command, Millis, TimerKey};
use crate::rule::{CmpOp, Condition, Trigger};

/// Shared resolution context for one whole lowering phase (all scenes + rules).
pub(crate) struct Lowerer<'a> {
    pub devices: &'a [DeviceDef],
    pub device_index: &'a HashMap<String, DeviceId>,
    pub scene_index: &'a HashMap<String, SceneId>,
    pub schedule_index: &'a HashMap<String, ScheduleId>,
    /// Synthetic device backing `sun_up` / `time_*` conditions.
    pub clock_device: DeviceId,
    pub diags: &'a mut Vec<Diagnostic>,
    /// Timer keys some `schedule_timer` creates, vs. keys triggers/cancels use.
    pub scheduled_keys: &'a mut HashSet<String>,
    pub referenced_keys: &'a mut HashSet<String>,
    pub used_scenes: &'a mut HashSet<SceneId>,
    /// Schedules a `schedule:` trigger references, for the unused-schedule lint.
    pub referenced_schedules: &'a mut HashSet<ScheduleId>,
}

impl Lowerer<'_> {
    fn error(&mut self, code: &'static str, msg: String, at: &str) {
        self.diags
            .push(Diagnostic::error(code, msg).at(at.to_string()));
    }

    // --- node shape helpers -------------------------------------------------

    /// Pull the single `{ verb: payload }` entry out of a node.
    fn single_entry(&mut self, v: &Value, node: &str, at: &str) -> Option<(String, Value)> {
        if let Value::Mapping(m) = v {
            if m.len() == 1 {
                if let Some((k, val)) = m.iter().next() {
                    if let Some(k) = k.as_str() {
                        return Some((k.to_string(), val.clone()));
                    }
                }
            }
        }
        self.error(
            "E_BAD_NODE",
            format!("{node} must be a single-key map like {{ verb: target }}"),
            at,
        );
        None
    }

    fn as_name(&mut self, v: &Value, node: &str, at: &str) -> Option<String> {
        match v.as_str() {
            Some(s) => Some(s.to_string()),
            None => {
                self.error("E_BAD_NODE", format!("'{node}' expects a name"), at);
                None
            }
        }
    }

    fn as_flag(&mut self, v: &Value, node: &str, at: &str) -> Option<bool> {
        match v.as_bool() {
            Some(b) => Some(b),
            None => {
                self.error("E_BAD_NODE", format!("'{node}' expects true/false"), at);
                None
            }
        }
    }

    fn payload<T: DeserializeOwned>(&mut self, v: Value, node: &str, at: &str) -> Option<T> {
        match serde_yaml::from_value::<T>(v) {
            Ok(t) => Some(t),
            Err(e) => {
                self.error("E_BAD_NODE", format!("invalid '{node}': {e}"), at);
                None
            }
        }
    }

    // --- reference + capability resolution ----------------------------------

    fn resolve_device(&mut self, name: &str, at: &str) -> Option<DeviceId> {
        match self.device_index.get(name) {
            Some(&id) => Some(id),
            None => {
                self.error("E_UNKNOWN_DEVICE", format!("unknown device '{name}'"), at);
                None
            }
        }
    }

    /// Verify a device actually declares the capability a usage needs. The
    /// synthetic clock device implicitly provides time/sun, so it is exempt.
    fn require_cap(
        &mut self,
        id: DeviceId,
        name: &str,
        cap: CapabilityKind,
        usage: &str,
        at: &str,
    ) {
        if id == self.clock_device {
            return;
        }
        let has = self
            .devices
            .get(id.0 as usize)
            .map(|d| d.capabilities.contains(&cap))
            .unwrap_or(false);
        if !has {
            self.error(
                "E_MISSING_CAPABILITY",
                format!(
                    "'{usage}' needs capability {cap:?}, which device '{name}' does not declare"
                ),
                at,
            );
        }
    }

    // --- triggers -----------------------------------------------------------

    pub(crate) fn trigger(&mut self, v: &Value, at: &str) -> Option<Trigger> {
        let (verb, payload) = self.single_entry(v, "trigger", at)?;
        Some(match verb.as_str() {
            "event" => {
                let spec = self.as_name(&payload, "event", at)?;
                let Some((dev_name, event_name)) = spec.split_once('.') else {
                    self.error(
                        "E_BAD_NODE",
                        format!("event trigger must be '<device>.<event>', got '{spec}'"),
                        at,
                    );
                    return None;
                };
                let device = self.resolve_device(dev_name, at)?;
                let action = self.resolve_event(device, dev_name, event_name, at)?;
                Trigger::Action { device, action }
            }
            "occupancy" | "occupancy_clear" => {
                let occupied = verb == "occupancy";
                let name = self.as_name(&payload, &verb, at)?;
                let device = self.resolve_device(&name, at)?;
                self.require_cap(device, &name, CapabilityKind::Occupancy, &verb, at);
                Trigger::Occupancy { device, occupied }
            }
            "timer" => {
                let key = self.as_name(&payload, "timer", at)?;
                self.referenced_keys.insert(key.clone());
                Trigger::Timer { key: TimerKey(key) }
            }
            "schedule" => {
                let name = self.as_name(&payload, "schedule", at)?;
                match self.schedule_index.get(&name) {
                    Some(&schedule) => {
                        self.referenced_schedules.insert(schedule);
                        Trigger::Time { schedule }
                    }
                    None => {
                        self.error(
                            "E_UNKNOWN_SCHEDULE",
                            format!("unknown schedule '{name}'"),
                            at,
                        );
                        return None;
                    }
                }
            }
            "command_failed" => {
                let spec = self.as_name(&payload, "command_failed", at)?;
                let device = if spec == "*" {
                    None
                } else {
                    Some(self.resolve_device(&spec, at)?)
                };
                Trigger::CommandFailed { device }
            }
            other => {
                self.error(
                    "E_UNKNOWN_TRIGGER",
                    format!("unknown trigger '{other}'"),
                    at,
                );
                return None;
            }
        })
    }

    /// Resolve `<device>.<event>` against the device's declared `events:` map.
    /// The event name must be one the device declares — that's what makes button
    /// triggers statically checkable (`event: wall.bogus` is a compile error).
    fn resolve_event(
        &mut self,
        device: DeviceId,
        dev_name: &str,
        event: &str,
        at: &str,
    ) -> Option<ActionId> {
        let dev = self.devices.get(device.0 as usize)?;
        match dev.events.iter().find(|e| e.name == event) {
            Some(e) => Some(e.id),
            None => {
                self.error(
                    "E_UNKNOWN_EVENT",
                    format!("device '{dev_name}' declares no event '{event}'"),
                    at,
                );
                None
            }
        }
    }

    // --- conditions ---------------------------------------------------------

    pub(crate) fn condition(&mut self, v: &Value, at: &str) -> Option<Condition> {
        let (verb, payload) = self.single_entry(v, "condition", at)?;
        Some(match verb.as_str() {
            "all" => Condition::And(self.condition_list(&payload, &verb, at)?),
            "any" => Condition::Or(self.condition_list(&payload, &verb, at)?),
            "not" => Condition::Not(Box::new(self.condition(&payload, at)?)),
            "sun_up" => Condition::BoolEquals {
                device: self.clock_device,
                kind: CapabilityKind::SunUp,
                value: self.as_flag(&payload, "sun_up", at)?,
            },
            "switch" => {
                let s: RawSwitchIs = self.payload(payload, "switch", at)?;
                let device = self.resolve_device(&s.device, at)?;
                self.require_cap(device, &s.device, CapabilityKind::Switch, "switch", at);
                Condition::BoolEquals {
                    device,
                    kind: CapabilityKind::Switch,
                    value: s.on,
                }
            }
            "occupancy_is" => {
                let o: RawOccupancyIs = self.payload(payload, "occupancy_is", at)?;
                let device = self.resolve_device(&o.device, at)?;
                self.require_cap(
                    device,
                    &o.device,
                    CapabilityKind::Occupancy,
                    "occupancy_is",
                    at,
                );
                Condition::BoolEquals {
                    device,
                    kind: CapabilityKind::Occupancy,
                    value: o.occupied,
                }
            }
            "time_after" => {
                let t = self.as_name(&payload, "time_after", at)?;
                Condition::Compare {
                    device: self.clock_device,
                    kind: CapabilityKind::TimeOfDay,
                    op: CmpOp::Ge,
                    value: self.parse_time(&t, at)? as i64,
                }
            }
            "time_before" => {
                let t = self.as_name(&payload, "time_before", at)?;
                Condition::Compare {
                    device: self.clock_device,
                    kind: CapabilityKind::TimeOfDay,
                    op: CmpOp::Lt,
                    value: self.parse_time(&t, at)? as i64,
                }
            }
            other => {
                self.error(
                    "E_UNKNOWN_CONDITION",
                    format!("unknown condition '{other}'"),
                    at,
                );
                return None;
            }
        })
    }

    fn condition_list(&mut self, v: &Value, node: &str, at: &str) -> Option<Vec<Condition>> {
        let Some(seq) = v.as_sequence() else {
            self.error(
                "E_BAD_NODE",
                format!("'{node}' expects a list of conditions"),
                at,
            );
            return None;
        };
        let mut out = Vec::with_capacity(seq.len());
        let mut ok = true;
        for item in seq {
            match self.condition(item, at) {
                Some(c) => out.push(c),
                None => ok = false,
            }
        }
        ok.then_some(out)
    }

    // --- commands -----------------------------------------------------------

    pub(crate) fn commands(&mut self, raws: &[Value], at: &str) -> Vec<Command> {
        raws.iter().filter_map(|v| self.command(v, at)).collect()
    }

    fn command(&mut self, v: &Value, at: &str) -> Option<Command> {
        let (verb, payload) = self.single_entry(v, "command", at)?;
        Some(match verb.as_str() {
            "turn_on" | "turn_off" => {
                let on = verb == "turn_on";
                let name = self.as_name(&payload, &verb, at)?;
                let device = self.resolve_device(&name, at)?;
                self.require_cap(device, &name, CapabilityKind::Switch, &verb, at);
                Command::SetSwitch { device, on }
            }
            "toggle" => {
                let name = self.as_name(&payload, "toggle", at)?;
                let device = self.resolve_device(&name, at)?;
                self.require_cap(device, &name, CapabilityKind::Switch, "toggle", at);
                Command::ToggleSwitch { device }
            }
            "set_brightness" => {
                let b: RawSetBrightness = self.payload(payload, "set_brightness", at)?;
                let device = self.resolve_device(&b.device, at)?;
                self.require_cap(
                    device,
                    &b.device,
                    CapabilityKind::Brightness,
                    "set_brightness",
                    at,
                );
                let transition = match &b.transition {
                    Some(s) => Some(self.parse_duration(s, at)?),
                    None => None,
                };
                Command::SetBrightness {
                    device,
                    value: b.value,
                    transition,
                }
            }
            "decrease_brightness" => {
                let b: RawDecreaseBrightness = self.payload(payload, "decrease_brightness", at)?;
                let device = self.resolve_device(&b.device, at)?;
                self.require_cap(
                    device,
                    &b.device,
                    CapabilityKind::Brightness,
                    "decrease_brightness",
                    at,
                );
                Command::DecreaseBrightness {
                    device,
                    value: b.value,
                }
            }
            "increase_brightness" => {
                let b: RawIncreaseBrightness = self.payload(payload, "increase_brightness", at)?;
                let device = self.resolve_device(&b.device, at)?;
                self.require_cap(
                    device,
                    &b.device,
                    CapabilityKind::Brightness,
                    "increase_brightness",
                    at,
                );
                Command::IncreaseBrightness {
                    device,
                    value: b.value,
                }
            }
            "set_color" => {
                let c: RawSetColor = self.payload(payload, "set_color", at)?;
                let device = self.resolve_device(&c.device, at)?;
                self.require_cap(device, &c.device, CapabilityKind::Color, "set_color", at);
                let (r, g, b) = self.parse_color(&c.color, at)?;
                let transition = match &c.transition {
                    Some(s) => Some(self.parse_duration(s, at)?),
                    None => None,
                };
                Command::SetColor {
                    device,
                    r,
                    g,
                    b,
                    transition,
                }
            }
            "set_color_temperature" => {
                let c: RawSetColorTemperature =
                    self.payload(payload, "set_color_temperature", at)?;
                let device = self.resolve_device(&c.device, at)?;
                self.require_cap(
                    device,
                    &c.device,
                    CapabilityKind::ColorTemperature,
                    "set_color_temperature",
                    at,
                );
                let mireds = self.parse_color_temperature(&c, at)?;
                let transition = match &c.transition {
                    Some(s) => Some(self.parse_duration(s, at)?),
                    None => None,
                };
                Command::SetColorTemperature {
                    device,
                    mireds,
                    transition,
                }
            }
            "activate_scene" => {
                let name = self.as_name(&payload, "activate_scene", at)?;
                match self.scene_index.get(&name) {
                    Some(&scene) => {
                        self.used_scenes.insert(scene);
                        Command::ActivateScene { scene }
                    }
                    None => {
                        self.error("E_UNKNOWN_SCENE", format!("unknown scene '{name}'"), at);
                        return None;
                    }
                }
            }
            "schedule_timer" => {
                let s: RawScheduleTimer = self.payload(payload, "schedule_timer", at)?;
                self.scheduled_keys.insert(s.key.clone());
                let after = self.parse_duration(&s.after, at)?;
                Command::ScheduleTimer {
                    key: TimerKey(s.key),
                    after,
                }
            }
            "cancel_timer" => {
                let key = self.as_name(&payload, "cancel_timer", at)?;
                self.referenced_keys.insert(key.clone());
                Command::CancelTimer { key: TimerKey(key) }
            }
            other => {
                self.error(
                    "E_UNKNOWN_COMMAND",
                    format!("unknown command '{other}'"),
                    at,
                );
                return None;
            }
        })
    }

    // --- scalar parsing -----------------------------------------------------

    fn parse_duration(&mut self, s: &str, at: &str) -> Option<Millis> {
        let trimmed = s.trim();
        if let Some(idx) = trimmed.find(|c: char| c.is_ascii_alphabetic()) {
            let (num, unit) = trimmed.split_at(idx);
            if let Ok(n) = num.trim().parse::<u64>() {
                let mult = match unit {
                    "ms" => Some(1),
                    "s" => Some(1000),
                    "m" => Some(60_000),
                    "h" => Some(3_600_000),
                    _ => None,
                };
                if let Some(ms) = mult.and_then(|m| n.checked_mul(m)) {
                    return Some(ms);
                }
            }
        }
        self.error(
            "E_BAD_DURATION",
            format!("invalid duration '{s}' (expected e.g. 30s, 10m, 1h)"),
            at,
        );
        None
    }

    fn parse_time(&mut self, s: &str, at: &str) -> Option<u16> {
        if let Some((h, m)) = s.trim().split_once(':') {
            if let (Ok(h), Ok(m)) = (h.trim().parse::<u16>(), m.trim().parse::<u16>()) {
                if h < 24 && m < 60 {
                    return Some(h * 60 + m);
                }
            }
        }
        self.error(
            "E_BAD_TIME",
            format!("invalid time '{s}' (expected HH:MM)"),
            at,
        );
        None
    }

    /// Parse a chromatic color from `#RRGGBB` or `{ r, g, b }` into sRGB bytes.
    fn parse_color(&mut self, v: &Value, at: &str) -> Option<(u8, u8, u8)> {
        if let Some(s) = v.as_str() {
            return match crate::color::hex_to_rgb(s) {
                Some(rgb) => Some(rgb),
                None => {
                    self.error(
                        "E_BAD_COLOR",
                        format!("invalid hex color '{s}' (expected `#RRGGBB`)"),
                        at,
                    );
                    None
                }
            };
        }
        if let Value::Mapping(m) = v {
            let r = m.get("r").and_then(Value::as_u64);
            let g = m.get("g").and_then(Value::as_u64);
            let b = m.get("b").and_then(Value::as_u64);
            if let (Some(r), Some(g), Some(b)) = (r, g, b) {
                if r <= 255 && g <= 255 && b <= 255 {
                    return Some((r as u8, g as u8, b as u8));
                }
            }
        }
        self.error(
            "E_BAD_COLOR",
            "color must be a `#RRGGBB` hex string or `{ r, g, b }` object with values 0–255".into(),
            at,
        );
        None
    }

    /// Resolve exactly one of `kelvin` or `mireds` to mireds.
    fn parse_color_temperature(&mut self, raw: &RawSetColorTemperature, at: &str) -> Option<u16> {
        let has_kelvin = raw.kelvin.is_some();
        let has_mireds = raw.mireds.is_some();
        match (has_kelvin, has_mireds) {
            (true, true) => {
                self.error(
                    "E_BAD_COLOR_TEMP",
                    "set_color_temperature must set exactly one of `kelvin` or `mireds`".into(),
                    at,
                );
                None
            }
            (false, false) => {
                self.error(
                    "E_BAD_COLOR_TEMP",
                    "set_color_temperature needs one of `kelvin` or `mireds`".into(),
                    at,
                );
                None
            }
            (true, false) => {
                let kelvin = raw.kelvin.unwrap();
                if !(1000..=10_000).contains(&kelvin) {
                    self.error(
                        "E_BAD_COLOR_TEMP",
                        format!("kelvin {kelvin} out of range (expected 1000–10000)"),
                        at,
                    );
                    return None;
                }
                Some((1_000_000 / kelvin).min(u16::MAX as u32) as u16)
            }
            (false, true) => {
                let mireds = raw.mireds.unwrap();
                if !(100..=1000).contains(&mireds) {
                    self.error(
                        "E_BAD_COLOR_TEMP",
                        format!("mireds {mireds} out of range (expected 100–1000)"),
                        at,
                    );
                    return None;
                }
                Some(mireds)
            }
        }
    }
}
