//! One wire spelling for `Trigger`, `Condition` and `Command`.
//!
//! `GET /rules`, `GET /scenes` and `GET /stream` all render the same runtime
//! types, so the rendering lives here once rather than being reinvented per
//! endpoint — the same reasoning that put [`CapabilityKind::name`] in `model.rs`
//! instead of in the config resolver and the REST layer separately.
//!
//! Every id is resolved back to the name it was written under in config, via the
//! [`Directory`]. The output is *structured*, not prose: a client's real query is
//! "find every rule triggered by an event, and render a button for it", which is
//! a filter on `trigger.type`, not a string match.
//!
//! # Two deliberate omissions
//!
//! * **`SendIrCode` does not carry its `code`.** The codes are 100+ character
//!   base64 blobs that would dominate a `/rules` payload, and a client never
//!   needs one — reaching IR *without* handling the code is the entire point of
//!   the events surface. The device is reported so the UI can still say what the
//!   command touches.
//! * **Nothing here can fail or panic.** These functions run on the engine thread
//!   when the stream renders a rule fire, so an unknown id falls back to a
//!   synthetic `device#7` label (see `Directory::device_name`) rather than
//!   unwrapping. See the module docs on the observer path in `stream.rs`.
//!
//! [`CapabilityKind::name`]: crate::model::CapabilityKind::name

use serde_json::{json, Value};

use crate::model::Command;
use crate::rule::{CmpOp, Condition, CrossDir, Rule, Trigger};

use super::Directory;

/// The wire spelling of a rule's trigger.
pub fn trigger(directory: &Directory, trigger: &Trigger) -> Value {
    match trigger {
        Trigger::Action { device, action } => json!({
            "type": "event",
            "device": directory.device_name(*device),
            "event": directory.action_name(*device, *action),
        }),
        Trigger::Changed { device, kind, to } => json!({
            "type": "changed",
            "device": directory.device_name(*device),
            "capability": kind.name(),
            "to": to,
        }),
        Trigger::Crosses {
            device,
            kind,
            bound,
            dir,
        } => json!({
            "type": "crosses",
            "device": directory.device_name(*device),
            "capability": kind.name(),
            "bound": bound,
            "dir": match dir { CrossDir::Above => "above", CrossDir::Below => "below" },
        }),
        Trigger::Reports { device, kind } => json!({
            "type": "reports",
            "device": directory.device_name(*device),
            "capability": kind.name(),
        }),
        Trigger::Timer { key } => json!({ "type": "timer", "key": key.0 }),
        Trigger::Time { schedule } => json!({
            "type": "time",
            "schedule": directory.schedule_name(*schedule),
        }),
        // `device: None` is the match-any form; it renders as an explicit null
        // rather than an absent key, so a client can tell the two apart.
        Trigger::CommandFailed { device } => json!({
            "type": "command_failed",
            "device": device.map(|d| directory.device_name(d)),
        }),
    }
}

/// The wire spelling of a rule's condition. `Always` renders as literal `true`
/// rather than an object: it is the overwhelmingly common case (a rule with no
/// `if:`), and `"condition": true` reads better than a tagged empty node.
pub fn condition(directory: &Directory, condition: &Condition) -> Value {
    match condition {
        Condition::Always => json!(true),
        Condition::Not(inner) => json!({
            "type": "not",
            "of": self::condition(directory, inner),
        }),
        Condition::And(cs) => json!({
            "type": "and",
            "of": cs.iter().map(|c| self::condition(directory, c)).collect::<Vec<_>>(),
        }),
        Condition::Or(cs) => json!({
            "type": "or",
            "of": cs.iter().map(|c| self::condition(directory, c)).collect::<Vec<_>>(),
        }),
        Condition::BoolEquals {
            device,
            kind,
            value,
        } => json!({
            "type": "bool_equals",
            "device": directory.device_name(*device),
            "capability": kind.name(),
            "value": value,
        }),
        Condition::Compare {
            device,
            kind,
            op,
            value,
        } => json!({
            "type": "compare",
            "device": directory.device_name(*device),
            "capability": kind.name(),
            "op": cmp_op(*op),
            "value": value,
        }),
        Condition::ColorEquals { device, r, g, b } => json!({
            "type": "color_equals",
            "device": directory.device_name(*device),
            "color": { "r": r, "g": g, "b": b },
        }),
    }
}

fn cmp_op(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Lt => "lt",
        CmpOp::Le => "le",
        CmpOp::Eq => "eq",
        CmpOp::Ne => "ne",
        CmpOp::Ge => "ge",
        CmpOp::Gt => "gt",
    }
}

/// The wire spelling of one command.
pub fn command(directory: &Directory, command: &Command) -> Value {
    match command {
        Command::SetSwitch { device, on } => json!({
            "type": "set_switch",
            "device": directory.device_name(*device),
            "on": on,
        }),
        Command::ToggleSwitch { device } => json!({
            "type": "toggle_switch",
            "device": directory.device_name(*device),
        }),
        Command::SetBrightness {
            device,
            value,
            transition,
        } => json!({
            "type": "set_brightness",
            "device": directory.device_name(*device),
            "value": value,
            "transition_ms": transition,
        }),
        Command::IncreaseBrightness { device, value } => json!({
            "type": "increase_brightness",
            "device": directory.device_name(*device),
            "by": value,
        }),
        Command::DecreaseBrightness { device, value } => json!({
            "type": "decrease_brightness",
            "device": directory.device_name(*device),
            "by": value,
        }),
        Command::SetColor {
            device,
            r,
            g,
            b,
            transition,
        } => json!({
            "type": "set_color",
            "device": directory.device_name(*device),
            "color": { "r": r, "g": g, "b": b },
            "transition_ms": transition,
        }),
        Command::SetColorTemperature {
            device,
            mireds,
            transition,
        } => json!({
            "type": "set_color_temperature",
            "device": directory.device_name(*device),
            "mireds": mireds,
            "transition_ms": transition,
        }),
        Command::ActivateScene { scene } => json!({
            "type": "activate_scene",
            "scene": directory.scene_name(*scene),
        }),
        Command::ScheduleTimer { key, after } => json!({
            "type": "schedule_timer",
            "key": key.0,
            "after_ms": after,
        }),
        Command::CancelTimer { key } => json!({
            "type": "cancel_timer",
            "key": key.0,
        }),
        // Deliberately omits `code` — see the module docs.
        Command::SendIrCode { device, .. } => json!({
            "type": "send_ir_code",
            "device": directory.device_name(*device),
        }),
    }
}

/// Render a command list, for a rule's `then:` or a scene's members.
pub fn commands(directory: &Directory, cmds: &[Command]) -> Vec<Value> {
    cmds.iter().map(|c| command(directory, c)).collect()
}

/// The static shape of a rule: everything that comes from config, with no
/// runtime status. `GET /rules` merges this with the mirror's [`RuleStatus`].
///
/// [`RuleStatus`]: super::RuleStatus
pub fn rule_shape(directory: &Directory, rule: &Rule) -> Value {
    json!({
        "name": rule.name,
        "trigger": trigger(directory, &rule.trigger),
        "condition": condition(directory, &rule.condition),
        "then": commands(directory, &rule.commands),
        "for_ms": rule.for_duration,
    })
}
