//! Pure request → response routing: the seam that makes the API testable
//! without a socket, and where the bulk of the tests live.
//!
//! [`handle`] takes a method, a path and a body, and returns a [`Response`]. It
//! performs no I/O of its own: reads come from the [`RestApiHandle`]'s mirror and
//! writes go into its channel. The connection thread does I/O and nothing else.
//!
//! Every response body is JSON. Errors all share one shape:
//!
//! ```json
//! { "error": { "code": "unknown_device", "message": "no device named 'kitchen_lmap'" } }
//! ```

use serde_json::{json, Map, Value};

use crate::model::{CapabilityKind, Desired};

use super::json::{capabilities_object, state_from_json};
use super::{describe, DeviceEntry, Directory, Inbound, RestApiHandle};

/// A routed response. `body` is always JSON.
#[derive(Clone, Debug, PartialEq)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Response {
    fn json(status: u16, value: Value) -> Self {
        Response {
            status,
            body: serde_json::to_vec(&value).unwrap_or_else(|_| {
                br#"{"error":{"code":"internal","message":"failed to encode response"}}"#.to_vec()
            }),
        }
    }

    /// The one error shape every failure uses.
    pub fn error(status: u16, code: &str, message: impl Into<String>) -> Self {
        Self::json(
            status,
            json!({ "error": { "code": code, "message": message.into() } }),
        )
    }

    /// Parse the body back to JSON. For tests and the server's own logging.
    pub fn json_body(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

/// Route one request. `path` is the raw path with any query string already
/// stripped by the caller.
pub fn handle(
    directory: &Directory,
    handle: &RestApiHandle,
    method: &str,
    path: &str,
    body: &[u8],
) -> Response {
    // Split on '/' and ignore empty segments, so `/devices/` and `/devices` are
    // the same route. No regex, no router crate.
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    match segments.as_slice() {
        ["system"] => get_only(method, || system(directory, handle)),
        ["devices"] => get_only(method, || devices(directory, handle)),
        ["devices", name] => get_only(method, || device(directory, handle, name)),
        ["devices", name, "intent"] => post_only(method, || intent(directory, handle, name, body)),
        ["devices", name, "events", event] => {
            post_only(method, || fire_event(directory, handle, name, event))
        }
        ["scenes"] => get_only(method, || scenes(directory)),
        ["scenes", name] => get_only(method, || scene(directory, name)),
        ["scenes", name, "activate"] => post_only(method, || activate(directory, handle, name)),
        ["rules"] => get_only(method, || rules(directory, handle)),
        ["rules", name] => get_only(method, || rule(directory, handle, name)),
        _ => Response::error(404, "unknown_route", format!("no route for '{path}'")),
    }
}

/// A known path that only serves `GET`. Anything else is a `405`, which tells a
/// client "your URL is right, your verb is wrong" — distinctly more useful than
/// a `404`.
fn get_only(method: &str, f: impl FnOnce() -> Response) -> Response {
    match method {
        "GET" => f(),
        other => method_not_allowed(other, "GET"),
    }
}

fn post_only(method: &str, f: impl FnOnce() -> Response) -> Response {
    match method {
        "POST" => f(),
        other => method_not_allowed(other, "POST"),
    }
}

fn method_not_allowed(got: &str, allowed: &str) -> Response {
    Response::error(
        405,
        "method_not_allowed",
        format!("{got} is not allowed here; use {allowed}"),
    )
}

// --- read endpoints ----------------------------------------------------------

fn system(directory: &Directory, handle: &RestApiHandle) -> Response {
    Response::json(
        200,
        json!({
            "name": directory.system_name,
            "timezone": directory.timezone,
            // Virtual engine time since boot — for the real-time host this tracks
            // wall-clock elapsed, but it is NOT a Unix timestamp.
            "engine_now_ms": handle.engine_now(),
            "version": env!("CARGO_PKG_VERSION"),
            "devices": directory.devices.len(),
            "scenes": directory.scenes.len(),
            "rules": directory.rules.len(),
        }),
    )
}

/// One device object, shared by `GET /devices` and `GET /devices/{name}`.
fn device_json(entry: &DeviceEntry, handle: &RestApiHandle) -> Value {
    json!({
        "name": entry.name,
        "room": entry.room,
        "synthetic": entry.synthetic,
        "capabilities": capabilities_object(&entry.capabilities, |kind| {
            handle.state(entry.id, kind)
        }),
        // Declared event names, in config order. The `raw:` protocol string each
        // maps to is an adapter implementation detail and stays off the wire.
        "events": entry.events.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
    })
}

/// Every device, rendered. Shared by `GET /devices` and the stream's opening
/// `snapshot` frame, so a subscriber's first view and a plain `GET` cannot
/// disagree about shape.
pub fn devices_value(directory: &Directory, handle: &RestApiHandle) -> Value {
    Value::Array(
        directory
            .devices
            .iter()
            .map(|d| device_json(d, handle))
            .collect(),
    )
}

fn devices(directory: &Directory, handle: &RestApiHandle) -> Response {
    Response::json(200, json!({ "devices": devices_value(directory, handle) }))
}

fn device(directory: &Directory, handle: &RestApiHandle, name: &str) -> Response {
    match directory.device(name) {
        Some(entry) => Response::json(200, device_json(entry, handle)),
        None => unknown_device(name),
    }
}

/// One scene object, shared by `GET /scenes` and `GET /scenes/{name}`.
///
/// `commands` (the member count) is retained alongside the rendered `then` so v1
/// clients reading the count do not break.
fn scene_json(directory: &Directory, entry: &super::SceneEntry) -> Value {
    json!({
        "name": entry.name,
        "commands": entry.commands.len(),
        "then": describe::commands(directory, &entry.commands),
    })
}

fn scenes(directory: &Directory) -> Response {
    let scenes: Vec<Value> = directory
        .scenes
        .iter()
        .map(|s| scene_json(directory, s))
        .collect();
    Response::json(200, json!({ "scenes": scenes }))
}

fn scene(directory: &Directory, name: &str) -> Response {
    match directory.scene(name) {
        Some(entry) => Response::json(200, scene_json(directory, entry)),
        None => unknown_scene(name),
    }
}

/// One rule object: its static shape from config, merged with its runtime status
/// from the mirror. The two come from different sources — `Directory` and
/// `RuleStatus` — but a client wants them as one object.
fn rule_json(directory: &Directory, handle: &RestApiHandle, rule: &crate::rule::Rule) -> Value {
    let status = handle.rule_status(rule.id);
    let mut value = describe::rule_shape(directory, rule);
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "last_considered_ms".into(),
            json!(status.last_considered_ms),
        );
        obj.insert(
            "last_truth".into(),
            json!(status.last_truth.map(truth_name)),
        );
        obj.insert("last_fired_ms".into(), json!(status.last_fired_ms));
        obj.insert("fire_count".into(), json!(status.fire_count));
    }
    value
}

fn rules(directory: &Directory, handle: &RestApiHandle) -> Response {
    let rules: Vec<Value> = directory
        .rules
        .iter()
        .map(|r| rule_json(directory, handle, r))
        .collect();
    Response::json(200, json!({ "rules": rules }))
}

fn rule(directory: &Directory, handle: &RestApiHandle, name: &str) -> Response {
    match directory.rule(name) {
        Some(entry) => Response::json(200, rule_json(directory, handle, entry)),
        None => Response::error(404, "unknown_rule", format!("no rule named '{name}'")),
    }
}

// `truth_name` lives in the parent module: `GET /rules` and the SSE stream both
// spell `Truth` on the wire, and one table keeps them from disagreeing.
use super::truth_name;

// --- write endpoints ---------------------------------------------------------

fn activate(directory: &Directory, handle: &RestApiHandle, name: &str) -> Response {
    let Some(entry) = directory.scene(name) else {
        return unknown_scene(name);
    };
    queue(handle, Inbound::Scene { scene: entry.id })
}

/// Fire one of a device's declared events. Empty body.
///
/// This is how the API reaches rules. The injected `Event::Action` is
/// indistinguishable from the one the southbound adapter produces for a physical
/// press, so the engine runs the full drain loop — rule matching, conditions,
/// `for:` timers, cascades — and multi-stage behavior (an IR toggle that arms a
/// timer whose rule sends a follow-up code) works end to end without the client
/// knowing any of it.
///
/// Validation is against the device's declared `events:` **only**, deliberately
/// independent of `capabilities:`: a device may declare events and no
/// capabilities at all, which is the supported idiom for an API-driven
/// automation surface.
fn fire_event(directory: &Directory, handle: &RestApiHandle, name: &str, event: &str) -> Response {
    let Some(entry) = directory.device(name) else {
        return unknown_device(name);
    };
    let Some(action) = entry.action(event) else {
        let declared = entry
            .events
            .iter()
            .map(|(n, _)| n.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Response::error(
            404,
            "unknown_event",
            format!(
                "device '{name}' declares no event '{event}' (it has: {})",
                if declared.is_empty() {
                    "no events"
                } else {
                    &declared
                }
            ),
        );
    };
    queue(
        handle,
        Inbound::Action {
            device: entry.id,
            action,
        },
    )
}

fn intent(directory: &Directory, handle: &RestApiHandle, name: &str, body: &[u8]) -> Response {
    let Some(entry) = directory.device(name) else {
        return unknown_device(name);
    };
    match parse_intent(entry, body) {
        Ok(desired) => queue(
            handle,
            Inbound::Device {
                device: entry.id,
                desired,
            },
        ),
        Err(response) => response,
    }
}

/// Hand a request to the engine.
///
/// **`202`, not `200`, and never the resulting state.** The request is queued;
/// the engine dispatches on the next loop iteration and the device's own echo
/// folds some time after that. Returning a state here would mean inventing one.
/// A client that needs confirmation polls `GET /devices/{name}`.
fn queue(handle: &RestApiHandle, inbound: Inbound) -> Response {
    if handle.request(inbound) {
        Response::json(202, json!({ "accepted": true }))
    } else {
        Response::error(
            503,
            "engine_unavailable",
            "the engine is not accepting requests (shutting down)",
        )
    }
}

fn unknown_device(name: &str) -> Response {
    Response::error(404, "unknown_device", format!("no device named '{name}'"))
}

fn unknown_scene(name: &str) -> Response {
    Response::error(404, "unknown_scene", format!("no scene named '{name}'"))
}

fn malformed(message: impl Into<String>) -> Response {
    Response::error(400, "malformed_body", message)
}

fn unsupported(message: impl Into<String>) -> Response {
    Response::error(422, "unsupported_capability", message)
}

/// Parse and validate an intent body against what the device actually declared.
///
/// Validating at this edge is the main user-facing benefit of having a directory
/// at all: it turns "a switch command dispatched at an occupancy sensor, failing
/// opaquely three hops later" into an immediate `422`.
fn parse_intent(entry: &DeviceEntry, body: &[u8]) -> Result<Desired, Response> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|e| malformed(format!("body is not valid JSON: {e}")))?;
    let Some(obj) = value.as_object() else {
        return Err(malformed(
            "body must be a JSON object, e.g. {\"set\": {\"switch\": true}}",
        ));
    };

    // Exactly one intent key. Zero is an empty request; two is ambiguous, and
    // guessing which one the client meant is worse than saying so.
    let keys: Vec<&str> = obj.keys().map(|k| k.as_str()).collect();
    let key = match keys.as_slice() {
        [only] => *only,
        [] => {
            return Err(malformed(
                "body has no intent; expected exactly one of \
                 'set', 'toggle', 'adjust_brightness', 'send_ir_code'",
            ))
        }
        many => {
            return Err(malformed(format!(
                "body has {} intents ({}); expected exactly one",
                many.len(),
                many.join(", ")
            )))
        }
    };
    let value = &obj[key];

    match key {
        "set" => parse_set(entry, value),
        "toggle" => {
            require_capability(entry, CapabilityKind::Switch)?;
            Ok(Desired::Toggle)
        }
        "adjust_brightness" => {
            require_capability(entry, CapabilityKind::Brightness)?;
            let n = value
                .as_i64()
                .ok_or_else(|| malformed("'adjust_brightness' expects an integer delta"))?;
            let delta = i8::try_from(n).map_err(|_| {
                malformed(format!(
                    "'adjust_brightness' must be between -128 and 127, got {n}"
                ))
            })?;
            Ok(Desired::AdjustBrightness(delta))
        }
        "send_ir_code" => {
            require_capability(entry, CapabilityKind::IrTransmitter)?;
            let code = value
                .as_str()
                .ok_or_else(|| malformed("'send_ir_code' expects a base64 string"))?;
            Ok(Desired::SendIr(code.to_string()))
        }
        other => Err(malformed(format!(
            "unknown intent '{other}'; expected one of \
             'set', 'toggle', 'adjust_brightness', 'send_ir_code'"
        ))),
    }
}

fn parse_set(entry: &DeviceEntry, value: &Value) -> Result<Desired, Response> {
    let Some(obj) = value.as_object() else {
        return Err(malformed(
            "'set' expects an object, e.g. {\"set\": {\"brightness\": 40}}",
        ));
    };
    let (name, raw) = single_entry(obj)?;

    let kind = CapabilityKind::from_name(name)
        .ok_or_else(|| unsupported(format!("unknown capability '{name}'")))?;
    require_capability(entry, kind)?;

    let state = state_from_json(kind, raw).map_err(|e| malformed(e.0))?;
    Ok(Desired::Set(state))
}

/// A `set` names exactly one capability, for the same reason an intent names
/// exactly one intent.
fn single_entry(obj: &Map<String, Value>) -> Result<(&str, &Value), Response> {
    let keys: Vec<&str> = obj.keys().map(|k| k.as_str()).collect();
    match keys.as_slice() {
        [only] => Ok((*only, &obj[*only])),
        [] => Err(malformed("'set' names no capability")),
        many => Err(malformed(format!(
            "'set' names {} capabilities ({}); set one at a time",
            many.len(),
            many.join(", ")
        ))),
    }
}

/// Reject a capability the device did not declare, and any read-only capability.
///
/// The engine treats a write to a report-only capability as a harmless no-op
/// (see `Engine::command_for_state`) — correct for Matter, where the alternative
/// is dropping a controller mid-write. But a REST client can be told plainly, so
/// it is, with a `422`.
fn require_capability(entry: &DeviceEntry, kind: CapabilityKind) -> Result<(), Response> {
    if !entry.capabilities.contains(&kind) {
        let declared = entry
            .capabilities
            .iter()
            .map(|k| k.name())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(unsupported(format!(
            "device '{}' did not declare '{}' (it has: {})",
            entry.name,
            kind.name(),
            if declared.is_empty() {
                "no capabilities"
            } else {
                &declared
            }
        )));
    }
    if !kind.is_writable() {
        return Err(unsupported(format!(
            "'{}' is read-only; it is reported by the device, not set",
            kind.name()
        )));
    }
    Ok(())
}
