//! [`TerminalSession`]: its state, construction, CAPS bookkeeping, the send
//! path and the plain accessors. The receive pump lives in `dispatch`, screen
//! and video reassembly in `frames`, file transfers in `files`, the direct
//! path and media channel in `direct`.

use std::time::{Duration, Instant};

use ovcore::session::Session;
use ovtransport::{Carrier, TransportError};

use crate::screen::{Frame, FrameHook};
use crate::tiles::{ScreenDecoder, UpdateMeta};

#[cfg(doc)]
use super::{cap_cursor_shape, chunk_utf8, truncate_utf8, CAP_H264};
use super::{
    encode, Command, CHANNEL, LOCAL_CAPS, MAX_COMMAND_LEN, RECORD_OVERHEAD, VIEWER_W2_CAPS,
};

/// Callback type for inbound commands (see [`TerminalSession::on_cmd`]).
pub type CmdHook = Box<dyn FnMut(&Command)>;

/// Completed inbound frames kept queued at most (see
/// [`TerminalSession::take_frame`]); older ones are dropped.
pub const MAX_QUEUED_FRAMES: usize = 4;

/// How long the viewer waits before repeating a KEYFRAME_REQUEST while it
/// stays out of sync (the request or its answer may be lost on UDP).
pub const KEYFRAME_RETRY: Duration = Duration::from_millis(300);

/// A completed inbound update awaiting `take_frame`: either already applied
/// (a framebuffer snapshot, when an `on_frame` hook forced eager decoding)
/// or still in wire form.
pub(super) enum PendingFrame {
    /// Already applied: a framebuffer snapshot.
    Decoded(Frame),
    /// Still in wire form, applied when taken.
    Wire {
        /// The update's header.
        meta: UpdateMeta,
        /// The update's sequence number.
        seq: u64,
        /// Exactly `meta.compressed_len` wire bytes.
        data: Vec<u8>,
    },
    /// An H.264 access unit, decoded and painted over the framebuffer in
    /// arrival order (never skipped: P-frames chain).
    Video {
        /// The unit's header.
        header: crate::video::VideoHeader,
        /// The Annex-B access unit.
        data: Vec<u8>,
    },
}

impl PendingFrame {
    /// A full frame (supersedes everything queued before it).
    pub(super) fn is_full(&self) -> bool {
        match self {
            PendingFrame::Decoded(_) => true,
            PendingFrame::Wire { meta, .. } => meta.is_full(),
            PendingFrame::Video { .. } => false,
        }
    }
}

/// A terminal session: one [`Session`] + one [`Carrier`], pumped by the caller.
///
/// The struct is deliberately *not* thread-aware: like the rest of the stack,
/// the consumer owns the loop. `pump()` is the only way data moves.
pub struct TerminalSession {
    /// The established record session (encrypt / decrypt).
    pub(super) session: Session,
    /// The transport (possibly wrapped in a direct carrier).
    pub(super) carrier: Box<dyn Carrier>,
    /// Receive hook: every decoded inbound command lands here.
    pub(super) on_cmd: Option<CmdHook>,
    /// FILE_BEGIN name + declared size of the transfer currently in progress.
    pub(super) rx_file: Option<(String, u32)>,
    /// Bytes accumulated for the in-progress inbound transfer.
    pub(super) rx_file_data: Vec<u8>,
    /// Completed inbound transfers, oldest first (drained by [`take_file`](Self::take_file)).
    pub(super) finished_files: std::collections::VecDeque<(String, Vec<u8>)>,
    /// File-id-keyed transfer reassembly (drained by [`take_xfer_file`](Self::take_xfer_file)).
    pub(super) xfer: crate::file_transfer::FileTransfer,
    /// Last PONG seen (for the ping liveness helper).
    pub(super) last_pong: Option<u64>,
    /// In-progress inbound update (None when no FRAME_BEGIN / TILES_BEGIN
    /// is open).
    pub(super) rx_frame: Option<UpdateMeta>,
    /// Pixel bytes accumulated for the in-progress inbound frame.
    pub(super) rx_frame_data: Vec<u8>,
    /// Completed inbound frames, oldest first (drained by [`take_frame`](Self::take_frame) /
    /// [`take_latest_frame`](Self::take_latest_frame)). Kept in wire (still-compressed) form and
    /// capped at [`MAX_QUEUED_FRAMES`] (oldest dropped), so a consumer that
    /// falls behind neither grows memory unboundedly nor pays to inflate
    /// frames it will never show.
    pub(super) finished_frames: std::collections::VecDeque<PendingFrame>,
    /// Recycled byte buffers (wire buffers of consumed/dropped frames), so
    /// reassembly and decompression reuse allocations frame to frame.
    pub(super) spare_bufs: Vec<Vec<u8>>,
    /// Recycled pixel buffers (decoded frames handed back / dropped).
    pub(super) spare_pixels: Vec<Vec<u8>>,
    /// Datagrams received before this session existed (polled in the same
    /// batch that completed the handshake), processed first by `pump`.
    pub(super) backlog: Vec<Vec<u8>>,
    /// Frame receive hook (reassembled frames land here).
    pub(super) on_frame: Option<FrameHook>,
    /// Persistent framebuffer: full frames and tile deltas are
    /// applied here in order; `take_*frame` hands out snapshots of it.
    pub(super) screen: ScreenDecoder,
    /// The peer's CAPS bits (None until it sent CAPS — an old peer never
    /// does).
    pub(super) peer_caps: Option<u32>,
    /// Our CAPS went out (announced, or sent as the reply to the peer's).
    pub(super) caps_sent: bool,
    /// The CAPS bits we advertise.
    pub(super) local_caps: u32,
    /// When the last KEYFRAME_REQUEST went out (rate limit).
    pub(super) last_kf_request: Option<Instant>,
    /// Command-channel counters.
    pub(super) stats: TerminalStats,
    /// The open VIDEO_BEGIN (its FRAME_DATA accumulates in
    /// `rx_frame_data` like a screen update's).
    pub(super) rx_video: Option<(crate::video::VideoHeader, u32)>,
    /// Video continuity (gap -> drop until IDR).
    pub(super) video_rx: crate::video::VideoRx,
    /// The H.264 decoder (installing one advertises [`CAP_H264`]).
    pub(super) video_dec: Option<Box<dyn crate::video::VideoDecode>>,
    /// Decoded picture scratch (tight BGRA).
    pub(super) video_px: Vec<u8>,
    /// When the last VIDEO_IDR_REQUEST went out (rate limit).
    pub(super) last_idr_request: Option<Instant>,
    /// Sequence number of the last complete video unit (IDR requests).
    pub(super) last_video_seq: u64,
    /// H.264 receive counters.
    pub(super) video_stats: VideoRxStats,
    /// Capture-to-paint latency per painted picture (us).
    pub(super) video_latencies: Vec<u32>,
    /// When the last picture of the current motion period was
    /// painted (stall detection; cleared at VIDEO_END).
    pub(super) last_paint: Option<Instant>,
    /// Media channel receive counters (video units, video frames
    /// lost, audio packets, audio packets lost).
    pub(super) media_rx: (u64, u64, u64, u64),
}

/// Viewer-side H.264 counters.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct VideoRxStats {
    /// Access units received (END markers excluded).
    pub units: u64,
    /// Wire bytes of those units.
    pub bytes: u64,
    /// Pictures decoded and painted.
    pub painted: u64,
    /// Units dropped (gap / waiting for an IDR).
    pub dropped: u64,
    /// Units the decoder rejected.
    pub decode_errors: u64,
    /// VIDEO_IDR_REQUESTs sent.
    pub idr_requests: u64,
    /// VIDEO_END markers received (end of a motion period).
    pub ends: u64,
    /// Total decode time.
    pub decode_time: Duration,
    /// Sum of capture-to-paint latency in us (host capture clock vs our
    /// wall clock: meaningful on one machine or with synced clocks).
    pub latency_sum_us: u64,
    /// Number of latency samples in `latency_sum_us`.
    pub latency_n: u64,
    /// Largest capture-to-paint latency seen, in us.
    pub latency_max_us: u64,
    /// Playback stalls — gaps longer than [`VIDEO_STALL_GAP`] between two
    /// painted pictures of one motion period.
    pub stalls: u64,
    /// Total stall time, in us.
    pub stall_us: u64,
}

/// A gap between painted pictures of one motion period longer than
/// this counts as a stall in [`VideoRxStats`].
pub const VIDEO_STALL_GAP: Duration = Duration::from_millis(100);

/// Most per-picture latency samples kept for percentiles.
pub const MAX_LATENCY_SAMPLES: usize = 200_000;

/// Command-channel counters.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TerminalStats {
    /// Commands received (decoded).
    pub cmds_rx: u64,
    /// Commands sent.
    pub cmds_tx: u64,
    /// Records that failed to decrypt (AEAD auth, wrong sender).
    pub decrypt_errors: u64,
    /// Records dropped by the anti-replay window (duplicates, too old).
    pub replayed: u64,
    /// Commands with an unknown opcode or bad payload.
    pub decode_errors: u64,
    /// Screen updates that could not be applied (lost/corrupt update or a
    /// delta without its base) — each one triggers a keyframe request.
    pub screen_desyncs: u64,
    /// KEYFRAME_REQUESTs sent.
    pub keyframe_requests: u64,
}

impl TerminalSession {
    /// Wrap an established session and its carrier.
    pub fn new(session: Session, carrier: impl Carrier + 'static) -> Self {
        Self {
            session,
            carrier: Box::new(carrier),
            on_cmd: None,
            rx_file: None,
            rx_file_data: Vec::new(),
            finished_files: std::collections::VecDeque::new(),
            xfer: crate::file_transfer::FileTransfer::new(),
            last_pong: None,
            rx_frame: None,
            rx_frame_data: Vec::new(),
            finished_frames: std::collections::VecDeque::new(),
            spare_bufs: Vec::new(),
            spare_pixels: Vec::new(),
            backlog: Vec::new(),
            on_frame: None,
            screen: ScreenDecoder::new(),
            peer_caps: None,
            caps_sent: false,
            local_caps: LOCAL_CAPS | VIEWER_W2_CAPS | crate::audio::audio_caps(),
            last_kf_request: None,
            stats: TerminalStats::default(),
            rx_video: None,
            video_rx: crate::video::VideoRx::new(),
            video_dec: None,
            video_px: Vec::new(),
            last_idr_request: None,
            last_video_seq: 0,
            video_stats: VideoRxStats::default(),
            video_latencies: Vec::new(),
            last_paint: None,
            media_rx: (0, 0, 0, 0),
        }
    }

    /// Advertise our capabilities ([`LOCAL_CAPS`]) to the peer — a viewer
    /// calls this once right after the session starts. A host answers with
    /// its own CAPS (see [`peer_caps`](Self::peer_caps)); a host that
    /// predates CAPS ignores it.
    pub fn announce_caps(&mut self) -> Result<(), TransportError> {
        self.caps_sent = true;
        self.send(&Command::Caps {
            flags: self.local_caps,
        })
    }

    /// Override the CAPS bits we advertise (default: [`LOCAL_CAPS`] plus
    /// `crate::audio::audio_caps`). Call before
    /// [`announce_caps`](Self::announce_caps) / the first pump.
    pub fn set_local_caps(&mut self, caps: u32) {
        self.local_caps = caps;
    }

    /// The CAPS bits we advertise.
    pub fn local_caps(&self) -> u32 {
        self.local_caps
    }

    /// The peer's capability bits, once it sent CAPS (`None` = old peer, or
    /// not yet).
    pub fn peer_caps(&self) -> Option<u32> {
        self.peer_caps
    }

    /// The persistent screen framebuffer (e.g. for the dirty rects of the
    /// last applied update).
    pub fn screen(&self) -> &ScreenDecoder {
        &self.screen
    }

    /// Queue datagrams that arrived in the same poll batch that completed
    /// the handshake (records the peer sent right behind its READY). The
    /// next [`pump`](Self::pump) decrypts them, in order, before polling the
    /// carrier — dropping them would desync the record sequence.
    pub fn queue_inbound(&mut self, datagrams: impl IntoIterator<Item = Vec<u8>>) {
        self.backlog.extend(datagrams);
    }

    /// Install the receive hook. Only one hook at a time; pass `None` to clear.
    pub fn on_cmd(&mut self, f: Option<CmdHook>) {
        self.on_cmd = f;
    }

    /// Encode + encrypt + send one command as a single record.
    ///
    /// A command whose encoding exceeds [`MAX_COMMAND_LEN`] is rejected with
    /// [`TransportError::FrameTooLarge`] (the would-be record length) *before*
    /// encryption, so it consumes no send sequence number and the session
    /// stays usable — callers chunk or cap such payloads (see
    /// [`chunk_utf8`], [`truncate_utf8`], [`cap_cursor_shape`]).
    pub fn send(&mut self, cmd: &Command) -> Result<(), TransportError> {
        let wire = encode(cmd);
        if wire.len() > MAX_COMMAND_LEN {
            return Err(TransportError::FrameTooLarge(wire.len() + RECORD_OVERHEAD));
        }
        let record = self.session.encrypt_record(CHANNEL, &wire);
        self.carrier.send(&record)?;
        self.stats.cmds_tx += 1;
        Ok(())
    }

    /// Block up to `timeout`, pumping until `f` returns a value (pump-driven
    /// wait — the standard way tests and the CLI wait for one peer reply).
    /// Returns the first `Some` value `f` produced.
    pub fn wait_for<T: 'static>(
        &mut self,
        timeout: Duration,
        mut f: impl FnMut(&Command) -> Option<T> + 'static,
    ) -> Option<T> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.on_cmd(Some(Box::new(move |cmd| {
            if let Some(v) = f(cmd) {
                let _ = tx.send(v);
            }
        })));
        let deadline = Instant::now() + timeout;
        let out = loop {
            match rx.try_recv() {
                Ok(v) => break Some(v),
                Err(_) if Instant::now() >= deadline => break None,
                Err(_) => {}
            }
            let _ = self.pump();
            std::thread::sleep(Duration::from_millis(2));
        };
        self.on_cmd(None);
        out
    }

    /// The nonce of the most recent PONG received (cleared by [`take_pong`](Self::take_pong)).
    pub fn last_pong(&self) -> Option<u64> {
        self.last_pong
    }

    /// Take the most recent PONG nonce (clearing it).
    pub fn take_pong(&mut self) -> Option<u64> {
        self.last_pong.take()
    }

    /// Mutable access to the underlying session (record counters, etc.).
    pub fn session_mut(&mut self) -> &mut Session {
        &mut self.session
    }

    /// The peer's device ID (from the handshake).
    pub fn peer_device_id(&self) -> u64 {
        self.session.target_id()
    }

    /// Command-channel counters.
    pub fn stats(&self) -> TerminalStats {
        self.stats
    }

    /// Cumulative bytes handed to the carrier (its own queue included; see
    /// [`pending_send`](Self::pending_send)). Link pacing uses `bytes_sent()
    /// - pending_send()` as "bytes that entered the network path".
    pub fn bytes_sent(&self) -> u64 {
        self.carrier.stats().bytes_sent
    }

    /// Cumulative bytes received by the carrier.
    pub fn bytes_received(&self) -> u64 {
        self.carrier.stats().bytes_recv
    }

    /// Outbound bytes the carrier queued but has not handed to the OS yet
    /// (see [`Carrier::pending_send`]); they drain on later sends/pumps.
    pub fn pending_send(&self) -> usize {
        self.carrier.pending_send()
    }

    /// Whether the carrier currently delivers losslessly and in order (a
    /// UDP carrier: once the reliable framing is on).
    pub fn carrier_reliable(&self) -> bool {
        self.carrier.is_reliable()
    }

    /// When the carrier next needs a pump for its own timers (UDP
    /// retransmissions); loops cap their waits with it.
    pub fn transport_deadline(&self) -> Option<Duration> {
        self.carrier.next_deadline()
    }

    /// Close the underlying carrier.
    pub fn close(&mut self) -> Result<(), TransportError> {
        self.carrier.close()
    }
}
