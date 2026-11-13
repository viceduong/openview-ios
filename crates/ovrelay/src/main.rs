//! `ovrelay` binary: the OpenView relay server.
//!
//! ```text
//! ovrelay [--bind HOST:PORT] [--timeout SECS] [--max-connections N]
//!         [--max-per-ip N] [--max-pending N] [--ws-path PATH]... [-v|-vv]
//!         [--log-level FILTER]
//! ovrelay --healthcheck [--bind HOST:PORT]
//! ```
//!
//! `--bind` also reads `OVRELAY_BIND`. `--healthcheck` does not serve: it
//! sends `GET /health` to the relay at `--bind` (an unspecified address such
//! as `0.0.0.0` is probed on loopback) and exits 0 on `200 OK`, 1 otherwise —
//! a container `HEALTHCHECK` for images without `curl` / `wget`.
//!
//! Logging goes to stderr through `tracing` (filter: `--log-level`, else
//! `-v`/`-vv`, else `OPENVIEW_LOG`, else `info`). At `info` the relay prints
//! its listen address and one line per paired / closed session; per-direction
//! byte statistics and WebSocket close diagnostics are `debug`.

#![forbid(unsafe_code)]

use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use ovrelay::{Relay, RelayLimits};

/// The port every client dials by default (`ovhost`, `ovview`, iOS, GUI).
const DEFAULT_BIND: &str = "0.0.0.0:4569";

/// Connect / read / write timeout of `--healthcheck`.
const HEALTHCHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// OpenView relay server: pairs a viewer and a host that announce the same
/// session id and pipes their (end-to-end encrypted) traffic.
#[derive(Debug, Parser)]
#[command(name = "ovrelay", version, about, long_about = None)]
struct Cli {
    /// Listen address (raw TCP and WebSocket on the same port).
    #[arg(long, value_name = "HOST:PORT", env = "OVRELAY_BIND", default_value = DEFAULT_BIND)]
    bind: SocketAddr,

    /// Do not serve: probe `GET /health` on the relay at `--bind` and exit
    /// 0 when it answers 200 (for container health checks).
    #[arg(long)]
    healthcheck: bool,

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
    if cli.healthcheck {
        let target = probe_target(cli.bind);
        return match probe_health(target, HEALTHCHECK_TIMEOUT) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                tracing::error!("health check of {target} failed: {e}");
                ExitCode::FAILURE
            }
        };
    }

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

/// The address `--healthcheck` dials for a listen address: an unspecified
/// IP (`0.0.0.0` / `::`) becomes loopback of the same family.
fn probe_target(bind: SocketAddr) -> SocketAddr {
    let ip = match bind.ip() {
        IpAddr::V4(v4) if v4.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(v6) if v6.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    SocketAddr::new(ip, bind.port())
}

/// `GET /health` on `addr`; `Ok` when the status line is `200`.
fn probe_health(addr: SocketAddr, timeout: Duration) -> io::Result<()> {
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    write!(
        stream,
        "GET /health HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )?;
    // The relay closes after its short plain response; cap the read anyway.
    let mut resp = Vec::new();
    stream.take(4096).read_to_end(&mut resp)?;
    let status = resp.split(|&b| b == b'\r').next().unwrap_or_default();
    let status = String::from_utf8_lossy(status);
    match status.split(' ').nth(1) {
        Some("200") => Ok(()),
        _ => Err(io::Error::other(format!("unexpected answer `{status}`"))),
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

    #[test]
    fn healthcheck_probes_loopback_for_unspecified_binds() {
        let t = probe_target("0.0.0.0:4569".parse().unwrap());
        assert_eq!(t, "127.0.0.1:4569".parse::<SocketAddr>().unwrap());
        let t = probe_target("[::]:7".parse().unwrap());
        assert_eq!(t, "[::1]:7".parse::<SocketAddr>().unwrap());
        let t = probe_target("192.0.2.1:9".parse().unwrap());
        assert_eq!(t, "192.0.2.1:9".parse::<SocketAddr>().unwrap());
        let c = Cli::try_parse_from(["ovrelay", "--healthcheck"]).unwrap();
        assert!(c.healthcheck);
    }

    #[test]
    fn bind_reads_ovrelay_bind() {
        // Inspect the definition instead of setting the variable: the
        // environment is process-wide and other tests parse defaults.
        use clap::CommandFactory;
        let cmd = Cli::command();
        let bind = cmd
            .get_arguments()
            .find(|a| a.get_id() == "bind")
            .expect("--bind exists");
        assert_eq!(
            bind.get_env().and_then(|e| e.to_str()),
            Some("OVRELAY_BIND")
        );
    }

    #[test]
    fn healthcheck_passes_against_a_live_relay_and_fails_without_one() {
        let relay = Relay::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = relay.local_addr();
        std::thread::spawn(move || relay.serve(None));
        probe_health(addr, Duration::from_secs(5)).expect("live relay is healthy");

        // A port nobody listens on (bound, then released).
        let gone = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        assert!(probe_health(gone, Duration::from_secs(2)).is_err());

        // Something else on the port, answering non-200.
        let other = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let other_addr = other.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = other.accept() {
                let mut head = [0u8; 256];
                let _ = s.read(&mut head);
                let _ = s.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
            }
        });
        assert!(probe_health(other_addr, Duration::from_secs(5)).is_err());
    }
}
