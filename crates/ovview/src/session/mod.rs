//! The post-handshake viewer session.
//!
//! A [`TerminalSession`] (channel 0) carries frames + input + TYPE text.
//! The remote screen is presented in a native window (`--no-window` skips
//! it); local mouse/keyboard events become POINTER/KEY commands
//! ([`input`], with the local hotkeys in [`keys`]). stdin lines (read on a
//! background thread) are forwarded as TYPE commands. Inbound commands
//! other than frames (audio, cursor, clipboard, monitors) are handled by the
//! hook in [`hooks`]; [`stats`] logs the end-of-session metrics.
//!
//! The loop blocks on a [`Wakeup`] (socket readable / window message / mic
//! tick / housekeeping timeout) instead of sleeping a fixed interval, and
//! ends on window close, Ctrl-C, `--duration`, a silent peer, a transport
//! error, or an `exit` / `quit` line on stdin.

use std::io::BufRead;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use ovapp::audio::{AudioBus, AudioEncoder};
use ovapp::clipboard::ClipboardSync;
use ovapp::liveness::{
    Keepalive, LinkStats, Liveness, DEAD_AFTER, KEEPALIVE_INTERVAL, SESSION_TIMEOUT,
};
use ovapp::netwait::Wakeup;
use ovapp::terminal::{Command, MonitorInfo, TerminalSession};
use ovcommon::status::{self, DirectState, StatusEvent};
use ovcore::handshake::Handshake;
use ovtransport::Carrier;
use tracing::{debug, info, warn};

use crate::localinput::LocalEvent;
use crate::mic::MicKind;
use crate::record::Recorder;
use crate::window::{RenderWindow, RendererPref};
use crate::{audio, clipboard_os, hud};

mod hooks;
mod input;
mod keys;
mod stats;

use hooks::AudioStats;
use keys::Hotkeys;

/// Longest event wait while idle: keeps the HUD (fps decay, RTT) fresh.
const IDLE_WAIT: Duration = Duration::from_millis(50);
/// Longest wait when the carrier's socket cannot be watched (no raw socket;
/// the UDP and TCP carriers expose theirs, so this is rare): bounds inbound
/// latency by polling.
const UNWATCHED_WAIT: Duration = Duration::from_millis(2);
/// Shortest wake-up while a vsync-refused present waits to be retried.
const REPAINT_RETRY_WAIT: Duration = Duration::from_millis(4);
/// Speaker jitter buffer: prefill target and ceiling (ms). The host sends
/// 10 ms Opus packets to us (`CAP_AUDIO10`), so a small cushion holds.
const AUDIO_PREFILL_MS: u32 = 30;
const AUDIO_CEILING_MS: u32 = 90;

/// Session settings from the command line.
pub(crate) struct SessionOptions {
    /// `--record-dir`: where F6 recordings go.
    pub(crate) record_dir: Option<String>,
    /// `--no-window`: run headless.
    pub(crate) no_window: bool,
    /// `--mic` / `--audio-synth`: stream a microphone source to the host.
    pub(crate) mic: Option<MicKind>,
    /// Sync the clipboard with the host (off with `--no-clipboard`).
    pub(crate) clipboard: bool,
    /// `--send-file`: push this file right after the handshake.
    pub(crate) send_file: Option<PathBuf>,
    /// Allow the direct UDP upgrade (off with `--no-direct`).
    pub(crate) direct: bool,
    /// Accept H.264 for high-motion regions (off with `--no-h264`).
    pub(crate) h264: bool,
    /// `--audio`: start with speaker playback on (F2 still toggles) — handy
    /// headless, where there is no F2.
    pub(crate) audio_at_start: bool,
    /// `--rtt-log`: log every keepalive RTT sample with the path it took.
    pub(crate) rtt_log: bool,
    /// `--ping-ms`: keepalive PING interval override (RTT measurements).
    pub(crate) ping_every: Option<Duration>,
    /// `--duration`: end the session cleanly after this long — headless
    /// smoke runs get their summary lines.
    pub(crate) run_for: Option<Duration>,
    /// `--renderer`: the window's presenter.
    pub(crate) renderer: RendererPref,
}

/// Run the viewer session over an established handshake until it ends.
///
/// `backlog` holds records the host sent right behind its READY (same poll
/// batch as the handshake's last message); they are queued first.
pub(crate) fn viewer_loop(
    carrier: impl Carrier + 'static,
    mut hs: Handshake,
    backlog: Vec<Vec<u8>>,
    options: SessionOptions,
) -> Result<(), String> {
    let peer_fp = hs
        .peer()
        .ok_or("handshake established without peer info")?
        .fingerprint;
    let session = hs.take_session().ok_or("no session after handshake")?;
    let mut term = TerminalSession::new(session, carrier);
    term.queue_inbound(backlog);
    // Let the session upgrade to a direct UDP path when the host offers
    // candidates (the relay stays the fallback).
    if options.direct {
        match ovtransport::DirectSockets::bind(0) {
            Ok(s) => term.enable_direct(s, Vec::new()),
            Err(e) => warn!("direct path unavailable ({e}); relay only"),
        }
    }
    // H.264 for high-motion regions, when the Media Foundation decoder is
    // available (installing it adds CAP_H264 to our CAPS).
    #[cfg(windows)]
    if options.h264 {
        match ovapp::mfcodec::H264Decoder::new() {
            Ok(d) => {
                term.set_video_decoder(Box::new(d));
                debug!("H.264 decoding available (Media Foundation)");
            }
            Err(e) => info!("no H.264 decoder ({e}); lossless tiles only"),
        }
    }
    // Advertise dirty-tile + zstd decoding and keyframe requests. A host
    // that supports them answers with its CAPS and switches to tile deltas;
    // an older host ignores the record and keeps sending full frames.
    term.announce_caps()
        .map_err(|e| format!("send failed: {e}"))?;
    info!("connected to peer fingerprint {}", hex::encode(peer_fp));
    status::emit(&StatusEvent::PeerConnected {
        fingerprint: hex::encode(peer_fp),
    });

    if let Some(path) = &options.send_file {
        send_file(&mut term, path)?;
    }
    debug!(
        "keys: H/F3 HUD, F1 quality AUTO/10/30/60 fps, F2 audio, M mute, F4 next monitor, \
         F6 record, F8 remote wallpaper"
    );

    crate::ctrlc::install();
    let stdin_rx = spawn_stdin_reader();
    let win = open_window(options.no_window);

    // Liveness: keepalive PINGs in both directions, RTT from the echoed
    // sequence, and a rolling FPS/bandwidth window for the HUD.
    let keepalive = Keepalive::new(
        options.ping_every.unwrap_or(KEEPALIVE_INTERVAL),
        Instant::now(),
    );
    let session_start = Instant::now();

    // Speaker playback: the AUDIO hook is installed unconditionally —
    // headless sessions still need the ticks flowing into the jitter buffer
    // so enabling audio later (F2) works without the host re-sending.
    let audio_bus = AudioBus::with_ms(AUDIO_PREFILL_MS, AUDIO_CEILING_MS);
    let audio_stop = Arc::new(AtomicBool::new(false));
    let audio_thread = audio::spawn(audio_bus.clone(), Arc::clone(&audio_stop));
    let hotkeys = Hotkeys::new(options.audio_at_start);
    audio_bus.set_enabled(hotkeys.audio_on);

    // Mic capture: a mic thread produces one 20 ms PCM tick at a time and
    // this loop encodes and sends it (`TerminalSession` is not shareable
    // across threads). Open errors (no mic device) disable capture for the
    // session with a log line, never a crash. The host plays ONLY what it
    // receives, so there is no local feedback loop.
    let wake = Wakeup::with_sockets(term.raw_socket(), &term.extra_sockets())
        .map_err(|e| format!("wait setup failed: {e}"))?;
    let mic_stop = Arc::new(AtomicBool::new(false));
    let mic_rx = options
        .mic
        .and_then(|kind| crate::mic::spawn(kind, wake.waker(), Arc::clone(&mic_stop)));

    // Clipboard sync: the echo-loop guard, shared with the CLIPBOARD command
    // hook; the loop polls the OS clipboard via the cheap sequence number
    // and sends what the guard reports as changed.
    let clip_sync = options
        .clipboard
        .then(|| Arc::new(Mutex::new(ClipboardSync::new())));

    let mut viewer = Viewer {
        term,
        win,
        peer_fp,
        rtt_log: options.rtt_log,
        session_start,
        run_for: options.run_for,
        keepalive,
        liveness: Liveness::new(Instant::now()),
        link_stats: hud::new_link_stats(),
        peer_dead: false,
        last_rtt_ms: None,
        rtt_samples: Vec::new(),
        last_dims: (0, 0),
        frames_presented: 0,
        recorder: Recorder::new(options.record_dir),
        hotkeys,
        held_buttons: 0,
        monitors: Arc::new(Mutex::new(Vec::new())),
        cur_mon: Arc::new(AtomicUsize::new(0)),
        audio_bus,
        audio_stats: Arc::new(AudioStats::default()),
        clip_seq: clipboard_os::clip_sequence(),
        clip_sync,
        mic_rx,
        mic_enc: AudioEncoder::new(),
    };
    viewer.install_cmd_hook();

    let end_reason = loop {
        // Window messages first: local input gathered here is sent right
        // away (not one loop iteration later).
        if let Some(w) = &viewer.win {
            w.pump_messages();
        }
        if viewer.handle_local_input(&stdin_rx)? == input::Flow::Exit {
            return finish(viewer.term);
        }
        viewer.pump_network()?;
        if let Some(reason) = viewer.check_liveness()? {
            break reason;
        }
        viewer.send_mic_ticks()?;
        viewer.sync_clipboard()?;
        while let Some(f) = viewer.term.take_xfer_file() {
            save_received_file(&f.name, &f.data);
        }
        viewer.present_latest_frame();
        viewer.update_hud();
        if let Some(reason) = viewer.exit_requested() {
            break reason;
        }
        viewer.wait(&wake);
    };

    info!("session ended ({end_reason})");
    status::emit(&StatusEvent::PeerDisconnected { reason: end_reason });
    viewer.recorder.stop();
    audio_stop.store(true, Ordering::Relaxed);
    mic_stop.store(true, Ordering::Relaxed);
    viewer.log_summary();
    if let Some(t) = audio_thread {
        let _ = t.join();
    }
    drop(wake);
    finish(viewer.term)
}

/// The live session state the loop's steps share.
struct Viewer {
    term: TerminalSession,
    /// The render window (`None` when headless).
    win: Option<Arc<RenderWindow>>,
    peer_fp: [u8; 16],
    /// `--rtt-log`.
    rtt_log: bool,
    session_start: Instant,
    /// `--duration`.
    run_for: Option<Duration>,
    keepalive: Keepalive,
    liveness: Liveness,
    /// FPS/bandwidth window for the HUD.
    link_stats: LinkStats,
    /// No inbound traffic for `DEAD_AFTER`: frames are no longer presented
    /// and the HUD shows the dead-peer banner.
    peer_dead: bool,
    last_rtt_ms: Option<f64>,
    /// Every RTT sample (capped), for the end-of-session percentiles.
    rtt_samples: Vec<f64>,
    /// Size of the last complete frame.
    last_dims: (u32, u32),
    frames_presented: u64,
    /// F6 recording.
    recorder: Recorder,
    /// Local hotkey toggles (HUD, quality, audio, wallpaper).
    hotkeys: Hotkeys,
    /// Mouse buttons pressed on the host via this viewer (bit = MouseButton).
    held_buttons: u8,
    /// The host's monitor list (from MONITORS_INFO, recorded by the hook).
    monitors: Arc<Mutex<Vec<MonitorInfo>>>,
    /// Index of the monitor currently viewed (F4 cycles it).
    cur_mon: Arc<AtomicUsize>,
    audio_bus: AudioBus,
    /// Received-audio counters, updated by the hook.
    audio_stats: Arc<AudioStats>,
    /// Clipboard echo-loop guard (`None` with `--no-clipboard`).
    clip_sync: Option<Arc<Mutex<ClipboardSync>>>,
    /// Last seen OS clipboard sequence number.
    clip_seq: u32,
    /// Mic ticks from the capture thread.
    mic_rx: Option<mpsc::Receiver<Vec<i16>>>,
    mic_enc: AudioEncoder,
}

impl Viewer {
    /// Pump inbound traffic (frames reassemble and land in the queue) and
    /// report direct-path events.
    fn pump_network(&mut self) -> Result<(), String> {
        let pumped = self.term.pump();
        for e in self.term.take_direct_events() {
            report_direct_event(&e);
        }
        pumped
            .map(|_| ())
            .map_err(|e| format!("transport closed: {e}"))
    }

    /// Keepalive and dead-peer detection. Returns the end reason once the
    /// peer has been silent for `SESSION_TIMEOUT`.
    fn check_liveness(&mut self) -> Result<Option<String>, String> {
        // Inbound traffic since the last iteration is a sign of life (the
        // pump decodes and auto-replies PING/PONG, so the peer's keepalive
        // reaches us here). Only a *change* in the counter counts.
        self.liveness
            .observe_count(self.term.stats().cmds_rx, Instant::now());
        // Our own keepalive: PING when due, RTT from the echoed sequence.
        if let Some(seq) = self.keepalive.tick(Instant::now()) {
            if self.term.send(&Command::Ping(seq)).is_err() {
                return Err("keepalive send failed".into());
            }
        }
        // The pump auto-replies to inbound PINGs; a PONG for OUR ping is
        // stored as `last_pong` — match it by sequence here.
        if let Some(echo) = self.term.take_pong() {
            if let Some(rtt) = self.keepalive.on_pong(echo, Instant::now()) {
                let ms = rtt.as_secs_f64() * 1000.0;
                self.last_rtt_ms = Some(ms);
                if self.rtt_samples.len() < 1_000_000 {
                    self.rtt_samples.push(ms);
                }
                if self.rtt_log {
                    let path = match self.term.direct_peer() {
                        Some(a) => format!("direct {a}"),
                        None => "relay".to_owned(),
                    };
                    info!("rtt {ms:.3} ms via {path}");
                }
            }
        }
        // Dead-peer detection: no inbound of ANY kind for DEAD_AFTER.
        let now_dead = self.liveness.is_dead(Instant::now(), DEAD_AFTER);
        if now_dead != self.peer_dead {
            self.peer_dead = now_dead;
            if now_dead {
                info!("peer silent for {DEAD_AFTER:?}");
            } else {
                info!("peer liveness recovered");
            }
        }
        // A peer silent for SESSION_TIMEOUT is gone: end the session instead
        // of idling on a silently-dead path.
        if self.liveness.is_dead(Instant::now(), SESSION_TIMEOUT) {
            return Ok(Some(format!("peer silent for {SESSION_TIMEOUT:?}")));
        }
        Ok(None)
    }

    /// Send the mic ticks captured since the last turn, one AUDIO record
    /// each: Opus to a `CAP_OPUS` host, raw PCM to an older one; over the
    /// media channel when open, the reliable channel otherwise.
    fn send_mic_ticks(&mut self) -> Result<(), String> {
        let Some(rx) = &self.mic_rx else {
            return Ok(());
        };
        while let Ok(pcm) = rx.try_recv() {
            let caps = self.term.peer_caps().unwrap_or(0);
            for payload in self.mic_enc.encode_ticks(&pcm, caps) {
                if self.term.send_audio(payload).is_err() {
                    return Err("mic send failed".into());
                }
            }
        }
        Ok(())
    }

    /// Clipboard poll: a cheap sequence-number check; read and send the
    /// text only on a real local change.
    fn sync_clipboard(&mut self) -> Result<(), String> {
        let Some(sync) = &self.clip_sync else {
            return Ok(());
        };
        let seq = clipboard_os::clip_sequence();
        if seq == self.clip_seq {
            return Ok(());
        }
        self.clip_seq = seq;
        if let Some(text) = clipboard_os::clip_read_text()
            .and_then(|t| sync.lock().ok().and_then(|mut s| s.note_local(&t)))
        {
            if self
                .term
                .send(&ovapp::clipboard::clipboard_command(&text))
                .is_err()
            {
                return Err("clipboard send failed".into());
            }
        }
        Ok(())
    }

    /// Present the latest complete frame. Latest-frame-only: frames that
    /// completed since the last turn are skipped so a slow paint never falls
    /// behind. Stale frames are never rendered while the peer is dead — the
    /// window keeps its last image, dimmed under the HUD's dead banner.
    fn present_latest_frame(&mut self) {
        let Some(f) = self.term.take_latest_frame() else {
            return;
        };
        if f.is_complete() {
            self.link_stats
                .frame_rx(Instant::now(), f.pixels.len() as u64);
            self.frames_presented += 1;
            self.last_dims = (f.width, f.height);
            self.recorder.append(&f);
        }
        // Hand the frame to the window by value (no pixel copy) and recycle
        // whichever buffer comes back for the next decode.
        let spare = match (&self.win, f.is_complete() && !self.peer_dead) {
            (Some(w), true) => w.present_owned(f),
            _ => Some(f),
        };
        if let Some(old) = spare {
            self.term.recycle_frame(old);
        }
    }

    /// The HUD text: RTT, FPS/bandwidth, resolution, the requested quality
    /// mode, and the monitor (or the recording indicator).
    fn update_hud(&mut self) {
        let Some(w) = &self.win else {
            return;
        };
        let (fps, bps) = self.link_stats.snapshot(Instant::now());
        let lines = hud::hud_lines(
            self.last_rtt_ms,
            fps,
            bps / (1024.0 * 1024.0),
            self.last_dims.0,
            self.last_dims.1,
            self.hotkeys.quality(),
            self.monitor_label(),
            self.recorder.hud_status(),
        );
        w.set_hud(lines, self.peer_dead, self.hotkeys.hud_visible);
    }

    /// `"<i>/<n> <w>x<h>"` for the viewed monitor; `None` before the first
    /// MONITORS_INFO.
    fn monitor_label(&self) -> Option<String> {
        let list = self.monitors.lock().map(|m| m.clone()).unwrap_or_default();
        let idx = self.cur_mon.load(Ordering::Relaxed);
        let m = list.get(idx.min(list.len().checked_sub(1)?))?;
        Some(format!("{}/{} {}x{}", idx + 1, list.len(), m.w, m.h))
    }

    /// Window closed, `--duration` elapsed, or Ctrl-C: the end reason.
    fn exit_requested(&self) -> Option<String> {
        if self.win.as_ref().is_some_and(|w| w.should_quit()) {
            return Some("window closed".into());
        }
        let reason = if self
            .run_for
            .is_some_and(|d| self.session_start.elapsed() >= d)
        {
            "--duration elapsed"
        } else if crate::ctrlc::pressed() {
            "interrupted"
        } else {
            return None;
        };
        if let Some(w) = &self.win {
            w.close();
        }
        Some(reason.into())
    }

    /// Block until a window message, inbound data, a mic tick, or the next
    /// housekeeping deadline — no fixed polling sleep.
    fn wait(&mut self, wake: &Wakeup) {
        let mut timeout = self
            .keepalive
            .until_due(Instant::now())
            .max(Duration::from_millis(1))
            .min(if wake.watches_socket() {
                IDLE_WAIT
            } else {
                UNWATCHED_WAIT
            });
        // UDP retransmission timers.
        if let Some(d) = self.term.transport_deadline() {
            timeout = timeout.min(d.max(Duration::from_millis(1)));
        }
        // A present deferred to the next vsync.
        if self.win.as_ref().is_some_and(|w| w.repaint_pending()) {
            timeout = timeout.min(REPAINT_RETRY_WAIT);
        }
        if self.win.is_some() {
            wake.wait_or_message(timeout);
        } else {
            wake.wait(timeout);
        }
    }
}

/// Push `--send-file` to the peer.
fn send_file(term: &mut TerminalSession, path: &std::path::Path) -> Result<(), String> {
    let data = std::fs::read(path).map_err(|e| format!("--send-file {}: {e}", path.display()))?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file")
        .to_owned();
    info!("sending {name} ({} bytes)", data.len());
    term.send_file_xfer(&name, &data, 0)
        .map_err(|e| format!("send {name}: {e}"))
}

/// stdin is blocking, so read it on a background thread and forward the
/// lines into the same event queue the window feeds (keeps the loop at a
/// fixed cadence instead of blocking between pumps).
fn spawn_stdin_reader() -> mpsc::Receiver<LocalEvent> {
    let (tx, rx) = mpsc::channel::<LocalEvent>();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(line) => {
                    if tx.send(LocalEvent::Line(line)).is_err() {
                        break; // loop is gone; stop reading
                    }
                }
                Err(_) => break, // EOF / Ctrl-C: clean end
            }
        }
    });
    rx
}

/// Open the render window unless headless; a failure warns and continues
/// headless.
fn open_window(no_window: bool) -> Option<Arc<RenderWindow>> {
    let win = if no_window {
        None
    } else {
        match RenderWindow::new() {
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
    win
}

/// Whether the host injects the input-v2 messages (`CAP_INPUT2`).
fn peer_input2(term: &TerminalSession) -> bool {
    term.peer_caps()
        .is_some_and(|c| c & ovapp::terminal::CAP_INPUT2 != 0)
}

/// Bounded final drain (in-flight replies/frames land) then clean close.
fn finish(mut term: TerminalSession) -> Result<(), String> {
    // Input v2: nothing we pressed may stay held on the host.
    if peer_input2(&term) {
        let _ = term.send(&Command::ReleaseAll);
    }
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        match term.pump() {
            Ok(_) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => break, // peer already closed the tunnel
        }
    }
    term.close().map_err(|e| format!("close failed: {e}"))
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
