//! The `domiform` binary: compile a config into the object graph and run it on
//! the deterministic engine, driving real adapters in real time.
//!
//! ```text
//!   domiform                       # runs ./config.yaml
//!   domiform -c examples/foo.yaml  # an explicit config path
//!   domiform -c foo.yaml -v        # + a full per-event trace
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
use std::time::{Duration, Instant};

use domiform::{build_engine_with_waker, compile_str, wake_channel, StderrObserver};

/// Upper bound on how long the loop will sleep before re-checking, even when no
/// timer is near. A `Waker` wakes us the instant inbound I/O arrives and
/// `next_wake_delay` covers timers/clock; this is a safety net that keeps virtual
/// time roughly tracking real time when a config schedules no wakes.
const MAX_SLEEP: Duration = Duration::from_secs(5);

/// Default config path when `-c` is omitted.
const DEFAULT_CONFIG: &str = "config.yaml";

const HELP: &str = "\
usage: domiform [-c <config.yaml>] [-v]

  -c, --config <path>   config file to run (default: ./config.yaml)
  -v, --verbose         trace every event, condition Truth, and dispatch
  -h, --help            show this help";

struct Args {
    config: String,
    verbose: bool,
}

/// Hand-rolled arg parsing (no `clap` dependency): two flags and an optional
/// bare path. Returns the help/usage text as the `Err` for `-h` and bad input.
fn parse_args() -> Result<Args, String> {
    let mut config: Option<String> = None;
    let mut verbose = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-v" | "--verbose" => verbose = true,
            "-c" | "--config" => {
                config = Some(args.next().ok_or("`-c`/`--config` needs a path")?);
            }
            "-h" | "--help" => {
                // Help is success and belongs on stdout, not the error path.
                println!("{HELP}");
                std::process::exit(0);
            }
            flag if flag.starts_with('-') => {
                return Err(format!("unknown flag '{flag}'\n\n{HELP}"))
            }
            // A bare positional path is accepted too, for convenience.
            path if config.is_none() => config = Some(path.to_string()),
            extra => return Err(format!("unexpected argument '{extra}'\n\n{HELP}")),
        }
    }
    Ok(Args {
        config: config.unwrap_or_else(|| DEFAULT_CONFIG.to_string()),
        verbose,
    })
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };

    // Missing config is a usage error, not a runtime one — fail clearly.
    if !Path::new(&args.config).exists() {
        eprintln!("config file not found: {}", args.config);
        return ExitCode::from(2);
    }
    let src = match std::fs::read_to_string(&args.config) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("could not read {}: {e}", args.config);
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
    let mut engine = build_engine_with_waker(&cfg, Some(waker));

    // The stderr observer always logs failures; `-v` adds the full trace. Hand it
    // the compiler's name tables so lines read in config names, not raw ids.
    let observer = if args.verbose {
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
    engine.set_observer(Box::new(observer));

    engine.start();
    println!(
        "running — Ctrl-C to stop{}",
        if args.verbose { " (verbose)" } else { "" }
    );

    // Event-driven pump: block until the earlier of the next scheduled wake or an
    // inbound `Waker`, advance virtual time by the elapsed wall-clock, drain.
    let mut last = Instant::now();
    loop {
        let timeout = engine
            .next_wake_delay()
            .map(Duration::from_millis)
            .unwrap_or(MAX_SLEEP)
            .min(MAX_SLEEP);
        wakes.wait(timeout);

        let now = Instant::now();
        let elapsed = now.duration_since(last).as_millis() as u64;
        last = now;
        engine.advance(elapsed);
    }
}
