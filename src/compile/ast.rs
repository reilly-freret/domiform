//! The parse layer: serde-deserializable mirrors of the config file.
//!
//! This is the *only* place the file format leaks in. These types are
//! intentionally dumb — they hold strings and (for rule nodes) raw YAML values,
//! doing no validation. Turning them into a resolved, reference-checked graph is
//! [`resolve`](super::resolve) / [`lower`](super::lower)'s job.
//!
//! Rule triggers/conditions/commands are kept as raw `serde_yaml::Value`s rather
//! than typed enums: their natural form is the terse single-key map
//! (`{ turn_on: hallway_light }`), which serde's externally-tagged enums do not
//! deserialize from. `lower` interprets those maps directly, with full control
//! over diagnostics.
//!
//! # Extending the config language
//!
//! What to touch depends on the change:
//!
//! * **New top-level section** (e.g. a `groups:` block) — add a field to
//!   [`RawConfig`] with `#[serde(default)]`, then teach [`resolve`](super::resolve)
//!   to walk it and extend [`CompiledConfig`](super::resolve::CompiledConfig).
//!   Update `schema/domiform.schema.json` so editors and `--check` stay aligned.
//! * **Top-level `x-*` YAML extensions** (anchor definitions, etc.) — allowed by
//!   the schema and stripped in [`parse_raw_config`]; no other compiler changes.
//! * **New rule trigger / condition / command form** — usually no AST change;
//!   add a dispatch arm in [`lower`](super::lower) for the new single-key map
//!   and a small typed payload struct here if the value has multiple fields.
//! * **New adapter `type:` or adapter config fields** — *not* here. Adapters own
//!   their config shape via [`AdapterPlugin::validate_config`](crate::adapters::AdapterPlugin::validate_config)
//!   and deserialize with [`config_of`](crate::adapters::config_of); only the
//!   generic [`RawAdapter::config`](RawAdapter) blob passes through.
//! * **New device field shared by all adapters** — extend [`RawDevice`] and
//!   [`DeviceDef`](super::resolve::DeviceDef) in `resolve`.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_yaml::Value;

/// Top of the file. `deny_unknown_fields` turns a mistyped section name into a
/// compile error instead of a silent no-op. Top-level `x-*` keys are allowed as
/// YAML extension/anchor definitions (see `parse_raw_config`); they are stripped
/// before deserialization and ignored by the compiler.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawConfig {
    #[serde(default)]
    pub system: RawSystem,
    /// `BTreeMap` everywhere gives deterministic, name-sorted iteration so
    /// compiled ids never depend on file ordering.
    #[serde(default)]
    pub adapters: BTreeMap<String, RawAdapter>,
    #[serde(default)]
    pub devices: BTreeMap<String, RawDevice>,
    #[serde(default)]
    pub scenes: BTreeMap<String, Vec<Value>>,
    /// Wall-clock schedules that fire `TimeReached` events. `BTreeMap` keeps
    /// `ScheduleId` interning deterministic, like every other section.
    #[serde(default)]
    pub schedules: BTreeMap<String, RawSchedule>,
    #[serde(default)]
    pub rules: BTreeMap<String, RawRule>,
}

/// Parse config YAML into [`RawConfig`]. Top-level `x-*` keys (YAML anchor
/// definitions and similar) are allowed per the schema's `patternProperties` and
/// stripped after anchor resolution — they never reach `deny_unknown_fields`.
pub fn parse_raw_config(src: &str) -> Result<RawConfig, serde_yaml::Error> {
    let mut value: Value = serde_yaml::from_str(src)?;
    if let Value::Mapping(ref mut map) = value {
        map.retain(|k, _| match k.as_str() {
            Some(s) => !s.starts_with("x-"),
            None => true,
        });
    }
    serde_yaml::from_value(value)
}

/// One schedule entry. Exactly one field is set: `cron` is the raw 5-field escape
/// hatch; `daily`/`weekday`/`weekend` are `"HH:MM"` sugar that desugars to cron in
/// `resolve`. `deny_unknown_fields` turns a mistyped key into a compile error;
/// "exactly one" is checked in `resolve` (serde can't express it here).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSchedule {
    #[serde(default)]
    pub cron: Option<String>,
    #[serde(default)]
    pub daily: Option<String>,
    #[serde(default)]
    pub weekday: Option<String>,
    #[serde(default)]
    pub weekend: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSystem {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub timezone: Option<String>,
    #[serde(default)]
    pub latitude: Option<f64>,
    #[serde(default)]
    pub longitude: Option<f64>,
    /// Directory under which features that strictly require runtime state (e.g.
    /// the `matter_device` adapter's Matter fabric/commissioning store) keep their
    /// files. This is *runtime data*, not configuration — see the reproducibility
    /// note in `docs/design/northbound-adapters.md`. When unset, the host defaults
    /// it to the config file's own directory (stable regardless of cwd).
    #[serde(default)]
    pub runtime_storage_path: Option<String>,
}

/// One adapter entry: its `type` discriminator plus the remaining
/// protocol-specific fields, kept as a raw value for the adapter's own plugin to
/// validate. Deliberately *not* an enum-with-a-variant-per-protocol: that would
/// force every new adapter to edit this file. Instead the compiler stays
/// adapter-agnostic and each protocol lives entirely in `src/adapters/`, keyed
/// by its `type` in the adapter registry (`adapters::plugins`).
#[derive(Debug, Deserialize)]
pub struct RawAdapter {
    #[serde(rename = "type")]
    pub kind: String,
    /// Every field besides `type`, captured verbatim. The adapter's
    /// `AdapterPlugin::validate_config` deserializes this into its own typed,
    /// `deny_unknown_fields` config — so a mistyped key is still a compile error,
    /// it's just caught by the adapter rather than by this enum.
    #[serde(flatten)]
    pub config: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawDevice {
    pub adapter: String,
    #[serde(default)]
    pub address: Option<String>,
    /// Sub-device endpoint. Matter: application endpoint (default 1). Z-Wave: load
    /// endpoint on a multi-relay module (default 0). zigbee2mqtt: multi-gang load
    /// index — 1 = `state_l1`, 2 = `state_l2`, …; omitted = single-load `state`.
    #[serde(default)]
    pub endpoint: Option<u16>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Stateless events this device can emit, as `<local name>: <raw protocol
    /// string>` (e.g. `top: toggle_l1`). Rules trigger on the local name; the
    /// adapter matches the raw string. `BTreeMap` keeps interning deterministic.
    #[serde(default)]
    pub events: BTreeMap<String, String>,
    #[serde(default)]
    pub manufacturer: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub room: Option<String>,
}

/// A rule: one trigger, an optional guard, and an ordered command list — held as
/// raw values and interpreted by `lower`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawRule {
    pub when: Value,
    /// Optional guard. `if:` in YAML (renamed to dodge the Rust keyword).
    #[serde(default, rename = "if")]
    pub condition: Option<Value>,
    #[serde(default)]
    pub then: Vec<Value>,
}

// --- typed payloads for the multi-field rule nodes --------------------------
// These are plain structs (deserialized via `from_value`), so the map form works
// fine — only the externally-tagged outer dispatch needed hand-parsing.

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSwitchIs {
    pub device: String,
    #[serde(rename = "is_on")]
    pub on: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawOccupancyIs {
    pub device: String,
    pub occupied: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSetBrightness {
    pub device: String,
    pub value: u8,
    /// Fade duration like `2s` / `500ms`. Pushed to the adapter, not sequenced.
    #[serde(default)]
    pub transition: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawDecreaseBrightness {
    pub device: String,
    #[serde(rename = "by")]
    pub value: u8,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawIncreaseBrightness {
    pub device: String,
    #[serde(rename = "by")]
    pub value: u8,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSetColor {
    pub device: String,
    /// Chromatic color: a `#RRGGBB` hex string or `{ r, g, b }` object.
    pub color: Value,
    #[serde(default)]
    pub transition: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSetColorTemperature {
    pub device: String,
    #[serde(default)]
    pub kelvin: Option<u32>,
    #[serde(default)]
    pub mireds: Option<u16>,
    #[serde(default)]
    pub transition: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawScheduleTimer {
    pub key: String,
    /// Delay like `10m` / `30s` / `1h`.
    pub after: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSendIrCode {
    pub device: String,
    /// Pre-learned IR payload in base64 (zigbee2mqtt `ir_code_to_send` form).
    pub code: String,
}
