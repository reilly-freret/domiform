//! The parse layer: serde-deserializable mirrors of the config file.
//!
//! This is the *only* place the file format leaks in. These types are
//! intentionally dumb — they hold strings and (for rule nodes) raw YAML values,
//! doing no validation. Turning them into a resolved, reference-checked graph is
//! `resolve`/`lower`'s job.
//!
//! Rule triggers/conditions/commands are kept as raw `serde_yaml::Value`s rather
//! than typed enums: their natural form is the terse single-key map
//! (`{ turn_on: hallway_light }`), which serde's externally-tagged enums do not
//! deserialize from. `lower` interprets those maps directly, with full control
//! over diagnostics.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_yaml::Value;

/// Top of the file. `deny_unknown_fields` turns a mistyped section name into a
/// compile error instead of a silent no-op.
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
    #[serde(default)]
    pub rules: BTreeMap<String, RawRule>,
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
}

/// An adapter, discriminated by its `type` field (internally tagged — which
/// serde_yaml *does* deserialize from maps). Connection details live here, with
/// the adapter that uses them — not in `system`.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RawAdapter {
    Zigbee2mqtt {
        /// Broker URL, e.g. `mqtt://mosquitto:1883`.
        url: String,
        #[serde(default = "default_base_topic")]
        base_topic: String,
    },
    /// A Matter controller (`python-matter-server`) over WebSocket.
    Matter {
        /// Controller WebSocket URL, e.g. `ws://host:5580/ws`.
        url: String,
    },
    /// In-memory adapter, for tests and bring-up.
    Mock,
}

fn default_base_topic() -> String {
    "zigbee2mqtt".to_string()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawDevice {
    pub adapter: String,
    #[serde(default)]
    pub address: Option<String>,
    /// Matter endpoint (ignored by other protocols). Defaults to 1 when omitted.
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
    pub value: u8,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawIncreaseBrightness {
    pub device: String,
    pub value: u8,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawScheduleTimer {
    pub key: String,
    /// Delay like `10m` / `30s` / `1h`.
    pub after: String,
}
