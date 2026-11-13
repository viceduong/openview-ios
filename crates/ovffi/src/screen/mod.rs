//! `ov_screen_*`: the screen assembler the shell feeds channel-0 commands.
//!
//! Every entry point here takes a screen pointer `s`. Unless stated
//! otherwise its `# Safety` contract is the same: `s` must be null or a live
//! handle from [`ov_screen_new`]. The screen is not internally locked, so no
//! other call on the same handle may run concurrently (the shell drives it
//! from one serial queue), and no entry point may be called from inside one
//! of this screen's sinks.

mod feed;
mod framebuffer;
mod liveness;
mod sinks;
mod video;

pub use feed::*;
pub use framebuffer::*;
pub use liveness::*;
pub use sinks::*;
pub use video::*;

use std::collections::VecDeque;

use crate::cabi::write;
use crate::callbacks::Sink;
use crate::error::OV_ERR_PANIC;

/// Screen-frame reassembler. Fed **terminal-channel (channel 0)** command
/// records — each [`ov_screen_feed`] call carries one wire command
/// (`[1B opcode][payload]`). Non-screen opcodes (ECHO/TYPE/FILE_*/PING) are
/// ignored; `FRAME_BEGIN/DATA/END` are reassembled into a pixel buffer and
/// delivered via the frame sink on `FRAME_END`; `CURSOR`/`CURSOR_SHAPE`
/// commands fire the cursor sinks.
///
/// This keeps pixel reassembly on the Rust side (single source of truth with
/// the desktop `TerminalSession`) so the phone shell only renders.
///
/// The assembler keeps a persistent framebuffer
/// ([`ovapp::tiles::ScreenDecoder`], shared with the desktop viewer), applies
/// full frames and `TILES_BEGIN` dirty-tile deltas (DEFLATE or zstd, decoded
/// in pure Rust) and still delivers the **whole** framebuffer through the
/// same frame sink — the shell's contract is unchanged. Capabilities and
/// keyframe requests ride the existing 9-byte reply path
/// ([`ov_screen_take_pong_reply`]): once the first screen command arrives
/// the next take yields a `CAPS` record, and a desync (lost/corrupt update)
/// queues a `KEYFRAME_REQUEST` — both exactly 9 bytes, like a PONG, so the
/// shell sends them without knowing what they are.
pub struct OvScreen {
    /// The update being reassembled: its FRAME_END seq + decode header.
    rx: Option<(u64, ovapp::tiles::UpdateMeta)>,
    buf: Vec<u8>,
    /// Persistent framebuffer (full frames + tile deltas applied in order).
    dec: ovapp::tiles::ScreenDecoder,
    /// 9-byte control records owed to the peer (CAPS, KEYFRAME_REQUEST),
    /// drained after any PONG by [`ov_screen_take_pong_reply`].
    outbox: VecDeque<[u8; ovapp::terminal::SMALL_CONTROL_LEN]>,
    /// CAPS queued once per screen (on the first screen command).
    caps_queued: bool,
    /// The host's CAPS bits (0 = old host: no keyframe requests).
    peer_caps: u32,
    /// Rate limit for KEYFRAME_REQUEST.
    last_kf_request: Option<std::time::Instant>,
    /// Advertise `CAP_DIRECT` (the shell wired the direct sinks).
    direct_capable: bool,
    /// The video stream's colour description (primaries,
    /// transfer, matrix, full_range) from the last SPS.
    video_colour: [u8; 4],
    sink: Sink<FrameSink>,
    cursor_sink: Sink<CursorSink>,
    cursor_shape_sink: Sink<CursorShapeSink>,
    /// Audio sink: fired on AUDIO commands with one 20 ms tick of
    /// s16le 48 kHz mono PCM.
    audio_sink: Sink<AudioSink>,
    /// Opus sink: fired on Opus AUDIO commands with the packet
    /// passed through (the shell decodes it natively). Installing it makes
    /// our CAPS advertise `CAP_OPUS`; without it the host keeps sending raw
    /// PCM to `audio_sink`.
    opus_sink: Sink<OpusSink>,
    /// H.264 video sink: access units passed through (SPS/PPS +
    /// AVCC) for VideoToolbox. Installing it advertises `CAP_H264`.
    video_sink: Sink<VideoSink>,
    /// The open VIDEO_BEGIN (its FRAME_DATA accumulates in `buf`).
    rx_video: Option<(ovapp::video::VideoHeader, u32)>,
    /// Video continuity (a gap drops P-frames until the next IDR).
    video_rx: ovapp::video::VideoRx,
    last_video_seq: u64,
    last_idr_request: Option<std::time::Instant>,
    /// Monitors sink: fired on MONITORS_INFO with the host's
    /// monitor list so the shell can offer a monitor picker.
    monitors_sink: Sink<MonitorsSink>,
    /// Clipboard sink: fired on CLIPBOARD with peer clipboard text.
    clipboard_sink: Sink<ClipboardSink>,
    /// File sink: fired when a peer's file transfer completes. The
    /// reassembler (`ovapp::file_transfer::FileTransfer`) mirrors the desktop
    /// `TerminalSession` pump exactly — single source of truth.
    file_sink: Sink<FileSink>,
    /// File-id-keyed transfer reassembly, drained into `file_sink`
    /// after each `FILE_XFER_*` command is fed.
    xfer: ovapp::file_transfer::FileTransfer,
    /// Liveness state: inbound tracking + keepalive pacing + RTT.
    live: ovapp::liveness::Liveness,
    keepalive: ovapp::liveness::Keepalive,
    /// First shell tick (shell ms, real instant): maps the shell's clock onto
    /// this screen's real clock (see `shell_now`).
    shell_clock_base: Option<(i64, std::time::Instant)>,
    /// Real-clock instant of the in-flight PING (RTT is measured in real
    /// time, since `ov_screen_feed` has no shell clock).
    ping_sent_real: Option<std::time::Instant>,
    /// Last measured RTT in milliseconds (-1 before the first PONG).
    last_rtt_ms: i32,
    /// Rolling inbound-frame statistics for the FPS sink value.
    stats: ovapp::liveness::LinkStats,
    /// Last computed FPS (kept between ticks so the sink can fire anytime).
    last_fps: f32,
    liveness_sink: Sink<LivenessSink>,
    /// The echoed nonce of an inbound PING awaiting the shell's PONG reply;
    /// `None` when nothing is pending.
    pong_reply_nonce: Option<u64>,
    /// The Instant `ov_screen_new` ran at — the epoch for `Instant` math.
    epoch: std::time::Instant,
}

impl OvScreen {
    /// An empty assembler with no sinks wired.
    fn new() -> Self {
        let now = std::time::Instant::now();
        Self {
            rx: None,
            buf: Vec::new(),
            dec: ovapp::tiles::ScreenDecoder::new(),
            outbox: VecDeque::new(),
            caps_queued: false,
            peer_caps: 0,
            last_kf_request: None,
            direct_capable: false,
            video_colour: [1, 1, 1, 0],
            sink: Sink::none(),
            cursor_sink: Sink::none(),
            cursor_shape_sink: Sink::none(),
            audio_sink: Sink::none(),
            opus_sink: Sink::none(),
            video_sink: Sink::none(),
            rx_video: None,
            video_rx: ovapp::video::VideoRx::new(),
            last_video_seq: 0,
            last_idr_request: None,
            monitors_sink: Sink::none(),
            clipboard_sink: Sink::none(),
            file_sink: Sink::none(),
            xfer: ovapp::file_transfer::FileTransfer::new(),
            live: ovapp::liveness::Liveness::new(now),
            keepalive: ovapp::liveness::Keepalive::new(ovapp::liveness::KEEPALIVE_INTERVAL, now),
            shell_clock_base: None,
            ping_sent_real: None,
            last_rtt_ms: -1,
            stats: ovapp::liveness::LinkStats::new(std::time::Duration::from_secs(2)),
            last_fps: 0.0,
            liveness_sink: Sink::none(),
            pong_reply_nonce: None,
            epoch: now,
        }
    }

    /// Map the shell's clock (`now_ms`, from its own epoch) onto this
    /// screen's real monotonic clock, which `record_inbound` / RTT use. The
    /// shell's epoch is its session controller's, which outlives this screen
    /// across reconnects; using `self.epoch + now_ms` directly would skew
    /// every liveness/keepalive check by the length of the earlier
    /// connection (a permanent "Reconnecting…" after an auto-reconnect). The
    /// first tick pins the offset; later ticks advance by the shell's
    /// elapsed time.
    fn shell_now(&mut self, now_ms: i64) -> std::time::Instant {
        let now_ms = now_ms.max(0);
        let (base_ms, base) = *self
            .shell_clock_base
            .get_or_insert_with(|| (now_ms, std::time::Instant::now().max(self.epoch)));
        let elapsed = (now_ms - base_ms).max(0) as u64;
        base + std::time::Duration::from_millis(elapsed)
    }

    /// Queue our CAPS once the peer turned out to be streaming a screen.
    fn queue_caps(&mut self) {
        if !self.caps_queued {
            self.caps_queued = true;
            let flags = self.local_caps();
            self.queue_small(&ovapp::terminal::Command::Caps { flags });
        }
    }

    /// The CAPS we advertise: tiles/zstd/keyframes and LZ4 block decode
    /// always (all pure Rust), `CAP_OPUS` only when the shell
    /// installed an Opus sink (it decodes Opus natively).
    fn local_caps(&self) -> u32 {
        // Lossy JPEG atlases + tile cache decode in the shared
        // ScreenDecoder (pure Rust), dirty rects via last_dirty.
        let mut flags = ovapp::terminal::LOCAL_CAPS
            | ovapp::terminal::CAP_LZ4
            | ovapp::terminal::VIEWER_W2_CAPS;
        if self.opus_sink.f.is_some() {
            // The iOS player reads the packet duration from the Opus TOC,
            // so 10 ms packets are fine too.
            flags |= ovapp::terminal::CAP_OPUS | ovapp::terminal::CAP_AUDIO10;
        }
        if self.video_sink.f.is_some() {
            flags |= ovapp::terminal::CAP_H264;
        }
        if self.direct_capable {
            // The session's direct engine speaks the v2 framing and
            // the media channel (video FEC + Opus over unreliable datagrams).
            flags |= ovapp::terminal::CAP_DIRECT | ovapp::terminal::CAP_MEDIA;
        }
        flags
    }

    /// Queue a 9-byte control record for [`ov_screen_take_pong_reply`].
    fn queue_small(&mut self, cmd: &ovapp::terminal::Command) {
        let wire = ovapp::terminal::encode(cmd);
        if let Ok(rec) = <[u8; ovapp::terminal::SMALL_CONTROL_LEN]>::try_from(wire.as_slice()) {
            self.outbox.push_back(rec);
        }
    }

    /// The framebuffer is stale: ask a capable host for a keyframe (rate
    /// limited, at most one queued).
    fn desync(&mut self) {
        if self.peer_caps & ovapp::terminal::CAP_KEYFRAME == 0 {
            return;
        }
        let now = std::time::Instant::now();
        if self
            .last_kf_request
            .is_some_and(|t| now.saturating_duration_since(t) < ovapp::terminal::KEYFRAME_RETRY)
        {
            return;
        }
        let op = ovapp::terminal::OpCode::KeyframeRequest as u8;
        if self.outbox.iter().any(|r| r[0] == op) {
            return;
        }
        self.last_kf_request = Some(now);
        let have_seq = self.dec.seq().unwrap_or(0);
        self.queue_small(&ovapp::terminal::Command::KeyframeRequest { have_seq });
    }
}

/// Create an empty screen assembler.
#[no_mangle]
pub extern "C" fn ov_screen_new() -> *mut OvScreen {
    ffi_guard!(core::ptr::null_mut(), {
        Box::into_raw(Box::new(OvScreen::new()))
    })
}

/// Free an assembler returned by [`ov_screen_new`].
///
/// # Safety
/// `s` must be null or a pointer from [`ov_screen_new`] that was not freed
/// yet; it must not be used afterwards.
#[no_mangle]
pub unsafe extern "C" fn ov_screen_free(s: *mut OvScreen) {
    ffi_guard!((), {
        if !s.is_null() {
            // SAFETY: `s` came from `Box::into_raw` in `ov_screen_new` and is
            // freed only once (caller contract).
            drop(unsafe { Box::from_raw(s) });
        }
    })
}

/// Advertise `CAP_DIRECT` in our CAPS (call before the first
/// screen command, alongside `ov_session_set_direct_sinks`). Returns 0 or -1.
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`], not used
/// concurrently.
#[no_mangle]
pub unsafe extern "C" fn ov_screen_set_direct_capable(s: *mut OvScreen, on: i32) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        // SAFETY: `s` is null or a live screen with no concurrent use
        // (caller contract).
        let Some(screen) = (unsafe { s.as_mut() }) else {
            return -1;
        };
        screen.direct_capable = on != 0;
        0
    })
}

/// Take the next 9-byte control record owed to the peer. First the PONG
/// echo for the last inbound PING (`0x11` opcode + the 8-byte BE
/// nonce); otherwise a queued control record (`CAPS` after the first
/// screen command, `KEYFRAME_REQUEST` after a desync). Writes it into `out`
/// and returns 1; the shell sends it on channel 0. Returns 0 when nothing is
/// pending, -1 on a null handle or a too-small buffer (`cap` must be >= 9).
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`]. The screen is
/// not internally locked: no other call on the same handle may run
/// concurrently (the shell drives it from one serial queue). `out` must be
/// null or writable for `cap` bytes.
#[no_mangle]
pub unsafe extern "C" fn ov_screen_take_pong_reply(
    s: *mut OvScreen,
    out: *mut u8,
    cap: usize,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        // SAFETY: `s` is null or a live screen with no concurrent use
        // (caller contract).
        let Some(screen) = (unsafe { s.as_mut() }) else {
            return -1;
        };
        let reply = if let Some(nonce) = screen.pong_reply_nonce {
            let mut reply = [0u8; 9];
            reply[0] = ovapp::terminal::OpCode::Pong as u8;
            reply[1..].copy_from_slice(&nonce.to_be_bytes());
            reply
        } else if let Some(rec) = screen.outbox.front() {
            *rec
        } else {
            return 0;
        };
        if out.is_null() || cap < 9 {
            return -1;
        }
        if screen.pong_reply_nonce.take().is_none() {
            screen.outbox.pop_front();
        }
        // SAFETY: `out` is non-null and writable for `cap >= 9` bytes
        // (caller contract).
        unsafe { write(&reply, out) };
        1
    })
}

/// The host's CAPS bits (`ovapp::terminal::CAP_*`), 0 until the host
/// answered our CAPS (an old host never does). The shell checks
/// `CAP_INPUT2` (0x40) before sending `POINTER_EX` / `WHEEL` / `KEY_EX` /
/// `TEXT` / `RELEASE_ALL` / `SAS`, and falls back to `POINTER` / `KEY`
/// otherwise. Returns 0 on a null handle.
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`] (not
/// internally locked: call it on the thread that feeds the screen).
#[no_mangle]
pub unsafe extern "C" fn ov_screen_peer_caps(s: *const OvScreen) -> u32 {
    ffi_guard!(0, {
        // SAFETY: `s` is null or a live screen with no concurrent mutation
        // (caller contract).
        unsafe { s.as_ref() }.map_or(0, |s| s.peer_caps)
    })
}
