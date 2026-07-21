//! The `domiform` binary: compile a config into the object graph and run it on
//! the deterministic engine, driving real adapters in real time.
//!
//! ```text
//!   domiform                        # runs ./config.yaml
//!   domiform -c examples/foo.yaml   # an explicit config path
//!   domiform -c foo.yaml -v         # + a full per-event trace
//!   domiform -c foo.yaml --check    # validate only, then exit (no engine/I/O)
//!   domiform --check 'examples/*.yaml'   # validate many at once (glob)
//! ```
//!
//! The real zigbee2mqtt / Matter transports are always compiled in; only
//! `type: mock` adapters are in-memory. The transports (re)connect in the
//! background, so a down broker/controller isn't fatal at startup.
//!
//! This is the runtime *host*: it owns the real-time pump (sleep until the next
//! scheduled wake or an inbound `Waker`, advance virtual time by the elapsed
//! wall-clock, drain). The engine and adapters stay agreement-free of wall time.

use std::path::Path;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use domiform::{build_engine_with_waker_in, compile_str, wake_channel, StderrObserver};
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

/// Upper bound on how long the loop will sleep before re-checking, even when no
/// timer is near. A `Waker` wakes us the instant inbound I/O arrives and
/// `next_wake_delay` covers timers/clock; this is a safety net that keeps virtual
/// time roughly tracking real time when a config schedules no wakes.
const MAX_SLEEP: Duration = Duration::from_secs(5);

/// Default config path when `-c` is omitted.
const DEFAULT_CONFIG: &str = "config.yaml";

const HELP: &str = "\
usage: domiform [-c <config.yaml>] [--check] [-v]
       domiform --check <config.yaml|glob> [<config.yaml|glob> ...]

  -c, --config <path>   config file to run (default: ./config.yaml)
      --check           compile and report problems, then exit without running.
                        Accepts multiple paths and globs (e.g. 'examples/*.yaml')
                        and validates each; exits non-zero if any fails.
  -v, --verbose         trace every event, condition Truth, and dispatch
  -h, --help            show this help";

struct Args {
    /// Config path(s)/glob pattern(s), in the order given. Empty means the
    /// default. Multiple entries are only valid with `--check`.
    configs: Vec<String>,
    verbose: bool,
    /// Compile only: validate the config(s) and exit, without starting the
    /// engine or touching any transport. Handy for editors, pre-commit hooks,
    /// and CI. Only this mode accepts more than one config / a glob.
    check: bool,
}

/// Hand-rolled arg parsing (no `clap` dependency): two flags and an optional
/// bare path. Returns the help/usage text as the `Err` for `-h` and bad input.
fn parse_args() -> Result<Args, String> {
    let mut configs: Vec<String> = Vec::new();
    let mut verbose = false;
    let mut check = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-v" | "--verbose" => verbose = true,
            "--check" => check = true,
            "-c" | "--config" => {
                configs.push(args.next().ok_or("`-c`/`--config` needs a path")?);
            }
            "-h" | "--help" => {
                // Help is success and belongs on stdout, not the error path.
                println!("{HELP}");
                std::process::exit(0);
            }
            flag if flag.starts_with('-') => {
                return Err(format!("unknown flag '{flag}'\n\n{HELP}"))
            }
            // Bare positional paths/globs are accepted too, for convenience.
            path => configs.push(path.to_string()),
        }
    }
    Ok(Args {
        configs,
        verbose,
        check,
    })
}

/// Whether a config argument looks like a glob pattern (vs. a literal path).
/// The one place that classifies `*?[`, so run-mode rejection and check-mode
/// expansion can't disagree about what counts as a glob.
fn is_glob(pat: &str) -> bool {
    pat.contains(['*', '?', '['])
}

/// Expand config path(s)/glob pattern(s) into concrete files, in the order the
/// patterns were given and deduped (a file matched by two patterns is checked
/// once). A pattern with no glob metacharacters is passed through as a literal
/// path, so a plain filename still reports "not found" rather than "no match".
/// A glob that matches nothing is an error — it usually means a typo, and we'd
/// rather say so than silently validate zero files.
fn expand_configs(patterns: &[String]) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for pat in patterns {
        // No glob metacharacters => a literal path; existence is checked later.
        if !is_glob(pat) {
            if seen.insert(pat.clone()) {
                out.push(pat.clone());
            }
            continue;
        }
        let entries = glob::glob(pat).map_err(|e| format!("bad pattern '{pat}': {e}"))?;
        let mut matched = 0usize;
        for entry in entries {
            let path = entry.map_err(|e| format!("reading '{pat}': {e}"))?;
            let s = path.to_string_lossy().into_owned();
            matched += 1;
            if seen.insert(s.clone()) {
                out.push(s);
            }
        }
        if matched == 0 {
            return Err(format!("no files match pattern '{pat}'"));
        }
    }
    Ok(out)
}

fn main() -> ExitCode {
    // Initialize a `log` backend so libraries that log via the `log` crate — the
    // `matter_device` adapter's rs-matter node in particular — reach the terminal.
    // rs-matter prints its commissioning pairing code / QR at `info`, so default to
    // `info` unless the user overrides `RUST_LOG`. domiform's own tracing still
    // goes through `StderrObserver`, independent of this.
    //
    // `rs_matter::im::invoker=off`: during commissioning a controller *probes* the
    // node by reading optional/manufacturer clusters it may not implement; rs-matter
    // logs each miss at ERROR (`UnsupportedCluster`/`UnsupportedAttribute`). These
    // are expected and harmless, so we silence that one target by default to keep
    // the log readable. A `RUST_LOG` override still surfaces them when debugging.
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info,rs_matter::im::invoker=off"),
    )
    .init();

    let args = match parse_args() {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };

    if args.check {
        return run_check(&args.configs);
    }

    // Run mode takes exactly one config; globs / multiple paths are check-only.
    let config = match args.configs.as_slice() {
        [] => DEFAULT_CONFIG.to_string(),
        [one] if !is_glob(one) => one.clone(),
        [one] => {
            eprintln!("'{one}' looks like a glob; globs are only supported with --check");
            return ExitCode::from(2);
        }
        _ => {
            eprintln!("only one config can be run; pass --check to validate several");
            return ExitCode::from(2);
        }
    };
    run_engine(&config, args.verbose)
}

/// `--check`: compile each resolved config and report per-file, then exit
/// non-zero if any failed. Never builds an engine or touches a transport, so it
/// validates broker-backed configs offline. Warnings don't fail the check; only
/// compile errors and unreadable/missing files do.
fn run_check(patterns: &[String]) -> ExitCode {
    // With no path given, check the default config (mirrors run mode's default).
    let owned_default;
    let patterns = if patterns.is_empty() {
        owned_default = vec![DEFAULT_CONFIG.to_string()];
        &owned_default
    } else {
        patterns
    };

    let files = match expand_configs(patterns) {
        Ok(f) => f,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };

    let mut ok = 0usize;
    let mut failed = 0usize;
    for path in &files {
        match check_one(path) {
            Ok(()) => ok += 1,
            Err(msg) => {
                eprintln!("{msg}");
                failed += 1;
            }
        }
    }

    // Only summarize when there's more than one file — a single check reads
    // cleaner as just its own `ok:`/error line.
    if files.len() > 1 {
        println!("checked {} file(s): {ok} ok, {failed} failed", files.len());
    }
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Compile a single config for `--check`. Prints its own `ok:`/error line(s) and
/// returns Ok on success. A missing/unreadable file is an error here (returned
/// as the `Err` string) rather than a hard exit, so a batch check keeps going.
fn check_one(path: &str) -> Result<(), String> {
    let src = std::fs::read_to_string(path).map_err(|e| format!("{path}: could not read: {e}"))?;
    let cfg = compile_str(&src).map_err(|errors| format!("{path}:\n{errors}"))?;
    for w in &cfg.warnings {
        eprintln!("{path}: {w}");
    }
    println!(
        "ok: {path} is valid ({} device(s), {} scene(s), {} rule(s))",
        cfg.devices.len(),
        cfg.scenes.len(),
        cfg.rules.len()
    );
    Ok(())
}

/// Compile one config and run it on the engine in real time (the normal mode).
fn run_engine(config: &str, verbose: bool) -> ExitCode {
    // Missing config is a usage error, not a runtime one — fail clearly.
    if !Path::new(config).exists() {
        eprintln!("config file not found: {config}");
        return ExitCode::from(2);
    }
    let src = match std::fs::read_to_string(config) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("could not read {config}: {e}");
            return ExitCode::from(2);
        }
    };

    let cfg = match compile_str(&src) {
        Ok(cfg) => cfg,
        Err(errors) => {
            eprintln!("{errors}");
            return ExitCode::FAILURE;
        }
    };
    for w in &cfg.warnings {
        eprintln!("{w}");
    }
    println!(
        "compiled {} device(s), {} scene(s), {} rule(s)",
        cfg.devices.len(),
        cfg.scenes.len(),
        cfg.rules.len()
    );

    // A wake channel lets transports signal inbound I/O so the loop can block
    // instead of polling; hand each adapter a `Waker` clone via the engine build.
    let (waker, wakes) = wake_channel();
    // Runtime state (e.g. the matter_device fabric store) defaults to living next
    // to the config file, so it's stable no matter where domiform is launched.
    let config_dir = Path::new(config).parent().unwrap_or(Path::new("."));
    let mut engine = build_engine_with_waker_in(&cfg, Some(waker.clone()), config_dir);

    // The stderr observer always logs failures; `-v` adds the full trace. Hand it
    // the compiler's name tables so lines read in config names, not raw ids.
    let observer = if verbose {
        StderrObserver::verbose()
    } else {
        StderrObserver::new()
    }
    .with_names(
        cfg.devices
            .iter()
            .map(|d| (d.id, d.name.clone()))
            // The synthetic clock device isn't in `devices`; name it too.
            .chain(std::iter::once((cfg.clock_device(), "clock".to_string()))),
        cfg.rules.iter().map(|r| (r.id, r.name.clone())),
        cfg.scenes.iter().map(|s| (s.id, s.name.clone())),
    );
    engine.add_observer(Box::new(observer));

    engine.start();
    println!("running {}", if verbose { "(verbose)" } else { "" });

    // Graceful shutdown. Running as PID 1 in a container, the kernel delivers
    // SIGTERM/SIGINT only if we install a handler — otherwise `docker stop` waits
    // out its grace period and then SIGKILLs (the ~10s stall). We flip an atomic
    // the run loop checks between iterations. The `Signals` iterator blocks on its
    // own thread (not in an async-signal context), so it can safely poke the
    // `Waker` to cut short the loop's `wait` and exit promptly; a second signal
    // escalates to an immediate exit for an impatient operator.
    let shutdown = Arc::new(AtomicBool::new(false));
    match Signals::new([SIGINT, SIGTERM]) {
        Ok(mut signals) => {
            let shutdown = shutdown.clone();
            std::thread::Builder::new()
                .name("signals".into())
                .spawn(move || {
                    for _ in signals.forever() {
                        if shutdown.swap(true, Ordering::SeqCst) {
                            std::process::exit(130);
                        }
                        waker.wake();
                    }
                })
                .expect("spawn signal thread");
        }
        // Not fatal: without the handler we just lose fast/graceful exit.
        Err(e) => eprintln!("could not install signal handler: {e} (shutdown may be slow)"),
    }

    // Event-driven pump: block until the earlier of the next scheduled wake or an
    // inbound `Waker`, advance virtual time by the elapsed wall-clock, drain.
    let mut last = Instant::now();
    while !shutdown.load(Ordering::SeqCst) {
        let timeout = engine
            .next_wake_delay()
            .map(Duration::from_millis)
            .unwrap_or(MAX_SLEEP)
            .min(MAX_SLEEP);
        wakes.wait(timeout);

        // A signal may have woken us; re-check before advancing so shutdown
        // doesn't wait on one last needless tick.
        if shutdown.load(Ordering::SeqCst) {
            break;
        }

        let now = Instant::now();
        let elapsed = now.duration_since(last).as_millis() as u64;
        last = now;
        engine.advance(elapsed);
    }

    // Returning drops `engine`, which drops each adapter and its transport (the
    // Matter node thread, the MQTT loop); the process then exits and the OS reaps
    // any remaining background threads.
    println!("shutting down");
    ExitCode::SUCCESS
}
