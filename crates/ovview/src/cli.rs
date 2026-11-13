//! Command-line options (`clap`) and their translation into what to run.
//!
//! The flag set is a contract with `ovgui` (which spawns `ovview --connect
//! ...` with the PIN in `OPENVIEW_PIN`), `scripts/e2e_relay.sh` and the
//! documentation in `docs/USAGE.md`; renaming a flag breaks them.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use ovcommon::relay_addr::RelayAddr;

use crate::mic::MicKind;
use crate::session::SessionOptions;

/// Default relay address (the relay OpenView runs on this PC).
const DEFAULT_RELAY: &str = "127.0.0.1:4569";

/// OpenView viewer: control a remote OpenView host.
///
/// Status events go to stdout as JSON lines; logs go to stderr.
#[derive(Debug, Parser)]
#[command(name = "ovview", version)]
#[command(group(
    clap::ArgGroup::new("mode")
        .required(true)
        .args(["connect", "code", "id", "info", "export", "play"]),
))]
pub(crate) struct Cli {
    /// Unattended access: dial the host with this device ID (needs a PIN).
    #[arg(long, value_name = "DEVICE_ID")]
    connect: Option<u64>,

    /// The host's access PIN for --connect / --id. Prefer the OPENVIEW_PIN
    /// environment variable: other processes can read a command line.
    #[arg(long, env = "OPENVIEW_PIN", hide_env_values = true, value_name = "PIN")]
    pin: Option<String>,

    /// Join an attended host by its one-time pairing code.
    #[arg(long, value_name = "CODE")]
    code: Option<String>,

    /// LAN access without the relay: find the host with this device ID on
    /// the local network (mDNS) and dial it directly (needs a PIN).
    #[arg(long, value_name = "DEVICE_ID")]
    id: Option<String>,

    /// With --id: dial these host LAN addresses (IP:PORT, comma-separated)
    /// instead of looking the host up by mDNS.
    #[arg(long, value_name = "IP:PORT", value_delimiter = ',', requires = "id")]
    lan_addr: Vec<SocketAddr>,

    /// With --id: when the host does not answer on the LAN (not found, or
    /// no address replies), dial it through --relay like --connect.
    #[arg(long, requires = "id")]
    relay_fallback: bool,

    /// Relay address: an IP or hostname with an optional port (raw TCP,
    /// default port 4569), or a `ws://` / `wss://` URL (WebSocket, e.g. a
    /// relay behind a reverse proxy or a Cloudflare tunnel).
    #[arg(long, value_name = "ADDR", default_value = DEFAULT_RELAY)]
    relay: RelayAddr,

    /// Run headless (no window; stdin lines are still sent as text).
    #[arg(long)]
    no_window: bool,

    /// Never accept H.264 for high-motion regions (lossless tiles only).
    #[arg(long)]
    no_h264: bool,

    /// Stay on the relay (no direct UDP upgrade).
    #[arg(long)]
    no_direct: bool,

    /// Presenter: Direct2D high-quality cubic, or GDI + CPU scaler.
    #[arg(long, value_enum, default_value_t = Renderer::D2d)]
    renderer: Renderer,

    /// Stream this PC's microphone to the host.
    #[arg(long)]
    mic: bool,

    /// Start with speaker playback on (F2 toggles).
    #[arg(long)]
    audio: bool,

    /// Do not sync the clipboard with the host.
    #[arg(long)]
    no_clipboard: bool,

    /// Folder for F6 session recordings (default %USERPROFILE%\ovrecord).
    #[arg(long, value_name = "DIR")]
    record_dir: Option<String>,

    /// Print a recording's header summary (offline).
    #[arg(long, value_name = "FILE.ovrec")]
    info: Option<PathBuf>,

    /// Write a recording's frames as PPM images into --out (offline).
    #[arg(long, value_name = "FILE.ovrec", requires = "out")]
    export: Option<PathBuf>,

    /// Output folder of --export.
    #[arg(long, value_name = "DIR")]
    out: Option<PathBuf>,

    /// With --export: only this frame sequence number.
    #[arg(long, value_name = "N")]
    frame: Option<u32>,

    /// Play a recording back in the window (offline).
    #[arg(long, value_name = "FILE.ovrec")]
    play: Option<PathBuf>,

    /// With --play: loop forever.
    #[arg(long = "loop")]
    loop_play: bool,

    /// More log output on stderr (-v debug, -vv trace).
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub(crate) verbose: u8,

    /// Log filter, e.g. `debug` or `info,ovtransport=trace` (overrides -v
    /// and the OPENVIEW_LOG environment variable).
    #[arg(long, value_name = "FILTER")]
    pub(crate) log_level: Option<String>,

    // ---- development / test switches (hidden from --help) ----
    /// Stream a synthetic 440 Hz tone instead of the microphone.
    #[arg(long, hide = true, conflicts_with = "mic")]
    audio_synth: bool,
    /// Log every keepalive RTT sample with its path.
    #[arg(long, hide = true)]
    rtt_log: bool,
    /// Keepalive PING interval in ms (RTT measurements).
    #[arg(long, hide = true, value_parser = clap::value_parser!(u64).range(10..))]
    ping_ms: Option<u64>,
    /// End the session cleanly after this many seconds (headless runs).
    #[arg(long, hide = true, value_parser = clap::value_parser!(u64).range(1..))]
    duration: Option<u64>,
    /// Send this file to the host right after the handshake.
    #[arg(long, hide = true, value_name = "PATH")]
    send_file: Option<PathBuf>,
}

/// `--renderer`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Renderer {
    D2d,
    Gdi,
}

/// Who to dial, and how to authenticate.
pub(crate) enum Target {
    /// Pairing code: join through the relay.
    Relay(String),
    /// LAN access by device id + access PIN: mDNS lookup (or `--lan-addr`)
    /// and a direct UDP session, no relay.
    Lan {
        device_id: u64,
        pin: String,
        addrs: Vec<SocketAddr>,
        /// Dial through the relay when the LAN attempt finds nobody.
        relay_fallback: bool,
    },
    /// Unattended access: dial by the host's device id + access PIN.
    Unattended { device_id: u64, pin: String },
}

/// What one `ovview` invocation does.
pub(crate) enum Mode {
    /// `--info <file>`: print a recording's header summary.
    Info(PathBuf),
    /// `--export <file> --out <dir> [--frame N]`: write frames as PPM.
    Export {
        file: PathBuf,
        out: PathBuf,
        frame: Option<u32>,
    },
    /// `--play <file> [--loop]`: play a recording in the window.
    Play { file: PathBuf, loop_forever: bool },
    /// A live session against a host.
    Connect {
        target: Target,
        relay: RelayAddr,
        options: SessionOptions,
    },
}

impl Cli {
    /// Resolve the flags into a [`Mode`]. The offline tools win over the
    /// connect flags; a connect mode needs exactly one target (and a PIN for
    /// `--connect`).
    pub(crate) fn into_mode(self) -> Result<Mode, String> {
        if let Some(file) = self.info {
            return Ok(Mode::Info(file));
        }
        if let Some(file) = self.export {
            let out = self.out.ok_or("--export needs --out <dir>")?;
            return Ok(Mode::Export {
                file,
                out,
                frame: self.frame,
            });
        }
        if let Some(file) = self.play {
            return Ok(Mode::Play {
                file,
                loop_forever: self.loop_play,
            });
        }

        let mic = if self.mic {
            Some(MicKind::Real)
        } else if self.audio_synth {
            Some(MicKind::Synth)
        } else {
            None
        };
        let target = match (self.code, self.id, self.connect) {
            (Some(c), None, None) => Target::Relay(c),
            (None, Some(i), None) => Target::Lan {
                device_id: parse_device_id(&i)?,
                pin: self
                    .pin
                    .filter(|p| !p.is_empty())
                    .ok_or("--id needs the host's PIN (OPENVIEW_PIN or --pin)")?,
                addrs: self.lan_addr,
                relay_fallback: self.relay_fallback,
            },
            (None, None, Some(dev)) => Target::Unattended {
                device_id: dev,
                pin: self
                    .pin
                    .filter(|p| !p.is_empty())
                    .ok_or("--connect needs the host's PIN (OPENVIEW_PIN or --pin)")?,
            },
            _ => return Err("pass exactly one of --connect, --code or --id".into()),
        };
        let options = SessionOptions {
            record_dir: self.record_dir,
            no_window: self.no_window,
            mic,
            clipboard: !self.no_clipboard,
            send_file: self.send_file,
            direct: !self.no_direct,
            h264: !self.no_h264,
            audio_at_start: self.audio,
            rtt_log: self.rtt_log,
            ping_every: self.ping_ms.map(Duration::from_millis),
            run_for: self.duration.map(Duration::from_secs),
            renderer: match self.renderer {
                Renderer::D2d => crate::window::RendererPref::Direct2D,
                Renderer::Gdi => crate::window::RendererPref::Gdi,
            },
        };
        Ok(Mode::Connect {
            target,
            relay: self.relay,
            options,
        })
    }
}

/// A device ID as typed or shown ("123 456 789 012"): digits, with spaces
/// or dashes allowed between groups.
pub(crate) fn parse_device_id(text: &str) -> Result<u64, String> {
    let digits: String = text
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("--id wants a device ID (digits), got `{text}`"));
    }
    digits
        .parse()
        .map_err(|_| format!("--id: device ID `{text}` is too long"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_ids_accept_grouping() {
        assert_eq!(parse_device_id("123 456 789"), Ok(123_456_789));
        assert_eq!(parse_device_id("123-456"), Ok(123_456));
        assert!(parse_device_id("").is_err());
        assert!(parse_device_id("12ab").is_err());
        assert!(parse_device_id("99999999999999999999999").is_err());
    }

    #[test]
    fn id_needs_a_pin_and_takes_lan_addresses() {
        let cli = Cli::try_parse_from([
            "ovview",
            "--id",
            "42",
            "--pin",
            "1234",
            "--lan-addr",
            "192.168.1.5:4571,10.0.0.2:4571",
        ])
        .unwrap();
        match cli.into_mode().unwrap() {
            Mode::Connect {
                target:
                    Target::Lan {
                        device_id,
                        pin,
                        addrs,
                        relay_fallback,
                    },
                ..
            } => {
                assert!(!relay_fallback);
                assert_eq!(device_id, 42);
                assert_eq!(pin, "1234");
                assert_eq!(addrs.len(), 2);
            }
            _ => panic!("expected a LAN target"),
        }
        assert!(Cli::try_parse_from(["ovview", "--lan-addr", "1.2.3.4:5"]).is_err());
    }
}
