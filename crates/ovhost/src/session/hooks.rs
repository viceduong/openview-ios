//! The inbound-command hook shared by both serve loops ([`HostHooks`]).

use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use ovapp::liveness::Liveness;
use ovapp::monitors::validate_region;
use ovapp::terminal::{Command, TerminalSession};
use tracing::{debug, info, trace, warn};

use super::SharedClipboard;
use crate::input::{self, VirtualScreen};
use crate::{clipboard_os, desktop_fx, settings, speaker};

#[cfg(test)]
mod tests;

/// A capture-target request slot: the rect the viewer asked for, in
/// virtual-desktop coordinates, parked until the capture loop consumes it.
pub(super) type PendingRegion = Arc<Mutex<Option<(i32, i32, u32, u32)>>>;

/// The session's input controller, shared by the command hook, the capture
/// thread (region retargets) and the session loop (stuck-input timeout).
pub(super) type SharedInput = Arc<Mutex<input::InputController>>;

/// Shared command handling for both serve loops: chat lines (key==0,
/// mods==0) are logged with their content and queued for echo back to the
/// peer; raw keystrokes print to stderr; PING -> PONG (auto); file transfers
/// reported on completion. Echoing chat lines makes a full
/// client->host->client round trip observable end-to-end (the viewer prints
/// `[peer] <line>`), and the `chat:` log line gives a deterministic,
/// content-bearing signal for the e2e test to assert on.
///
/// Also carries the liveness state: every inbound record (of any kind)
/// refreshes [`Liveness`], so an actively streaming session stays alive even
/// when PONGs are dropped, and the auto-replied PING/PONG exchange gives the
/// peer an RTT measurement.
pub(super) struct HostHooks {
    /// Chat payloads received this cycle, awaiting the echo send.
    echo: Arc<Mutex<Vec<Vec<u8>>>>,
    /// Screen-mode input injection: the controller owns the captured
    /// region + virtual screen mapping (shared with the capture loop so a
    /// `CAPTURE_REGION` retarget also re-maps pointer input) and the held
    /// key/button bookkeeping. `None` in terminal mode (log-only).
    pub(super) input: Option<SharedInput>,
    /// Inbound-record liveness tracker: any decoded record refreshes it, so
    /// the host notices a dead peer on its side too.
    pub(super) liveness: Arc<Mutex<Liveness>>,
    /// The viewer's requested capture rate. `0` = AUTO (the queue-delay
    /// driven governor decides); anything else is a fixed base fps the
    /// capture loop honours. Written by the `QUALITY_SET` handler, read
    /// every capture tick.
    pub(super) target_fps: Arc<AtomicU16>,
    /// PONG echoes received this cycle: drained by the session loop and fed
    /// into its [`Keepalive`](ovapp::liveness::Keepalive) to measure RTT,
    /// which then lands in `rtt_us`.
    pub(super) pong_seq: Arc<Mutex<Vec<u64>>>,
    /// The host's own smoothed RTT in microseconds, measured from its
    /// keepalive PING/PONG exchanges. `0` = no measurement yet.
    pub(super) rtt_us: Arc<AtomicU64>,
    /// Capture-target request: the rect the viewer asked the host to
    /// capture (`CAPTURE_REGION`), in **virtual-desktop** coordinates.
    /// Written by the command handler; the capture loop consumes it on its
    /// next tick and re-targets the screen source there — off the command
    /// thread, so the handler never holds the screen lock across a capture.
    pub(super) pending_region: PendingRegion,
    /// Speaker playback bus (full-duplex audio): the AUDIO hook pushes the
    /// viewer's mic ticks here; the playback thread drains them into
    /// `waveOut`. `None` when `--audio` was not given.
    speaker: Option<speaker::AudioBus>,
    /// Stop flag + handle of the speaker playback thread: set and joined
    /// when the hooks drop at session end, so the thread and its `waveOut`
    /// device never outlive the session.
    speaker_stop: Option<Arc<AtomicBool>>,
    speaker_thread: Option<std::thread::JoinHandle<()>>,
    /// Clipboard sync: the echo-loop guard. The `CLIPBOARD` hook calls
    /// [`ClipboardSync::note_remote`](ovapp::clipboard::ClipboardSync::note_remote)
    /// after applying the peer's text; the serve loop polls the OS
    /// clipboard and calls
    /// [`ClipboardSync::note_local`](ovapp::clipboard::ClipboardSync::note_local),
    /// sending what it returns. `None` when `--no-clipboard` was given.
    pub(super) clipboard: SharedClipboard,
    /// The viewer's CAPS bits (0 = a viewer that never sent CAPS → full
    /// DEFLATE frames only). Read by the capture thread.
    pub(super) peer_caps: Arc<AtomicU32>,
    /// KEYFRAME_REQUEST seen: the capture thread sends a full frame next.
    pub(super) keyframe: Arc<AtomicBool>,
    /// VIDEO_IDR_REQUEST seen: the next H.264 picture is an IDR.
    pub(super) video_idr: Arc<AtomicBool>,
    /// The session's desktop effects (SESSION_OPTION toggles).
    pub(super) fx: Option<Arc<Mutex<desktop_fx::DesktopFx>>>,
}

/// The input backend for this run: `SendInput`, or the logging dry run.
pub(super) fn input_backend(vs: VirtualScreen) -> Box<dyn input::InputBackend> {
    if settings::DRY_INPUT.load(Ordering::Relaxed) {
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
    pub(super) fn new() -> Self {
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

    /// Enable the clipboard sync path: install the echo-loop guard and make
    /// the `CLIPBOARD` hook write the peer's text to the OS clipboard. On by
    /// default; `--no-clipboard` skips this.
    pub(super) fn enable_clipboard(&mut self) {
        let sync = Arc::new(Mutex::new(ovapp::clipboard::ClipboardSync::new()));
        self.clipboard = Some(Arc::clone(&sync));
    }

    /// Enable the speaker playback path (`--audio`): install the bus and
    /// spawn the playback thread, already enabled — the host plays whatever
    /// arrives the moment it arrives.
    pub(super) fn enable_speaker(&mut self) {
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
    pub(super) fn register(&self, term: &mut TerminalSession) {
        let echo_hook = Arc::clone(&self.echo);
        let input = self.input.clone();
        let live = Arc::clone(&self.liveness);
        let target_fps = Arc::clone(&self.target_fps);
        let pong_seq = Arc::clone(&self.pong_seq);
        let pending_region = Arc::clone(&self.pending_region);
        let hooks_audio = self.speaker.clone();
        // Raw PCM or Opus (the viewer encodes Opus only after our CAPS
        // advertised CAP_OPUS); one decoder state for the stream.
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
                    // Hand the echo to the session loop's keepalive so it can
                    // measure RTT.
                    if let Ok(mut q) = pong_seq.lock() {
                        q.push(*n);
                    }
                }
                Command::FileBegin { size, name } => {
                    info!("file `{name}` incoming ({size} bytes)")
                }
                Command::FileData(_) => {}
                Command::FileEnd => {}
                // File-id-keyed transfers: reassembled by the pump and
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
                // CAPS: what the viewer can decode. The pump already
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
                // KEYFRAME_REQUEST: the viewer lost sync.
                Command::KeyframeRequest { have_seq } => {
                    keyframe.store(true, Ordering::Relaxed);
                    debug!("keyframe requested (viewer at frame {have_seq})");
                }
                // VIDEO_IDR_REQUEST: the viewer's H.264 chain broke.
                Command::VideoIdrRequest { have_seq } => {
                    video_idr.store(true, Ordering::Relaxed);
                    debug!("H.264 IDR requested (viewer at video unit {have_seq})");
                }
                // Cursor commands are viewer-bound (host → viewer only); the host
                // never receives them, so this is just the exhaustive-match arm.
                Command::Cursor { .. } | Command::CursorShape { .. } => {}
                // AUDIO is direction-agnostic: a received AUDIO is the REMOTE
                // peer's mic, so with `--audio` it is decoded and pushed into
                // the speaker bus (malformed payloads are dropped, never panic).
                Command::Audio(payload) => {
                    if let Some(bus) = &hooks_audio {
                        if let Ok(samples) = audio_dec.decode(payload) {
                            bus.push(&samples);
                        }
                    }
                }
                // CLIPBOARD: the peer's clipboard text — write it to the
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
                // CAPTURE_REGION: the viewer picked a capture rect (a whole
                // monitor from MONITORS_INFO). Validate it against the
                // virtual desktop and park it for the capture loop — the
                // handler never touches the screen source itself, so there
                // is no re-entrancy with a capture in progress.
                Command::CaptureRegion { x, y, w, h } => {
                    let vs = VirtualScreen::query();
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
                // MONITORS_INFO is host->viewer only; the host never receives it.
                Command::MonitorsInfo { .. } => {}
                // SESSION_OPTION: the viewer toggles a desktop effect.
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
                // Host -> viewer only.
                Command::DirectCandidates { .. } => {}
                // Media-channel signals. A video unit never reaches the
                // hook; an audio gap of the viewer's mic is simply skipped
                // (the speaker jitter buffer rides it out).
                Command::VideoUnit { .. } | Command::MediaGap { .. } => {}
                // QUALITY_SET: the viewer's target capture rate (0 = auto).
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
    pub(super) fn drain(&self) -> Vec<Vec<u8>> {
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
