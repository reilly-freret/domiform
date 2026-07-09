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
use crate::ids::AdapterIdx;
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

/// Like [`build_engine_with_waker`], but with the config file's directory used to
/// resolve `system.runtime_storage_path`. This is what the real-time host uses so
/// runtime state (the `matter_device` fabric store, …) lands next to the config,
/// stable across working directories. Seeds the clock from the real wall clock.
pub fn build_engine_with_waker_in(
    cfg: &CompiledConfig,
    waker: Option<Waker>,
    config_dir: &std::path::Path,
) -> Engine {
    build_engine_full(cfg, waker, now_unix_ms(), config_dir)
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
    // No config path known (tests, one-shot tools): base the runtime storage dir on
    // the process cwd. A real host uses `build_engine_full` to supply the config's
    // own directory, which is the stable default.
    build_engine_full(cfg, waker, boot_epoch_ms, std::path::Path::new("."))
}

/// Like [`build_engine_at`], but with the directory used to resolve a relative or
/// defaulted `system.runtime_storage_path` — the config file's own directory. The
/// host (`main.rs`) passes this so runtime state (e.g. the `matter_device` fabric
/// store) lands next to the config regardless of the process's working directory.
pub fn build_engine_full(
    cfg: &CompiledConfig,
    waker: Option<Waker>,
    boot_epoch_ms: i64,
    config_dir: &std::path::Path,
) -> Engine {
    let runtime_dir = cfg.system.runtime_storage_dir(config_dir);
    let mut engine = Engine::new();

    // Group devices by their config adapter so each adapter can be built with
    // the device registry it needs.
    let mut by_adapter: Vec<Vec<&DeviceDef>> = vec![Vec::new(); cfg.adapters.len()];
    for device in &cfg.devices {
        by_adapter[device.adapter].push(device);
    }

    // One runtime slot per config adapter, in config order. (Slot 0 in the engine
    // is the scheduler, so southbound adapters land at 1..=N.) Northbound adapters
    // aren't dispatch targets and get no slot: their entry is a sentinel that no
    // device ever looks up (nothing binds to a northbound adapter — see below).
    const NO_SLOT: AdapterIdx = usize::MAX;
    let mut runtime_idx = Vec::with_capacity(cfg.adapters.len());
    for (i, adapter) in cfg.adapters.iter().enumerate() {
        // Compilation fails on unknown adapter types, so `plugin` is always
        // `Some` in a successfully built config.
        let plugin = adapter
            .plugin
            .expect("compiled adapter has a registered plugin");
        match plugin.polarity() {
            crate::adapters::Polarity::Southbound => {
                let built = plugin.build(&adapter.config, &by_adapter[i], waker.clone());
                runtime_idx.push(engine.add_adapter(built));
            }
            crate::adapters::Polarity::Northbound => {
                // Resolve the devices this adapter exposes (declared under other
                // adapters; validated in `resolve`) and build it against those.
                let exposed = exposed_devices(cfg, &adapter.config, plugin);
                let ctx = crate::adapters::NorthboundCtx {
                    adapter_name: &adapter.name,
                    runtime_storage_dir: &runtime_dir,
                };
                if let Some(nb) =
                    plugin.build_northbound(&adapter.config, &exposed, waker.clone(), &ctx)
                {
                    engine.add_northbound(nb);
                }
                // No dispatch slot: nothing routes commands to a northbound adapter.
                runtime_idx.push(NO_SLOT);
            }
        }
    }
    for device in &cfg.devices {
        // A device never binds to a northbound adapter (the resolver rejects that),
        // so its slot is always a real southbound one.
        debug_assert_ne!(runtime_idx[device.adapter], NO_SLOT);
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

/// Resolve a northbound adapter's `expose` spec into the concrete `DeviceDef`s it
/// mirrors. `resolve` has already validated that every named device exists, so an
/// unknown name here is impossible and simply contributes nothing (mirroring how
/// the rest of the builder treats values the resolver already checked). A plugin
/// that returns no [`ExposeSpec`](crate::adapters::ExposeSpec) exposes nothing.
fn exposed_devices<'a>(
    cfg: &'a CompiledConfig,
    config: &serde_yaml::Value,
    plugin: &'static dyn crate::adapters::AdapterPlugin,
) -> Vec<&'a DeviceDef> {
    use crate::adapters::ExposeSpec;
    match plugin.expose_spec(config) {
        Some(ExposeSpec::All) => cfg.devices.iter().collect(),
        Some(ExposeSpec::Named(names)) => cfg
            .devices
            .iter()
            .filter(|d| names.iter().any(|n| n == &d.name))
            .collect(),
        None => Vec::new(),
    }
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
