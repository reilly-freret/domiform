//! `CapabilityState` ↔ JSON, in both directions.
//!
//! Values are encoded in the **canonical units** `src/model.rs` defines, with no
//! conversion at this layer — the canonical-unit contract is the whole point of
//! having one. Concretely:
//!
//! | Capability | JSON | Unit |
//! |---|---|---|
//! | `switch`, `occupancy`, `contact`, `water_leak`, `smoke`, `sun_up` | bool | — |
//! | `brightness`, `battery`, `humidity` | number | percent, `0..=100` |
//! | `temperature` | number | centidegrees Celsius (2350 = 23.5 °C) |
//! | `color_temperature` | number | mireds |
//! | `illuminance` | number | lux |
//! | `power` | number | watts |
//! | `time_of_day` | number | minutes since local midnight |
//! | `color` | `{"r":…, "g":…, "b":…}` | sRGB, each `0..=255` |
//!
//! `ir_transmitter` has no `CapabilityState` at all (it is write-only), so it
//! never appears here — it always reads as `null`.

use serde_json::{json, Map, Value};

use crate::model::{CapabilityKind, CapabilityState};

/// Encode a capability value for the wire.
pub fn state_to_json(state: &CapabilityState) -> Value {
    use CapabilityState as S;
    match state {
        S::Switch(b)
        | S::Occupancy(b)
        | S::Contact(b)
        | S::WaterLeak(b)
        | S::Smoke(b)
        | S::SunUp(b) => json!(b),
        S::Brightness(v) | S::Battery(v) | S::Humidity(v) => json!(v),
        S::ColorTemperature(v) | S::TimeOfDay(v) => json!(v),
        S::Temperature(v) => json!(v),
        S::Illuminance(v) | S::Power(v) => json!(v),
        S::Color { r, g, b } => json!({ "r": r, "g": g, "b": b }),
    }
}

/// Why a JSON value could not become a `CapabilityState`. Carries a
/// human-readable message; the caller decides the status code.
#[derive(Debug, PartialEq)]
pub struct ValueError(pub String);

/// Decode a capability value from the wire, given the capability it is for.
///
/// Every scalar is range-checked against its canonical type here rather than
/// being silently truncated: a `brightness` of 400 is a client bug worth a `400`,
/// not a request to set 100.
pub fn state_from_json(kind: CapabilityKind, value: &Value) -> Result<CapabilityState, ValueError> {
    let name = kind.name();
    match kind {
        CapabilityKind::Switch => bool_of(value, name).map(CapabilityState::Switch),
        CapabilityKind::Brightness => pct_of(value, name).map(CapabilityState::Brightness),
        CapabilityKind::Color => color_of(value),
        CapabilityKind::ColorTemperature => {
            int_of(value, name, 0, 65535).map(|v| CapabilityState::ColorTemperature(v as u16))
        }
        CapabilityKind::Occupancy => bool_of(value, name).map(CapabilityState::Occupancy),
        CapabilityKind::Battery => pct_of(value, name).map(CapabilityState::Battery),
        CapabilityKind::Temperature => {
            int_of(value, name, -32768, 32767).map(|v| CapabilityState::Temperature(v as i16))
        }
        CapabilityKind::Humidity => pct_of(value, name).map(CapabilityState::Humidity),
        CapabilityKind::Illuminance => {
            int_of(value, name, 0, u32::MAX as i64).map(|v| CapabilityState::Illuminance(v as u32))
        }
        CapabilityKind::Power => {
            int_of(value, name, 0, u32::MAX as i64).map(|v| CapabilityState::Power(v as u32))
        }
        CapabilityKind::Contact => bool_of(value, name).map(CapabilityState::Contact),
        CapabilityKind::WaterLeak => bool_of(value, name).map(CapabilityState::WaterLeak),
        CapabilityKind::Smoke => bool_of(value, name).map(CapabilityState::Smoke),
        CapabilityKind::TimeOfDay => {
            int_of(value, name, 0, 1439).map(|v| CapabilityState::TimeOfDay(v as u16))
        }
        CapabilityKind::SunUp => bool_of(value, name).map(CapabilityState::SunUp),
        // Write-only: it has no state representation to decode into. Callers
        // reject this capability before reaching here (`Desired::SendIr` carries
        // the code instead), so this is a defensive arm rather than a live path.
        CapabilityKind::IrTransmitter => Err(ValueError(
            "'ir_transmitter' has no readable state; send a code with \
             {\"send_ir_code\": \"<base64>\"}"
                .to_string(),
        )),
    }
}

fn bool_of(value: &Value, name: &str) -> Result<bool, ValueError> {
    value.as_bool().ok_or_else(|| {
        ValueError(format!(
            "'{name}' expects a boolean, got {}",
            type_of(value)
        ))
    })
}

/// A percentage capability: an integer `0..=100`.
fn pct_of(value: &Value, name: &str) -> Result<u8, ValueError> {
    int_of(value, name, 0, 100).map(|v| v as u8)
}

/// An integer in `min..=max`. Returns `i64`; each caller casts to the canonical
/// width its `CapabilityState` variant uses, which the bounds already guarantee.
fn int_of(value: &Value, name: &str, min: i64, max: i64) -> Result<i64, ValueError> {
    let Some(n) = value.as_i64() else {
        // Reject floats explicitly: silently truncating 40.7 to 40 would make the
        // API lie about what it did.
        return Err(ValueError(format!(
            "'{name}' expects an integer, got {}",
            type_of(value)
        )));
    };
    if n < min || n > max {
        return Err(ValueError(format!(
            "'{name}' must be between {min} and {max}, got {n}"
        )));
    }
    Ok(n)
}

fn color_of(value: &Value) -> Result<CapabilityState, ValueError> {
    let Some(obj) = value.as_object() else {
        return Err(ValueError(format!(
            "'color' expects an object like {{\"r\":255,\"g\":170,\"b\":0}}, got {}",
            type_of(value)
        )));
    };
    let channel = |k: &str| -> Result<u8, ValueError> {
        let v = obj
            .get(k)
            .ok_or_else(|| ValueError(format!("'color' is missing '{k}'")))?;
        int_of(v, &format!("color.{k}"), 0, 255).map(|n| n as u8)
    };
    let (r, g, b) = (channel("r")?, channel("g")?, channel("b")?);
    // Reject stray keys so a typo ("rgb", "red") is an error rather than a
    // silently-ignored field that leaves the color wrong.
    if let Some(extra) = obj.keys().find(|k| !matches!(k.as_str(), "r" | "g" | "b")) {
        return Err(ValueError(format!("'color' has unexpected key '{extra}'")));
    }
    Ok(CapabilityState::Color { r, g, b })
}

/// A JSON type name for error messages.
fn type_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(n) if n.is_f64() => "a fractional number",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// The `capabilities` object for one device: every capability it **declared**, in
/// declaration order, mapped to its mirrored value or `null`.
///
/// A capability the engine has never folded is an explicit `null` — the wire
/// form of the engine's `Truth::Unknown`, which is a real and meaningful state
/// here (see `StateStore::bool_value`). `ir_transmitter` is write-only and is
/// therefore always `null`; that is expected, not a gap.
pub fn capabilities_object(
    capabilities: &[CapabilityKind],
    lookup: impl Fn(CapabilityKind) -> Option<CapabilityState>,
) -> Value {
    let mut out = Map::new();
    for kind in capabilities {
        let value = lookup(*kind)
            .map(|s| state_to_json(&s))
            .unwrap_or(Value::Null);
        out.insert(kind.name().to_string(), value);
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalars_round_trip_in_canonical_units() {
        let cases = [
            (
                CapabilityKind::Brightness,
                json!(40),
                CapabilityState::Brightness(40),
            ),
            (
                CapabilityKind::Temperature,
                json!(-500),
                CapabilityState::Temperature(-500),
            ),
            (
                CapabilityKind::ColorTemperature,
                json!(370),
                CapabilityState::ColorTemperature(370),
            ),
            (
                CapabilityKind::Switch,
                json!(true),
                CapabilityState::Switch(true),
            ),
            (
                CapabilityKind::Illuminance,
                json!(1200),
                CapabilityState::Illuminance(1200),
            ),
        ];
        for (kind, wire, state) in cases {
            assert_eq!(state_from_json(kind, &wire).unwrap(), state, "{kind:?}");
            assert_eq!(state_to_json(&state), wire, "{kind:?}");
        }
    }

    #[test]
    fn color_round_trips_and_rejects_malformed_objects() {
        let wire = json!({"r": 255, "g": 170, "b": 0});
        let state = CapabilityState::Color {
            r: 255,
            g: 170,
            b: 0,
        };
        assert_eq!(
            state_from_json(CapabilityKind::Color, &wire).unwrap(),
            state
        );
        assert_eq!(state_to_json(&state), wire);

        for bad in [
            json!({"r": 255, "g": 170}),                 // missing a channel
            json!({"r": 255, "g": 170, "b": 300}),       // out of range
            json!({"r": 255, "g": 170, "b": 0, "a": 1}), // stray key
            json!([255, 170, 0]),                        // not an object
        ] {
            assert!(
                state_from_json(CapabilityKind::Color, &bad).is_err(),
                "{bad} should be rejected"
            );
        }
    }

    #[test]
    fn out_of_range_and_wrong_typed_scalars_are_errors() {
        assert!(state_from_json(CapabilityKind::Brightness, &json!(101)).is_err());
        assert!(state_from_json(CapabilityKind::Brightness, &json!(-1)).is_err());
        // A float is rejected rather than truncated: the API must not silently
        // do something other than what was asked.
        assert!(state_from_json(CapabilityKind::Brightness, &json!(40.7)).is_err());
        assert!(state_from_json(CapabilityKind::Brightness, &json!("40")).is_err());
        assert!(state_from_json(CapabilityKind::Switch, &json!(1)).is_err());
        assert!(state_from_json(CapabilityKind::TimeOfDay, &json!(1440)).is_err());
    }

    #[test]
    fn undeclared_values_render_as_explicit_null() {
        let caps = [CapabilityKind::Switch, CapabilityKind::Brightness];
        let obj = capabilities_object(&caps, |k| match k {
            CapabilityKind::Switch => Some(CapabilityState::Switch(true)),
            _ => None,
        });
        assert_eq!(obj, json!({"switch": true, "brightness": null}));
    }
}
