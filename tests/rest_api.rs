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

use domiform::adapters::rest_api::{
    self, routes, stream::Broadcaster, Directory, Response, RestApiHandle,
};
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
  synthetic: { type: virtual }
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
  knob:
    adapter: bench
    events:
      click: toggle
      cw: brightness_step_up
  # An events-only device on the `virtual` adapter: no capabilities at all. This
  # is the supported idiom for an API-driven automation surface, and it must not
  # be rejected by capability-shaped validation.
  api:
    adapter: synthetic
    events:
      movie_mode: movie_mode
scenes:
  evening:
    - turn_on: kitchen_lamp
    - set_brightness: { device: kitchen_lamp, value: 40 }
rules:
  motion_lights:
    when: { changed: { device: motion_sensor, capability: occupancy, to: true } }
    then:
      - activate_scene: evening
  # The two-stage IR pattern from the homelab config: a button sends a toggle and
  # arms a timer whose rule sends the follow-up code. A client replaying only the
  # first stage would silently get half the behavior — which is exactly why the
  # API fires the *event* rather than reconstructing the commands.
  knob_toggle_ac:
    when: { event: knob.click }
    then:
      - send_ir_code: { device: blaster, code: "QUJDRA==" }
      - schedule_timer: { key: ac_mode_timer, after: 2s }
  ac_mode:
    when: { timer: ac_mode_timer }
    then:
      - send_ir_code: { device: blaster, code: "TU9ERQ==" }
  # Two rules sharing one trigger, disambiguated solely by their conditions — the
  # shape that makes a rule-trigger endpoint ambiguous and the event endpoint
  # unambiguous.
  lamp_on:
    when: { event: knob.cw }
    if: { switch: { device: kitchen_lamp, is_on: false } }
    then:
      - set_brightness: { device: kitchen_lamp, value: 100 }
  lamp_off:
    when: { event: knob.cw }
    if: { switch: { device: kitchen_lamp, is_on: true } }
    then:
      - turn_off: kitchen_lamp
  movie_mode:
    when: { event: api.movie_mode }
    then:
      - set_brightness: { device: kitchen_lamp, value: 15 }
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

/// The rule object for `name` out of a `GET /rules` response. By name, not
/// index, so adding a rule to the fixture cannot silently retarget a test.
fn rule_in(body: &Value, name: &str) -> Value {
    body["rules"]
        .as_array()
        .expect("rules array")
        .iter()
        .find(|r| r["name"] == name)
        .unwrap_or_else(|| panic!("no rule named {name} in {body}"))
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
    // Five declared devices plus the synthetic clock.
    assert_eq!(body["devices"], json!(6));
    assert_eq!(body["scenes"], json!(1));
    assert_eq!(body["rules"], json!(6));
    assert_eq!(body["version"], json!(env!("CARGO_PKG_VERSION")));
    // Virtual engine time since boot, not a Unix timestamp.
    assert_eq!(body["engine_now_ms"], json!(0));
}

#[test]
fn get_scenes_lists_scenes_with_their_commands() {
    let (_cfg, directory, handle) = fixture();

    let body = get(&directory, &handle, "/scenes").json_body();
    assert_eq!(
        body,
        json!({ "scenes": [{
            "name": "evening",
            // The count is retained alongside the rendered members, so a v1
            // client reading `commands` still works.
            "commands": 2,
            "then": [
                { "type": "set_switch", "device": "kitchen_lamp", "on": true },
                { "type": "set_brightness", "device": "kitchen_lamp",
                  "value": 40, "transition_ms": null },
            ],
        }] })
    );
}

#[test]
fn get_scene_by_name_returns_one_scene() {
    let (_cfg, directory, handle) = fixture();

    let body = get(&directory, &handle, "/scenes/evening").json_body();
    assert_eq!(body["name"], json!("evening"));
    assert_eq!(body["commands"], json!(2));

    let missing = get(&directory, &handle, "/scenes/nope");
    assert_eq!(missing.status, 404);
    assert_eq!(error_code(&missing), "unknown_scene");
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

    // Before anything happens: never considered. Asserted per-field rather than
    // as a whole object, so adding a field to the rule shape does not break this
    // test's actual subject (the runtime status).
    let body = get(&directory, &handle, "/rules").json_body();
    let rule = rule_in(&body, "motion_lights");
    assert_eq!(rule["last_considered_ms"], json!(null));
    assert_eq!(rule["last_truth"], json!(null));
    assert_eq!(rule["last_fired_ms"], json!(null));
    assert_eq!(rule["fire_count"], json!(0));

    // Advance so the adapter records `now`, then trip the rule's trigger.
    engine.advance(1000);
    engine.inject(Event::StateReported {
        device: cfg.device_id("motion_sensor").expect("device exists"),
        state: CapabilityState::Occupancy(true),
    });

    let body = get(&directory, &handle, "/rules").json_body();
    let rule = rule_in(&body, "motion_lights");
    assert_eq!(rule["fire_count"], json!(1));
    assert_eq!(rule["last_truth"], json!("true"));
    // Stamped with the engine's virtual `now` as of the tick that preceded the
    // drain — exact, not approximate.
    assert_eq!(rule["last_considered_ms"], json!(1000));
    assert_eq!(rule["last_fired_ms"], json!(1000));
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
    // Rules too: the event endpoint's whole purpose is to reach them, so a
    // harness without them could not test it.
    for rule in &cfg.rules {
        engine.add_rule(rule.clone());
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
            token: None,
        }),
        Arc::new(Directory::from_config(&cfg)),
        handle,
        Arc::new(AtomicBool::new(false)),
        Broadcaster::default(),
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

// --- device events (v2 §4) ---------------------------------------------------
//
// The write surface that reaches *rules*. An injected `Event::Action` is
// indistinguishable from the physical press, so these assert the full behavior
// the button would produce — not merely that a request was accepted.

#[test]
fn firing_an_event_runs_the_rule_including_its_second_stage() {
    let mut h = harness();

    // One POST, standing in for a knob click. The client knows nothing about IR
    // codes or the follow-up timer — that is the entire point.
    let response = h.post("/devices/knob/events/click", "");
    assert_eq!(response.status, 202);
    assert_eq!(response.json_body(), json!({ "accepted": true }));

    h.pump();

    // Stage one: the toggle code went out.
    let blaster = h.device("blaster");
    assert_eq!(
        h.recorder.commands(),
        vec![Command::SendIrCode {
            device: blaster,
            code: "QUJDRA==".to_string(),
        }]
    );

    // Stage two arrives only after the armed timer elapses. A client that had
    // replayed the rule's first command itself would have stopped here and
    // silently gotten half the behavior.
    h.engine.advance(2000);
    assert_eq!(
        h.recorder.commands(),
        vec![
            Command::SendIrCode {
                device: blaster,
                code: "QUJDRA==".to_string(),
            },
            Command::SendIrCode {
                device: blaster,
                code: "TU9ERQ==".to_string(),
            },
        ]
    );
}

#[test]
fn one_event_selects_between_rules_disambiguated_only_by_condition() {
    let mut h = harness();
    let lamp = h.device("kitchen_lamp");

    // Establish a known switch state first. With an empty store the condition
    // evaluates to `Truth::Unknown`, not `false`, so *neither* rule fires — the
    // three-valued logic working as intended, and worth pinning down here.
    h.post("/devices/knob/events/cw", "");
    h.pump();
    assert_eq!(
        h.recorder.commands(),
        vec![],
        "an unknown store satisfies neither condition"
    );

    h.engine.inject(Event::StateReported {
        device: lamp,
        state: CapabilityState::Switch(false),
    });

    // Now the lamp is known-off, so `lamp_on` is the rule whose condition holds.
    h.post("/devices/knob/events/cw", "");
    h.pump();
    assert_eq!(
        h.recorder.commands(),
        vec![Command::SetBrightness {
            device: lamp,
            value: 100,
            transition: None,
        }]
    );

    // Flip the switch to on. (The `SetBrightness` above echoed only brightness —
    // the recorder models a real device, where the two capabilities report
    // independently.) The *same* event now selects the other rule of the pair.
    // A rule-trigger endpoint could not do this without the client tracking
    // which of the two to call, which is exactly the coupling we avoided.
    h.engine.inject(Event::StateReported {
        device: lamp,
        state: CapabilityState::Switch(true),
    });

    h.post("/devices/knob/events/cw", "");
    h.pump();
    assert_eq!(
        h.recorder.commands().last(),
        Some(&Command::SetSwitch {
            device: lamp,
            on: false,
        })
    );
}

#[test]
fn an_events_only_device_with_no_capabilities_accepts_an_event() {
    let mut h = harness();

    // `api` is a `virtual` device declaring events and no capabilities at all —
    // the supported idiom for an API-driven automation surface. Capability-shaped
    // validation must not reject it.
    let response = h.post("/devices/api/events/movie_mode", "");
    assert_eq!(response.status, 202);

    h.pump();
    assert_eq!(
        h.recorder.commands(),
        vec![Command::SetBrightness {
            device: h.device("kitchen_lamp"),
            value: 15,
            transition: None,
        }]
    );
}

#[test]
fn an_undeclared_event_is_a_404_naming_what_the_device_does_declare() {
    let (_cfg, directory, handle) = fixture();

    let response = post(&directory, &handle, "/devices/knob/events/nope", "");
    assert_eq!(response.status, 404);
    assert_eq!(error_code(&response), "unknown_event");
    let message = response.json_body()["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(message.contains("click"), "should list declared: {message}");

    // An unknown *device* is still `unknown_device`, not `unknown_event`.
    let response = post(&directory, &handle, "/devices/nope/events/click", "");
    assert_eq!(response.status, 404);
    assert_eq!(error_code(&response), "unknown_device");
}

#[test]
fn a_device_with_no_events_reports_an_empty_list_and_rejects_any_event() {
    let (_cfg, directory, handle) = fixture();

    let body = get(&directory, &handle, "/devices").json_body();
    // A capability-only device declares no events.
    assert_eq!(device_in(&body, "kitchen_lamp")["events"], json!([]));
    // And an events-only device declares no capabilities.
    let api = device_in(&body, "api");
    assert_eq!(api["capabilities"], json!({}));
    assert_eq!(api["events"], json!(["movie_mode"]));

    let response = post(
        &directory,
        &handle,
        "/devices/kitchen_lamp/events/click",
        "",
    );
    assert_eq!(response.status, 404);
    assert_eq!(error_code(&response), "unknown_event");
}

#[test]
fn get_devices_lists_declared_events_in_config_order() {
    let (_cfg, directory, handle) = fixture();

    let body = get(&directory, &handle, "/devices").json_body();
    assert_eq!(device_in(&body, "knob")["events"], json!(["click", "cw"]));
}

#[test]
fn the_event_route_rejects_a_wrong_method() {
    let (_cfg, directory, handle) = fixture();

    let response = get(&directory, &handle, "/devices/knob/events/click");
    assert_eq!(response.status, 405);
    assert_eq!(error_code(&response), "method_not_allowed");
}

// --- introspection (v2 §5) ---------------------------------------------------

#[test]
fn get_rules_renders_each_trigger_shape_with_names_resolved() {
    let (_cfg, directory, handle) = fixture();
    let body = get(&directory, &handle, "/rules").json_body();

    // An event trigger — the shape a dashboard filters on to build buttons.
    assert_eq!(
        rule_in(&body, "knob_toggle_ac")["trigger"],
        json!({ "type": "event", "device": "knob", "event": "click" })
    );
    // An edge trigger on a bool capability.
    assert_eq!(
        rule_in(&body, "motion_lights")["trigger"],
        json!({
            "type": "changed", "device": "motion_sensor",
            "capability": "occupancy", "to": true,
        })
    );
    // A timer trigger. `TimerKey` is a plain string, so it needs no lookup.
    assert_eq!(
        rule_in(&body, "ac_mode")["trigger"],
        json!({ "type": "timer", "key": "ac_mode_timer" })
    );
}

#[test]
fn get_rules_renders_conditions_and_omits_ir_codes() {
    let (_cfg, directory, handle) = fixture();
    let body = get(&directory, &handle, "/rules").json_body();

    // A rule with no `if:` renders `true`, not a tagged empty node.
    assert_eq!(rule_in(&body, "knob_toggle_ac")["condition"], json!(true));
    assert_eq!(
        rule_in(&body, "lamp_off")["condition"],
        json!({
            "type": "bool_equals", "device": "kitchen_lamp",
            "capability": "switch", "value": true,
        })
    );

    // The IR code is deliberately absent: 100+ char base64 blobs would dominate
    // the payload, and reaching IR *without* handling the code is the point.
    let then = &rule_in(&body, "knob_toggle_ac")["then"];
    assert_eq!(
        then[0],
        json!({ "type": "send_ir_code", "device": "blaster" })
    );
    assert!(
        !then.to_string().contains("QUJDRA"),
        "the IR code must not appear on the wire: {then}"
    );
    assert_eq!(
        then[1],
        json!({ "type": "schedule_timer", "key": "ac_mode_timer", "after_ms": 2000 })
    );
}

#[test]
fn get_rule_by_name_merges_shape_with_runtime_status() {
    let (_cfg, directory, handle) = fixture();

    let body = get(&directory, &handle, "/rules/motion_lights").json_body();
    // Static shape from config...
    assert_eq!(body["name"], json!("motion_lights"));
    assert_eq!(body["trigger"]["type"], json!("changed"));
    assert_eq!(body["for_ms"], json!(null));
    // ...merged with runtime status from the mirror.
    assert_eq!(body["fire_count"], json!(0));
    assert_eq!(body["last_truth"], json!(null));

    let missing = get(&directory, &handle, "/rules/nope");
    assert_eq!(missing.status, 404);
    assert_eq!(error_code(&missing), "unknown_rule");
}

// --- the stream (v2 §6) ------------------------------------------------------
//
// These drive the `Broadcaster` directly rather than over a socket: the socket
// layer is a thin write loop, while the fan-out and its backpressure are where
// the risk lives. The engine-isolation tests below are the important ones.

/// An engine wired to a broadcaster, as `main.rs` wires it.
fn streaming_harness() -> (Harness, Broadcaster) {
    let cfg = config();
    let directory = Arc::new(Directory::from_config(&cfg));
    let (adapter, mut observer, handle) = rest_api::channel(None);
    let broadcaster = Broadcaster::default();
    observer.with_stream(broadcaster.clone(), Arc::clone(&directory));

    let recorder = Recorder::default();
    let mut engine = Engine::new();
    let idx = engine.add_adapter(Box::new(recorder.clone()));
    for device in &cfg.devices {
        engine.bind_device(device.id, idx);
    }
    engine.add_northbound(Box::new(adapter));
    engine.add_observer(Box::new(observer));
    for scene in &cfg.scenes {
        engine.add_scene(scene.id, scene.commands.clone());
    }
    for rule in &cfg.rules {
        engine.add_rule(rule.clone());
    }

    let harness = Harness {
        cfg,
        directory: Directory::from_config(&config()),
        handle,
        engine,
        recorder,
    };
    (harness, broadcaster)
}

/// Drain whatever a subscription has pending, without blocking.
fn drain(sub: &domiform::adapters::rest_api::stream::Subscription) -> Vec<(String, Value)> {
    use domiform::adapters::rest_api::stream::Batch;
    match sub.next_batch(std::time::Duration::from_millis(0)) {
        Batch::Frames { frames, .. } => frames
            .into_iter()
            .map(|f| (f.event.to_string(), f.data))
            .collect(),
        _ => Vec::new(),
    }
}

#[test]
fn a_folded_state_reaches_a_subscriber_as_a_state_event() {
    let (mut h, broadcaster) = streaming_harness();
    let sub = broadcaster.subscribe();

    let lamp = h.device("kitchen_lamp");
    h.engine.inject(Event::StateReported {
        device: lamp,
        state: CapabilityState::Switch(true),
    });

    assert_eq!(
        drain(&sub),
        vec![(
            "state".to_string(),
            json!({ "device": "kitchen_lamp", "capability": "switch", "value": true })
        )]
    );
}

#[test]
fn a_fired_rule_and_the_action_that_caused_it_both_reach_a_subscriber() {
    let (mut h, broadcaster) = streaming_harness();
    let sub = broadcaster.subscribe();

    h.post("/devices/api/events/movie_mode", "");
    h.pump();

    let frames = drain(&sub);
    let kinds: Vec<&str> = frames.iter().map(|(k, _)| k.as_str()).collect();
    // The action that entered the queue, and the rule it fired.
    assert!(kinds.contains(&"action"), "expected an action in {kinds:?}");
    assert!(kinds.contains(&"rule"), "expected a rule in {kinds:?}");

    let action = frames.iter().find(|(k, _)| k == "action").expect("action");
    assert_eq!(
        action.1,
        // depth 0: this event started a causal chain. It deliberately does *not*
        // say whether a human or the API produced it — both are depth 0.
        json!({ "device": "api", "event": "movie_mode", "depth": 0 })
    );

    let rule = frames.iter().find(|(k, _)| k == "rule").expect("rule");
    assert_eq!(
        rule.1,
        json!({ "rule": "movie_mode", "truth": "true", "fired": true })
    );
}

#[test]
fn a_command_failure_reaches_a_subscriber_with_the_command_rendered() {
    let cfg = config();
    let directory = Arc::new(Directory::from_config(&cfg));
    let (adapter, mut observer, handle) = rest_api::channel(None);
    let broadcaster = Broadcaster::default();
    observer.with_stream(broadcaster.clone(), Arc::clone(&directory));

    /// An adapter that always refuses, permanently.
    struct Broken;
    impl Adapter for Broken {
        fn dispatch(&mut self, _cmd: &Command, _now: Millis) -> DispatchOutcome {
            DispatchOutcome::Permanent("device is offline".into())
        }
    }

    let mut engine = Engine::new();
    let idx = engine.add_adapter(Box::new(Broken));
    for device in &cfg.devices {
        engine.bind_device(device.id, idx);
    }
    engine.add_northbound(Box::new(adapter));
    engine.add_observer(Box::new(observer));

    let sub = broadcaster.subscribe();
    let lamp = cfg.device_id("kitchen_lamp").expect("device exists");
    engine.inject(Event::RequestedChange {
        device: lamp,
        desired: domiform::model::Desired::Set(CapabilityState::Switch(true)),
    });

    let frames = drain(&sub);
    let failure = frames
        .iter()
        .find(|(k, _)| k == "command_failed")
        .unwrap_or_else(|| panic!("expected a command_failed in {frames:?}"));
    assert_eq!(
        failure.1["command"],
        json!({ "type": "set_switch", "device": "kitchen_lamp", "on": true })
    );
    assert_eq!(failure.1["reason"], json!("device is offline"));

    drop(handle);
}

#[test]
fn a_saturated_subscriber_never_blocks_the_engine() {
    // The invariant that matters most: `Observer` callbacks run on the engine
    // thread inside `drain`, so a subscriber that has stopped reading must not
    // be able to stall the run loop.
    let (mut h, broadcaster) = streaming_harness();

    // Attach subscribers and never read them.
    let _subs: Vec<_> = (0..8).map(|_| broadcaster.subscribe()).collect();

    // Far more folds than the queues can hold, so every one of them overflows.
    let lamp = h.device("kitchen_lamp");
    for n in 0..(domiform::adapters::rest_api::stream::QUEUE_CAPACITY * 2) {
        h.engine.inject(Event::StateReported {
            device: lamp,
            state: CapabilityState::Brightness((n % 101) as u8),
        });
    }

    // The engine is still live and still dispatching: a POST placed now reaches
    // the device exactly as it would with no subscribers at all.
    h.post("/devices/kitchen_lamp/intent", r#"{"set":{"switch":true}}"#);
    h.pump();
    assert_eq!(
        h.recorder.commands().last(),
        Some(&Command::SetSwitch {
            device: lamp,
            on: true,
        })
    );
}

#[test]
fn an_overflowed_subscriber_is_told_its_view_has_a_hole() {
    use domiform::adapters::rest_api::stream::{Batch, QUEUE_CAPACITY};

    let (mut h, broadcaster) = streaming_harness();
    let sub = broadcaster.subscribe();

    let lamp = h.device("kitchen_lamp");
    for n in 0..(QUEUE_CAPACITY + 10) {
        h.engine.inject(Event::StateReported {
            device: lamp,
            state: CapabilityState::Brightness((n % 101) as u8),
        });
    }

    match sub.next_batch(std::time::Duration::from_millis(0)) {
        Batch::Frames { frames, lagged } => {
            assert!(lagged, "the client must learn frames were dropped");
            assert_eq!(frames.len(), QUEUE_CAPACITY);
        }
        _ => panic!("expected frames"),
    }
}

#[test]
fn a_dropped_subscriber_frees_its_slot_without_engine_involvement() {
    let (mut h, broadcaster) = streaming_harness();

    let subs: Vec<_> = (0..8).map(|_| broadcaster.subscribe()).collect();
    assert_eq!(broadcaster.subscriber_count(), 8);
    drop(subs);
    assert_eq!(broadcaster.subscriber_count(), 0);

    // The engine neither knew nor cared.
    let lamp = h.device("kitchen_lamp");
    h.engine.inject(Event::StateReported {
        device: lamp,
        state: CapabilityState::Switch(true),
    });
}

#[test]
fn without_a_stream_attached_the_observer_still_serves_get_rules() {
    // A host that does not serve the stream pays nothing, and `GET /rules` is
    // unaffected — the mirror is maintained either way.
    let mut h = harness();
    h.post("/devices/api/events/movie_mode", "");
    h.pump();

    let body = get(&h.directory, &h.handle, "/rules").json_body();
    assert_eq!(rule_in(&body, "movie_mode")["fire_count"], json!(1));
}

// --- auth (v2 §7) ------------------------------------------------------------

/// Start a server on an ephemeral port, optionally token-protected.
fn serve_with_token(token: Option<&str>) -> (std::net::SocketAddr, RestApiServer) {
    use std::sync::atomic::AtomicBool;

    let cfg = config();
    let (_adapter, _observer, handle) = rest_api::channel(None);
    let server = RestApiServer::new(
        Some(domiform::compile::ast::RawRestApi {
            host: "127.0.0.1".to_string(),
            port: 0,
            token: token.map(|t| t.to_string()),
        }),
        Arc::new(Directory::from_config(&cfg)),
        handle,
        Arc::new(AtomicBool::new(false)),
        Broadcaster::default(),
    );
    let addr = server
        .start()
        .expect("server binds")
        .expect("an enabled server reports its address");
    (addr, server)
}

/// Issue a raw request and return its status line plus body.
fn raw_request(addr: std::net::SocketAddr, request: &str) -> String {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let mut stream = TcpStream::connect(addr).expect("connect");
    stream.write_all(request.as_bytes()).expect("write");
    let mut raw = String::new();
    stream.read_to_string(&mut raw).expect("read");
    raw
}

fn status_of(raw: &str) -> u16 {
    raw.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

#[test]
fn with_no_token_configured_every_route_is_open() {
    let (addr, _server) = serve_with_token(None);
    let raw = raw_request(addr, "GET /system HTTP/1.1\r\nHost: x\r\n\r\n");
    assert_eq!(status_of(&raw), 200);
}

#[test]
fn a_configured_token_is_required_on_every_route() {
    let (addr, _server) = serve_with_token(Some("s3cret"));

    // Reads are protected too: device state reveals whether anyone is home.
    for path in ["/system", "/devices", "/rules", "/scenes", "/stream"] {
        let raw = raw_request(addr, &format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n"));
        assert_eq!(
            status_of(&raw),
            401,
            "{path} must require the token, got: {raw}"
        );
    }

    // And writes.
    let raw = raw_request(
        addr,
        "POST /devices/knob/events/click HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n",
    );
    assert_eq!(status_of(&raw), 401);
}

#[test]
fn the_right_token_is_accepted_and_a_wrong_one_is_forbidden() {
    let (addr, _server) = serve_with_token(Some("s3cret"));

    let ok = raw_request(
        addr,
        "GET /system HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer s3cret\r\n\r\n",
    );
    assert_eq!(status_of(&ok), 200);

    // Well-formed but wrong is 403 — distinct from "you did not authenticate".
    let wrong = raw_request(
        addr,
        "GET /system HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer nope\r\n\r\n",
    );
    assert_eq!(status_of(&wrong), 403);

    // A malformed credential is 401, not 403.
    let malformed = raw_request(
        addr,
        "GET /system HTTP/1.1\r\nHost: x\r\nAuthorization: Basic s3cret\r\n\r\n",
    );
    assert_eq!(status_of(&malformed), 401);

    // The scheme is case-insensitive; the credential is not.
    let cased = raw_request(
        addr,
        "GET /system HTTP/1.1\r\nHost: x\r\nAuthorization: bearer s3cret\r\n\r\n",
    );
    assert_eq!(status_of(&cased), 200);
}

#[test]
fn a_stream_over_a_socket_sends_a_snapshot_then_deltas() {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::sync::atomic::AtomicBool;

    let cfg = config();
    let directory = Arc::new(Directory::from_config(&cfg));
    let (adapter, mut observer, handle) = rest_api::channel(None);
    let broadcaster = Broadcaster::default();
    observer.with_stream(broadcaster.clone(), Arc::clone(&directory));

    let server = RestApiServer::new(
        Some(domiform::compile::ast::RawRestApi {
            host: "127.0.0.1".to_string(),
            port: 0,
            token: None,
        }),
        Arc::clone(&directory),
        handle,
        Arc::new(AtomicBool::new(false)),
        broadcaster.clone(),
    );
    let addr = server.start().expect("binds").expect("an address");

    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .write_all(b"GET /stream HTTP/1.1\r\nHost: x\r\n\r\n")
        .expect("write");
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .expect("timeout");

    // Read until the snapshot frame is complete. The head and the snapshot are
    // written separately, and TCP is a byte stream — a single `read` is not
    // guaranteed to return both, so accumulate rather than assuming framing.
    let mut buf = [0u8; 4096];
    let mut head = String::new();
    while !head.contains("event: snapshot") {
        let n = stream.read(&mut buf).expect("read head");
        assert!(n > 0, "connection closed before the snapshot: {head}");
        head.push_str(&String::from_utf8_lossy(&buf[..n]));
    }

    assert!(head.starts_with("HTTP/1.1 200 OK"), "bad status: {head}");
    assert!(
        head.contains("Content-Type: text/event-stream"),
        "bad content type: {head}"
    );
    // A stream has no end, so it must not claim a length.
    assert!(
        !head.contains("Content-Length"),
        "a stream must not send Content-Length: {head}"
    );
    // The snapshot carries current state, so a client is correct from its first
    // frame without a separate GET.
    assert!(
        head.contains("kitchen_lamp"),
        "snapshot lacks devices: {head}"
    );

    // A delta pushed after the snapshot arrives on the same connection.
    let mut engine = Engine::new();
    engine.add_northbound(Box::new(adapter));
    engine.add_observer(Box::new(observer));
    engine.inject(Event::StateReported {
        device: cfg.device_id("kitchen_lamp").expect("device"),
        state: CapabilityState::Switch(true),
    });

    let n = stream.read(&mut buf).expect("read delta");
    let delta = String::from_utf8_lossy(&buf[..n]).to_string();
    assert!(delta.contains("event: state"), "no state event: {delta}");
    assert!(delta.contains("kitchen_lamp"), "wrong device: {delta}");

    broadcaster.shutdown();
}
