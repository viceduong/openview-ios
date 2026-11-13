//! Prioritized outbound path for the host's screen session (Phase 2).
//!
//! The capture side and the session side run on different threads:
//!
//! - a **capture thread** grabs + change-detects + compresses frames
//!   ([`EncodedFrame::compress`]) and drops each one into a single-slot
//!   [`FrameMailbox`] — a newer frame *replaces* an older one that has not
//!   started sending yet, so a slow link always gets the freshest pixels and
//!   never a backlog;
//! - the **session thread** owns the [`TerminalSession`] (encryption,
//!   framing, the carrier) and drives a [`PrioritySender`]: control commands
//!   (PONG, cursor, audio, clipboard, chat echo, ...) always go first, and
//!   frame records are interleaved one at a time, so a control message never
//!   waits behind more than one frame chunk. At most one frame is in flight:
//!   the mailbox is only consulted once the previous frame fully drained.
//!
//! The sender also measures how long each frame takes to drain
//! ([`DrainMeter`]) so the capture thread can pace itself to what the link
//! actually carries instead of compressing frames that would be replaced
//! before they are sent.
//!
//! # Phase 3: dirty-tile deltas through the mailbox
//!
//! [`UpdateEncoder`] (capture thread) turns each capture into a full frame
//! or a tile delta. A delta must be diffed against the frame the viewer will
//! actually hold when it arrives — the last update the sender **took** from
//! the mailbox — not against the previous capture, because an unsent update
//! can be replaced. So the encoder keeps the tiles dirty since that base
//! (`pending`) and, while its previous update still sits unsent in the
//! mailbox, a new delta carries the union of both (the replaced one's tiles
//! are re-sent with the newest pixels). [`FrameMailbox::put_if_taken`]
//! makes the "which base?" decision atomic with the hand-off: if the sender
//! took the previous update in between, the put is refused and the encoder
//! re-diffs against that one instead.
//!
//! # Phase 4: link pacing
//!
//! The sender owns a [`LinkPacer`]: while frames flow it interleaves tagged
//! probe PINGs, and the PONGs (routed back via [`PrioritySender::on_pong`])
//! give it a delivery rate and min RTT. Frame records are only released
//! while the estimated bytes in flight stay within `rate x (min RTT + 15
//! ms)`, so the kernel/relay buffers never build a standing queue in front
//! of control records (see [`crate::pacing`]).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ovtransport::TransportError;

use crate::bands::{BandPolicy, MoveRect};
use crate::codec::Compressor;
use crate::pacing::LinkPacer;
use crate::screen::{Frame, PixelFormat};
use crate::terminal::{
    compress_frame, Command, FrameCodec, TerminalSession, CAP_BANDS, CAP_LZ4, CAP_MOVE, CAP_TILES,
    CAP_ZSTD,
};
use crate::tiles::{
    copy_rects, diff_tiles, gather, rects_from_map, Delta, TileMap, TileRect, MAX_TILE_FRAME_DIM,
    MAX_TILE_RECTS,
};
use crate::video::VideoHeader;

/// Default FRAME_DATA chunk size: small enough that one chunk occupies a
/// 10 Mbps link for only ~3 ms (the most a control message waits behind
/// frame data at the application layer).
pub const DEFAULT_CHUNK: usize = 4096;

/// One captured frame, already compressed — everything the session thread
/// needs to stream it (no pixel work left on the I/O thread).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    /// Frame sequence number (FRAME_BEGIN / FRAME_END).
    pub seq: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: PixelFormat,
    /// Uncompressed pixel-byte total.
    pub total: u32,
    pub codec: FrameCodec,
    /// The FRAME_DATA payload stream (`compressed_len` bytes).
    pub data: Vec<u8>,
    /// `None` = a full frame (FRAME_BEGIN); `Some` = a tile delta
    /// (TILES_BEGIN) on top of frame `base_seq`.
    pub delta: Option<Delta>,
    /// Phase 5: `Some` = an H.264 access unit (VIDEO_BEGIN; `seq` is the
    /// video seq, `data` Annex-B) instead of a screen update.
    pub video: Option<VideoHeader>,
    /// Phase 5 ordering barrier (set by the mailbox): a screen update is
    /// only sent once this many video units went out before it.
    pub barrier: u64,
}

impl EncodedFrame {
    /// Compress a captured frame with raw DEFLATE (codec 1) — the same wire
    /// form [`TerminalSession::send_frame`] produces.
    pub fn compress(frame: &Frame) -> Self {
        Self {
            seq: frame.seq,
            width: frame.width,
            height: frame.height,
            stride: frame.stride,
            format: frame.format,
            total: frame.pixels.len() as u32,
            codec: FrameCodec::Deflate,
            data: compress_frame(&frame.pixels),
            delta: None,
            video: None,
            barrier: 0,
        }
    }

    /// A video access unit (or a `VIDEO_END` marker with empty `data`).
    pub fn video(header: VideoHeader, data: Vec<u8>) -> Self {
        Self {
            seq: header.seq,
            width: u32::from(header.rect.w),
            height: u32::from(header.rect.h),
            stride: 0,
            format: PixelFormat::Bgra,
            total: 0,
            codec: FrameCodec::Raw,
            data,
            delta: None,
            video: Some(header),
            barrier: 0,
        }
    }

    /// What a receiver reconstructs from [`begin`](Self::begin) (for
    /// decoding an update without a wire round trip).
    pub fn meta(&self) -> crate::tiles::UpdateMeta {
        crate::tiles::UpdateMeta {
            width: self.width,
            height: self.height,
            stride: self.stride,
            format: self.format,
            total: self.total,
            codec: self.codec,
            compressed_len: self.data.len() as u32,
            delta: self.delta.clone(),
        }
    }

    /// The header record that opens this update on the wire.
    pub fn begin(&self) -> Command {
        if let Some(header) = self.video {
            return Command::VideoBegin {
                header,
                compressed_len: self.data.len() as u32,
            };
        }
        match &self.delta {
            None => Command::FrameBegin {
                seq: self.seq,
                width: self.width,
                height: self.height,
                stride: self.stride,
                format: self.format,
                total: self.total,
                codec: self.codec,
                compressed_len: self.data.len() as u32,
            },
            Some(d) => Command::TilesBegin {
                seq: self.seq,
                base_seq: d.base_seq,
                width: self.width,
                height: self.height,
                format: self.format,
                codec: self.codec,
                total: self.total,
                compressed_len: self.data.len() as u32,
                rects: d.rects.clone(),
            },
        }
    }
}

/// Wake-up callback a [`FrameMailbox`] fires after each `put` (e.g. signal
/// the session thread's wait event).
pub type Notify = Box<dyn Fn() + Send + Sync>;

/// Single-slot "latest frame" mailbox between the capture thread and the
/// session thread. `put` replaces any frame that has not been taken yet.
pub struct FrameMailbox {
    slot: Mutex<Slot>,
    notify: Option<Notify>,
    replaced: AtomicU64,
}

#[derive(Default)]
struct Slot {
    frame: Option<EncodedFrame>,
    /// Seq of the last frame `take` handed to the sender (the viewer's
    /// next delta base).
    last_taken: Option<u64>,
    /// Phase 5: video access units, strictly FIFO (never replaced).
    video: VecDeque<EncodedFrame>,
    /// Video units ever pushed / taken (screen-update barrier).
    video_pushed: u64,
    video_taken: u64,
    /// The last `take` returned a video unit (alternate when both wait).
    last_was_video: bool,
}

impl Default for FrameMailbox {
    fn default() -> Self {
        Self::new(None)
    }
}

impl FrameMailbox {
    /// An empty mailbox; `notify` (if any) runs after every `put`.
    pub fn new(notify: Option<Notify>) -> Self {
        Self {
            slot: Mutex::new(Slot::default()),
            notify,
            replaced: AtomicU64::new(0),
        }
    }

    /// Offer a frame. Returns `true` when it replaced an older frame that
    /// was never sent (the older one is dropped).
    pub fn put(&self, frame: EncodedFrame) -> bool {
        self.put_if_taken(frame, None, false).unwrap_or_default()
    }

    /// Offer a frame only if the last frame the sender took is still
    /// `expect` (the base the frame was diffed against). `Err` hands the
    /// frame back untouched when the sender took something else meanwhile
    /// (re-encode against the new base). `Ok(replaced)` as for [`put`].
    pub fn put_if_taken(
        &self,
        frame: EncodedFrame,
        expect: Option<u64>,
        check: bool,
    ) -> Result<bool, Box<EncodedFrame>> {
        let replaced = match self.slot.lock() {
            Ok(mut s) => {
                if check && s.last_taken != expect {
                    return Err(Box::new(frame));
                }
                // Video units queued before this update go out first (a
                // VIDEO_END must precede the region's lossless refresh).
                let mut frame = frame;
                frame.barrier = s.video_pushed;
                s.frame.replace(frame).is_some()
            }
            Err(_) => return Err(Box::new(frame)),
        };
        if replaced {
            self.replaced.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(n) = &self.notify {
            n();
        }
        Ok(replaced)
    }

    /// Take the next record stream to send: the waiting screen update (it
    /// becomes the next delta base) or the oldest video unit. A screen
    /// update waits for the video units queued before it; when both are
    /// eligible they alternate, so neither starves the other.
    pub fn take(&self) -> Option<EncodedFrame> {
        let mut s = self.slot.lock().ok()?;
        let frame_ok = s.frame.as_ref().is_some_and(|f| f.barrier <= s.video_taken);
        let video_ok = !s.video.is_empty();
        let take_video = video_ok && (!frame_ok || !s.last_was_video);
        if take_video {
            let v = s.video.pop_front()?;
            s.video_taken += 1;
            s.last_was_video = true;
            return Some(v);
        }
        if !frame_ok {
            return None;
        }
        let f = s.frame.take()?;
        s.last_taken = Some(f.seq);
        s.last_was_video = false;
        Some(f)
    }

    /// Queue a video access unit (Phase 5): FIFO, never replaced.
    pub fn push_video(&self, unit: EncodedFrame) {
        if let Ok(mut s) = self.slot.lock() {
            s.video.push_back(unit);
            s.video_pushed += 1;
        }
        if let Some(n) = &self.notify {
            n();
        }
    }

    /// Video units waiting to be sent.
    pub fn video_queued(&self) -> usize {
        self.slot.lock().map(|s| s.video.len()).unwrap_or(0)
    }

    /// Seq of the last frame [`take`](Self::take) returned.
    pub fn last_taken(&self) -> Option<u64> {
        self.slot.lock().ok().and_then(|s| s.last_taken)
    }

    /// Whether a frame (or a video unit) is waiting.
    pub fn is_pending(&self) -> bool {
        self.slot
            .lock()
            .map(|s| s.frame.is_some() || !s.video.is_empty())
            .unwrap_or(false)
    }

    /// Frames dropped because a newer one replaced them before sending.
    pub fn replaced_count(&self) -> u64 {
        self.replaced.load(Ordering::Relaxed)
    }
}

/// What [`UpdateEncoder::submit`] did with a capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitOutcome {
    /// Identical to the last queued frame: nothing queued.
    Unchanged,
    /// A full frame was queued (`bytes` on the wire).
    Full { bytes: usize },
    /// A tile delta was queued: dirty tiles (incl. ones re-sent for a
    /// replaced unsent update), header rects, wire bytes.
    Delta {
        tiles: usize,
        rects: usize,
        bytes: usize,
    },
}

/// Capture-thread encoder (Phase 3): diffs each capture, decides full frame
/// vs tile delta, compresses, and hands the result to a [`FrameMailbox`]
/// with the right base (see the module docs).
///
/// Mode follows the viewer's CAPS ([`set_peer_caps`](Self::set_peer_caps)):
/// without [`CAP_TILES`] every change is a DEFLATE full frame exactly as
/// before Phase 3; zstd only with [`CAP_ZSTD`] (and a `zstd-encode` build).
pub struct UpdateEncoder {
    peer_caps: u32,
    seq: u64,
    /// Pixels of the last queued update (tight BGRA).
    last_px: Vec<u8>,
    last_dims: Option<(u32, u32)>,
    /// Seq of the last queued update.
    last_put: Option<u64>,
    /// The frame the viewer holds before our queued update lands (the
    /// mailbox's last-taken when we queued).
    base: Option<u64>,
    /// Tiles that differ between `base` and `last_px`.
    pending: TileMap,
    /// Scratch: tiles that differ between `last_px` and the new capture.
    fresh: TileMap,
    /// Scratch: tiles a delta must carry.
    acc: TileMap,
    force_full: bool,
    rects: Vec<TileRect>,
    gather_buf: Vec<u8>,
    comp: Compressor,
    /// Phase 5: tiles of the video region (scratch).
    mask_map: TileMap,
    /// `fresh` holds real changes (not a forced fill).
    fresh_valid: bool,
    /// Wave 1: MOVE records of the update being built (scratch).
    moves: Vec<MoveRect>,
    /// Wave 1: hint rects for the post-move re-diff (scratch).
    move_hints: Vec<TileRect>,
    /// Wave 1: counters (moves sent, band codecs used).
    stats: EncoderStats,
    /// Wave 1: tiles still owed a lossless refresh (after H.264 motion),
    /// sent a bounded chunk per update ([`REFRESH_CHUNK_BYTES`]).
    backlog: TileMap,
    /// Scratch: this update's refresh chunk.
    chunk: TileMap,
    /// Smoothed wire bytes per refreshed tile (chunk sizing).
    refresh_tile_bytes: f64,
    /// Wave 1: the pacer's delivery rate (bytes/s, 0 = unknown): LZ4 bands
    /// only on a link fast enough that bytes matter less than decode time.
    link_rate: u64,
    // ---- Wave 2 (lossy-first images, refinement lane, tile cache) ----
    /// Host switches (both default on; effective only for a viewer with
    /// the CAPS bits).
    lossy_on: bool,
    cache_on: bool,
    lossy_params: crate::lossy::LossyParams,
    /// Per tile: ms (since `epoch`) of its last change. `backlog` tiles
    /// are refined once they stayed unchanged [`REFINE_DELAY`].
    changed_ms: Vec<u32>,
    epoch: Instant,
    /// Test clock override.
    clock: Option<Instant>,
    /// Viewer tile-cache model as of the last update the sender took ...
    cache_committed: crate::lossy::TileCache,
    /// ... and after our last queued (not yet taken) update.
    cache_tentative: Option<crate::lossy::TileCache>,
    /// Our last queued update is a full frame (a follow-up that replaces
    /// it must be full too).
    pending_full: bool,
    /// Scratch maps: image tiles, cache hits, lossless tiles, trimmable
    /// lossless tiles, tiles a MOVE fills with lossy pixels, classifier
    /// candidates.
    img_map: TileMap,
    ref_map: TileMap,
    lossless_map: TileMap,
    trim_map: TileMap,
    move_lossy: TileMap,
    cand_map: TileMap,
    /// Scratch: a full frame with its lossy tiles blanked; a copy of
    /// `last_px` for refine-only updates.
    blank_buf: Vec<u8>,
    refine_buf: Vec<u8>,
}

/// Wave 2: a lossy tile is refined to exact pixels once it stayed
/// unchanged this long.
pub const REFINE_DELAY: Duration = Duration::from_millis(300);

/// Wave 2: a refinement chunk occupies the link for at most about this
/// long (at the pacer's delivery rate), so a change behind it waits little.
pub const REFINE_SLICE: Duration = Duration::from_millis(15);

/// Wave 2: smallest refinement chunk (bytes) on a slow link.
pub const REFINE_MIN_CHUNK: usize = 8 * 1024;

/// Wave 1: LZ4 (fast decode, ~15 % bigger on photo content) is only chosen
/// for low-ratio bands when the link delivers at least this (bytes/s,
/// ~32 Mbit/s: LAN / direct path); slower or unknown links get zstd.
pub const LZ4_MIN_LINK_RATE: u64 = 4_000_000;

/// Wave 1: the lossless refresh after H.264 motion goes out in chunks of
/// about this many wire bytes, one per update, so an input-driven change
/// never waits behind a whole-region refresh.
pub const REFRESH_CHUNK_BYTES: usize = 64 * 1024;

/// Wave 1 [`UpdateEncoder`] counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EncoderStats {
    /// Updates that carried a MOVE (scroll / OS move rect).
    pub moves: u64,
    /// Of those, moves found by the scroll detector (the rest came from
    /// verified OS move rects).
    pub detected_moves: u64,
    /// Tiles the moves saved (dirty before - residual after).
    pub tiles_saved: u64,
    /// Bands sent as LZ4 / zstd / raw in band containers.
    pub lz4_bands: u64,
    pub zstd_bands: u64,
    pub raw_bands: u64,
    /// Wave 2: tiles sent as lossy JPEG / their JPEG bytes.
    pub lossy_tiles: u64,
    pub lossy_bytes: u64,
    /// Wave 2: lossy (or video-painted) tiles refined to exact pixels.
    pub refined_tiles: u64,
    /// Wave 2: tiles sent as cache REFs / slots filled by STOREs.
    pub cache_refs: u64,
    pub cache_stores: u64,
}

impl Default for UpdateEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl UpdateEncoder {
    pub fn new() -> Self {
        Self {
            peer_caps: 0,
            seq: 0,
            last_px: Vec::new(),
            last_dims: None,
            last_put: None,
            base: None,
            pending: TileMap::default(),
            fresh: TileMap::default(),
            acc: TileMap::default(),
            force_full: true,
            rects: Vec::new(),
            gather_buf: Vec::new(),
            comp: Compressor::new(),
            mask_map: TileMap::default(),
            fresh_valid: false,
            moves: Vec::new(),
            move_hints: Vec::new(),
            stats: EncoderStats::default(),
            backlog: TileMap::default(),
            chunk: TileMap::default(),
            refresh_tile_bytes: 0.0,
            link_rate: 0,
            lossy_on: true,
            cache_on: true,
            lossy_params: crate::lossy::LossyParams::default(),
            changed_ms: Vec::new(),
            epoch: Instant::now(),
            clock: None,
            cache_committed: crate::lossy::TileCache::default(),
            cache_tentative: None,
            pending_full: false,
            img_map: TileMap::default(),
            ref_map: TileMap::default(),
            lossless_map: TileMap::default(),
            trim_map: TileMap::default(),
            move_lossy: TileMap::default(),
            cand_map: TileMap::default(),
            blank_buf: Vec::new(),
            refine_buf: Vec::new(),
        }
    }

    /// Wave 2: allow lossy-first image tiles (default on; only used for a
    /// viewer with `CAP_LOSSY` + `CAP_BANDS`).
    pub fn set_lossy(&mut self, on: bool) {
        self.lossy_on = on;
    }

    /// Wave 2: allow the tile cache (default on; only used for a viewer
    /// with `CAP_CACHE` + `CAP_BANDS`).
    pub fn set_cache(&mut self, on: bool) {
        self.cache_on = on;
    }

    /// Wave 2: tile-cache slots to use (default [`crate::lossy::CACHE_SLOTS`],
    /// at most [`crate::lossy::MAX_SLOTS`]). Forgets the cache and makes
    /// the next update a full frame (which empties the viewer's slots).
    pub fn set_cache_slots(&mut self, n: usize) {
        self.cache_committed = crate::lossy::TileCache::new(n);
        self.cache_tentative = None;
        self.force_full = true;
    }

    /// Wave 2: JPEG quality / chroma for image tiles.
    pub fn set_lossy_params(&mut self, p: crate::lossy::LossyParams) {
        self.lossy_params = p;
    }

    /// Wave 2: pin the encoder's clock (tests / simulations); `None` =
    /// real time.
    pub fn set_clock(&mut self, now: Option<Instant>) {
        self.clock = now;
    }

    /// Encoder clock in ms, offset so that 0 (a never-changed tile) reads
    /// as "settled long ago".
    fn now_ms(&self) -> u32 {
        const OFFSET_MS: u128 = 60_000;
        let now = self.clock.unwrap_or_else(Instant::now);
        (now.saturating_duration_since(self.epoch).as_millis() + OFFSET_MS)
            .min(u128::from(u32::MAX - 1)) as u32
    }

    /// Lossy images for this viewer?
    fn lossy_active(&self) -> bool {
        let need = crate::terminal::CAP_LOSSY | CAP_BANDS;
        self.lossy_on && self.peer_caps & need == need
    }

    /// Tile cache for this viewer?
    fn cache_active(&self) -> bool {
        let need = crate::terminal::CAP_CACHE | CAP_BANDS;
        self.cache_on && self.peer_caps & need == need
    }

    /// Shape `changed_ms` for a `w x h` frame (a new shape: everything
    /// "changed now").
    fn ensure_change_clock(&mut self, w: u32, h: u32, now: u32) {
        let n = (w.div_ceil(crate::tiles::TILE) * h.div_ceil(crate::tiles::TILE)) as usize;
        if self.changed_ms.len() != n {
            self.changed_ms.clear();
            self.changed_ms.resize(n, now);
        }
    }

    /// Tile `i` changed less than [`REFINE_DELAY`] ago?
    fn recent(&self, i: usize, now: u32) -> bool {
        self.changed_ms
            .get(i)
            .is_some_and(|&t| now.saturating_sub(t) < REFINE_DELAY.as_millis() as u32)
    }

    /// Wave 2: a lossy / video-painted tile has settled and waits for its
    /// refinement (the capture loop then calls
    /// [`submit_refine`](Self::submit_refine) whenever the link is free).
    pub fn refine_due(&self) -> bool {
        if self.backlog.count() == 0 || self.last_dims.is_none() {
            return false;
        }
        let now = self.now_ms();
        let cols = self.backlog.cols();
        (0..self.backlog.rows()).any(|r| {
            (0..cols).any(|c| self.backlog.get(c, r) && !self.recent((r * cols + c) as usize, now))
        })
    }

    /// Wave 2 refinement lane: when the previous update was taken (the
    /// link is not backed up) and a lossy tile has settled, queue an
    /// update carrying only a refinement chunk (exact pixels of the last
    /// capture, at most [`REFRESH_CHUNK_BYTES`] / [`REFINE_SLICE`] of link
    /// time). Needs no new capture. `Unchanged` when there is nothing to do.
    pub fn submit_refine(&mut self, mailbox: &FrameMailbox) -> SubmitOutcome {
        let Some((w, h)) = self.last_dims else {
            return SubmitOutcome::Unchanged;
        };
        if self.force_full
            || self.last_put.is_none()
            || mailbox.last_taken() != self.last_put
            || !self.refine_due()
        {
            return SubmitOutcome::Unchanged;
        }
        let mut buf = std::mem::take(&mut self.refine_buf);
        buf.clear();
        buf.extend_from_slice(&self.last_px);
        let out = self.submit_ex(&buf, w, h, Some(&[]), None, None, None, mailbox);
        self.refine_buf = buf;
        out
    }

    /// Wave 1: the link pacer's delivery-rate estimate (bytes/s, 0 = none
    /// yet); see [`LZ4_MIN_LINK_RATE`].
    pub fn set_link_rate(&mut self, bytes_per_sec: u64) {
        self.link_rate = bytes_per_sec;
    }

    /// Wave 1: owe `r` a lossless refresh, sent in low-priority chunks of
    /// about [`REFRESH_CHUNK_BYTES`] over the following submits (each also
    /// carrying whatever changed). Ignored before the first frame.
    ///
    /// Wave 2: this is the refinement lane — the region's tiles join the
    /// lossy backlog (the viewer painted video there) already settled, so
    /// they are refined right away, interleaved with normal updates.
    pub fn queue_refresh(&mut self, r: TileRect) {
        let Some((w, h)) = self.last_dims else { return };
        if self.backlog.cols() != w.div_ceil(crate::tiles::TILE)
            || self.backlog.rows() != h.div_ceil(crate::tiles::TILE)
        {
            self.backlog.reset(w, h);
        }
        let now = self.now_ms();
        self.ensure_change_clock(w, h, now);
        let mut m = TileMap::new(w, h);
        m.mark_rect(&r);
        let settled = now.saturating_sub(REFINE_DELAY.as_millis() as u32);
        for row in 0..m.rows() {
            for col in 0..m.cols() {
                if m.get(col, row) {
                    let i = (row * m.cols() + col) as usize;
                    self.changed_ms[i] = self.changed_ms[i].min(settled);
                }
            }
        }
        self.backlog.union_with(&m);
    }

    /// Wave 1: tiles still waiting for their lossless refresh.
    pub fn refresh_pending(&self) -> usize {
        self.backlog.count()
    }

    /// This update's refresh chunk (the first tiles of the backlog, in
    /// raster order) into `self.chunk`.
    fn take_chunk(&mut self, w: u32, h: u32, now: u32) {
        self.chunk.reset(w, h);
        if self.backlog.count() == 0
            || self.backlog.cols() != self.chunk.cols()
            || self.backlog.rows() != self.chunk.rows()
        {
            self.backlog.reset(w, h);
            return;
        }
        let per_tile = if self.refresh_tile_bytes > 0.0 {
            self.refresh_tile_bytes
        } else {
            8192.0
        };
        // Wave 2: at most REFINE_SLICE of link time per chunk.
        let budget = if self.link_rate > 0 {
            ((self.link_rate as f64 * REFINE_SLICE.as_secs_f64()) as usize)
                .clamp(REFINE_MIN_CHUNK, REFRESH_CHUNK_BYTES)
        } else {
            REFRESH_CHUNK_BYTES
        };
        let mut n = ((budget as f64 / per_tile) as usize).clamp(2, 64);
        let cols = self.backlog.cols();
        'rows: for r in 0..self.backlog.rows() {
            for c in 0..cols {
                // Wave 2: only settled tiles (a tile still changing is
                // re-sent lossy anyway).
                if self.backlog.get(c, r) && !self.recent((r * cols + c) as usize, now) {
                    self.chunk.set(c, r);
                    n -= 1;
                    if n == 0 {
                        break 'rows;
                    }
                }
            }
        }
    }

    /// Wave 1 counters (moves, band codecs).
    pub fn stats(&self) -> EncoderStats {
        self.stats
    }

    /// Compress an update payload for the viewer: a band container (with
    /// `moves`) for a CAP_BANDS viewer; otherwise zstd as parallel
    /// concatenated bands for a large payload, or the plain codec.
    fn compress_payload(
        &mut self,
        codec: FrameCodec,
        data: &[u8],
        moves: &[MoveRect],
        extras: &crate::bands::Extras<'_>,
    ) -> (FrameCodec, Vec<u8>) {
        if self.peer_caps & CAP_BANDS != 0 {
            let policy = BandPolicy {
                zstd: self.peer_caps & CAP_ZSTD != 0 && crate::codec::zstd_encode_available(),
                lz4: self.peer_caps & CAP_LZ4 != 0 && self.link_rate >= LZ4_MIN_LINK_RATE,
                ..BandPolicy::default()
            };
            let mut out = Vec::new();
            let st = crate::bands::encode_packed_ex(data, moves, extras, &policy, &mut out);
            self.stats.lz4_bands += st.lz4_bands as u64;
            self.stats.zstd_bands += st.zstd_bands as u64;
            self.stats.raw_bands += st.raw_bands as u64;
            return (FrameCodec::Packed, out);
        }
        debug_assert!(moves.is_empty(), "MOVEs need CAP_BANDS");
        if codec == FrameCodec::Zstd && data.len() >= 2 * crate::bands::MIN_BAND {
            if let Some(v) = crate::bands::compress_zstd_bands(data, crate::bands::TARGET_BANDS) {
                return (FrameCodec::Zstd, v);
            }
        }
        self.comp.compress(codec, data)
    }

    /// Wave 1 copy-rect: find a MOVE that turns `last_px` (what the viewer
    /// will hold) into most of `px` — a verified OS move rect first, else
    /// the scroll detector over the dirty band. On success the move is
    /// applied to `last_px`, `fresh` becomes the residual tiles (so a scroll
    /// is not "motion" for the H.264 detector) and the move is recorded in
    /// `self.moves`.
    fn find_move(
        &mut self,
        px: &[u8],
        w: u32,
        h: u32,
        stride: usize,
        os_moves: Option<&[MoveRect]>,
    ) {
        let before = self.fresh.count();
        let mut found: Option<(MoveRect, bool)> = None;
        for m in os_moves.unwrap_or(&[]) {
            let ok = crate::scroll::verify_move(&self.last_px, px, w, h, stride, m);
            if m.h > 0 && ok * 10 >= usize::from(m.h) * 9 && (m.dx != 0 || m.dy != 0) {
                found = Some((*m, false));
                break;
            }
        }
        if found.is_none() && before >= 8 {
            if let Some(area) = crate::scroll::dirty_bbox(&self.fresh, w, h) {
                found = crate::scroll::detect_vertical(&self.last_px, px, stride, area)
                    .map(|m| (m, true));
            }
        }
        let Some((m, detected)) = found else { return };
        if crate::bands::apply_move(&mut self.last_px, w, h, stride, 4, &m).is_err() {
            return;
        }
        // Differences after the move lie inside the old dirty tiles or the
        // move's destination.
        rects_from_map(&self.fresh, w, h, &mut self.move_hints);
        self.move_hints.push(m.dst());
        let hints = std::mem::take(&mut self.move_hints);
        diff_tiles(
            &self.last_px,
            px,
            w,
            h,
            stride,
            4,
            Some(&hints),
            &mut self.fresh,
        );
        self.move_hints = hints;
        self.moves.push(m);
        self.stats.moves += 1;
        self.stats.detected_moves += u64::from(detected);
        self.stats.tiles_saved += before.saturating_sub(self.fresh.count()) as u64;
    }

    /// The viewer's CAPS bits (0 = an old viewer: full DEFLATE frames).
    pub fn set_peer_caps(&mut self, caps: u32) {
        self.peer_caps = caps;
    }

    /// Next submit sends a full frame, changed or not (KEYFRAME_REQUEST).
    pub fn request_keyframe(&mut self) {
        self.force_full = true;
    }

    /// Forget the previous capture (monitor switch / resolution change):
    /// the next submit is a full frame.
    pub fn reset(&mut self) {
        self.force_full = true;
        self.last_dims = None;
    }

    /// Seq the next queued update will carry.
    pub fn next_seq(&self) -> u64 {
        self.seq
    }

    fn codec(&self) -> FrameCodec {
        if self.peer_caps & CAP_ZSTD != 0 {
            FrameCodec::Zstd
        } else {
            FrameCodec::Deflate
        }
    }

    /// Encode one capture (`px` = `w x h` tight BGRA) and queue it in
    /// `mailbox` unless nothing changed. `hints` (the OS's dirty/move rects
    /// since the previous capture) narrow the compare; `None` compares
    /// every tile.
    pub fn submit(
        &mut self,
        px: &[u8],
        w: u32,
        h: u32,
        hints: Option<&[TileRect]>,
        mailbox: &FrameMailbox,
    ) -> SubmitOutcome {
        self.submit_masked(px, w, h, hints, None, None, mailbox)
    }

    /// The tiles that changed between the previous capture and the last one
    /// submitted (the motion detector's input).
    pub fn fresh(&self) -> &TileMap {
        &self.fresh
    }

    /// Whether [`fresh`](Self::fresh) reflects real changes (false when the
    /// last submit was a forced full frame and the map was simply filled).
    pub fn fresh_valid(&self) -> bool {
        self.fresh_valid
    }

    /// [`submit`](Self::submit) with the Phase 5 video region: tiles
    /// touching `mask` (the region streamed as H.264) are left out of tile
    /// deltas — the viewer shows video there — and tiles touching `refresh`
    /// are sent whether they changed or not (the lossless refresh after
    /// `VIDEO_END`). Full frames always carry everything.
    #[allow(clippy::too_many_arguments)]
    pub fn submit_masked(
        &mut self,
        px: &[u8],
        w: u32,
        h: u32,
        hints: Option<&[TileRect]>,
        mask: Option<TileRect>,
        refresh: Option<TileRect>,
        mailbox: &FrameMailbox,
    ) -> SubmitOutcome {
        self.submit_ex(px, w, h, hints, None, mask, refresh, mailbox)
    }

    /// [`submit_masked`](Self::submit_masked) plus Wave 1 copy-rect: for a
    /// viewer with `CAP_MOVE` + `CAP_BANDS`, `os_moves` (the OS's move
    /// rects, region coordinates — verified against the mirror before use)
    /// or the scroll detector turn a scroll into one MOVE record plus the
    /// residual tiles. Scrolls then do not count as motion
    /// ([`fresh`](Self::fresh) holds only the residual).
    #[allow(clippy::too_many_arguments)]
    pub fn submit_ex(
        &mut self,
        px: &[u8],
        w: u32,
        h: u32,
        hints: Option<&[TileRect]>,
        os_moves: Option<&[MoveRect]>,
        mask: Option<TileRect>,
        refresh: Option<TileRect>,
        mailbox: &FrameMailbox,
    ) -> SubmitOutcome {
        let stride = w as usize * 4;
        if px.len() < stride * h as usize || w == 0 || h == 0 {
            return SubmitOutcome::Unchanged; // malformed capture: never queue
        }
        let px = &px[..stride * h as usize];
        let new_dims = self.last_dims != Some((w, h)) || self.last_px.len() != px.len();
        let mut force = self.force_full || new_dims;
        self.fresh_valid = !force;
        if force {
            self.fresh.reset(w, h);
            self.fresh.fill();
        } else if diff_tiles(&self.last_px, px, w, h, stride, 4, hints, &mut self.fresh) == 0
            && refresh.is_none()
            && !self.refine_due()
        {
            return SubmitOutcome::Unchanged;
        }
        if force {
            self.backlog.reset(w, h); // a full frame refreshes everything
        }
        if let Some(m) = mask {
            // Back in motion there: video covers it again.
            let mut mm = TileMap::new(w, h);
            mm.mark_rect(&m);
            self.backlog.subtract(&mm);
        }
        let now = self.now_ms();
        if new_dims {
            self.changed_ms.clear();
        }
        self.ensure_change_clock(w, h, now);
        self.take_chunk(w, h, now);
        let tiles_ok =
            self.peer_caps & CAP_TILES != 0 && w <= MAX_TILE_FRAME_DIM && h <= MAX_TILE_FRAME_DIM;
        // Wave 1 copy-rect, only when the viewer will hold exactly
        // `last_px` (our previous update was taken: the move is relative
        // to it) and no video region / forced refresh is involved.
        self.moves.clear();
        let move_caps = CAP_MOVE | CAP_BANDS;
        if !force
            && tiles_ok
            && mask.is_none()
            && refresh.is_none()
            && self.peer_caps & move_caps == move_caps
            && self.last_put.is_some()
            && mailbox.last_taken() == self.last_put
        {
            self.find_move(px, w, h, stride, os_moves);
        }
        let moves = std::mem::take(&mut self.moves);
        // Wave 2: the tiles that really changed restart their refine clock.
        {
            let cols = self.fresh.cols();
            for r in 0..self.fresh.rows() {
                for c in 0..cols {
                    if self.fresh.get(c, r) {
                        self.changed_ms[(r * cols + c) as usize] = now;
                    }
                }
            }
        }
        // Wave 2: a MOVE whose source holds lossy pixels carries them to
        // its destination (those tiles join the backlog with the update).
        self.move_lossy.reset(w, h);
        for m in &moves {
            let src = TileRect {
                x: (i32::from(m.x) - i32::from(m.dx)) as u16,
                y: (i32::from(m.y) - i32::from(m.dy)) as u16,
                w: m.w,
                h: m.h,
            };
            if crate::tiles::map_touches(&self.backlog, &src) {
                self.move_lossy.mark_rect(&m.dst());
            }
        }
        if let Some(m) = mask {
            self.mask_map.reset(w, h);
            self.mask_map.mark_rect(&m);
        }
        let lossy_on = self.lossy_active() && tiles_ok;
        let cache_on = self.cache_active() && tiles_ok;
        let codec = self.codec();
        let seq = self.seq;
        let outcome = loop {
            let taken = mailbox.last_taken();
            // Our previous update already went to the sender: the viewer
            // will hold exactly `last_px`, so only the fresh tiles matter.
            // Otherwise it still waits (and is about to be replaced): carry
            // its tiles too, against the older base.
            let chained = taken.is_some() && taken == self.last_put;
            if chained {
                // Wave 2: the viewer will apply our previous update: its
                // cache operations are now the committed state.
                if let Some(t) = self.cache_tentative.take() {
                    self.cache_committed = t;
                }
            }
            if !chained && !moves.is_empty() {
                // Cannot happen (only our own put changes the base), but a
                // move relative to the wrong base would corrupt: go full.
                force = true;
            }
            let base = if chained { taken } else { self.base };
            self.acc.copy_from(&self.fresh);
            if !chained {
                self.acc.union_with(&self.pending);
            }
            if let Some(r) = refresh {
                self.acc.mark_rect(&r);
            }
            self.acc.union_with(&self.chunk);
            if mask.is_some() && !force {
                self.acc.subtract(&self.mask_map);
                if self.acc.count() == 0 {
                    // Every change is inside the video region: no tile
                    // update (the region is refreshed when video ends).
                    self.advance_last_px(px, w, h, stride, new_dims);
                    return SubmitOutcome::Unchanged;
                }
            }
            let full_needed = force
                || !tiles_ok
                || base.is_none()
                || (!chained && self.pending_full)
                // A cache viewer takes big deltas too: a full frame would
                // empty its cache (window switching is the cache's case).
                || (self.acc.mostly_dirty() && !cache_on);
            if !full_needed {
                // ---- Wave 2 split: cache hits / lossy images / lossless.
                let mut cache = cache_on.then(|| self.cache_committed.clone());
                self.ref_map.reset(w, h);
                self.img_map.reset(w, h);
                let mut refs = Vec::new();
                let mut hashes: Vec<(u32, u32, u128)> = Vec::new();
                if let Some(c) = cache.as_mut() {
                    hashes = tile_hashes(px, w, h, stride, &self.acc);
                    for &(col, row, hv) in &hashes {
                        if let Some(slot) = c.lookup(hv) {
                            refs.push(crate::bands::CacheOp {
                                slot,
                                col: col as u16,
                                row: row as u16,
                            });
                            self.ref_map.set(col, row);
                        }
                    }
                }
                if lossy_on {
                    self.cand_map.copy_from(&self.acc);
                    self.cand_map.subtract(&self.ref_map);
                    let cols = self.cand_map.cols();
                    for r in 0..self.cand_map.rows() {
                        for c in 0..cols {
                            if self.cand_map.get(c, r) && !self.recent((r * cols + c) as usize, now)
                            {
                                self.cand_map.unset(c, r);
                            }
                        }
                    }
                    crate::lossy::classify_tiles(
                        px,
                        w,
                        h,
                        stride,
                        &self.cand_map,
                        &mut self.img_map,
                    );
                }
                let images = if self.img_map.count() > 0 {
                    let list = map_tiles(&self.img_map);
                    match crate::lossy::encode_images(px, w, h, stride, &list, self.lossy_params) {
                        Some(v) => v,
                        None => {
                            self.img_map.reset(w, h); // encode failed: lossless
                            Vec::new()
                        }
                    }
                } else {
                    Vec::new()
                };
                self.lossless_map.copy_from(&self.acc);
                self.lossless_map.subtract(&self.ref_map);
                self.lossless_map.subtract(&self.img_map);
                // Rows of a changed tile that did not change need not be
                // sent when the viewer holds `last_px` exactly there (a 24
                // px scroll strip is 24 rows, a caret ~16). Tiles owed a
                // refinement (lossy on the viewer) always go whole.
                self.trim_map.reset(w, h);
                if chained && refresh.is_none() {
                    self.trim_map.copy_from(&self.lossless_map);
                    self.trim_map.subtract(&self.backlog);
                    self.trim_map.subtract(&self.move_lossy);
                    self.trim_map.subtract(&self.chunk);
                }
                let mut whole = self.lossless_map.clone();
                whole.subtract(&self.trim_map);
                rects_from_map(&whole, w, h, &mut self.rects);
                let mut trimmed = Vec::new();
                rects_from_map(&self.trim_map, w, h, &mut trimmed);
                trim_rows(&self.last_px, px, stride, &mut trimmed);
                self.rects.extend_from_slice(&trimmed);
                if self.rects.len() > MAX_TILE_RECTS {
                    force = true;
                    continue;
                }
                // Cache STOREs: every lossless tile is exact on the viewer
                // after this update.
                let mut stores = Vec::new();
                if let Some(c) = cache.as_mut() {
                    for &(col, row, hv) in &hashes {
                        if self.lossless_map.get(col, row) && !c.contains(hv) {
                            let slot = c.insert(hv);
                            stores.push(crate::bands::CacheOp {
                                slot,
                                col: col as u16,
                                row: row as u16,
                            });
                        }
                    }
                }
                self.gather_buf.clear();
                gather(px, stride, 4, &self.rects, &mut self.gather_buf);
                let gathered = std::mem::take(&mut self.gather_buf);
                let extras = crate::bands::Extras {
                    cache: cache_on,
                    refs: &refs,
                    stores: &stores,
                    images: &images,
                };
                let (used, data) = self.compress_payload(codec, &gathered, &moves, &extras);
                self.gather_buf = gathered;
                let bytes = data.len();
                let frame = EncodedFrame {
                    seq,
                    width: w,
                    height: h,
                    stride: stride as u32,
                    format: PixelFormat::Bgra,
                    total: self.gather_buf.len() as u32,
                    codec: used,
                    data,
                    delta: Some(Delta {
                        base_seq: base.unwrap_or(0),
                        rects: self.rects.clone(),
                    }),
                    video: None,
                    barrier: 0,
                };
                match mailbox.put_if_taken(frame, taken, true) {
                    Ok(_) => {
                        std::mem::swap(&mut self.pending, &mut self.acc);
                        // A moved area differs from the base too: a
                        // follow-up that replaces this update re-sends it.
                        moves.iter().for_each(|m| self.pending.mark_rect(&m.dst()));
                        self.base = base;
                        self.pending_full = false;
                        if self.chunk.count() > 0 {
                            let per = bytes as f64 / self.pending.count().max(1) as f64;
                            self.refresh_tile_bytes = if self.refresh_tile_bytes > 0.0 {
                                self.refresh_tile_bytes * 0.5 + per * 0.5
                            } else {
                                per
                            };
                        }
                        // Wave 2: what the viewer will hold inexactly.
                        let mut refined = self.backlog.clone();
                        refined.union_with(&self.move_lossy);
                        let before = refined.count();
                        refined.subtract(&self.lossless_map);
                        refined.subtract(&self.ref_map);
                        self.stats.refined_tiles += (before - refined.count()) as u64;
                        refined.union_with(&self.img_map);
                        self.backlog = refined;
                        self.stats.lossy_tiles += self.img_map.count() as u64;
                        self.stats.lossy_bytes +=
                            images.iter().map(|i| i.data.len() as u64).sum::<u64>();
                        self.stats.cache_refs += refs.len() as u64;
                        self.stats.cache_stores += stores.len() as u64;
                        if cache_on {
                            self.cache_tentative = cache;
                        }
                        break SubmitOutcome::Delta {
                            tiles: self.pending.count(),
                            rects: self.rects.len(),
                            bytes,
                        };
                    }
                    // The sender took our previous update meanwhile: rebase.
                    Err(_) => continue,
                }
            }
            // ---- Full frame (Wave 2: image tiles lossy, blanked in the
            // lossless payload; every exact full tile stored in the cache).
            let mut images = Vec::new();
            self.img_map.reset(w, h);
            if lossy_on {
                self.cand_map.reset(w, h);
                let cols = self.cand_map.cols();
                for r in 0..self.cand_map.rows() {
                    for c in 0..cols {
                        if self.recent((r * cols + c) as usize, now) {
                            self.cand_map.set(c, r);
                        }
                    }
                }
                crate::lossy::classify_tiles(px, w, h, stride, &self.cand_map, &mut self.img_map);
                if self.img_map.count() > 0 {
                    match crate::lossy::encode_images(
                        px,
                        w,
                        h,
                        stride,
                        &map_tiles(&self.img_map),
                        self.lossy_params,
                    ) {
                        Some(v) => images = v,
                        None => self.img_map.reset(w, h),
                    }
                }
            }
            let mut cache = crate::lossy::TileCache::new(self.cache_committed.capacity());
            let mut stores = Vec::new();
            if cache_on {
                let mut exact = TileMap::new(w, h);
                exact.fill();
                exact.subtract(&self.img_map);
                for (col, row, hv) in tile_hashes(px, w, h, stride, &exact) {
                    if !cache.contains(hv) {
                        let slot = cache.insert(hv);
                        stores.push(crate::bands::CacheOp {
                            slot,
                            col: col as u16,
                            row: row as u16,
                        });
                    }
                }
            }
            let mut blank = std::mem::take(&mut self.blank_buf);
            let payload: &[u8] = if self.img_map.count() > 0 {
                blank.clear();
                blank.extend_from_slice(px);
                blank_tiles(&mut blank, w, h, stride, &self.img_map);
                &blank
            } else {
                px
            };
            let extras = crate::bands::Extras {
                cache: cache_on,
                refs: &[],
                stores: &stores,
                images: &images,
            };
            let (used, data) = self.compress_payload(codec, payload, &[], &extras);
            self.blank_buf = blank;
            let bytes = data.len();
            mailbox.put(EncodedFrame {
                seq,
                width: w,
                height: h,
                stride: stride as u32,
                format: PixelFormat::Bgra,
                total: px.len() as u32,
                codec: used,
                data,
                delta: None,
                video: None,
                barrier: 0,
            });
            // A full frame refreshes everything; its lossy tiles start the
            // backlog again.
            self.backlog.reset(w, h);
            self.backlog.union_with(&self.img_map);
            self.stats.lossy_tiles += self.img_map.count() as u64;
            self.stats.lossy_bytes += images.iter().map(|i| i.data.len() as u64).sum::<u64>();
            self.stats.cache_stores += stores.len() as u64;
            self.cache_tentative = cache_on.then_some(cache);
            // Until this full frame is taken, anything may differ from the
            // viewer's base: a follow-up must be full too.
            self.pending.reset(w, h);
            self.pending.fill();
            self.pending_full = true;
            self.base = taken;
            break SubmitOutcome::Full { bytes };
        };
        self.advance_last_px(px, w, h, stride, new_dims);
        self.last_put = Some(seq);
        self.seq = self.seq.wrapping_add(1);
        self.force_full = false;
        outcome
    }

    /// `last_px` := this capture (only the changed tiles move).
    fn advance_last_px(&mut self, px: &[u8], w: u32, h: u32, stride: usize, new_dims: bool) {
        if new_dims {
            self.last_px.clear();
            self.last_px.extend_from_slice(px);
        } else {
            rects_from_map(&self.fresh, w, h, &mut self.rects);
            copy_rects(&mut self.last_px, px, stride, 4, &self.rects);
        }
        self.last_dims = Some((w, h));
    }
}

/// Wave 2: the tiles set in `m`, raster order, as `(col, row)`.
fn map_tiles(m: &TileMap) -> Vec<(u16, u16)> {
    let mut v = Vec::with_capacity(m.count());
    for r in 0..m.rows() {
        for c in 0..m.cols() {
            if m.get(c, r) {
                v.push((c as u16, r as u16));
            }
        }
    }
    v
}

/// Wave 2: content hashes (size included) of the tiles set in `m`,
/// clipped edge tiles too, raster order; in parallel for many tiles.
fn tile_hashes(px: &[u8], w: u32, h: u32, stride: usize, m: &TileMap) -> Vec<(u32, u32, u128)> {
    use rayon::prelude::*;
    let list: Vec<(u32, u32)> = (0..m.rows())
        .flat_map(|r| (0..m.cols()).map(move |c| (c, r)))
        .filter(|&(c, r)| m.get(c, r))
        .collect();
    let hash = |&(c, r): &(u32, u32)| {
        let tr = crate::lossy::tile_rect(c, r, w, h);
        let (x, y, tw, th) = (
            usize::from(tr.x),
            usize::from(tr.y),
            usize::from(tr.w),
            usize::from(tr.h),
        );
        (c, r, crate::lossy::tile_hash_wh(px, stride, x, y, tw, th))
    };
    if list.len() >= 64 {
        list.par_iter().map(hash).collect()
    } else {
        list.iter().map(hash).collect()
    }
}

/// Wave 2: zero the tiles set in `m` (a full frame's lossy tiles: the
/// atlases paint over them, and zeros cost nothing in the bands).
fn blank_tiles(buf: &mut [u8], w: u32, h: u32, stride: usize, m: &TileMap) {
    for (c, r) in map_tiles(m) {
        let tr = crate::lossy::tile_rect(u32::from(c), u32::from(r), w, h);
        for y in usize::from(tr.y)..usize::from(tr.y) + usize::from(tr.h) {
            let o = y * stride + usize::from(tr.x) * 4;
            buf[o..o + usize::from(tr.w) * 4].fill(0);
        }
    }
}

/// Shrink each rect from the top and bottom past rows that are identical
/// in `prev` and `cur` (rects whose rows all match are dropped).
fn trim_rows(prev: &[u8], cur: &[u8], stride: usize, rects: &mut Vec<TileRect>) {
    let same = |r: &TileRect, y: usize| {
        let o = y * stride + usize::from(r.x) * 4;
        let n = usize::from(r.w) * 4;
        prev[o..o + n] == cur[o..o + n]
    };
    rects.retain_mut(|r| {
        let (mut y0, mut y1) = (usize::from(r.y), usize::from(r.y) + usize::from(r.h));
        while y0 < y1 && same(r, y0) {
            y0 += 1;
        }
        while y1 > y0 && same(r, y1 - 1) {
            y1 -= 1;
        }
        r.y = y0 as u16;
        r.h = (y1 - y0) as u16;
        r.h > 0
    });
}

/// Smoothed per-frame drain time: from the FRAME_BEGIN send to the
/// FRAME_END send. On a backlogged link this is the link's real per-frame
/// cost; the capture thread uses it as a floor for its capture interval.
#[derive(Debug, Clone, Default)]
pub struct DrainMeter {
    /// EWMA of the drain time, seconds.
    smooth: Option<f64>,
    /// EWMA of the drained bytes per frame.
    bytes: Option<f64>,
    frames: u64,
}

impl DrainMeter {
    /// EWMA weight of a new sample.
    const ALPHA: f64 = 0.3;

    /// Record one fully drained frame.
    pub fn record(&mut self, bytes: usize, took: Duration) {
        let s = took.as_secs_f64();
        let ewma = |prev: Option<f64>, x: f64| match prev {
            Some(p) => p * (1.0 - Self::ALPHA) + x * Self::ALPHA,
            None => x,
        };
        self.smooth = Some(ewma(self.smooth, s));
        self.bytes = Some(ewma(self.bytes, bytes as f64));
        self.frames += 1;
    }

    /// Smoothed drain time per frame (`None` before the first frame).
    pub fn per_frame(&self) -> Option<Duration> {
        self.smooth.map(Duration::from_secs_f64)
    }

    /// Smoothed drain throughput in bytes per second.
    pub fn bytes_per_sec(&self) -> Option<f64> {
        match (self.smooth, self.bytes) {
            (Some(t), Some(b)) if t > 0.0 => Some(b / t),
            _ => None,
        }
    }

    /// Frames measured so far.
    pub fn frames(&self) -> u64 {
        self.frames
    }
}

/// The frame currently being streamed.
struct InFlight {
    frame: EncodedFrame,
    /// Bytes of `frame.data` already sent as FRAME_DATA.
    offset: usize,
    /// FRAME_BEGIN already sent?
    begun: bool,
    /// When FRAME_BEGIN went out.
    started: Instant,
}

/// One record the sender wants on the wire next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outgoing {
    /// A control command (always drained first).
    Control(Command),
    /// The next record of the in-flight frame.
    Frame(Command),
    /// Wave 3: a whole H.264 access unit for the media channel (FEC,
    /// paced by the transport; it never occupies the in-flight slot).
    Media(EncodedFrame),
}

/// Control-first, one-frame-in-flight outbound scheduler (see the module
/// docs). Pure state: [`next`](Self::next) decides, [`flush`](Self::flush)
/// pushes the decisions through a [`TerminalSession`].
pub struct PrioritySender {
    control: VecDeque<Command>,
    inflight: Option<InFlight>,
    chunk: usize,
    meter: DrainMeter,
    frames_sent: u64,
    pacer: LinkPacer,
    /// Wave 3: video units go to the media channel (set per flush).
    media: bool,
    /// Wave 3: the transport's delay-based controller governs the rate
    /// (direct v2 path): the PING-window pacer is bypassed.
    governed: bool,
}

impl PrioritySender {
    /// A sender cutting FRAME_DATA into `chunk`-byte records (0 = default).
    pub fn new(chunk: usize) -> Self {
        Self {
            control: VecDeque::new(),
            inflight: None,
            chunk: if chunk == 0 { DEFAULT_CHUNK } else { chunk },
            meter: DrainMeter::default(),
            frames_sent: 0,
            pacer: LinkPacer::new(),
            media: false,
            governed: false,
        }
    }

    /// The link pacer (Phase 4).
    pub fn pacer(&self) -> &LinkPacer {
        &self.pacer
    }

    /// Mutable pacer access (e.g. to disable pacing).
    pub fn pacer_mut(&mut self) -> &mut LinkPacer {
        &mut self.pacer
    }

    /// Route a received PONG: `true` when it answered one of the pacer's
    /// probe PINGs (consumed), `false` when it belongs to someone else (the
    /// keepalive).
    pub fn on_pong(&mut self, nonce: u64, now: Instant) -> bool {
        self.pacer.on_pong(nonce, now)
    }

    /// Bytes that entered the network path (handed to the OS).
    fn path_bytes(term: &TerminalSession) -> u64 {
        term.bytes_sent().saturating_sub(term.pending_send() as u64)
    }

    /// Frame data is being held back by the pacer's window right now.
    pub fn paced(&self, term: &TerminalSession) -> bool {
        !self.governed
            && (self.inflight.is_some())
            && self
                .pacer
                .reopen_in(Instant::now(), Self::path_bytes(term))
                .is_some()
    }

    /// The longest a loop should block before calling [`flush`](Self::flush)
    /// again on the pacer's behalf (window reopening, or the next probe
    /// while frames flow); `None` = no pacing deadline.
    pub fn pacing_deadline(
        &self,
        term: &TerminalSession,
        mailbox: &FrameMailbox,
    ) -> Option<Duration> {
        if !self.pacer.enabled() || self.governed {
            return None;
        }
        let now = Instant::now();
        let active = self.inflight.is_some() || mailbox.is_pending();
        let reopen = self
            .inflight
            .as_ref()
            .and_then(|_| self.pacer.reopen_in(now, Self::path_bytes(term)));
        let probe = active
            .then(|| self.pacer.next_probe_at())
            .flatten()
            .map(|t| t.saturating_duration_since(now));
        match (reopen, probe) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Queue a control command; it goes out before any further frame data.
    pub fn push_control(&mut self, cmd: Command) {
        self.control.push_back(cmd);
    }

    /// Control commands waiting.
    pub fn pending_control(&self) -> usize {
        self.control.len()
    }

    /// A frame is partially sent.
    pub fn frame_in_flight(&self) -> bool {
        self.inflight.is_some()
    }

    /// Anything to send right now (control, an in-flight frame, or a new
    /// frame waiting in `mailbox`)?
    pub fn has_work(&self, mailbox: &FrameMailbox) -> bool {
        !self.control.is_empty() || self.inflight.is_some() || mailbox.is_pending()
    }

    /// Frames fully sent.
    pub fn frames_sent(&self) -> u64 {
        self.frames_sent
    }

    /// The drain-time meter.
    pub fn meter(&self) -> &DrainMeter {
        &self.meter
    }

    /// Estimated time until the in-flight frame is fully handed off, from
    /// its remaining bytes and the measured drain throughput. Zero when no
    /// frame is in flight or nothing has been measured yet. A capture
    /// thread that starts its next capture about this long before the link
    /// frees up delivers the freshest possible frame with no idle link.
    pub fn inflight_eta(&self) -> Duration {
        let (Some(f), Some(bps)) = (&self.inflight, self.meter.bytes_per_sec()) else {
            return Duration::ZERO;
        };
        let left = f.frame.data.len().saturating_sub(f.offset);
        if bps <= 0.0 {
            return Duration::ZERO;
        }
        Duration::from_secs_f64(left as f64 / bps)
    }

    /// The next record to send: queued control first; otherwise the next
    /// record of the in-flight frame; otherwise a FRAME_BEGIN for the frame
    /// waiting in `mailbox` (taken only when nothing else is in flight).
    pub fn next(&mut self, mailbox: &FrameMailbox, now: Instant) -> Option<Outgoing> {
        if let Some(cmd) = self.control.pop_front() {
            return Some(Outgoing::Control(cmd));
        }
        if self.inflight.is_none() {
            let frame = mailbox.take()?;
            if self.media && frame.video.is_some_and(|h| !h.is_end()) && !frame.data.is_empty() {
                self.frames_sent += 1;
                return Some(Outgoing::Media(frame));
            }
            self.inflight = Some(InFlight {
                frame,
                offset: 0,
                begun: false,
                started: now,
            });
        }
        let f = self.inflight.as_mut()?;
        if !f.begun {
            f.begun = true;
            f.started = now;
            return Some(Outgoing::Frame(f.frame.begin()));
        }
        if f.offset < f.frame.data.len() {
            let end = (f.offset + self.chunk).min(f.frame.data.len());
            let chunk = f.frame.data[f.offset..end].to_vec();
            f.offset = end;
            return Some(Outgoing::Frame(Command::FrameData(chunk)));
        }
        // All data out: close the frame and record how long it drained.
        let done = self.inflight.take()?;
        self.meter.record(
            done.frame.data.len(),
            now.saturating_duration_since(done.started),
        );
        self.frames_sent += 1;
        Some(Outgoing::Frame(Command::FrameEnd(done.frame.seq)))
    }

    /// Whether frame data must wait for the carrier: more than one chunk is
    /// still queued below us (the link is the bottleneck). Control is never
    /// held back by this.
    pub fn link_backlogged(&self, term: &TerminalSession) -> bool {
        term.pending_send() > self.chunk
    }

    /// Send every queued control command, then at most `max_frame_records`
    /// frame records — fewer when the carrier already queues more than one
    /// chunk (frame data never piles up ahead of later control). Returns the
    /// number of records sent. A control command too large for one record is
    /// skipped (nothing was sent, the session is intact); any other error is
    /// returned (the session is gone).
    pub fn flush(
        &mut self,
        term: &mut TerminalSession,
        mailbox: &FrameMailbox,
        max_frame_records: usize,
    ) -> Result<usize, TransportError> {
        let mut sent = 0usize;
        let mut frame_records = 0usize;
        // Phase 4: a probe PING ahead of the frame data (a cumulative ack
        // + RTT sample once the peer echoes it).
        let now = Instant::now();
        // Wave 3: on the direct v2 path the transport's controller paces
        // and rate-limits; video rides the media channel.
        self.governed = term.transport_rate().is_some();
        self.media = term.media_ready();
        if !self.governed
            && self
                .pacer
                .probe_due(now, self.inflight.is_some() || mailbox.is_pending())
        {
            let nonce = self.pacer.next_nonce();
            term.send(&Command::Ping(nonce))?;
            // The PING's stream position: everything handed to the carrier,
            // its own queue included (the PING may still sit in it).
            self.pacer.probe_sent(nonce, now, term.bytes_sent());
            sent += 1;
        }
        loop {
            if self.control.is_empty() {
                if frame_records >= max_frame_records {
                    break;
                }
                let has_frame = self.inflight.is_some() || mailbox.is_pending();
                if has_frame && self.link_backlogged(term) {
                    self.pacer.note_limited();
                    break;
                }
                if has_frame
                    && !self.governed
                    && !self.pacer.allow(Instant::now(), Self::path_bytes(term))
                {
                    break;
                }
            }
            let Some(out) = self.next(mailbox, Instant::now()) else {
                break;
            };
            match out {
                Outgoing::Control(cmd) => match term.send(&cmd) {
                    Ok(()) => sent += 1,
                    Err(TransportError::FrameTooLarge(n)) => {
                        tracing::warn!("control command too large ({n} B); skipped");
                    }
                    Err(e) => return Err(e),
                },
                Outgoing::Frame(cmd) => {
                    term.send(&cmd)?;
                    sent += 1;
                    frame_records += 1;
                }
                Outgoing::Media(f) => {
                    let header = f.video.unwrap_or_default();
                    if term.send_video_media(&header, &f.data)? {
                        sent += 1;
                        frame_records += 1;
                    } else {
                        // The channel closed meanwhile (path demoted):
                        // stream it reliably instead.
                        self.frames_sent = self.frames_sent.saturating_sub(1);
                        self.inflight = Some(InFlight {
                            frame: f,
                            offset: 0,
                            begun: false,
                            started: Instant::now(),
                        });
                    }
                }
            }
        }
        Ok(sent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(seq: u64, bytes: usize) -> EncodedFrame {
        EncodedFrame {
            seq,
            width: 1,
            height: 1,
            stride: 4,
            format: PixelFormat::Bgra,
            total: 4,
            codec: FrameCodec::Deflate,
            data: vec![seq as u8; bytes],
            delta: None,
            video: None,
            barrier: 0,
        }
    }

    fn is_frame_end(o: &Outgoing, seq: u64) -> bool {
        matches!(o, Outgoing::Frame(Command::FrameEnd(s)) if *s == seq)
    }

    #[test]
    fn newer_frame_replaces_an_unsent_one() {
        let mb = FrameMailbox::default();
        assert!(!mb.put(frame(1, 10)));
        assert!(mb.put(frame(2, 10)), "frame 1 was never taken");
        assert_eq!(mb.replaced_count(), 1);
        assert_eq!(mb.take().map(|f| f.seq), Some(2));
        assert!(mb.take().is_none());
    }

    #[test]
    fn mailbox_put_fires_the_notify_hook() {
        let hits = std::sync::Arc::new(AtomicU64::new(0));
        let h = std::sync::Arc::clone(&hits);
        let mb = FrameMailbox::new(Some(Box::new(move || {
            h.fetch_add(1, Ordering::Relaxed);
        })));
        mb.put(frame(1, 1));
        mb.put(frame(2, 1));
        assert_eq!(hits.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn frame_streams_begin_chunks_end() {
        let mb = FrameMailbox::default();
        let mut tx = PrioritySender::new(4);
        mb.put(frame(7, 10));
        let now = Instant::now();
        let recs: Vec<_> = std::iter::from_fn(|| tx.next(&mb, now)).collect();
        assert_eq!(recs.len(), 1 + 3 + 1, "BEGIN + ceil(10/4) chunks + END");
        assert!(matches!(
            recs[0],
            Outgoing::Frame(Command::FrameBegin {
                seq: 7,
                compressed_len: 10,
                ..
            })
        ));
        let data: Vec<u8> = recs[1..4]
            .iter()
            .flat_map(|r| match r {
                Outgoing::Frame(Command::FrameData(c)) => c.clone(),
                other => panic!("expected FRAME_DATA, got {other:?}"),
            })
            .collect();
        assert_eq!(data, vec![7u8; 10]);
        assert!(is_frame_end(&recs[4], 7));
        assert_eq!(tx.frames_sent(), 1);
        assert_eq!(tx.meter().frames(), 1);
    }

    #[test]
    fn at_most_one_frame_in_flight_and_newest_wins() {
        let mb = FrameMailbox::default();
        let mut tx = PrioritySender::new(4);
        let now = Instant::now();
        mb.put(frame(1, 8));
        // Start frame 1 (BEGIN + first chunk).
        assert!(matches!(
            tx.next(&mb, now),
            Some(Outgoing::Frame(Command::FrameBegin { seq: 1, .. }))
        ));
        assert!(matches!(
            tx.next(&mb, now),
            Some(Outgoing::Frame(Command::FrameData(_)))
        ));
        // Two newer frames arrive mid-flight: frame 2 is replaced by 3.
        mb.put(frame(2, 8));
        mb.put(frame(3, 8));
        // Frame 1 still finishes first (never torn).
        assert!(matches!(
            tx.next(&mb, now),
            Some(Outgoing::Frame(Command::FrameData(_)))
        ));
        assert!(is_frame_end(&tx.next(&mb, now).unwrap(), 1));
        // Then the newest frame, 3 — frame 2 was never sent.
        assert!(matches!(
            tx.next(&mb, now),
            Some(Outgoing::Frame(Command::FrameBegin { seq: 3, .. }))
        ));
        assert_eq!(mb.replaced_count(), 1);
    }

    #[test]
    fn inflight_eta_tracks_remaining_bytes_at_the_measured_rate() {
        let mb = FrameMailbox::default();
        let mut tx = PrioritySender::new(100);
        let t = Instant::now();
        // No measurement yet: no estimate.
        mb.put(frame(1, 1000));
        tx.next(&mb, t);
        assert_eq!(tx.inflight_eta(), Duration::ZERO);
        // Drain frame 1 over 100 ms -> 10 000 B/s measured.
        while tx.frame_in_flight() {
            tx.next(&mb, t + Duration::from_millis(100));
        }
        assert_eq!(tx.meter().per_frame(), Some(Duration::from_millis(100)));
        // Frame 2: BEGIN + 3 chunks out, 700 B left -> ~70 ms.
        mb.put(frame(2, 1000));
        for _ in 0..4 {
            tx.next(&mb, t + Duration::from_millis(200));
        }
        let eta = tx.inflight_eta();
        assert!(
            eta > Duration::from_millis(65) && eta < Duration::from_millis(75),
            "{eta:?}"
        );
        // Idle link: zero.
        while tx.frame_in_flight() {
            tx.next(&mb, t + Duration::from_millis(300));
        }
        assert_eq!(tx.inflight_eta(), Duration::ZERO);
    }

    #[test]
    fn control_never_waits_behind_more_than_one_chunk() {
        let mb = FrameMailbox::default();
        let mut tx = PrioritySender::new(4);
        let now = Instant::now();
        mb.put(frame(1, 4 * 100));
        let mut frame_records_since_control = 0usize;
        let mut worst = 0usize;
        let mut pong_pending = false;
        for i in 0..400 {
            // A PONG becomes due at arbitrary points while the frame streams.
            if i % 7 == 3 {
                tx.push_control(Command::Pong(i));
                pong_pending = true;
                frame_records_since_control = 0;
            }
            match tx.next(&mb, now) {
                Some(Outgoing::Control(Command::Pong(_))) => {
                    worst = worst.max(frame_records_since_control);
                    pong_pending = false;
                }
                Some(Outgoing::Frame(_)) => {
                    if pong_pending {
                        frame_records_since_control += 1;
                    }
                }
                Some(other) => panic!("unexpected {other:?}"),
                None => break,
            }
        }
        assert_eq!(worst, 0, "queued control goes before the next frame record");
        assert!(!tx.frame_in_flight());
    }

    #[test]
    fn flush_sends_control_then_one_frame_record_over_a_session() {
        let (mut host, mut view) = session_pair();
        let mb = FrameMailbox::default();
        let mut tx = PrioritySender::new(1024);
        let mut f = crate::screen::synthetic_frame(5, 64, 32);
        f.seq = 5;
        let px = f.pixels.clone();
        mb.put(EncodedFrame::compress(&f));
        tx.push_control(Command::Cursor { x: 1, y: 2 });
        // One call: the cursor, the pacer's probe PING (Phase 4: frames
        // are about to flow) + exactly one frame record (FRAME_BEGIN).
        assert_eq!(tx.flush(&mut host, &mb, 1).unwrap(), 3);
        assert!(tx.frame_in_flight());
        while tx.has_work(&mb) {
            tx.flush(&mut host, &mb, 1).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut got = None;
        while got.is_none() && Instant::now() < deadline {
            view.pump().unwrap();
            got = view.take_frame();
        }
        let got = got.expect("frame reassembled");
        assert_eq!(got.seq, 5);
        assert_eq!(got.pixels, px);
        // The viewer auto-echoed the probe; its PONG belongs to the pacer.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut pong = None;
        while pong.is_none() && Instant::now() < deadline {
            host.pump().unwrap();
            pong = host.take_pong();
        }
        let pong = pong.expect("probe echoed");
        assert!(crate::pacing::is_probe(pong));
        assert!(
            tx.on_pong(pong, Instant::now()),
            "probe PONG consumed by the pacer"
        );
        assert!(!tx.on_pong(3, Instant::now()), "a keepalive PONG is not");
        assert_eq!(tx.pacer().stats().probes_acked, 1);
    }

    #[test]
    fn disabled_pacer_sends_no_probes() {
        let (mut host, _view) = session_pair();
        let mb = FrameMailbox::default();
        let mut tx = PrioritySender::new(1024);
        tx.pacer_mut().set_enabled(false);
        let f = crate::screen::synthetic_frame(1, 16, 16);
        mb.put(EncodedFrame::compress(&f));
        assert_eq!(tx.flush(&mut host, &mb, 1).unwrap(), 1, "FRAME_BEGIN only");
        assert_eq!(tx.pacing_deadline(&host, &mb), None);
    }

    // ---------------- Phase 3: UpdateEncoder ----------------

    use crate::terminal::{CAP_KEYFRAME, LOCAL_CAPS};
    use crate::tiles::ScreenDecoder;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn below(&mut self, n: u32) -> u32 {
            (self.next() % u64::from(n.max(1))) as u32
        }
    }

    fn noise(w: u32, h: u32, seed: u64) -> Vec<u8> {
        let mut r = Rng(seed | 1);
        (0..w * h * 4).map(|_| r.next() as u8).collect()
    }

    /// Mutate a frame like a desktop does: small edits (typing, a blinking
    /// caret), a block fill (a window/video region), or a vertical scroll.
    fn mutate(px: &mut [u8], w: u32, h: u32, rng: &mut Rng) {
        let stride = (w * 4) as usize;
        match rng.below(10) {
            0..=5 => {
                for _ in 0..1 + rng.below(6) {
                    let i = rng.below(w * h * 4) as usize;
                    px[i] = px[i].wrapping_add(1 + rng.below(200) as u8);
                }
            }
            6..=8 => {
                let (bx, by) = (rng.below(w), rng.below(h));
                let (bw, bh) = (1 + rng.below(w - bx), 1 + rng.below(h - by));
                let v = rng.next() as u8;
                for y in by..by + bh {
                    let o = y as usize * stride + bx as usize * 4;
                    px[o..o + bw as usize * 4].iter_mut().for_each(|b| *b = v);
                }
            }
            _ => {
                let dy = 1 + rng.below(h / 2) as usize;
                px.copy_within(dy * stride.., 0);
            }
        }
    }

    fn apply(dec: &mut ScreenDecoder, f: &EncodedFrame) {
        dec.apply(&f.meta(), f.seq, &f.data).unwrap_or_else(|e| {
            panic!(
                "update {} ({:?}) failed: {e}",
                f.seq,
                f.delta.as_ref().map(|d| d.base_seq)
            )
        });
    }

    /// The core Phase 3 invariant: whatever subset of updates the sender
    /// gets to take (newer ones replace unsent older ones in the mailbox),
    /// the viewer applying exactly the taken ones in order always ends up
    /// with the pixels of the capture behind the last one it applied.
    #[test]
    fn dropped_updates_merge_into_the_next_delta() {
        let (w, h) = (300, 200);
        let mb = FrameMailbox::default();
        let mut enc = UpdateEncoder::new();
        enc.set_peer_caps(LOCAL_CAPS);
        let mut dec = ScreenDecoder::new();
        let mut rng = Rng(0xC0FFEE);
        let mut screen = noise(w, h, 1);
        // seq -> the capture each queued update encodes.
        let mut captured = std::collections::HashMap::new();
        let (mut deltas, mut fulls, mut replaced) = (0, 0, 0);
        for step in 0..600 {
            mutate(&mut screen, w, h, &mut rng);
            let seq = enc.next_seq();
            let was_pending = mb.is_pending();
            match enc.submit(&screen, w, h, None, &mb) {
                SubmitOutcome::Unchanged => {}
                SubmitOutcome::Full { .. } => {
                    fulls += 1;
                    replaced += usize::from(was_pending);
                    captured.insert(seq, screen.clone());
                }
                SubmitOutcome::Delta { .. } => {
                    deltas += 1;
                    replaced += usize::from(was_pending);
                    captured.insert(seq, screen.clone());
                }
            }
            // The link drains at a random pace: often several captures per
            // send, so unsent updates get replaced.
            if rng.below(3) == 0 || step % 50 == 49 {
                if let Some(f) = mb.take() {
                    apply(&mut dec, &f);
                    assert_eq!(
                        dec.frame().unwrap().pixels,
                        captured[&f.seq],
                        "after update {}",
                        f.seq
                    );
                }
            }
        }
        assert!(
            deltas > 100 && replaced > 50,
            "deltas {deltas} fulls {fulls} replaced {replaced}"
        );
    }

    /// The sender taking the previous update between the encoder reading
    /// the base and queueing its delta must not let a delta with a stale
    /// base through.
    #[test]
    fn a_take_between_diff_and_put_forces_a_rebase() {
        let mb = FrameMailbox::default();
        mb.put(frame(1, 4));
        let base = mb.last_taken();
        assert_eq!(mb.take().map(|f| f.seq), Some(1));
        assert!(
            mb.put_if_taken(frame(2, 4), base, true).is_err(),
            "stale base refused"
        );
        assert_eq!(mb.put_if_taken(frame(2, 4), Some(1), true), Ok(false));
        assert_eq!(mb.last_taken(), Some(1));
    }

    #[test]
    fn many_dirty_tiles_send_a_full_frame_instead() {
        let (w, h) = (640, 640); // 100 tiles
        let mb = FrameMailbox::default();
        let mut enc = UpdateEncoder::new();
        enc.set_peer_caps(LOCAL_CAPS);
        let mut px = noise(w, h, 3);
        assert!(
            matches!(enc.submit(&px, w, h, None, &mb), SubmitOutcome::Full { .. }),
            "first frame"
        );
        mb.take();
        let touch = |px: &mut Vec<u8>, tiles: u32| {
            for t in 0..tiles {
                let (c, r) = (t % 10, t / 10);
                let i = (((r * 64) * w + c * 64) * 4) as usize;
                px[i] = px[i].wrapping_add(1);
            }
        };
        touch(&mut px, 60); // exactly 60%: still a delta
        assert!(matches!(
            enc.submit(&px, w, h, None, &mb),
            SubmitOutcome::Delta { tiles: 60, .. }
        ));
        mb.take();
        touch(&mut px, 61); // > 60%: full
        assert!(matches!(
            enc.submit(&px, w, h, None, &mb),
            SubmitOutcome::Full { .. }
        ));
        mb.take();
        // Unchanged after that, until a keyframe is requested.
        assert_eq!(enc.submit(&px, w, h, None, &mb), SubmitOutcome::Unchanged);
        enc.request_keyframe();
        assert!(matches!(
            enc.submit(&px, w, h, None, &mb),
            SubmitOutcome::Full { .. }
        ));
        assert!(mb.take().unwrap().delta.is_none());
        assert_eq!(enc.submit(&px, w, h, None, &mb), SubmitOutcome::Unchanged);
        // Monitor switch / resolution change: full.
        enc.reset();
        assert!(matches!(
            enc.submit(&px, w, h, None, &mb),
            SubmitOutcome::Full { .. }
        ));
        let small = noise(320, 200, 4);
        assert!(matches!(
            enc.submit(&small, 320, 200, None, &mb),
            SubmitOutcome::Full { .. }
        ));
    }

    #[test]
    fn an_old_viewer_only_ever_gets_deflate_full_frames() {
        let (w, h) = (256, 256);
        let mb = FrameMailbox::default();
        let mut enc = UpdateEncoder::new(); // no CAPS from the viewer
        let mut px = noise(w, h, 5);
        let mut dec = ScreenDecoder::new();
        for i in 0..5 {
            px[i * 4] ^= 1;
            assert!(matches!(
                enc.submit(&px, w, h, None, &mb),
                SubmitOutcome::Full { .. }
            ));
            let f = mb.take().unwrap();
            assert!(f.delta.is_none());
            assert_eq!(f.codec, FrameCodec::Deflate);
            assert!(matches!(f.begin(), Command::FrameBegin { .. }));
            apply(&mut dec, &f);
            assert_eq!(dec.frame().unwrap().pixels, px);
        }
        assert_eq!(enc.submit(&px, w, h, None, &mb), SubmitOutcome::Unchanged);
    }

    #[test]
    fn zstd_only_for_a_viewer_that_advertised_it() {
        let (w, h) = (128, 128);
        let px = noise(w, h, 6);
        let mb = FrameMailbox::default();
        let mut enc = UpdateEncoder::new();
        enc.set_peer_caps(CAP_TILES);
        enc.submit(&px, w, h, None, &mb);
        assert_eq!(mb.take().unwrap().codec, FrameCodec::Deflate);
        enc.set_peer_caps(CAP_TILES | crate::terminal::CAP_ZSTD);
        enc.request_keyframe();
        enc.submit(&px, w, h, None, &mb);
        let want = if crate::codec::zstd_encode_available() {
            FrameCodec::Zstd
        } else {
            FrameCodec::Deflate
        };
        assert_eq!(mb.take().unwrap().codec, want);
        // Wave 1: a CAP_BANDS viewer gets band containers.
        enc.set_peer_caps(LOCAL_CAPS);
        enc.request_keyframe();
        enc.submit(&px, w, h, None, &mb);
        assert_eq!(mb.take().unwrap().codec, FrameCodec::Packed);
    }

    /// OS dirty rects (DXGI) as hints: changes inside them are found,
    /// claimed-dirty-but-identical areas cost nothing.
    #[test]
    fn hints_drive_the_diff() {
        let (w, h) = (256, 256);
        let mb = FrameMailbox::default();
        let mut enc = UpdateEncoder::new();
        enc.set_peer_caps(LOCAL_CAPS);
        let mut px = noise(w, h, 7);
        enc.submit(&px, w, h, Some(&[]), &mb); // first frame: full regardless
        mb.take();
        assert_eq!(
            enc.submit(&px, w, h, Some(&[]), &mb),
            SubmitOutcome::Unchanged
        );
        let whole = [TileRect {
            x: 0,
            y: 0,
            w: 256,
            h: 256,
        }];
        assert_eq!(
            enc.submit(&px, w, h, Some(&whole), &mb),
            SubmitOutcome::Unchanged
        );
        px[(70 * 256 + 70) * 4] ^= 0xFF;
        let hint = [TileRect {
            x: 70,
            y: 70,
            w: 1,
            h: 1,
        }];
        assert!(matches!(
            enc.submit(&px, w, h, Some(&hint), &mb),
            SubmitOutcome::Delta { tiles: 1, .. }
        ));
    }

    /// A change captured while the link is busy (its update still unsent)
    /// and followed only by idle captures must still reach the viewer:
    /// DXGI hints name only what changed since the *previous capture*, so a
    /// later delta must re-carry the replaced update's tiles, and idle
    /// captures (empty hints) must not drop them.
    #[test]
    fn change_queued_while_busy_survives_replacement_and_idle() {
        let (w, h) = (320, 256);
        let mb = FrameMailbox::default();
        let mut enc = UpdateEncoder::new();
        enc.set_peer_caps(LOCAL_CAPS);
        let mut dec = ScreenDecoder::new();
        let mut px = noise(w, h, 41);
        enc.submit(&px, w, h, None, &mb);
        apply(&mut dec, &mb.take().unwrap());
        let fill = |px: &mut [u8], r: TileRect, v: u8| {
            for y in r.y..r.y + r.h {
                let o = (usize::from(y) * w as usize + usize::from(r.x)) * 4;
                px[o..o + usize::from(r.w) * 4]
                    .iter_mut()
                    .for_each(|b| *b = v);
            }
        };
        // A: a big block appears (the Start menu); the link is busy, so
        // its update waits in the mailbox.
        let a = TileRect {
            x: 0,
            y: 64,
            w: 192,
            h: 192,
        };
        fill(&mut px, a, 0x11);
        assert!(matches!(
            enc.submit(&px, w, h, Some(&[a]), &mb),
            SubmitOutcome::Delta { .. }
        ));
        // B: a small change elsewhere replaces the unsent update; its hint
        // names only B.
        let b = TileRect {
            x: 300,
            y: 0,
            w: 4,
            h: 4,
        };
        fill(&mut px, b, 0x22);
        assert!(matches!(
            enc.submit(&px, w, h, Some(&[b]), &mb),
            SubmitOutcome::Delta { .. }
        ));
        assert_eq!(mb.replaced_count(), 1);
        // Idle: nothing new, nothing queued, nothing forgotten.
        for _ in 0..5 {
            assert_eq!(
                enc.submit(&px, w, h, Some(&[]), &mb),
                SubmitOutcome::Unchanged
            );
        }
        apply(
            &mut dec,
            &mb.take().expect("the merged update is still waiting"),
        );
        assert_eq!(
            dec.frame().unwrap().pixels,
            px,
            "A and B both on the viewer"
        );
        assert!(!mb.is_pending());
    }

    /// Caps negotiation over a real session: an old viewer (no CAPS) leaves
    /// the host without peer caps; a new viewer's CAPS gets the host's reply.
    #[test]
    fn caps_negotiation_old_and_new_viewer() {
        let (mut host, mut view) = session_pair();
        for _ in 0..20 {
            host.pump().unwrap();
            view.pump().unwrap();
        }
        assert_eq!(host.peer_caps(), None, "old viewer: never advertised");
        assert_eq!(view.peer_caps(), None);
        view.announce_caps().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while (host.peer_caps().is_none() || view.peer_caps().is_none())
            && Instant::now() < deadline
        {
            host.pump().unwrap();
            view.pump().unwrap();
        }
        let ours = LOCAL_CAPS | crate::terminal::VIEWER_W2_CAPS | crate::audio::audio_caps();
        assert_eq!(host.peer_caps(), Some(ours));
        assert_eq!(view.peer_caps(), Some(ours));
    }

    /// End to end: tile deltas stream through PrioritySender into the
    /// viewer's framebuffer; a lost update makes the viewer ask for a
    /// keyframe, and the host's answer resyncs it.
    #[test]
    fn lost_update_triggers_a_keyframe_request_and_resync() {
        let (mut host, mut view) = session_pair();
        let hook_kf = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let caps = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        {
            let kf = std::sync::Arc::clone(&hook_kf);
            let caps = std::sync::Arc::clone(&caps);
            host.on_cmd(Some(Box::new(move |cmd| match cmd {
                Command::KeyframeRequest { .. } => kf.store(true, Ordering::Relaxed),
                Command::Caps { flags } => caps.store(*flags, Ordering::Relaxed),
                _ => {}
            })));
        }
        view.announce_caps().unwrap();
        let (w, h) = (320, 192);
        let mb = FrameMailbox::default();
        let mut tx = PrioritySender::new(1024);
        let mut enc = UpdateEncoder::new();
        let pump_all =
            |host: &mut TerminalSession, view: &mut TerminalSession, tx: &mut PrioritySender| {
                for _ in 0..50 {
                    tx.flush(host, &mb, 64).unwrap();
                    host.pump().unwrap();
                    view.pump().unwrap();
                }
            };
        pump_all(&mut host, &mut view, &mut tx);
        assert!(view.peer_caps().is_some_and(|c| c & CAP_KEYFRAME != 0));
        enc.set_peer_caps(caps.load(Ordering::Relaxed));
        // Exact pixels per update are the point here (Wave 2 lossy tiles
        // are covered by their own tests).
        enc.set_lossy(false);
        let mut px = noise(w, h, 8);
        let mut rng = Rng(99);
        for _ in 0..10 {
            mutate(&mut px, w, h, &mut rng);
            if enc.submit(&px, w, h, None, &mb) == SubmitOutcome::Unchanged {
                continue;
            }
            pump_all(&mut host, &mut view, &mut tx);
            let got = view.take_latest_frame().expect("frame");
            assert_eq!(got.pixels, px);
        }
        // Lose one update on the way (taken from the mailbox, never sent).
        px[0] ^= 0xFF;
        enc.submit(&px, w, h, None, &mb);
        let lost = mb.take().unwrap();
        assert!(lost.delta.is_some());
        px[(100 * w as usize + 300) * 4] ^= 0xFF;
        enc.submit(&px, w, h, None, &mb);
        pump_all(&mut host, &mut view, &mut tx);
        assert!(
            view.take_latest_frame().is_none(),
            "the orphan delta is not shown"
        );
        pump_all(&mut host, &mut view, &mut tx);
        assert!(
            hook_kf.load(Ordering::Relaxed),
            "viewer asked for a keyframe"
        );
        assert_eq!(view.stats().keyframe_requests, 1);
        enc.request_keyframe();
        assert!(matches!(
            enc.submit(&px, w, h, None, &mb),
            SubmitOutcome::Full { .. }
        ));
        pump_all(&mut host, &mut view, &mut tx);
        assert_eq!(view.take_latest_frame().expect("keyframe").pixels, px);
    }

    /// A viewer that falls behind: more updates than MAX_QUEUED_FRAMES pile
    /// up between takes. Deltas can never be skipped, so overflow applies
    /// them; older full frames are dropped undecoded; the newest state wins.
    #[test]
    fn slow_viewer_queue_keeps_the_delta_chain() {
        let (mut host, mut view) = session_pair();
        let (w, h) = (256, 192);
        let mb = FrameMailbox::default();
        let mut tx = PrioritySender::new(4096);
        let mut enc = UpdateEncoder::new();
        enc.set_peer_caps(LOCAL_CAPS);
        let mut px = noise(w, h, 12);
        let mut push = |px: &[u8],
                        enc: &mut UpdateEncoder,
                        host: &mut TerminalSession,
                        view: &mut TerminalSession| {
            enc.submit(px, w, h, None, &mb);
            while tx.has_work(&mb) {
                tx.flush(host, &mb, 64).unwrap();
            }
            for _ in 0..5 {
                view.pump().unwrap();
            }
        };
        push(&px, &mut enc, &mut host, &mut view); // keyframe
        for i in 0..10 {
            px[i * 4 * 70] ^= 0x55;
            push(&px, &mut enc, &mut host, &mut view);
        }
        let got = view.take_latest_frame().expect("frame");
        assert_eq!(got.pixels, px);
        assert!(view.take_latest_frame().is_none(), "nothing newer");
        assert_eq!(view.stats().screen_desyncs, 0);

        // Oldest-first consumer: every update yields its own snapshot.
        let mut want = Vec::new();
        for i in 0..3 {
            px[(i + 1) * 4 * 300] ^= 0x33;
            want.push(px.clone());
            push(&px, &mut enc, &mut host, &mut view);
        }
        for w in want {
            assert_eq!(view.take_frame().expect("queued").pixels, w);
        }
        assert!(view.take_frame().is_none());
    }

    // ---------------- Phase 5: video through the mailbox ----------------

    use crate::terminal::CAP_H264;
    use crate::video::{
        EncodedAu, EncoderFactory, MotionConfig, VideoDecode, VideoEncode, VideoSession,
        CODEC_H264, VIDEO_END, VIDEO_KEYFRAME,
    };

    fn vunit(seq: u64, flags: u8) -> EncodedFrame {
        EncodedFrame::video(
            VideoHeader {
                seq,
                flags,
                codec: CODEC_H264,
                rect: TileRect {
                    x: 0,
                    y: 0,
                    w: 64,
                    h: 64,
                },
                screen_w: 128,
                screen_h: 128,
                pts_us: 0,
            },
            vec![0, 0, 0, 1, 0x41, seq as u8],
        )
    }

    /// Video units are FIFO (never replaced), a screen update queued after
    /// a video unit waits for it, and when both are eligible they alternate.
    #[test]
    fn mailbox_keeps_video_fifo_and_orders_updates_after_it() {
        let mb = FrameMailbox::default();
        mb.push_video(vunit(0, VIDEO_KEYFRAME));
        mb.push_video(vunit(1, 0));
        mb.put(frame(10, 4)); // barrier = 2
        mb.push_video(vunit(2, 0));
        assert_eq!(mb.video_queued(), 3);
        assert!(mb.is_pending());
        let order: Vec<(bool, u64)> =
            std::iter::from_fn(|| mb.take().map(|f| (f.video.is_some(), f.seq))).collect();
        assert_eq!(order, [(true, 0), (true, 1), (false, 10), (true, 2)]);
        assert_eq!(
            mb.last_taken(),
            Some(10),
            "only screen updates are delta bases"
        );
        // A newer update still replaces an unsent one; video is never replaced.
        mb.push_video(vunit(3, VIDEO_END));
        mb.put(frame(11, 4));
        assert!(mb.put(frame(12, 4)));
        let order: Vec<(bool, u64)> =
            std::iter::from_fn(|| mb.take().map(|f| (f.video.is_some(), f.seq))).collect();
        assert_eq!(
            order,
            [(true, 3), (false, 12)],
            "END goes before the refresh queued after it"
        );
        // Streaming a video unit: VIDEO_BEGIN + chunks + FRAME_END(seq).
        mb.push_video(vunit(4, 0));
        let mut tx = PrioritySender::new(4);
        let now = Instant::now();
        let recs: Vec<_> = std::iter::from_fn(|| tx.next(&mb, now)).collect();
        assert!(
            matches!(&recs[0], Outgoing::Frame(Command::VideoBegin { header, compressed_len: 6 }) if header.seq == 4)
        );
        assert!(matches!(
            recs.last(),
            Some(Outgoing::Frame(Command::FrameEnd(4)))
        ));
    }

    /// Masked tiles never go out as tiles; the refresh sends the whole
    /// region losslessly even where nothing changed since.
    #[test]
    fn masked_region_is_left_to_video_and_refreshed_afterwards() {
        let (w, h) = (256, 256);
        let mb = FrameMailbox::default();
        let mut enc = UpdateEncoder::new();
        enc.set_peer_caps(LOCAL_CAPS);
        let mut dec = ScreenDecoder::new();
        let mut px = noise(w, h, 21);
        enc.submit(&px, w, h, None, &mb);
        apply(&mut dec, &mb.take().unwrap());
        let mask = TileRect {
            x: 64,
            y: 64,
            w: 128,
            h: 128,
        };
        // Changes only inside the mask: no tile update at all, but the
        // change is visible to the motion detector.
        px[(100 * 256 + 100) * 4] ^= 0xFF;
        assert_eq!(
            enc.submit_masked(&px, w, h, None, Some(mask), None, &mb),
            SubmitOutcome::Unchanged
        );
        assert!(enc.fresh_valid());
        assert_eq!(enc.fresh().count(), 1);
        assert!(!mb.is_pending());
        // Inside + outside: only the outside tile is sent.
        px[(100 * 256 + 101) * 4] ^= 0xFF;
        px[(10 * 256 + 10) * 4] ^= 0xFF;
        assert!(matches!(
            enc.submit_masked(&px, w, h, None, Some(mask), None, &mb),
            SubmitOutcome::Delta { tiles: 1, .. }
        ));
        let f = mb.take().unwrap();
        // (Wave 1: trimmed to the one changed row of that tile.)
        assert_eq!(
            f.delta.as_ref().unwrap().rects,
            [TileRect {
                x: 0,
                y: 10,
                w: 64,
                h: 1
            }]
        );
        apply(&mut dec, &f);
        assert_ne!(
            dec.frame().unwrap().pixels,
            px,
            "the masked change is not in the tile framebuffer"
        );
        // Motion over: refresh the region with nothing changed since.
        assert_eq!(
            enc.submit(&px, w, h, Some(&[]), &mb),
            SubmitOutcome::Unchanged
        );
        assert!(matches!(
            enc.submit_masked(&px, w, h, Some(&[]), None, Some(mask), &mb),
            SubmitOutcome::Delta { tiles: 4, .. }
        ));
        apply(&mut dec, &mb.take().unwrap());
        assert_eq!(dec.frame().unwrap().pixels, px, "lossless again");
    }

    /// "Encodes" a picture as its first BGRA pixel's blue byte; the fake
    /// decoder paints a picture of that colour.
    struct FakeEnc {
        first: bool,
    }
    impl VideoEncode for FakeEnc {
        fn encode(
            &mut self,
            bgra: &[u8],
            _s: usize,
            force_idr: bool,
            pts_us: u64,
        ) -> Result<Vec<EncodedAu>, String> {
            let key = force_idr || self.first;
            self.first = false;
            Ok(vec![EncodedAu {
                data: vec![0, 0, 0, 1, if key { 0x65 } else { 0x41 }, bgra[0]],
                keyframe: key,
                pts_us,
            }])
        }
        fn set_bitrate(&mut self, _bps: u32) {}
        fn name(&self) -> String {
            "fake".into()
        }
    }
    struct FakeDec {
        decoded: std::rc::Rc<std::cell::Cell<u32>>,
    }
    impl VideoDecode for FakeDec {
        fn decode(
            &mut self,
            annexb: &[u8],
            w: u32,
            h: u32,
            out: &mut Vec<u8>,
        ) -> Result<bool, String> {
            let v = *annexb.last().ok_or("empty unit")?;
            out.clear();
            out.resize((w * h * 4) as usize, v);
            self.decoded.set(self.decoded.get() + 1);
            Ok(true)
        }
    }
    fn fake_factory() -> EncoderFactory {
        Box::new(|_, _, _, _| Ok(Box::new(FakeEnc { first: true }) as Box<dyn VideoEncode>))
    }

    /// CAPS negotiation: only a viewer with a decoder advertises CAP_H264,
    /// and the host's VideoSession only streams to it. End to end over a
    /// session: motion becomes video painted over the tile framebuffer,
    /// rest brings back the exact lossless pixels.
    #[test]
    fn h264_only_to_capable_viewers_and_composited_over_tiles() {
        let caps_seen = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let run = |with_decoder: bool| -> (u32, u64, u32) {
            let (mut host, mut view) = session_pair();
            let decoded = std::rc::Rc::new(std::cell::Cell::new(0));
            if with_decoder {
                view.set_video_decoder(Box::new(FakeDec {
                    decoded: decoded.clone(),
                }));
            }
            let seen = std::sync::Arc::clone(&caps_seen);
            host.on_cmd(Some(Box::new(move |cmd| {
                if let Command::Caps { flags } = cmd {
                    seen.store(*flags, Ordering::Relaxed);
                }
            })));
            view.announce_caps().unwrap();
            let (w, h) = (512u32, 384u32);
            let mb = FrameMailbox::default();
            let mut tx = PrioritySender::new(4096);
            let mut enc = UpdateEncoder::new();
            let cfg = MotionConfig {
                enter_after: Duration::from_millis(100),
                ..MotionConfig::default()
            };
            let mut video = VideoSession::with_config(fake_factory(), 30, cfg);
            let mut px = noise(w, h, 31);
            let region = TileRect {
                x: 128,
                y: 64,
                w: 256,
                h: 256,
            };
            let t0 = Instant::now();
            let mut shown: Option<Frame> = None;
            let mut caps = 0;
            for step in 0..120u64 {
                for _ in 0..3 {
                    host.pump().unwrap();
                    view.pump().unwrap();
                }
                caps = caps_seen.load(Ordering::Relaxed);
                enc.set_peer_caps(caps);
                let now = t0 + Duration::from_millis(step * 33);
                // 60 captures of motion in the region, then rest.
                if step < 60 {
                    let v = (step as u8).wrapping_mul(7) | 1;
                    for y in region.y..region.y + region.h {
                        let o = (usize::from(y) * w as usize + usize::from(region.x)) * 4;
                        px[o..o + usize::from(region.w) * 4]
                            .iter_mut()
                            .for_each(|b| *b = v);
                    }
                }
                // Wave 2: the viewer advertises CAP_LOSSY (noise is
                // "image"): lossy first, refined on the encoder's clock.
                enc.set_clock(Some(now));
                let out = enc.submit_masked(&px, w, h, None, video.active_rect(), None, &mb);
                let _ = out;
                video.on_capture(
                    now,
                    &px,
                    w,
                    h,
                    w as usize * 4,
                    enc.fresh(),
                    enc.fresh_valid(),
                    caps & CAP_H264 != 0,
                    &mb,
                );
                if let Some(r) = video.take_refresh() {
                    // Wave 2: the post-motion refresh rides the refine lane.
                    enc.queue_refresh(r);
                    enc.submit_masked(&px, w, h, Some(&[]), None, None, &mb);
                }
                while tx.has_work(&mb) {
                    tx.flush(&mut host, &mb, 64).unwrap();
                }
                // Refine lane: whenever the link is free.
                while enc.submit_refine(&mb) != SubmitOutcome::Unchanged {
                    while tx.has_work(&mb) {
                        tx.flush(&mut host, &mb, 64).unwrap();
                    }
                }
                for _ in 0..3 {
                    view.pump().unwrap();
                }
                if let Some(f) = view.take_latest_frame() {
                    shown = Some(f);
                }
                if step == 40 && with_decoder {
                    // Mid-motion: the region shows the decoded picture, the
                    // rest the lossless tiles.
                    let f = shown.as_ref().expect("frame");
                    let o =
                        (usize::from(region.y + 10) * w as usize + usize::from(region.x + 10)) * 4;
                    assert_eq!(f.pixels[o], px[o], "video painted over the region");
                    assert!(video.active_rect().is_some());
                }
            }
            let f = shown.expect("frames shown");
            assert_eq!(f.pixels, px, "lossless after the motion ended");
            assert_eq!(video.active_rect(), None);
            (caps, video.stats().frames, decoded.get())
        };
        let (caps, frames, decoded) = run(true);
        assert!(
            caps & CAP_H264 != 0,
            "a viewer with a decoder advertises H.264"
        );
        assert!(
            frames > 20 && decoded as u64 >= frames - 1,
            "frames {frames} decoded {decoded}"
        );
        let (caps, frames, decoded) = run(false);
        assert_eq!(caps & CAP_H264, 0, "no decoder, no CAP_H264");
        assert_eq!(
            (frames, decoded),
            (0, 0),
            "never any video to an incapable viewer"
        );
    }

    /// Two terminal sessions over an in-memory carrier pair: (host, view).
    pub(crate) fn session_pair() -> (TerminalSession, TerminalSession) {
        use ovcore::handshake::{Auth, Handshake, HandshakeEvent, Role};
        use ovcore::identity::Identity;
        use ovtransport::{Carrier, InMemoryCarrier};
        let (id_v, id_h) = (Identity::generate(), Identity::generate());
        let (mut vc, mut hc) = InMemoryCarrier::pair();
        let code = id_h.pairing_code();
        let mut client = Handshake::new(Role::Client, 0x11, &id_v, Auth::PairingCode(code.clone()));
        client.expect_fingerprint(id_h.fingerprint());
        let mut server = Handshake::new(Role::Server, 0x22, &id_h, Auth::PairingCode(code));
        for _ in 0..200 {
            let _ = vc.poll_recv(&mut |d| {
                let _ = client.feed(d);
            });
            let _ = hc.poll_recv(&mut |d| {
                let _ = server.feed(d);
            });
            for ev in client.take_events() {
                let HandshakeEvent::Sent(d) = ev;
                let _ = vc.send(&d);
            }
            for ev in server.take_events() {
                let HandshakeEvent::Sent(d) = ev;
                let _ = hc.send(&d);
            }
            if client.is_established() && server.is_established() {
                break;
            }
        }
        let view = TerminalSession::new(client.take_session().expect("client"), vc);
        let host = TerminalSession::new(server.take_session().expect("server"), hc);
        (host, view)
    }

    /// A "document": unique text-like rows.
    fn doc(w: u32, h: u32, seed: u32) -> Vec<u8> {
        let mut px = vec![255u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let o = ((y * w + x) * 4) as usize;
                if (x / 6 + y / 3) % 5 != 0 {
                    let v = ((x.wrapping_mul(7) ^ (y + seed).wrapping_mul(131)) % 251) as u8;
                    px[o] = v;
                    px[o + 1] = v.wrapping_mul(3);
                    px[o + 2] = (y % 256) as u8;
                }
            }
        }
        px
    }

    /// Scroll `win` of `px` up by `dy`, new content in the exposed strip.
    fn scroll_up(px: &[u8], w: u32, win: TileRect, dy: usize, t: u32) -> Vec<u8> {
        let stride = w as usize * 4;
        let mut cur = px.to_vec();
        let (x0, x1) = (usize::from(win.x) * 4, usize::from(win.x + win.w) * 4);
        let (y0, y1) = (usize::from(win.y), usize::from(win.y + win.h));
        for y in y0..y1 - dy {
            let src = px[(y + dy) * stride + x0..(y + dy) * stride + x1].to_vec();
            cur[y * stride + x0..y * stride + x1].copy_from_slice(&src);
        }
        for y in y1 - dy..y1 {
            for x in (x0..x1).step_by(4) {
                cur[y * stride + x] = (x as u32 * 13 + y as u32 * 7 + t) as u8;
            }
        }
        cur
    }

    /// Wave 1 copy-rect end to end: scroll steps become MOVE + strip, the
    /// decoder reproduces every frame exactly, the bytes collapse, and the
    /// scroll does not register as motion (`fresh` = residual only).
    #[test]
    fn scrolls_are_sent_as_moves_and_decode_exactly() {
        let (w, h) = (1280u32, 720u32);
        let win = TileRect {
            x: 180,
            y: 100,
            w: 900,
            h: 560,
        };
        let mut px = doc(w, h, 0);
        let mb = FrameMailbox::default();
        let mut dec = ScreenDecoder::new();
        // Same steps for a viewer without CAP_MOVE (the baseline bytes).
        let mut old = UpdateEncoder::new();
        old.set_peer_caps(LOCAL_CAPS & !crate::terminal::CAP_MOVE & !CAP_BANDS);
        let mbo = FrameMailbox::default();
        let mut enc = UpdateEncoder::new();
        enc.set_peer_caps(LOCAL_CAPS);
        enc.submit(&px, w, h, None, &mb);
        apply(&mut dec, &mb.take().unwrap());
        old.submit(&px, w, h, None, &mbo);
        mbo.take();
        let (mut with, mut without) = (0usize, 0usize);
        for step in 0..6 {
            px = scroll_up(&px, w, win, 24, step);
            match enc.submit(&px, w, h, None, &mb) {
                SubmitOutcome::Delta { bytes, .. } => with += bytes,
                other => panic!("step {step}: {other:?}"),
            }
            assert!(
                enc.fresh().count() <= 2 * 15,
                "residual only: {}",
                enc.fresh().count()
            );
            let f = mb.take().unwrap();
            assert_eq!(f.codec, FrameCodec::Packed);
            apply(&mut dec, &f);
            assert_eq!(dec.frame().unwrap().pixels, px, "step {step}");
            assert!(
                dec.last_dirty().iter().any(|r| r.w == 900),
                "moved rect reported dirty"
            );
            if let SubmitOutcome::Delta { bytes, .. } | SubmitOutcome::Full { bytes } =
                old.submit(&px, w, h, None, &mbo)
            {
                without += bytes;
            }
            mbo.take();
        }
        assert_eq!(enc.stats().moves, 6);
        eprintln!("6 scroll steps: {with} bytes with MOVE vs {without} without");
        assert!(with * 5 < without, "{with} vs {without}");
    }

    /// A move update replaced in the mailbox before it was sent: the next
    /// update (against the older base) still reconstructs exactly.
    #[test]
    fn a_replaced_move_update_is_recovered_by_the_next_delta() {
        let (w, h) = (1024u32, 640u32);
        let win = TileRect {
            x: 64,
            y: 64,
            w: 800,
            h: 512,
        };
        let mut px = doc(w, h, 3);
        let mb = FrameMailbox::default();
        let mut dec = ScreenDecoder::new();
        let mut enc = UpdateEncoder::new();
        enc.set_peer_caps(LOCAL_CAPS);
        enc.submit(&px, w, h, None, &mb);
        apply(&mut dec, &mb.take().unwrap());
        px = scroll_up(&px, w, win, 30, 1);
        enc.submit(&px, w, h, None, &mb); // a MOVE update, never taken
        assert_eq!(enc.stats().moves, 1);
        px = scroll_up(&px, w, win, 30, 2);
        enc.submit(&px, w, h, None, &mb); // replaces it: no move (not chained)
        apply(&mut dec, &mb.take().unwrap());
        assert_eq!(dec.frame().unwrap().pixels, px);
        // And chaining resumes with moves.
        px = scroll_up(&px, w, win, 30, 3);
        enc.submit(&px, w, h, None, &mb);
        apply(&mut dec, &mb.take().unwrap());
        assert_eq!(dec.frame().unwrap().pixels, px);
        assert_eq!(enc.stats().moves, 2);
    }

    /// OS move rects (DXGI) are verified against the mirror: a correct one
    /// is used, a wrong one is ignored (the detector still finds it).
    #[test]
    fn os_move_rects_are_verified_before_use() {
        let (w, h) = (800u32, 600u32);
        let win = TileRect {
            x: 0,
            y: 0,
            w: 800,
            h: 600,
        };
        let mut px = doc(w, h, 9);
        let mb = FrameMailbox::default();
        let mut dec = ScreenDecoder::new();
        let mut enc = UpdateEncoder::new();
        enc.set_peer_caps(LOCAL_CAPS);
        enc.submit(&px, w, h, None, &mb);
        apply(&mut dec, &mb.take().unwrap());
        px = scroll_up(&px, w, win, 20, 1);
        let good = MoveRect {
            x: 0,
            y: 0,
            w: 800,
            h: 580,
            dx: 0,
            dy: -20,
        };
        enc.submit_ex(&px, w, h, None, Some(&[good]), None, None, &mb);
        apply(&mut dec, &mb.take().unwrap());
        assert_eq!(dec.frame().unwrap().pixels, px);
        assert_eq!((enc.stats().moves, enc.stats().detected_moves), (1, 0));
        px = scroll_up(&px, w, win, 20, 2);
        let wrong = MoveRect { dy: -7, ..good };
        enc.submit_ex(&px, w, h, None, Some(&[wrong]), None, None, &mb);
        apply(&mut dec, &mb.take().unwrap());
        assert_eq!(dec.frame().unwrap().pixels, px);
        assert_eq!((enc.stats().moves, enc.stats().detected_moves), (2, 1));
    }

    /// Wave 1: the post-motion refresh drains in bounded chunks, each
    /// update stays small, and the result is exact.
    #[test]
    fn refresh_backlog_drains_in_small_chunks() {
        let (w, h) = (1024u32, 768u32);
        let px = noise(w, h, 31);
        let mb = FrameMailbox::default();
        let mut dec = ScreenDecoder::new();
        let mut enc = UpdateEncoder::new();
        enc.set_peer_caps(LOCAL_CAPS);
        enc.submit(&px, w, h, None, &mb);
        apply(&mut dec, &mb.take().unwrap());
        // Pretend video painted the region lossy on the viewer.
        let region = TileRect {
            x: 128,
            y: 128,
            w: 640,
            h: 448,
        };
        let stride = w as usize * 4;
        let mut lossy = dec.frame().unwrap().pixels.clone();
        for y in 128..576usize {
            for b in &mut lossy[y * stride + 128 * 4..y * stride + 768 * 4] {
                *b ^= 1;
            }
        }
        dec.blit(
            TileRect {
                x: 0,
                y: 0,
                w: w as u16,
                h: h as u16,
            },
            w,
            h,
            &lossy,
        );
        enc.queue_refresh(region);
        assert_eq!(enc.refresh_pending(), 70);
        let mut updates = 0;
        while enc.refresh_pending() > 0 {
            match enc.submit(&px, w, h, Some(&[]), &mb) {
                SubmitOutcome::Delta { tiles, .. } => assert!(tiles <= 64, "{tiles}"),
                other => panic!("{other:?}"),
            }
            let f = mb.take().unwrap();
            assert!(
                f.data.len() <= 2 * REFRESH_CHUNK_BYTES + 4096 || updates == 0,
                "{}",
                f.data.len()
            );
            apply(&mut dec, &f);
            updates += 1;
            assert!(updates < 100);
        }
        assert!(updates >= 2, "chunked: {updates}");
        assert_eq!(dec.frame().unwrap().pixels, px);
        assert_eq!(
            enc.submit(&px, w, h, Some(&[]), &mb),
            SubmitOutcome::Unchanged
        );
    }

    /// Wave 1: photo-like (incompressible) content goes out as LZ4/raw
    /// bands only on a fast link; a slow or unknown link gets zstd.
    #[cfg(feature = "zstd-encode")]
    #[test]
    fn lz4_bands_only_on_a_fast_link() {
        let (w, h) = (512u32, 512u32);
        // Photo-like: a gradient plus fine noise (zstd ~1.2x, LZ4 ~1x).
        let px: Vec<u8> = noise(w, h, 77)
            .iter()
            .enumerate()
            .map(|(i, n)| {
                if i % 4 == 3 {
                    255
                } else {
                    ((i / 4) % 512 / 4) as u8 / 2 + (n & 15)
                }
            })
            .collect();
        for (rate, want_lz4) in [(0u64, false), (1_000_000, false), (50_000_000, true)] {
            let mb = FrameMailbox::default();
            let mut enc = UpdateEncoder::new();
            enc.set_peer_caps(LOCAL_CAPS);
            enc.set_link_rate(rate);
            enc.submit(&px, w, h, None, &mb);
            let f = mb.take().unwrap();
            let mut dec = ScreenDecoder::new();
            apply(&mut dec, &f);
            assert_eq!(dec.frame().unwrap().pixels, px);
            let st = enc.stats();
            assert_eq!(st.zstd_bands == 0, want_lz4, "rate {rate}: {st:?}");
        }
    }

    // ---------------- Wave 2: lossy-first, refinement, tile cache ----------------

    use crate::bands::{FLAG_CACHE, FLAG_LOSSY};
    use crate::terminal::{CAP_CACHE, CAP_LOSSY, VIEWER_W2_CAPS};

    const W2: u32 = LOCAL_CAPS | VIEWER_W2_CAPS;

    /// A desktop-ish screen: text everywhere, a photo window at `photo_at`.
    fn desk(w: u32, h: u32, photo_at: Option<TileRect>, seed: u64) -> Vec<u8> {
        let mut px = crate::lossy::tests::text(w, h, seed);
        if let Some(r) = photo_at {
            paste_photo(&mut px, w, r, seed ^ 0x55);
        }
        px
    }

    fn paste_photo(px: &mut [u8], w: u32, r: TileRect, seed: u64) {
        let p = crate::lossy::tests::photo(u32::from(r.w), u32::from(r.h), seed);
        let stride = w as usize * 4;
        let rw = usize::from(r.w) * 4;
        for y in 0..usize::from(r.h) {
            let d = (usize::from(r.y) + y) * stride + usize::from(r.x) * 4;
            px[d..d + rw].copy_from_slice(&p[y * rw..(y + 1) * rw]);
        }
    }

    /// Container flags of an update (0 for a non-container codec).
    fn flags(f: &EncodedFrame) -> u8 {
        if f.codec == FrameCodec::Packed {
            f.data[1]
        } else {
            0
        }
    }

    /// Run the refine lane dry (each refine update taken and applied):
    /// (updates, largest update bytes).
    fn drain_refine(
        enc: &mut UpdateEncoder,
        mb: &FrameMailbox,
        dec: &mut ScreenDecoder,
    ) -> (usize, usize) {
        let (mut n, mut max) = (0, 0);
        while enc.submit_refine(mb) != SubmitOutcome::Unchanged {
            let f = mb.take().expect("refine update queued");
            max = max.max(f.data.len());
            apply(dec, &f);
            n += 1;
            assert!(n < 2000, "refine lane never drains");
        }
        (n, max)
    }

    fn tile_eq(a: &[u8], b: &[u8], w: u32, h: u32, c: u32, r: u32) -> bool {
        let tr = crate::lossy::tile_rect(c, r, w, h);
        let stride = w as usize * 4;
        (usize::from(tr.y)..usize::from(tr.y + tr.h)).all(|y| {
            let o = y * stride + usize::from(tr.x) * 4;
            a[o..o + usize::from(tr.w) * 4] == b[o..o + usize::from(tr.w) * 4]
        })
    }

    fn dirty_covers(dirty: &[TileRect], w: u32, h: u32, c: u32, r: u32) -> bool {
        let tr = crate::lossy::tile_rect(c, r, w, h);
        dirty.iter().any(|d| {
            d.x <= tr.x && d.y <= tr.y && d.x + d.w >= tr.x + tr.w && d.y + d.h >= tr.y + tr.h
        })
    }

    /// Wave 2 core: photo tiles go out lossy first (a fraction of the
    /// lossless bytes), text stays exact, and once the photo settled the
    /// refine lane converges to the exact pixels in small chunks.
    #[test]
    fn lossy_first_then_refined_to_exact_pixels() {
        let (w, h) = (640u32, 384u32);
        let photo = TileRect {
            x: 128,
            y: 64,
            w: 320,
            h: 256,
        }; // 5x4 tiles
        let px = desk(w, h, Some(photo), 1);
        let t0 = Instant::now();
        // Lossless reference (a Wave 1 viewer).
        let lossless_bytes = {
            let mb = FrameMailbox::default();
            let mut enc = UpdateEncoder::new();
            enc.set_peer_caps(LOCAL_CAPS);
            match enc.submit(&px, w, h, None, &mb) {
                SubmitOutcome::Full { bytes } => bytes,
                o => panic!("{o:?}"),
            }
        };
        let mb = FrameMailbox::default();
        let mut enc = UpdateEncoder::new();
        let mut dec = ScreenDecoder::new();
        enc.set_peer_caps(W2);
        enc.set_clock(Some(t0));
        let first = match enc.submit(&px, w, h, None, &mb) {
            SubmitOutcome::Full { bytes } => bytes,
            o => panic!("{o:?}"),
        };
        let f = mb.take().unwrap();
        assert_ne!(flags(&f) & FLAG_LOSSY, 0);
        assert_ne!(flags(&f) & FLAG_CACHE, 0);
        apply(&mut dec, &f);
        assert!(
            first * 3 < lossless_bytes,
            "lossy first {first} vs lossless {lossless_bytes}"
        );
        assert_eq!(dec.lossy_tiles().count(), 20);
        assert_eq!(enc.refresh_pending(), 20);
        assert_eq!(enc.stats().lossy_tiles, 20);
        let fb = dec.frame().unwrap().pixels.clone();
        for r in 0..h / 64 {
            for c in 0..w / 64 {
                let in_photo = (2..7).contains(&c) && (1..5).contains(&r);
                assert_eq!(tile_eq(&fb, &px, w, h, c, r), !in_photo, "tile ({c},{r})");
            }
        }
        // Text tiles went into the cache, lossy ones did not.
        assert_eq!(dec.cache_slots_filled(), (w / 64 * (h / 64)) as usize - 20);
        // Not settled yet: nothing to refine.
        enc.set_clock(Some(t0 + Duration::from_millis(200)));
        assert!(!enc.refine_due());
        assert_eq!(enc.submit_refine(&mb), SubmitOutcome::Unchanged);
        assert_eq!(enc.submit(&px, w, h, None, &mb), SubmitOutcome::Unchanged);
        // Settled: refined in chunks, to the exact pixels.
        enc.set_clock(Some(t0 + Duration::from_millis(350)));
        enc.set_link_rate(1_250_000); // 10 Mbit/s: ~19 KB chunks
        assert!(enc.refine_due());
        let (n, max) = drain_refine(&mut enc, &mb, &mut dec);
        assert!(n >= 3, "chunked: {n}");
        assert!(max <= 2 * REFRESH_CHUNK_BYTES, "chunk {max}");
        assert_eq!(dec.frame().unwrap().pixels, px);
        assert_eq!(dec.lossy_tiles().count(), 0);
        assert_eq!(enc.refresh_pending(), 0);
        assert_eq!(enc.stats().refined_tiles, 20);
        // Refined tiles are exact now: cached.
        assert_eq!(dec.cache_slots_filled(), (w / 64 * (h / 64)) as usize);
        assert_eq!(enc.submit(&px, w, h, None, &mb), SubmitOutcome::Unchanged);
    }

    /// A tile that changes again while its refinement waits unsent is sent
    /// lossy again (the refinement is superseded), and its own clock
    /// restarts.
    #[test]
    fn refinement_is_cancelled_when_the_tile_changes_again() {
        let (w, h) = (640u32, 384u32);
        let photo = TileRect {
            x: 128,
            y: 64,
            w: 320,
            h: 256,
        };
        let mut px = desk(w, h, Some(photo), 1);
        let t0 = Instant::now();
        let mb = FrameMailbox::default();
        let mut enc = UpdateEncoder::new();
        let mut dec = ScreenDecoder::new();
        enc.set_peer_caps(W2);
        enc.set_clock(Some(t0));
        enc.submit(&px, w, h, None, &mb);
        apply(&mut dec, &mb.take().unwrap());
        // Settled: a refine chunk is queued but the link has not taken it.
        enc.set_clock(Some(t0 + Duration::from_millis(350)));
        assert!(matches!(
            enc.submit_refine(&mb),
            SubmitOutcome::Delta { .. }
        ));
        assert!(mb.is_pending());
        // The photo changes (a new picture) before the chunk went out.
        paste_photo(&mut px, w, photo, 99);
        enc.set_clock(Some(t0 + Duration::from_millis(360)));
        let replaced_before = mb.replaced_count();
        assert!(matches!(
            enc.submit(&px, w, h, None, &mb),
            SubmitOutcome::Delta { .. }
        ));
        assert_eq!(
            mb.replaced_count(),
            replaced_before + 1,
            "the refine chunk was superseded"
        );
        let f = mb.take().unwrap();
        assert_ne!(flags(&f) & FLAG_LOSSY, 0, "changed tiles go lossy again");
        // No photo tile travels losslessly in it.
        for r in &f.delta.as_ref().unwrap().rects {
            let inter = r.x < photo.x + photo.w
                && photo.x < r.x + r.w
                && r.y < photo.y + photo.h
                && photo.y < r.y + r.h;
            assert!(!inter, "lossless rect {r:?} inside the changed photo");
        }
        apply(&mut dec, &f);
        assert_eq!(dec.lossy_tiles().count(), 20);
        for c in 2..7 {
            for r in 1..5 {
                assert!(
                    dirty_covers(dec.last_dirty(), w, h, c, r),
                    "lossy tile ({c},{r}) reported dirty"
                );
            }
        }
        // Its clock restarted at 360 ms: not due at 600 ms, due at 700 ms.
        enc.set_clock(Some(t0 + Duration::from_millis(600)));
        assert!(!enc.refine_due());
        enc.set_clock(Some(t0 + Duration::from_millis(700)));
        drain_refine(&mut enc, &mb, &mut dec);
        assert_eq!(dec.frame().unwrap().pixels, px);
        assert_eq!(dec.lossy_tiles().count(), 0);
    }

    /// The host's cache model and the viewer's slots stay identical over
    /// many random updates — screens switching back and forth, edits,
    /// photos, refinement, a tiny cache (constant eviction), and the
    /// mailbox dropping unsent updates — and every tile the host believes
    /// exact is exact on the viewer.
    #[test]
    fn cache_and_lossy_state_stay_in_sync_over_random_updates() {
        let (w, h) = (384u32, 256u32); // 6x4 = 24 tiles
        let screens: Vec<Vec<u8>> = (0..4)
            .map(|i| {
                desk(
                    w,
                    h,
                    (i % 2 == 1).then_some(TileRect {
                        x: 64,
                        y: 64,
                        w: 192,
                        h: 128,
                    }),
                    10 + i,
                )
            })
            .collect();
        let mb = FrameMailbox::default();
        let mut enc = UpdateEncoder::new();
        let mut dec = ScreenDecoder::new();
        enc.set_peer_caps(W2);
        enc.set_cache_slots(16);
        let t0 = Instant::now();
        let mut t = Duration::ZERO;
        enc.set_clock(Some(t0));
        let mut rng = Rng(0xBADC0DE);
        let mut px = screens[0].clone();
        let mut captured = std::collections::HashMap::new();
        let mut applied = 0;
        for step in 0..1500 {
            match rng.below(10) {
                0..=2 => px.copy_from_slice(&screens[rng.below(4) as usize]),
                3..=5 => mutate(&mut px, w, h, &mut rng),
                6 => {
                    let r = TileRect {
                        x: 64 * rng.below(4) as u16,
                        y: 64 * rng.below(2) as u16,
                        w: 128,
                        h: 128,
                    };
                    paste_photo(&mut px, w, r, u64::from(rng.below(5)));
                }
                _ => {}
            }
            t += Duration::from_millis(u64::from(rng.below(120)));
            enc.set_clock(Some(t0 + t));
            let seq = enc.next_seq();
            if enc.submit(&px, w, h, None, &mb) != SubmitOutcome::Unchanged {
                captured.insert(seq, px.clone());
            }
            if rng.below(4) == 0 {
                let seq = enc.next_seq();
                if enc.submit_refine(&mb) != SubmitOutcome::Unchanged {
                    captured.insert(seq, enc.last_px.clone());
                }
            }
            if rng.below(3) == 0 || step % 40 == 39 {
                if let Some(f) = mb.take() {
                    apply(&mut dec, &f);
                    applied += 1;
                    // Cache sync: the host's model of this update vs the
                    // viewer's slots.
                    let model = enc.cache_tentative.as_ref().unwrap_or(&enc.cache_committed);
                    for s in 0..model.capacity() as u16 {
                        if let Some(k) = model.slot_key(s) {
                            assert_eq!(
                                dec.cache_slot_hash(s),
                                Some(k),
                                "slot {s} after update {}",
                                f.seq
                            );
                        }
                    }
                    // Exactness: tiles outside the host's backlog are exact.
                    let want = &captured[&f.seq];
                    let fb = &dec.frame().unwrap().pixels;
                    for r in 0..enc.backlog.rows() {
                        for c in 0..enc.backlog.cols() {
                            if !enc.backlog.get(c, r) {
                                assert!(
                                    tile_eq(fb, want, w, h, c, r),
                                    "tile ({c},{r}) after update {}",
                                    f.seq
                                );
                            }
                        }
                    }
                }
            }
        }
        let st = enc.stats();
        assert!(applied > 300, "{applied}");
        assert!(st.cache_refs > 100 && st.cache_stores > 100, "{st:?}");
        assert!(st.lossy_tiles > 50 && st.refined_tiles > 20, "{st:?}");
        // Settle: everything converges to exact.
        if let Some(f) = mb.take() {
            apply(&mut dec, &f);
        }
        enc.set_clock(Some(t0 + t + Duration::from_secs(1)));
        if enc.submit(&px, w, h, None, &mb) != SubmitOutcome::Unchanged {
            apply(&mut dec, &mb.take().unwrap());
        }
        enc.set_clock(Some(t0 + t + Duration::from_secs(2)));
        drain_refine(&mut enc, &mb, &mut dec);
        assert_eq!(dec.frame().unwrap().pixels, px);
        assert_eq!(enc.refresh_pending(), 0);
    }

    /// Window switching: going back to a screen the viewer has seen costs
    /// cache REFs (a few bytes per tile) instead of pixels.
    #[test]
    fn switching_back_to_a_seen_window_is_sent_as_cache_refs() {
        let (w, h) = (1280u32, 768u32);
        let a = desk(w, h, None, 1);
        let b = desk(w, h, None, 2);
        let run = |caps: u32| -> (usize, ScreenDecoder) {
            let mb = FrameMailbox::default();
            let mut enc = UpdateEncoder::new();
            let mut dec = ScreenDecoder::new();
            enc.set_peer_caps(caps);
            let mut last = 0;
            for px in [&a, &b, &a] {
                last = match enc.submit(px, w, h, None, &mb) {
                    SubmitOutcome::Full { bytes } | SubmitOutcome::Delta { bytes, .. } => bytes,
                    o => panic!("{o:?}"),
                };
                apply(&mut dec, &mb.take().unwrap());
            }
            assert_eq!(dec.frame().unwrap().pixels, a);
            (last, dec)
        };
        let (with_cache, dec) = run(W2);
        let (without, _) = run(LOCAL_CAPS);
        assert!(
            with_cache * 20 < without,
            "back to A: {with_cache} B with the cache vs {without} without"
        );
        // Every tile of the switch is reported dirty (the Metal path uploads it).
        for r in 0..h / 64 {
            for c in 0..w / 64 {
                assert!(dirty_covers(dec.last_dirty(), w, h, c, r));
            }
        }
    }

    /// CAPS interop: Wave 2 sections only reach a viewer that advertised
    /// them (and CAP_BANDS); every combination still ends exact.
    #[test]
    fn wave2_sections_only_for_viewers_that_advertised_them() {
        let (w, h) = (512u32, 320u32);
        let photo = TileRect {
            x: 64,
            y: 64,
            w: 256,
            h: 192,
        };
        let a = desk(w, h, Some(photo), 3);
        let b = desk(w, h, None, 4);
        for caps in [
            LOCAL_CAPS,
            LOCAL_CAPS | CAP_LOSSY,
            LOCAL_CAPS | CAP_CACHE,
            W2,
            W2 & !CAP_BANDS,
            CAP_TILES | CAP_KEYFRAME | VIEWER_W2_CAPS,
            0,
        ] {
            let mb = FrameMailbox::default();
            let mut enc = UpdateEncoder::new();
            let mut dec = ScreenDecoder::new();
            enc.set_peer_caps(caps);
            let t0 = Instant::now();
            enc.set_clock(Some(t0));
            let mut seen = 0u8;
            for (i, px) in [&a, &b, &a].into_iter().enumerate() {
                enc.set_clock(Some(t0 + Duration::from_millis(10 * i as u64)));
                if enc.submit(px, w, h, None, &mb) != SubmitOutcome::Unchanged {
                    let f = mb.take().unwrap();
                    seen |= flags(&f);
                    apply(&mut dec, &f);
                }
            }
            enc.set_clock(Some(t0 + Duration::from_secs(1)));
            while enc.submit_refine(&mb) != SubmitOutcome::Unchanged {
                let f = mb.take().unwrap();
                seen |= flags(&f);
                apply(&mut dec, &f);
            }
            let bands = caps & CAP_BANDS != 0;
            assert_eq!(
                seen & FLAG_LOSSY != 0,
                bands && caps & CAP_LOSSY != 0,
                "caps {caps:#x}"
            );
            assert_eq!(
                seen & FLAG_CACHE != 0,
                bands && caps & CAP_CACHE != 0,
                "caps {caps:#x}"
            );
            assert_eq!(dec.frame().unwrap().pixels, a, "caps {caps:#x}");
        }
    }

    /// The post-H.264 refresh rides the refine lane for any tile viewer:
    /// the region drains in chunks after VIDEO_END.
    #[test]
    fn video_region_is_refined_through_the_refine_lane() {
        let (w, h) = (640u32, 384u32);
        let px = desk(w, h, None, 5);
        let mb = FrameMailbox::default();
        let mut enc = UpdateEncoder::new();
        let mut dec = ScreenDecoder::new();
        enc.set_peer_caps(W2);
        enc.submit(&px, w, h, None, &mb);
        apply(&mut dec, &mb.take().unwrap());
        // The viewer painted video over a region (lossy pixels).
        let region = TileRect {
            x: 64,
            y: 64,
            w: 448,
            h: 256,
        };
        let video: Vec<u8> = vec![77; region.area() * 4];
        assert!(dec.blit(region, w, h, &video));
        assert_eq!(dec.lossy_tiles().count(), 28);
        enc.queue_refresh(region);
        assert!(enc.refine_due(), "a video region is due at once");
        let (n, _) = drain_refine(&mut enc, &mb, &mut dec);
        assert!(n >= 1);
        assert_eq!(dec.frame().unwrap().pixels, px);
        assert_eq!(dec.lossy_tiles().count(), 0);
    }
}
