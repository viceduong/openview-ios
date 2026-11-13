//! `ovhost` — the OpenView host: makes this computer reachable through an
//! OpenView relay (and upgrades to a direct UDP path when the network
//! allows), then streams the screen and injects the viewer's input.
//!
//! - `ovhost --unattended --service --screen` (PIN in `OPENVIEW_PIN`): what
//!   the GUI runs — a stable device ID + PIN authentication, serving
//!   sessions forever.
//! - `ovhost --screen`: attended mode — prints a one-time pairing code and
//!   serves exactly one session.
//!
//! Run `ovhost --help` for every option. Output contract:
//!
//! - **stdout** carries machine-readable status events, one JSON object per
//!   line ([`ovcommon::status`]); the GUI parses these.
//! - **stderr** carries human-readable logs (`tracing`): lifecycle lines at
//!   the default `info` level, periodic statistics with `-v`.
//!
//! The relay session id is the device ID's low 32 bits (unattended) or the
//! pairing code's first 4 bytes (matches what `ovview` dials).
//!
//! Network: the relay connection is outbound TCP (`--relay`); the direct
//! path binds UDP `0.0.0.0:<--direct-port>` (+ `[::]` when available) —
//! the only inbound port the host needs (UPnP maps it on the router when
//! allowed).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::Parser;
use ovapp::adapt::{AutoQuality, RateGovernor, RateStats};
use ovapp::gdisrc::GdiScreen;
use ovapp::liveness::{Keepalive, Liveness, DEAD_AFTER, KEEPALIVE_INTERVAL, SESSION_TIMEOUT};
use ovapp::monitors::{enumerate_monitors, validate_region, Monitor};
use ovapp::sendq::{SubmitOutcome, UpdateEncoder};
use ovapp::terminal::{Command, TerminalSession};
use ovcommon::status::{self, DirectState, StatusEvent};
use ovcore::handshake::{Auth, Handshake, HandshakeEvent, Role};
use ovcore::identity::Identity;
use ovtransport::{Carrier, RelayTunnelCarrier, TunnelRole};
use tracing::{debug, error, info, trace, warn};

mod capture;
mod cleanup;
mod clipboard_os;
mod cursor;
mod desktop_fx;
mod direct_net;
#[cfg(windows)]
mod dxgi;
mod input;
mod mic;
mod speaker;
#[cfg(windows)]
mod sysaudio;

use input::{Region, VirtualScreen};

/// Default relay: the one OpenView runs on this PC.
const DEFAULT_RELAY: &str = "127.0.0.1:4569";

/// OpenView host: makes this computer reachable through an OpenView relay.
///
/// Status events go to stdout as JSON lines; logs go to stderr.
#[derive(Debug, Parser)]
#[command(name = "ovhost", version)]
struct Cli {
    /// Relay address.
    #[arg(long, value_name = "HOST:PORT", default_value = DEFAULT_RELAY)]
    relay: String,

    /// Serve sessions forever (back off after errors) instead of exactly one.
    #[arg(long)]
    service: bool,

    /// Unattended access: a stable device ID and PIN authentication instead
    /// of a one-time pairing code (implied by a PIN).
    #[arg(long)]
    unattended: bool,

    /// Access PIN for unattended mode. Prefer the OPENVIEW_PIN environment
    /// variable: other processes can read a command line.
    #[arg(long, env = "OPENVIEW_PIN", hide_env_values = true, value_name = "PIN")]
    pin: Option<String>,

    /// Data directory (stable identity). Default: %LOCALAPPDATA%\OpenView, or
    /// <exe dir>\data when portable.txt sits next to the exe.
    #[arg(long, value_name = "DIR")]
    data_dir: Option<PathBuf>,

    /// Stream the screen (optionally only a WxH region of the primary
    /// monitor) instead of a terminal-only session.
    #[arg(long, value_name = "WxH", num_args = 0..=1, default_missing_value = "full",
          value_parser = parse_screen)]
    screen: Option<ScreenArea>,

    /// Base capture rate (frames per second).
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..=240))]
    fps: u32,

    /// Highest rate AUTO quality may raise the capture to when the link
    /// carries it.
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u32).range(1..=240))]
    max_fps: u32,

    /// Stream the microphone to the viewer.
    #[arg(long, conflicts_with_all = ["system_audio", "mic_synth"])]
    mic: bool,

    /// Stream what this PC plays (WASAPI loopback) to the viewer.
    #[arg(long, conflicts_with = "mic_synth")]
    system_audio: bool,

    /// Play the viewer's microphone on this PC's speakers.
    #[arg(long)]
    audio: bool,

    /// Do not sync the clipboard with the viewer.
    #[arg(long)]
    no_clipboard: bool,

    /// Capture with GDI instead of DXGI Desktop Duplication.
    #[arg(long)]
    gdi: bool,

    /// H.264 for high-motion regions (needs Media Foundation).
    #[arg(long, value_enum, default_value_t = H264Mode::Auto)]
    h264: H264Mode,

    /// Relay only: never upgrade to a direct UDP path.
    #[arg(long)]
    no_direct: bool,

    /// UDP port of the direct path (the one inbound port; allow it in the
    /// firewall).
    #[arg(long, value_name = "PORT", default_value_t = direct_net::DEFAULT_DIRECT_PORT)]
    direct_port: u16,

    /// Do not map the direct port on the router (UPnP IGD).
    #[arg(long)]
    no_upnp: bool,

    /// STUN server used to learn the public address.
    #[arg(long, value_name = "HOST:PORT", default_value = ovdiscovery::stun::DEFAULT_STUN)]
    stun: String,

    /// Skip the STUN query.
    #[arg(long)]
    no_stun: bool,

    /// Keep the wallpaper during screen sessions (hidden by default).
    #[arg(long)]
    keep_wallpaper: bool,

    /// Keep UI animations during screen sessions (off by default).
    #[arg(long)]
    keep_animations: bool,

    /// Flat desktop colour while the wallpaper is hidden.
    #[arg(long, value_name = "RRGGBB", value_parser = parse_color)]
    desktop_color: Option<u32>,

    /// More log output on stderr (-v debug, -vv trace).
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Log filter, e.g. `debug` or `info,ovtransport=trace` (overrides -v
    /// and the OPENVIEW_LOG environment variable).
    #[arg(long, value_name = "FILTER")]
    log_level: Option<String>,

    // ---- development / test switches (hidden from --help) ----
    /// Accepted for compatibility: one session is the default.
    #[arg(long, hide = true)]
    listen: bool,
    /// Log every SendInput call instead of injecting it.
    #[arg(long, hide = true)]
    dry_input: bool,
    /// Log desktop-effect changes instead of applying them.
    #[arg(long, hide = true)]
    fx_dry_run: bool,
    /// Stream a synthetic 440 Hz tone instead of a real source.
    #[arg(long, hide = true)]
    mic_synth: bool,
    /// Stream a synthetic moving screen.
    #[arg(long, hide = true, value_enum)]
    test_pattern: Option<Pattern>,
    /// Never send lossy JPEG image tiles.
    #[arg(long, hide = true)]
    lossless: bool,
    /// Disable the tile cache.
    #[arg(long, hide = true)]
    no_tile_cache: bool,
    /// JPEG quality of lossy image tiles.
    #[arg(long, hide = true, value_parser = clap::value_parser!(u8).range(1..=100))]
    lossy: Option<u8>,
    /// Offer exactly these direct candidates (network-emulator benches).
    #[arg(long, hide = true, value_delimiter = ',')]
    direct_advertise: Vec<SocketAddr>,
    /// Feed the H.264 MFT system-memory NV12 instead of D3D11 surfaces.
    #[arg(long, hide = true)]
    no_gpu_encode: bool,
    /// H.264 area-based bitrate budget (bits per pixel).
    #[arg(long, hide = true)]
    video_bpp: Option<f64>,
    /// Disable the media channel (A/B benches).
    #[arg(long, hide = true)]
    no_media: bool,
    /// Disable link pacing (A/B benches).
    #[arg(long, hide = true)]
    no_pacing: bool,
    /// Send this file to the viewer right after the handshake.
    #[arg(long, hide = true, value_name = "PATH")]
    send_file: Option<PathBuf>,
}

/// `--screen [WxH]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScreenArea {
    /// The full primary monitor.
    Full,
    /// A fixed region at the primary monitor's origin.
    Region(u32, u32),
}

impl ScreenArea {
    fn region(self) -> Option<(u32, u32)> {
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

fn parse_color(s: &str) -> Result<u32, String> {
    desktop_fx::parse_rgb(s).ok_or_else(|| format!("bad colour `{s}` (want RRGGBB)"))
}

/// `--h264`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum H264Mode {
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
enum Pattern {
    Video,
    Scroll,
    Photo,
}

/// Which microphone source was selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MicKind {
    /// The real microphone (WinMM `waveIn`, default input device).
    Real,
    /// A 440 Hz synthetic sine — hardware-free demos and tests.
    Synth,
    /// What the host's speakers play (WASAPI loopback of the default render
    /// device, 10 ms ticks).
    System,
}

/// `--dry-input`: log every `SendInput` call instead of performing it.
static DRY_INPUT: AtomicBool = AtomicBool::new(false);

/// `--gdi`: skip DXGI Desktop Duplication.
static FORCE_GDI: AtomicBool = AtomicBool::new(false);

/// `--h264`: which H.264 encoder the motion path may use (0 auto = hardware
/// first, 1 off, 2 software, 3 hardware only).
static H264_MODE: AtomicU8 = AtomicU8::new(0);
const H264_OFF: u8 = 1;
const H264_SW: u8 = 2;
const H264_HW: u8 = 3;

impl H264Mode {
    fn code(self) -> u8 {
        match self {
            Self::Auto => 0,
            Self::Off => H264_OFF,
            Self::Sw => H264_SW,
            Self::Hw => H264_HW,
        }
    }
}

/// `--test-pattern`: 0 = the real screen.
static TEST_PATTERN: AtomicU8 = AtomicU8::new(0);

/// `--lossless`: never send lossy JPEG image tiles (even to a CAP_LOSSY
/// viewer).
static NO_LOSSY: AtomicBool = AtomicBool::new(false);
/// `--no-tile-cache`: no CACHE REF / STORE records.
static NO_TILE_CACHE: AtomicBool = AtomicBool::new(false);
/// `--lossy Q`: JPEG quality of lossy image tiles.
static LOSSY_QUALITY: AtomicU8 = AtomicU8::new(ovapp::lossy::DEFAULT_QUALITY);
/// `--video-bpp`: H.264 bitrate budget override (bench knob; unset = the
/// codec default).
static VIDEO_BPP: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
/// `--no-media`: no media channel / v2 congestion control (A/B benches).
static NO_MEDIA: AtomicBool = AtomicBool::new(false);
/// `--no-pacing`: the unpaced sender (A/B benches).
static NO_PACING: AtomicBool = AtomicBool::new(false);

/// The direct-path socket + candidates (`None` = `--no-direct`).
static DIRECT: std::sync::OnceLock<direct_net::DirectNet> = std::sync::OnceLock::new();

/// Desktop effects configuration for screen sessions.
#[derive(Debug, Clone, Copy)]
struct FxCfg {
    hide_wallpaper: bool,
    no_animations: bool,
    flat_color: Option<u32>,
    dry_run: bool,
}

static FX_CFG: std::sync::OnceLock<FxCfg> = std::sync::OnceLock::new();

/// The desktop-effects backend: the real Win32 one, or the logging dry run.
fn fx_backend(dry_run: bool) -> Box<dyn desktop_fx::FxBackend> {
    #[cfg(windows)]
    if !dry_run {
        return Box::new(desktop_fx::Win32Fx);
    }
    Box::new(desktop_fx::DryRunFx {
        verbose: dry_run,
        ..Default::default()
    })
}

/// One session's desktop effects: applied at start, restored on drop (and
/// at process exit through the cleanup registry if the drop never runs).
struct SessionFx {
    fx: Arc<Mutex<desktop_fx::DesktopFx>>,
    token: u64,
}

impl SessionFx {
    fn start(cfg: FxCfg) -> Self {
        let mut fx = desktop_fx::DesktopFx::new(
            fx_backend(cfg.dry_run),
            desktop_fx::default_marker(),
            cfg.flat_color,
        );
        if cfg.hide_wallpaper {
            match fx.set_wallpaper_hidden(true) {
                Ok(()) => debug!("wallpaper hidden for the session"),
                Err(e) => warn!("could not hide wallpaper ({e})"),
            }
        }
        if cfg.no_animations {
            match fx.set_animations_off(true) {
                Ok(()) => debug!("UI animations off for the session"),
                Err(e) => warn!("could not disable animations ({e})"),
            }
        }
        let fx = Arc::new(Mutex::new(fx));
        let at_exit = Arc::clone(&fx);
        let token = cleanup::register(move || {
            if let Ok(mut f) = at_exit.lock() {
                f.restore_all();
            }
        });
        Self { fx, token }
    }
}

impl Drop for SessionFx {
    fn drop(&mut self) {
        cleanup::unregister(self.token);
        if let Ok(mut f) = self.fx.lock() {
            let had = f.wallpaper_hidden() || f.animations_off();
            f.restore_all();
            if had {
                debug!("desktop effects restored");
            }
        }
    }
}

/// Capture rate while a region streams as H.264 (AUTO quality only; a
/// manual QUALITY_SET wins).
const VIDEO_FPS: u32 = 30;

fn main() -> ExitCode {
    let cli = Cli::parse();
    ovcommon::logging::init(cli.verbose, cli.log_level.as_deref());
    let rc = match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e}");
            status::emit(&StatusEvent::Error { message: e });
            ExitCode::FAILURE
        }
    };
    // Delete the UPnP mapping (and anything else registered).
    cleanup::run_all();
    rc
}

/// Everything a session needs from the command line.
struct SessionOpts {
    relay: String,
    /// `None` = terminal-only session; `Some(None)` = full primary monitor.
    screen: Option<Option<(u32, u32)>>,
    fps: u32,
    mic: Option<MicKind>,
    audio_playback: bool,
    clipboard: bool,
    /// `Some` = unattended mode with this PIN.
    pin: Option<String>,
    data_dir: PathBuf,
    send_file: Option<PathBuf>,
}

fn run(cli: Cli) -> Result<(), String> {
    info!("OpenView host {} starting", env!("CARGO_PKG_VERSION"));
    DRY_INPUT.store(cli.dry_input, Ordering::Relaxed);
    FORCE_GDI.store(cli.gdi, Ordering::Relaxed);
    H264_MODE.store(cli.h264.code(), Ordering::Relaxed);
    #[cfg(windows)]
    if cli.h264 != H264Mode::Off && !ovapp::mfcodec::available() {
        info!(
            "{}; H.264 off, streaming lossless tiles",
            ovapp::mfcodec::UNAVAILABLE
        );
        H264_MODE.store(H264_OFF, Ordering::Relaxed);
    }
    #[cfg(windows)]
    dxgi::GPU_ENCODE.store(!cli.no_gpu_encode, Ordering::Relaxed);
    TEST_PATTERN.store(
        match cli.test_pattern {
            None => 0,
            Some(Pattern::Video) => 1,
            Some(Pattern::Scroll) => 2,
            Some(Pattern::Photo) => 3,
        },
        Ordering::Relaxed,
    );
    NO_LOSSY.store(cli.lossless, Ordering::Relaxed);
    NO_TILE_CACHE.store(cli.no_tile_cache, Ordering::Relaxed);
    if let Some(q) = cli.lossy {
        LOSSY_QUALITY.store(q, Ordering::Relaxed);
    }
    MAX_AUTO_FPS.store(cli.max_fps, Ordering::Relaxed);
    if let Some(bpp) = cli.video_bpp {
        let _ = VIDEO_BPP.set(bpp);
    }
    NO_MEDIA.store(cli.no_media, Ordering::Relaxed);
    NO_PACING.store(cli.no_pacing, Ordering::Relaxed);
    // Wave 3: 1 ms waits for the direct path's packet pacer.
    ovapp::netwait::high_res_timers();

    let pin = cli.pin.filter(|p| !p.is_empty());
    if cli.unattended && pin.is_none() {
        return Err("--unattended needs a PIN (OPENVIEW_PIN or --pin)".into());
    }
    let data_dir = ovcommon::paths::ensure_data_dir(cli.data_dir.as_deref())
        .map_err(|e| format!("cannot create the data directory: {e}"))?;
    let opts = SessionOpts {
        relay: cli.relay,
        screen: cli.screen.map(ScreenArea::region),
        fps: cli.fps,
        mic: if cli.system_audio {
            Some(MicKind::System)
        } else if cli.mic {
            Some(MicKind::Real)
        } else if cli.mic_synth {
            Some(MicKind::Synth)
        } else {
            None
        },
        audio_playback: cli.audio,
        clipboard: !cli.no_clipboard,
        pin,
        data_dir,
        send_file: cli.send_file,
    };
    let fx = FxCfg {
        hide_wallpaper: !cli.keep_wallpaper,
        no_animations: !cli.keep_animations,
        flat_color: cli.desktop_color,
        dry_run: cli.fx_dry_run,
    };

    // Restore-on-exit (Ctrl-C, console close, panic) for desktop effects
    // and the UPnP mapping; then undo a crashed run's leftover effects.
    cleanup::install();
    desktop_fx::set_marker_dir(&opts.data_dir);
    for marker in std::iter::once(desktop_fx::default_marker()).chain(desktop_fx::legacy_marker()) {
        let mut be = fx_backend(fx.dry_run);
        if desktop_fx::DesktopFx::recover_stale(&mut *be, &marker) {
            info!("restored desktop effects left over by a previous run");
        }
    }
    let _ = FX_CFG.set(fx);
    if cli.no_direct {
        status::emit(&StatusEvent::DirectPath {
            state: DirectState::Disabled,
            port: None,
            addr: None,
            detail: Some("--no-direct".into()),
        });
    } else {
        let d = direct_net::DirectOpts {
            port: cli.direct_port,
            upnp: !cli.no_upnp,
            stun: (!cli.no_stun).then_some(cli.stun),
            advertise: (!cli.direct_advertise.is_empty()).then_some(cli.direct_advertise),
        };
        match direct_net::DirectNet::start(&d) {
            Ok(n) => {
                let _ = DIRECT.set(n);
            }
            Err(e) => {
                warn!("direct path disabled (UDP {}: {e})", d.port);
                status::emit(&StatusEvent::DirectPath {
                    state: DirectState::Disabled,
                    port: Some(d.port),
                    addr: None,
                    detail: Some(e.to_string()),
                });
            }
        }
    }

    if !cli.service {
        return listen_once(&opts);
    }
    // Serve forever; back off after errors so a wedged relay or bad peer
    // cannot spin the CPU. An idle relay park (`XT`, nobody dialed within
    // the relay's window) is not an error: `listen_once` re-parks
    // internally without touching the backoff.
    let mut backoff = Duration::from_secs(1);
    loop {
        match listen_once(&opts) {
            Ok(()) => backoff = Duration::from_secs(1),
            Err(e) => {
                warn!("session failed: {e}; retrying in {backoff:?}");
                status::emit(&StatusEvent::SessionFailed {
                    message: e,
                    retry_in_secs: backoff.as_secs(),
                });
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

/// Serve exactly one inbound session (blocks until the peer disconnects).
fn listen_once(opts: &SessionOpts) -> Result<(), String> {
    let SessionOpts {
        ref relay,
        screen,
        fps,
        mic,
        audio_playback,
        clipboard,
        ..
    } = *opts;
    // Unattended mode: a STABLE identity persisted in the data dir (the
    // device id must not change between runs — viewers dial it by ID), and
    // PIN auth instead of the pairing code.
    let (me, auth, session_id) = if let Some(pin) = opts.pin.as_deref() {
        let me = load_or_create_unattended_identity(&opts.data_dir)?;
        let psk = ovcore::pin::pin_psk(pin.as_bytes());
        let my_dev = unattended_device_id(&me);
        let session_id = (my_dev & 0xFFFF_FFFF) as u32;
        info!("device id {my_dev} (unattended)");
        status::emit(&StatusEvent::DeviceId {
            id: my_dev.to_string(),
        });
        (me, Auth::Pin(psk), session_id)
    } else {
        let me = Identity::generate();
        let code = me.pairing_code();
        info!("fingerprint  {}", me.fingerprint_pine());
        info!("pairing code {code}");
        status::emit(&StatusEvent::PairingCode {
            code: code.to_string(),
            fingerprint: hex::encode(me.fingerprint()),
        });
        let session_id = u32::from_be_bytes(
            code.as_bytes()[..4]
                .try_into()
                .map_err(|_| "pairing code shorter than 4 bytes".to_owned())?,
        );
        (me, Auth::PairingCode(code), session_id)
    };
    let fp = me.fingerprint();
    let addr: SocketAddr = relay
        .parse()
        .map_err(|e| format!("bad relay `{relay}`: {e}"))?;
    info!("waiting on relay {addr} (session {session_id:08x})");
    status::emit(&StatusEvent::Waiting {
        relay: addr.to_string(),
    });
    // Hold the relay slot until a viewer attaches: the relay times an
    // unclaimed park out after its ~30 s window, and that is normal idle —
    // re-park immediately with the same identity/code (no backoff), so an
    // attended host stays reachable for as long as it runs and a dialing
    // viewer never falls into a gap.
    let mut carrier = park_until_attached(addr, session_id)?;

    let my_dev = u64::from_be_bytes([0, 0, 0, fp[0], fp[1], fp[2], fp[3], fp[4]]);
    let mut hs = Handshake::new(Role::Server, my_dev, &me, auth);
    // The viewer starts its handshake the moment the relay attaches it, so
    // the handshake timer only starts now.
    let backlog = handshake_over(&mut hs, &mut carrier, HANDSHAKE_AFTER_ATTACH)?;
    let peer_fp = hs
        .peer()
        .ok_or("handshake established without peer info")?
        .fingerprint;
    info!("peer connected, fingerprint {}", hex::encode(peer_fp));
    status::emit(&StatusEvent::PeerConnected {
        fingerprint: hex::encode(peer_fp),
    });

    let session = hs.take_session().ok_or("no session after handshake")?;
    let mut term = TerminalSession::new(session, carrier);
    // Datagrams polled in the batch that completed the handshake.
    term.queue_inbound(backlog);
    // Section 5: offer a direct UDP path (the relay stays the fallback).
    if let Some((sockets, cands)) = DIRECT.get().and_then(|d| d.for_session()) {
        term.enable_direct(sockets, cands);
    }
    // v0.24: push `--send-file` to the peer right after the handshake.
    if let Some(path) = opts.send_file.as_deref() {
        let data =
            std::fs::read(path).map_err(|e| format!("--send-file {}: {e}", path.display()))?;
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_owned();
        info!("sending {name} ({} bytes)", data.len());
        term.send_file_xfer(&name, &data, 0)
            .map_err(|e| format!("send {name}: {e}"))?;
    }
    let result = match screen {
        // Terminal mode; with `--audio` (v0.23 full duplex) a playback bus
        // for the viewer's mic rides along.
        None => serve_terminal(term, clipboard, audio_playback, &opts.data_dir),
        Some(region) => {
            // Section 4: hide the wallpaper / stop animations while the
            // viewer watches; restored when `_fx` drops.
            let cfg = FX_CFG.get().copied().unwrap_or(FxCfg {
                hide_wallpaper: false,
                no_animations: false,
                flat_color: None,
                dry_run: true,
            });
            let fx = SessionFx::start(cfg);
            term.set_local_caps(term.local_caps() | ovapp::terminal::CAP_DESKTOP_FX);
            let r = serve_screen(
                term,
                region,
                fps,
                mic,
                audio_playback,
                clipboard,
                Some(Arc::clone(&fx.fx)),
                &opts.data_dir,
            );
            drop(fx);
            r
        }
    };
    if let Err(e) = &result {
        info!("session ended: {e}");
        status::emit(&StatusEvent::PeerDisconnected { reason: e.clone() });
    }
    result
}

/// How long the relay may take to answer a park before we re-park (its own
/// unclaimed-park timeout is 30 s; this only bounds a relay that went
/// silent).
const PARK_WAIT: Duration = Duration::from_secs(35);
/// Handshake budget once the relay attached a viewer.
const HANDSHAKE_AFTER_ATTACH: Duration = Duration::from_secs(10);
/// A park that ends sooner than this was refused rather than timed out
/// (e.g. the relay still holds our previous slot); pause before retrying so
/// a refusing relay is not hammered.
const MIN_PARK: Duration = Duration::from_secs(2);

/// Park on the relay as the RESPONDER until a viewer attaches. A park that
/// times out (`XT`, the relay's normal "nobody came" verdict) re-parks
/// immediately with the same session id; only a failure to reach the relay
/// at all is an error (the service loop backs off on those).
fn park_until_attached(addr: SocketAddr, session_id: u32) -> Result<RelayTunnelCarrier, String> {
    let mut announced = false;
    loop {
        let parked_at = Instant::now();
        let mut carrier = RelayTunnelCarrier::connect(addr, session_id, TunnelRole::Responder)
            .map_err(|e| format!("relay connect failed: {e}"))?;
        if !std::mem::replace(&mut announced, true) {
            info!("ready: parked on relay {addr}");
            status::emit(&StatusEvent::Ready {
                relay: addr.to_string(),
            });
        }
        match carrier.wait_attached(PARK_WAIT) {
            Ok(true) => return Ok(carrier),
            // Relay silent for the whole window, or it timed the park out:
            // both are idle, not failure.
            Ok(false) | Err(ovtransport::TransportError::Closed) => {
                let _ = carrier.close();
                trace!("relay park expired; re-parking");
                if parked_at.elapsed() < MIN_PARK {
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
            Err(e) => return Err(format!("relay park failed: {e}")),
        }
    }
}

/// Pump the server-side handshake to completion. Returns any datagrams that
/// arrived after establishment within the same poll batch (they are session
/// records — feeding them to the established handshake would drop them).
fn handshake_over(
    hs: &mut Handshake,
    carrier: &mut dyn Carrier,
    timeout: Duration,
) -> Result<Vec<Vec<u8>>, String> {
    let iterations = timeout.as_millis().div_ceil(10) as usize;
    let mut backlog = Vec::new();
    for _ in 0..iterations {
        if hs.is_established() {
            return Ok(backlog);
        }
        if let Some(e) = hs.failed() {
            return Err(format!("handshake failed: {e}"));
        }
        carrier
            .poll_recv(&mut |dgram| {
                if hs.is_established() {
                    backlog.push(dgram.to_vec());
                } else {
                    let _ = hs.feed(dgram);
                }
            })
            .map_err(|e| format!("handshake recv: {e}"))?;
        for ev in hs.take_events() {
            let HandshakeEvent::Sent(d) = ev;
            let _ = carrier.send(&d);
        }
        if hs.is_established() {
            return Ok(backlog);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Err("handshake timed out".into())
}

/// Shared command handling for both serve loops: chat lines (key==0,
/// mods==0) are logged with their content and queued for echo back to the
/// peer; raw keystrokes print to stderr; PING -> PONG (auto); file transfers
/// reported on completion. Echoing chat lines makes a full
/// client->host->client round trip observable end-to-end (the viewer prints
/// `[peer] <line>`), and the `chat:` log line gives a deterministic,
/// content-bearing signal for the e2e test to assert on.
///
/// Also carries the v0.16 liveness state: every inbound record (of any kind)
/// refreshes [`Liveness`], so an actively streaming session stays alive even
/// when PONGs are dropped, and the auto-replied PING/PONG exchange gives the
/// peer an RTT measurement.
struct HostHooks {
    /// Chat payloads received this cycle, awaiting the echo send.
    echo: Arc<Mutex<Vec<Vec<u8>>>>,
    /// Screen-mode input injection (v0.18, input v2): the controller owns
    /// the captured region + virtual screen mapping (shared with the capture
    /// loop so a `CAPTURE_REGION` retarget also re-maps pointer input) and
    /// the held key/button bookkeeping. `None` in terminal mode (log-only).
    input: Option<SharedInput>,
    /// Inbound-record liveness tracker (v0.16): any decoded record refreshes
    /// it, so the host notices a dead peer on its side too.
    liveness: Arc<Mutex<Liveness>>,
    /// Quality state (v0.17): the viewer's requested capture rate. `0` =
    /// AUTO (the RTT-aware governor decides); anything else is a fixed
    /// base fps the capture loop honours. Written by the `QUALITY_SET`
    /// handler, read every capture tick.
    target_fps: Arc<AtomicU16>,
    /// PONG echoes received this cycle (v0.17): drained by the capture loop
    /// and fed into its [`Keepalive`] to measure RTT, which then lands in
    /// `rtt_us` for the auto-quality rule.
    pong_seq: Arc<Mutex<Vec<u64>>>,
    /// The host's own smoothed RTT in microseconds (v0.17), measured from
    /// its keepalive PING/PONG exchanges. `0` = no measurement yet.
    rtt_us: Arc<AtomicU64>,
    /// Capture-target request (v0.18): the rect the viewer asked the host to
    /// capture (`CAPTURE_REGION`), in **virtual-desktop** coordinates.
    /// Written by the command handler; the capture loop consumes it on its
    /// next tick and re-targets the GDI source there — off the command
    /// thread, so the handler never holds the screen lock across a BitBlt.
    pending_region: PendingRegion,
    /// Speaker playback bus (v0.23 full-duplex audio): the AUDIO hook pushes
    /// the viewer's mic ticks here; the playback thread drains them into
    /// `waveOut`. `None` when `--audio` was not given.
    speaker: Option<speaker::AudioBus>,
    /// Stop flag + handle of the speaker playback thread (v0.23). Phase 4:
    /// set and joined when the hooks drop at session end, so the thread and
    /// its `waveOut` device no longer leak per session.
    speaker_stop: Option<Arc<AtomicBool>>,
    speaker_thread: Option<std::thread::JoinHandle<()>>,
    /// Clipboard sync (v0.26): the echo-loop guard. The `CLIPBOARD` hook
    /// calls [`ClipboardSync::note_remote`] after applying the peer's text;
    /// the serve loop polls the OS clipboard and calls
    /// [`ClipboardSync::note_local`], sending what it returns.
    /// `None` when `--no-clipboard` was given.
    clipboard: Option<Arc<Mutex<ovapp::clipboard::ClipboardSync>>>,
    /// The viewer's CAPS bits (Phase 3; 0 = an old viewer that never sent
    /// CAPS → full DEFLATE frames only). Read by the capture thread.
    peer_caps: Arc<AtomicU32>,
    /// KEYFRAME_REQUEST seen (Phase 3): the capture thread sends a full
    /// frame next.
    keyframe: Arc<AtomicBool>,
    /// VIDEO_IDR_REQUEST seen (Phase 5): the next H.264 picture is an IDR.
    video_idr: Arc<AtomicBool>,
    /// Section 4: the session's desktop effects (SESSION_OPTION toggles).
    fx: Option<Arc<Mutex<desktop_fx::DesktopFx>>>,
}

/// A capture-target request slot: the rect the viewer asked for, in
/// virtual-desktop coordinates, parked until the capture loop consumes it.
type PendingRegion = Arc<Mutex<Option<(i32, i32, u32, u32)>>>;

/// The session's input controller, shared by the command hook, the capture
/// thread (region retargets) and the session loop (stuck-input timeout).
type SharedInput = Arc<Mutex<input::InputController>>;

/// The input backend for this run: `SendInput`, or the logging dry run.
fn input_backend(vs: VirtualScreen) -> Box<dyn input::InputBackend> {
    if DRY_INPUT.load(Ordering::Relaxed) {
        info!("--dry-input: input is logged, not injected");
        Box::new(input::DryRunBackend::new(vs))
    } else {
        Box::new(input::SendInputBackend)
    }
}

impl Drop for HostHooks {
    /// Session over: stop and join the speaker thread (closes `waveOut`).
    fn drop(&mut self) {
        if let Some(stop) = &self.speaker_stop {
            stop.store(true, Ordering::Relaxed);
        }
        if let Some(t) = self.speaker_thread.take() {
            let _ = t.join();
        }
    }
}

impl HostHooks {
    fn new() -> Self {
        Self {
            echo: Arc::new(Mutex::new(Vec::new())),
            input: None,
            liveness: Arc::new(Mutex::new(Liveness::new(Instant::now()))),
            target_fps: Arc::new(AtomicU16::new(0)),
            pong_seq: Arc::new(Mutex::new(Vec::new())),
            rtt_us: Arc::new(AtomicU64::new(0)),
            pending_region: Arc::new(Mutex::new(None)),
            speaker: None,
            speaker_stop: None,
            speaker_thread: None,
            clipboard: None,
            peer_caps: Arc::new(AtomicU32::new(0)),
            keyframe: Arc::new(AtomicBool::new(false)),
            video_idr: Arc::new(AtomicBool::new(false)),
            fx: None,
        }
    }

    /// Enable the clipboard sync path (v0.26): install the echo-loop guard
    /// and make the `CLIPBOARD` hook write the peer's text to the OS
    /// clipboard. On by default; `--no-clipboard` skips this.
    fn enable_clipboard(&mut self) {
        let sync = Arc::new(Mutex::new(ovapp::clipboard::ClipboardSync::new()));
        self.clipboard = Some(Arc::clone(&sync));
    }

    /// Enable the speaker playback path (v0.23 `--audio`): install the bus
    /// and spawn the playback thread, already enabled — the host plays
    /// whatever arrives the moment it arrives.
    fn enable_speaker(&mut self) {
        let bus = speaker::AudioBus::new();
        bus.set_enabled(true);
        let stop = Arc::new(AtomicBool::new(false));
        self.speaker_thread = speaker::spawn(bus.clone(), Arc::clone(&stop));
        self.speaker = Some(bus);
        self.speaker_stop = Some(stop);
    }

    /// Install the command hook on a session (chat echo, key logging,
    /// file/transfer reporting; frames are filtered out by the pump).
    /// Every decoded command also refreshes the liveness tracker.
    fn register(&self, term: &mut TerminalSession) {
        let echo_hook = Arc::clone(&self.echo);
        let input = self.input.clone();
        let live = Arc::clone(&self.liveness);
        let target_fps = Arc::clone(&self.target_fps);
        let pong_seq = Arc::clone(&self.pong_seq);
        let pending_region = Arc::clone(&self.pending_region);
        let hooks_audio = self.speaker.clone();
        // Phase 4: raw PCM or Opus (the viewer encodes Opus only after our
        // CAPS advertised CAP_OPUS); one decoder state for the stream.
        let mut audio_dec = ovapp::audio::AudioDecoder::new();
        let hooks_clip = self.clipboard.clone();
        let peer_caps = Arc::clone(&self.peer_caps);
        let keyframe = Arc::clone(&self.keyframe);
        let video_idr = Arc::clone(&self.video_idr);
        let fx = self.fx.clone();
        term.on_cmd(Some(Box::new(move |cmd| {
            let _ = live.lock().map(|mut l| l.record_inbound(Instant::now()));
            match cmd {
                Command::Type { key, mods, payload } => {
                    if *key == 0 && *mods == 0 {
                        // A synthetic keystroke that is really a chat message.
                        let line = line_str(payload);
                        info!("chat: {line}");
                        echo_hook.lock().unwrap().push(payload.clone());
                    } else {
                        trace!(
                            "key=0x{key:02x} mods=0x{mods:02x} payload={} bytes",
                            payload.len()
                        );
                    }
                }
                // Input, legacy (POINTER / KEY) and v2 (CAP_INPUT2): the
                // controller maps and injects it, and tracks what is held.
                Command::Pointer { .. }
                | Command::Key { .. }
                | Command::PointerEx { .. }
                | Command::Wheel { .. }
                | Command::KeyEx { .. }
                | Command::Text(_)
                | Command::ReleaseAll
                | Command::Sas => {
                    log_input(cmd);
                    if let Some(slot) = &input {
                        if let Ok(mut ctl) = slot.lock() {
                            if ctl.handle(cmd, Instant::now()) == Some(false) {
                                // Refused once means refused for every
                                // event until the desktop changes: warn once.
                                static WARNED: AtomicBool = AtomicBool::new(false);
                                if WARNED.swap(true, Ordering::Relaxed) {
                                    debug!("input injection refused");
                                } else {
                                    warn!("input injection refused (blocked desktop / UIPI?)");
                                }
                            }
                        }
                    }
                }
                Command::Echo(p) => trace!("echo {} bytes", p.len()),
                Command::Ping(_) => {} // auto-replied by the pump
                Command::Pong(n) => {
                    // v0.17: hand the echo to the capture loop's keepalive so it
                    // can measure RTT (the auto-quality input).
                    if let Ok(mut q) = pong_seq.lock() {
                        q.push(*n);
                    }
                }
                Command::FileBegin { size, name } => {
                    info!("file `{name}` incoming ({size} bytes)")
                }
                Command::FileData(_) => {}
                Command::FileEnd => {}
                // v0.24 file-id-keyed transfers: reassembled by the pump and
                // drained in the serve loop (take_xfer_file); nothing to do here.
                Command::FileXferData { .. } | Command::FileXferComplete { .. } => {}
                // Frame traffic is reassembled into the screen path by the session
                // (see `on_frame`); the pump filters it out, so `on_cmd` never sees
                // these. The arm exists only to keep the match exhaustive.
                Command::FrameBegin { .. }
                | Command::TilesBegin { .. }
                | Command::VideoBegin { .. }
                | Command::FrameData(_)
                | Command::FrameEnd(_) => {}
                // CAPS (Phase 3): what the viewer can decode. The pump already
                // answered with ours; the capture thread picks the mode up.
                Command::Caps { flags } => {
                    peer_caps.store(*flags, Ordering::Relaxed);
                    debug!(
                        "viewer caps {flags:#x} (tiles {}, zstd {}, h264 {})",
                        flags & ovapp::terminal::CAP_TILES != 0,
                        flags & ovapp::terminal::CAP_ZSTD != 0,
                        flags & ovapp::terminal::CAP_H264 != 0
                    );
                }
                // KEYFRAME_REQUEST (Phase 3): the viewer lost sync.
                Command::KeyframeRequest { have_seq } => {
                    keyframe.store(true, Ordering::Relaxed);
                    debug!("keyframe requested (viewer at frame {have_seq})");
                }
                // VIDEO_IDR_REQUEST (Phase 5): the viewer's H.264 chain broke.
                Command::VideoIdrRequest { have_seq } => {
                    video_idr.store(true, Ordering::Relaxed);
                    debug!("H.264 IDR requested (viewer at video unit {have_seq})");
                }
                // Cursor commands are viewer-bound (host → viewer only); the host
                // never receives them, so this is just the exhaustive-match arm.
                Command::Cursor { .. } | Command::CursorShape { .. } => {}
                // AUDIO (v0.21) is viewer-bound in one-way mode; v0.23 makes
                // 0x40 direction-agnostic — a received AUDIO is the REMOTE peer's
                // mic, so with `--audio` it is decoded and pushed into the
                // speaker bus (malformed payloads are dropped, never panic).
                Command::Audio(payload) => {
                    if let Some(bus) = &hooks_audio {
                        if let Ok(samples) = audio_dec.decode(payload) {
                            bus.push(&samples);
                        }
                    }
                }
                // CLIPBOARD (v0.26): the peer's clipboard text — write it to the
                // local OS clipboard, then record it in the echo-loop guard so
                // the serve loop's own poll does not send it straight back.
                // Applying on the hook thread is fine: the Win32 clipboard API
                // is call-synchronous and the serve loop's poll only takes the
                // lock for a read when the sequence number changed.
                Command::Clipboard { kind: 0, text } => {
                    if let Some(sync) = &hooks_clip {
                        let text = String::from_utf8_lossy(text).to_string();
                        if clipboard_os::clip_write_text(&text) {
                            if let Ok(mut s) = sync.lock() {
                                s.note_remote(&text);
                            }
                        } else {
                            warn!("clipboard write failed (peer text dropped)");
                        }
                    }
                }
                // Reserved clipboard kinds are rejected by decode; this arm only
                // keeps the match exhaustive.
                Command::Clipboard { .. } => {}
                // CAPTURE_REGION (v0.18): the viewer picked a capture rect (a
                // whole monitor from MONITORS_INFO). Validate it against the
                // virtual desktop and park it for the capture loop — the handler
                // never touches the GDI source itself, so there is no re-entrancy
                // with the BitBlt.
                Command::CaptureRegion { x, y, w, h } => {
                    let vs = VirtualScreen::query();
                    {
                        let (vx, vy, vw, vh) = (vs.x, vs.y, vs.w, vs.h);
                        // Translate desktop coords into the (0,0)-origin space
                        // validate_region clamps against, then translate back.
                        match validate_region(x - vx, y - vy, *w, *h, vw as i32, vh as i32) {
                            Ok((rx, ry, rw, rh)) => {
                                let rect = (rx + vx, ry + vy, rw, rh);
                                if let Ok(mut slot) = pending_region.lock() {
                                    *slot = Some(rect);
                                }
                                debug!(
                                    "capture target -> {}x{} at ({},{})",
                                    rect.2, rect.3, rect.0, rect.1
                                );
                            }
                            Err(e) => warn!("capture region rejected: {e}"),
                        }
                    }
                }
                // MONITORS_INFO is host->viewer only; the host never receives it.
                Command::MonitorsInfo { .. } => {}
                // SESSION_OPTION (Section 4): the viewer toggles a desktop effect.
                Command::SessionOption { key, value } => {
                    if let Some(fx) = &fx {
                        if let Ok(mut f) = fx.lock() {
                            let on = *value != 0;
                            let r = match *key {
                                ovapp::terminal::OPT_HIDE_WALLPAPER => {
                                    Some(("wallpaper hidden", f.set_wallpaper_hidden(on)))
                                }
                                ovapp::terminal::OPT_NO_ANIMATIONS => {
                                    Some(("animations off", f.set_animations_off(on)))
                                }
                                _ => None,
                            };
                            match r {
                                Some((what, Ok(()))) => {
                                    debug!("viewer set {what} = {on}")
                                }
                                Some((what, Err(e))) => {
                                    warn!("viewer set {what} = {on} failed ({e})")
                                }
                                None => {}
                            }
                        }
                    }
                }
                // Section 5: host -> viewer only.
                Command::DirectCandidates { .. } => {}
                // Wave 3: media-channel signals. A video unit never reaches
                // the hook; an audio gap of the viewer's mic is simply skipped
                // (the speaker jitter buffer rides it out).
                Command::VideoUnit { .. } | Command::MediaGap { .. } => {}
                // QUALITY_SET (v0.17): the viewer's target capture rate (0 = auto).
                Command::QualitySet { target_fps: want } => {
                    target_fps.store(*want, Ordering::Relaxed);
                    if *want == 0 {
                        debug!("quality -> AUTO (queue-delay driven)");
                    } else {
                        debug!("quality -> {want} fps (manual)");
                    }
                }
            }
        })));
    }

    /// Take the chat payloads queued since the last drain.
    fn drain(&self) -> Vec<Vec<u8>> {
        std::mem::take(&mut *self.echo.lock().unwrap())
    }
}

/// One trace line per inbound input command.
fn log_input(cmd: &Command) {
    if !tracing::enabled!(tracing::Level::TRACE) {
        return;
    }
    match cmd {
        Command::Pointer { kind, x, y } => trace!("pointer {kind:?} ({x},{y})/65535"),
        Command::Key { vk, down } => {
            trace!("key vk=0x{vk:02x} {}", if *down { "down" } else { "up" })
        }
        Command::PointerEx {
            action,
            button,
            flags,
            x,
            y,
        } => trace!("pointer-ex {action:?} {button:?} flags={flags:#x} ({x},{y})"),
        Command::Wheel { dy, dx } => trace!("wheel dy={dy} dx={dx}"),
        Command::KeyEx { vk, scan, flags } => trace!(
            "key-ex vk=0x{vk:02x} scan=0x{scan:02x} {}{}",
            if flags & ovapp::terminal::KEY_DOWN != 0 {
                "down"
            } else {
                "up"
            },
            if flags & ovapp::terminal::KEY_EXTENDED != 0 {
                " ext"
            } else {
                ""
            }
        ),
        Command::Text(t) => trace!("text {t:?} ({} chars)", t.chars().count()),
        Command::ReleaseAll => trace!("release-all"),
        Command::Sas => trace!("ctrl+alt+del (SAS) requested"),
        _ => {}
    }
}

/// Clipboard poll (v0.26): cheap sequence-number check each loop; on a
/// change, read the text and feed the echo-loop guard. Returns the text to
/// send to the peer, if any. `None` when clipboard sync is off, nothing
/// changed, or the read failed (transient lock contention — retried next
/// poll because the sequence number has not been consumed).
fn clipboard_poll(
    sync: &Option<Arc<Mutex<ovapp::clipboard::ClipboardSync>>>,
    last_seq: &mut u32,
) -> Option<String> {
    let sync = sync.as_ref()?;
    let seq = clipboard_os::clip_sequence();
    if seq == *last_seq {
        return None;
    }
    *last_seq = seq;
    let text = clipboard_os::clip_read_text()?;
    let mut s = sync.lock().ok()?;
    s.note_local(&text)
}

/// Write completed inbound file transfers to disk / log them (both the
/// v0.24 file-id-keyed path and the legacy FILE_BEGIN path). Received files
/// land in `Downloads\OpenView` (never overwriting an existing file); the
/// iOS app's diagnostic logs go to `<data dir>\logs\phone`.
fn drain_received_files(term: &mut TerminalSession, data_dir: &Path) {
    while let Some(f) = term.take_xfer_file() {
        let name = ovapp::file_transfer::FileTransfer::sanitize_name(&f.name);
        // Diagnostics: the iOS app uploads its session logs from earlier
        // runs right after connecting. They go to the host's log folder,
        // not to the user's received files.
        if let Some(dir) = phone_log_dir(data_dir, &name) {
            let _ = std::fs::create_dir_all(&dir);
            let dest = dir.join(&name);
            match std::fs::write(&dest, &f.data) {
                Ok(()) => debug!(
                    "phone log saved: {} ({} bytes)",
                    dest.display(),
                    f.data.len()
                ),
                Err(e) => warn!("phone log `{name}` not saved: {e}"),
            }
            continue;
        }
        let dir = ovcommon::paths::received_files_dir();
        let saved = std::fs::create_dir_all(&dir).and_then(|()| {
            let dest = ovcommon::paths::unique_path(&dir, &name);
            std::fs::write(&dest, &f.data).map(|()| dest)
        });
        match saved {
            Ok(dest) => info!(
                "received `{}` -> {} ({} bytes)",
                f.name,
                dest.display(),
                f.data.len()
            ),
            Err(e) => warn!("received `{}` but could not save it: {e}", f.name),
        }
    }
    while let Some((name, data)) = term.take_file() {
        info!(
            "received file `{name}` ({} bytes, legacy transfer, not saved)",
            data.len()
        );
    }
}

/// File-name prefix of the iOS app's diagnostic log uploads.
const PHONE_LOG_PREFIX: &str = "ovlog-ios-";

/// Where an uploaded phone log goes: `<data dir>\logs\phone` (`None` = not
/// a phone log). Only plain `ovlog-ios-*.log` names qualify.
fn phone_log_dir(data_dir: &Path, name: &str) -> Option<PathBuf> {
    let ok = name.starts_with(PHONE_LOG_PREFIX)
        && name.ends_with(".log")
        && name.len() <= 160
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    ok.then(|| ovcommon::paths::logs_dir(data_dir).join("phone"))
}

/// Feed the PONG echoes the hook collected into our keepalive; a matched
/// echo updates the smoothed RTT the auto-quality rule reads.
fn absorb_pongs(hooks: &HostHooks, keepalive: &mut Keepalive) {
    absorb_pongs_with(hooks, keepalive, |_| false);
}

/// [`absorb_pongs`], first offering each echo to `pacer` (Phase 4: the
/// screen sender's probe PINGs come back as PONGs too; it consumes its own).
fn absorb_pongs_with(
    hooks: &HostHooks,
    keepalive: &mut Keepalive,
    mut pacer: impl FnMut(u64) -> bool,
) {
    let pongs = hooks
        .pong_seq
        .lock()
        .map(|mut q| std::mem::take(&mut *q))
        .unwrap_or_default();
    for seq in pongs {
        if pacer(seq) {
            continue;
        }
        if let Some(rtt) = keepalive.on_pong(seq, Instant::now()) {
            hooks
                .rtt_us
                .store(rtt.as_micros() as u64, Ordering::Relaxed);
        }
    }
}

/// Peer-liveness verdict for one loop iteration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeerState {
    Alive,
    /// No inbound traffic for [`DEAD_AFTER`]: stop producing media, keep
    /// the session in case traffic resumes.
    Dead,
    /// No inbound traffic for [`SESSION_TIMEOUT`]: give the session up.
    Gone,
}

/// Classify the time since the last inbound record (pure; unit-tested).
fn peer_state(since_inbound: Duration) -> PeerState {
    if since_inbound > SESSION_TIMEOUT {
        PeerState::Gone
    } else if since_inbound > DEAD_AFTER {
        PeerState::Dead
    } else {
        PeerState::Alive
    }
}

/// Track liveness transitions and log them once. Returns the new state.
fn check_peer(hooks: &HostHooks, was: PeerState) -> PeerState {
    let since = hooks
        .liveness
        .lock()
        .map(|l| l.since_inbound(Instant::now()))
        .unwrap_or_default();
    let now = peer_state(since);
    match (was, now) {
        (PeerState::Alive, PeerState::Dead) => {
            info!("peer silent for {DEAD_AFTER:?}; pausing the stream")
        }
        (PeerState::Dead, PeerState::Alive) => info!("peer liveness recovered"),
        (_, PeerState::Gone) => {
            session_over(&format!("peer silent for {SESSION_TIMEOUT:?}"));
        }
        _ => {}
    }
    now
}

/// The session is over: log it and tell the GUI (status channel).
fn session_over(reason: &str) {
    info!("peer disconnected ({reason})");
    status::emit(&StatusEvent::PeerDisconnected {
        reason: reason.to_owned(),
    });
}

/// How often the session loop polls the OS clipboard sequence number.
const CLIPBOARD_POLL: Duration = Duration::from_millis(50);
/// How often the screen session polls the cursor (the tracker throttles
/// actual sends further).
const CURSOR_POLL: Duration = Duration::from_millis(8);
/// Cursor poll interval while it is moving (> the 120 Hz send cap).
const CURSOR_POLL_ACTIVE: Duration = Duration::from_millis(4);
/// How long after the last move the fast poll continues.
const CURSOR_ACTIVE_FOR: Duration = Duration::from_millis(250);
/// Longest idle wait when the socket cannot be watched (no readiness
/// registration): bounds inbound latency on such carriers.
const UNWATCHED_WAIT: Duration = Duration::from_millis(2);
/// Longest idle wait in terminal mode (keepalive/clipboard housekeeping).
const TERMINAL_IDLE_WAIT: Duration = Duration::from_millis(50);

/// The terminal-mode serve loop (no screen): chat echo, file transfers,
/// clipboard sync, keepalive + dead-peer detection, and — with `--audio`
/// (v0.23 full duplex) — the viewer's mic played on the host speakers.
/// Blocks on the socket between events instead of polling on a sleep.
fn serve_terminal(
    mut term: TerminalSession,
    clipboard: bool,
    audio_playback: bool,
    data_dir: &Path,
) -> Result<(), String> {
    let mut hooks = HostHooks::new();
    if audio_playback {
        hooks.enable_speaker();
        debug!("speaker playback on (viewer's mic, 20 ms ticks)");
    }
    if clipboard {
        hooks.enable_clipboard();
    }
    hooks.register(&mut term);
    let wake = ovapp::netwait::Wakeup::with_sockets(term.raw_socket(), &term.extra_sockets())
        .map_err(|e| format!("wait setup failed: {e}"))?;
    let mut keepalive = Keepalive::new(KEEPALIVE_INTERVAL, Instant::now());
    let mut peer = PeerState::Alive;
    let mut clip_seq = clipboard_os::clip_sequence();
    let mut next_clip = Instant::now();
    loop {
        if let Err(e) = term.pump() {
            log_direct(&mut term);
            session_over(&e.to_string());
            return Ok(());
        }
        log_direct(&mut term);
        peer = check_peer(&hooks, peer);
        if peer == PeerState::Gone {
            let _ = term.close();
            return Ok(());
        }
        if let Some(seq) = keepalive.tick(Instant::now()) {
            if term.send(&Command::Ping(seq)).is_err() {
                session_over("keepalive send failed");
                return Ok(());
            }
        }
        absorb_pongs(&hooks, &mut keepalive);
        if Instant::now() >= next_clip {
            next_clip = Instant::now() + CLIPBOARD_POLL;
            if let Some(text) = clipboard_poll(&hooks.clipboard, &mut clip_seq) {
                let _ = term.send(&ovapp::clipboard::clipboard_command(&text));
            }
        }
        // Echo any chat lines received this cycle back to the peer.
        for payload in hooks.drain() {
            if term
                .send(&Command::Type {
                    key: 0,
                    mods: 0,
                    payload,
                })
                .is_err()
            {
                break;
            }
        }
        drain_received_files(&mut term, data_dir);
        let mut wait = if wake.watches_socket() {
            TERMINAL_IDLE_WAIT
        } else {
            UNWATCHED_WAIT
        };
        if let Some(d) = term.transport_deadline() {
            wait = wait.min(d.max(Duration::from_millis(1)));
        }
        wake.wait(wait);
    }
}

/// State shared between the screen session thread and its capture thread.
struct CaptureCtl {
    /// Ask the capture thread to exit.
    stop: AtomicBool,
    /// Peer dead: stop capturing (no wire traffic) until it recovers.
    paused: AtomicBool,
    /// The viewer's QUALITY_SET target (0 = AUTO), shared with the hooks.
    target_fps: Arc<AtomicU16>,
    /// The host's smoothed RTT (us), shared with the hooks.
    rtt_us: Arc<AtomicU64>,
    /// The session thread's smoothed per-frame drain time (us; 0 = none
    /// yet) — what the link actually sustains, for the link-limited log.
    drain_us: AtomicU64,
    /// Estimated time (us) until the in-flight frame is fully handed to the
    /// carrier (0 = link free): the capture thread times its next capture
    /// to finish just as the link frees up.
    link_eta_us: AtomicU64,
    /// CAPTURE_REGION requests parked by the hook.
    pending_region: PendingRegion,
    /// The input controller (its region is re-mapped on retarget).
    input: SharedInput,
    /// The pacer's excess queueing delay (us; `u64::MAX` = no fresh
    /// signal): the auto-quality rule's congestion input.
    queue_delay_us: AtomicU64,
    /// The captured region's origin (cursor coordinates are relative to it).
    origin: Mutex<(i32, i32)>,
    /// Set after a retarget: the session re-announces MONITORS_INFO.
    monitors_dirty: AtomicBool,
    /// The viewer's CAPS bits (shared with the hooks).
    peer_caps: Arc<AtomicU32>,
    /// Pending KEYFRAME_REQUEST (shared with the hooks).
    keyframe: Arc<AtomicBool>,
    /// Pending VIDEO_IDR_REQUEST (Phase 5, shared with the hooks).
    video_idr: Arc<AtomicBool>,
    /// The link pacer's delivery-rate estimate, bytes/s (0 = none yet):
    /// the H.264 bitrate follows it (Phase 5).
    link_rate: AtomicU64,
}

/// Drain pacing (pure; unit-tested): how long the capture thread should
/// hold off so its next frame is ready just as the link finishes the frame
/// in flight — `None` = capture now. `eta` is the in-flight frame's
/// remaining drain time, `cost` the smoothed capture+compress time, and
/// `queued` whether a captured frame is already waiting to be sent (then a
/// new capture would only replace it, wasting a compress).
fn capture_hold(eta: Duration, cost: Duration, queued: bool) -> Option<Duration> {
    let max_slice = Duration::from_millis(20);
    if eta > cost {
        Some((eta - cost).min(max_slice))
    } else if queued {
        Some(Duration::from_millis(1))
    } else {
        None
    }
}

/// Rolling capture-thread counters for the periodic metrics line.
#[derive(Default)]
struct CaptureMetrics {
    grabs: u64,
    grab_time: Duration,
    encodes: u64,
    encode_time: Duration,
    full: u64,
    deltas: u64,
    tiles: u64,
    bytes: u64,
    /// Wave 1: capture-phase latency (OS present -> grab), DXGI only.
    latency: Duration,
    latency_n: u64,
    latency_max: Duration,
    /// Wave 2: refinement-lane updates and the tiles they carried.
    refines: u64,
    refine_tiles: u64,
}

impl CaptureMetrics {
    fn log(&self, backend: capture::Backend, over: Duration) {
        let per = |t: Duration, n: u64| t.as_secs_f64() * 1000.0 / n.max(1) as f64;
        debug!(
            "capture {backend}: {:.2} ms/grab, encode {:.2} ms/update; {} full + {} delta \
             updates ({:.1} tiles/delta), {:.1} KB/s",
            per(self.grab_time, self.grabs),
            per(self.encode_time, self.encodes),
            self.full,
            self.deltas,
            self.tiles as f64 / self.deltas.max(1) as f64,
            self.bytes as f64 / 1024.0 / over.as_secs_f64().max(0.001)
        );
        if self.latency_n > 0 {
            debug!(
                "capture latency (present -> grab): avg {:.2} ms, max {:.2} ms over {} grabs",
                per(self.latency, self.latency_n),
                self.latency_max.as_secs_f64() * 1000.0,
                self.latency_n
            );
        }
        if self.refines > 0 {
            debug!(
                "refinement lane: {} update(s), {} tile(s) refined to exact pixels",
                self.refines, self.refine_tiles
            );
        }
    }
}

/// Wave 1: highest AUTO capture rate (`--max-fps`); the rate only goes
/// above `--fps` when the pacer's delivery rate carries it.
static MAX_AUTO_FPS: AtomicU32 = AtomicU32::new(60);

/// Wave 1 (pure; unit-tested): the AUTO base rate the link allows — `base`
/// raised to 30 or `max` fps when the pacer's delivery rate (`rate` bytes/s)
/// carries that many updates of the smoothed size (`update_bytes`) in
/// about half the link. `None` signal = `base`.
fn link_base_fps(base: u32, max: u32, rate: u64, update_bytes: f64) -> u32 {
    if rate == 0 || update_bytes <= 0.0 || max <= base {
        return base;
    }
    let carried = rate as f64 * 0.5 / update_bytes.max(1024.0);
    let mut fps = base;
    for step in [30, max] {
        if f64::from(step) <= carried && step > fps {
            fps = step.min(max);
        }
    }
    fps
}

/// Wave 1: register the calling thread with MMCSS ("Capture" task) so the
/// capture/encode thread is scheduled ahead of background work. Returns
/// the handle for [`mmcss_leave`] (`0` = not registered).
#[cfg(windows)]
fn mmcss_enter(task: &str) -> isize {
    #[link(name = "avrt")]
    extern "system" {
        fn AvSetMmThreadCharacteristicsW(task: *const u16, index: *mut u32) -> isize;
    }
    let name: Vec<u16> = task.encode_utf16().chain(std::iter::once(0)).collect();
    let mut idx = 0u32;
    // SAFETY: NUL-terminated UTF-16 name, valid out-pointer.
    unsafe { AvSetMmThreadCharacteristicsW(name.as_ptr(), &mut idx) }
}

#[cfg(windows)]
fn mmcss_leave(h: isize) {
    #[link(name = "avrt")]
    extern "system" {
        fn AvRevertMmThreadCharacteristics(h: isize) -> i32;
    }
    if h != 0 {
        // SAFETY: a handle returned by AvSetMmThreadCharacteristicsW.
        unsafe { AvRevertMmThreadCharacteristics(h) };
    }
}

/// The capture thread: owns the screen source (DXGI or GDI); captures at
/// the governor's rate — but never faster than the link drains frames (see
/// [`capture_hold`]) — and hands each capture to the Phase 3
/// [`UpdateEncoder`], which diffs it against what the viewer holds and
/// queues a full frame or a tile delta (or nothing) in the mailbox.
/// Never touches the session.
fn capture_loop(
    mut src: capture::ScreenSource,
    base_fps: u32,
    ctl: Arc<CaptureCtl>,
    mailbox: Arc<ovapp::sendq::FrameMailbox>,
) {
    let mut gov = RateGovernor::new(base_fps.max(1), IDLE_FPS);
    let mut auto_q = AutoQuality::new();
    let mut last_scale_step: i32 = 10;
    let mut enc = UpdateEncoder::new();
    // Wave 2: lossy-first image tiles + tile cache (CAPS-gated per viewer).
    enc.set_lossy(!NO_LOSSY.load(Ordering::Relaxed));
    enc.set_cache(!NO_TILE_CACHE.load(Ordering::Relaxed));
    enc.set_lossy_params(ovapp::lossy::LossyParams {
        quality: LOSSY_QUALITY.load(Ordering::Relaxed),
        ..ovapp::lossy::LossyParams::default()
    });
    // Phase 5: high-motion regions as H.264 (only to a CAP_H264 viewer).
    let mut video = ovapp::video::VideoSession::new(video_encoder_factory(), VIDEO_FPS);
    // Benches: `--video-bpp` raises the area-based bitrate budget.
    if let Some(&bpp) = VIDEO_BPP.get() {
        video.set_bits_per_pixel(bpp);
    }
    let mut video_was: Option<ovapp::tiles::TileRect> = None;
    let mut video_logged = ovapp::video::VideoStats::default();
    let mut metrics = CaptureMetrics::default();
    let mut backend = src.backend();
    let mut frames_sent: u64 = 0;
    let mut frames_skipped: u64 = 0;
    let mut last_metrics = Instant::now();
    let mut last_effective: u32 = 0;
    let mut last_link_ms: u64 = 0;
    let mut next_capture = Instant::now();
    let mut last_started = Instant::now();
    // Smoothed capture + change-detect + compress time.
    let mut cost = Duration::from_millis(20);
    // Wave 1: a change was pulled in by `wait_change` since the last grab.
    let mut buffered = false;
    // Wave 1: smoothed wire bytes of a changed update (for the 60 fps rule).
    let mut update_bytes: f64 = 0.0;
    let mut enc_logged = ovapp::sendq::EncoderStats::default();
    #[cfg(windows)]
    let mmcss = mmcss_enter("Capture");
    #[cfg(windows)]
    if mmcss == 0 {
        debug!("MMCSS registration failed (capture thread keeps normal priority)");
    }
    while !ctl.stop.load(Ordering::Relaxed) {
        // Capture-target retarget (v0.18): re-create the DIB, re-map
        // pointer input, and have the session re-announce the monitors.
        if let Some(rect) = ctl.pending_region.lock().ok().and_then(|mut s| s.take()) {
            match src.retarget(rect.0, rect.1, rect.2, rect.3) {
                Ok(()) => {
                    if let Ok(mut o) = ctl.origin.lock() {
                        *o = src.origin();
                    }
                    if let Ok(mut input) = ctl.input.lock() {
                        input.set_region(Region {
                            x: rect.0,
                            y: rect.1,
                            w: rect.2,
                            h: rect.3,
                        });
                        input.set_virtual_screen(VirtualScreen::query());
                    }
                    // New monitor: the next update is a full frame (and
                    // whatever streamed as video is gone).
                    enc.reset();
                    video.stop(&mailbox, false);
                    ctl.monitors_dirty.store(true, Ordering::Relaxed);
                    debug!(
                        "capture retargeted to {}x{} at ({},{})",
                        rect.2, rect.3, rect.0, rect.1
                    );
                    next_capture = Instant::now();
                }
                Err(e) => warn!("retarget failed: {e}"),
            }
        }
        if ctl.paused.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        // Phase 3: follow the viewer's CAPS; a KEYFRAME_REQUEST is served
        // by the very next capture.
        let caps = ctl.peer_caps.load(Ordering::Relaxed);
        enc.set_peer_caps(caps);
        enc.set_link_rate(ctl.link_rate.load(Ordering::Relaxed));
        let h264 =
            caps & ovapp::terminal::CAP_H264 != 0 && H264_MODE.load(Ordering::Relaxed) != H264_OFF;
        if ctl.keyframe.swap(false, Ordering::Relaxed) {
            enc.request_keyframe();
            video.request_idr();
            next_capture = Instant::now();
        }
        if ctl.video_idr.swap(false, Ordering::Relaxed) {
            video.request_idr();
            next_capture = Instant::now();
        }
        // Quality: a manual QUALITY_SET wins; otherwise the auto rule
        // (AutoQuality) follows the pacer's excess queueing delay — never
        // the raw RTT — and lowers the H.264 bitrate before the fps.
        let want = ctl.target_fps.load(Ordering::Relaxed);
        let rtt_us = ctl.rtt_us.load(Ordering::Relaxed);
        let qd_us = ctl.queue_delay_us.load(Ordering::Relaxed);
        let queue_delay = (qd_us != u64::MAX).then(|| Duration::from_micros(qd_us));
        // Phase 5: smooth motion while a region streams as H.264 (AUTO
        // quality only); its bitrate follows the pacer's delivery rate.
        let link_base = link_base_fps(
            base_fps.max(1),
            MAX_AUTO_FPS.load(Ordering::Relaxed),
            ctl.link_rate.load(Ordering::Relaxed),
            update_bytes,
        );
        let auto_base = if video.active_rect().is_some() {
            link_base.max(VIDEO_FPS)
        } else {
            link_base
        };
        let decision = if want == 0 {
            auto_q.update(Instant::now(), auto_base, queue_delay)
        } else {
            ovapp::adapt::QualityDecision {
                fps: u32::from(want),
                bitrate_scale: 1.0,
                degraded: false,
            }
        };
        let effective = decision.fps;
        let rate = ctl.link_rate.load(Ordering::Relaxed);
        video.set_link_rate((rate > 0).then_some(rate as f64 * decision.bitrate_scale));
        video.set_fps(if want == 0 && !decision.degraded {
            effective.max(VIDEO_FPS)
        } else {
            effective
        });
        let scale_step = (decision.bitrate_scale * 10.0).round() as i32;
        if effective != last_effective || scale_step != last_scale_step {
            gov.set_base_fps(effective);
            last_effective = effective;
            last_scale_step = scale_step;
            let mode = if want == 0 { "AUTO" } else { "manual" };
            debug!(
                "capture rate -> {effective} fps ({mode}, queue delay {}, H.264 bitrate x{:.1}, rtt {rtt_us} us)",
                queue_delay.map_or_else(|| "n/a".to_owned(), |d| format!("{} ms", d.as_millis())),
                decision.bitrate_scale
            );
        }
        // Report when the link, not the governor, sets the frame rate.
        let drain = Duration::from_micros(ctl.drain_us.load(Ordering::Relaxed));
        let link_ms = drain.as_millis() as u64;
        if drain > gov.interval() && link_ms.abs_diff(last_link_ms) * 4 > last_link_ms.max(20) {
            debug!(
                "link-limited: ~{:.1} fps (a frame drains in {link_ms} ms)",
                1000.0 / link_ms.max(1) as f64
            );
            last_link_ms = link_ms;
        }
        // Wave 2 refinement lane: lossy (JPEG / video-painted) tiles that
        // settled go out as small exact chunks whenever the previous
        // update left the mailbox — no capture needed, never ahead of a
        // waiting change (a capture replaces an unsent chunk).
        let refine_due = enc.refine_due();
        if refine_due && !mailbox.is_pending() {
            let t0 = Instant::now();
            if let SubmitOutcome::Delta { tiles, bytes, .. } = enc.submit_refine(&mailbox) {
                metrics.refines += 1;
                metrics.refine_tiles += tiles as u64;
                metrics.bytes += bytes as u64;
                metrics.encode_time += t0.elapsed();
                metrics.encodes += 1;
            }
        }
        let poll = if refine_due {
            Duration::from_millis(4)
        } else {
            Duration::from_millis(20)
        };
        let now = Instant::now();
        let active = Duration::from_secs_f64(1.0 / f64::from(effective.max(1)));
        if now < next_capture {
            // Short slices so stop/retarget/pause are noticed promptly.
            let slice = (next_capture - now).min(poll);
            // Section 5 / Wave 1: event-driven capture, idle *and* active:
            // block on the duplication instead of sleeping. While idle (a
            // slow governor tick) a change pulls the next capture in to the
            // active frame interval; while streaming it is buffered for the
            // slot (the capture below then runs without waiting).
            let idle = gov.interval() > active;
            match src.wait_change(slice) {
                Some(true) => {
                    buffered = true;
                    if idle {
                        next_capture = next_capture.min(last_started + active);
                    }
                }
                Some(false) => {}
                None => std::thread::sleep(slice),
            }
            continue;
        }
        let eta = Duration::from_micros(ctl.link_eta_us.load(Ordering::Relaxed));
        if let Some(hold) = capture_hold(eta, cost, mailbox.is_pending()) {
            std::thread::sleep(hold);
            continue;
        }
        // Wave 1: the slot is open but nothing new arrived yet: block for
        // the first change (up to one more active interval) and capture the
        // moment it arrives, instead of grabbing an unchanged screen and
        // then making the change wait a whole interval for the next slot.
        // Past that the grab runs anyway (idle accounting, video exit).
        // (Wave 2: settled lossy tiles no longer need a grab — the
        // refinement lane above sends them from the last capture.)
        if !buffered && src.can_wait() {
            let deadline = next_capture + active;
            if now < deadline {
                let slice = (deadline - now).min(poll);
                if src.wait_change(slice) == Some(false) {
                    continue; // nothing yet: keep waiting (up to the deadline)
                }
            }
        }
        buffered = false;
        let started = Instant::now();
        last_started = started;
        // Section 5 `--gpu-encode`: the encoder reads the GPU copy.
        #[cfg(windows)]
        video.set_gpu_frame(
            src.gpu_frame()
                .map(|f| Box::new(f) as Box<dyn std::any::Any>),
        );
        match src.grab() {
            Ok(g) => {
                backend = g.backend;
                let grabbed = Instant::now();
                metrics.grabs += 1;
                metrics.grab_time += grabbed - started;
                // Phase 5: the video region is masked out of tile updates;
                // the motion detector then sees this capture's dirty tiles
                // (Wave 1: after copy-rect, so a scroll is not motion).
                let out = enc.submit_ex(
                    g.px,
                    g.w,
                    g.h,
                    g.hints,
                    g.moves,
                    video.active_rect(),
                    None,
                    &mailbox,
                );
                let encoded_video = video.on_capture(
                    Instant::now(),
                    g.px,
                    g.w,
                    g.h,
                    g.w as usize * 4,
                    enc.fresh(),
                    enc.fresh_valid(),
                    h264,
                    &mailbox,
                );
                if let Some(e) = video.take_error() {
                    warn!("H.264 encoder failed ({e}); staying on lossless tiles");
                }
                // Motion over: VIDEO_END is queued; now the lossless refresh
                // of the region (ordered after the END). Wave 1: queued as a
                // backlog the following updates drain ~64 KB at a time, so
                // input-driven changes never wait behind the whole region.
                let refresh = video.take_refresh();
                let out = match refresh {
                    Some(r) => {
                        enc.queue_refresh(r);
                        let o =
                            enc.submit_ex(g.px, g.w, g.h, Some(&[]), None, None, None, &mailbox);
                        if o == SubmitOutcome::Unchanged {
                            out
                        } else {
                            o
                        }
                    }
                    None => out,
                };
                let now_active = video.active_rect();
                if now_active != video_was {
                    match (video_was, now_active) {
                        (_, Some(r)) => debug!(
                            "motion -> H.264 region {}x{} at ({},{}) via {}",
                            r.w,
                            r.h,
                            r.x,
                            r.y,
                            video.encoder_name().unwrap_or_else(|| "(opening)".into())
                        ),
                        (Some(r), None) => debug!(
                            "motion over -> lossless refresh of {}x{} at ({},{})",
                            r.w, r.h, r.x, r.y
                        ),
                        (None, None) => {}
                    }
                    video_was = now_active;
                }
                match out {
                    SubmitOutcome::Unchanged if encoded_video => {
                        // Only the video region changed: full rate.
                        frames_sent += 1;
                        let _ = gov.on_change();
                    }
                    SubmitOutcome::Unchanged => {
                        // Unchanged screen: no wire traffic at all.
                        frames_skipped += 1;
                        let _ = gov.on_idle();
                    }
                    out => {
                        metrics.encodes += 1;
                        metrics.encode_time += grabbed.elapsed();
                        match out {
                            SubmitOutcome::Full { bytes } => {
                                metrics.full += 1;
                                metrics.bytes += bytes as u64;
                            }
                            SubmitOutcome::Delta { tiles, bytes, .. } => {
                                metrics.deltas += 1;
                                metrics.tiles += tiles as u64;
                                metrics.bytes += bytes as u64;
                                update_bytes = if update_bytes == 0.0 {
                                    bytes as f64
                                } else {
                                    update_bytes * 0.9 + bytes as f64 * 0.1
                                };
                            }
                            SubmitOutcome::Unchanged => {}
                        }
                        frames_sent += 1;
                        let _ = gov.on_change();
                    }
                }
            }
            Err(e) => debug!("capture error: {e}"),
        }
        if let Some(l) = src.take_latency() {
            metrics.latency += l;
            metrics.latency_n += 1;
            metrics.latency_max = metrics.latency_max.max(l);
        }
        // Schedule from the capture start so capture+compress time counts
        // against the interval (the effective rate is the requested one).
        let took = started.elapsed();
        cost = (cost * 3 + took) / 4;
        next_capture = started + gov.interval();
        if last_metrics.elapsed() >= METRICS_EVERY {
            log_screen_metrics(frames_sent, frames_skipped, &gov.stats());
            metrics.log(backend, last_metrics.elapsed());
            log_video(&video.stats(), &mut video_logged, last_metrics.elapsed());
            metrics = CaptureMetrics::default();
            let es = enc.stats();
            if es != enc_logged {
                debug!(
                    "copy-rect: {} move update(s) ({} detected scrolls), {} tiles saved; bands lz4/zstd/raw {}/{}/{}",
                    es.moves - enc_logged.moves,
                    es.detected_moves - enc_logged.detected_moves,
                    es.tiles_saved - enc_logged.tiles_saved,
                    es.lz4_bands - enc_logged.lz4_bands,
                    es.zstd_bands - enc_logged.zstd_bands,
                    es.raw_bands - enc_logged.raw_bands
                );
                if es.lossy_tiles != enc_logged.lossy_tiles
                    || es.refined_tiles != enc_logged.refined_tiles
                    || es.cache_refs != enc_logged.cache_refs
                {
                    debug!(
                        "wave2: {} lossy tile(s) in {:.1} KB, {} refined, cache {} ref(s) / {} store(s), {} awaiting refinement",
                        es.lossy_tiles - enc_logged.lossy_tiles,
                        (es.lossy_bytes - enc_logged.lossy_bytes) as f64 / 1024.0,
                        es.refined_tiles - enc_logged.refined_tiles,
                        es.cache_refs - enc_logged.cache_refs,
                        es.cache_stores - enc_logged.cache_stores,
                        enc.refresh_pending()
                    );
                }
                enc_logged = es;
            }
            debug!(
                "{} stale frame(s) superseded before sending",
                mailbox.replaced_count()
            );
            last_metrics = Instant::now();
        }
    }
    log_screen_metrics(frames_sent, frames_skipped, &gov.stats());
    log_video(&video.stats(), &mut video_logged, last_metrics.elapsed());
    #[cfg(windows)]
    mmcss_leave(mmcss);
}

/// The H.264 encoder factory of the capture thread (Phase 5): Media
/// Foundation, hardware first unless `--h264 sw`.
fn video_encoder_factory() -> ovapp::video::EncoderFactory {
    Box::new(|w, h, fps, bitrate| {
        #[cfg(windows)]
        {
            let pref = match H264_MODE.load(Ordering::Relaxed) {
                H264_SW => ovapp::mfcodec::EncoderPref::Software,
                H264_HW => ovapp::mfcodec::EncoderPref::Hardware,
                _ => ovapp::mfcodec::EncoderPref::Auto,
            };
            let t0 = Instant::now();
            let e = ovapp::mfcodec::MfVideoEncoder::new(w, h, fps, bitrate, pref)?;
            debug!(
                "H.264 encoder {} for {w}x{h} @ {fps} fps, {:.1} Mbps (opened in {} ms)",
                ovapp::video::VideoEncode::name(&e),
                f64::from(bitrate) / 1e6,
                t0.elapsed().as_millis()
            );
            Ok(Box::new(e) as Box<dyn ovapp::video::VideoEncode>)
        }
        #[cfg(not(windows))]
        {
            let _ = (w, h, fps, bitrate);
            Err("no H.264 encoder on this platform".to_owned())
        }
    })
}

/// One H.264 line per metrics period (only when video was streamed).
fn log_video(st: &ovapp::video::VideoStats, last: &mut ovapp::video::VideoStats, over: Duration) {
    let frames = st.frames - last.frames;
    if frames > 0 || st.enters != last.enters {
        let bytes = st.bytes - last.bytes;
        let t = st.encode_time.saturating_sub(last.encode_time);
        debug!(
            "H.264: {frames} pictures ({} IDR), {:.2} ms/picture encode, {:.0} KB/s; \
             {} motion period(s), {} grow(s), {} back to tiles, {} skipped (link busy)",
            st.keyframes - last.keyframes,
            t.as_secs_f64() * 1000.0 / frames.max(1) as f64,
            bytes as f64 / 1024.0 / over.as_secs_f64().max(0.001),
            st.enters - last.enters,
            st.grows - last.grows,
            st.exits - last.exits,
            st.skipped - last.skipped
        );
    }
    *last = *st;
}

/// Open the mic on its own thread and stream its PCM ticks (one per 20 ms)
/// over a channel, waking the session loop for each; the session thread
/// encodes them (Opus or raw, per the viewer's CAPS). The real mic paces
/// itself on its buffer-completion event and delivers every buffer; the
/// synthetic one is paced by a drift-free clock. `None` when the mic cannot
/// be opened.
fn spawn_mic(
    kind: MicKind,
    waker: ovapp::netwait::Waker,
    stop: Arc<AtomicBool>,
) -> Option<std::sync::mpsc::Receiver<Vec<i16>>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<bool>();
    std::thread::spawn(move || {
        let mut src: Box<dyn mic::MicSource> = match kind {
            MicKind::Synth => Box::new(mic::SynthMic::new()),
            #[cfg(windows)]
            MicKind::Real => match mic::WinmmMic::open() {
                Ok(m) => {
                    debug!(
                        "mic open (WinMM waveIn, 48 kHz mono, {} x 20 ms buffers)",
                        ovapp::winaudio::MIC_BUFFERS
                    );
                    Box::new(m)
                }
                Err(e) => {
                    warn!("mic unavailable, audio disabled: {e}");
                    let _ = ready_tx.send(false);
                    return;
                }
            },
            #[cfg(windows)]
            MicKind::System => match sysaudio::LoopbackSource::open() {
                Ok(m) => {
                    debug!("system audio open ({})", m.description);
                    Box::new(m)
                }
                Err(e) => {
                    warn!("system audio unavailable, audio disabled: {e}");
                    let _ = ready_tx.send(false);
                    return;
                }
            },
            #[cfg(not(windows))]
            MicKind::System => {
                warn!("no system-audio capture on this platform");
                let _ = ready_tx.send(false);
                return;
            }
            #[cfg(not(windows))]
            MicKind::Real => {
                warn!("no microphone support on this platform");
                let _ = ready_tx.send(false);
                return;
            }
        };
        let _ = ready_tx.send(true);
        ovapp::audio::run_mic_loop(src.as_mut(), &stop, |pcm| {
            if tx.send(pcm).is_err() {
                return false; // session gone
            }
            waker.wake();
            true
        });
    });
    if ready_rx.recv().unwrap_or(false) {
        debug!("audio streaming on (10 ms Opus packets to a CAP_AUDIO10 viewer, 20 ms otherwise)");
        Some(rx)
    } else {
        None
    }
}

/// FRAME_DATA chunk size on the wire (see [`ovapp::sendq::DEFAULT_CHUNK`]).
const FRAME_CHUNK: usize = ovapp::sendq::DEFAULT_CHUNK;

/// The host-side screen session (Phase 2 split):
///
/// - a **capture thread** ([`capture_loop`]) grabs, change-detects and
///   compresses frames into a single-slot latest-frame mailbox;
/// - a **mic thread** ([`spawn_mic`]) produces AUDIO payloads;
/// - this **session thread** owns the [`TerminalSession`]: it blocks on the
///   socket (or a wake from the other threads), injects input the moment it
///   is decoded, and sends with priority — control first (PONG, PING,
///   cursor, audio, clipboard, chat echo, monitor list), frame records one
///   at a time in between, at most one frame in flight.
///
/// Exits cleanly when the peer disconnects or goes silent for
/// [`SESSION_TIMEOUT`].
#[allow(clippy::too_many_arguments)]
fn serve_screen(
    mut term: TerminalSession,
    region: Option<(u32, u32)>,
    fps: u32,
    mic: Option<MicKind>,
    audio_playback: bool,
    clipboard: bool,
    fx: Option<Arc<Mutex<desktop_fx::DesktopFx>>>,
    data_dir: &Path,
) -> Result<(), String> {
    let mut hooks = HostHooks::new();
    hooks.fx = fx;
    let vs = VirtualScreen::query();
    let input: SharedInput = Arc::new(Mutex::new(input::InputController::new(
        input_backend(vs),
        Region {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        },
        vs,
    )));
    // In screen mode, POINTER/KEY events inject real input mapped through
    // the captured region (re-mapped by the capture thread on retarget).
    hooks.input = Some(Arc::clone(&input));
    if audio_playback {
        hooks.enable_speaker();
        debug!("speaker playback on (viewer's mic, 20 ms ticks)");
    }
    if clipboard {
        hooks.enable_clipboard();
    }
    hooks.register(&mut term);

    let wake = ovapp::netwait::Wakeup::with_sockets(term.raw_socket(), &term.extra_sockets())
        .map_err(|e| format!("wait setup failed: {e}"))?;
    let frame_waker = wake.waker();
    let mailbox = Arc::new(ovapp::sendq::FrameMailbox::new(Some(Box::new(move || {
        frame_waker.wake()
    }))));
    let ctl = Arc::new(CaptureCtl {
        stop: AtomicBool::new(false),
        paused: AtomicBool::new(false),
        target_fps: Arc::clone(&hooks.target_fps),
        rtt_us: Arc::clone(&hooks.rtt_us),
        drain_us: AtomicU64::new(0),
        link_eta_us: AtomicU64::new(0),
        pending_region: Arc::clone(&hooks.pending_region),
        input,
        queue_delay_us: AtomicU64::new(u64::MAX),
        origin: Mutex::new((0, 0)),
        monitors_dirty: AtomicBool::new(false),
        peer_caps: Arc::clone(&hooks.peer_caps),
        keyframe: Arc::clone(&hooks.keyframe),
        video_idr: Arc::clone(&hooks.video_idr),
        link_rate: AtomicU64::new(0),
    });
    // The GDI source lives on the capture thread (its DCs/DIB stay on the
    // thread that created them); init errors come back before streaming.
    let capture = {
        let ctl = Arc::clone(&ctl);
        let mailbox = Arc::clone(&mailbox);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let handle = std::thread::Builder::new()
            .name("ovhost-capture".into())
            .spawn(move || {
                let gdi = match region {
                    Some((w, h)) => GdiScreen::new(0, 0, w, h),
                    None => GdiScreen::full_primary(),
                };
                let gdi = match gdi {
                    Ok(g) => g,
                    Err(e) => {
                        let _ = ready_tx.send(Err(format!("screen capture init failed: {e}")));
                        return;
                    }
                };
                // DXGI Desktop Duplication on the same region when
                // available (Phase 3), GDI otherwise.
                let mut src = capture::ScreenSource::new(gdi, !FORCE_GDI.load(Ordering::Relaxed));
                match TEST_PATTERN.load(Ordering::Relaxed) {
                    1 => src.set_pattern(capture::PatternKind::Video),
                    2 => src.set_pattern(capture::PatternKind::Scroll),
                    3 => src.set_pattern(capture::PatternKind::Photo),
                    _ => {}
                }
                if let Ok(mut o) = ctl.origin.lock() {
                    *o = src.origin();
                }
                if let Ok(mut input) = ctl.input.lock() {
                    input.set_region(Region {
                        x: src.origin().0,
                        y: src.origin().1,
                        w: src.size().0,
                        h: src.size().1,
                    });
                }
                let _ = ready_tx.send(Ok(()));
                capture_loop(src, fps, ctl, mailbox);
            })
            .map_err(|e| format!("capture thread: {e}"))?;
        ready_rx
            .recv()
            .map_err(|_| "capture thread died during init".to_owned())??;
        handle
    };
    let mic_stop = Arc::new(AtomicBool::new(false));
    let mic_rx = mic.and_then(|kind| spawn_mic(kind, wake.waker(), Arc::clone(&mic_stop)));

    let result = screen_session_loop(
        &mut term,
        &hooks,
        &ctl,
        &mailbox,
        &wake,
        mic_rx.as_ref(),
        data_dir,
    );
    if term.media_stats().is_some() {
        debug!("{}", term.media_summary());
    }

    // Session over: nothing the viewer pressed may stay held on the host.
    if let Ok(mut input) = ctl.input.lock() {
        if let Some(n) = input.release_all().filter(|n| *n > 0) {
            debug!("session ended; released {n} held key(s)/button(s)");
        }
    }
    ctl.stop.store(true, Ordering::Relaxed);
    mic_stop.store(true, Ordering::Relaxed);
    let _ = capture.join();
    result
}

/// The screen session thread's loop (see [`serve_screen`]).
fn screen_session_loop(
    term: &mut TerminalSession,
    hooks: &HostHooks,
    ctl: &CaptureCtl,
    mailbox: &ovapp::sendq::FrameMailbox,
    wake: &ovapp::netwait::Wakeup,
    mic_rx: Option<&std::sync::mpsc::Receiver<Vec<i16>>>,
    data_dir: &Path,
) -> Result<(), String> {
    let mut sender = ovapp::sendq::PrioritySender::new(FRAME_CHUNK);
    // Monitors (v0.18): tell the viewer what it can pick from before the
    // first frame flies.
    let monitors = enumerate_monitors();
    sender.push_control(Command::MonitorsInfo {
        monitors: monitors_info(&monitors),
    });
    let mut cursor_tr = cursor::CursorTracker::default();
    // Liveness (v0.16): keepalive PINGs keep an idle (static) screen alive
    // on the wire and measure the RTT the auto-quality rule uses.
    let mut keepalive = Keepalive::new(KEEPALIVE_INTERVAL, Instant::now());
    let mut peer = PeerState::Alive;
    let mut clip_seq = clipboard_os::clip_sequence();
    let mut next_clip = Instant::now();
    let mut next_cursor = Instant::now();
    let mut cursor_moved_at: Option<Instant> = None;
    let mut audio_enc = ovapp::audio::AudioEncoder::new();
    let mut next_audio_log: u64 = 250; // first line after ~5 s
    let mut pacing_log = Instant::now();
    let mut media_logged = false;
    if NO_MEDIA.load(Ordering::Relaxed) {
        // A/B switch for benches: no media channel / v2 congestion control.
        term.set_local_caps(term.local_caps() & !ovapp::terminal::CAP_MEDIA);
        info!("media channel disabled (--no-media)");
    }
    if NO_PACING.load(Ordering::Relaxed) {
        // A/B switch for benches: the pre-Phase-4 unpaced sender.
        sender.pacer_mut().set_enabled(false);
        info!("link pacing disabled (--no-pacing)");
    }
    loop {
        // Inbound first: input is injected by the hook as it is decoded,
        // PING is answered inline.
        if let Err(e) = term.pump() {
            // The path events of this last pump say why (demotion reason).
            log_direct(term);
            session_over(&e.to_string());
            return Ok(());
        }
        log_direct(term);
        peer = check_peer(hooks, peer);
        match peer {
            PeerState::Gone => {
                let _ = term.close();
                return Ok(());
            }
            PeerState::Dead => ctl.paused.store(true, Ordering::Relaxed),
            PeerState::Alive => ctl.paused.store(false, Ordering::Relaxed),
        }
        let now = Instant::now();
        if let Some(seq) = keepalive.tick(now) {
            sender.push_control(Command::Ping(seq));
        }
        absorb_pongs_with(hooks, &mut keepalive, |seq| sender.on_pong(seq, now));
        if pacing_log.elapsed() >= METRICS_EVERY {
            pacing_log = Instant::now();
            log_pacing(&sender.pacer().stats());
            if term.media_stats().is_some() {
                debug!("{}", term.media_summary());
            }
        }
        if !media_logged && term.media_ready() {
            media_logged = true;
            debug!(
                "media channel open (video FEC + Opus over unreliable datagrams, delay-based CC)"
            );
        }
        if ctl.monitors_dirty.swap(false, Ordering::Relaxed) {
            sender.push_control(Command::MonitorsInfo {
                monitors: monitors_info(&monitors),
            });
        }
        for payload in hooks.drain() {
            sender.push_control(Command::Type {
                key: 0,
                mods: 0,
                payload,
            });
        }
        drain_received_files(term, data_dir);
        if now >= next_clip {
            next_clip = now + CLIPBOARD_POLL;
            if let Some(text) = clipboard_poll(&hooks.clipboard, &mut clip_seq) {
                sender.push_control(ovapp::clipboard::clipboard_command(&text));
            }
        }
        // Cursor overlay (v0.15): poll + throttle independent of frames.
        if now >= next_cursor && peer == PeerState::Alive {
            // Poll faster while the cursor is moving, so the 120 Hz
            // position cap is actually reachable.
            let moving = cursor_moved_at
                .is_some_and(|t| now.saturating_duration_since(t) < CURSOR_ACTIVE_FOR);
            next_cursor = now
                + if moving {
                    CURSOR_POLL_ACTIVE
                } else {
                    CURSOR_POLL
                };
            if let Some(pc) = cursor::poll() {
                let origin = ctl.origin.lock().map(|o| *o).unwrap_or((0, 0));
                let out = cursor_tr.update(
                    now,
                    Some((pc.x - origin.0, pc.y - origin.1)),
                    Some(pc.handle),
                    cursor::render_shape,
                );
                if let Some((x, y)) = out.pos {
                    cursor_moved_at = Some(now);
                    sender.push_control(Command::Cursor { x, y });
                }
                if let Some(cmd) = out.shape {
                    sender.push_control(cmd);
                }
            }
        }
        if let Some(rx) = mic_rx {
            while let Ok(pcm) = rx.try_recv() {
                if peer == PeerState::Alive {
                    // Opus to a CAP_OPUS viewer, raw PCM to an old one.
                    let caps = ctl.peer_caps.load(Ordering::Relaxed);
                    // Wave 1: 10 ms packets (FEC on) to a CAP_AUDIO10 viewer.
                    for payload in audio_enc.encode_ticks(&pcm, caps) {
                        // Wave 3: on the media channel when it is open (a
                        // lost packet is concealed, nothing stalls).
                        if term.media_ready() {
                            if let Err(e) = term.send_audio(payload) {
                                session_over(&format!("send failed: {e}"));
                                return Ok(());
                            }
                        } else {
                            sender.push_control(Command::Audio(payload));
                        }
                    }
                }
            }
            // One bitrate line every ~30 s of audio.
            if audio_enc.ticks >= next_audio_log {
                next_audio_log = audio_enc.ticks + 3000;
                debug!(
                    "audio {:.1} kbps over {} ticks ({})",
                    audio_enc.bitrate() / 1000.0,
                    audio_enc.ticks,
                    if ctl.peer_caps.load(Ordering::Relaxed) & ovapp::terminal::CAP_OPUS != 0 {
                        "opus"
                    } else {
                        "raw s16le"
                    }
                );
            }
        }
        // Send: all control, then one frame record; loop back to the
        // socket before the next chunk so input never waits on a frame.
        if let Err(e) = sender.flush(term, mailbox, 1) {
            session_over(&format!("send failed: {e}"));
            return Ok(());
        }
        if let Some(d) = sender.meter().per_frame() {
            ctl.drain_us.store(d.as_micros() as u64, Ordering::Relaxed);
        }
        ctl.link_eta_us
            .store(sender.inflight_eta().as_micros() as u64, Ordering::Relaxed);
        // Wave 3: on the direct v2 path the delay-based controller's target
        // (net of the FEC share) is the link rate; elsewhere the pacer's.
        // (The H.264 budget is 70 % of this, `target_bitrate`: headroom for
        // tiles, audio and the controller's probing.)
        let rate = match term.transport_rate() {
            Some(r) => (r / (1.0 + term.media_fec_ratio())) as u64,
            None => sender.pacer().stats().rate.map_or(0, |r| r as u64),
        };
        ctl.link_rate.store(rate, Ordering::Relaxed);
        let qd = match term.transport_queue_delay() {
            Some(d) => Some(d),
            None => sender.pacer().queue_delay(Instant::now()),
        };
        ctl.queue_delay_us.store(
            qd.map_or(u64::MAX, |d| d.as_micros() as u64),
            Ordering::Relaxed,
        );
        // Input v2: a viewer that vanished mid-drag must not leave a button
        // or modifier held on the host.
        if let Ok(mut input) = ctl.input.lock() {
            let n = input.tick(Instant::now());
            if n > 0 {
                debug!(
                    "released {n} stuck key(s)/button(s) after {:?} without input",
                    input::STUCK_RELEASE_AFTER
                );
            }
        }
        // Idle, or only frame data left and the link is still draining the
        // previous chunk: block until the socket (data in, or send space
        // back), a new frame / mic tick, or the next cursor poll.
        // Phase 4: frame data held back by the pacer's window counts as
        // backlogged too; the wait then ends when the window reopens or the
        // next probe is due (PONGs arriving wake us via the socket).
        let backlogged =
            sender.pending_control() == 0 && (sender.link_backlogged(term) || sender.paced(term));
        if !sender.has_work(mailbox) || backlogged {
            let until_cursor = next_cursor.saturating_duration_since(Instant::now());
            let cap = if wake.watches_socket() {
                Duration::from_millis(50)
            } else {
                UNWATCHED_WAIT
            };
            let mut wait = until_cursor.max(Duration::from_millis(1)).min(cap);
            if let Some(d) = sender.pacing_deadline(term, mailbox) {
                wait = wait.min(d.max(Duration::from_millis(1)));
            }
            // UDP retransmission timers (Phase 4).
            if let Some(d) = term.transport_deadline() {
                wait = wait.min(d.max(Duration::from_millis(1)));
            }
            wake.wait(wait);
        }
    }
}

/// Section 5: log direct-path events (promotion, demotion, probes) and
/// report path changes on the status channel.
fn log_direct(term: &mut TerminalSession) {
    for e in term.take_direct_events() {
        report_direct_event(&e);
    }
}

/// Classify one direct-path event string from `ovtransport`: `... active
/// ...` = promoted, `... back on relay` = demoted; anything else (probe
/// results, rebinds) is debug detail.
fn report_direct_event(e: &str) {
    let state = if e.contains("back on relay") {
        Some(DirectState::Fallback)
    } else if e.contains(" active") {
        Some(DirectState::Active)
    } else {
        None
    };
    match state {
        Some(state) => {
            info!("direct: {e}");
            status::emit(&StatusEvent::DirectPath {
                state,
                port: None,
                addr: None,
                detail: Some(e.to_owned()),
            });
        }
        None if e.starts_with("relay gone") => warn!("direct: {e}"),
        None => debug!("direct: {e}"),
    }
}

/// One-line link-pacing summary (Phase 4): the delivery-rate / min-RTT
/// model and how often frame data was held back to keep queues short.
fn log_pacing(st: &ovapp::pacing::PacerStats) {
    let ms = |d: Option<Duration>| d.map_or(-1.0, |d| d.as_secs_f64() * 1000.0);
    debug!(
        "link pacing: rate {:.2} Mbps, min RTT {:.1} ms, last RTT {:.1} ms, window {} KB, \
         {} hold(s), probes {}/{} acked",
        st.rate.unwrap_or(0.0) * 8e-6,
        ms(st.min_rtt),
        ms(st.rtt),
        st.window.map_or(0, |w| w / 1024),
        st.holds,
        st.probes_acked,
        st.probes_sent
    );
}

/// Idle capture rate for a static screen (frames per second).
const IDLE_FPS: u32 = 4;
/// How often the host logs capture metrics while streaming.
const METRICS_EVERY: Duration = Duration::from_secs(30);

/// One-line capture metrics: frames sent vs idle captures skipped, plus the
/// governor's current rate — makes the static-screen savings observable.
fn log_screen_metrics(sent: u64, skipped: u64, stats: &RateStats) {
    let ms = stats.interval.as_millis().max(1) as f64;
    let fps_now = 1000.0 / ms;
    debug!(
        "screen capture — sent {sent}, skipped {skipped} unchanged, \
         capturing at ~{fps_now:.1} fps{}",
        if stats.at_idle_rate {
            " (idle rate)"
        } else {
            ""
        }
    );
}

/// Map the enumerated monitors onto the wire-level `MonitorInfo` records
/// (position = index, exactly what the decoder reconstructs).
fn monitors_info(monitors: &[Monitor]) -> Vec<ovapp::terminal::MonitorInfo> {
    monitors
        .iter()
        .map(|m| ovapp::terminal::MonitorInfo {
            x: m.x,
            y: m.y,
            w: m.w,
            h: m.h,
            is_primary: m.is_primary,
        })
        .collect()
}

/// Decode a TYPE chat payload as a string: NUL-trim (mirrors
/// `ovapp::chat::line_from_payload`) then UTF-8 lossy so bad bytes never
/// panic the host.
fn line_str(payload: &[u8]) -> String {
    let end = payload
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(payload.len());
    String::from_utf8_lossy(&payload[..end]).into_owned()
}

/// File name of the persisted unattended identity.
const IDENTITY_FILE: &str = "ovhost-unattended.id";

/// Unattended identity path (v0.22): the host's identity must be STABLE
/// (viewers dial it by device id), so it is persisted to
/// `<data dir>\ovhost-unattended.id` and reused across runs. Older releases
/// kept it next to the exe; that file is copied over once so existing
/// device IDs survive the upgrade.
fn load_or_create_unattended_identity(data_dir: &Path) -> Result<Identity, String> {
    let path = data_dir.join(IDENTITY_FILE);
    if let Some(old) = ovcommon::paths::exe_dir().map(|d| d.join(IDENTITY_FILE)) {
        match ovcommon::paths::migrate_file(&old, &path) {
            Ok(true) => info!(
                "migrated the device identity from {} to {}",
                old.display(),
                path.display()
            ),
            Ok(false) => {}
            Err(e) => warn!("could not migrate {}: {e}", old.display()),
        }
    }
    match Identity::load(&path) {
        Ok(id) => Ok(id),
        Err(_) => {
            let id = Identity::generate();
            id.save(&path)
                .map_err(|e| format!("cannot save unattended identity: {e}"))?;
            Ok(id)
        }
    }
}

/// The stable unattended device id: the fingerprint's low bytes (same recipe
/// as the pairing path, but derived from the persisted identity).
fn unattended_device_id(me: &Identity) -> u64 {
    let fp = me.fingerprint();
    u64::from_be_bytes([0, 0, 0, fp[0], fp[1], fp[2], fp[3], fp[4]])
}

/// Parse a `WxH` region spec such as `640x360`.
fn parse_region(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.split_once('x')?;
    let w = w.parse::<u32>().ok()?;
    let h = h.parse::<u32>().ok()?;
    (w > 0 && h > 0).then_some((w, h))
}

#[cfg(test)]
mod tests {
    #[test]
    fn phone_logs_are_recognized_strictly() {
        let data = std::path::Path::new(r"C:\data");
        let d = super::phone_log_dir(data, "ovlog-ios-1a2b3c4d-20261113-101500.log")
            .expect("a phone log");
        assert_eq!(d, data.join("logs").join("phone"));
        for bad in [
            "photo.jpg",
            "ovlog-ios-x.txt",
            "ovlog-ios-a b.log",
            "xovlog-ios-1.log",
            "ovlog-ios-..\\x.log",
        ] {
            assert!(super::phone_log_dir(data, bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn auto_rate_rises_to_60_only_when_the_link_carries_it() {
        use super::link_base_fps;
        assert_eq!(link_base_fps(10, 60, 0, 50_000.0), 10, "no rate yet");
        assert_eq!(link_base_fps(10, 60, 1_000_000, 0.0), 10, "no updates yet");
        // 1 MB/s, 40 KB updates: half the link = 12 updates/s.
        assert_eq!(link_base_fps(10, 60, 1_000_000, 40_000.0), 10);
        // 4 MB/s, 40 KB: 50/s -> 30.
        assert_eq!(link_base_fps(10, 60, 4_000_000, 40_000.0), 30);
        // 20 MB/s, 40 KB: 250/s -> 60.
        assert_eq!(link_base_fps(10, 60, 20_000_000, 40_000.0), 60);
        assert_eq!(
            link_base_fps(10, 45, 20_000_000, 40_000.0),
            45,
            "--max-fps caps"
        );
        assert_eq!(
            link_base_fps(30, 20, 20_000_000, 1.0),
            30,
            "never below --fps"
        );
    }

    use super::*;
    use ovapp::adapt::{frame_changed, should_send};

    #[test]
    fn silent_peer_goes_dead_then_ends_the_session() {
        assert_eq!(peer_state(Duration::from_secs(1)), PeerState::Alive);
        assert_eq!(
            peer_state(DEAD_AFTER + Duration::from_millis(1)),
            PeerState::Dead
        );
        assert_eq!(
            peer_state(SESSION_TIMEOUT - Duration::from_millis(1)),
            PeerState::Dead
        );
        assert_eq!(
            peer_state(SESSION_TIMEOUT + Duration::from_millis(1)),
            PeerState::Gone
        );
    }

    #[test]
    fn capture_waits_for_the_link_to_free_up() {
        let ms = Duration::from_millis;
        // Link free, nothing queued: capture now.
        assert_eq!(capture_hold(Duration::ZERO, ms(20), false), None);
        // In-flight frame finishes within one capture's time: start now so
        // the new frame is ready as the link frees up.
        assert_eq!(capture_hold(ms(15), ms(20), false), None);
        // Link busy for a while: hold (in short slices).
        assert_eq!(capture_hold(ms(30), ms(20), false), Some(ms(10)));
        assert_eq!(capture_hold(ms(300), ms(20), false), Some(ms(20)));
        // A captured frame is already waiting: don't compress another.
        assert_eq!(capture_hold(Duration::ZERO, ms(20), true), Some(ms(1)));
    }

    /// The relay times an unclaimed park out (`XT`) — the host must re-park
    /// at once (not fail, not back off) so a viewer dialing later still
    /// finds it; here the park window is 300 ms and the viewer shows up
    /// after ~1 s, i.e. after several XT cycles.
    #[test]
    fn host_reparks_after_relay_park_timeout() {
        let mut relay = ovrelay::Relay::bind("127.0.0.1:0".parse().unwrap()).expect("bind");
        relay.set_timeout(Duration::from_millis(300));
        let addr = relay.local_addr();
        std::thread::spawn(move || relay.serve(None));
        let sid = 0x5EED_0001;
        let host = std::thread::spawn(move || park_until_attached(addr, sid));
        std::thread::sleep(Duration::from_millis(1100));
        let mut viewer =
            RelayTunnelCarrier::connect(addr, sid, TunnelRole::Initiator).expect("viewer connect");
        assert!(viewer
            .wait_attached(Duration::from_secs(10))
            .expect("viewer attach"));
        let mut hc = host.join().expect("host thread").expect("host parked");
        assert!(hc.is_attached());
        // The tunnel is live end to end.
        viewer.send(b"hello").unwrap();
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while got.is_empty() && Instant::now() < deadline {
            hc.poll_recv(&mut |d| got.push(d.to_vec())).unwrap();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(got, vec![b"hello".to_vec()]);
    }

    /// An in-memory "screen": a fixed BGRA frame that never changes.
    struct StaticScreen {
        px: Vec<u8>,
        dims: (u32, u32),
    }

    impl StaticScreen {
        fn capture(&self) -> (Vec<u8>, u32, u32) {
            (self.px.clone(), self.dims.0, self.dims.1)
        }
    }

    /// Replays the `serve_screen` capture-decision core (no GDI, no wire):
    /// governor-gated captures, `should_send` decision, sent/skipped
    /// accounting. Returns the counters so the test can assert a static
    /// screen sends exactly one frame and eases to the idle rate.
    fn run_static_loop(screen: &StaticScreen, ticks: usize) -> (u64, u64, RateStats) {
        let mut gov = RateGovernor::new(10, 4);
        // Simulated clock: each pump tick advances 5 ms, matching the real
        // loop's sleep, so the test is deterministic and instant.
        let mut now = Instant::now();
        let mut last_capture = now;
        let mut last_px: Option<Vec<u8>> = None;
        let mut last_dims = (0u32, 0u32);
        let mut sent = 0u64;
        let mut skipped = 0u64;
        // Simulate the 5 ms pump sleep per tick; the governor gate decides
        // whether a capture actually happens.
        for _ in 0..ticks {
            now += Duration::from_millis(5);
            if now.saturating_duration_since(last_capture) >= gov.interval() {
                last_capture = now;
                let (px, w, h) = screen.capture();
                let dims = (w, h);
                if should_send(
                    last_px.as_deref().map(|p| (p, last_dims)),
                    dims != last_dims,
                    &px,
                ) {
                    last_px = Some(px);
                    last_dims = dims;
                    sent += 1;
                    let _ = gov.on_change();
                } else {
                    skipped += 1;
                    let _ = gov.on_idle();
                }
            }
        }
        (sent, skipped, gov.stats())
    }

    #[test]
    fn static_screen_sends_once_then_skips_at_idle_rate() {
        let screen = StaticScreen {
            px: vec![0x40; 320 * 240 * 4],
            dims: (320, 240),
        };
        // ~3 s of pump ticks at 5 ms: at 10 fps base the first capture
        // sends; everything after is unchanged and skipped as the governor
        // eases toward 4 fps.
        let (sent, skipped, stats) = run_static_loop(&screen, 600);
        assert_eq!(sent, 1, "a static screen sends exactly one frame");
        assert!(skipped > 0, "unchanged captures must be skipped");
        assert!(stats.at_idle_rate, "governor must reach the idle rate");
        assert!(
            stats.interval >= Duration::from_millis(250),
            "idle interval = 1/4 fps"
        );
        // Sanity: the skipped frame really is identical to the sent one.
        let (px, _, _) = screen.capture();
        assert!(!frame_changed(&screen.px, &px));
    }

    #[test]
    fn metrics_log_formats() {
        let stats = RateStats {
            idle_streak: 480,
            interval: Duration::from_millis(250),
            at_idle_rate: true,
        };
        // No panic + mentions both counters and the idle rate.
        log_screen_metrics(12, 480, &stats);
    }

    /// Phase 3: a viewer's CAPS reaches the capture thread's atomic (and the
    /// pump answers with the host's CAPS); a KEYFRAME_REQUEST raises the
    /// keyframe flag.
    #[test]
    fn caps_and_keyframe_requests_reach_the_capture_thread() {
        use ovapp::terminal::LOCAL_CAPS;
        let (mut view, mut host) = term_pair();
        let hooks = HostHooks::new();
        hooks.register(&mut host);
        view.announce_caps().unwrap();
        view.send(&Command::KeyframeRequest { have_seq: 5 })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while (view.peer_caps().is_none() || !hooks.keyframe.load(Ordering::Relaxed))
            && Instant::now() < deadline
        {
            host.pump().unwrap();
            view.pump().unwrap();
        }
        // Phase 4: desktop builds also advertise CAP_OPUS (libopus); Wave 1
        // adds CAP_AUDIO10 (10 ms Opus packets decode fine); Wave 2 the
        // viewer-side lossy / tile-cache decode bits.
        let ours = LOCAL_CAPS
            | ovapp::terminal::CAP_OPUS
            | ovapp::terminal::CAP_AUDIO10
            | ovapp::terminal::VIEWER_W2_CAPS;
        assert_eq!(hooks.peer_caps.load(Ordering::Relaxed), ours);
        assert!(hooks.keyframe.load(Ordering::Relaxed));
        assert_eq!(view.peer_caps(), Some(ours), "host answered with its CAPS");
    }

    /// Phase 5: a viewer with an H.264 decoder advertises CAP_H264 (the
    /// capture thread then may stream video), and its VIDEO_IDR_REQUEST
    /// reaches the capture thread.
    #[test]
    fn h264_caps_and_idr_requests_reach_the_capture_thread() {
        struct NoDecode;
        impl ovapp::video::VideoDecode for NoDecode {
            fn decode(
                &mut self,
                _a: &[u8],
                _w: u32,
                _h: u32,
                _o: &mut Vec<u8>,
            ) -> Result<bool, String> {
                Ok(false)
            }
        }
        let (mut view, mut host) = term_pair();
        let hooks = HostHooks::new();
        hooks.register(&mut host);
        view.set_video_decoder(Box::new(NoDecode));
        view.announce_caps().unwrap();
        view.send(&Command::VideoIdrRequest { have_seq: 9 })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !hooks.video_idr.load(Ordering::Relaxed) && Instant::now() < deadline {
            host.pump().unwrap();
            view.pump().unwrap();
        }
        assert!(hooks.peer_caps.load(Ordering::Relaxed) & ovapp::terminal::CAP_H264 != 0);
        assert!(hooks.video_idr.load(Ordering::Relaxed));
        assert!(
            !hooks.keyframe.load(Ordering::Relaxed),
            "an IDR request is not a tile keyframe"
        );
    }

    /// Input v2 end to end over a real session: the host answers CAPS with
    /// CAP_INPUT2, and both a legacy viewer's POINTER/KEY and the new
    /// messages reach the controller; RELEASE_ALL releases what was held.
    #[test]
    fn input_commands_reach_the_controller_old_and_new() {
        use ovapp::terminal::{MouseButton, PointerAction, PointerKind, CAP_INPUT2, KEY_DOWN};
        struct Rec(Arc<Mutex<Vec<input::InputOp>>>);
        impl input::InputBackend for Rec {
            fn send(&mut self, ops: &[input::InputOp]) -> bool {
                self.0.lock().unwrap().extend_from_slice(ops);
                true
            }
            fn cursor_pos(&mut self) -> Option<(i32, i32)> {
                Some((10, 10))
            }
            fn send_sas(&mut self) -> Result<&'static str, String> {
                Err("test".into())
            }
        }
        let (mut view, mut host) = term_pair();
        let ops: Arc<Mutex<Vec<input::InputOp>>> = Arc::default();
        let mut hooks = HostHooks::new();
        let region = Region {
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
        };
        let vs = VirtualScreen {
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
        };
        hooks.input = Some(Arc::new(Mutex::new(input::InputController::new(
            Box::new(Rec(Arc::clone(&ops))),
            region,
            vs,
        ))));
        hooks.register(&mut host);
        view.announce_caps().unwrap();
        let cmds = [
            Command::Pointer {
                kind: PointerKind::Down,
                x: 0,
                y: 0,
            },
            Command::Pointer {
                kind: PointerKind::Up,
                x: 0,
                y: 0,
            },
            Command::Key {
                vk: 0x41,
                down: true,
            },
            Command::Key {
                vk: 0x41,
                down: false,
            },
            Command::PointerEx {
                action: PointerAction::Down,
                button: MouseButton::Right,
                flags: 0,
                x: 0,
                y: 0,
            },
            Command::Wheel { dy: -120, dx: 0 },
            Command::Text("é".into()),
            Command::KeyEx {
                vk: 0xA2,
                scan: 0x1D,
                flags: KEY_DOWN,
            },
            Command::ReleaseAll,
        ];
        for c in &cmds {
            view.send(c).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while (view.peer_caps().is_none() || ops.lock().unwrap().len() < 13)
            && Instant::now() < deadline
        {
            host.pump().unwrap();
            view.pump().unwrap();
        }
        assert!(
            view.peer_caps().unwrap() & CAP_INPUT2 != 0,
            "host advertises input v2"
        );
        let ops = ops.lock().unwrap().clone();
        use input::InputOp as O;
        assert!(ops.contains(&O::Button {
            button: MouseButton::Left,
            down: true
        }));
        assert!(ops.contains(&O::Button {
            button: MouseButton::Left,
            down: false
        }));
        assert!(ops.contains(&O::Button {
            button: MouseButton::Right,
            down: true
        }));
        assert!(ops.contains(&O::Wheel(-120)));
        assert!(ops.contains(&O::Unicode {
            unit: 0xE9,
            up: false
        }));
        // RELEASE_ALL: right button up, then left ctrl up — the last two.
        assert_eq!(
            &ops[ops.len() - 2..],
            &[
                O::Button {
                    button: MouseButton::Right,
                    down: false
                },
                O::Key {
                    vk: 0xA2,
                    scan: 0x1D,
                    flags: input::keyflags::KEYUP
                }
            ]
        );
    }

    /// (viewer, host) terminal sessions over an in-memory carrier pair.
    fn term_pair() -> (TerminalSession, TerminalSession) {
        use ovcore::handshake::{Auth, Handshake, HandshakeEvent, Role};
        use ovtransport::InMemoryCarrier;
        let (id_a, id_b) = (Identity::generate(), Identity::generate());
        let (mut cc, mut sc) = InMemoryCarrier::pair();
        let code = id_b.pairing_code();
        let mut client = Handshake::new(Role::Client, 0x11, &id_a, Auth::PairingCode(code.clone()));
        client.expect_fingerprint(id_b.fingerprint());
        let mut server = Handshake::new(Role::Server, 0x22, &id_b, Auth::PairingCode(code));
        for _ in 0..200 {
            let _ = cc.poll_recv(&mut |d| {
                let _ = client.feed(d);
            });
            let _ = sc.poll_recv(&mut |d| {
                let _ = server.feed(d);
            });
            for ev in client.take_events() {
                let HandshakeEvent::Sent(d) = ev;
                let _ = cc.send(&d);
            }
            for ev in server.take_events() {
                let HandshakeEvent::Sent(d) = ev;
                let _ = sc.send(&d);
            }
            if client.is_established() && server.is_established() {
                break;
            }
        }
        (
            TerminalSession::new(client.take_session().expect("client"), cc),
            TerminalSession::new(server.take_session().expect("server"), sc),
        )
    }

    /// A QUALITY_SET command sent by a viewer over a real (in-memory) session
    /// reaches the host hook and updates the shared target-fps atomic.
    #[test]
    fn quality_set_command_updates_the_target_atomic() {
        use ovapp::terminal::TerminalSession;
        use ovcore::identity::Identity;
        use ovtransport::InMemoryCarrier;

        let (id_a, id_b) = (Identity::generate(), Identity::generate());
        // Reuse the terminal crate's in-memory handshake helper shape: build
        // two sessions directly via the handshake over in-memory carriers.
        let (mut cc, mut sc) = InMemoryCarrier::pair();
        let code = id_b.pairing_code();
        let mut client = ovcore::handshake::Handshake::new(
            ovcore::handshake::Role::Client,
            0x11,
            &id_a,
            ovcore::handshake::Auth::PairingCode(code.clone()),
        );
        client.expect_fingerprint(id_b.fingerprint());
        let mut server = ovcore::handshake::Handshake::new(
            ovcore::handshake::Role::Server,
            0x22,
            &id_b,
            ovcore::handshake::Auth::PairingCode(code),
        );
        client.expect_device(0x22);
        for _ in 0..200 {
            let _ = cc.poll_recv(&mut |d| {
                let _ = client.feed(d);
            });
            let _ = sc.poll_recv(&mut |d| {
                let _ = server.feed(d);
            });
            for ev in client.take_events() {
                let ovcore::handshake::HandshakeEvent::Sent(d) = ev;
                let _ = cc.send(&d);
            }
            for ev in server.take_events() {
                let ovcore::handshake::HandshakeEvent::Sent(d) = ev;
                let _ = sc.send(&d);
            }
            if client.is_established() && server.is_established() {
                break;
            }
        }
        assert!(client.is_established() && server.is_established());
        let mut view = TerminalSession::new(client.take_session().expect("client session"), cc);
        let mut host = TerminalSession::new(server.take_session().expect("server session"), sc);

        let hooks = HostHooks::new();
        hooks.register(&mut host);

        // AUTO first, then a manual 30 fps: the atomic must track both.
        view.send(&Command::QualitySet { target_fps: 0 })
            .expect("send auto");
        view.send(&Command::QualitySet { target_fps: 30 })
            .expect("send 30");
        let deadline = Instant::now() + Duration::from_secs(5);
        while hooks.target_fps.load(Ordering::Relaxed) != 30 && Instant::now() < deadline {
            host.pump().expect("host pump");
        }
        assert_eq!(
            hooks.target_fps.load(Ordering::Relaxed),
            30,
            "the last QUALITY_SET wins"
        );
    }
}
