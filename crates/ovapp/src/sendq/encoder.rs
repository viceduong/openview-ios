//! [`UpdateEncoder`]: full frame vs tile delta, MOVE detection and band compression.

use std::time::{Duration, Instant};

use crate::bands::{BandPolicy, MoveRect};
use crate::codec::Compressor;
use crate::screen::PixelFormat;
use crate::terminal::{FrameCodec, CAP_BANDS, CAP_LZ4, CAP_MOVE, CAP_TILES, CAP_ZSTD};
use crate::tiles::{
    copy_rects, diff_tiles, gather, rects_from_map, Delta, TileMap, TileRect, MAX_TILE_FRAME_DIM,
    MAX_TILE_RECTS,
};

use super::*;

/// Capture-thread encoder: diffs each capture, decides full frame
/// vs tile delta, compresses, and hands the result to a [`FrameMailbox`]
/// with the right base (see the module docs).
///
/// Mode follows the viewer's CAPS ([`set_peer_caps`](Self::set_peer_caps)):
/// without [`CAP_TILES`] every change is a DEFLATE full frame exactly as
/// older viewers expect; zstd only with [`CAP_ZSTD`] (and a `zstd-encode` build).
pub struct UpdateEncoder {
    pub(super) peer_caps: u32,
    pub(super) seq: u64,
    /// Pixels of the last queued update (tight BGRA).
    pub(super) last_px: Vec<u8>,
    pub(super) last_dims: Option<(u32, u32)>,
    /// Seq of the last queued update.
    pub(super) last_put: Option<u64>,
    /// The frame the viewer holds before our queued update lands (the
    /// mailbox's last-taken when we queued).
    pub(super) base: Option<u64>,
    /// Tiles that differ between `base` and `last_px`.
    pub(super) pending: TileMap,
    /// Scratch: tiles that differ between `last_px` and the new capture.
    pub(super) fresh: TileMap,
    /// Scratch: tiles a delta must carry.
    pub(super) acc: TileMap,
    pub(super) force_full: bool,
    pub(super) rects: Vec<TileRect>,
    pub(super) gather_buf: Vec<u8>,
    pub(super) comp: Compressor,
    /// Tiles of the video region (scratch).
    pub(super) mask_map: TileMap,
    /// `fresh` holds real changes (not a forced fill).
    pub(super) fresh_valid: bool,
    /// MOVE records of the update being built (scratch).
    pub(super) moves: Vec<MoveRect>,
    /// Hint rects for the post-move re-diff (scratch).
    pub(super) move_hints: Vec<TileRect>,
    /// Counters (moves sent, band codecs used).
    pub(super) stats: EncoderStats,
    /// Tiles still owed a lossless refresh (after H.264 motion),
    /// sent a bounded chunk per update ([`REFRESH_CHUNK_BYTES`]).
    pub(super) backlog: TileMap,
    /// Scratch: this update's refresh chunk.
    pub(super) chunk: TileMap,
    /// Smoothed wire bytes per refreshed tile (chunk sizing).
    pub(super) refresh_tile_bytes: f64,
    /// The pacer's delivery rate (bytes/s, 0 = unknown): LZ4 bands
    /// only on a link fast enough that bytes matter less than decode time.
    pub(super) link_rate: u64,
    // ---- Lossy-first images, refinement lane, tile cache ----
    /// Host switches (both default on; effective only for a viewer with
    /// the CAPS bits).
    pub(super) lossy_on: bool,
    pub(super) cache_on: bool,
    pub(super) lossy_params: crate::lossy::LossyParams,
    /// Per tile: ms (since `epoch`) of its last change. `backlog` tiles
    /// are refined once they stayed unchanged [`REFINE_DELAY`].
    pub(super) changed_ms: Vec<u32>,
    pub(super) epoch: Instant,
    /// Test clock override.
    pub(super) clock: Option<Instant>,
    /// Viewer tile-cache model as of the last update the sender took ...
    pub(super) cache_committed: crate::lossy::TileCache,
    /// ... and after our last queued (not yet taken) update.
    pub(super) cache_tentative: Option<crate::lossy::TileCache>,
    /// Our last queued update is a full frame (a follow-up that replaces
    /// it must be full too).
    pub(super) pending_full: bool,
    /// Scratch maps: image tiles, cache hits, lossless tiles, trimmable
    /// lossless tiles, tiles a MOVE fills with lossy pixels, classifier
    /// candidates.
    pub(super) img_map: TileMap,
    pub(super) ref_map: TileMap,
    pub(super) lossless_map: TileMap,
    pub(super) trim_map: TileMap,
    pub(super) move_lossy: TileMap,
    pub(super) cand_map: TileMap,
    /// Scratch: a full frame with its lossy tiles blanked; a copy of
    /// `last_px` for refine-only updates.
    pub(super) blank_buf: Vec<u8>,
    pub(super) refine_buf: Vec<u8>,
}

/// A lossy tile is refined to exact pixels once it stayed
/// unchanged this long.
pub(crate) const REFINE_DELAY: Duration = Duration::from_millis(300);

/// A refinement chunk occupies the link for at most about this
/// long (at the pacer's delivery rate), so a change behind it waits little.
pub(crate) const REFINE_SLICE: Duration = Duration::from_millis(15);

/// Smallest refinement chunk (bytes) on a slow link.
pub(crate) const REFINE_MIN_CHUNK: usize = 8 * 1024;

/// LZ4 (fast decode, ~15 % bigger on photo content) is only chosen
/// for low-ratio bands when the link delivers at least this (bytes/s,
/// ~32 Mbit/s: LAN / direct path); slower or unknown links get zstd.
pub(crate) const LZ4_MIN_LINK_RATE: u64 = 4_000_000;

/// The lossless refresh after H.264 motion goes out in chunks of
/// about this many wire bytes, one per update, so an input-driven change
/// never waits behind a whole-region refresh.
pub(crate) const REFRESH_CHUNK_BYTES: usize = 64 * 1024;

/// [`UpdateEncoder`] counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EncoderStats {
    /// Updates that carried a MOVE (scroll / OS move rect).
    pub moves: u64,
    /// Of those, moves found by the scroll detector (the rest came from
    /// verified OS move rects).
    pub detected_moves: u64,
    /// Tiles the moves saved (dirty before - residual after).
    pub tiles_saved: u64,
    /// Bands sent as LZ4 in band containers.
    pub lz4_bands: u64,
    /// Bands sent as zstd.
    pub zstd_bands: u64,
    /// Bands sent raw.
    pub raw_bands: u64,
    /// Tiles sent as lossy JPEG.
    pub lossy_tiles: u64,
    /// JPEG bytes of those lossy tiles.
    pub lossy_bytes: u64,
    /// Lossy (or video-painted) tiles refined to exact pixels.
    pub refined_tiles: u64,
    /// Tiles sent as cache REFs.
    pub cache_refs: u64,
    /// Cache slots filled by STORE records.
    pub cache_stores: u64,
}

impl Default for UpdateEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl UpdateEncoder {
    /// A fresh encoder: the first update is a full frame.
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
}

impl UpdateEncoder {
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

    /// Copy-rect: find a MOVE that turns `last_px` (what the viewer
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

    /// [`submit`](Self::submit) with a video region: tiles
    /// touching `mask` (the region streamed as H.264) are left out of tile
    /// deltas — the viewer shows video there — and tiles touching `refresh`
    /// are sent whether they changed or not (the lossless refresh after
    /// `VIDEO_END`). Full frames always carry everything.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn submit_masked(
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

    /// `submit_masked` plus copy-rect: for a
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
        // Copy-rect, only when the viewer will hold exactly
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
        // The tiles that really changed restart their refine clock.
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
        // A MOVE whose source holds lossy pixels carries them to
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
                // The viewer will apply our previous update: its
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
                // ---- Split: cache hits / lossy images / lossless.
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
                        // What the viewer will hold inexactly.
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
            // ---- Full frame (image tiles lossy, blanked in the
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

/// The tiles set in `m`, raster order, as `(col, row)`.
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

/// Content hashes (size included) of the tiles set in `m`,
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

/// Zero the tiles set in `m` (a full frame's lossy tiles: the
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
