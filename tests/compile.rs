//! Compiler tests: resolution (names → ids, refs → indices, strings → kinds),
//! deterministic id assignment, one-pass error collection, and the compile→run
//! seam.

use domiform::model::CapabilityKind;
use domiform::{build_engine, compile_str, CapabilityState, Event};

const GOOD: &str = r#"
system:
  timezone: America/New_York
  latitude: 40.7
  longitude: -74.0

adapters:
  zigbee:
    type: zigbee2mqtt
    url: mqtt://127.0.0.1:1883
    base_topic: zigbee2mqtt

devices:
  kitchen_light:
    adapter: zigbee
    address: "0x00158d0004abcd"
    capabilities: [switch, brightness, color_temperature]
  motion_sensor:
    adapter: zigbee
    address: "0x00158d0004ef01"
    capabilities: [occupancy, battery]
"#;

#[test]
fn resolves_names_refs_and_capabilities() {
    let cfg = compile_str(GOOD).expect("should compile");

    // Names interned to ids, sorted by name: kitchen_light < motion_sensor.
    let kitchen = cfg.device_id("kitchen_light").unwrap();
    let motion = cfg.device_id("motion_sensor").unwrap();
    assert_eq!(kitchen.0, 0);
    assert_eq!(motion.0, 1);

    // The string `adapter: zigbee` became a resolved index.
    let zigbee = cfg.adapter_idx("zigbee").unwrap();
    assert_eq!(cfg.device(kitchen).unwrap().adapter, zigbee);

    // Capability strings became kinds.
    assert_eq!(
        cfg.device(kitchen).unwrap().capabilities,
        vec![
            CapabilityKind::Switch,
            CapabilityKind::Brightness,
            CapabilityKind::ColorTemperature
        ]
    );

    // Connection details live on the adapter; system carries only global values.
    assert_eq!(
        cfg.adapters[zigbee].plugin.map(|p| p.type_tag()),
        Some("zigbee2mqtt")
    );
    assert_eq!(
        cfg.adapters[zigbee].config["url"].as_str(),
        Some("mqtt://127.0.0.1:1883")
    );
    assert_eq!(cfg.system.timezone, "America/New_York");
}

#[test]
fn ids_are_deterministic_regardless_of_file_order() {
    // Same devices, declared in the opposite order.
    let reordered: &str = r#"
adapters:
  zigbee: { type: mock }
devices:
  motion_sensor: { adapter: zigbee, capabilities: [occupancy] }
  kitchen_light: { adapter: zigbee, capabilities: [switch] }
"#;
    let a = compile_str(GOOD).unwrap();
    let b = compile_str(reordered).unwrap();
    assert_eq!(a.device_id("kitchen_light"), b.device_id("kitchen_light"));
    assert_eq!(a.device_id("motion_sensor"), b.device_id("motion_sensor"));
}

#[test]
fn collects_every_error_in_one_pass() {
    let bad: &str = r#"
adapters:
  zigbee: { type: mock }
devices:
  lamp:
    adapter: nonexistent
    capabilities: [switch, frobnicate, time_of_day]
"#;
    let errs = compile_str(bad).expect_err("should fail");
    let codes: Vec<_> = errs.errors().map(|d| d.code).collect();

    // Unknown adapter AND both bad capabilities reported together — not just the
    // first. `time_of_day` is synthetic and not user-declarable.
    assert!(codes.contains(&"E_UNKNOWN_ADAPTER"));
    assert_eq!(
        codes
            .iter()
            .filter(|c| **c == "E_UNKNOWN_CAPABILITY")
            .count(),
        2
    );
}

#[test]
fn syntax_error_is_a_single_parse_diagnostic() {
    let errs = compile_str("devices: [this is not a mapping").expect_err("should fail");
    let codes: Vec<_> = errs.errors().map(|d| d.code).collect();
    assert_eq!(codes, vec!["E_PARSE"]);
}

#[test]
fn allows_top_level_x_extension_keys_for_yaml_anchors() {
    let cfg = compile_str(
        r#"
x-ac-off: &ac-off CyEMLCbMASoGzAE3AuABA+ADDwBbIBcJKgY3AioGzAE3AuAhA0AvgDsBzAHgAzcDKgbMAQ==

adapters:
  z: { type: mock }
devices:
  ac: { adapter: z, capabilities: [ir_transmitter] }
  btn: { adapter: z, events: { press: p } }
rules:
  off:
    when: { event: btn.press }
    then:
      - send_ir_code: { device: ac, code: *ac-off }
"#,
    )
    .expect("x-* anchor definitions should compile");

    assert_eq!(cfg.devices.len(), 2);
    assert_eq!(cfg.rules.len(), 1);
}

#[test]
fn rejects_unknown_top_level_keys_even_with_x_extensions() {
    let errs = compile_str(
        r#"
x-ac-off: &ac-off
  code: abc==
typo_section: {}
adapters:
  z: { type: mock }
"#,
    )
    .expect_err("non-x unknown keys should still fail");
    assert!(errs.errors().any(|d| d.code == "E_PARSE"));
}

#[test]
fn unused_adapter_is_a_warning_not_an_error() {
    let cfg = compile_str(
        r#"
adapters:
  zigbee: { type: mock }
  spare:  { type: mock }
devices:
  lamp: { adapter: zigbee, capabilities: [switch] }
"#,
    )
    .expect("warnings do not fail compilation");
    assert!(cfg.warnings.iter().any(|d| d.code == "E_UNUSED_ADAPTER"));
}

const MATTER: &str = r#"
adapters:
  thread:
    type: matter
    url: ws://127.0.0.1:5580/ws
devices:
  desk_lamp:
    adapter: thread
    address: "12"
    capabilities: [switch, brightness]
  ceiling:
    adapter: thread
    address: "13"
    endpoint: 2
    capabilities: [switch]
"#;

#[test]
fn resolves_matter_adapter_url_and_endpoint() {
    let cfg = compile_str(MATTER).expect("should compile");

    let thread = cfg.adapter_idx("thread").unwrap();
    assert_eq!(
        cfg.adapters[thread].plugin.map(|p| p.type_tag()),
        Some("matter")
    );
    assert_eq!(
        cfg.adapters[thread].config["url"].as_str(),
        Some("ws://127.0.0.1:5580/ws")
    );

    // `endpoint` is carried through verbatim — `None` when omitted, `Some` when
    // set. The per-protocol default (matter → 1) is applied where the adapter is
    // built, not in `resolve`, so each protocol keeps its own default.
    let lamp = cfg.device_id("desk_lamp").unwrap();
    let ceiling = cfg.device_id("ceiling").unwrap();
    assert_eq!(cfg.device(lamp).unwrap().endpoint, None);
    assert_eq!(cfg.device(ceiling).unwrap().endpoint, Some(2));
}

#[test]
fn matter_device_needs_a_numeric_node_id() {
    // A non-numeric address (E_BAD_ADDRESS) and a missing one (E_MISSING_ADDRESS),
    // plus a non-ws url (E_BAD_URL) — all caught in one pass.
    let bad: &str = r#"
adapters:
  thread: { type: matter, url: http://nope:5580 }
devices:
  a: { adapter: thread, address: "not-a-number", capabilities: [switch] }
  b: { adapter: thread, capabilities: [switch] }
"#;
    let errs = compile_str(bad).expect_err("should fail");
    let codes: Vec<_> = errs.errors().map(|d| d.code).collect();
    assert!(codes.contains(&"E_BAD_URL"));
    assert!(codes.contains(&"E_BAD_ADDRESS"));
    assert!(codes.contains(&"E_MISSING_ADDRESS"));
}

const ZWAVE: &str = r#"
adapters:
  zwave:
    type: zwavejs
    url: ws://127.0.0.1:3000
devices:
  scene_controller:
    adapter: zwave
    address: "5"
    events:
      up_single: "1:KeyPressed"
  bedroom_bulb:
    adapter: zwave
    address: "8"
    capabilities: [switch, brightness]
"#;

#[test]
fn resolves_zwavejs_adapter_url() {
    let cfg = compile_str(ZWAVE).expect("should compile");

    let zwave = cfg.adapter_idx("zwave").unwrap();
    assert_eq!(
        cfg.adapters[zwave].plugin.map(|p| p.type_tag()),
        Some("zwavejs")
    );
    assert_eq!(
        cfg.adapters[zwave].config["url"].as_str(),
        Some("ws://127.0.0.1:3000")
    );

    // The Central Scene event interns to an action addressable from a rule.
    let controller = cfg.device_id("scene_controller").unwrap();
    assert!(cfg.action_id(controller, "up_single").is_some());
}

#[test]
fn zwavejs_device_needs_a_numeric_node_id() {
    // A non-numeric address (E_BAD_ADDRESS) and a missing one (E_MISSING_ADDRESS),
    // plus a non-ws url (E_BAD_URL) — all caught in one pass.
    let bad: &str = r#"
adapters:
  zwave: { type: zwavejs, url: http://nope:3000 }
devices:
  a: { adapter: zwave, address: "not-a-number", capabilities: [switch] }
  b: { adapter: zwave, capabilities: [switch] }
"#;
    let errs = compile_str(bad).expect_err("should fail");
    let codes: Vec<_> = errs.errors().map(|d| d.code).collect();
    assert!(codes.contains(&"E_BAD_URL"));
    assert!(codes.contains(&"E_BAD_ADDRESS"));
    assert!(codes.contains(&"E_MISSING_ADDRESS"));
}

#[test]
fn compiled_ids_drive_the_runtime() {
    let cfg = compile_str(GOOD).unwrap();
    let mut engine = build_engine(&cfg);

    // The DeviceId the compiler assigned is exactly what the engine binds.
    let kitchen = cfg.device_id("kitchen_light").unwrap();
    engine.inject(Event::StateReported {
        device: kitchen,
        state: CapabilityState::Switch(true),
    });
    assert_eq!(engine.switch_state(kitchen), Some(true));
}
