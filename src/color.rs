//! Pure color conversions shared by the compiler and adapters.
//!
//! sRGB bytes are the canonical wire form inside domiform (`Command::SetColor`
//! carries `r`/`g`/`b`). Config parsing and every protocol adapter reach for the
//! same primitives — parsing `#RRGGBB`, mapping RGB to a protocol's hue/sat, or
//! translating mireds to a warm/cold-white mix — so they live here once rather
//! than being re-derived per call site.

/// Parse a `#RRGGBB` (or bare `RRGGBB`) hex string into sRGB bytes.
///
/// Returns `None` on any malformed input (wrong length, non-hex digits). Callers
/// that need a diagnostic wrap this and emit their own error on `None`; callers
/// parsing device output just drop the report.
pub fn hex_to_rgb(s: &str) -> Option<(u8, u8, u8)> {
    let hex = s.strip_prefix('#').unwrap_or(s);
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some((r, g, b))
}

/// The warm end of the tunable-white range domiform maps color temperature onto.
pub const WARM_KELVIN: u32 = 2700;
/// The cold end of the tunable-white range domiform maps color temperature onto.
pub const COLD_KELVIN: u32 = 6500;

/// Convert mireds to a warm/cold-white channel mix (each 0..=255).
///
/// Tunable-white hardware (e.g. Z-Wave Color Switch) exposes two channels rather
/// than a mireds value, so a requested temperature is expressed as a blend. The
/// input is clamped to [`WARM_KELVIN`]..=[`COLD_KELVIN`]; requests outside that
/// band are honored at the nearest bound — the hardware cannot render them.
pub fn mireds_to_warm_cold(mireds: u16) -> (u8, u8) {
    let kelvin = (1_000_000_u32 / mireds.max(1) as u32).clamp(WARM_KELVIN, COLD_KELVIN);
    let cold_pct = (kelvin - WARM_KELVIN) * 100 / (COLD_KELVIN - WARM_KELVIN);
    let warm = (255 * (100 - cold_pct) / 100) as u8;
    let cold = (255 * cold_pct / 100) as u8;
    (warm, cold)
}

/// Approximate mireds from a warm/cold-white channel mix.
///
/// The inverse of [`mireds_to_warm_cold`], for reading tunable-white devices that
/// report channel levels instead of a mireds value. Returns `None` when both
/// channels are off (no white output → no meaningful temperature).
pub fn warm_cold_to_mireds(warm: u64, cold: u64) -> Option<u16> {
    if warm == 0 && cold == 0 {
        return None;
    }
    let cold_pct = (cold * 100 / (warm + cold).max(1)) as u32;
    let kelvin = WARM_KELVIN + ((COLD_KELVIN - WARM_KELVIN) * cold_pct / 100);
    Some((1_000_000 / kelvin.max(1)).min(u16::MAX as u32) as u16)
}

/// Convert sRGB to a hue/saturation pair, each scaled to 0..=254.
///
/// This is the encoding Matter's ColorControl `MoveToHueAndSaturation` expects
/// (hue over 0..=360°, saturation over 0..=100%). The value/brightness component
/// of HSV is intentionally discarded: brightness is a separate capability, and a
/// color command must not silently dim the bulb. Consequently `#400000` (dark
/// red) and `#ff0000` (bright red) both map to hue 0 / saturation 254.
pub fn rgb_to_hue_sat_254(r: u8, g: u8, b: u8) -> (u8, u8) {
    let rf = r as f32 / 255.0;
    let gf = g as f32 / 255.0;
    let bf = b as f32 / 255.0;
    let max = rf.max(gf).max(bf);
    let min = rf.min(gf).min(bf);
    let delta = max - min;

    let hue_deg = if delta == 0.0 {
        0.0
    } else if max == rf {
        60.0 * (((gf - bf) / delta) % 6.0)
    } else if max == gf {
        60.0 * (((bf - rf) / delta) + 2.0)
    } else {
        60.0 * (((rf - gf) / delta) + 4.0)
    };
    let hue_deg = if hue_deg < 0.0 {
        hue_deg + 360.0
    } else {
        hue_deg
    };

    let saturation = if max == 0.0 { 0.0 } else { delta / max };

    let hue = ((hue_deg / 360.0) * 254.0).round().clamp(0.0, 254.0) as u8;
    let sat = (saturation * 254.0).round().clamp(0.0, 254.0) as u8;
    (hue, sat)
}

/// Convert a hue/saturation pair (each 0..=254) back to sRGB at full brightness.
///
/// The inverse of [`rgb_to_hue_sat_254`] for reading devices that report
/// ColorControl `CurrentHue`/`CurrentSaturation`. Because brightness is dropped
/// on the way out, it is assumed maximal on the way back — the reconstructed
/// color has the right hue and saturation but always full value.
pub fn hue_sat_254_to_rgb(hue: u8, sat: u8) -> (u8, u8, u8) {
    let h = (hue as f32 / 254.0) * 360.0;
    let s = sat as f32 / 254.0;
    let v = 1.0_f32; // brightness is a separate capability; assume full.

    let c = v * s;
    let x = c * (1.0 - (((h / 60.0) % 2.0) - 1.0).abs());
    let m = v - c;
    let (rp, gp, bp) = match h {
        h if h < 60.0 => (c, x, 0.0),
        h if h < 120.0 => (x, c, 0.0),
        h if h < 180.0 => (0.0, c, x),
        h if h < 240.0 => (0.0, x, c),
        h if h < 300.0 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let to_byte = |f: f32| ((f + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    (to_byte(rp), to_byte(gp), to_byte(bp))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_parsing_accepts_prefixed_and_bare() {
        assert_eq!(hex_to_rgb("#ff8000"), Some((255, 128, 0)));
        assert_eq!(hex_to_rgb("FF8000"), Some((255, 128, 0)));
        assert_eq!(hex_to_rgb("#FfFfFf"), Some((255, 255, 255)));
    }

    #[test]
    fn hex_parsing_rejects_malformed() {
        assert_eq!(hex_to_rgb("#fff"), None); // too short
        assert_eq!(hex_to_rgb("#gg0000"), None); // non-hex
        assert_eq!(hex_to_rgb("#ff00000"), None); // too long
        assert_eq!(hex_to_rgb(""), None);
    }

    #[test]
    fn primary_red_is_hue_zero_full_sat() {
        assert_eq!(rgb_to_hue_sat_254(255, 0, 0), (0, 254));
    }

    #[test]
    fn brightness_is_discarded_from_hue_sat() {
        // Dark and bright red share hue/sat; only brightness differs.
        assert_eq!(
            rgb_to_hue_sat_254(0x40, 0, 0),
            rgb_to_hue_sat_254(0xff, 0, 0)
        );
    }

    #[test]
    fn white_has_zero_saturation() {
        let (_, sat) = rgb_to_hue_sat_254(255, 255, 255);
        assert_eq!(sat, 0);
    }

    #[test]
    fn hue_sat_round_trips_through_rgb() {
        // Primary and secondary colors survive a round trip (full brightness).
        for rgb in [
            (255, 0, 0),
            (0, 255, 0),
            (0, 0, 255),
            (255, 255, 0),
            (0, 255, 255),
            (255, 0, 255),
        ] {
            let (h, s) = rgb_to_hue_sat_254(rgb.0, rgb.1, rgb.2);
            let back = hue_sat_254_to_rgb(h, s);
            // Allow a small rounding tolerance from the 254-step quantization.
            let close = |a: u8, b: u8| (a as i16 - b as i16).abs() <= 3;
            assert!(
                close(back.0, rgb.0) && close(back.1, rgb.1) && close(back.2, rgb.2),
                "{rgb:?} -> ({h},{s}) -> {back:?}"
            );
        }
    }

    #[test]
    fn warm_cold_round_trips_at_bounds() {
        // Pure warm → 2700 K ≈ 370 mireds; pure cold → 6500 K ≈ 153 mireds.
        assert_eq!(mireds_to_warm_cold(370), (255, 0));
        assert_eq!(warm_cold_to_mireds(255, 0), Some(370));
        assert_eq!(warm_cold_to_mireds(0, 0), None);
    }
}
