//! Phase 2: the `matter_device` northbound adapter, proven with the in-memory
//! `InMemoryMatter` transport (no rs-matter node, no network). Covers the pure
//! `CapabilityState` ↔ Matter mapping, the adapter's two facets (mirror outward on
//! fold, controller write inward on tick), and the config/compile/build path.
//!
//! The real rs-matter-backed transport is intentionally a no-op stub this phase
//! (its commissioning/fabric persistence is being designed separately), so these
//! tests exercise everything *except* the live node.

use domiform::adapters::matter_device::{
    capability_is_exposable, device_type_for, ExposedDevice, InMemoryMatter, MatterDeviceAdapter,
    MatterDeviceType,
};
use domiform::ids::DeviceId;
use domiform::model::{CapabilityKind, CapabilityState, Millis};
use domiform::{build_engine, compile_str, Adapter, Engine, Event, Observer};

const LIGHT: DeviceId = DeviceId(7);

// --- pure mapping ------------------------------------------------------------

#[test]
fn brightness_makes_a_dimmable_light() {
    assert_eq!(
        device_type_for(&[CapabilityKind::Switch, CapabilityKind::Brightness]),
        MatterDeviceType::DimmableLight
    );
}

#[test]
fn switch_only_is_an_onoff_light() {
    assert_eq!(
        device_type_for(&[CapabilityKind::Switch]),
        MatterDeviceType::OnOffLight
    );
}

#[test]
fn occupancy_only_is_a_sensor() {
    assert_eq!(
        device_type_for(&[CapabilityKind::Occupancy]),
        MatterDeviceType::OccupancySensor
    );
}

#[test]
fn engine_internal_capabilities_are_not_exposable() {
    // Time/sun/IR must never be projected onto a Matter cluster.
    assert!(!capability_is_exposable(CapabilityKind::TimeOfDay));
    assert!(!capability_is_exposable(CapabilityKind::SunUp));
    assert!(!capability_is_exposable(CapabilityKind::IrTransmitter));
    // Ordinary device capabilities are.
    assert!(capability_is_exposable(CapabilityKind::Switch));
    assert!(capability_is_exposable(CapabilityKind::Brightness));
}

// --- adapter facets (via InMemoryMatter) -------------------------------------

/// A cloneable in-memory transport lets a test hold a handle to the same inner
/// state the adapter owns — read what it published, enqueue controller writes.
#[test]
fn state_fold_publishes_to_the_transport() {
    let transport = InMemoryMatter::new();
    let mut adapter = MatterDeviceAdapter::new(
        vec![ExposedDevice {
            id: LIGHT,
            label: "lamp".into(),
            capabilities: vec![CapabilityKind::Switch, CapabilityKind::Brightness],
        }],
        Box::new(transport.clone()),
    );

    // The observer facet mirrors folded state outward, through the shared handle.
    adapter.state_folded(LIGHT, &CapabilityState::Switch(true));
    adapter.state_folded(LIGHT, &CapabilityState::Brightness(70));

    assert_eq!(
        transport.published(),
        vec![
            (LIGHT, CapabilityState::Switch(true)),
            (LIGHT, CapabilityState::Brightness(70)),
        ]
    );
}

#[test]
fn publish_filters_unexposable_capabilities() {
    let transport = InMemoryMatter::new();
    let mut adapter = MatterDeviceAdapter::new(
        vec![ExposedDevice {
            id: LIGHT,
            label: "lamp".into(),
            capabilities: vec![CapabilityKind::Brightness],
        }],
        Box::new(transport.clone()),
    );

    adapter.state_folded(LIGHT, &CapabilityState::Brightness(50)); // exposable
    adapter.state_folded(LIGHT, &CapabilityState::TimeOfDay(600)); // internal → dropped

    assert_eq!(
        transport.published(),
        vec![(LIGHT, CapabilityState::Brightness(50))]
    );
}

#[test]
fn a_controller_write_becomes_a_requested_change_on_tick() {
    let transport = InMemoryMatter::new();
    transport.queue_inbound(LIGHT, CapabilityState::Switch(true));

    let mut adapter = MatterDeviceAdapter::new(
        vec![ExposedDevice {
            id: LIGHT,
            label: "lamp".into(),
            capabilities: vec![CapabilityKind::Switch],
        }],
        Box::new(transport),
    );

    let events = adapter.tick(0);
    assert_eq!(
        events,
        vec![Event::RequestedChange {
            device: LIGHT,
            desired: CapabilityState::Switch(true),
        }]
    );
    // Drained: a second tick yields nothing.
    assert!(adapter.tick(0).is_empty());
}

#[test]
fn a_northbound_adapter_binds_no_devices_so_dispatch_is_permanent_failure() {
    use domiform::DispatchOutcome;
    let mut adapter = MatterDeviceAdapter::new(vec![], Box::<InMemoryMatter>::default());
    let outcome = adapter.dispatch(
        &domiform::Command::SetSwitch {
            device: LIGHT,
            on: true,
        },
        0 as Millis,
    );
    assert!(matches!(outcome, DispatchOutcome::Permanent(_)));
}

// --- end-to-end through the engine (in-memory transport) ---------------------

#[test]
fn controller_write_drives_the_bound_device_end_to_end() {
    // A full northbound round trip using the real engine wiring: an inbound
    // controller write on the matter_device adapter drives the southbound device.
    use domiform::{Command, DispatchOutcome};
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Clone, Default)]
    struct Recorder(Rc<RefCell<Vec<Command>>>);
    impl Adapter for Recorder {
        fn dispatch(&mut self, cmd: &Command, _now: Millis) -> DispatchOutcome {
            self.0.borrow_mut().push(cmd.clone());
            match cmd {
                Command::SetSwitch { device, on } => {
                    DispatchOutcome::Ok(vec![Event::StateReported {
                        device: *device,
                        state: CapabilityState::Switch(*on),
                    }])
                }
                _ => DispatchOutcome::ok(),
            }
        }
    }

    let recorder = Recorder::default();
    let mut engine = Engine::new();
    let idx = engine.add_adapter(Box::new(recorder.clone()));
    engine.bind_device(LIGHT, idx);

    let transport = InMemoryMatter::new();
    transport.queue_inbound(LIGHT, CapabilityState::Switch(true));
    engine.add_northbound(Box::new(MatterDeviceAdapter::new(
        vec![ExposedDevice {
            id: LIGHT,
            label: "lamp".into(),
            capabilities: vec![CapabilityKind::Switch],
        }],
        Box::new(transport),
    )));

    // advance ticks the northbound adapter, draining the write into a
    // RequestedChange that drives the southbound recorder.
    engine.advance(1);

    assert_eq!(
        recorder.0.borrow().clone(),
        vec![Command::SetSwitch {
            device: LIGHT,
            on: true,
        }]
    );
    assert_eq!(engine.switch_state(LIGHT), Some(true));
}

// --- config / compile / build path -------------------------------------------

#[test]
fn matter_device_config_compiles_and_builds() {
    let cfg = compile_str(
        r#"
adapters:
  z: { type: mock }
  home: { type: matter_device, expose: all }
devices:
  lamp: { adapter: z, capabilities: [switch, brightness] }
"#,
    )
    .expect("valid config");

    assert!(
        !cfg.warnings
            .iter()
            .any(|w| w.to_string().contains("E_UNUSED_ADAPTER")),
        "matter_device should not warn as unused: {:?}",
        cfg.warnings
    );

    let mut engine = build_engine(&cfg);
    engine.start(); // must not panic even though the real transport is a stub
}

// --- runtime storage path resolution -----------------------------------------

#[test]
fn default_state_file_is_stable_and_under_the_runtime_dir() {
    use domiform::default_state_file;
    use std::path::Path;

    let a = default_state_file(Path::new("/var/lib/domiform"), "home");
    let b = default_state_file(Path::new("/var/lib/domiform"), "home");
    // Deterministic: same adapter name → same file every run (idempotent).
    assert_eq!(a, b);
    // Lives under the runtime dir with the documented shape.
    assert!(a.starts_with("/var/lib/domiform"));
    let name = a.file_name().unwrap().to_string_lossy();
    assert!(
        name.starts_with("homekit.") && name.ends_with(".state"),
        "{name}"
    );
}

#[test]
fn distinct_adapters_get_distinct_state_files() {
    use domiform::default_state_file;
    use std::path::Path;

    let home = default_state_file(Path::new("/x"), "home");
    let guest = default_state_file(Path::new("/x"), "guest_house");
    assert_ne!(home, guest);
}

#[test]
fn runtime_storage_dir_defaults_to_config_dir_but_honors_override() {
    use std::path::Path;

    // Unset → the config file's directory (host-supplied), not the cwd.
    let cfg = compile_str(
        r#"
system: { timezone: UTC }
adapters:
  z: { type: mock }
devices:
  lamp: { adapter: z, capabilities: [switch] }
"#,
    )
    .unwrap();
    assert_eq!(
        cfg.system.runtime_storage_dir(Path::new("/etc/domiform")),
        Path::new("/etc/domiform")
    );

    // Set → used verbatim, ignoring the config dir.
    let cfg2 = compile_str(
        r#"
system: { timezone: UTC, runtime_storage_path: /data/state }
adapters:
  z: { type: mock }
devices:
  lamp: { adapter: z, capabilities: [switch] }
"#,
    )
    .unwrap();
    assert_eq!(
        cfg2.system.runtime_storage_dir(Path::new("/etc/domiform")),
        Path::new("/data/state")
    );
}

#[test]
fn exposing_an_unknown_device_is_a_compile_error() {
    let err = compile_str(
        r#"
adapters:
  z: { type: mock }
  home: { type: matter_device, expose: [lamp, ghost] }
devices:
  lamp: { adapter: z, capabilities: [switch] }
"#,
    )
    .unwrap_err();

    assert!(err
        .0
        .iter()
        .any(|d| d.to_string().contains("E_UNKNOWN_EXPOSED_DEVICE")));
}
