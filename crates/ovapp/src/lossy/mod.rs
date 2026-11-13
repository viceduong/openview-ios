//! Per-tile content classification, lossy-first
//! JPEG image tiles, and the exact-pixel tile cache.
//!
//! # Classification (`classify_tiles`)
//!
//! Every candidate `TILE`x`TILE` tile is sampled on every other row:
//! the share of pixels equal to their left or upper neighbour (UI, text,
//! flat fills: high; photos, video stills, noise: near zero) and an
//! approximate distinct-colour count (a 1024-bit hashed bitset). A tile is
//! `IMAGE` when few neighbours repeat **and** it has many
//! colours, or when a photo's edge cuts through it (a quarter of it noisy
//! and 192+ colours); everything else (text, flat, palette-like dithers)
//! stays on the lossless path. ~0.3 ms for a whole 1080p frame (tile rows
//! in parallel), 1.7 ms on one core.
//!
//! # Lossy images (`CAP_LOSSY`)
//!
//! Image tiles of an update are packed into *atlases* — up to
//! `ATLAS_COLS` tiles per atlas row, raster order, edge tiles padded by
//! edge replication — and each atlas is one baseline JPEG (pure-Rust
//! `jpeg-encoder` on the host, pure-Rust `zune-jpeg` on every viewer). The
//! atlases of one update encode and decode in parallel. See
//! [`crate::bands`] for the wire layout.
//!
//! # Tile cache (`CAP_CACHE`)
//!
//! The host keeps a bounded LRU ([`TileCache`]) of 128-bit content hashes of
//! tiles the viewer holds **exactly**; the viewer keeps the pixels in
//! numbered slots ([`SlotStore`]). The host assigns every slot explicitly
//! (`STORE slot <- tile` after an update, `REF slot -> tile` instead of
//! pixels), so the viewer never hashes and the two sides cannot drift: the
//! viewer simply does what it is told. Both sides clear on a full frame.

use std::collections::HashMap;

use rayon::prelude::*;

use crate::error::AppError;
use crate::tiles::{TileMap, TileRect, TILE};

/// Tile edge in pixels (re-exported for readability here).
const T: usize = TILE as usize;
/// Bytes of one full BGRA tile.
pub const TILE_BYTES: usize = T * T * 4;
/// Most tiles per atlas row.
pub(crate) const ATLAS_COLS: usize = 8;
/// Most tiles one atlas (one JPEG) carries.
pub(crate) const MAX_ATLAS_TILES: usize = 64;
/// Most atlases (JPEGs) one update carries.
pub(crate) const MAX_IMAGES: usize = 256;
/// Default JPEG quality for image tiles (a size / quality trade-off picked by measurement).
pub const DEFAULT_QUALITY: u8 = 75;
/// Slots the host uses (16 KB each on the viewer: 64 MB).
pub(crate) const CACHE_SLOTS: usize = 4096;
/// Highest slot count a viewer accepts.
pub(crate) const MAX_SLOTS: usize = 4096;
/// A tile is "image" when fewer than this percentage of its sampled
/// pixels repeat their left / upper neighbour ...
pub(crate) const IMAGE_MAX_SAME_PCT: u32 = 30;
/// ... and its sample shows at least this many (approximate) colours.
pub(crate) const IMAGE_MIN_COLOURS: u32 = 48;
/// A tile a photo only partly covers (a photo window's edge) is "image"
/// too when at least this percentage of its samples is noisy (differs
/// from both neighbours) ...
pub(crate) const MIXED_MIN_NOISY_PCT: u32 = 25;
/// ... and it shows at least this many colours (real-desktop text tiles
/// stay below ~160; photo edges are 200+).
pub(crate) const MIXED_MIN_COLOURS: u32 = 192;
/// Tiles narrower / shorter than this are never image (edge slivers).
const MIN_CLASSIFY_DIM: usize = 16;

/// Pixel rect of tile (`col`, `row`) clipped to `w x h`.
pub(crate) fn tile_rect(col: u32, row: u32, w: u32, h: u32) -> TileRect {
    let x = col * TILE;
    let y = row * TILE;
    TileRect {
        x: x as u16,
        y: y as u16,
        w: (w - x).min(TILE) as u16,
        h: (h - y).min(TILE) as u16,
    }
}

/// Is the tile at `(x0, y0)` (`tw x th`, BGRA rows of `stride` bytes)
/// photo-like? See the module docs.
pub(crate) fn classify_tile(
    px: &[u8],
    stride: usize,
    x0: usize,
    y0: usize,
    tw: usize,
    th: usize,
) -> bool {
    if tw < MIN_CLASSIFY_DIM || th < MIN_CLASSIFY_DIM {
        return false;
    }
    let mut same = 0u32;
    let mut total = 0u32;
    let mut bits = [0u64; 16];
    let px32 = |row: &[u8], x: usize| {
        u32::from_le_bytes([row[x * 4], row[x * 4 + 1], row[x * 4 + 2], row[x * 4 + 3]])
    };
    let mut y = y0 + 1;
    while y < y0 + th {
        let row = &px[y * stride + x0 * 4..y * stride + (x0 + tw) * 4];
        let up = &px[(y - 1) * stride + x0 * 4..(y - 1) * stride + (x0 + tw) * 4];
        let mut left = px32(row, 0);
        for x in 1..tw {
            let p = px32(row, x);
            same += u32::from(p == left || p == px32(up, x));
            if x & 1 == 0 {
                let i = ((p & 0x00FF_FFFF).wrapping_mul(0x9E37_79B1) >> 22) as usize;
                bits[i >> 6] |= 1 << (i & 63);
            }
            left = p;
        }
        total += (tw - 1) as u32;
        y += 2;
    }
    let colours: u32 = bits.iter().map(|b| b.count_ones()).sum();
    if total == 0 {
        return false;
    }
    // Colour thresholds are for a full tile's 32x32 colour samples; a
    // clipped edge tile samples fewer pixels.
    let scale = |c: u32| (c as usize * (tw / 2) * (th / 2) / (32 * 32)).max(8) as u32;
    let noisy = 100 - same * 100 / total;
    // Mostly photo, or a tile a photo's edge cuts through (its photo part
    // alone costs more lossless than the whole tile as JPEG).
    (noisy > 100 - IMAGE_MAX_SAME_PCT && colours >= scale(IMAGE_MIN_COLOURS))
        || (noisy >= MIXED_MIN_NOISY_PCT && colours >= scale(MIXED_MIN_COLOURS))
}

/// Classify the tiles set in `cand` (a `w x h` BGRA frame); `out` is
/// reshaped to the frame and gets the image tiles. Returns their count.
pub(crate) fn classify_tiles(
    px: &[u8],
    w: u32,
    h: u32,
    stride: usize,
    cand: &TileMap,
    out: &mut TileMap,
) -> usize {
    out.reset(w, h);
    if cand.cols() != out.cols() || cand.rows() != out.rows() || cand.count() == 0 {
        return 0;
    }
    let rows: Vec<Vec<u32>> = {
        let row_job = |r: u32| -> Vec<u32> {
            (0..cand.cols())
                .filter(|&c| cand.get(c, r))
                .filter(|&c| {
                    let tr = tile_rect(c, r, w, h);
                    classify_tile(
                        px,
                        stride,
                        usize::from(tr.x),
                        usize::from(tr.y),
                        usize::from(tr.w),
                        usize::from(tr.h),
                    )
                })
                .collect()
        };
        if cand.count() >= 32 {
            (0..cand.rows()).into_par_iter().map(row_job).collect()
        } else {
            (0..cand.rows()).map(row_job).collect()
        }
    };
    for (r, cols) in rows.iter().enumerate() {
        for &c in cols {
            out.set(c, r as u32);
        }
    }
    out.count()
}

// ---------------------------------------------------------------- hashing

#[inline]
fn mum(a: u64, b: u64) -> u64 {
    let r = u128::from(a) * u128::from(b);
    (r as u64) ^ ((r >> 64) as u64)
}

const K: [u64; 6] = [
    0xa076_1d64_78bd_642f,
    0xe703_7ed1_a0b4_28db,
    0x8ebc_6af0_9c88_c6e3,
    0x5899_65cc_7537_4cc3,
    0x1d8e_4e27_c47d_124f,
    0x9e37_79b9_7f4a_7c15,
];

/// 128-bit content hash of the full tile at pixel `(x0, y0)` (BGRA rows of
/// `stride` bytes; the tile must lie inside `px`). Four independent
/// folded-multiply lanes (wyhash-style), ~0.4 us per tile.
pub fn tile_hash(px: &[u8], stride: usize, x0: usize, y0: usize) -> u128 {
    tile_hash_wh(px, stride, x0, y0, T, T)
}

/// [`tile_hash`] of a `tw x th` (clipped edge) tile; the size is part of
/// the hash, so equal hashes imply equal shapes.
pub(crate) fn tile_hash_wh(
    px: &[u8],
    stride: usize,
    x0: usize,
    y0: usize,
    tw: usize,
    th: usize,
) -> u128 {
    let dims = ((tw as u64) << 32) | th as u64;
    let (mut a, mut b, mut c, mut d) = (K[0] ^ dims, K[1], K[2], K[3] ^ dims.rotate_left(17));
    let w = |s: &[u8], i: usize| u64::from_le_bytes(s[i * 8..i * 8 + 8].try_into().unwrap());
    for y in 0..th {
        let row = &px[(y0 + y) * stride + x0 * 4..(y0 + y) * stride + (x0 + tw) * 4];
        // 64-byte steps: 4 lanes x 2 words (a full row = 4 steps).
        let steps = row.len() / 64;
        for k in 0..steps {
            let o = k * 8;
            a = mum(w(row, o) ^ K[0], w(row, o + 1) ^ a);
            b = mum(w(row, o + 2) ^ K[1], w(row, o + 3) ^ b);
            c = mum(w(row, o + 4) ^ K[2], w(row, o + 5) ^ c);
            d = mum(w(row, o + 6) ^ K[3], w(row, o + 7) ^ d);
        }
        // Clipped rows: the tail, 8 bytes (zero padded) at a time.
        for t in row[steps * 64..].chunks(8) {
            let mut v = [0u8; 8];
            v[..t.len()].copy_from_slice(t);
            a = mum(u64::from_le_bytes(v) ^ K[4], a ^ K[5]);
        }
    }
    let lo = mum(a ^ K[4], b ^ K[5]) ^ mum(c ^ K[1], d ^ K[2]);
    let hi = mum(a ^ d ^ K[3], b ^ c ^ K[0]) ^ mum(lo ^ K[5], a ^ K[4]);
    (u128::from(hi) << 64) | u128::from(lo)
}

// ---------------------------------------------------------------- host cache

const NIL: u16 = u16::MAX;

/// Host-side model of the viewer's tile slots: content hash -> slot, with
/// LRU eviction. Cloneable (the encoder keeps a committed and a tentative
/// copy, see `UpdateEncoder`).
#[derive(Debug, Clone)]
pub struct TileCache {
    cap: usize,
    map: HashMap<u128, u16>,
    key: Vec<Option<u128>>,
    prev: Vec<u16>,
    next: Vec<u16>,
    /// Most / least recently used slot.
    head: u16,
    tail: u16,
    used: usize,
}

impl Default for TileCache {
    fn default() -> Self {
        Self::new(CACHE_SLOTS)
    }
}

impl TileCache {
    /// An empty cache model with `slots` slots (clamped to `1..=MAX_SLOTS`).
    pub fn new(slots: usize) -> Self {
        let cap = slots.clamp(1, MAX_SLOTS);
        Self {
            cap,
            map: HashMap::with_capacity(cap),
            key: vec![None; cap],
            prev: vec![NIL; cap],
            next: vec![NIL; cap],
            head: NIL,
            tail: NIL,
            used: 0,
        }
    }

    /// Number of slots.
    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Slots currently holding a tile.
    pub fn len(&self) -> usize {
        self.used
    }

    /// No slot holds a tile.
    pub fn is_empty(&self) -> bool {
        self.used == 0
    }

    /// Forget everything (the viewer clears on a full frame too).
    pub fn clear(&mut self) {
        self.map.clear();
        self.key.iter_mut().for_each(|k| *k = None);
        self.prev.iter_mut().for_each(|p| *p = NIL);
        self.next.iter_mut().for_each(|p| *p = NIL);
        self.head = NIL;
        self.tail = NIL;
        self.used = 0;
    }

    fn unlink(&mut self, s: u16) {
        let (p, n) = (self.prev[s as usize], self.next[s as usize]);
        if p != NIL {
            self.next[p as usize] = n;
        } else {
            self.head = n;
        }
        if n != NIL {
            self.prev[n as usize] = p;
        } else {
            self.tail = p;
        }
        self.prev[s as usize] = NIL;
        self.next[s as usize] = NIL;
    }

    fn push_front(&mut self, s: u16) {
        self.prev[s as usize] = NIL;
        self.next[s as usize] = self.head;
        if self.head != NIL {
            self.prev[self.head as usize] = s;
        }
        self.head = s;
        if self.tail == NIL {
            self.tail = s;
        }
    }

    /// Slot holding `h` (marked most recently used).
    pub fn lookup(&mut self, h: u128) -> Option<u16> {
        let s = *self.map.get(&h)?;
        self.unlink(s);
        self.push_front(s);
        Some(s)
    }

    /// Whether `h` is cached (no LRU touch).
    pub fn contains(&self, h: u128) -> bool {
        self.map.contains_key(&h)
    }

    /// Put `h` in a slot: a free one, else the least recently used one
    /// (evicted). Returns the slot (an already cached `h` keeps its slot).
    pub fn insert(&mut self, h: u128) -> u16 {
        if let Some(s) = self.lookup(h) {
            return s;
        }
        let s = if self.used < self.cap {
            self.used += 1;
            (self.used - 1) as u16
        } else {
            let s = self.tail;
            self.unlink(s);
            if let Some(old) = self.key[s as usize].take() {
                self.map.remove(&old);
            }
            s
        };
        self.key[s as usize] = Some(h);
        self.map.insert(h, s);
        self.push_front(s);
        s
    }

    /// Content hash a slot holds (tests / diagnostics).
    pub fn slot_key(&self, s: u16) -> Option<u128> {
        self.key.get(s as usize).copied().flatten()
    }
}

// ---------------------------------------------------------------- viewer slots

/// Viewer-side tile slots (lazily allocated, up to 16 KB each). A slot
/// remembers its tile's size: clipped edge tiles are cached too, and a REF
/// only fits a destination tile of the same size.
#[derive(Debug, Default, Clone)]
pub struct SlotStore {
    slots: Vec<(u16, u16, Vec<u8>)>,
    filled: usize,
}

impl SlotStore {
    /// An empty slot store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Slots holding a tile.
    pub fn filled(&self) -> usize {
        self.filled
    }

    /// Mark every slot empty (allocations kept).
    pub fn clear(&mut self) {
        self.slots.iter_mut().for_each(|s| s.2.clear());
        self.filled = 0;
    }

    /// Slot `s` holds a tile.
    pub fn is_filled(&self, s: u16) -> bool {
        self.slots.get(s as usize).is_some_and(|v| !v.2.is_empty())
    }

    /// Size `(w, h)` of the tile slot `s` holds.
    pub fn dims(&self, s: u16) -> Option<(usize, usize)> {
        self.slots
            .get(s as usize)
            .filter(|v| !v.2.is_empty())
            .map(|v| (usize::from(v.0), usize::from(v.1)))
    }

    /// Copy the `tw x th` tile at pixel `(x0, y0)` of `fb` into slot `s`.
    #[allow(clippy::too_many_arguments)]
    pub fn store(
        &mut self,
        s: u16,
        fb: &[u8],
        stride: usize,
        x0: usize,
        y0: usize,
        tw: usize,
        th: usize,
    ) {
        let i = s as usize;
        if self.slots.len() <= i {
            self.slots.resize_with(i + 1, || (0, 0, Vec::new()));
        }
        let (sw, sh, v) = &mut self.slots[i];
        if v.is_empty() {
            self.filled += 1;
        }
        (*sw, *sh) = (tw as u16, th as u16);
        v.clear();
        v.reserve(tw * th * 4);
        for y in 0..th {
            let o = (y0 + y) * stride + x0 * 4;
            v.extend_from_slice(&fb[o..o + tw * 4]);
        }
    }

    /// Content hash of slot `s` (`None` when empty), as `tile_hash_wh`:
    /// diagnostics / sync tests against the host's [`TileCache`].
    pub fn slot_hash(&self, s: u16) -> Option<u128> {
        let (w, h, v) = self.slots.get(s as usize).filter(|v| !v.2.is_empty())?;
        let (w, h) = (usize::from(*w), usize::from(*h));
        Some(tile_hash_wh(v, w * 4, 0, 0, w, h))
    }

    /// Copy slot `s` into the `tw x th` tile at pixel `(x0, y0)` of `fb`.
    /// False (nothing written) for an empty slot or a size mismatch.
    #[allow(clippy::too_many_arguments)]
    pub fn load(
        &self,
        s: u16,
        fb: &mut [u8],
        stride: usize,
        x0: usize,
        y0: usize,
        tw: usize,
        th: usize,
    ) -> bool {
        let Some((_, _, v)) = self
            .slots
            .get(s as usize)
            .filter(|v| (usize::from(v.0), usize::from(v.1)) == (tw, th) && !v.2.is_empty())
        else {
            return false;
        };
        for (y, src) in v.chunks_exact(tw * 4).enumerate() {
            let o = (y0 + y) * stride + x0 * 4;
            fb[o..o + tw * 4].copy_from_slice(src);
        }
        true
    }
}

// ---------------------------------------------------------------- JPEG atlases

/// One lossy atlas on the wire (borrowing the container).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LossyImage<'a> {
    /// 0 = baseline JPEG (the only kind so far).
    pub kind: u8,
    /// Tiles per atlas row.
    pub cols: u16,
    /// Destination tiles `(col, row)` in atlas order.
    pub tiles: Vec<(u16, u16)>,
    /// The JPEG bytes.
    pub data: &'a [u8],
}

/// An encoded atlas (host side).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OwnedImage {
    pub kind: u8,
    pub cols: u16,
    pub tiles: Vec<(u16, u16)>,
    pub data: Vec<u8>,
}

/// JPEG chroma subsampling for image tiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chroma {
    /// 4:2:0 (default: ~half the bytes of 4:4:4).
    Sub420,
    /// 4:4:4 (sharper coloured edges).
    Full444,
}

/// Lossy encoder settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LossyParams {
    /// JPEG quality (1..=100).
    pub quality: u8,
    /// Chroma subsampling.
    pub chroma: Chroma,
}

impl Default for LossyParams {
    fn default() -> Self {
        Self {
            quality: DEFAULT_QUALITY,
            chroma: Chroma::Sub420,
        }
    }
}

/// Atlas grid for `n` tiles: (cols, rows).
pub(crate) fn atlas_dims(n: usize, cols: usize) -> (usize, usize) {
    let cols = cols.clamp(1, ATLAS_COLS).min(n.max(1));
    (cols, n.div_ceil(cols))
}

/// Tiles per atlas for an update with `n` image tiles: enough atlases to
/// keep the cores busy, few enough to amortise the ~600-byte JPEG headers.
fn group_size(n: usize) -> usize {
    n.div_ceil(8).clamp(16, MAX_ATLAS_TILES)
}

/// Encode the tiles `tiles` (`(col, row)`, any order — kept) of a `w x h`
/// BGRA frame into atlases (in parallel). `None` when an atlas fails to
/// encode (the caller falls back to lossless).
pub(crate) fn encode_images(
    px: &[u8],
    w: u32,
    h: u32,
    stride: usize,
    tiles: &[(u16, u16)],
    p: LossyParams,
) -> Option<Vec<OwnedImage>> {
    if tiles.is_empty() {
        return Some(Vec::new());
    }
    let groups: Vec<&[(u16, u16)]> = tiles.chunks(group_size(tiles.len())).collect();
    if groups.len() > MAX_IMAGES {
        return None;
    }
    let enc = |g: &&[(u16, u16)]| encode_atlas(px, w, h, stride, g, p);
    let out: Vec<Option<OwnedImage>> = if groups.len() > 1 {
        groups.par_iter().map(enc).collect()
    } else {
        groups.iter().map(enc).collect()
    };
    out.into_iter().collect()
}

fn encode_atlas(
    px: &[u8],
    w: u32,
    h: u32,
    stride: usize,
    tiles: &[(u16, u16)],
    p: LossyParams,
) -> Option<OwnedImage> {
    let (cols, rows) = atlas_dims(tiles.len(), ATLAS_COLS);
    let aw = cols * T;
    let ah = rows * T;
    let astride = aw * 4;
    // Unused cells: flat mid grey (costs a few bytes).
    let mut atlas = vec![128u8; astride * ah];
    for (i, &(c, r)) in tiles.iter().enumerate() {
        let tr = tile_rect(u32::from(c), u32::from(r), w, h);
        if !tr.fits(w, h) {
            return None;
        }
        let (ax, ay) = ((i % cols) * T, (i / cols) * T);
        let (tw, th) = (usize::from(tr.w), usize::from(tr.h));
        for y in 0..T {
            let sy = usize::from(tr.y) + y.min(th - 1);
            let src = &px
                [sy * stride + usize::from(tr.x) * 4..sy * stride + (usize::from(tr.x) + tw) * 4];
            let d = (ay + y) * astride + ax * 4;
            atlas[d..d + tw * 4].copy_from_slice(src);
            // Edge replication right of a partial tile.
            if tw < T {
                let last: [u8; 4] = src[(tw - 1) * 4..tw * 4].try_into().unwrap();
                for x in tw..T {
                    atlas[d + x * 4..d + x * 4 + 4].copy_from_slice(&last);
                }
            }
        }
    }
    let mut data = Vec::with_capacity(tiles.len() * 3000 + 700);
    let mut e = jpeg_encoder::Encoder::new(&mut data, p.quality.clamp(1, 100));
    e.set_sampling_factor(match p.chroma {
        Chroma::Sub420 => jpeg_encoder::SamplingFactor::R_4_2_0,
        Chroma::Full444 => jpeg_encoder::SamplingFactor::R_4_4_4,
    });
    e.encode(&atlas, aw as u16, ah as u16, jpeg_encoder::ColorType::Bgra)
        .ok()?;
    Some(OwnedImage {
        kind: 0,
        cols: cols as u16,
        tiles: tiles.to_vec(),
        data,
    })
}

/// Decode one atlas to tight BGRA (`cols*64 x rows*64`), validating its
/// geometry against the tile list.
pub(crate) fn decode_atlas(img: &LossyImage<'_>, out: &mut Vec<u8>) -> Result<(), AppError> {
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;
    if img.kind != 0 {
        return Err(AppError::malformed(format!(
            "lossy image kind {} unknown",
            img.kind
        )));
    }
    if img.tiles.is_empty()
        || img.tiles.len() > MAX_ATLAS_TILES
        || img.cols == 0
        || usize::from(img.cols) > ATLAS_COLS
    {
        return Err(AppError::malformed(format!(
            "lossy atlas of {} tiles / {} cols invalid",
            img.tiles.len(),
            img.cols
        )));
    }
    let cols = usize::from(img.cols).min(img.tiles.len());
    if cols != usize::from(img.cols) {
        return Err(AppError::malformed("lossy atlas wider than its tiles"));
    }
    let (aw, ah) = (cols * T, img.tiles.len().div_ceil(cols) * T);
    let opts = DecoderOptions::default()
        .jpeg_set_out_colorspace(ColorSpace::BGRA)
        .set_max_width(aw)
        .set_max_height(ah);
    let mut d = zune_jpeg::JpegDecoder::new_with_options(img.data, opts);
    d.decode_headers()
        .map_err(|e| AppError::malformed(format!("lossy atlas header: {e:?}")))?;
    let (dw, dh) = d
        .dimensions()
        .ok_or_else(|| AppError::malformed("lossy atlas without dimensions"))?;
    if (dw, dh) != (aw, ah) {
        return Err(AppError::malformed(format!(
            "lossy atlas is {dw}x{dh}, tiles need {aw}x{ah}"
        )));
    }
    let n = d
        .output_buffer_size()
        .ok_or_else(|| AppError::malformed("lossy atlas size"))?;
    if n != aw * ah * 4 {
        return Err(AppError::malformed(format!(
            "lossy atlas decodes to {n} bytes, want {}",
            aw * ah * 4
        )));
    }
    out.clear();
    out.resize(n, 0);
    d.decode_into(out)
        .map_err(|e| AppError::malformed(format!("lossy atlas corrupt: {e:?}")))?;
    Ok(())
}

/// Paint a decoded atlas (`decode_atlas` output) into a `fw x fh` BGRA
/// framebuffer (tiles clipped to the frame; validated by the caller).
pub(crate) fn paint_atlas(
    img: &LossyImage<'_>,
    atlas: &[u8],
    fb: &mut [u8],
    fw: u32,
    fh: u32,
    stride: usize,
) {
    let cols = usize::from(img.cols);
    let astride = cols * T * 4;
    for (i, &(c, r)) in img.tiles.iter().enumerate() {
        let tr = tile_rect(u32::from(c), u32::from(r), fw, fh);
        let (ax, ay) = ((i % cols) * T, (i / cols) * T);
        let wb = usize::from(tr.w) * 4;
        for y in 0..usize::from(tr.h) {
            let s = (ay + y) * astride + ax * 4;
            let d = (usize::from(tr.y) + y) * stride + usize::from(tr.x) * 4;
            fb[d..d + wb].copy_from_slice(&atlas[s..s + wb]);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
