//! Phase 1: the northbound adapter seam, proven end-to-end with the in-memory
//! `MockNorthbound` (no HAP). Covers both directions —
//!   * inward: every folded state change is fanned to the northbound adapter's
//!     mirror (this is what keeps a HomeKit app fresh), and
//!   * outward: a consumer "tap" the adapter drains on `tick` becomes a
//!     `RequestedChange` that drives the bound southbound device.
//!
//! It also covers the config/compile path (`type: mock_northbound`, `expose`
//! validation).

use std::cell::RefCell;
use std::rc::Rc;

use domiform::ids::DeviceId;
use domiform::model::{CapabilityState, Millis};
use domiform::{
    build_engine, compile_str, Adapter, Command, DispatchOutcome, Engine, Event, MockNorthbound,
};

const LIGHT: DeviceId = DeviceId(1);

/// A southbound device that echoes commanded state back (like `MockDeviceAdapter`)
/// and records what it was told, so we can assert the write path end-to-end.
#[derive(Clone, Default)]
struct Recorder(Rc<RefCell<Vec<Command>>>);
impl Recorder {
    fn commands(&self) -> Vec<Command> {
        self.0.borrow().clone()
    }
}
impl Adapter for Recorder {
    fn dispatch(&mut self, cmd: &Command, _now: Millis) -> DispatchOutcome {
        self.0.borrow_mut().push(cmd.clone());
        match cmd {
            Command::SetSwitch { device, on } => DispatchOutcome::Ok(vec![Event::StateReported {
                device: *device,
                state: CapabilityState::Switch(*on),
            }]),
            Command::SetBrightness { device, value, .. } => {
                DispatchOutcome::Ok(vec![Event::StateReported {
                    device: *device,
                    state: CapabilityState::Brightness(*value),
                }])
            }
            _ => DispatchOutcome::ok(),
        }
    }
}

/// Engine with `LIGHT` bound to a recorder and a mock northbound adapter watching.
fn build() -> (Engine, Recorder, MockNorthbound) {
    let recorder = Recorder::default();
    let bridge = MockNorthbound::new();
    let mut engine = Engine::new();
    let idx = engine.add_adapter(Box::new(recorder.clone()));
    engine.bind_device(LIGHT, idx);
    engine.add_northbound(Box::new(bridge.clone()));
    (engine, recorder, bridge)
}

#[test]
fn folded_state_is_mirrored_to_the_northbound_adapter() {
    let (mut engine, _rec, bridge) = build();

    // A device reports state (as a real device would): the bridge sees it.
    engine.inject(Event::StateReported {
        device: LIGHT,
        state: CapabilityState::Switch(true),
    });

    assert_eq!(bridge.latest(LIGHT), Some(CapabilityState::Switch(true)));
}

#[test]
fn command_echo_reaches_the_mirror() {
    // The full inward path: a request drives the device, the device echoes, and
    // the echo is what the bridge mirrors (not the request — reality, once known).
    let (mut engine, rec, bridge) = build();

    engine.inject(Event::RequestedChange {
        device: LIGHT,
        desired: CapabilityState::Brightness(60),
    });

    assert_eq!(
        rec.commands(),
        vec![Command::SetBrightness {
            device: LIGHT,
            value: 60,
            transition: None,
        }]
    );
    assert_eq!(bridge.latest(LIGHT), Some(CapabilityState::Brightness(60)));
}

#[test]
fn a_consumer_tap_drives_the_bound_device_on_tick() {
    // Outward→inward: queue a "tap" on the bridge; `advance` ticks it, producing a
    // RequestedChange that reaches the southbound device.
    let (mut engine, rec, bridge) = build();

    bridge.queue_write(LIGHT, CapabilityState::Switch(true));
    // No timers are due, but advancing ticks all adapters (including northbound).
    engine.advance(1);

    assert_eq!(
        rec.commands(),
        vec![Command::SetSwitch {
            device: LIGHT,
            on: true,
        }]
    );
    // And the resulting echo settled the store.
    assert_eq!(engine.switch_state(LIGHT), Some(true));
    assert_eq!(bridge.latest(LIGHT), Some(CapabilityState::Switch(true)));
}

#[test]
fn northbound_coexists_with_a_trace_observer() {
    // The multi-observer change: a northbound adapter and an ordinary observer
    // both receive folds. We assert the northbound mirror still works when another
    // observer is also registered (regression guard for the Vec<Observer> switch).
    #[derive(Clone, Default)]
    struct Counter(Rc<RefCell<usize>>);
    impl domiform::Observer for Counter {
        fn state_folded(&mut self, _d: DeviceId, _s: &CapabilityState) {
            *self.0.borrow_mut() += 1;
        }
    }

    let (mut engine, _rec, bridge) = build();
    let counter = Counter::default();
    engine.add_observer(Box::new(counter.clone()));

    engine.inject(Event::StateReported {
        device: LIGHT,
        state: CapabilityState::Switch(false),
    });

    assert_eq!(*counter.0.borrow(), 1);
    assert_eq!(bridge.latest(LIGHT), Some(CapabilityState::Switch(false)));
}

#[test]
fn start_replays_existing_state_into_a_freshly_added_northbound_adapter() {
    // A northbound adapter added after state already exists (e.g. a projection
    // built at boot) is caught up by `start()`: it receives the current store as
    // `state_folded`, so its mirror reflects engine truth rather than defaults.
    let recorder = Recorder::default();
    let mut engine = Engine::new();
    let idx = engine.add_adapter(Box::new(recorder));
    engine.bind_device(LIGHT, idx);

    // Seed engine state *before* the bridge exists.
    engine.inject(Event::StateReported {
        device: LIGHT,
        state: CapabilityState::Switch(true),
    });
    engine.inject(Event::StateReported {
        device: LIGHT,
        state: CapabilityState::Brightness(42),
    });

    // Now add a fresh bridge — it has seen nothing yet.
    let bridge = MockNorthbound::new();
    assert!(bridge.mirrored().is_empty());
    engine.add_northbound(Box::new(bridge.clone()));

    // start() replays the store into the northbound adapter.
    engine.start();

    assert_eq!(bridge.latest(LIGHT), Some(CapabilityState::Switch(true)));
    let mut kinds: Vec<_> = bridge.mirrored().iter().map(|(_, s)| s.clone()).collect();
    kinds.sort_by_key(|s| format!("{s:?}"));
    assert!(kinds.contains(&CapabilityState::Switch(true)));
    assert!(kinds.contains(&CapabilityState::Brightness(42)));
}

// --- config / compile path ---------------------------------------------------

#[test]
fn northbound_adapter_binds_no_devices_and_compiles() {
    // A northbound adapter that binds no devices must NOT trip E_UNUSED_ADAPTER,
    // and the config must build a runnable engine.
    let cfg = compile_str(
        r#"
adapters:
  z: { type: mock }
  home: { type: mock_northbound, expose: all }
devices:
  lamp: { adapter: z, capabilities: [switch] }
"#,
    )
    .expect("valid config");

    // No E_UNUSED_ADAPTER warning for the northbound adapter.
    assert!(
        !cfg.warnings
            .iter()
            .any(|w| w.to_string().contains("E_UNUSED_ADAPTER")),
        "northbound adapter should not warn as unused: {:?}",
        cfg.warnings
    );

    let mut engine = build_engine(&cfg);
    engine.start();
}

#[test]
fn exposing_an_unknown_device_is_a_compile_error() {
    let err = compile_str(
        r#"
adapters:
  z: { type: mock }
  home: { type: mock_northbound, expose: [lamp, ghost] }
devices:
  lamp: { adapter: z, capabilities: [switch] }
"#,
    )
    .unwrap_err();

    assert!(
        err.0
            .iter()
            .any(|d| d.to_string().contains("E_UNKNOWN_EXPOSED_DEVICE")),
        "expected E_UNKNOWN_EXPOSED_DEVICE, got: {err}"
    );
}

#[test]
fn a_bad_expose_keyword_is_rejected() {
    let err = compile_str(
        r#"
adapters:
  z: { type: mock }
  home: { type: mock_northbound, expose: everything }
devices:
  lamp: { adapter: z, capabilities: [switch] }
"#,
    )
    .unwrap_err();

    assert!(
        err.0.iter().any(|d| d.to_string().contains("expose")),
        "expected an expose-keyword error, got: {err}"
    );
}
