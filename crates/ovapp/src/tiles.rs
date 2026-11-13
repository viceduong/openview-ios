//! Phase 3 "send less data": dirty-tile screen updates.
//!
//! The host diffs each capture against the last frame it actually handed to
//! the wire in [`TILE`]x[`TILE`] tiles and streams only the dirty ones as one
//! update (`TILES_BEGIN` + `FRAME_DATA`… + `FRAME_END`, see
//! [`crate::terminal`]). The payload of an update is the dirty rects' pixel
//! rows concatenated in rect order and compressed **once** for the whole
//! update (not per tile), so the compressor sees all the context.
//!
//! The viewer keeps a persistent framebuffer ([`ScreenDecoder`]) and patches
//! it. Every delta names the frame it was diffed against (`base_seq`); a
//! delta whose base is not what the viewer holds (a lost or corrupt update)
//! is rejected with [`ApplyError::Desync`] and the viewer asks for a
//! keyframe (`KEYFRAME_REQUEST`).
//!
//! Everything here is pure (no OS, no I/O) and unit-tested.

use crate::codec::Decompressor;
use crate::screen::{Frame, PixelFormat};
use crate::terminal::FrameCodec;

/// Tile edge in pixels.
pub const TILE: u32 = 64;
/// More than this fraction of tiles dirty (numerator / denominator) → send a
/// full frame instead (a full frame compresses better than ~all tiles and
/// needs no rect list).
pub const FULL_FRAME_NUM: usize = 6;
/// See [`FULL_FRAME_NUM`].
pub const FULL_FRAME_DEN: usize = 10;
/// Most rects one `TILES_BEGIN` header carries (8 bytes each, so the header
/// stays far below one record). More → full frame.
pub const MAX_TILE_RECTS: usize = 4096;
/// Largest frame edge a tile update can describe (rects are u16 on the wire).
pub const MAX_TILE_FRAME_DIM: u32 = u16::MAX as u32;

/// A pixel rect inside a frame (also the wire form of one dirty region).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TileRect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl TileRect {
    /// Pixel count.
    pub fn area(&self) -> usize {
        usize::from(self.w) * usize::from(self.h)
    }

    /// True when the rect is non-empty and lies inside a `fw x fh` frame.
    pub fn fits(&self, fw: u32, fh: u32) -> bool {
        self.w > 0
            && self.h > 0
            && u32::from(self.x) + u32::from(self.w) <= fw
            && u32::from(self.y) + u32::from(self.h) <= fh
    }
}

/// One dirty flag per tile of a `w x h` frame.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TileMap {
    cols: u32,
    rows: u32,
    bits: Vec<bool>,
    count: usize,
}

impl TileMap {
    /// An all-clean map for a `w x h` frame.
    pub fn new(w: u32, h: u32) -> Self {
        let mut m = Self::default();
        m.reset(w, h);
        m
    }

    /// Re-shape for a `w x h` frame and clear.
    pub fn reset(&mut self, w: u32, h: u32) {
        self.cols = w.div_ceil(TILE);
        self.rows = h.div_ceil(TILE);
        self.bits.clear();
        self.bits.resize((self.cols * self.rows) as usize, false);
        self.count = 0;
    }

    pub fn cols(&self) -> u32 {
        self.cols
    }

    pub fn rows(&self) -> u32 {
        self.rows
    }

    /// Total tiles.
    pub fn len(&self) -> usize {
        self.bits.len()
    }

    /// No tiles at all (zero-sized frame).
    pub fn is_empty(&self) -> bool {
        self.bits.is_empty()
    }

    /// Dirty tiles.
    pub fn count(&self) -> usize {
        self.count
    }

    pub fn get(&self, col: u32, row: u32) -> bool {
        self.bits[(row * self.cols + col) as usize]
    }

    pub fn set(&mut self, col: u32, row: u32) {
        let i = (row * self.cols + col) as usize;
        if !self.bits[i] {
            self.bits[i] = true;
            self.count += 1;
        }
    }

    /// Mark one tile clean.
    pub fn unset(&mut self, col: u32, row: u32) {
        let i = (row * self.cols + col) as usize;
        if self.bits[i] {
            self.bits[i] = false;
            self.count -= 1;
        }
    }

    /// Mark everything clean.
    pub fn clear(&mut self) {
        self.bits.iter_mut().for_each(|b| *b = false);
        self.count = 0;
    }

    /// Mark everything dirty.
    pub fn fill(&mut self) {
        self.bits.iter_mut().for_each(|b| *b = true);
        self.count = self.bits.len();
    }

    /// `self |= other` (same shape; a shape mismatch marks everything).
    pub fn union_with(&mut self, other: &TileMap) {
        if other.cols != self.cols || other.rows != self.rows {
            self.fill();
            return;
        }
        for (a, b) in self.bits.iter_mut().zip(&other.bits) {
            if *b && !*a {
                *a = true;
                self.count += 1;
            }
        }
    }

    /// `self = other` without reallocating.
    pub fn copy_from(&mut self, other: &TileMap) {
        self.cols = other.cols;
        self.rows = other.rows;
        self.bits.clear();
        self.bits.extend_from_slice(&other.bits);
        self.count = other.count;
    }

    /// Mark every tile a pixel rect touches (clipped to the map).
    pub fn mark_rect(&mut self, r: &TileRect) {
        if r.w == 0 || r.h == 0 || self.bits.is_empty() {
            return;
        }
        let c0 = u32::from(r.x) / TILE;
        let r0 = u32::from(r.y) / TILE;
        let c1 = ((u32::from(r.x) + u32::from(r.w) - 1) / TILE).min(self.cols - 1);
        let r1 = ((u32::from(r.y) + u32::from(r.h) - 1) / TILE).min(self.rows - 1);
        for row in r0..=r1.min(self.rows.saturating_sub(1)) {
            for col in c0..=c1 {
                if col < self.cols {
                    self.set(col, row);
                }
            }
        }
    }

    /// `self &= !other` (same shape; a shape mismatch leaves `self` as is).
    pub fn subtract(&mut self, other: &TileMap) {
        if other.cols != self.cols || other.rows != self.rows {
            return;
        }
        for (a, b) in self.bits.iter_mut().zip(&other.bits) {
            if *b && *a {
                *a = false;
                self.count -= 1;
            }
        }
    }

    /// More than [`FULL_FRAME_NUM`]/[`FULL_FRAME_DEN`] of the tiles dirty?
    pub fn mostly_dirty(&self) -> bool {
        self.count * FULL_FRAME_DEN > self.len() * FULL_FRAME_NUM
    }
}

/// Mark the tiles where `cur` differs from `prev` (both `stride * h` bytes of
/// a `w x h` BGRA-like frame with `bpp` bytes per pixel) in `map` (reset to
/// the frame's shape first). Rows are compared whole first (one memcmp per
/// row — the fast path for an unchanged band), then per tile for rows that
/// differ, skipping tiles already known dirty.
///
/// `hints` (e.g. the OS's dirty + move rects) restricts the compare to the
/// tiles they touch; tiles outside every hint are assumed unchanged. `None`
/// compares every tile. Returns the dirty-tile count.
#[allow(clippy::too_many_arguments)]
pub fn diff_tiles(
    prev: &[u8],
    cur: &[u8],
    w: u32,
    h: u32,
    stride: usize,
    bpp: usize,
    hints: Option<&[TileRect]>,
    map: &mut TileMap,
) -> usize {
    map.reset(w, h);
    if w == 0 || h == 0 {
        return 0;
    }
    let need = stride * h as usize;
    if prev.len() < need || cur.len() < need {
        map.fill();
        return map.count();
    }
    // Candidate tiles: everything, or just what the hints touch.
    let mut cand = TileMap::new(w, h);
    match hints {
        None => cand.fill(),
        Some(rs) => rs.iter().for_each(|r| cand.mark_rect(r)),
    }
    if cand.count() == 0 {
        return 0;
    }
    let row_bytes = w as usize * bpp;
    let tile_bytes = TILE as usize * bpp;
    for trow in 0..map.rows() {
        // Any candidate in this band?
        let mut open = (0..map.cols()).filter(|&c| cand.get(c, trow)).count();
        if open == 0 {
            continue;
        }
        let y0 = trow * TILE;
        let y1 = (y0 + TILE).min(h);
        for y in y0..y1 {
            let off = y as usize * stride;
            let a = &prev[off..off + row_bytes];
            let b = &cur[off..off + row_bytes];
            if a == b {
                continue;
            }
            for col in 0..map.cols() {
                if !cand.get(col, trow) || map.get(col, trow) {
                    continue;
                }
                let s = col as usize * tile_bytes;
                let e = (s + tile_bytes).min(row_bytes);
                if a[s..e] != b[s..e] {
                    map.set(col, trow);
                    open -= 1;
                }
            }
            if open == 0 {
                break; // every candidate in the band is already dirty
            }
        }
    }
    map.count()
}

/// Turn a dirty-tile map into pixel rects (clipped to `w x h`): horizontal
/// runs of dirty tiles become one rect, and a run with the same column span
/// as the run directly above it extends that rect downward. Cuts the header
/// size (a 40 px scroll dirtying most of the screen is a handful of rects).
pub fn rects_from_map(map: &TileMap, w: u32, h: u32, out: &mut Vec<TileRect>) {
    out.clear();
    // (c0, c1) col span of each rect still open from the previous tile row,
    // with its index in `out`.
    let mut open: Vec<(u32, u32, usize)> = Vec::new();
    let mut next_open: Vec<(u32, u32, usize)> = Vec::new();
    for row in 0..map.rows() {
        next_open.clear();
        let y0 = row * TILE;
        let y1 = (y0 + TILE).min(h);
        let mut col = 0;
        while col < map.cols() {
            if !map.get(col, row) {
                col += 1;
                continue;
            }
            let c0 = col;
            while col < map.cols() && map.get(col, row) {
                col += 1;
            }
            let c1 = col; // exclusive
            if let Some(&(_, _, idx)) = open.iter().find(|&&(a, b, _)| a == c0 && b == c1) {
                out[idx].h += (y1 - y0) as u16;
                next_open.push((c0, c1, idx));
            } else {
                let x0 = c0 * TILE;
                let x1 = (c1 * TILE).min(w);
                out.push(TileRect {
                    x: x0 as u16,
                    y: y0 as u16,
                    w: (x1 - x0) as u16,
                    h: (y1 - y0) as u16,
                });
                next_open.push((c0, c1, out.len() - 1));
            }
        }
        std::mem::swap(&mut open, &mut next_open);
    }
}

/// Uncompressed payload bytes for `rects` at `bpp` bytes per pixel.
pub fn payload_len(rects: &[TileRect], bpp: usize) -> usize {
    rects.iter().map(|r| r.area() * bpp).sum()
}

/// Append the pixel rows of each rect (rect order, top to bottom) to `out`.
pub fn gather(px: &[u8], stride: usize, bpp: usize, rects: &[TileRect], out: &mut Vec<u8>) {
    out.reserve(payload_len(rects, bpp));
    for r in rects {
        let xb = usize::from(r.x) * bpp;
        let wb = usize::from(r.w) * bpp;
        for y in usize::from(r.y)..usize::from(r.y) + usize::from(r.h) {
            let off = y * stride + xb;
            out.extend_from_slice(&px[off..off + wb]);
        }
    }
}

/// Copy the pixel rows of each rect from `src` into `dst` (same geometry).
pub fn copy_rects(dst: &mut [u8], src: &[u8], stride: usize, bpp: usize, rects: &[TileRect]) {
    for r in rects {
        let xb = usize::from(r.x) * bpp;
        let wb = usize::from(r.w) * bpp;
        for y in usize::from(r.y)..usize::from(r.y) + usize::from(r.h) {
            let off = y * stride + xb;
            dst[off..off + wb].copy_from_slice(&src[off..off + wb]);
        }
    }
}

/// Inverse of [`gather`]: write `data` (exactly [`payload_len`] bytes) into
/// the rects of `fb`. Validates sizes/bounds first, so a bad update never
/// tears the framebuffer.
pub fn scatter(
    fb: &mut [u8],
    fw: u32,
    fh: u32,
    stride: usize,
    bpp: usize,
    rects: &[TileRect],
    data: &[u8],
) -> Result<(), String> {
    if data.len() != payload_len(rects, bpp) {
        return Err(format!(
            "tile payload is {} bytes, rects need {}",
            data.len(),
            payload_len(rects, bpp)
        ));
    }
    if fb.len() < stride * fh as usize || stride < fw as usize * bpp {
        return Err("framebuffer smaller than its geometry".into());
    }
    if let Some(bad) = rects.iter().find(|r| !r.fits(fw, fh)) {
        return Err(format!("tile rect {bad:?} outside {fw}x{fh}"));
    }
    let mut at = 0;
    for r in rects {
        let xb = usize::from(r.x) * bpp;
        let wb = usize::from(r.w) * bpp;
        for y in usize::from(r.y)..usize::from(r.y) + usize::from(r.h) {
            let off = y * stride + xb;
            fb[off..off + wb].copy_from_slice(&data[at..at + wb]);
            at += wb;
        }
    }
    Ok(())
}

/// The delta part of an update header: what it was diffed against and
/// which rects it carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delta {
    pub base_seq: u64,
    pub rects: Vec<TileRect>,
}

/// Everything needed to decode one screen update once its `FRAME_DATA`
/// stream is complete (from a `FRAME_BEGIN` or a `TILES_BEGIN`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateMeta {
    pub width: u32,
    pub height: u32,
    /// Row stride of the frame (full frames: as declared; deltas: tight).
    pub stride: u32,
    pub format: PixelFormat,
    /// Uncompressed payload bytes.
    pub total: u32,
    pub codec: FrameCodec,
    /// Wire bytes the `FRAME_DATA` chunks carry.
    pub compressed_len: u32,
    /// `None` = a full frame.
    pub delta: Option<Delta>,
}

impl UpdateMeta {
    pub fn is_full(&self) -> bool {
        self.delta.is_none()
    }
}

/// Why an update could not be applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyError {
    /// A delta whose base is not the framebuffer we hold (an update was
    /// lost, or no keyframe yet): the viewer needs a keyframe.
    Desync,
    /// Corrupt payload / impossible geometry (also needs a keyframe).
    Corrupt(String),
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Desync => f.write_str("delta base does not match the framebuffer"),
            Self::Corrupt(e) => f.write_str(e),
        }
    }
}

/// Viewer-side persistent framebuffer: applies full frames and tile deltas
/// in order. Shared by the desktop [`crate::terminal::TerminalSession`] and
/// the iOS FFI assembler, so both patch identically.
#[derive(Default)]
pub struct ScreenDecoder {
    fb: Option<Frame>,
    scratch: Vec<u8>,
    dec: Decompressor,
    /// Rects the last successful apply changed (a full frame = one rect).
    dirty: Vec<TileRect>,
    /// Wave 2: host-assigned tile cache slots (`CAP_CACHE`).
    slots: crate::lossy::SlotStore,
    /// Wave 2: tiles currently showing lossy pixels (JPEG atlases, video
    /// paints) — informational; the host drives refinement.
    lossy: TileMap,
    /// Wave 2: decoded atlases of the update being applied (scratch).
    atlas_bufs: Vec<Vec<u8>>,
    /// Wave 2: scratch map for the REF / lossy dirty rects.
    extra_map: TileMap,
}

impl ScreenDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// The current framebuffer (None until the first full frame).
    pub fn frame(&self) -> Option<&Frame> {
        self.fb.as_ref()
    }

    /// Sequence number of the framebuffer.
    pub fn seq(&self) -> Option<u64> {
        self.fb.as_ref().map(|f| f.seq)
    }

    /// Rects changed by the last successful [`apply`](Self::apply) (Wave 1:
    /// MOVE destination rects first, then the tile rects).
    pub fn last_dirty(&self) -> &[TileRect] {
        &self.dirty
    }

    /// Phase 5: paint a decoded video picture (tight BGRA, `rect.w x
    /// rect.h`) into the framebuffer at `rect`, without touching its seq
    /// (tile deltas keep chaining). Refused (false) when there is no
    /// framebuffer, it is not `screen_w x screen_h` BGRA, or the sizes do
    /// not match.
    pub fn blit(&mut self, rect: TileRect, screen_w: u32, screen_h: u32, bgra: &[u8]) -> bool {
        let Some(fb) = self.fb.as_mut() else {
            return false;
        };
        if fb.width != screen_w
            || fb.height != screen_h
            || fb.format.bytes_per_pixel() != 4
            || !rect.fits(fb.width, fb.height)
            || bgra.len() != rect.area() * 4
        {
            return false;
        }
        let stride = fb.stride as usize;
        let xb = usize::from(rect.x) * 4;
        let wb = usize::from(rect.w) * 4;
        for (i, row) in bgra.chunks_exact(wb).enumerate() {
            let off = (usize::from(rect.y) + i) * stride + xb;
            fb.pixels[off..off + wb].copy_from_slice(row);
        }
        self.dirty.clear();
        self.dirty.push(rect);
        // Wave 2: video pixels are lossy (the host refines them later).
        if self.lossy.cols() != screen_w.div_ceil(TILE)
            || self.lossy.rows() != screen_h.div_ceil(TILE)
        {
            self.lossy.reset(screen_w, screen_h);
        }
        self.lossy.mark_rect(&rect);
        true
    }

    /// Drop the framebuffer (the next delta is a desync).
    pub fn reset(&mut self) {
        self.fb = None;
        self.dirty.clear();
        self.slots.clear();
        self.lossy.clear();
    }

    /// Wave 2: tiles showing lossy pixels (JPEG atlases or video) that no
    /// exact update has replaced yet.
    pub fn lossy_tiles(&self) -> &TileMap {
        &self.lossy
    }

    /// Wave 2: tile cache slots currently filled.
    pub fn cache_slots_filled(&self) -> usize {
        self.slots.filled()
    }

    /// Wave 2: content hash of cache slot `s` (`None` = empty).
    pub fn cache_slot_hash(&self, s: u16) -> Option<u128> {
        self.slots.slot_hash(s)
    }

    /// Wave 2: a [`FrameCodec::Packed`] update (full or delta) — MOVEs,
    /// bands, cache REFs, lossy atlases, cache STOREs, in that order (see
    /// [`crate::bands`]). Everything is validated and decoded before the
    /// framebuffer changes.
    fn apply_packed(&mut self, meta: &UpdateMeta, seq: u64, data: &[u8]) -> Result<(), ApplyError> {
        use crate::lossy::MAX_SLOTS;
        let corrupt = |s: String| ApplyError::Corrupt(s);
        let bpp = meta.format.bytes_per_pixel() as usize;
        let total = meta.total as usize;
        let p = crate::bands::parse(data).map_err(corrupt)?;
        if p.raw_len() != total {
            return Err(corrupt(format!(
                "bands decode to {} bytes, declared {total}",
                p.raw_len()
            )));
        }
        if p.has_extras() && bpp != 4 {
            return Err(corrupt(
                "cache / lossy records need a 4-byte pixel format".into(),
            ));
        }
        let (fw, fh) = (meta.width, meta.height);
        match &meta.delta {
            None => {
                if (meta.stride as usize) < fw as usize * bpp
                    || total != meta.stride as usize * fh as usize
                {
                    return Err(corrupt(format!(
                        "full frame {fw}x{fh} stride {} total {total} inconsistent",
                        meta.stride
                    )));
                }
                if !p.moves.is_empty() || !p.refs.is_empty() {
                    return Err(corrupt("full frame with MOVE / cache REF records".into()));
                }
            }
            Some(delta) => {
                let Some(fb) = self.fb.as_ref() else {
                    return Err(ApplyError::Desync);
                };
                if fb.seq != delta.base_seq
                    || fb.width != fw
                    || fb.height != fh
                    || fb.format != meta.format
                {
                    return Err(ApplyError::Desync);
                }
                if total != payload_len(&delta.rects, bpp) {
                    return Err(corrupt(format!(
                        "tile update declares {total} bytes, rects need {}",
                        payload_len(&delta.rects, bpp)
                    )));
                }
                if let Some(bad) = p.moves.iter().find(|m| !m.fits(fw, fh)) {
                    return Err(corrupt(format!("MOVE {bad:?} outside {fw}x{fh}")));
                }
                if let Some(bad) = delta.rects.iter().find(|r| !r.fits(fw, fh)) {
                    return Err(corrupt(format!("tile rect {bad:?} outside {fw}x{fh}")));
                }
                if (fb.stride as usize) < fw as usize * bpp
                    || fb.pixels.len() < fb.stride as usize * fh as usize
                {
                    return Err(corrupt("framebuffer smaller than its geometry".into()));
                }
            }
        }
        // Cache records: tiles inside the frame, valid slots; a REF must
        // name a filled slot holding a tile of the destination's size
        // (clipped edge tiles are cached too).
        let (cols, rows) = (fw.div_ceil(TILE), fh.div_ceil(TILE));
        for op in p.refs.iter().chain(&p.stores) {
            if usize::from(op.slot) >= MAX_SLOTS
                || u32::from(op.col) >= cols
                || u32::from(op.row) >= rows
            {
                return Err(corrupt(format!(
                    "cache record {op:?} invalid for {fw}x{fh}"
                )));
            }
        }
        for op in &p.refs {
            let tr = crate::lossy::tile_rect(u32::from(op.col), u32::from(op.row), fw, fh);
            if self.slots.dims(op.slot) != Some((usize::from(tr.w), usize::from(tr.h))) {
                return Err(corrupt(format!(
                    "cache REF {op:?}: slot empty or of another size"
                )));
            }
        }
        for im in &p.images {
            if let Some(&(c, r)) = im
                .tiles
                .iter()
                .find(|&&(c, r)| u32::from(c) >= cols || u32::from(r) >= rows)
            {
                return Err(corrupt(format!("lossy tile ({c},{r}) outside {fw}x{fh}")));
            }
        }
        // Decode bands and atlases (both in parallel) before touching the
        // framebuffer.
        crate::bands::decode_bands(&p.bands, &mut self.scratch).map_err(corrupt)?;
        if self.atlas_bufs.len() < p.images.len() {
            self.atlas_bufs.resize_with(p.images.len(), Vec::new);
        }
        {
            use rayon::prelude::*;
            let bufs = &mut self.atlas_bufs[..p.images.len()];
            if p.images.len() > 1 {
                p.images
                    .par_iter()
                    .zip(bufs.par_iter_mut())
                    .try_for_each(|(im, b)| crate::lossy::decode_atlas(im, b))
                    .map_err(corrupt)?;
            } else {
                for (im, b) in p.images.iter().zip(bufs.iter_mut()) {
                    crate::lossy::decode_atlas(im, b).map_err(corrupt)?;
                }
            }
        }
        // Apply.
        if self.lossy.cols() != cols || self.lossy.rows() != rows {
            self.lossy.reset(fw, fh);
        }
        match &meta.delta {
            None => {
                let px = std::mem::take(&mut self.scratch);
                let old = self.fb.replace(Frame {
                    width: fw,
                    height: fh,
                    stride: meta.stride,
                    format: meta.format,
                    seq,
                    pixels: px,
                });
                if let Some(old) = old {
                    self.scratch = old.pixels;
                }
                self.slots.clear();
                self.lossy.clear();
            }
            Some(delta) => {
                let fb = self.fb.as_mut().expect("checked above");
                let stride = fb.stride as usize;
                for m in &p.moves {
                    crate::bands::apply_move(&mut fb.pixels, fw, fh, stride, bpp, m)
                        .map_err(corrupt)?;
                    // Lossy pixels travel with the move.
                    let src = TileRect {
                        x: (i32::from(m.x) - i32::from(m.dx)) as u16,
                        y: (i32::from(m.y) - i32::from(m.dy)) as u16,
                        w: m.w,
                        h: m.h,
                    };
                    if map_touches(&self.lossy, &src) {
                        self.lossy.mark_rect(&m.dst());
                    }
                }
                scatter(
                    &mut fb.pixels,
                    fw,
                    fh,
                    stride,
                    bpp,
                    &delta.rects,
                    &self.scratch,
                )
                .map_err(corrupt)?;
                for r in &delta.rects {
                    unmark_covered(&mut self.lossy, r, fw, fh);
                }
                fb.seq = seq;
            }
        }
        let fb = self.fb.as_mut().expect("framebuffer present");
        let stride = fb.stride as usize;
        self.extra_map.reset(fw, fh);
        for op in &p.refs {
            let tr = crate::lossy::tile_rect(u32::from(op.col), u32::from(op.row), fw, fh);
            let (x0, y0, tw, th) = (
                usize::from(tr.x),
                usize::from(tr.y),
                usize::from(tr.w),
                usize::from(tr.h),
            );
            self.slots
                .load(op.slot, &mut fb.pixels, stride, x0, y0, tw, th);
            self.extra_map.set(u32::from(op.col), u32::from(op.row));
            unmark_tile(&mut self.lossy, u32::from(op.col), u32::from(op.row));
        }
        for (im, buf) in p.images.iter().zip(&self.atlas_bufs) {
            crate::lossy::paint_atlas(im, buf, &mut fb.pixels, fw, fh, stride);
            for &(c, r) in &im.tiles {
                self.extra_map.set(u32::from(c), u32::from(r));
                self.lossy.set(u32::from(c), u32::from(r));
            }
        }
        for op in &p.stores {
            let tr = crate::lossy::tile_rect(u32::from(op.col), u32::from(op.row), fw, fh);
            let (x0, y0, tw, th) = (
                usize::from(tr.x),
                usize::from(tr.y),
                usize::from(tr.w),
                usize::from(tr.h),
            );
            self.slots
                .store(op.slot, &fb.pixels, stride, x0, y0, tw, th);
        }
        self.dirty.clear();
        match &meta.delta {
            None => self.dirty.push(TileRect {
                x: 0,
                y: 0,
                w: fw.min(MAX_TILE_FRAME_DIM) as u16,
                h: fh.min(MAX_TILE_FRAME_DIM) as u16,
            }),
            Some(delta) => {
                self.dirty.extend(p.moves.iter().map(|m| m.dst()));
                self.dirty.extend_from_slice(&delta.rects);
                if self.extra_map.count() > 0 {
                    let mut extra = Vec::new();
                    rects_from_map(&self.extra_map, fw, fh, &mut extra);
                    self.dirty.extend(extra);
                }
            }
        }
        Ok(())
    }

    /// Apply one complete update (`data` = exactly `meta.compressed_len`
    /// wire bytes). On error the framebuffer is left exactly as it was.
    pub fn apply(&mut self, meta: &UpdateMeta, seq: u64, data: &[u8]) -> Result<(), ApplyError> {
        if meta.codec == FrameCodec::Packed {
            return self.apply_packed(meta, seq, data);
        }
        let bpp = meta.format.bytes_per_pixel() as usize;
        match &meta.delta {
            None => {
                let total = meta.total as usize;
                if (meta.stride as usize) < meta.width as usize * bpp
                    || total != meta.stride as usize * meta.height as usize
                {
                    return Err(ApplyError::Corrupt(format!(
                        "full frame {}x{} stride {} total {total} inconsistent",
                        meta.width, meta.height, meta.stride
                    )));
                }
                self.dec
                    .decompress_into(meta.codec, data, total, &mut self.scratch)
                    .map_err(ApplyError::Corrupt)?;
                // Swap the decoded buffer in (no copy); the old pixels become
                // the next scratch.
                let px = std::mem::take(&mut self.scratch);
                let old = self.fb.replace(Frame {
                    width: meta.width,
                    height: meta.height,
                    stride: meta.stride,
                    format: meta.format,
                    seq,
                    pixels: px,
                });
                if let Some(old) = old {
                    self.scratch = old.pixels;
                }
                self.slots.clear();
                self.lossy.reset(meta.width, meta.height);
                self.dirty.clear();
                self.dirty.push(TileRect {
                    x: 0,
                    y: 0,
                    w: meta.width.min(MAX_TILE_FRAME_DIM) as u16,
                    h: meta.height.min(MAX_TILE_FRAME_DIM) as u16,
                });
                Ok(())
            }
            Some(delta) => {
                let Some(fb) = self.fb.as_mut() else {
                    return Err(ApplyError::Desync);
                };
                if fb.seq != delta.base_seq
                    || fb.width != meta.width
                    || fb.height != meta.height
                    || fb.format != meta.format
                {
                    return Err(ApplyError::Desync);
                }
                let total = meta.total as usize;
                if total != payload_len(&delta.rects, bpp) {
                    return Err(ApplyError::Corrupt(format!(
                        "tile update declares {total} bytes, rects need {}",
                        payload_len(&delta.rects, bpp)
                    )));
                }
                // (Band containers — MOVEs, Wave 2 sections — take
                // `apply_packed`.)
                self.dec
                    .decompress_into(meta.codec, data, total, &mut self.scratch)
                    .map_err(ApplyError::Corrupt)?;
                scatter(
                    &mut fb.pixels,
                    fb.width,
                    fb.height,
                    fb.stride as usize,
                    bpp,
                    &delta.rects,
                    &self.scratch,
                )
                .map_err(ApplyError::Corrupt)?;
                fb.seq = seq;
                let (fw, fh) = (fb.width, fb.height);
                for r in &delta.rects {
                    unmark_covered(&mut self.lossy, r, fw, fh);
                }
                self.dirty.clear();
                self.dirty.extend_from_slice(&delta.rects);
                Ok(())
            }
        }
    }
}

/// Does `r` touch any tile set in `map`?
pub(crate) fn map_touches(map: &TileMap, r: &TileRect) -> bool {
    if map.count() == 0 || r.w == 0 || r.h == 0 {
        return false;
    }
    let c0 = u32::from(r.x) / TILE;
    let r0 = u32::from(r.y) / TILE;
    let c1 = ((u32::from(r.x) + u32::from(r.w) - 1) / TILE).min(map.cols().saturating_sub(1));
    let r1 = ((u32::from(r.y) + u32::from(r.h) - 1) / TILE).min(map.rows().saturating_sub(1));
    (r0..=r1)
        .any(|row| (c0..=c1).any(|col| col < map.cols() && row < map.rows() && map.get(col, row)))
}

/// Clear one tile of `map` (no-op outside it).
fn unmark_tile(map: &mut TileMap, col: u32, row: u32) {
    if col < map.cols() && row < map.rows() {
        map.unset(col, row);
    }
}

/// Clear the tiles of `map` that `r` covers completely (edge tiles count
/// as covered when `r` reaches the frame edge).
fn unmark_covered(map: &mut TileMap, r: &TileRect, fw: u32, fh: u32) {
    if map.count() == 0 || r.w == 0 || r.h == 0 {
        return;
    }
    let (x0, y0) = (u32::from(r.x), u32::from(r.y));
    let (x1, y1) = (x0 + u32::from(r.w), y0 + u32::from(r.h));
    for row in y0.div_ceil(TILE)..map.rows() {
        let (ty0, ty1) = (row * TILE, ((row + 1) * TILE).min(fh));
        if ty0 >= y1 {
            break;
        }
        if ty1 > y1 {
            continue;
        }
        for col in x0.div_ceil(TILE)..map.cols() {
            let (tx0, tx1) = (col * TILE, ((col + 1) * TILE).min(fw));
            if tx0 >= x1 {
                break;
            }
            if tx1 <= x1 {
                unmark_tile(map, col, row);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tiny deterministic PRNG (xorshift) — no dev-dependency needed.
    pub(crate) struct Rng(u64);
    impl Rng {
        pub(crate) fn new(seed: u64) -> Self {
            Self(seed.max(1))
        }
        pub(crate) fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        pub(crate) fn below(&mut self, n: u32) -> u32 {
            (self.next() % u64::from(n.max(1))) as u32
        }
    }

    fn frame(w: u32, h: u32, seed: u64) -> Vec<u8> {
        let mut r = Rng::new(seed);
        (0..w * h * 4).map(|_| r.next() as u8).collect()
    }

    /// Brute-force reference: tile is dirty iff any pixel in it differs.
    fn reference(prev: &[u8], cur: &[u8], w: u32, h: u32) -> TileMap {
        let mut m = TileMap::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                if prev[i..i + 4] != cur[i..i + 4] {
                    m.set(x / TILE, y / TILE);
                }
            }
        }
        m
    }

    #[test]
    fn diff_matches_brute_force_on_random_edits() {
        let (w, h) = (300, 200); // partial edge tiles on both axes
        let mut rng = Rng::new(42);
        let prev = frame(w, h, 7);
        for _ in 0..50 {
            let mut cur = prev.clone();
            for _ in 0..rng.below(12) {
                let x = rng.below(w);
                let y = rng.below(h);
                let i = ((y * w + x) * 4 + rng.below(4)) as usize;
                cur[i] = cur[i].wrapping_add(1 + rng.below(250) as u8);
            }
            let mut m = TileMap::default();
            let n = diff_tiles(&prev, &cur, w, h, (w * 4) as usize, 4, None, &mut m);
            let want = reference(&prev, &cur, w, h);
            assert_eq!(m, want);
            assert_eq!(n, want.count());
        }
    }

    #[test]
    fn identical_frames_have_no_dirty_tiles() {
        let px = frame(130, 70, 3);
        let mut m = TileMap::default();
        assert_eq!(
            diff_tiles(&px, &px.clone(), 130, 70, 520, 4, None, &mut m),
            0
        );
    }

    #[test]
    fn hints_restrict_the_compare() {
        let (w, h) = (256, 128);
        let prev = frame(w, h, 1);
        let mut cur = prev.clone();
        cur[0] ^= 0xFF; // tile (0,0)
        let far = ((100 * w + 200) * 4) as usize;
        cur[far] ^= 0xFF; // tile (3,1)
        let mut m = TileMap::default();
        // Hint only covers the top-left tile: the other change is (by
        // contract) assumed not to exist.
        let hint = [TileRect {
            x: 0,
            y: 0,
            w: 10,
            h: 10,
        }];
        assert_eq!(
            diff_tiles(&prev, &cur, w, h, (w * 4) as usize, 4, Some(&hint), &mut m),
            1
        );
        assert!(m.get(0, 0));
        // A hint over an unchanged area finds nothing (OS rects are verified).
        let hint = [TileRect {
            x: 64,
            y: 0,
            w: 64,
            h: 64,
        }];
        assert_eq!(
            diff_tiles(&prev, &cur, w, h, (w * 4) as usize, 4, Some(&hint), &mut m),
            0
        );
        // Empty hint list = nothing changed.
        assert_eq!(
            diff_tiles(&prev, &cur, w, h, (w * 4) as usize, 4, Some(&[]), &mut m),
            0
        );
    }

    #[test]
    fn rects_cover_exactly_the_dirty_tiles_and_merge_runs() {
        let (w, h) = (300, 200);
        let mut m = TileMap::new(w, h);
        // A 3-wide run on rows 0 and 1 (one merged rect), plus a lone edge
        // tile on the partial last column/row.
        for c in 0..3 {
            m.set(c, 0);
            m.set(c, 1);
        }
        m.set(4, 3);
        let mut rects = Vec::new();
        rects_from_map(&m, w, h, &mut rects);
        assert_eq!(
            rects,
            vec![
                TileRect {
                    x: 0,
                    y: 0,
                    w: 192,
                    h: 128
                },
                TileRect {
                    x: 256,
                    y: 192,
                    w: 44,
                    h: 8
                },
            ]
        );
        // Re-marking the rects reproduces the map.
        let mut back = TileMap::new(w, h);
        rects.iter().for_each(|r| back.mark_rect(r));
        assert_eq!(back, m);
    }

    #[test]
    fn random_maps_round_trip_through_rects() {
        let mut rng = Rng::new(9);
        for _ in 0..100 {
            let (w, h) = (1 + rng.below(700), 1 + rng.below(500));
            let mut m = TileMap::new(w, h);
            for _ in 0..rng.below(40) {
                m.set(rng.below(m.cols()), rng.below(m.rows()));
            }
            let mut rects = Vec::new();
            rects_from_map(&m, w, h, &mut rects);
            let mut back = TileMap::new(w, h);
            rects.iter().for_each(|r| {
                assert!(r.fits(w, h), "{r:?} in {w}x{h}");
                back.mark_rect(r)
            });
            assert_eq!(back, m);
            // Rects never overlap: their areas sum to the dirty pixel area.
            let area: usize = rects.iter().map(TileRect::area).sum();
            let mut px = 0usize;
            for row in 0..m.rows() {
                for col in 0..m.cols() {
                    if m.get(col, row) {
                        let tw = (w - col * TILE).min(TILE) as usize;
                        let th = (h - row * TILE).min(TILE) as usize;
                        px += tw * th;
                    }
                }
            }
            assert_eq!(area, px);
        }
    }

    #[test]
    fn gather_scatter_patches_the_framebuffer() {
        let (w, h) = (200, 150);
        let prev = frame(w, h, 5);
        let mut cur = prev.clone();
        let mut rng = Rng::new(77);
        for _ in 0..30 {
            let i = rng.below(w * h * 4) as usize;
            cur[i] ^= 0x5A;
        }
        let mut m = TileMap::default();
        diff_tiles(&prev, &cur, w, h, (w * 4) as usize, 4, None, &mut m);
        let mut rects = Vec::new();
        rects_from_map(&m, w, h, &mut rects);
        let mut payload = Vec::new();
        gather(&cur, (w * 4) as usize, 4, &rects, &mut payload);
        let mut fb = prev.clone();
        scatter(&mut fb, w, h, (w * 4) as usize, 4, &rects, &payload).unwrap();
        assert_eq!(fb, cur);
        // Bad payload length / out-of-bounds rect: rejected, fb untouched.
        let before = fb.clone();
        assert!(scatter(&mut fb, w, h, (w * 4) as usize, 4, &rects, &payload[1..]).is_err());
        let oob = [TileRect {
            x: 190,
            y: 0,
            w: 20,
            h: 1,
        }];
        assert!(scatter(&mut fb, w, h, (w * 4) as usize, 4, &oob, &[0; 80]).is_err());
        assert_eq!(fb, before);
    }

    #[test]
    fn scroll_dirties_most_tiles() {
        let (w, h) = (640, 480);
        let prev = frame(w, h, 11);
        // Scroll up by 40 px: every row now shows content from 40 rows down.
        let stride = (w * 4) as usize;
        let mut cur = vec![0u8; prev.len()];
        cur[..(h as usize - 40) * stride].copy_from_slice(&prev[40 * stride..]);
        let mut m = TileMap::default();
        diff_tiles(&prev, &cur, w, h, stride, 4, None, &mut m);
        assert!(m.mostly_dirty(), "{} of {}", m.count(), m.len());
        let mut rects = Vec::new();
        rects_from_map(&m, w, h, &mut rects);
        assert_eq!(rects.len(), 1, "whole-screen dirt merges into one rect");
    }

    fn meta_full(w: u32, h: u32, len: usize) -> UpdateMeta {
        UpdateMeta {
            width: w,
            height: h,
            stride: w * 4,
            format: PixelFormat::Bgra,
            total: w * h * 4,
            codec: FrameCodec::Raw,
            compressed_len: len as u32,
            delta: None,
        }
    }

    #[test]
    fn decoder_applies_full_then_deltas_and_rejects_a_broken_chain() {
        let (w, h) = (130, 90);
        let f0 = frame(w, h, 1);
        let mut d = ScreenDecoder::new();
        // A delta before any keyframe is a desync.
        let early = UpdateMeta {
            delta: Some(Delta {
                base_seq: 0,
                rects: vec![],
            }),
            total: 0,
            ..meta_full(w, h, 0)
        };
        assert_eq!(d.apply(&early, 1, &[]), Err(ApplyError::Desync));
        d.apply(&meta_full(w, h, f0.len()), 10, &f0).unwrap();
        assert_eq!(d.frame().unwrap().pixels, f0);
        // Delta 10 -> 11.
        let mut f1 = f0.clone();
        f1[4 * 70] = 1;
        f1[(w * 4 * 80) as usize] = 2;
        let mut m = TileMap::default();
        diff_tiles(&f0, &f1, w, h, (w * 4) as usize, 4, None, &mut m);
        let mut rects = Vec::new();
        rects_from_map(&m, w, h, &mut rects);
        let mut payload = Vec::new();
        gather(&f1, (w * 4) as usize, 4, &rects, &mut payload);
        let dm = UpdateMeta {
            total: payload.len() as u32,
            compressed_len: payload.len() as u32,
            delta: Some(Delta {
                base_seq: 10,
                rects: rects.clone(),
            }),
            ..meta_full(w, h, 0)
        };
        d.apply(&dm, 11, &payload).unwrap();
        assert_eq!(d.frame().unwrap().pixels, f1);
        assert_eq!(d.seq(), Some(11));
        assert_eq!(d.last_dirty(), &rects[..]);
        // The same delta again: its base (10) is no longer what we hold.
        assert_eq!(d.apply(&dm, 12, &payload), Err(ApplyError::Desync));
        // Corrupt length: rejected, framebuffer untouched.
        let bad = UpdateMeta {
            delta: Some(Delta {
                base_seq: 11,
                rects,
            }),
            ..dm.clone()
        };
        assert!(matches!(
            d.apply(&bad, 12, &payload[1..]),
            Err(ApplyError::Corrupt(_))
        ));
        assert_eq!(d.frame().unwrap().pixels, f1);
        assert_eq!(d.seq(), Some(11));
    }
}
