//! Screen payloads: parallel band compression, the
//! [`FrameCodec::Packed`] band container, and copy-rect MOVE records.
//!
//! # Concatenated zstd bands (no new CAPS bit)
//!
//! `compress_zstd_bands` compresses a payload as up to `MAX_BANDS`
//! independent zstd frames (each declaring its content size) on several
//! cores and concatenates them. Every zstd decoder decodes such a stream
//! sequentially, so this is still plain [`FrameCodec::Zstd`]; new viewers
//! split it and decode the frames in parallel ([`crate::zstd_par`]).
//!
//! # The band container ([`FrameCodec::Packed`], `CAP_BANDS`)
//!
//! All integers big-endian:
//!
//! ```text
//! u8  version          = 1
//! u8  flags            bit0 = MOVES (CAP_MOVE), bit1 = CACHE (CAP_CACHE),
//!                      bit2 = LOSSY (CAP_LOSSY); other bits 0
//! [if MOVES]
//!   u16 move_count     1..=MAX_MOVES
//!   move_count x 12 B: u16 x, u16 y, u16 w, u16 h, i16 dx, i16 dy
//! [if CACHE]           (see crate::lossy)
//!   u16 ref_count      0..=65535
//!   ref_count x 6 B:   u16 slot, u16 tile_col, u16 tile_row
//!   u16 store_count    0..=65535
//!   store_count x 6 B: u16 slot, u16 tile_col, u16 tile_row
//! [if LOSSY]
//!   u16 image_count    1..=MAX_IMAGES
//!   image_count x:     u8 kind (0 = baseline JPEG), u16 atlas_cols,
//!                      u16 tile_count (1..=64), tile_count x (u16 col,
//!                      u16 row), u32 jpeg_len, jpeg_len bytes
//! u16 band_count       0..=MAX_BANDS
//! band_count x 9 B:    u8 codec (0 raw, 1 DEFLATE, 2 zstd, 3 LZ4 block),
//!                      u32 raw_len, u32 wire_len
//! band payloads back to back (wire_len bytes each)
//! ```
//!
//! The bands decode independently (in parallel) and concatenate to the
//! update's `total` uncompressed bytes (sum of `raw_len`); the container is
//! exactly `compressed_len` bytes (header + sum of `wire_len`). The host
//! picks LZ4 for low-ratio (photo-like) bands and zstd for the rest.
//!
//! # MOVE records (`CAP_MOVE`, deltas only)
//!
//! A MOVE copies the pixels of the framebuffer rect `(x - dx, y - dy, w,
//! h)` to the destination rect `(x, y, w, h)` (memmove semantics: source
//! and destination may overlap). MOVEs apply in order, each on the result
//! of the previous one, **before** the delta's tile rects are scattered;
//! the destination rects count as changed (they are part of
//! [`crate::tiles::ScreenDecoder::last_dirty`]). A scroll by 24 px of a
//! 1305x595 window becomes one MOVE plus the exposed 1305x24 strip
//! (~1 KB instead of ~78 KB of re-sent tiles).
//!
//! # Cache and lossy sections (`CAP_CACHE`, `CAP_LOSSY`)
//!
//! A viewer applies an update in this order: MOVEs, the bands (scattered
//! into the delta rects, or the whole full frame), cache REFs (slot ->
//! tile), lossy atlases (JPEG -> tiles), cache STOREs (tile -> slot, read
//! from the final framebuffer). REF / STORE / lossy tiles are tile grid
//! cells inside the frame, clipped at its right / bottom edge; a slot
//! remembers the size it was stored with and a REF must land on a tile of
//! that size. In a
//! delta the REF and lossy tiles are not part of the rects (the bands
//! carry only lossless pixels); in a full frame they are blanked in the
//! band payload and painted over. A full frame empties every slot before
//! its STOREs. All sections are validated and decoded before the
//! framebuffer is touched.
//!
//! Pure Rust everywhere (decode runs in the iOS staticlib); the zstd
//! *encoder* is the host-only C `zstd` crate behind `zstd-encode`.

use std::cell::RefCell;

use rayon::prelude::*;

use crate::error::AppError;
use crate::terminal::FrameCodec;
use crate::tiles::TileRect;

/// Container version byte.
pub(crate) const VERSION: u8 = 1;
/// Container flag: MOVE records follow the flags byte.
pub(crate) const FLAG_MOVES: u8 = 1 << 0;
/// Container flag: cache REF + STORE records.
pub(crate) const FLAG_CACHE: u8 = 1 << 1;
/// Container flag: lossy JPEG atlases.
pub(crate) const FLAG_LOSSY: u8 = 1 << 2;
/// Bytes per cache REF / STORE record.
pub(crate) const CACHE_OP_LEN: usize = 6;
/// Most bands one container may carry.
pub(crate) const MAX_BANDS: usize = 64;
/// Most MOVE records one container may carry.
pub(crate) const MAX_MOVES: usize = 256;
/// Bands the host aims for on a large payload (one per core, roughly).
pub(crate) const TARGET_BANDS: usize = 8;
/// Smallest band worth its own frame (smaller payloads use fewer bands).
pub(crate) const MIN_BAND: usize = 96 * 1024;
/// Bytes per MOVE record on the wire.
pub(crate) const MOVE_LEN: usize = 12;
/// Bytes per band table entry.
pub(crate) const BAND_ENTRY_LEN: usize = 9;
/// A band whose LZ4 ratio (raw / compressed, estimated on a sample) stays
/// below this is photo-like: zstd would gain ~15 % at 3-5x the encode and
/// ~20x the (pure-Rust) decode time, so it goes out as LZ4.
pub(crate) const LZ4_MAX_RATIO: f32 = 1.6;
/// Below this payload size parallel decode is not worth the handoffs.
pub(crate) const MIN_PARALLEL_DECODE: usize = 128 * 1024;
/// Hard ceiling on the bytes one container may decode to (sum of
/// `raw_len`): an 8192 x 8192 BGRA frame. A peer cannot make the decoder
/// allocate more than this, whatever the declared geometry; callers that
/// know the frame size bound it tighter with [`parse_bounded`].
pub(crate) const MAX_RAW_TOTAL: usize = 8192 * 8192 * 4;

/// Bytes a `width x height` frame of `bpp`-byte pixels decodes to, if it
/// fits `MAX_RAW_TOTAL` — the natural bound for [`parse_bounded`].
#[must_use]
pub fn max_raw_len(width: u32, height: u32, bpp: usize) -> Option<usize> {
    let n = usize::try_from(width)
        .ok()?
        .checked_mul(usize::try_from(height).ok()?)?
        .checked_mul(bpp)?;
    (n <= MAX_RAW_TOTAL).then_some(n)
}

/// One copy-rect record: destination rect `(x, y, w, h)`, source offset
/// by `(-dx, -dy)` (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MoveRect {
    /// Destination left edge (pixels).
    pub x: u16,
    /// Destination top edge (pixels).
    pub y: u16,
    /// Width (pixels).
    pub w: u16,
    /// Height (pixels).
    pub h: u16,
    /// Horizontal offset: the source is `dx` pixels left of the destination.
    pub dx: i16,
    /// Vertical offset: the source is `dy` pixels above the destination.
    pub dy: i16,
}

impl MoveRect {
    /// The destination rect.
    pub fn dst(&self) -> TileRect {
        TileRect {
            x: self.x,
            y: self.y,
            w: self.w,
            h: self.h,
        }
    }

    /// Source origin (may be out of range: see [`fits`](Self::fits)).
    fn src_xy(&self) -> (i64, i64) {
        (
            i64::from(self.x) - i64::from(self.dx),
            i64::from(self.y) - i64::from(self.dy),
        )
    }

    /// Non-empty, and both source and destination lie inside `fw x fh`.
    pub fn fits(&self, fw: u32, fh: u32) -> bool {
        let (sx, sy) = self.src_xy();
        self.dst().fits(fw, fh)
            && sx >= 0
            && sy >= 0
            && sx + i64::from(self.w) <= i64::from(fw)
            && sy + i64::from(self.h) <= i64::from(fh)
    }

    fn write(&self, out: &mut Vec<u8>) {
        for v in [self.x, self.y, self.w, self.h] {
            out.extend_from_slice(&v.to_be_bytes());
        }
        out.extend_from_slice(&self.dx.to_be_bytes());
        out.extend_from_slice(&self.dy.to_be_bytes());
    }

    fn read(b: &[u8]) -> Self {
        let u = |i: usize| u16::from_be_bytes([b[i], b[i + 1]]);
        Self {
            x: u(0),
            y: u(2),
            w: u(4),
            h: u(6),
            dx: u(8) as i16,
            dy: u(10) as i16,
        }
    }
}

/// Apply one MOVE to a framebuffer (memmove semantics). Validates first:
/// on error nothing is touched.
pub(crate) fn apply_move(
    fb: &mut [u8],
    fw: u32,
    fh: u32,
    stride: usize,
    bpp: usize,
    m: &MoveRect,
) -> Result<(), AppError> {
    if !m.fits(fw, fh) {
        return Err(AppError::malformed(format!("MOVE {m:?} outside {fw}x{fh}")));
    }
    if fb.len() < stride * fh as usize || stride < fw as usize * bpp {
        return Err(AppError::malformed("framebuffer smaller than its geometry"));
    }
    let (sx, sy) = m.src_xy();
    let (sx, sy) = (sx as usize, sy as usize);
    let wb = usize::from(m.w) * bpp;
    let h = usize::from(m.h);
    let (dx0, dy0) = (usize::from(m.x), usize::from(m.y));
    let row = |i: usize| {
        let s = (sy + i) * stride + sx * bpp;
        let d = (dy0 + i) * stride + dx0 * bpp;
        (s, d)
    };
    // Moving down: copy bottom rows first so no source row is overwritten
    // before it is read (rows never overlap within themselves except for a
    // purely horizontal move, which `copy_within` handles).
    if m.dy > 0 {
        for i in (0..h).rev() {
            let (s, d) = row(i);
            fb.copy_within(s..s + wb, d);
        }
    } else {
        for i in 0..h {
            let (s, d) = row(i);
            fb.copy_within(s..s + wb, d);
        }
    }
    Ok(())
}

/// One cache REF / STORE record: slot and destination / source tile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CacheOp {
    /// Cache slot index.
    pub slot: u16,
    /// Tile column.
    pub col: u16,
    /// Tile row.
    pub row: u16,
}

impl CacheOp {
    fn write(&self, out: &mut Vec<u8>) {
        for v in [self.slot, self.col, self.row] {
            out.extend_from_slice(&v.to_be_bytes());
        }
    }

    fn read(b: &[u8]) -> Self {
        let u = |i: usize| u16::from_be_bytes([b[i], b[i + 1]]);
        Self {
            slot: u(0),
            col: u(2),
            row: u(4),
        }
    }
}

/// Sections the host adds to a container.
#[derive(Debug, Clone, Default)]
pub(crate) struct Extras<'a> {
    /// Emit the CACHE section (even when both lists are empty).
    pub cache: bool,
    pub refs: &'a [CacheOp],
    pub stores: &'a [CacheOp],
    pub images: &'a [crate::lossy::OwnedImage],
}

/// One band of a parsed container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BandRef<'a> {
    /// Band codec.
    pub codec: FrameCodec,
    /// Decoded size in bytes.
    pub raw_len: usize,
    /// The band's wire bytes.
    pub data: &'a [u8],
}

/// A parsed [`FrameCodec::Packed`] container (borrowing the payload).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Packed<'a> {
    /// MOVE records, applied in order before the bands.
    pub moves: Vec<MoveRect>,
    /// Cache section present.
    pub cache: bool,
    /// Cache REF records (slot -> tile).
    pub refs: Vec<CacheOp>,
    /// Cache STORE records (tile -> slot).
    pub stores: Vec<CacheOp>,
    /// Lossy atlases.
    pub images: Vec<crate::lossy::LossyImage<'a>>,
    /// The pixel bands, concatenated in order.
    pub bands: Vec<BandRef<'a>>,
}

impl Packed<'_> {
    /// Carries a cache or lossy section (needs a [`crate::tiles::ScreenDecoder`]).
    pub(crate) fn has_extras(&self) -> bool {
        self.cache || !self.images.is_empty()
    }

    /// Uncompressed bytes the bands decode to.
    pub(crate) fn raw_len(&self) -> usize {
        self.bands.iter().map(|b| b.raw_len).sum()
    }
}

fn band_codec(b: u8) -> Option<FrameCodec> {
    Some(match b {
        0 => FrameCodec::Raw,
        1 => FrameCodec::Deflate,
        2 => FrameCodec::Zstd,
        3 => FrameCodec::Lz4,
        _ => return None,
    })
}

/// Parse a container. Errors on any inconsistency (unknown version /
/// flags / codec, too many records, lengths that do not add up exactly,
/// bands decoding to more than `MAX_RAW_TOTAL` bytes). Never panics and
/// never allocates in proportion to a declared (rather than present) size.
pub fn parse(data: &[u8]) -> Result<Packed<'_>, AppError> {
    parse_bounded(data, MAX_RAW_TOTAL)
}

/// [`parse`] with a caller-chosen ceiling on the decoded size (e.g.
/// [`max_raw_len`] of the frame geometry: the bands carry pixels only).
/// `max_raw` above `MAX_RAW_TOTAL` is clamped to it.
pub fn parse_bounded(data: &[u8], max_raw: usize) -> Result<Packed<'_>, AppError> {
    let max_raw = max_raw.min(MAX_RAW_TOTAL);
    let get = |at: usize, n: usize| {
        at.checked_add(n)
            .and_then(|end| data.get(at..end))
            .ok_or_else(|| AppError::malformed("band container truncated"))
    };
    let get_u16 = |at: usize| get(at, 2).map(|b| usize::from(u16::from_be_bytes([b[0], b[1]])));
    let head = get(0, 2)?;
    if head[0] != VERSION {
        return Err(AppError::malformed(format!(
            "band container version {} unsupported",
            head[0]
        )));
    }
    let flags = head[1];
    if flags & !(FLAG_MOVES | FLAG_CACHE | FLAG_LOSSY) != 0 {
        return Err(AppError::malformed(format!(
            "band container has unknown flags {flags:#04x}"
        )));
    }
    let mut p = 2usize;
    let mut moves = Vec::new();
    if flags & FLAG_MOVES != 0 {
        let n = get_u16(p)?;
        p += 2;
        if n == 0 || n > MAX_MOVES {
            return Err(AppError::malformed(format!(
                "band container declares {n} moves"
            )));
        }
        let recs = get(p, n * MOVE_LEN)?;
        let (recs, _) = recs.as_chunks::<MOVE_LEN>();
        moves.extend(recs.iter().map(|r| MoveRect::read(r)));
        p += n * MOVE_LEN;
    }
    let (mut refs, mut stores) = (Vec::new(), Vec::new());
    if flags & FLAG_CACHE != 0 {
        for list in [&mut refs, &mut stores] {
            let n = get_u16(p)?;
            p += 2;
            let recs = get(p, n * CACHE_OP_LEN)?;
            let (recs, _) = recs.as_chunks::<CACHE_OP_LEN>();
            list.extend(recs.iter().map(|r| CacheOp::read(r)));
            p += n * CACHE_OP_LEN;
        }
    }
    let mut images = Vec::new();
    if flags & FLAG_LOSSY != 0 {
        let n = get_u16(p)?;
        p += 2;
        if n == 0 || n > crate::lossy::MAX_IMAGES {
            return Err(AppError::malformed(format!(
                "band container declares {n} lossy images"
            )));
        }
        for _ in 0..n {
            let h = get(p, 5)?;
            let kind = h[0];
            let cols = u16::from_be_bytes([h[1], h[2]]);
            let count = usize::from(u16::from_be_bytes([h[3], h[4]]));
            p += 5;
            if count == 0 || count > crate::lossy::MAX_ATLAS_TILES {
                return Err(AppError::malformed(format!(
                    "lossy image declares {count} tiles"
                )));
            }
            let recs = get(p, count * 4)?;
            let (recs, _) = recs.as_chunks::<4>();
            let tiles = recs
                .iter()
                .map(|t| {
                    (
                        u16::from_be_bytes([t[0], t[1]]),
                        u16::from_be_bytes([t[2], t[3]]),
                    )
                })
                .collect();
            p += count * 4;
            let l = get(p, 4)?;
            let len = usize::try_from(u32::from_be_bytes([l[0], l[1], l[2], l[3]]))
                .map_err(|_| AppError::malformed("lossy image length overflow"))?;
            p += 4;
            let data = get(p, len)?;
            p += len;
            images.push(crate::lossy::LossyImage {
                kind,
                cols,
                tiles,
                data,
            });
        }
    }
    let n = get_u16(p)?;
    p += 2;
    if n > MAX_BANDS {
        return Err(AppError::malformed(format!(
            "band container declares {n} bands"
        )));
    }
    let table = get(p, n * BAND_ENTRY_LEN)?;
    p += n * BAND_ENTRY_LEN;
    let mut bands = Vec::with_capacity(n);
    let mut raw_total = 0usize;
    let (table, _) = table.as_chunks::<BAND_ENTRY_LEN>();
    for e in table {
        let codec = band_codec(e[0])
            .ok_or_else(|| AppError::malformed(format!("band codec {} unknown", e[0])))?;
        let raw_len = usize::try_from(u32::from_be_bytes([e[1], e[2], e[3], e[4]]))
            .map_err(|_| AppError::malformed("band length overflow"))?;
        let wire = usize::try_from(u32::from_be_bytes([e[5], e[6], e[7], e[8]]))
            .map_err(|_| AppError::malformed("band length overflow"))?;
        raw_total = raw_total
            .checked_add(raw_len)
            .filter(|&t| t <= max_raw)
            .ok_or_else(|| {
                AppError::malformed(format!("bands decode to more than {max_raw} bytes"))
            })?;
        let body = data
            .get(
                p..p.checked_add(wire)
                    .ok_or_else(|| AppError::malformed("band length overflow"))?,
            )
            .ok_or_else(|| AppError::malformed("band container truncated"))?;
        if codec == FrameCodec::Raw && wire != raw_len {
            return Err(AppError::malformed(format!(
                "raw band is {wire} bytes, declares {raw_len}"
            )));
        }
        bands.push(BandRef {
            codec,
            raw_len,
            data: body,
        });
        p += wire;
    }
    if p != data.len() {
        return Err(AppError::malformed(format!(
            "band container has {} trailing bytes",
            data.len() - p
        )));
    }
    Ok(Packed {
        moves,
        cache: flags & FLAG_CACHE != 0,
        refs,
        stores,
        images,
        bands,
    })
}

thread_local! {
    static ZSTD_DEC: RefCell<Option<ruzstd::FrameDecoder>> = const { RefCell::new(None) };
}

/// Decode one band into `out` (exactly `band.raw_len` bytes).
fn decode_band(b: &BandRef<'_>, out: &mut [u8]) -> Result<(), AppError> {
    if out.len() != b.raw_len {
        return Err(AppError::malformed("band output slot size mismatch"));
    }
    match b.codec {
        FrameCodec::Raw => {
            if b.data.len() != out.len() {
                return Err(AppError::malformed(format!(
                    "raw band is {} bytes, declares {}",
                    b.data.len(),
                    out.len()
                )));
            }
            out.copy_from_slice(b.data);
            Ok(())
        }
        FrameCodec::Lz4 => {
            let n = lz4_flex::block::decompress_into(b.data, out)
                .map_err(|e| AppError::malformed(format!("lz4 band corrupt: {e}")))?;
            if n != out.len() {
                return Err(AppError::malformed(format!(
                    "lz4 band decoded {n} bytes, declared {}",
                    out.len()
                )));
            }
            Ok(())
        }
        FrameCodec::Zstd => {
            if out.is_empty() {
                return if b.data.is_empty() {
                    Ok(())
                } else {
                    Err(AppError::malformed("zstd band for 0 bytes"))
                };
            }
            let r = ZSTD_DEC.with(|d| {
                let mut d = d.borrow_mut();
                let dec = d.get_or_insert_with(ruzstd::FrameDecoder::new);
                dec.decode_all(b.data, out)
            });
            match r {
                Ok(n) if n == out.len() => Ok(()),
                Ok(n) => Err(AppError::malformed(format!(
                    "zstd band decoded {n} bytes, declared {}",
                    out.len()
                ))),
                Err(e) => {
                    ZSTD_DEC.with(|d| *d.borrow_mut() = None);
                    Err(AppError::malformed(format!("zstd band corrupt: {e}")))
                }
            }
        }
        FrameCodec::Deflate => {
            let mut v = Vec::new();
            crate::terminal::decompress_into(b.data, out.len(), &mut v)?;
            if v.len() != out.len() {
                return Err(AppError::malformed(format!(
                    "deflate band decoded {} bytes, declared {}",
                    v.len(),
                    out.len()
                )));
            }
            out.copy_from_slice(&v);
            Ok(())
        }
        FrameCodec::Packed => Err(AppError::malformed("nested band container")),
    }
}

/// Decode every band into `out` (resized to exactly the bands' total), in
/// parallel for large payloads. Refuses more than `MAX_RAW_TOTAL` bytes or
/// `MAX_BANDS` bands before allocating.
pub fn decode_bands(bands: &[BandRef<'_>], out: &mut Vec<u8>) -> Result<(), AppError> {
    if bands.len() > MAX_BANDS {
        return Err(AppError::malformed(format!(
            "{} bands (max {MAX_BANDS})",
            bands.len()
        )));
    }
    let total = bands
        .iter()
        .try_fold(0usize, |acc, b| acc.checked_add(b.raw_len))
        .filter(|&t| t <= MAX_RAW_TOTAL)
        .ok_or_else(|| {
            AppError::malformed(format!("bands decode to more than {MAX_RAW_TOTAL} bytes"))
        })?;
    if out.len() != total {
        out.clear();
        out.resize(total, 0);
    }
    let mut slots: Vec<&mut [u8]> = Vec::with_capacity(bands.len());
    let mut rest = &mut out[..];
    for b in bands {
        let (a, r) = rest.split_at_mut(b.raw_len);
        slots.push(a);
        rest = r;
    }
    if bands.len() > 1 && total >= MIN_PARALLEL_DECODE {
        bands
            .par_iter()
            .zip(slots.into_par_iter())
            .try_for_each(|(b, s)| decode_band(b, s))
    } else {
        bands
            .iter()
            .zip(slots)
            .try_for_each(|(b, s)| decode_band(b, s))
    }
}

/// What the host may put in a band (from the viewer's CAPS).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct BandPolicy {
    /// zstd bands (CAP_ZSTD and a `zstd-encode` build).
    pub zstd: bool,
    /// LZ4 bands for low-ratio content (CAP_LZ4).
    pub lz4: bool,
    /// Bands to aim for on a large payload.
    pub bands: usize,
    /// See [`LZ4_MAX_RATIO`].
    pub lz4_max_ratio: f32,
}

impl Default for BandPolicy {
    fn default() -> Self {
        Self {
            zstd: crate::codec::zstd_encode_available(),
            lz4: true,
            bands: TARGET_BANDS,
            lz4_max_ratio: LZ4_MAX_RATIO,
        }
    }
}

/// Counters of one [`encode_packed`] call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PackStats {
    pub bands: usize,
    pub lz4_bands: usize,
    pub zstd_bands: usize,
    pub raw_bands: usize,
}

/// Split `len` bytes into band lengths (4-byte aligned, all but the last
/// equal): one band per [`MIN_BAND`], at most `target`.
pub(crate) fn band_lengths(len: usize, target: usize) -> Vec<usize> {
    if len == 0 {
        return Vec::new();
    }
    let n = (len / MIN_BAND).clamp(1, target.clamp(1, MAX_BANDS));
    let per = len.div_ceil(n).next_multiple_of(4);
    let mut v = Vec::with_capacity(n);
    let mut left = len;
    while left > 0 {
        let b = per.min(left);
        v.push(b);
        left -= b;
    }
    v
}

/// Estimate the LZ4 ratio of `band` on three 16 KB samples (start, middle,
/// end): cheap (~10 us), good enough to tell photo from UI.
fn lz4_ratio_estimate(band: &[u8]) -> f32 {
    const S: usize = 16 * 1024;
    if band.len() <= 3 * S {
        let c = lz4_flex::block::compress(band);
        return band.len() as f32 / c.len().max(1) as f32;
    }
    let mid = (band.len() / 2) & !3;
    let mut sample = Vec::with_capacity(3 * S);
    sample.extend_from_slice(&band[..S]);
    sample.extend_from_slice(&band[mid..mid + S]);
    sample.extend_from_slice(&band[band.len() - S..]);
    let c = lz4_flex::block::compress(&sample);
    sample.len() as f32 / c.len().max(1) as f32
}

#[cfg(feature = "zstd-encode")]
thread_local! {
    static ZSTD_ENC: RefCell<Option<zstd::bulk::Compressor<'static>>> = const { RefCell::new(None) };
}

/// zstd-compress one band as an independent frame (content size declared).
#[cfg(feature = "zstd-encode")]
fn zstd_band(band: &[u8]) -> Option<Vec<u8>> {
    ZSTD_ENC.with(|c| {
        let mut c = c.borrow_mut();
        if c.is_none() {
            *c = zstd::bulk::Compressor::new(crate::codec::ZSTD_LEVEL).ok();
        }
        c.as_mut()?.compress(band).ok()
    })
}

#[cfg(not(feature = "zstd-encode"))]
fn zstd_band(_band: &[u8]) -> Option<Vec<u8>> {
    None
}

/// Compress one band per the policy: (codec, wire bytes).
fn pack_band(band: &[u8], policy: &BandPolicy) -> (FrameCodec, Vec<u8>) {
    let photo = policy.lz4 && (!policy.zstd || lz4_ratio_estimate(band) < policy.lz4_max_ratio);
    let (codec, wire) = if photo {
        (FrameCodec::Lz4, lz4_flex::block::compress(band))
    } else if let Some(z) = policy.zstd.then(|| zstd_band(band)).flatten() {
        (FrameCodec::Zstd, z)
    } else if policy.lz4 {
        (FrameCodec::Lz4, lz4_flex::block::compress(band))
    } else {
        (FrameCodec::Deflate, crate::terminal::compress_frame(band))
    };
    if wire.len() >= band.len() {
        return (FrameCodec::Raw, band.to_vec());
    }
    (codec, wire)
}

/// Build a [`FrameCodec::Packed`] container for `data` (+ `moves`) into
/// `out` (cleared first): bands compressed in parallel.
pub(crate) fn encode_packed(
    data: &[u8],
    moves: &[MoveRect],
    policy: &BandPolicy,
    out: &mut Vec<u8>,
) -> PackStats {
    encode_packed_ex(data, moves, &Extras::default(), policy, out)
}

/// [`encode_packed`] plus the cache / lossy sections (cache REF / STORE records,
/// lossy atlases; see the module docs).
pub(crate) fn encode_packed_ex(
    data: &[u8],
    moves: &[MoveRect],
    extras: &Extras<'_>,
    policy: &BandPolicy,
    out: &mut Vec<u8>,
) -> PackStats {
    debug_assert!(
        extras.refs.len() <= usize::from(u16::MAX) && extras.stores.len() <= usize::from(u16::MAX)
    );
    debug_assert!(extras.images.len() <= crate::lossy::MAX_IMAGES);
    let lens = band_lengths(data.len(), policy.bands);
    let mut pieces: Vec<&[u8]> = Vec::with_capacity(lens.len());
    let mut at = 0;
    for l in &lens {
        pieces.push(&data[at..at + l]);
        at += l;
    }
    let packed: Vec<(FrameCodec, Vec<u8>)> = if pieces.len() > 1 {
        pieces.par_iter().map(|b| pack_band(b, policy)).collect()
    } else {
        pieces.iter().map(|b| pack_band(b, policy)).collect()
    };
    out.clear();
    out.push(VERSION);
    let moves = &moves[..moves.len().min(MAX_MOVES)];
    let cache = extras.cache || !extras.refs.is_empty() || !extras.stores.is_empty();
    let mut flags = if moves.is_empty() { 0 } else { FLAG_MOVES };
    if cache {
        flags |= FLAG_CACHE;
    }
    if !extras.images.is_empty() {
        flags |= FLAG_LOSSY;
    }
    out.push(flags);
    if !moves.is_empty() {
        out.extend_from_slice(&(moves.len() as u16).to_be_bytes());
        moves.iter().for_each(|m| m.write(out));
    }
    if cache {
        for list in [extras.refs, extras.stores] {
            out.extend_from_slice(&(list.len() as u16).to_be_bytes());
            list.iter().for_each(|c| c.write(out));
        }
    }
    if !extras.images.is_empty() {
        out.extend_from_slice(&(extras.images.len() as u16).to_be_bytes());
        for im in extras.images {
            out.push(im.kind);
            out.extend_from_slice(&im.cols.to_be_bytes());
            out.extend_from_slice(&(im.tiles.len() as u16).to_be_bytes());
            for &(c, r) in &im.tiles {
                out.extend_from_slice(&c.to_be_bytes());
                out.extend_from_slice(&r.to_be_bytes());
            }
            out.extend_from_slice(&(im.data.len() as u32).to_be_bytes());
            out.extend_from_slice(&im.data);
        }
    }
    out.extend_from_slice(&(packed.len() as u16).to_be_bytes());
    let mut st = PackStats {
        bands: packed.len(),
        ..PackStats::default()
    };
    for ((codec, wire), raw) in packed.iter().zip(&lens) {
        out.push(*codec as u8);
        out.extend_from_slice(&(*raw as u32).to_be_bytes());
        out.extend_from_slice(&(wire.len() as u32).to_be_bytes());
        match codec {
            FrameCodec::Lz4 => st.lz4_bands += 1,
            FrameCodec::Zstd => st.zstd_bands += 1,
            FrameCodec::Raw => st.raw_bands += 1,
            _ => {}
        }
    }
    for (_, wire) in &packed {
        out.extend_from_slice(wire);
    }
    st
}

/// Compress `data` as concatenated independent zstd frames, one per band,
/// in parallel (plain [`FrameCodec::Zstd`] on the wire; see the module
/// docs). `None` without `zstd-encode` or when a band fails.
pub(crate) fn compress_zstd_bands(data: &[u8], target: usize) -> Option<Vec<u8>> {
    if !crate::codec::zstd_encode_available() {
        return None;
    }
    let lens = band_lengths(data.len(), target);
    let mut pieces: Vec<&[u8]> = Vec::with_capacity(lens.len());
    let mut at = 0;
    for l in &lens {
        pieces.push(&data[at..at + l]);
        at += l;
    }
    let frames: Vec<Option<Vec<u8>>> = pieces.par_iter().map(|b| zstd_band(b)).collect();
    let mut out = Vec::with_capacity(frames.iter().map(|f| f.as_ref().map_or(0, Vec::len)).sum());
    for f in frames {
        out.extend_from_slice(&f?);
    }
    Some(out)
}

#[cfg(test)]
mod tests;
