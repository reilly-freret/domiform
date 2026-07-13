//! Feature B: the `color_is` condition.
//!
//! Color is neither bool- nor numeric-shaped, so it needs its own condition leaf
//! (`Condition::ColorEquals`). The read path (adapters folding inbound color into
//! `CapabilityState::Color`) already existed; these tests prove the condition end
//! of it: a button press fires a rule gated on the light's *reported* color.

use domiform::{build_engine, compile_str, CapabilityState, Event};

/// A strip (switch + color) plus a button. The rule turns the strip on when the
/// button is pressed *and* the strip's reported color equals `expect`.
fn config(expect: &str) -> String {
    format!(
        r#"
adapters:
  z: {{ type: mock }}
devices:
  strip:
    adapter: z
    capabilities: [switch, color]
  btn:
    adapter: z
    events: {{ press: p }}
rules:
  r:
    when: {{ event: btn.press }}
    if: {{ color_is: {{ device: strip, color: "{expect}" }} }}
    then:
      - turn_on: strip
"#
    )
}

/// Report a color, press the button, read whether the strip turned on.
fn fires_with_color(cfg_src: &str, reported: Option<(u8, u8, u8)>) -> bool {
    let cfg = compile_str(cfg_src).expect("should compile");
    let strip = cfg.device_id("strip").unwrap();
    let btn = cfg.device_id("btn").unwrap();
    let press = cfg.device(btn).unwrap().events[0].id;

    let mut engine = build_engine(&cfg);
    engine.start();
    if let Some((r, g, b)) = reported {
        engine.inject(Event::StateReported {
            device: strip,
            state: CapabilityState::Color { r, g, b },
        });
    }
    engine.inject(Event::Action {
        device: btn,
        action: press,
    });
    engine.switch_state(strip) == Some(true)
}

#[test]
fn matches_exact_color_and_rejects_others() {
    let cfg = config("#FF0000");
    assert!(fires_with_color(&cfg, Some((255, 0, 0))), "red matches");
    assert!(
        !fires_with_color(&cfg, Some((0, 255, 0))),
        "green does not match red"
    );
}

#[test]
fn never_reported_is_unknown_and_does_not_fire() {
    let cfg = config("#FF0000");
    assert!(
        !fires_with_color(&cfg, None),
        "color never reported → Unknown → rule must not fire"
    );
}

#[test]
fn both_color_forms_lower_identically() {
    // `#RRGGBB` and `{ r, g, b }` must produce the same ColorEquals.
    let hex_cfg = config("#0A141E"); // 10, 20, 30
    let obj_cfg = r#"
adapters:
  z: { type: mock }
devices:
  strip: { adapter: z, capabilities: [switch, color] }
  btn: { adapter: z, events: { press: p } }
rules:
  r:
    when: { event: btn.press }
    if: { color_is: { device: strip, color: { r: 10, g: 20, b: 30 } } }
    then: [ { turn_on: strip } ]
"#;
    assert!(fires_with_color(&hex_cfg, Some((10, 20, 30))));
    assert!(fires_with_color(obj_cfg, Some((10, 20, 30))));
    assert!(!fires_with_color(obj_cfg, Some((10, 20, 31))));
}

#[test]
fn color_is_on_device_without_color_is_an_error() {
    let src = r##"
adapters:
  z: { type: mock }
devices:
  strip: { adapter: z, capabilities: [switch] }
  btn: { adapter: z, events: { press: p } }
rules:
  r:
    when: { event: btn.press }
    if: { color_is: { device: strip, color: "#FF0000" } }
    then: [ { turn_on: strip } ]
"##;
    let errs = compile_str(src).expect_err("color_is without color cap should fail");
    assert!(errs.errors().any(|d| d.code == "E_MISSING_CAPABILITY"));
}
