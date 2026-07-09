//! The compiler: config text → resolved object graph (→ runnable engine).
//!
//! Pipeline, mirroring the plan:
//! ```text
//!   YAML ──parse──▶ AST ──resolve──▶ CompiledConfig ──build_engine──▶ Engine
//!         (ast)          (resolve)                     (placeholder adapters)
//! ```
//! The runtime never consults config text after startup; it runs the graph
//! `compile_str` produces.

pub mod ast;
pub mod diagnostic;
pub mod lower;
pub mod resolve;

pub use diagnostic::{CompileErrors, Diagnostic, Severity};
pub use resolve::{
    AdapterDef, CompiledConfig, CompiledScene, CompiledSchedule, DeviceDef, DeviceEvent,
    DeviceMetadata, SystemConfig,
};

use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::adapters::ClockAdapter;
use crate::engine::Engine;
use crate::wake::Waker;

/// Parse and resolve config text into a [`CompiledConfig`], or return every
/// diagnostic found. A syntax error short-circuits to a single `E_PARSE`.
///
/// # Examples
///
/// ```
/// use domiform::compile_str;
///
/// let cfg = compile_str(
///     r#"
/// adapters:
///   z: { type: mock }
/// devices:
///   lamp: { adapter: z, capabilities: [switch] }
/// "#,
/// )
/// .expect("valid config");
///
/// assert_eq!(cfg.devices.len(), 1);
/// assert!(cfg.device_id("lamp").is_some());
/// ```
///
/// Invalid configs collect every error in one pass:
///
/// ```
/// use domiform::compile_str;
///
/// let err = compile_str(
///     r#"
/// adapters:
///   z: { type: not_a_real_adapter }
/// devices:
///   lamp: { adapter: missing, capabilities: [switch] }
/// "#,
/// )
/// .unwrap_err();
///
/// assert!(err.0.len() >= 2);
/// ```
pub fn compile_str(src: &str) -> Result<CompiledConfig, CompileErrors> {
    let raw: ast::RawConfig = match ast::parse_raw_config(src) {
        Ok(raw) => raw,
        Err(e) => {
            return Err(CompileErrors(vec![Diagnostic::error(
                "E_PARSE",
                e.to_string(),
            )]));
        }
    };
    resolve::resolve(raw)
}

/// Construct a runnable [`Engine`] from compiled config: build each adapter, bind
/// every device, wire the synthetic clock, install scenes, and load rules. A
/// compiled YAML file becomes a running automation.
///
/// Real protocol transports are always built (the network is behind a trait seam,
/// not a cargo feature); they (re)connect in the background, so a down
/// broker/controller isn't fatal. `MockDeviceAdapter` serves only `type: mock`.
/// Tests construct engines directly with in-memory transports instead.
///
/// # Examples
///
/// ```
/// use domiform::{build_engine, compile_str};
///
/// let cfg = compile_str(
///     r#"
/// adapters:
///   z: { type: mock }
/// devices:
///   lamp: { adapter: z, capabilities: [switch] }
/// "#,
/// )
/// .unwrap();
///
/// let mut engine = build_engine(&cfg);
/// engine.start();
/// assert_eq!(engine.now(), 0);
/// assert!(cfg.device_id("lamp").is_some());
/// ```
pub fn build_engine(cfg: &CompiledConfig) -> Engine {
    build_engine_with_waker(cfg, None)
}

/// Like [`build_engine`], but hands each async transport a [`Waker`] clone so a
/// real-time host can block until inbound I/O arrives instead of polling. Pass
/// `None` (what `build_engine` does) when driving the engine by hand — tests and
/// one-shot tools don't need it.
///
/// Seeds the clock from the real wall clock ([`SystemTime::now`]). Use
/// [`build_engine_at`] to inject a fixed boot epoch for deterministic time tests.
pub fn build_engine_with_waker(cfg: &CompiledConfig, waker: Option<Waker>) -> Engine {
    build_engine_at(cfg, waker, now_unix_ms())
}

/// The current Unix time in ms, or `0` if the clock is somehow before the epoch.
fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Like [`build_engine_with_waker`], but with an explicit `boot_epoch_ms` (real
/// Unix time, ms) for the clock adapter. This is the injection point that keeps
/// wall-clock time out of the deterministic core: the engine's virtual clock
/// starts at 0 and the wall instant is `boot_epoch_ms + engine_now`, so a test
/// that passes a fixed epoch and drives `advance` by hand is fully replayable.
pub fn build_engine_at(cfg: &CompiledConfig, waker: Option<Waker>, boot_epoch_ms: i64) -> Engine {
    let mut engine = Engine::new();

    // Group devices by their config adapter so each adapter can be built with
    // the device registry it needs.
    let mut by_adapter: Vec<Vec<&DeviceDef>> = vec![Vec::new(); cfg.adapters.len()];
    for device in &cfg.devices {
        by_adapter[device.adapter].push(device);
    }

    // One runtime adapter slot per config adapter, in config order. (Slot 0 in
    // the engine is the scheduler, so these land at 1..=N — hence the mapping.)
    let mut runtime_idx = Vec::with_capacity(cfg.adapters.len());
    for (i, adapter) in cfg.adapters.iter().enumerate() {
        // Compilation fails on unknown adapter types, so `plugin` is always
        // `Some` in a successfully built config.
        let built = adapter
            .plugin
            .expect("compiled adapter has a registered plugin")
            .build(&adapter.config, &by_adapter[i], waker.clone());
        runtime_idx.push(engine.add_adapter(built));
    }
    for device in &cfg.devices {
        engine.bind_device(device.id, runtime_idx[device.adapter]);
    }

    // The synthetic clock device, backed by a real clock adapter seeded with the
    // boot epoch, configured timezone (already validated in `resolve`), and
    // lat/long for the solar ephemeris. It also fires the wall-clock schedules.
    let tz = chrono_tz::Tz::from_str(&cfg.system.timezone).unwrap_or(chrono_tz::Tz::UTC);
    let schedules = compiled_schedules(cfg);
    let clock = ClockAdapter::new(
        cfg.clock_device(),
        boot_epoch_ms,
        tz,
        cfg.system.latitude,
        cfg.system.longitude,
    )
    .with_schedules(schedules);
    let clock_idx = engine.add_adapter(Box::new(clock));
    engine.bind_device(cfg.clock_device(), clock_idx);

    for scene in &cfg.scenes {
        engine.add_scene(scene.id, scene.commands.clone());
    }
    for rule in &cfg.rules {
        engine.add_rule(rule.clone());
    }

    engine
}

/// Parse each compiled schedule's cron string back into a `croner::Cron` for the
/// clock adapter. `resolve` already validated every expression, so a parse
/// failure here is impossible — such an entry is dropped rather than panicking.
fn compiled_schedules(cfg: &CompiledConfig) -> Vec<(crate::ids::ScheduleId, croner::Cron)> {
    cfg.schedules
        .iter()
        .filter_map(|s| {
            croner::Cron::from_str(&s.cron)
                .ok()
                .map(|cron| (s.id, cron))
        })
        .collect()
}
