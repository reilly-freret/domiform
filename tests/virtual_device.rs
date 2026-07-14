//! The `virtual` adapter: domiform-owned stateful devices with no physical
//! backing. Two things to prove: (1) the echo contract — a state-setting command
//! makes the device *be* that state; (2) the real motivating flow end-to-end — a
//! controller (Apple Home) tapping a virtual switch fires an IR rule, so a
//! stateless appliance gets a stateful tile.

use domiform::adapters::matter_device::{ExposedDevice, InMemoryMatter, MatterDeviceAdapter};
use domiform::ids::DeviceId;
use domiform::model::{CapabilityKind, CapabilityState, Command, Millis};
use domiform::{Adapter, DispatchOutcome, Event, VirtualDeviceAdapter};

const AC: DeviceId = DeviceId(3);

// --- the echo contract -------------------------------------------------------

#[test]
fn a_virtual_device_echoes_commanded_state_as_a_report() {
    let mut adapter = VirtualDeviceAdapter;

    // Set switch on → it reports Switch(true) (the store will fold this as truth).
    let out = adapter.dispatch(
        &Command::SetSwitch {
            device: AC,
            on: true,
        },
        0 as Millis,
    );
    assert!(matches!(
        out,
        DispatchOutcome::Ok(events)
            if events == vec![Event::StateReported { device: AC, state: CapabilityState::Switch(true) }]
    ));

    // Brightness echoes too (a virtual dimmer is just as valid).
    let out = adapter.dispatch(
        &Command::SetBrightness {
            device: AC,
            value: 40,
            transition: None,
        },
        0,
    );
    assert!(matches!(
        out,
        DispatchOutcome::Ok(events)
            if events == vec![Event::StateReported { device: AC, state: CapabilityState::Brightness(40) }]
    ));
}

// --- the motivating flow, end-to-end -----------------------------------------

#[test]
fn a_home_app_tap_on_a_virtual_switch_fires_an_ir_rule() {
    // Full wiring via the real compiler/engine: a virtual switch (`ac_power`)
    // exposed to Matter, and a rule that sends an IR code when it turns on. A
    // controller write flips the switch → the virtual adapter echoes it → the
    // rule fires → the IR blaster (a mock device) receives SendIrCode.
    use domiform::{build_engine, compile_str, Observer};
    use std::cell::RefCell;
    use std::rc::Rc;

    let cfg = compile_str(
        r#"
adapters:
  z:       { type: mock }
  virtual: { type: virtual }
  home:    { type: matter_device, expose: [ac_power] }
devices:
  ir_blaster: { adapter: z, capabilities: [ir_transmitter] }
  ac_power:   { adapter: virtual, capabilities: [switch] }
rules:
  ac_on_fires_ir:
    when: { changed: { device: ac_power, capability: switch, to: true } }
    then:
      - send_ir_code: { device: ir_blaster, code: "dG9nZ2xl" }
"#,
    )
    .expect("valid config");

    let ac = cfg.device_id("ac_power").unwrap();

    // Observe dispatched commands so we can assert the IR fired.
    #[derive(Clone, Default)]
    struct CmdRecorder(Rc<RefCell<Vec<Command>>>);
    impl Observer for CmdRecorder {
        fn command_dispatched(&mut self, command: &Command, _depth: u32) {
            self.0.borrow_mut().push(command.clone());
        }
    }

    let mut engine = build_engine(&cfg);
    let recorder = CmdRecorder::default();
    engine.add_observer(Box::new(recorder.clone()));
    engine.start();

    // Simulate a controller (Apple Home) tapping the switch on. This is what the
    // real Matter node's `poll` would surface; we inject the equivalent inbound
    // event directly so the test needs no live node.
    engine.inject(Event::RequestedChange {
        device: ac,
        desired: CapabilityState::Switch(true),
    });

    // The switch is now on (the virtual adapter echoed it and the engine folded it).
    assert_eq!(engine.switch_state(ac), Some(true));

    // And the IR code was sent exactly once, as a consequence of the change.
    let ir_sends: Vec<_> = recorder
        .0
        .borrow()
        .iter()
        .filter(|c| matches!(c, Command::SendIrCode { .. }))
        .cloned()
        .collect();
    assert_eq!(ir_sends.len(), 1, "expected one IR send, got {ir_sends:?}");
}

// --- config surface ----------------------------------------------------------

#[test]
fn a_virtual_device_exposed_to_matter_compiles_and_builds() {
    use domiform::{build_engine, compile_str};

    let cfg = compile_str(
        r#"
adapters:
  virtual: { type: virtual }
  home:    { type: matter_device, expose: all }
devices:
  ac_power: { adapter: virtual, capabilities: [switch] }
"#,
    )
    .expect("valid config");

    let mut engine = build_engine(&cfg);
    engine.start(); // must not panic (builds the Matter node for the virtual switch)

    // Sanity: the virtual switch mirrors outward like any exposed device.
    let _ = MatterDeviceAdapter::new(
        vec![ExposedDevice {
            id: AC,
            label: "ac_power".into(),
            capabilities: vec![CapabilityKind::Switch],
        }],
        Box::<InMemoryMatter>::default(),
    );
}
