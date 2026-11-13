//! `ovrelay` binary: the OpenView relay server.
//!
//! ```text
//! ovrelay [--bind HOST:PORT] [--timeout SECS] [--max-connections N]
//!         [--max-per-ip N] [--max-pending N] [--ws-path PATH]... [-v|-vv]
//!         [--log-level FILTER]
//! ```
//!
//! Logging goes to stderr through `tracing` (filter: `--log-level`, else
//! `-v`/`-vv`, else `OPENVIEW_LOG`, else `info`). At `info` the relay prints
//! its listen address and one line per paired / closed session; per-direction
//! byte statistics and WebSocket close diagnostics are `debug`.

#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use ovrelay::{Relay, RelayLimits};

/// The port every client dials by default (`ovhost`, `ovview`, iOS, GUI).
const DEFAULT_BIND: &str = "0.0.0.0:4569";

/// OpenView relay server: pairs a viewer and a host that announce the same
/// session id and pipes their (end-to-end encrypted) traffic.
#[derive(Debug, Parser)]
#[command(name = "ovrelay", version, about, long_about = None)]
struct Cli {
    /// Listen address (raw TCP and WebSocket on the same port).
    #[arg(long, value_name = "HOST:PORT", default_value = DEFAULT_BIND)]
    bind: SocketAddr,

    /// Seconds an unmatched (parked) session waits for its peer.
    #[arg(long, value_name = "SECS", default_value_t = 30)]
    timeout: u64,

    /// Most simultaneous client connections.
    #[arg(long, value_name = "N", default_value_t = RelayLimits::DEFAULT.max_connections)]
    max_connections: usize,

    /// Most simultaneous connections from one IP address (loopback, i.e. a
    /// local reverse proxy / tunnel, is exempt).
    #[arg(long, value_name = "N", default_value_t = RelayLimits::DEFAULT.max_per_ip)]
    max_per_ip: usize,

    /// Most parked (unpaired) sessions.
    #[arg(long, value_name = "N", default_value_t = RelayLimits::DEFAULT.max_pending)]
    max_pending: usize,

    /// Request path accepted for WebSocket upgrades (repeat for several).
    /// Clients dial `wss://host<PATH>`; other paths get 404.
    #[arg(long = "ws-path", value_name = "PATH", default_value = ovrelay::DEFAULT_WS_PATH)]
    ws_paths: Vec<String>,

    /// More log output on stderr (-v debug, -vv trace).
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Log filter, e.g. `debug` or `info,ovrelay=trace` (overrides -v and
    /// the OPENVIEW_LOG environment variable).
    #[arg(long, value_name = "FILTER")]
    log_level: Option<String>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    ovcommon::logging::init(cli.verbose, cli.log_level.as_deref());

    let mut relay = match Relay::bind(cli.bind) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("{e}");
            return ExitCode::FAILURE;
        }
    };
    relay.set_timeout(Duration::from_secs(cli.timeout));
    relay.set_limits(RelayLimits {
        max_connections: cli.max_connections,
        max_per_ip: cli.max_per_ip,
        max_pending: cli.max_pending,
        ..RelayLimits::DEFAULT
    });
    if let Err(e) = relay.set_ws_paths(cli.ws_paths.clone()) {
        tracing::error!("{e}");
        return ExitCode::FAILURE;
    }
    tracing::info!(
        "listening on {} (park timeout {}s, max {} connections / {} per IP / {} pending)",
        relay.local_addr(),
        cli.timeout,
        cli.max_connections,
        cli.max_per_ip,
        cli.max_pending
    );
    match relay.serve(None) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses_defaults_and_flags() {
        let c = Cli::try_parse_from(["ovrelay"]).unwrap();
        assert_eq!(c.bind, DEFAULT_BIND.parse::<SocketAddr>().unwrap());
        assert_eq!(c.timeout, 30);
        assert_eq!(c.ws_paths, vec!["/relay".to_owned()]);
        let c = Cli::try_parse_from(["ovrelay", "--ws-path", "/a", "--ws-path", "/"]).unwrap();
        assert_eq!(c.ws_paths, vec!["/a".to_owned(), "/".to_owned()]);
        let c = Cli::try_parse_from([
            "ovrelay",
            "--bind",
            "127.0.0.1:4569",
            "--timeout",
            "5",
            "--max-per-ip",
            "2",
            "-vv",
        ])
        .unwrap();
        assert_eq!(c.bind.port(), 4569);
        assert_eq!((c.timeout, c.max_per_ip, c.verbose), (5, 2, 2));
        assert!(Cli::try_parse_from(["ovrelay", "--bind", "nope"]).is_err());
        assert!(Cli::try_parse_from(["ovrelay", "--bogus"]).is_err());
    }

    #[test]
    fn clap_definition_is_consistent() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
