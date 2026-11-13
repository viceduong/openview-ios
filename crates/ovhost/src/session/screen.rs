//! The screen-mode session: capture thread, mic thread and the session
//! thread that owns the wire.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ovapp::gdisrc::GdiScreen;
use ovapp::liveness::{Keepalive, KEEPALIVE_INTERVAL};
use ovapp::monitors::{enumerate_monitors, Monitor};
use ovapp::terminal::{Command, TerminalSession};
use tracing::{debug, info};

use super::capture_loop::{capture_loop, CaptureCtl, METRICS_EVERY};
use super::direct::log_direct;
use super::files::drain_received_files;
use super::hooks::{input_backend, HostHooks, SharedInput};
use super::liveness::{absorb_pongs_with, check_peer, session_over, PeerState};
use super::{clipboard_poll, CLIPBOARD_POLL, UNWATCHED_WAIT};
use crate::input::{self, Region, VirtualScreen};
use crate::mic::{spawn_mic, MicKind};
use crate::settings::{FORCE_GDI, NO_MEDIA, NO_PACING, TEST_PATTERN};
use crate::{capture, clipboard_os, cursor, desktop_fx};

/// FRAME_DATA chunk size on the wire (see [`ovapp::sendq::DEFAULT_CHUNK`]).
const FRAME_CHUNK: usize = ovapp::sendq::DEFAULT_CHUNK;
/// How often the screen session polls the cursor (the tracker throttles
/// actual sends further).
const CURSOR_POLL: Duration = Duration::from_millis(8);
/// Cursor poll interval while it is moving (> the 120 Hz send cap).
const CURSOR_POLL_ACTIVE: Duration = Duration::from_millis(4);
/// How long after the last move the fast poll continues.
const CURSOR_ACTIVE_FOR: Duration = Duration::from_millis(250);

/// What a screen session streams and accepts.
pub(super) struct ScreenOpts {
    /// `None` = the full primary monitor; `Some((w, h))` = a fixed region
    /// at its origin.
    pub(super) region: Option<(u32, u32)>,
    /// Base capture rate.
    pub(super) fps: u32,
    /// Audio source streamed to the viewer, if any.
    pub(super) mic: Option<MicKind>,
    /// Play the viewer's mic on the host speakers.
    pub(super) audio_playback: bool,
    /// Sync the clipboard.
    pub(super) clipboard: bool,
    /// The session's desktop effects (SESSION_OPTION toggles them).
    pub(super) fx: Option<Arc<Mutex<desktop_fx::DesktopFx>>>,
}

/// The host-side screen session:
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
/// [`SESSION_TIMEOUT`](ovapp::liveness::SESSION_TIMEOUT).
pub(super) fn serve_screen(
    mut term: TerminalSession,
    opts: ScreenOpts,
    data_dir: &Path,
) -> Result<(), String> {
    let ScreenOpts {
        region,
        fps,
        mic,
        audio_playback,
        clipboard,
        fx,
    } = opts;
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
                // available, GDI otherwise.
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
    mic_rx: Option<&Receiver<Vec<i16>>>,
    data_dir: &Path,
) -> Result<(), String> {
    let mut sender = ovapp::sendq::PrioritySender::new(FRAME_CHUNK);
    // Tell the viewer which monitors it can pick from before the first
    // frame flies.
    let monitors = enumerate_monitors();
    sender.push_control(Command::MonitorsInfo {
        monitors: monitors_info(&monitors),
    });
    let mut cursor_tr = cursor::CursorTracker::default();
    // Keepalive PINGs keep an idle (static) screen alive on the wire and
    // measure the RTT.
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
        // A/B switch for benches: the unpaced sender.
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
        // Cursor overlay: poll + throttle independent of frames.
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
                    // Opus to a CAP_OPUS viewer (10 ms packets with FEC to a
                    // CAP_AUDIO10 one), raw PCM to an old one.
                    let caps = ctl.peer_caps.load(Ordering::Relaxed);
                    for payload in audio_enc.encode_ticks(&pcm, caps) {
                        // On the media channel when it is open (a lost
                        // packet is concealed, nothing stalls).
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
        // On the direct v2 path the delay-based controller's target (net of
        // the FEC share) is the link rate; elsewhere the pacer's. (The H.264
        // budget is 70 % of this, `target_bitrate`: headroom for tiles,
        // audio and the controller's probing.)
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
        // A viewer that vanished mid-drag must not leave a button or
        // modifier held on the host.
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
        // back), a new frame / mic tick, or the next cursor poll. Frame
        // data held back by the pacer's window counts as backlogged too;
        // the wait then ends when the window reopens or the next probe is
        // due (PONGs arriving wake us via the socket).
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
            // UDP retransmission timers.
            if let Some(d) = term.transport_deadline() {
                wait = wait.min(d.max(Duration::from_millis(1)));
            }
            wake.wait(wait);
        }
    }
}

/// One-line link-pacing summary: the delivery-rate / min-RTT model and how
/// often frame data was held back to keep queues short.
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
