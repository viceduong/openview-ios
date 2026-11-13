//! The `tracing` subscriber of the CLI binaries.
//!
//! Output goes to **stderr** (stdout carries [`crate::status`] events), with
//! ANSI colours only when stderr is a terminal. The filter is chosen, first
//! match wins, from:
//!
//! 1. `--log-level <filter>` (any `EnvFilter` directive, e.g. `debug` or
//!    `info,ovtransport=trace`);
//! 2. `-v` (debug) / `-vv` (trace);
//! 3. the `OPENVIEW_LOG` environment variable;
//! 4. `info`.
//!
//! At `info` only lifecycle lines appear (startup, device ID, relay wait,
//! peer connected / disconnected, direct path changes, UPnP result,
//! warnings and errors). Periodic statistics are `debug`, per-event noise
//! `trace`.

use std::io::IsTerminal;

use tracing_subscriber::EnvFilter;

/// Environment variable holding the default log filter.
pub const LOG_ENV: &str = "OPENVIEW_LOG";

/// Install the global subscriber. `verbose` counts `-v` flags. Safe to call
/// once per process; later calls are ignored.
pub fn init(verbose: u8, level: Option<&str>) {
    let (directive, from) = choose(verbose, level, std::env::var(LOG_ENV).ok().as_deref());
    let (filter, bad) = match EnvFilter::try_new(&directive) {
        Ok(f) => (f, None),
        Err(e) => (EnvFilter::new("info"), Some(e)),
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        // Module targets only help when debugging; keep lifecycle lines short.
        .with_target(directive != "info")
        .try_init();
    if let Some(e) = bad {
        tracing::warn!("invalid log filter `{directive}` (from {from}): {e}; using `info`");
    }
}

/// The filter directive and where it came from (pure; unit-tested).
fn choose(verbose: u8, level: Option<&str>, env: Option<&str>) -> (String, &'static str) {
    if let Some(l) = level.map(str::trim).filter(|l| !l.is_empty()) {
        return (l.to_owned(), "--log-level");
    }
    match verbose {
        0 => {}
        1 => return ("debug".to_owned(), "-v"),
        _ => return ("trace".to_owned(), "-vv"),
    }
    if let Some(e) = env.map(str::trim).filter(|e| !e.is_empty()) {
        return (e.to_owned(), LOG_ENV);
    }
    ("info".to_owned(), "default")
}

#[cfg(test)]
mod tests {
    use super::choose;

    #[test]
    fn precedence() {
        assert_eq!(choose(0, None, None).0, "info");
        assert_eq!(choose(0, None, Some("warn")).0, "warn");
        assert_eq!(choose(1, None, Some("warn")).0, "debug");
        assert_eq!(choose(2, None, None).0, "trace");
        assert_eq!(choose(2, Some("error"), Some("warn")).0, "error");
        assert_eq!(choose(0, Some("  "), None).0, "info");
    }
}
