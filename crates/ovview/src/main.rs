//! `ovview` — the OpenView desktop viewer: shows a remote host's screen in a
//! native window and sends it local mouse/keyboard input.
//!
//! Three ways to reach a host:
//!
//! - `ovview --connect <device-id>` (PIN in `OPENVIEW_PIN` or `--pin`):
//!   unattended access through the relay — what the GUI runs.
//! - `ovview --code <pairing-code>`: an attended host's one-time code.
//! - `ovview --id <fingerprint-hex>`: LAN discovery (mDNS) + direct UDP.
//!
//! Offline recording tools (no network): `--info`, `--export`, `--play`.
//!
//! Output contract: **stdout** carries machine-readable status events, one
//! JSON object per line ([`ovcommon::status`]; the GUI parses these), except
//! for the offline `--info` / `--export` reports, which print plain text.
//! **stderr** carries human-readable logs (`tracing`): lifecycle lines at
//! the default `info` level, session metrics with `-v`.
//!
//! Received files are saved to `Downloads\OpenView`; recordings (F6) to
//! `%USERPROFILE%\ovrecord` unless `--record-dir` says otherwise.

use std::io::BufRead;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

mod audio;
mod clipboard_os;
mod hud;
mod localinput;
mod mic;
mod window;

use clap::Parser;
use ovapp::liveness::{Keepalive, Liveness, DEAD_AFTER, KEEPALIVE_INTERVAL, SESSION_TIMEOUT};
use ovapp::terminal::{Command, MonitorInfo, TerminalSession};
use ovcommon::status::{self, DirectState, StatusEvent};
use ovcore::handshake::{Auth, Handshake, HandshakeEvent, Role};
use ovcore::identity::Identity;
use ovtransport::{Carrier, RelayTunnelCarrier, TunnelRole, UdpP2pCarrier};
use tracing::{debug, error, info, warn};
use window::CursorBmp;

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
struct Cli {
    /// Unattended access: dial the host with this device ID (needs a PIN).
    #[arg(long, value_name = "DEVICE_ID")]
    connect: Option<u64>,

    /// The host's access PIN for --connect. Prefer the OPENVIEW_PIN
    /// environment variable: other processes can read a command line.
    #[arg(long, env = "OPENVIEW_PIN", hide_env_values = true, value_name = "PIN")]
    pin: Option<String>,

    /// Join an attended host by its one-time pairing code.
    #[arg(long, value_name = "CODE")]
    code: Option<String>,

    /// Join a LAN host by identity fingerprint (mDNS discovery).
    #[arg(long, value_name = "FINGERPRINT")]
    id: Option<String>,

    /// Relay address.
    #[arg(long, value_name = "HOST:PORT", default_value = DEFAULT_RELAY)]
    relay: String,

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
    verbose: u8,

    /// Log filter, e.g. `debug` or `info,ovtransport=trace` (overrides -v
    /// and the OPENVIEW_LOG environment variable).
    #[arg(long, value_name = "FILTER")]
    log_level: Option<String>,

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

/// Which microphone source `--mic` / `--audio-synth` selected (v0.23).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MicKind {
    /// The real microphone (WinMM `waveIn`, default input device).
    Real,
    /// A 440 Hz synthetic sine — hardware-free demos and tests.
    Synth,
}

/// `--no-direct`: stay on the relay (no direct-path upgrade, Section 5).
static NO_DIRECT: AtomicBool = AtomicBool::new(false);
/// `--rtt-log`: log every keepalive RTT sample with the path it took.
static RTT_LOG: AtomicBool = AtomicBool::new(false);
/// `--ping-ms N`: keepalive PING interval override (RTT measurements).
static PING_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn main() -> ExitCode {
    let cli = Cli::parse();
    ovcommon::logging::init(cli.verbose, cli.log_level.as_deref());
    match run_cli(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e}");
            status::emit(&StatusEvent::Error { message: e });
            ExitCode::FAILURE
        }
    }
}

fn run_cli(cli: Cli) -> Result<(), String> {
    // Offline modes run before any network or identity code.
    if let Some(file) = &cli.info {
        return record_info(file);
    }
    if let Some(file) = &cli.export {
        let out = cli.out.as_deref().ok_or("--export needs --out <dir>")?;
        return record_export(file, out, cli.frame);
    }
    if let Some(file) = &cli.play {
        return record_play(file, cli.loop_play);
    }

    // Wave 3: 1 ms waits (media-channel loss decisions, pacing of our sends).
    ovapp::netwait::high_res_timers();
    NO_H264.store(cli.no_h264, Ordering::Relaxed);
    NO_DIRECT.store(cli.no_direct, Ordering::Relaxed);
    AUDIO_AT_START.store(cli.audio, Ordering::Relaxed);
    RTT_LOG.store(cli.rtt_log, Ordering::Relaxed);
    PING_MS.store(cli.ping_ms.unwrap_or(0), Ordering::Relaxed);
    RUN_FOR_SECS.store(cli.duration.unwrap_or(0), Ordering::Relaxed);
    window::set_renderer(match cli.renderer {
        Renderer::D2d => window::RendererPref::Direct2D,
        Renderer::Gdi => window::RendererPref::Gdi,
    });
    let mic = if cli.mic {
        Some(MicKind::Real)
    } else if cli.audio_synth {
        Some(MicKind::Synth)
    } else {
        None
    };
    let target = match (cli.code, cli.id, cli.connect) {
        (Some(c), None, None) => Target::Relay(c),
        (None, Some(i), None) => Target::Lan(i),
        (None, None, Some(dev)) => Target::Unattended {
            device_id: dev,
            pin: cli
                .pin
                .filter(|p| !p.is_empty())
                .ok_or("--connect needs the host's PIN (OPENVIEW_PIN or --pin)")?,
        },
        _ => return Err("pass exactly one of --connect, --code or --id".into()),
    };
    info!("OpenView viewer {} starting", env!("CARGO_PKG_VERSION"));
    run(
        target,
        &cli.relay,
        cli.record_dir,
        cli.no_window,
        mic,
        !cli.no_clipboard,
        cli.send_file.as_deref(),
    )
}

enum Target {
    /// Pairing code: join through the relay.
    Relay(String),
    /// Fingerprint hex: LAN discovery + direct UDP.
    Lan(String),
    /// Unattended access (v0.22): dial by the host's device id + access PIN.
    Unattended { device_id: u64, pin: String },
}

fn run(
    target: Target,
    relay: &str,
    record_dir: Option<String>,
    no_window: bool,
    mic: Option<MicKind>,
    clipboard: bool,
    send_file: Option<&std::path::Path>,
) -> Result<(), String> {
    let me = Identity::generate();
    debug!("our fingerprint {}", me.fingerprint_pine());

    // Handshake as client. The device ID is the low 24 bits of our
    // fingerprint — stable per identity, unique enough for a demo.
    let fp = me.fingerprint();
    let my_dev = u64::from_be_bytes([0, 0, 0, fp[0], fp[1], fp[2], fp[3], fp[4]]);

    match target {
        Target::Relay(code) => run_relay(
            me, my_dev, code, relay, record_dir, no_window, mic, clipboard, send_file,
        ),
        Target::Lan(id_hex) => run_lan(
            me, my_dev, id_hex, record_dir, no_window, mic, clipboard, send_file,
        ),
        Target::Unattended { device_id, pin } => run_unattended(
            me, my_dev, device_id, &pin, relay, record_dir, no_window, mic, clipboard, send_file,
        ),
    }
}

/// Pairing-code path: dial the relay, tunnel to the host, handshake as client.
#[allow(clippy::too_many_arguments)]
fn run_relay(
    me: Identity,
    my_dev: u64,
    code: String,
    relay: &str,
    record_dir: Option<String>,
    no_window: bool,
    mic: Option<MicKind>,
    clipboard: bool,
    send_file: Option<&std::path::Path>,
) -> Result<(), String> {
    let addr: SocketAddr = relay
        .parse()
        .map_err(|e| format!("bad relay `{relay}`: {e}"))?;
    info!("dialing relay {addr} with pairing code {code}");
    status::emit(&StatusEvent::Connecting {
        relay: addr.to_string(),
        target: code.clone(),
    });
    // The relay session id is the code's first 4 bytes (matches ovhost).
    let session_id = u32::from_be_bytes(
        code.as_bytes()[..4]
            .try_into()
            .map_err(|_| "pairing code shorter than 4 bytes".to_owned())?,
    );
    let mut carrier = dial_relay(addr, session_id, ATTACH_WAIT)?;

    let mut hs = Handshake::new(Role::Client, my_dev, &me, Auth::PairingCode(code));
    let backlog = handshake_over(&mut hs, &mut carrier)?;
    let peer_fp = hs
        .peer()
        .ok_or("handshake established without peer info")?
        .fingerprint;
    viewer_loop(
        me, my_dev, carrier, hs, backlog, peer_fp, record_dir, no_window, mic, clipboard, send_file,
    )
}

/// Fingerprint path: mDNS-browse for the peer, direct UDP handshake.
#[allow(clippy::too_many_arguments)]
fn run_lan(
    me: Identity,
    my_dev: u64,
    id_hex: String,
    record_dir: Option<String>,
    no_window: bool,
    mic: Option<MicKind>,
    clipboard: bool,
    send_file: Option<&std::path::Path>,
) -> Result<(), String> {
    let want = parse_fingerprint(&id_hex)?;
    info!("browsing LAN for fingerprint {}", hex::encode(want));
    let peer = lan_find(&want).ok_or("no LAN peer advertising that fingerprint")?;
    info!("found peer on UDP port {}", peer.udp_port);

    let mut carrier = UdpP2pCarrier::new(
        "127.0.0.1:0".parse().unwrap(),
        SocketAddr::from(([127, 0, 0, 1], peer.udp_port)),
    )
    .map_err(|e| format!("udp bind failed: {e}"))?;

    let mut hs = Handshake::new(Role::Client, my_dev, &me, Auth::DirectDevice);
    hs.expect_fingerprint(want);
    let backlog = handshake_over(&mut hs, &mut carrier)?;
    let peer_fp = hs
        .peer()
        .ok_or("handshake established without peer info")?
        .fingerprint;
    viewer_loop(
        me, my_dev, carrier, hs, backlog, peer_fp, record_dir, no_window, mic, clipboard, send_file,
    )
}

/// Unattended path (v0.22): dial the relay by the host's device id (the relay
/// session id is the low 32 bits, matching ovhost --unattended) and complete
/// the handshake with PIN auth — no pairing, no pre-shared identity.
#[allow(clippy::too_many_arguments)]
fn run_unattended(
    me: Identity,
    my_dev: u64,
    device_id: u64,
    pin: &str,
    relay: &str,
    record_dir: Option<String>,
    no_window: bool,
    mic: Option<MicKind>,
    clipboard: bool,
    send_file: Option<&std::path::Path>,
) -> Result<(), String> {
    let addr: SocketAddr = relay
        .parse()
        .map_err(|e| format!("bad relay `{relay}`: {e}"))?;
    let session_id = (device_id & 0xFFFF_FFFF) as u32;
    info!("dialing relay {addr} for device {device_id} (session {session_id:08x}, PIN auth)");
    status::emit(&StatusEvent::Connecting {
        relay: addr.to_string(),
        target: device_id.to_string(),
    });
    let mut carrier = dial_relay(addr, session_id, ATTACH_WAIT)?;

    let psk = ovcore::pin::pin_psk(pin.as_bytes());
    let mut hs = Handshake::new(Role::Client, my_dev, &me, Auth::Pin(psk));
    // Dialing by device id: pin the peer's device id so we cannot be steered
    // to a different host that happens to share the relay session id.
    hs.expect_device(device_id);
    let backlog = handshake_over(&mut hs, &mut carrier)?;
    let peer_fp = hs
        .peer()
        .ok_or("handshake established without peer info")?
        .fingerprint;
    viewer_loop(
        me, my_dev, carrier, hs, backlog, peer_fp, record_dir, no_window, mic, clipboard, send_file,
    )
}

/// How long a dialing viewer waits for the relay to attach the host: the
/// relay's own park window (30 s) plus slack. A host that is between parks
/// (re-parking after an idle timeout) attaches within this window.
const ATTACH_WAIT: Duration = Duration::from_secs(35);

/// Dial the relay as the INITIATOR and wait (up to `attach_wait`) for the
/// relay's attach confirmation before returning — the handshake timer only
/// starts once the host is actually on the other end.
fn dial_relay(
    addr: SocketAddr,
    session_id: u32,
    attach_wait: Duration,
) -> Result<RelayTunnelCarrier, String> {
    let mut carrier = RelayTunnelCarrier::connect(addr, session_id, TunnelRole::Initiator)
        .map_err(|e| format!("relay connect failed: {e}"))?;
    if !carrier.is_attached() {
        info!("waiting for the host to attach (up to {attach_wait:?})");
    }
    match carrier.wait_attached(attach_wait) {
        Ok(true) => Ok(carrier),
        Ok(false) => Err(format!("host did not attach within {attach_wait:?}")),
        Err(ovtransport::TransportError::Closed) => {
            Err("host not reachable (relay park timed out with no host)".into())
        }
        Err(e) => Err(format!("relay attach failed: {e}")),
    }
}

/// Pump the handshake over a carrier until established (datagram re-send on
/// the client side covers loss; loopback converges on the first pass).
///
/// Returns the datagrams that arrived *after* the handshake completed within
/// the same poll batch (e.g. a record the host sent right behind its READY):
/// they belong to the session and must be handed to it, not fed to the
/// (now established) handshake, which would silently discard them.
fn handshake_over(hs: &mut Handshake, carrier: &mut dyn Carrier) -> Result<Vec<Vec<u8>>, String> {
    for ev in hs.take_events() {
        let HandshakeEvent::Sent(d) = ev;
        carrier
            .send(&d)
            .map_err(|e| format!("handshake send: {e}"))?;
    }
    let mut backlog = Vec::new();
    for _ in 0..200 {
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

/// Longest event wait while idle: keeps the HUD (fps decay, RTT) fresh.
const IDLE_WAIT: Duration = Duration::from_millis(50);
/// Longest wait when the carrier's socket cannot be watched (no raw socket;
/// since Phase 4 UDP and TCP carriers expose theirs, so this is rare):
/// bounds inbound latency by polling.
const UNWATCHED_WAIT: Duration = Duration::from_millis(2);

/// Open the mic (v0.23 full duplex) on its own thread and stream its PCM
/// ticks (one per 20 ms) over a channel, waking the viewer loop for each;
/// the loop encodes them (Opus or raw, per the host's CAPS). The real mic
/// paces itself on its buffer-completion event and delivers every buffer;
/// the synthetic tone is paced by a drift-free clock. `None` when the mic
/// cannot be opened (capture disabled for the session, never a crash).
fn spawn_mic(
    kind: MicKind,
    waker: ovapp::netwait::Waker,
    stop: Arc<AtomicBool>,
) -> Option<mpsc::Receiver<Vec<i16>>> {
    let (tx, rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel::<bool>();
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
                    warn!("mic unavailable, mic capture disabled: {e}");
                    let _ = ready_tx.send(false);
                    return;
                }
            },
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
        debug!("mic streaming on (20 ms ticks)");
        Some(rx)
    } else {
        None
    }
}

/// The shared post-handshake viewer loop (spec v0.12).
///
/// A [`TerminalSession`] (channel 0) carries frames + input + TYPE text.
/// The remote screen is presented in a native window (`--no-window` skips
/// it); local mouse/keyboard events become POINTER/KEY commands. stdin lines
/// (read on a background thread) are still forwarded as TYPE commands.
/// Ends on window close, Ctrl-C, or stdin EOF.
#[allow(clippy::too_many_arguments)]
fn viewer_loop(
    me: Identity,
    my_dev: u64,
    carrier: impl Carrier + 'static,
    mut hs: Handshake,
    backlog: Vec<Vec<u8>>,
    peer_fp: [u8; 16],
    record_dir: Option<String>,
    no_window: bool,
    mic: Option<MicKind>,
    clipboard: bool,
    send_file: Option<&std::path::Path>,
) -> Result<(), String> {
    let session = hs.take_session().ok_or("no session after handshake")?;
    let mut term = TerminalSession::new(session, carrier);
    // Records the host sent right behind its READY (same poll batch).
    term.queue_inbound(backlog);
    // Section 5: let the session upgrade to a direct UDP path when the host
    // offers candidates (the relay stays the fallback).
    if !NO_DIRECT.load(Ordering::Relaxed) {
        match ovtransport::DirectSockets::bind(0) {
            Ok(s) => term.enable_direct(s, Vec::new()),
            Err(e) => warn!("direct path unavailable ({e}); relay only"),
        }
    }
    // Phase 5: H.264 for high-motion regions, when the Media Foundation
    // decoder is available (installing it adds CAP_H264 to our CAPS).
    #[cfg(windows)]
    if !NO_H264.load(Ordering::Relaxed) {
        match ovapp::mfcodec::H264Decoder::new() {
            Ok(d) => {
                term.set_video_decoder(Box::new(d));
                debug!("H.264 decoding available (Media Foundation)");
            }
            Err(e) => info!("no H.264 decoder ({e}); lossless tiles only"),
        }
    }
    // Phase 3: advertise dirty-tile + zstd decoding and keyframe requests.
    // A Phase 3 host answers with its CAPS and switches to tile deltas; an
    // old host ignores the record and keeps sending full frames.
    term.announce_caps()
        .map_err(|e| format!("send failed: {e}"))?;
    info!("connected to peer fingerprint {}", hex::encode(peer_fp));
    status::emit(&StatusEvent::PeerConnected {
        fingerprint: hex::encode(peer_fp),
    });

    // v0.24: push `--send-file` to the peer right after the handshake.
    if let Some(path) = send_file {
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
    debug!(
        "keys: H/F3 HUD, F1 quality AUTO/10/30/60 fps, F2 audio, M mute, F4 next monitor, \
         F6 record, F8 remote wallpaper"
    );

    ctrlc_flag();

    // stdin is blocking, so read it on a background thread and forward the
    // lines into the same event queue the window feeds (keeps the loop at a
    // fixed cadence instead of blocking between pumps).
    let (tx, stdin_rx) = mpsc::channel::<localinput::LocalEvent>();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(line) => {
                    if tx.send(localinput::LocalEvent::Line(line)).is_err() {
                        break; // loop is gone; stop reading
                    }
                }
                Err(_) => break, // EOF / Ctrl-C: clean end
            }
        }
    });

    let win: Option<Arc<window::RenderWindow>> = if no_window {
        None
    } else {
        match window::RenderWindow::new("OpenView — Remote") {
            Ok(w) => Some(Arc::new(w)),
            Err(e) => {
                warn!("window failed: {e}");
                None
            }
        }
    };
    if let Some(w) = &win {
        info!("renderer: {}", w.renderer_name());
    }
    if win.is_none() && !no_window {
        warn!("no window available; running headless (stdin/TYPE still works)");
    }
    // Liveness (v0.16): keepalive PINGs in both directions, RTT from the
    // echoed sequence, and a rolling FPS/bandwidth window for the HUD.
    let ping_every = match PING_MS.load(Ordering::Relaxed) {
        0 => KEEPALIVE_INTERVAL,
        n => Duration::from_millis(n),
    };
    let mut keepalive = Keepalive::new(ping_every, Instant::now());
    let session_start = Instant::now();
    let mut frames_presented: u64 = 0;
    let run_for = Duration::from_secs(RUN_FOR_SECS.load(Ordering::Relaxed));
    let mut liveness = Liveness::new(Instant::now());
    let mut link_stats = hud::new_link_stats();
    let mut hud_visible = true;
    let mut peer_dead = false;
    let mut last_rtt_ms: Option<f64> = None;
    let mut last_dims = (0u32, 0u32);
    // Recording (v0.19): F6 toggles an `.ovrec` capture of the received
    // session. `record_start` timestamps frames; the writer is finalized on
    // toggle-off and on any loop exit so the trailer is never missing.
    let mut recording: Option<ovapp::record::RecordWriter> = None;
    let mut record_start = Instant::now();
    let record_path = record_dir.unwrap_or_else(default_record_dir);
    // Quality (v0.17): the requested mode, cycled with F1 (AUTO -> 10 -> 30
    // -> 60 -> AUTO). `None` = AUTO; every change sends a QUALITY_SET.
    const QUALITY_STEPS: [Option<u16>; 4] = [None, Some(10), Some(30), Some(60)];
    let mut quality_step = 0usize;
    let send_quality = |term: &mut TerminalSession, step: usize| -> Result<(), String> {
        term.send(&Command::QualitySet {
            target_fps: QUALITY_STEPS[step].unwrap_or(0),
        })
        .map_err(|e| format!("send failed: {e}"))
    };
    // Monitors (v0.18): the host's monitor list (from MONITORS_INFO) and
    // the index of the monitor we are currently viewing. The hook records
    // the list; F4 cycles the capture target across monitors.
    let monitors: Arc<Mutex<Vec<MonitorInfo>>> = Arc::new(Mutex::new(Vec::new()));
    let cur_mon = Arc::new(AtomicUsize::new(0));
    // Audio (v0.21): the AUDIO hook must be installed unconditionally —
    // headless sessions still need the ticks flowing into the jitter buffer
    // so enabling audio later (F2) works without the host re-sending.
    // Wave 1: 30 ms prefill / 90 ms ceiling (was 60 / 120): the host sends
    // 10 ms Opus packets to us (CAP_AUDIO10), so a smaller cushion holds.
    let audio_bus = audio::AudioBus::with_ms(30, 90);
    let audio_stop = Arc::new(AtomicBool::new(false));
    let audio_thread = audio::spawn(audio_bus.clone(), Arc::clone(&audio_stop));
    // F2 toggles playback on/off; M mutes (drops incoming ticks) without
    // tearing the device down.
    let mut audio_on = AUDIO_AT_START.load(Ordering::Relaxed);
    audio_bus.set_enabled(audio_on);
    // Received AUDIO ticks / payload bytes / Opus ticks (session summary).
    let audio_rx = Arc::new([
        std::sync::atomic::AtomicU64::new(0),
        std::sync::atomic::AtomicU64::new(0),
        std::sync::atomic::AtomicU64::new(0),
    ]);
    // Wave 3 metrics: [last arrival us, gaps > AUDIO_GAP_US, gap us total,
    // packets lost on the media channel and concealed].
    let audio_gaps = Arc::new([
        std::sync::atomic::AtomicU64::new(0),
        std::sync::atomic::AtomicU64::new(0),
        std::sync::atomic::AtomicU64::new(0),
        std::sync::atomic::AtomicU64::new(0),
    ]);
    let mut rtt_samples: Vec<f64> = Vec::new();
    let mut audio_muted = false;
    // Mic capture (v0.23 full-duplex audio): when `--mic`/`--audio-synth` is
    // given, a mic thread produces one AUDIO (0x40) payload per 20 ms tick
    // and this loop sends it (`TerminalSession` is not shareable across
    // threads). Open errors (no mic device) disable capture for the session
    // with a log line, never a crash. The host plays ONLY what it receives,
    // so there is no local feedback loop.
    //
    // The loop blocks on `wake` (socket readable / window message / mic
    // tick / housekeeping timeout) instead of sleeping a fixed interval.
    let wake = ovapp::netwait::Wakeup::with_sockets(term.raw_socket(), &term.extra_sockets())
        .map_err(|e| format!("wait setup failed: {e}"))?;
    // Section 4: the host hides its wallpaper by default; F8 toggles it.
    let mut remote_wall_hidden = true;
    let mic_stop = Arc::new(AtomicBool::new(false));
    let mic_rx = mic.and_then(|kind| spawn_mic(kind, wake.waker(), Arc::clone(&mic_stop)));
    let mut mic_enc = ovapp::audio::AudioEncoder::new();
    // Clipboard sync (v0.26): the echo-loop guard, shared with the
    // CLIPBOARD command hook; the serve loop polls the OS clipboard via the
    // cheap sequence number and sends what the guard reports as changed.
    let clip_sync = if clipboard {
        Some(Arc::new(Mutex::new(ovapp::clipboard::ClipboardSync::new())))
    } else {
        None
    };
    let mut clip_seq = clipboard_os::clip_sequence();
    // Cursor overlay commands (viewer-bound host→viewer traffic) composite
    // the remote cursor onto the presented frame; window handles are only
    // touched when a window exists (headless sessions still decode AUDIO).
    {
        let w = win.clone();
        let bus = audio_bus.clone();
        // Phase 4: raw PCM or Opus (the host encodes Opus only for our
        // CAP_OPUS); one decoder state for the stream.
        let mut audio_dec = ovapp::audio::AudioDecoder::new();
        let audio_rx_hook = Arc::clone(&audio_rx);
        let audio_gap_hook = Arc::clone(&audio_gaps);
        // Wave 3: (packets, samples) lost right before the next AUDIO.
        let mut audio_lost: (u16, u32) = (0, 0);
        let monitors_hook = Arc::clone(&monitors);
        let cur_hook = Arc::clone(&cur_mon);
        let clip_hook = clip_sync.clone();
        term.on_cmd(Some(Box::new(move |cmd| match cmd {
            Command::MonitorsInfo { monitors } => {
                if let Ok(mut list) = monitors_hook.lock() {
                    *list = monitors.clone();
                }
                cur_hook.store(0, Ordering::Relaxed);
            }
            Command::Audio(payload) => {
                // Wave 3 metrics: inter-arrival gaps longer than the
                // playback cushion (each one is an audible gap unless
                // concealed).
                let now_us = session_start.elapsed().as_micros() as u64;
                let last = audio_gap_hook[0].swap(now_us, Ordering::Relaxed);
                if last != 0 && now_us.saturating_sub(last) > AUDIO_GAP_US {
                    audio_gap_hook[1].fetch_add(1, Ordering::Relaxed);
                    audio_gap_hook[2].fetch_add(now_us - last, Ordering::Relaxed);
                }
                audio_rx_hook[0].fetch_add(1, Ordering::Relaxed);
                audio_rx_hook[1].fetch_add(payload.len() as u64, Ordering::Relaxed);
                if payload.first() == Some(&ovapp::audio::CODEC_OPUS) {
                    audio_rx_hook[2].fetch_add(1, Ordering::Relaxed);
                }
                // Wave 3: packets lost on the media channel right before
                // this one: Opus PLC for all but the last, which the in-band
                // FEC of this packet reconstructs.
                let (lost, lost_samples) = std::mem::take(&mut audio_lost);
                if bus.is_enabled() {
                    if lost > 0 {
                        let per = lost_samples / u32::from(lost).max(1);
                        for _ in 1..lost {
                            if let Ok(s) = audio_dec.conceal(per) {
                                bus.push(&s);
                            }
                        }
                        if let Ok(s) = audio_dec.decode_fec(payload, per) {
                            bus.push(&s);
                        }
                    }
                    if let Ok(samples) = audio_dec.decode(payload) {
                        bus.push(&samples);
                    }
                }
            }
            Command::MediaGap {
                kind,
                packets,
                samples,
            } if *kind == ovapp::terminal::GAP_AUDIO => {
                audio_lost = (
                    audio_lost.0.saturating_add(*packets),
                    audio_lost.1.saturating_add(*samples),
                );
                // Concealed (FEC / PLC), not a stall: counted separately.
                audio_gap_hook[3].fetch_add(u64::from(*packets), Ordering::Relaxed);
            }
            // CLIPBOARD (v0.26): the host's clipboard text — write it to the
            // local OS clipboard, then record it in the echo-loop guard so
            // this loop's own poll does not send it straight back.
            Command::Clipboard { kind: 0, text } => {
                if let Some(sync) = &clip_hook {
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
            Command::Cursor { x, y } if w.is_some() => w.as_ref().unwrap().set_cursor(*x, *y),
            Command::CursorShape {
                hotspot_x,
                hotspot_y,
                w: cw,
                h: ch,
                bgra,
            } if w.is_some() => w.as_ref().unwrap().set_cursor_shape(CursorBmp {
                hotspot_x: *hotspot_x,
                hotspot_y: *hotspot_y,
                w: *cw,
                h: *ch,
                bgra: bgra.clone(),
            }),
            // A chat line (TYPE with key 0): the terminal path's echo.
            Command::Type {
                key: 0,
                mods: 0,
                payload,
            } => {
                let end = payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(payload.len());
                info!("[peer] {}", String::from_utf8_lossy(&payload[..end]));
            }
            _ => {}
        })));
    }

    // Mouse buttons pressed on the host via this viewer (bit = MouseButton).
    let mut held_buttons: u8 = 0;
    // Why the loop below ended (status event + log).
    let end_reason: String;
    loop {
        // Window messages first: local input gathered here is sent right
        // away (not one loop iteration later).
        if let Some(w) = &win {
            w.pump_messages();
        }
        // Turn local input into remote commands.
        let mut local = win.as_ref().map_or_else(Vec::new, |w| w.take_input());
        // stdin lines (terminal path: chat text, `exit` / `quit`).
        local.extend(stdin_rx.try_iter());
        for ev in local {
            match ev {
                localinput::LocalEvent::Pointer {
                    kind,
                    button,
                    client_x,
                    client_y,
                } => {
                    if let Some(w) = &win {
                        use ovapp::terminal::{PointerAction, PointerKind, POINTER_HAS_POS};
                        let rect = w.content_rect();
                        let input2 = peer_input2(&term);
                        let bit = 1u8 << (button as u8);
                        // A press only starts inside the frame; while a
                        // button is held, moves and the release are clamped
                        // to it (never dropped: a lost UP sticks the button).
                        let pos = match kind {
                            PointerKind::Down => {
                                localinput::client_to_normalized(client_x, client_y, &rect)
                            }
                            PointerKind::Up if held_buttons & bit != 0 => {
                                localinput::client_to_normalized_clamped(client_x, client_y, &rect)
                            }
                            PointerKind::Up => None,
                            PointerKind::Move if held_buttons != 0 => {
                                localinput::client_to_normalized_clamped(client_x, client_y, &rect)
                            }
                            PointerKind::Move => {
                                localinput::client_to_normalized(client_x, client_y, &rect)
                            }
                        };
                        if let Some((nx, ny)) = pos {
                            match kind {
                                PointerKind::Down => held_buttons |= bit,
                                PointerKind::Up => held_buttons &= !bit,
                                PointerKind::Move => {}
                            }
                            let cmd = if input2 {
                                let (action, flags) = match kind {
                                    PointerKind::Move => (PointerAction::MoveAbs, 0),
                                    PointerKind::Down => (PointerAction::Down, POINTER_HAS_POS),
                                    PointerKind::Up => (PointerAction::Up, POINTER_HAS_POS),
                                };
                                Some(Command::PointerEx {
                                    action,
                                    button,
                                    flags,
                                    x: nx as i32,
                                    y: ny as i32,
                                })
                            } else if kind == PointerKind::Move
                                || button == ovapp::terminal::MouseButton::Left
                            {
                                // An old host only knows the left button.
                                Some(Command::Pointer { kind, x: nx, y: ny })
                            } else {
                                None
                            };
                            if let Some(cmd) = cmd {
                                term.send(&cmd).map_err(|e| format!("send failed: {e}"))?;
                            }
                        }
                    }
                }
                localinput::LocalEvent::Wheel { dy, dx } => {
                    // An old host has no wheel: dropped.
                    if peer_input2(&term) {
                        term.send(&Command::Wheel { dy, dx })
                            .map_err(|e| format!("send failed: {e}"))?;
                    }
                }
                localinput::LocalEvent::Key {
                    vk,
                    scan,
                    extended,
                    down,
                } => {
                    // HUD toggle (v0.16): H or F3 shows/hides the health panel.
                    if down && (vk == b'H' as u16 || vk == b'h' as u16 || vk == 0x72) {
                        hud_visible = !hud_visible;
                    }
                    // Quality cycle (v0.17): F1 steps AUTO -> 10 -> 30 -> 60
                    // -> AUTO and sends a QUALITY_SET for each step.
                    if down && vk == 0x70 {
                        quality_step = (quality_step + 1) % QUALITY_STEPS.len();
                        send_quality(&mut term, quality_step)?;
                    }
                    // Audio toggle (v0.21): F2 starts/stops speaker playback.
                    if down && vk == 0x71 {
                        audio_on = !audio_on;
                        audio_bus.set_enabled(audio_on && !audio_muted);
                        info!("audio {}", if audio_on { "on" } else { "off" });
                    }
                    // Audio mute (v0.21): M drops incoming ticks; playback
                    // keeps running so unmuting resumes in-sync audio.
                    if down && (vk == b'M' as u16 || vk == b'm' as u16) {
                        audio_muted = !audio_muted;
                        audio_bus.set_enabled(audio_on && !audio_muted);
                        info!("audio {}", if audio_muted { "muted" } else { "unmuted" });
                    }
                    // Recording toggle (v0.19): F6 starts/stops an `.ovrec`
                    // capture of the received session.
                    if down && vk == 0x75 {
                        if recording.is_some() {
                            stop_recording(&mut recording);
                        } else {
                            toggle_recording(
                                &mut recording,
                                &mut record_start,
                                &record_path,
                                last_dims,
                                QUALITY_STEPS[quality_step],
                                &hex::encode(peer_fp),
                            );
                        }
                    }
                    // Section 4: F8 toggles the host's wallpaper (a host
                    // that advertised CAP_DESKTOP_FX).
                    if vk == 0x77 {
                        if down
                            && term.peer_caps().unwrap_or(0) & ovapp::terminal::CAP_DESKTOP_FX != 0
                        {
                            remote_wall_hidden = !remote_wall_hidden;
                            term.send(&Command::SessionOption {
                                key: ovapp::terminal::OPT_HIDE_WALLPAPER,
                                value: remote_wall_hidden as u8,
                            })
                            .map_err(|e| format!("send failed: {e}"))?;
                            debug!(
                                "remote wallpaper {}",
                                if remote_wall_hidden {
                                    "hidden"
                                } else {
                                    "shown"
                                }
                            );
                        }
                        continue;
                    }
                    // Monitor cycle (v0.18): F4 steps the capture target to
                    // the next monitor (no-op with fewer than two).
                    if down && vk == 0x73 {
                        let list = monitors.lock().map(|m| m.clone()).unwrap_or_default();
                        if list.len() >= 2 {
                            let idx = cur_mon.load(Ordering::Relaxed);
                            let next = (idx + 1) % list.len();
                            cur_mon.store(next, Ordering::Relaxed);
                            let m = &list[next];
                            term.send(&Command::CaptureRegion {
                                x: m.x,
                                y: m.y,
                                w: m.w,
                                h: m.h,
                            })
                            .map_err(|e| format!("send failed: {e}"))?;
                        }
                    }
                    // Input v2: scancode + extended flag (right Ctrl/Alt,
                    // arrows with NumLock on, numpad Enter come out right).
                    let cmd = if peer_input2(&term) {
                        use ovapp::terminal::{KEY_DOWN, KEY_EXTENDED};
                        Command::KeyEx {
                            vk,
                            scan,
                            flags: if down { KEY_DOWN } else { 0 }
                                | if extended { KEY_EXTENDED } else { 0 },
                        }
                    } else {
                        Command::Key { vk, down }
                    };
                    term.send(&cmd).map_err(|e| format!("send failed: {e}"))?;
                }
                localinput::LocalEvent::Line(line) => {
                    if line == "exit" || line == "quit" {
                        return finish(me, my_dev, term);
                    }
                    // A pasted giant line is split so each TYPE record
                    // fits the carrier (an oversized record would end the
                    // session).
                    for piece in
                        ovapp::terminal::chunk_utf8(&line, ovapp::terminal::MAX_TEXT_PAYLOAD)
                    {
                        let cmd = Command::Type {
                            key: 0,
                            mods: 0,
                            payload: piece.as_bytes().to_vec(),
                        };
                        term.send(&cmd).map_err(|e| format!("send failed: {e}"))?;
                    }
                }
            }
        }
        // Pump inbound traffic; frames reassemble and land in the queue.
        if let Err(e) = term.pump() {
            for ev in term.take_direct_events() {
                report_direct_event(&ev);
            }
            return Err(format!("transport closed: {e}"));
        }
        for e in term.take_direct_events() {
            report_direct_event(&e);
        }
        // Inbound traffic since the last iteration is a sign of life (the
        // pump decodes and auto-replies PING/PONG, so the peer's keepalive
        // reaches us here). Only a *change* in the counter counts.
        liveness.observe_count(term.stats().cmds_rx, Instant::now());
        // Our own keepalive: PING when due, RTT from the echoed sequence.
        if let Some(seq) = keepalive.tick(Instant::now()) {
            if term.send(&Command::Ping(seq)).is_err() {
                return Err("keepalive send failed".into());
            }
        }
        // PONG handling: the pump auto-replies to inbound PINGs but a PONG
        // for OUR ping lands in the command stream — match it by sequence.
        // (The pump stores it as `last_pong`; drain it here.)
        if let Some(echo) = term.take_pong() {
            if let Some(rtt) = keepalive.on_pong(echo, Instant::now()) {
                last_rtt_ms = Some(rtt.as_secs_f64() * 1000.0);
                if rtt_samples.len() < 1_000_000 {
                    rtt_samples.push(rtt.as_secs_f64() * 1000.0);
                }
                if RTT_LOG.load(Ordering::Relaxed) {
                    let path = match term.direct_peer() {
                        Some(a) => format!("direct {a}"),
                        None => "relay".to_owned(),
                    };
                    info!("rtt {:.3} ms via {path}", rtt.as_secs_f64() * 1000.0);
                }
            }
        }
        // Dead-peer detection: no inbound of ANY kind for DEAD_AFTER.
        let now_dead = liveness.is_dead(Instant::now(), DEAD_AFTER);
        if now_dead != peer_dead {
            peer_dead = now_dead;
            if peer_dead {
                info!("peer silent for {DEAD_AFTER:?}");
            } else {
                info!("peer liveness recovered");
            }
        }
        // A peer silent for SESSION_TIMEOUT is gone: end the session instead
        // of idling on a silently-dead path.
        if liveness.is_dead(Instant::now(), SESSION_TIMEOUT) {
            end_reason = format!("peer silent for {SESSION_TIMEOUT:?}");
            break;
        }
        // Mic ticks (v0.23) arrive from the mic thread, one AUDIO record each.
        if let Some(rx) = &mic_rx {
            while let Ok(pcm) = rx.try_recv() {
                // Opus to a CAP_OPUS host, raw PCM to an old one.
                for payload in mic_enc.encode_ticks(&pcm, term.peer_caps().unwrap_or(0)) {
                    // Wave 3: media channel when open, reliable otherwise.
                    if term.send_audio(payload).is_err() {
                        return Err("mic send failed".into());
                    }
                }
            }
        }
        // v0.24: drain completed file-id-keyed transfers to disk.
        // Clipboard poll (v0.26): cheap sequence check; send on a real change.
        if let Some(sync) = &clip_sync {
            let seq = clipboard_os::clip_sequence();
            if seq != clip_seq {
                clip_seq = seq;
                if let Some(text) = clipboard_os::clip_read_text()
                    .and_then(|t| sync.lock().ok().and_then(|mut s| s.note_local(&t)))
                {
                    if term
                        .send(&ovapp::clipboard::clipboard_command(&text))
                        .is_err()
                    {
                        return Err("clipboard send failed".into());
                    }
                }
            }
        }
        while let Some(f) = term.take_xfer_file() {
            save_received_file(&f.name, &f.data);
        }
        // Present the latest complete frame (never render stale frames while
        // the peer is dead — the window keeps its last image dimmed by the
        // HUD's dead banner instead).
        // Latest-frame-only: frames that completed since the last iteration
        // are skipped (never inflated) so a slow paint never falls behind.
        if let Some(f) = term.take_latest_frame() {
            if f.is_complete() {
                link_stats.frame_rx(Instant::now(), f.pixels.len() as u64);
                frames_presented += 1;
                last_dims = (f.width, f.height);
                if let Some(rec) = recording.as_mut() {
                    let ts = Instant::now().duration_since(record_start).as_millis() as u64;
                    if let Err(e) = rec.append_frame(
                        f.seq as u32,
                        ts,
                        &f.pixels,
                        f.pixels.len() as u32,
                        f.width,
                        f.height,
                    ) {
                        warn!("recording stopped: {e}");
                        recording = None;
                    }
                }
            }
            // Hand the frame to the window by value (no pixel copy) and
            // recycle whichever buffer comes back for the next decode.
            let spare = match (&win, f.is_complete() && !peer_dead) {
                (Some(w), true) => w.present_owned(f),
                _ => Some(f),
            };
            if let Some(old) = spare {
                term.recycle_frame(old);
            }
        }
        // HUD update on the render tick: RTT + FPS/bandwidth + resolution +
        // the requested quality mode.
        if let Some(w) = &win {
            let (fps, bps) = link_stats.snapshot(Instant::now());
            let lines = hud::hud_lines(
                last_rtt_ms,
                fps,
                bps / (1024.0 * 1024.0),
                last_dims.0,
                last_dims.1,
                QUALITY_STEPS[quality_step],
                {
                    let list = monitors.lock().map(|m| m.clone()).unwrap_or_default();
                    let idx = cur_mon.load(Ordering::Relaxed);
                    if list.is_empty() {
                        None
                    } else {
                        let m = &list[idx.min(list.len() - 1)];
                        Some(format!("{}/{} {}x{}", idx + 1, list.len(), m.w, m.h))
                    }
                },
                recording.as_ref().map(|r| {
                    (
                        Instant::now().duration_since(record_start).as_millis() as u64,
                        r.bytes_written(),
                        r.frame_count(),
                    )
                }),
            );
            w.set_hud(lines, peer_dead, hud_visible);
        }
        if let Some(w) = &win {
            if w.should_quit() {
                end_reason = "window closed".into();
                break;
            }
        }
        if !run_for.is_zero() && session_start.elapsed() >= run_for {
            end_reason = "--duration elapsed".into();
            if let Some(w) = &win {
                w.close();
            }
            break;
        }
        if CTRL_C.load(std::sync::atomic::Ordering::Relaxed) {
            end_reason = "interrupted".into();
            if let Some(w) = &win {
                w.close();
            }
            break;
        }
        // Block until a window message, inbound data, a mic tick, or the
        // next housekeeping deadline — no fixed polling sleep.
        let mut timeout = keepalive
            .until_due(Instant::now())
            .max(Duration::from_millis(1))
            .min(if wake.watches_socket() {
                IDLE_WAIT
            } else {
                UNWATCHED_WAIT
            });
        // UDP retransmission timers (Phase 4).
        if let Some(d) = term.transport_deadline() {
            timeout = timeout.min(d.max(Duration::from_millis(1)));
        }
        // Section 4: a present deferred to the next vsync.
        if win.as_ref().is_some_and(|w| w.repaint_pending()) {
            timeout = timeout.min(Duration::from_millis(4));
        }
        if win.is_some() {
            wake.wait_or_message(timeout);
        } else {
            wake.wait(timeout);
        }
    }
    info!("session ended ({end_reason})");
    status::emit(&StatusEvent::PeerDisconnected { reason: end_reason });
    stop_recording(&mut recording);
    audio_stop.store(true, Ordering::Relaxed);
    mic_stop.store(true, Ordering::Relaxed);
    if mic_enc.ticks > 0 {
        debug!(
            "mic sent {} ticks at {:.1} kbps",
            mic_enc.ticks,
            mic_enc.bitrate() / 1000.0
        );
    }
    let st = term.stats();
    debug!(
        "session summary: {frames_presented} frame(s) presented ({}x{}), {} cmds rx ({:.0} KB/s), {} desync(s), {} keyframe request(s), peer caps {:#x}, last RTT {}",
        last_dims.0,
        last_dims.1,
        st.cmds_rx,
        term.bytes_received() as f64 / 1024.0 / session_start.elapsed().as_secs_f64().max(0.001),
        st.screen_desyncs,
        st.keyframe_requests,
        term.peer_caps().unwrap_or(0),
        last_rtt_ms.map_or("n/a".to_owned(), |r| format!("{r:.1} ms"))
    );
    // Wave 2: lossy tiles still waiting for refinement (0 = exact) and the
    // tile-cache slots in use.
    let scr = term.screen();
    debug!(
        "wave2: {} lossy tile(s) on screen, {} tile-cache slot(s) filled",
        scr.lossy_tiles().count(),
        scr.cache_slots_filled()
    );
    let vs = term.video_stats();
    if vs.units > 0 || vs.ends > 0 {
        debug!(
            "H.264: {} unit(s) ({:.1} MB), {} painted, {:.2} ms/decode, capture-to-paint latency avg {:.1} ms / max {:.1} ms, \
             {} dropped, {} decode error(s), {} IDR request(s), {} motion end(s)",
            vs.units,
            vs.bytes as f64 / 1e6,
            vs.painted,
            vs.decode_time.as_secs_f64() * 1000.0 / vs.painted.max(1) as f64,
            vs.latency_sum_us as f64 / 1000.0 / vs.latency_n.max(1) as f64,
            vs.latency_max_us as f64 / 1000.0,
            vs.dropped,
            vs.decode_errors,
            vs.idr_requests,
            vs.ends
        );
    }
    // Wave 3 metrics line.
    {
        let mut lat: Vec<u32> = term.video_latencies().to_vec();
        lat.sort_unstable();
        let p = |v: &[u32], q: f64| -> f64 {
            if v.is_empty() {
                return 0.0;
            }
            let i = ((v.len() as f64 - 1.0) * q).round() as usize;
            f64::from(v[i.min(v.len() - 1)]) / 1000.0
        };
        let mut rtt = rtt_samples.clone();
        rtt.sort_by(|a, b| a.total_cmp(b));
        let pr = |q: f64| -> f64 {
            if rtt.is_empty() {
                return 0.0;
            }
            rtt[((rtt.len() as f64 - 1.0) * q).round() as usize]
        };
        let secs = session_start.elapsed().as_secs_f64().max(0.001);
        debug!(
            "metrics: video_lat_ms p50 {:.1} p95 {:.1} p99 {:.1} n {} | stalls {} total {:.0} ms | \
             rtt_ms p50 {:.1} p95 {:.1} n {} | audio_gaps {} total {:.0} ms concealed {} | rx {:.2} Mbps | {}",
            p(&lat, 0.5),
            p(&lat, 0.95),
            p(&lat, 0.99),
            lat.len(),
            vs.stalls,
            vs.stall_us as f64 / 1000.0,
            pr(0.5),
            pr(0.95),
            rtt.len(),
            audio_gaps[1].load(Ordering::Relaxed),
            audio_gaps[2].load(Ordering::Relaxed) as f64 / 1000.0,
            audio_gaps[3].load(Ordering::Relaxed),
            term.bytes_received() as f64 * 8.0 / secs / 1e6,
            term.media_summary()
        );
    }
    let ticks = audio_rx[0].load(Ordering::Relaxed);
    if ticks > 0 {
        let bytes = audio_rx[1].load(Ordering::Relaxed);
        debug!(
            "audio received {ticks} ticks ({} Opus), {:.1} kbps",
            audio_rx[2].load(Ordering::Relaxed),
            bytes as f64 * 8.0 / (ticks as f64 * 0.020) / 1000.0
        );
    }
    let (depth, underruns, dropped) = audio_bus.status();
    debug!(
        "audio playback: depth {depth} ms, {underruns} underrun(s), {dropped} sample(s) dropped"
    );
    if let Some(t) = audio_thread {
        let _ = t.join();
    }
    drop(wake);
    finish(me, my_dev, term)
}

/// `--info <file.ovrec>`: print the header summary and recording stats.
fn record_info(path: &std::path::Path) -> Result<(), String> {
    let s = ovapp::record::validate(path)?;
    let h = &s.header;
    println!("file:            {}", path.display());
    println!("app_version:     {}", h.app_version);
    println!("pixel_format:    {} (0 = BGRA)", h.pixel_format);
    println!(
        "codec:           {} ({})",
        h.codec,
        if h.codec == ovapp::record::CODEC_DEFLATE {
            "raw DEFLATE"
        } else {
            "unknown"
        }
    );
    println!("initial size:    {}x{}", h.initial_width, h.initial_height);
    println!(
        "quality_mode:    {}",
        if h.quality_mode == 0 {
            "AUTO".to_owned()
        } else {
            format!("{} fps", h.quality_mode)
        }
    );
    println!(
        "host_name:       {}",
        if h.host_name.is_empty() {
            "(none)"
        } else {
            &h.host_name
        }
    );
    println!("start_unix_ms:   {}", h.start_unix_ms);
    println!("frame_count:     {}", s.frame_count);
    println!("duration_ms:     {}", s.total_duration_ms);
    println!("bytes:           {}", s.bytes);
    Ok(())
}

/// `--export <file.ovrec> --out <dir> [--frame N]`: write frame(s) as PPM.
/// Without `--frame`, every frame is written (with a confirmation prompt if
/// the recording holds more than [`EXPORT_WARN_FRAMES`] frames).
const EXPORT_WARN_FRAMES: u64 = 500;

fn record_export(
    path: &std::path::Path,
    out_dir: &std::path::Path,
    only_frame: Option<u32>,
) -> Result<(), String> {
    let summary = ovapp::record::validate(path)?;
    let frames = ovapp::record::read_frames(path)?;
    if let Some(n) = only_frame {
        let Some(f) = frames.iter().find(|f| f.seq == n) else {
            return Err(format!(
                "frame seq {n} not found (recording holds seqs {}..={})",
                frames.first().map_or(0, |f| f.seq),
                frames.last().map_or(0, |f| f.seq)
            ));
        };
        write_ppm(out_dir, f)?;
        info!(
            "wrote {}",
            out_dir.join(format!("frame-{}.ppm", f.seq)).display()
        );
        return Ok(());
    }
    if summary.frame_count > EXPORT_WARN_FRAMES {
        warn!(
            "about to export {} frames ({} MiB of PPM); Ctrl-C to abort",
            summary.frame_count,
            summary.frame_count
                * u64::from(summary.header.initial_width)
                * u64::from(summary.header.initial_height)
                * 3
                / (1024 * 1024)
        );
    }
    std::fs::create_dir_all(out_dir).map_err(|e| format!("create dir {:?}: {e}", out_dir))?;
    for f in &frames {
        write_ppm(out_dir, f)?;
    }
    info!(
        "exported {} frame(s) to {}",
        frames.len(),
        out_dir.display()
    );
    Ok(())
}

/// Encode one frame as PPM and write it to `<dir>/frame-<seq>.ppm`.
fn write_ppm(dir: &std::path::Path, f: &ovapp::record::RecordedFrame) -> Result<(), String> {
    let ppm = ovapp::image::bgra_to_ppm(f.width, f.height, &f.pixels)?;
    let path = dir.join(format!("frame-{}.ppm", f.seq));
    std::fs::write(&path, &ppm).map_err(|e| format!("write {:?}: {e}", path))
}

/// `--play <file.ovrec> [--loop]`: present the recording in the render
/// window at the recorded pace. Reuses the live session's window + present
/// path (`RenderWindow::present`); Escape or Ctrl-C stops.
fn record_play(path: &std::path::Path, loop_forever: bool) -> Result<(), String> {
    let summary = ovapp::record::validate(path)?;
    let frames = ovapp::record::read_frames(path)?;
    if frames.is_empty() {
        return Err("recording has no frames".into());
    }
    let pacing = ovapp::image::frame_pacing(&frames);
    info!(
        "playing {} ({} frames, {:.1} s) — Escape or Ctrl-C stops",
        path.display(),
        frames.len(),
        summary.total_duration_ms as f64 / 1000.0,
    );
    ctrlc_flag();
    let win = window::RenderWindow::new("OpenView — Playback")
        .map_err(|e| format!("no window available: {e}"))?;
    let mut pass = 0u64;
    loop {
        pass += 1;
        if loop_forever && pass > 1 {
            debug!("loop pass {pass}");
        }
        for (f, delay) in frames.iter().zip(pacing.iter()) {
            if delay.is_zero() {
                win.pump_messages();
            } else {
                let deadline = Instant::now() + *delay;
                while Instant::now() < deadline {
                    win.pump_messages();
                    if win.should_quit() || CTRL_C.load(std::sync::atomic::Ordering::Relaxed) {
                        info!("playback stopped");
                        win.close();
                        return Ok(());
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            // Same present path the live session uses: build a `Frame` from
            // the recorded pixels and hand it to the letterboxing painter.
            let frame = ovapp::screen::Frame {
                width: f.width,
                height: f.height,
                stride: f.width * 4,
                format: ovapp::screen::PixelFormat::Bgra,
                seq: u64::from(f.seq),
                pixels: f.pixels.clone(),
            };
            win.present(&frame);
        }
        if !loop_forever {
            break;
        }
    }
    info!("playback finished");
    win.close();
    Ok(())
}

/// Default recording directory: `$USERPROFILE/ovrecord` (fallback: temp dir).
fn default_record_dir() -> String {
    let base =
        std::env::var("USERPROFILE").unwrap_or_else(|_| std::env::temp_dir().display().to_string());
    format!("{base}\\ovrecord")
}

/// Timestamped recording file name: `ovview-YYYYMMDD-HHMMSS-mmm.ovrec`
/// (local time, no external deps — civil-from-days per Howard Hinnant).
fn record_file_name(now_unix_ms: u64) -> String {
    let secs = now_unix_ms / 1000;
    let ms = now_unix_ms % 1000;
    let days = secs / 86_400;
    let sod = secs % 86_400;
    let (h, m, s) = (sod / 3600, (sod / 60) % 60, sod % 60);
    // Shift to a civil date (days since 1970-03-01 era algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("ovview-{y:04}{mo:02}{d:02}-{h:02}{m:02}{s:02}-{ms:03}.ovrec")
}

/// F6: start a recording (building the header from current state) or stop
/// and finalize the current one.
fn toggle_recording(
    recording: &mut Option<ovapp::record::RecordWriter>,
    record_start: &mut Instant,
    dir: &str,
    dims: (u32, u32),
    quality: Option<u16>,
    host_name: &str,
) {
    match recording {
        Some(_) => {} // handled by the caller via stop_recording below
        None => {
            let now_unix_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let dir_path = std::path::Path::new(dir);
            if let Err(e) = std::fs::create_dir_all(dir_path) {
                warn!("cannot create record dir {dir}: {e}");
                return;
            }
            let path = dir_path.join(record_file_name(now_unix_ms));
            let header = ovapp::record::RecordHeader {
                app_version: "0.23.0".to_owned(),
                pixel_format: ovapp::screen::PixelFormat::Bgra.tag(),
                codec: ovapp::record::CODEC_DEFLATE,
                initial_width: dims.0,
                initial_height: dims.1,
                quality_mode: quality.map_or(0, |q| q.min(255) as u8),
                host_name: host_name.to_owned(),
                start_unix_ms: now_unix_ms,
            };
            match ovapp::record::RecordWriter::create(&path, &header) {
                Ok(w) => {
                    info!("recording to {}", path.display());
                    *record_start = Instant::now();
                    *recording = Some(w);
                }
                Err(e) => warn!("cannot start recording: {e}"),
            }
        }
    }
}

/// Finalize an active recording (if any) and print the summary.
fn stop_recording(recording: &mut Option<ovapp::record::RecordWriter>) {
    if let Some(w) = recording.take() {
        match w.finalize() {
            Ok(s) => info!(
                "recording saved: {} frames, {:.1} s, {:.2} MiB",
                s.frame_count,
                s.total_duration_ms as f64 / 1000.0,
                s.bytes as f64 / (1024.0 * 1024.0),
            ),
            Err(e) => warn!("finalize recording failed: {e}"),
        }
    }
}

/// Whether the host injects the input-v2 messages (`CAP_INPUT2`).
fn peer_input2(term: &TerminalSession) -> bool {
    term.peer_caps()
        .is_some_and(|c| c & ovapp::terminal::CAP_INPUT2 != 0)
}

/// Bounded final drain (in-flight replies/frames land) then clean close.
fn finish(me: Identity, my_dev: u64, mut term: TerminalSession) -> Result<(), String> {
    // Input v2: nothing we pressed may stay held on the host.
    if peer_input2(&term) {
        let _ = term.send(&Command::ReleaseAll);
    }
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    while std::time::Instant::now() < deadline {
        match term.pump() {
            Ok(_) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => break, // peer already closed the tunnel
        }
    }
    let _ = (me, my_dev);
    term.close().map_err(|e| format!("close failed: {e}"))
}

/// Best-effort Ctrl-C flag. Windows: `SetConsoleCtrlHandler` via a tiny
/// `extern "system"` binding (no new deps). Other platforms: unsupported
/// (EOF on stdin ends the session instead).
fn ctrlc_flag() {
    #[cfg(windows)]
    {
        static mut HANDLER_SET: bool = false;
        unsafe {
            if !HANDLER_SET {
                HANDLER_SET = true;
                extern "system" {
                    fn SetConsoleCtrlHandler(
                        handler: Option<extern "system" fn(u32) -> i32>,
                        add: i32,
                    ) -> i32;
                }
                extern "system" fn handler(_: u32) -> i32 {
                    CTRL_C.store(true, std::sync::atomic::Ordering::Relaxed);
                    1
                }
                SetConsoleCtrlHandler(Some(handler), 1);
            }
        }
    }
}

static CTRL_C: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// `--audio` (Phase 4): start with speaker playback on (F2 still toggles) —
/// handy headless (`--no-window`), where there is no F2.
static AUDIO_AT_START: AtomicBool = AtomicBool::new(false);
/// Wave 3 metrics: an audio inter-arrival gap longer than this (us) counts
/// as an audible gap (the 30 ms playback cushion plus one 10 ms packet).
const AUDIO_GAP_US: u64 = 40_000;
/// `--no-h264` (Phase 5): never advertise H.264 (the
/// host keeps streaming lossless tiles only).
static NO_H264: AtomicBool = AtomicBool::new(false);
/// `--duration <secs>` (Phase 4): end the session cleanly after this long
/// (0 = run until closed) — headless smoke runs get their summary lines.
static RUN_FOR_SECS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Minimal LAN discovery: mDNS-browse for peers and match the fingerprint.
/// Returns the first match within ~5 s, or `None`.
fn lan_find(want: &[u8; 16]) -> Option<ovdiscovery::DiscoveredPeer> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        let peers = ovdiscovery::browse("0.0.0.0:0".parse().unwrap(), deadline).ok()?;
        if let Some(p) = peers.into_iter().find(|p| &p.fingerprint == want) {
            return Some(p);
        }
    }
    None
}

fn parse_fingerprint(hex_str: &str) -> Result<[u8; 16], String> {
    let s: String = hex_str.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if s.len() != 32 {
        return Err(format!(
            "--id wants 16 bytes of hex (32 chars), got `{hex_str}`"
        ));
    }
    let mut fp = [0u8; 16];
    for (i, b) in fp.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|e| e.to_string())?;
    }
    Ok(fp)
}

/// Save a file the host sent to `Downloads\OpenView` (never overwriting an
/// existing file).
fn save_received_file(name: &str, data: &[u8]) {
    let clean = ovapp::file_transfer::FileTransfer::sanitize_name(name);
    let dir = ovcommon::paths::received_files_dir();
    let saved = std::fs::create_dir_all(&dir).and_then(|()| {
        let dest = ovcommon::paths::unique_path(&dir, &clean);
        std::fs::write(&dest, data).map(|()| dest)
    });
    match saved {
        Ok(dest) => info!(
            "received `{name}` -> {} ({} bytes)",
            dest.display(),
            data.len()
        ),
        Err(e) => warn!("received `{name}` but could not save it: {e}"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use ovtransport::InMemoryCarrier;

    /// The viewer dials first and the host only parks 2.5 s later — longer
    /// than the old 2 s handshake window. `dial_relay` must hold on until the
    /// relay attaches the host, and only then hand back the carrier.
    #[test]
    fn viewer_waits_for_a_late_host_to_attach() {
        let relay = ovrelay::Relay::bind("127.0.0.1:0".parse().unwrap()).expect("bind");
        let addr = relay.local_addr();
        std::thread::spawn(move || relay.serve(None));
        let sid = 0x7E57_0002;
        let host = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(2500));
            let mut c = RelayTunnelCarrier::connect(addr, sid, TunnelRole::Responder)
                .expect("host connect");
            assert!(c
                .wait_attached(Duration::from_secs(10))
                .expect("host attach"));
            c.send(b"ready").expect("host send");
            // Keep the tunnel up until the viewer read it.
            std::thread::sleep(Duration::from_millis(500));
        });
        let t0 = Instant::now();
        let mut c = dial_relay(addr, sid, Duration::from_secs(10)).expect("viewer attached");
        assert!(t0.elapsed() >= Duration::from_secs(2));
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while got.is_empty() && Instant::now() < deadline {
            c.poll_recv(&mut |d| got.push(d.to_vec())).unwrap();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(got, vec![b"ready".to_vec()]);
        host.join().unwrap();
    }

    #[test]
    fn viewer_reports_a_missing_host() {
        let mut relay = ovrelay::Relay::bind("127.0.0.1:0".parse().unwrap()).expect("bind");
        relay.set_timeout(Duration::from_millis(200));
        let addr = relay.local_addr();
        std::thread::spawn(move || relay.serve(None));
        let err = dial_relay(addr, 0x7E57_0003, Duration::from_secs(10)).unwrap_err();
        assert!(err.contains("not reachable"), "{err}");
    }

    /// The host completes its side, then immediately sends a record (e.g.
    /// MONITORS_INFO) — so the viewer's single poll delivers HELLO, READY
    /// *and* that record. The record must reach the TerminalSession instead
    /// of being swallowed by the established handshake (which would also
    /// desync every later record's sequence number).
    #[test]
    fn record_behind_ready_survives_the_handshake() {
        let (ida, idb) = (Identity::generate(), Identity::generate());
        // Pairing-code mode: the server completes on the client's HELLO, so
        // it can send a record right behind READY. (PIN mode adds a CONFIRM
        // flight, after which the client is already established.)
        let code = idb.pairing_code();
        let (mut cc, mut sc) = InMemoryCarrier::pair();
        let mut client = Handshake::new(Role::Client, 1, &ida, Auth::PairingCode(code.clone()));
        let mut server = Handshake::new(Role::Server, 2, &idb, Auth::PairingCode(code));
        for ev in client.take_events() {
            let HandshakeEvent::Sent(d) = ev;
            cc.send(&d).unwrap();
        }
        let _ = sc.poll_recv(&mut |d| {
            let _ = server.feed(d);
        });
        for ev in server.take_events() {
            let HandshakeEvent::Sent(d) = ev;
            sc.send(&d).unwrap();
        }
        assert!(server.is_established());
        let mut host = TerminalSession::new(server.take_session().unwrap(), sc);
        host.send(&Command::Echo(b"first-record".to_vec())).unwrap();

        let backlog = handshake_over(&mut client, &mut cc).expect("handshake");
        assert_eq!(
            backlog.len(),
            1,
            "the trailing record is kept, not fed to the handshake"
        );
        let mut view = TerminalSession::new(client.take_session().unwrap(), cc);
        view.queue_inbound(backlog);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        view.on_cmd(Some(Box::new(move |c: &Command| {
            if let Command::Echo(p) = c {
                sink.lock().unwrap().push(p.clone());
            }
        })));
        host.send(&Command::Echo(b"second".to_vec())).unwrap();
        for _ in 0..5 {
            let _ = view.pump();
        }
        assert_eq!(view.stats().decrypt_errors, 0, "no sequence desync");
        assert_eq!(
            *seen.lock().unwrap(),
            vec![b"first-record".to_vec(), b"second".to_vec()]
        );
    }
}
