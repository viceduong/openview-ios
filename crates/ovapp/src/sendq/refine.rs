//! [`UpdateEncoder`] lossy/cache settings, the refinement lane and the lossless refresh backlog.

use std::time::Instant;

use crate::terminal::CAP_BANDS;
use crate::tiles::{TileMap, TileRect};

use super::*;

impl UpdateEncoder {
    /// Allow lossy-first image tiles (default on; only used for a
    /// viewer with `CAP_LOSSY` + `CAP_BANDS`).
    pub fn set_lossy(&mut self, on: bool) {
        self.lossy_on = on;
    }

    /// Allow the tile cache (default on; only used for a viewer
    /// with `CAP_CACHE` + `CAP_BANDS`).
    pub fn set_cache(&mut self, on: bool) {
        self.cache_on = on;
    }

    /// Tile-cache slots to use (default `crate::lossy::CACHE_SLOTS`,
    /// at most `crate::lossy::MAX_SLOTS`). Forgets the cache and makes
    /// the next update a full frame (which empties the viewer's slots).
    pub fn set_cache_slots(&mut self, n: usize) {
        self.cache_committed = crate::lossy::TileCache::new(n);
        self.cache_tentative = None;
        self.force_full = true;
    }

    /// JPEG quality / chroma for image tiles.
    pub fn set_lossy_params(&mut self, p: crate::lossy::LossyParams) {
        self.lossy_params = p;
    }

    /// Pin the encoder's clock (tests / simulations); `None` =
    /// real time.
    pub fn set_clock(&mut self, now: Option<Instant>) {
        self.clock = now;
    }

    /// Encoder clock in ms, offset so that 0 (a never-changed tile) reads
    /// as "settled long ago".
    pub(super) fn now_ms(&self) -> u32 {
        const OFFSET_MS: u128 = 60_000;
        let now = self.clock.unwrap_or_else(Instant::now);
        (now.saturating_duration_since(self.epoch).as_millis() + OFFSET_MS)
            .min(u128::from(u32::MAX - 1)) as u32
    }

    /// Lossy images for this viewer?
    pub(super) fn lossy_active(&self) -> bool {
        let need = crate::terminal::CAP_LOSSY | CAP_BANDS;
        self.lossy_on && self.peer_caps & need == need
    }

    /// Tile cache for this viewer?
    pub(super) fn cache_active(&self) -> bool {
        let need = crate::terminal::CAP_CACHE | CAP_BANDS;
        self.cache_on && self.peer_caps & need == need
    }

    /// Shape `changed_ms` for a `w x h` frame (a new shape: everything
    /// "changed now").
    pub(super) fn ensure_change_clock(&mut self, w: u32, h: u32, now: u32) {
        let n = (w.div_ceil(crate::tiles::TILE) * h.div_ceil(crate::tiles::TILE)) as usize;
        if self.changed_ms.len() != n {
            self.changed_ms.clear();
            self.changed_ms.resize(n, now);
        }
    }

    /// Tile `i` changed less than [`REFINE_DELAY`] ago?
    pub(super) fn recent(&self, i: usize, now: u32) -> bool {
        self.changed_ms
            .get(i)
            .is_some_and(|&t| now.saturating_sub(t) < REFINE_DELAY.as_millis() as u32)
    }

    /// A lossy / video-painted tile has settled and waits for its
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

    /// Refinement lane: when the previous update was taken (the
    /// link is not backed up) and a lossy tile has settled, queue an
    /// update carrying only a refinement chunk (exact pixels of the last
    /// capture, at most `REFRESH_CHUNK_BYTES` / `REFINE_SLICE` of link
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

    /// The link pacer's delivery-rate estimate (bytes/s, 0 = none
    /// yet); see `LZ4_MIN_LINK_RATE`.
    pub fn set_link_rate(&mut self, bytes_per_sec: u64) {
        self.link_rate = bytes_per_sec;
    }

    /// Owe `r` a lossless refresh, sent in low-priority chunks of
    /// about `REFRESH_CHUNK_BYTES` over the following submits (each also
    /// carrying whatever changed). Ignored before the first frame.
    ///
    /// This is the refinement lane — the region's tiles join the
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

    /// Tiles still waiting for their lossless refresh.
    pub fn refresh_pending(&self) -> usize {
        self.backlog.count()
    }

    /// This update's refresh chunk (the first tiles of the backlog, in
    /// raster order) into `self.chunk`.
    pub(super) fn take_chunk(&mut self, w: u32, h: u32, now: u32) {
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
        // At most REFINE_SLICE of link time per chunk.
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
                // Only settled tiles (a tile still changing is
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

    /// Counters (moves, band codecs).
    pub fn stats(&self) -> EncoderStats {
        self.stats
    }
}
