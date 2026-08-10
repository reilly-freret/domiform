//! The REST API: routing, JSON encoding, and the write path into a real engine.
//!
//! Most tests drive `routes::handle` directly with a `Directory` and a
//! `RestApiHandle` — no socket, no threads, fully deterministic. That is the seam
//! the design puts the decision-making behind, so it is where the coverage lives.
//!
//! The write-path tests run through a real `Engine` with a recording southbound
//! adapter, so they assert the command that actually reached the device rather
//! than merely that a request was queued. One test at the end goes over a real
//! socket to prove the wiring.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use serde_json::{json, Value};

use domiform::adapters::rest_api::{self, routes, Directory, Response, RestApiHandle};
use domiform::ids::DeviceId;
use domiform::model::{CapabilityState, Millis};
use domiform::{
    compile_str, Adapter, Command, CompiledConfig, DispatchOutcome, Engine, Event, RestApiServer,
};

/// A config exercising every shape the API reports: writable capabilities, a
/// read-only sensor, a write-only IR blaster, a scene, and a rule.
const CONFIG: &str = r#"
system:
  name: home
  timezone: America/New_York
adapters:
  bench: { type: mock }
devices:
  kitchen_lamp:
    adapter: bench
    room: kitchen
    capabilities: [switch, brightness, color_temperature]
  motion_sensor:
    adapter: bench
    room: hall
    capabilities: [occupancy, battery]
  blaster:
    adapter: bench
    capabilities: [ir_transmitter]
scenes:
  evening:
    - turn_on: kitchen_lamp
    - set_brightness: { device: kitchen_lamp, value: 40 }
rules:
  motion_lights:
    when: { changed: { device: motion_sensor, capability: occupancy, to: true } }
    then:
      - activate_scene: evening
"#;

fn config() -> CompiledConfig {
    compile_str(CONFIG).expect("valid config")
}

/// A read-only fixture: a directory plus a handle whose engine side is dropped.
/// Enough for every GET, and for asserting request *validation* without caring
/// where the request goes.
fn fixture() -> (CompiledConfig, Directory, RestApiHandle) {
    let cfg = config();
    let directory = Directory::from_config(&cfg);
    let (_adapter, _observer, handle) = rest_api::channel(None);
    (cfg, directory, handle)
}

fn get(directory: &Directory, handle: &RestApiHandle, path: &str) -> Response {
    routes::handle(directory, handle, "GET", path, b"")
}

fn post(directory: &Directory, handle: &RestApiHandle, path: &str, body: &str) -> Response {
    routes::handle(directory, handle, "POST", path, body.as_bytes())
}

/// The device object for `name` out of a `GET /devices` response.
fn device_in(body: &Value, name: &str) -> Value {
    body["devices"]
        .as_array()
        .expect("devices array")
        .iter()
        .find(|d| d["name"] == name)
        .unwrap_or_else(|| panic!("no device named {name} in {body}"))
        .clone()
}

fn error_code(response: &Response) -> String {
    response.json_body()["error"]["code"]
        .as_str()
        .unwrap_or("<missing>")
        .to_string()
}

// --- read path ---------------------------------------------------------------

#[test]
fn a_declared_but_unreported_capability_serializes_as_null() {
    let (_cfg, directory, handle) = fixture();

    let response = get(&directory, &handle, "/devices");
    assert_eq!(response.status, 200);

    // Nothing has been folded yet, so every declared capability is an explicit
    // `null` — the wire form of the engine's `Truth::Unknown`, distinct from
    // `false` or `0`.
    let lamp = device_in(&response.json_body(), "kitchen_lamp");
    assert_eq!(
        lamp["capabilities"],
        json!({ "switch": null, "brightness": null, "color_temperature": null })
    );
    // `ir_transmitter` is write-only, so it is always null. Expected, not a gap.
    let blaster = device_in(&response.json_body(), "blaster");
    assert_eq!(blaster["capabilities"], json!({ "ir_transmitter": null }));
}

#[test]
fn folded_state_is_reflected_in_get_devices() {
    let cfg = config();
    let directory = Directory::from_config(&cfg);
    let (adapter, _observer, handle) = rest_api::channel(None);

    let mut engine = Engine::new();
    engine.add_northbound(Box::new(adapter));
    let lamp = cfg.device_id("kitchen_lamp").expect("device exists");

    engine.inject(Event::StateReported {
        device: lamp,
        state: CapabilityState::Switch(true),
    });
    engine.inject(Event::StateReported {
        device: lamp,
        state: CapabilityState::Brightness(40),
    });

    let body = get(&directory, &handle, "/devices").json_body();
    let lamp_json = device_in(&body, "kitchen_lamp");
    assert_eq!(lamp_json["room"], json!("kitchen"));
    assert_eq!(lamp_json["synthetic"], json!(false));
    assert_eq!(
        lamp_json["capabilities"],
        // The unreported capability stays null alongside the two known ones.
        json!({ "switch": true, "brightness": 40, "color_temperature": null })
    );
}

#[test]
fn the_synthetic_clock_device_is_listed_with_its_capabilities() {
    let cfg = config();
    let directory = Directory::from_config(&cfg);
    let (adapter, _observer, handle) = rest_api::channel(None);

    let mut engine = Engine::new();
    engine.add_northbound(Box::new(adapter));
    engine.inject(Event::StateReported {
        device: cfg.clock_device(),
        state: CapabilityState::TimeOfDay(745),
    });
    engine.inject(Event::StateReported {
        device: cfg.clock_device(),
        state: CapabilityState::SunUp(true),
    });

    let clock = device_in(&get(&directory, &handle, "/devices").json_body(), "clock");
    assert_eq!(clock["synthetic"], json!(true));
    assert_eq!(clock["room"], json!(null));
    assert_eq!(
        clock["capabilities"],
        json!({ "time_of_day": 745, "sun_up": true })
    );
}

#[test]
fn get_one_device_returns_it_and_404s_an_unknown_name() {
    let (_cfg, directory, handle) = fixture();

    let response = get(&directory, &handle, "/devices/kitchen_lamp");
    assert_eq!(response.status, 200);
    assert_eq!(response.json_body()["name"], json!("kitchen_lamp"));

    let response = get(&directory, &handle, "/devices/kitchen_lmap");
    assert_eq!(response.status, 404);
    assert_eq!(error_code(&response), "unknown_device");
}

#[test]
fn get_system_reports_the_config_name_timezone_and_counts() {
    let (_cfg, directory, handle) = fixture();

    let body = get(&directory, &handle, "/system").json_body();
    assert_eq!(body["name"], json!("home"));
    assert_eq!(body["timezone"], json!("America/New_York"));
    // Three declared devices plus the synthetic clock.
    assert_eq!(body["devices"], json!(4));
    assert_eq!(body["scenes"], json!(1));
    assert_eq!(body["rules"], json!(1));
    assert_eq!(body["version"], json!(env!("CARGO_PKG_VERSION")));
    // Virtual engine time since boot, not a Unix timestamp.
    assert_eq!(body["engine_now_ms"], json!(0));
}

#[test]
fn get_scenes_lists_scenes_with_their_command_counts() {
    let (_cfg, directory, handle) = fixture();

    let body = get(&directory, &handle, "/scenes").json_body();
    assert_eq!(
        body,
        json!({ "scenes": [{ "name": "evening", "commands": 2 }] })
    );
}

#[test]
fn get_rules_reflects_a_rule_that_fired() {
    let cfg = config();
    let directory = Directory::from_config(&cfg);
    let (adapter, observer, handle) = rest_api::channel(None);

    let mut engine = Engine::new();
    engine.add_northbound(Box::new(adapter));
    // `rule_considered` reaches only the ordinary observer list, never the
    // northbound one — so the observer half must be registered too.
    engine.add_observer(Box::new(observer));
    for rule in &cfg.rules {
        engine.add_rule(rule.clone());
    }

    // Before anything happens: never considered.
    let body = get(&directory, &handle, "/rules").json_body();
    assert_eq!(
        body["rules"][0],
        json!({
            "name": "motion_lights",
            "last_considered_ms": null,
            "last_truth": null,
            "last_fired_ms": null,
            "fire_count": 0,
        })
    );

    // Advance so the adapter records `now`, then trip the rule's trigger.
    engine.advance(1000);
    engine.inject(Event::StateReported {
        device: cfg.device_id("motion_sensor").expect("device exists"),
        state: CapabilityState::Occupancy(true),
    });

    let body = get(&directory, &handle, "/rules").json_body();
    assert_eq!(body["rules"][0]["fire_count"], json!(1));
    assert_eq!(body["rules"][0]["last_truth"], json!("true"));
    // Stamped with the engine's virtual `now` as of the tick that preceded the
    // drain — exact, not approximate.
    assert_eq!(body["rules"][0]["last_considered_ms"], json!(1000));
    assert_eq!(body["rules"][0]["last_fired_ms"], json!(1000));
}

// --- write path --------------------------------------------------------------
//
// These run through a real `Engine` and assert the command that actually reached
// the device, not merely that the request was accepted.

/// Records every command it is handed, echoing state back like a real device.
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

/// An engine whose devices are all bound to one recorder, with the REST adapter
/// registered — the closest thing to the real host wiring that stays offline.
struct Harness {
    cfg: CompiledConfig,
    directory: Directory,
    handle: RestApiHandle,
    engine: Engine,
    recorder: Recorder,
}

fn harness() -> Harness {
    let cfg = config();
    let directory = Directory::from_config(&cfg);
    let (adapter, observer, handle) = rest_api::channel(None);
    let recorder = Recorder::default();

    let mut engine = Engine::new();
    let idx = engine.add_adapter(Box::new(recorder.clone()));
    for device in &cfg.devices {
        engine.bind_device(device.id, idx);
    }
    // Both halves, exactly as `main.rs` wires them.
    engine.add_northbound(Box::new(adapter));
    engine.add_observer(Box::new(observer));
    for scene in &cfg.scenes {
        engine.add_scene(scene.id, scene.commands.clone());
    }

    Harness {
        cfg,
        directory,
        handle,
        engine,
        recorder,
    }
}

impl Harness {
    fn post(&self, path: &str, body: &str) -> Response {
        post(&self.directory, &self.handle, path, body)
    }

    /// Let the engine tick, draining whatever the API queued.
    fn pump(&mut self) {
        self.engine.advance(1);
    }

    fn device(&self, name: &str) -> DeviceId {
        self.cfg.device_id(name).expect("device exists")
    }
}

#[test]
fn a_set_intent_reaches_the_device_as_a_command() {
    let mut h = harness();

    let response = h.post("/devices/kitchen_lamp/intent", r#"{"set":{"switch":true}}"#);
    // 202, not 200: the request is queued, and the device's echo lands later.
    // Returning a state here would mean inventing one.
    assert_eq!(response.status, 202);
    assert_eq!(response.json_body(), json!({ "accepted": true }));

    // Nothing has reached the device yet — the engine has not run.
    assert!(h.recorder.commands().is_empty());

    h.pump();
    assert_eq!(
        h.recorder.commands(),
        vec![Command::SetSwitch {
            device: h.device("kitchen_lamp"),
            on: true
        }]
    );
}

#[test]
fn a_toggle_intent_resolves_against_the_store() {
    let mut h = harness();
    let lamp = h.device("kitchen_lamp");

    // Settle the store at `on` via a device report.
    h.engine.inject(Event::StateReported {
        device: lamp,
        state: CapabilityState::Switch(true),
    });

    assert_eq!(
        h.post("/devices/kitchen_lamp/intent", r#"{"toggle":{}}"#)
            .status,
        202
    );
    h.pump();

    // The engine resolved the toggle against the store, so the device saw a
    // concrete SetSwitch — the API never computed `!current` itself.
    assert_eq!(
        h.recorder.commands(),
        vec![Command::SetSwitch {
            device: lamp,
            on: false
        }]
    );
}

#[test]
fn an_adjust_brightness_intent_clamps_at_the_floor() {
    let mut h = harness();
    let lamp = h.device("kitchen_lamp");

    h.engine.inject(Event::StateReported {
        device: lamp,
        state: CapabilityState::Brightness(5),
    });

    assert_eq!(
        h.post(
            "/devices/kitchen_lamp/intent",
            r#"{"adjust_brightness":-10}"#
        )
        .status,
        202
    );
    h.pump();

    assert_eq!(
        h.recorder.commands(),
        vec![Command::SetBrightness {
            device: lamp,
            value: 0,
            transition: None,
        }]
    );
}

#[test]
fn a_send_ir_code_intent_reaches_the_blaster() {
    let mut h = harness();

    assert_eq!(
        h.post("/devices/blaster/intent", r#"{"send_ir_code":"aGVsbG8="}"#)
            .status,
        202
    );
    h.pump();

    assert_eq!(
        h.recorder.commands(),
        vec![Command::SendIrCode {
            device: h.device("blaster"),
            code: "aGVsbG8=".to_string(),
        }]
    );
}

#[test]
fn activating_a_scene_runs_its_commands() {
    let mut h = harness();

    let response = h.post("/scenes/evening/activate", "");
    assert_eq!(response.status, 202);

    h.pump();
    let lamp = h.device("kitchen_lamp");
    assert_eq!(
        h.recorder.commands(),
        vec![
            Command::SetSwitch {
                device: lamp,
                on: true
            },
            Command::SetBrightness {
                device: lamp,
                value: 40,
                transition: None,
            },
        ]
    );
}

#[test]
fn activating_an_unknown_scene_is_a_404() {
    let h = harness();

    let response = h.post("/scenes/morning/activate", "");
    assert_eq!(response.status, 404);
    assert_eq!(error_code(&response), "unknown_scene");
}

// --- validation at the edge --------------------------------------------------

#[test]
fn an_undeclared_capability_is_422_and_never_reaches_the_engine() {
    let mut h = harness();

    // The sensor never declared `switch`. Rejecting here turns "a switch command
    // dispatched at an occupancy sensor, failing opaquely three hops later" into
    // an immediate, explicable error.
    let response = h.post(
        "/devices/motion_sensor/intent",
        r#"{"set":{"switch":true}}"#,
    );
    assert_eq!(response.status, 422);
    assert_eq!(error_code(&response), "unsupported_capability");

    h.pump();
    assert!(h.recorder.commands().is_empty());
}

#[test]
fn a_read_only_capability_is_422() {
    let h = harness();

    // `occupancy` is declared by this device, but it is reported *by* the device,
    // never set. The engine would treat it as a silent no-op; the API can do
    // better and say so.
    let response = h.post(
        "/devices/motion_sensor/intent",
        r#"{"set":{"occupancy":true}}"#,
    );
    assert_eq!(response.status, 422);
    assert_eq!(error_code(&response), "unsupported_capability");

    // Same for a relative intent naming a capability the device lacks.
    let response = h.post("/devices/motion_sensor/intent", r#"{"toggle":{}}"#);
    assert_eq!(response.status, 422);
}

#[test]
fn malformed_bodies_are_400() {
    let h = harness();

    let cases = [
        ("", "empty body"),
        ("{}", "no intent key"),
        (r#"{"set":{"switch":true},"toggle":{}}"#, "two intent keys"),
        (
            r#"{"set":{"brightness":"bright"}}"#,
            "non-numeric brightness",
        ),
        (r#"{"set":{"brightness":101}}"#, "out-of-range brightness"),
        (r#"{"set":{"brightness":40.5}}"#, "fractional brightness"),
        (r#"{"set":{}}"#, "no capability named"),
        (
            r#"{"set":{"switch":true,"brightness":40}}"#,
            "two capabilities",
        ),
        ("not json at all", "unparseable"),
        (r#"{"nudge":1}"#, "unknown intent"),
    ];
    for (body, why) in cases {
        let response = h.post("/devices/kitchen_lamp/intent", body);
        assert_eq!(
            response.status, 400,
            "{why}: expected 400, got {response:?}"
        );
        assert_eq!(error_code(&response), "malformed_body", "{why}");
    }
}

#[test]
fn an_intent_for_an_unknown_device_is_404() {
    let h = harness();

    let response = h.post("/devices/nope/intent", r#"{"toggle":{}}"#);
    assert_eq!(response.status, 404);
    assert_eq!(error_code(&response), "unknown_device");
}

#[test]
fn unknown_routes_404_and_wrong_methods_405() {
    let (_cfg, directory, handle) = fixture();

    let response = get(&directory, &handle, "/nope");
    assert_eq!(response.status, 404);
    assert_eq!(error_code(&response), "unknown_route");

    // A known path with the wrong verb is a 405, which tells a client "your URL
    // is right, your method is wrong" — more useful than a 404.
    let response = post(&directory, &handle, "/devices", "");
    assert_eq!(response.status, 405);
    assert_eq!(error_code(&response), "method_not_allowed");

    let response = get(&directory, &handle, "/devices/kitchen_lamp/intent");
    assert_eq!(response.status, 405);
}

// --- the socket layer --------------------------------------------------------

#[test]
fn a_real_request_over_a_socket_returns_parseable_json() {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::sync::atomic::AtomicBool;

    let cfg = config();
    let (_adapter, _observer, handle) = rest_api::channel(None);
    let server = RestApiServer::new(
        // Port 0: the OS picks a free port, and `start` reports which.
        Some(domiform::compile::ast::RawRestApi {
            host: "127.0.0.1".to_string(),
            port: 0,
        }),
        Arc::new(Directory::from_config(&cfg)),
        handle,
        Arc::new(AtomicBool::new(false)),
    );

    let addr = server
        .start()
        .expect("server binds")
        .expect("an enabled server reports its address");

    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .write_all(b"GET /system HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .expect("write request");

    let mut raw = String::new();
    stream.read_to_string(&mut raw).expect("read response");

    assert!(
        raw.starts_with("HTTP/1.1 200 OK"),
        "unexpected status line in: {raw}"
    );
    assert!(raw.contains("Connection: close"), "missing close: {raw}");
    assert!(
        raw.contains("Content-Type: application/json"),
        "missing content type: {raw}"
    );

    let body = raw.split("\r\n\r\n").nth(1).expect("a body");
    let parsed: Value = serde_json::from_str(body).expect("body parses as JSON");
    assert_eq!(parsed["name"], json!("home"));
    assert_eq!(parsed["timezone"], json!("America/New_York"));
}
