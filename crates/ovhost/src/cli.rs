//! Command-line options (`ovhost --help`).

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;
use ovcommon::relay_addr::RelayAddr;

use crate::{desktop_fx, direct_net};

/// Default relay: the one OpenView runs on this PC.
const DEFAULT_RELAY: &str = "127.0.0.1:4569";

/// OpenView host: makes this computer reachable through an OpenView relay.
///
/// Status events go to stdout as JSON lines; logs go to stderr.
#[derive(Debug, Parser)]
#[command(name = "ovhost", version)]
pub(crate) struct Cli {
    /// Relay address: an IP or hostname with an optional port (raw TCP,
    /// default port 4569), or a `ws://` / `wss://` URL (WebSocket, e.g. a
    /// relay behind a reverse proxy or a Cloudflare tunnel).
    #[arg(long, value_name = "ADDR", default_value = DEFAULT_RELAY)]
    pub(crate) relay: RelayAddr,

    /// Serve sessions forever (back off after errors) instead of exactly one.
    #[arg(long)]
    pub(crate) service: bool,

    /// Unattended access: a stable device ID and PIN authentication instead
    /// of a one-time pairing code (implied by a PIN).
    #[arg(long)]
    pub(crate) unattended: bool,

    /// Access PIN for unattended mode. Prefer the OPENVIEW_PIN environment
    /// variable: other processes can read a command line.
    #[arg(long, env = "OPENVIEW_PIN", hide_env_values = true, value_name = "PIN")]
    pub(crate) pin: Option<String>,

    /// Data directory (stable identity). Default: %LOCALAPPDATA%\OpenView, or
    /// `<exe dir>\data` when portable.txt sits next to the exe.
    #[arg(long, value_name = "DIR")]
    pub(crate) data_dir: Option<PathBuf>,

    /// Stream the screen (optionally only a WxH region of the primary
    /// monitor) instead of a terminal-only session.
    #[arg(long, value_name = "WxH", num_args = 0..=1, default_missing_value = "full",
          value_parser = parse_screen)]
    pub(crate) screen: Option<ScreenArea>,

    /// Base capture rate (frames per second).
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..=240))]
    pub(crate) fps: u32,

    /// Highest rate AUTO quality may raise the capture to when the link
    /// carries it.
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u32).range(1..=240))]
    pub(crate) max_fps: u32,

    /// Stream the microphone to the viewer.
    #[arg(long, conflicts_with_all = ["system_audio", "mic_synth"])]
    pub(crate) mic: bool,

    /// Stream what this PC plays (WASAPI loopback) to the viewer.
    #[arg(long, conflicts_with = "mic_synth")]
    pub(crate) system_audio: bool,

    /// Play the viewer's microphone on this PC's speakers.
    #[arg(long)]
    pub(crate) audio: bool,

    /// Do not sync the clipboard with the viewer.
    #[arg(long)]
    pub(crate) no_clipboard: bool,

    /// Capture with GDI instead of DXGI Desktop Duplication.
    #[arg(long)]
    pub(crate) gdi: bool,

    /// H.264 for high-motion regions (needs Media Foundation).
    #[arg(long, value_enum, default_value_t = H264Mode::Auto)]
    pub(crate) h264: H264Mode,

    /// Relay only: never upgrade to a direct UDP path.
    #[arg(long)]
    pub(crate) no_direct: bool,

    /// UDP port of the direct path (the one inbound port; allow it in the
    /// firewall).
    #[arg(long, value_name = "PORT", default_value_t = direct_net::DEFAULT_DIRECT_PORT)]
    pub(crate) direct_port: u16,

    /// UDP port for viewers on this network (sessions without the relay;
    /// `0` = any free port, announced via mDNS in unattended mode).
    #[arg(long, value_name = "PORT", default_value_t = crate::lan::DEFAULT_LAN_PORT)]
    pub(crate) lan_port: u16,

    /// Do not accept LAN sessions (relay only; implies --no-mdns).
    #[arg(long)]
    pub(crate) no_lan: bool,

    /// Do not announce this host on the local network (mDNS / Bonjour
    /// `_openview._udp`). LAN viewers then need its address.
    #[arg(long)]
    pub(crate) no_mdns: bool,

    /// Do not map the direct port on the router (UPnP IGD).
    #[arg(long)]
    pub(crate) no_upnp: bool,

    /// STUN server used to learn the public address.
    #[arg(long, value_name = "HOST:PORT", default_value = ovdiscovery::stun::DEFAULT_STUN)]
    pub(crate) stun: String,

    /// Skip the STUN query.
    #[arg(long)]
    pub(crate) no_stun: bool,

    /// Keep the wallpaper during screen sessions (hidden by default).
    #[arg(long)]
    pub(crate) keep_wallpaper: bool,

    /// Keep UI animations during screen sessions (off by default).
    #[arg(long)]
    pub(crate) keep_animations: bool,

    /// Flat desktop colour while the wallpaper is hidden.
    #[arg(long, value_name = "RRGGBB", value_parser = parse_color)]
    pub(crate) desktop_color: Option<u32>,

    /// More log output on stderr (-v debug, -vv trace).
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub(crate) verbose: u8,

    /// Log filter, e.g. `debug` or `info,ovtransport=trace` (overrides -v
    /// and the OPENVIEW_LOG environment variable).
    #[arg(long, value_name = "FILTER")]
    pub(crate) log_level: Option<String>,

    // ---- development / test switches (hidden from --help) ----
    /// Accepted for compatibility: one session is the default.
    #[arg(long, hide = true)]
    pub(crate) listen: bool,
    /// Log every SendInput call instead of injecting it.
    #[arg(long, hide = true)]
    pub(crate) dry_input: bool,
    /// Log desktop-effect changes instead of applying them.
    #[arg(long, hide = true)]
    pub(crate) fx_dry_run: bool,
    /// Stream a synthetic 440 Hz tone instead of a real source.
    #[arg(long, hide = true)]
    pub(crate) mic_synth: bool,
    /// Stream a synthetic moving screen.
    #[arg(long, hide = true, value_enum)]
    pub(crate) test_pattern: Option<Pattern>,
    /// Never send lossy JPEG image tiles.
    #[arg(long, hide = true)]
    pub(crate) lossless: bool,
    /// Disable the tile cache.
    #[arg(long, hide = true)]
    pub(crate) no_tile_cache: bool,
    /// JPEG quality of lossy image tiles.
    #[arg(long, hide = true, value_parser = clap::value_parser!(u8).range(1..=100))]
    pub(crate) lossy: Option<u8>,
    /// Offer exactly these direct candidates (network-emulator benches).
    #[arg(long, hide = true, value_delimiter = ',')]
    pub(crate) direct_advertise: Vec<SocketAddr>,
    /// Feed the H.264 MFT system-memory NV12 instead of D3D11 surfaces.
    #[arg(long, hide = true)]
    pub(crate) no_gpu_encode: bool,
    /// H.264 area-based bitrate budget (bits per pixel).
    #[arg(long, hide = true)]
    pub(crate) video_bpp: Option<f64>,
    /// Disable the media channel (A/B benches).
    #[arg(long, hide = true)]
    pub(crate) no_media: bool,
    /// Disable link pacing (A/B benches).
    #[arg(long, hide = true)]
    pub(crate) no_pacing: bool,
    /// Send this file to the viewer right after the handshake.
    #[arg(long, hide = true, value_name = "PATH")]
    pub(crate) send_file: Option<PathBuf>,
}

/// `--screen [WxH]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScreenArea {
    /// The full primary monitor.
    Full,
    /// A fixed region at the primary monitor's origin.
    Region(u32, u32),
}

impl ScreenArea {
    /// The fixed region size, `None` for the full monitor.
    pub(crate) fn region(self) -> Option<(u32, u32)> {
        match self {
            Self::Full => None,
            Self::Region(w, h) => Some((w, h)),
        }
    }
}

fn parse_screen(s: &str) -> Result<ScreenArea, String> {
    if s == "full" {
        return Ok(ScreenArea::Full);
    }
    parse_region(s)
        .map(|(w, h)| ScreenArea::Region(w, h))
        .ok_or_else(|| format!("bad region `{s}` (want WxH, e.g. 640x360)"))
}

/// Parse a `WxH` region spec such as `640x360`.
fn parse_region(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.split_once('x')?;
    let w = w.parse::<u32>().ok()?;
    let h = h.parse::<u32>().ok()?;
    (w > 0 && h > 0).then_some((w, h))
}

fn parse_color(s: &str) -> Result<u32, String> {
    desktop_fx::parse_rgb(s).ok_or_else(|| format!("bad colour `{s}` (want RRGGBB)"))
}

/// `--h264`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum H264Mode {
    /// Hardware encoder first, software fallback.
    Auto,
    /// Hardware encoders only.
    Hw,
    /// Microsoft's software encoder only.
    Sw,
    /// Lossless tiles only.
    Off,
}

/// `--test-pattern`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Pattern {
    Video,
    Scroll,
    Photo,
}
