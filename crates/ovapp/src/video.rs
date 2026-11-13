//! Phase 5: hardware H.264 for high-motion screen content.
//!
//! Lossless dirty tiles ([`crate::tiles`]) stay the default. When a large
//! region keeps changing (video, fast scrolling, games) the host's
//! [`MotionDetector`] switches that **region** to H.264: the tile encoder
//! masks it out ([`crate::sendq::UpdateEncoder::submit_masked`]) and a
//! [`VideoSession`] streams it as `VIDEO_BEGIN` access units composited over
//! the viewer's tile framebuffer. Once the region has been still for
//! [`MotionConfig::exit_after`] the host sends a `VIDEO_END` marker followed
//! by a forced lossless tile refresh of the whole region.
//!
//! # Wire form (see [`crate::terminal`])
//!
//! `VIDEO_BEGIN` (opcode `0x25`) carries a [`VideoHeader`]; the access unit
//! follows as `FRAME_DATA` chunks closed by `FRAME_END(seq)` — the same
//! chunking/pacing path as screen frames. The payload is **Annex-B** H.264
//! (start codes, exactly what the Media Foundation encoder emits); every
//! keyframe carries SPS + PPS in-band. Video seqs are consecutive per
//! session: a gap (or a decode error) makes the viewer drop P-frames and send
//! `VIDEO_IDR_REQUEST` (`0x26`) until the next keyframe. Video access units
//! are never replaced ("newest wins" would break the reference chain): the
//! host instead holds capture while the link is busy and adapts the bitrate
//! to the pacer's delivery-rate estimate.
//!
//! Only viewers that advertised [`crate::terminal::CAP_H264`] ever see any of
//! this. The iOS shell gets AVCC (length-prefixed NAL units) + the parameter
//! sets split out by [`AccessUnit::from_annexb`]; desktop decodes Annex-B.
//!
//! Everything here except the codecs themselves is pure and unit-tested; the
//! Media Foundation encoder/decoder live in [`crate::mfcodec`] (Windows).

use std::time::{Duration, Instant};

use crate::sendq::{EncodedFrame, FrameMailbox};
use crate::tiles::{TileMap, TileRect, TILE};

/// `VIDEO_BEGIN` codec byte: H.264, Annex-B byte stream.
pub const CODEC_H264: u8 = 1;
/// [`VideoHeader::flags`]: an IDR access unit (SPS + PPS in-band).
pub const VIDEO_KEYFRAME: u8 = 1 << 0;
/// [`VideoHeader::flags`]: motion over — no payload; `rect` is the region
/// the following lossless tile update refreshes. The viewer drops its video
/// overlay once that update is applied.
pub const VIDEO_END: u8 = 1 << 1;
/// Wire size of a `VIDEO_BEGIN` payload (without the opcode).
pub const VIDEO_HEADER_LEN: usize = 38;
/// Video access units queued in the mailbox at most; past this the host
/// skips encoding a capture (the link is the bottleneck) instead of piling
/// up a backlog it may not drop.
pub const MAX_VIDEO_QUEUE: usize = 3;

/// The header of one video access unit (`VIDEO_BEGIN`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VideoHeader {
    /// Consecutive per host session (END markers included).
    pub seq: u64,
    /// [`VIDEO_KEYFRAME`] | [`VIDEO_END`].
    pub flags: u8,
    /// [`CODEC_H264`].
    pub codec: u8,
    /// Where the picture goes in the screen (its size = the coded picture
    /// size, always even).
    pub rect: TileRect,
    /// The screen (tile framebuffer) size `rect` refers to.
    pub screen_w: u32,
    pub screen_h: u32,
    /// Capture time, microseconds since the UNIX epoch (end-to-end latency
    /// on a shared clock; also the presentation timestamp).
    pub pts_us: u64,
}

impl VideoHeader {
    pub fn is_keyframe(&self) -> bool {
        self.flags & VIDEO_KEYFRAME != 0
    }

    pub fn is_end(&self) -> bool {
        self.flags & VIDEO_END != 0
    }

    /// Wire form: `8B seq, 1B flags, 1B codec, 2B x, 2B y, 2B w, 2B h, 4B
    /// screen_w, 4B screen_h, 8B pts_us, 4B compressed_len`, all BE.
    pub fn encode(&self, compressed_len: u32, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.seq.to_be_bytes());
        out.push(self.flags);
        out.push(self.codec);
        for v in [self.rect.x, self.rect.y, self.rect.w, self.rect.h] {
            out.extend_from_slice(&v.to_be_bytes());
        }
        out.extend_from_slice(&self.screen_w.to_be_bytes());
        out.extend_from_slice(&self.screen_h.to_be_bytes());
        out.extend_from_slice(&self.pts_us.to_be_bytes());
        out.extend_from_slice(&compressed_len.to_be_bytes());
    }

    /// Parse [`encode`](Self::encode)'s output: `(header, compressed_len)`.
    pub fn decode(p: &[u8]) -> Result<(Self, u32), String> {
        if p.len() != VIDEO_HEADER_LEN {
            return Err(format!(
                "VIDEO_BEGIN payload is {} bytes, want {VIDEO_HEADER_LEN}",
                p.len()
            ));
        }
        let u16_at = |i: usize| u16::from_be_bytes([p[i], p[i + 1]]);
        let u32_at = |i: usize| u32::from_be_bytes(p[i..i + 4].try_into().unwrap());
        let u64_at = |i: usize| u64::from_be_bytes(p[i..i + 8].try_into().unwrap());
        let h = VideoHeader {
            seq: u64_at(0),
            flags: p[8],
            codec: p[9],
            rect: TileRect {
                x: u16_at(10),
                y: u16_at(12),
                w: u16_at(14),
                h: u16_at(16),
            },
            screen_w: u32_at(18),
            screen_h: u32_at(22),
            pts_us: u64_at(26),
        };
        if h.codec != CODEC_H264 {
            return Err(format!("VIDEO_BEGIN has an unknown codec {}", h.codec));
        }
        if !h.rect.fits(h.screen_w, h.screen_h) {
            return Err(format!(
                "VIDEO_BEGIN rect {:?} outside {}x{}",
                h.rect, h.screen_w, h.screen_h
            ));
        }
        Ok((h, u32_at(34)))
    }
}

/// Microseconds since the UNIX epoch (the `pts_us` clock).
pub fn unix_micros() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Annex-B / AVCC
// ---------------------------------------------------------------------------

/// NAL unit types we care about.
pub const NAL_SLICE: u8 = 1;
pub const NAL_IDR: u8 = 5;
pub const NAL_SEI: u8 = 6;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
pub const NAL_AUD: u8 = 9;

/// The NAL units of an Annex-B byte stream (start codes stripped; trailing
/// zero bytes of a unit belong to the next start code and are dropped).
pub fn annexb_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new(); // (start-code begin, payload begin)
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let b = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
            starts.push((b, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &(_, p)) in starts.iter().enumerate() {
        let mut end = starts.get(k + 1).map_or(data.len(), |&(b, _)| b);
        while end > p && data[end - 1] == 0 {
            end -= 1;
        }
        if end > p {
            out.push(&data[p..end]);
        }
    }
    out
}

/// `nal_unit_type` of a NAL unit (first byte & 0x1F).
pub fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |b| b & 0x1F)
}

/// Whether an Annex-B access unit contains an IDR slice.
pub fn annexb_is_idr(data: &[u8]) -> bool {
    annexb_nals(data).iter().any(|n| nal_type(n) == NAL_IDR)
}

/// One access unit split for a decoder that wants parameter sets out of band
/// (VideoToolbox): SPS/PPS (first of each, if present) and the remaining NAL
/// units as AVCC — each prefixed by its 4-byte big-endian length. Access
/// unit delimiters are dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccessUnit {
    pub sps: Option<Vec<u8>>,
    pub pps: Option<Vec<u8>>,
    pub avcc: Vec<u8>,
    pub idr: bool,
}

impl AccessUnit {
    pub fn from_annexb(data: &[u8]) -> Self {
        let mut au = AccessUnit::default();
        for nal in annexb_nals(data) {
            match nal_type(nal) {
                NAL_SPS => {
                    au.sps.get_or_insert_with(|| nal.to_vec());
                }
                NAL_PPS => {
                    au.pps.get_or_insert_with(|| nal.to_vec());
                }
                NAL_AUD => {}
                t => {
                    au.idr |= t == NAL_IDR;
                    au.avcc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                    au.avcc.extend_from_slice(nal);
                }
            }
        }
        au
    }
}

/// Encoder-side helper: remembers the last SPS/PPS seen and prepends them to
/// an IDR access unit that lacks them (some encoders only emit parameter
/// sets with the first IDR, not with a forced one), so every keyframe on the
/// wire is self-contained.
#[derive(Debug, Clone, Default)]
pub struct ParamSetCache {
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

impl ParamSetCache {
    pub fn fix(&mut self, au: Vec<u8>) -> Vec<u8> {
        // Section 4: every SPS on the wire signals BT.709 limited range
        // (what `bgra_to_nv12` produces), whatever the encoder wrote.
        let au = sps::fix_annexb(&au).unwrap_or(au);
        let nals = annexb_nals(&au);
        let mut has_sps = false;
        let mut has_pps = false;
        let mut idr = false;
        for n in &nals {
            match nal_type(n) {
                NAL_SPS => {
                    has_sps = true;
                    self.sps = Some(n.to_vec());
                }
                NAL_PPS => {
                    has_pps = true;
                    self.pps = Some(n.to_vec());
                }
                NAL_IDR => idr = true,
                _ => {}
            }
        }
        if !idr || (has_sps && has_pps) {
            return au;
        }
        let (Some(sps), Some(pps)) = (&self.sps, &self.pps) else {
            return au;
        };
        let mut out = Vec::with_capacity(au.len() + sps.len() + pps.len() + 8);
        for ps in [sps, pps] {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(ps);
        }
        out.extend_from_slice(&au);
        out
    }
}

// ---------------------------------------------------------------------------
// Colour conversion (BT.709, limited range — what the encoder is told)
// ---------------------------------------------------------------------------

/// NV12 buffer size for a `w x h` picture (w, h even).
pub fn nv12_len(w: u32, h: u32) -> usize {
    let (w, h) = (w as usize, h as usize);
    w * h + w * h / 2
}

/// Worker threads for a colour conversion of `pixels` (a 1080p frame is
/// ~2 ms single-threaded; bands make it ~4x cheaper).
fn conv_threads(pixels: usize) -> usize {
    if pixels < 300_000 {
        1
    } else {
        std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .clamp(1, 4)
    }
}

/// Convert a `w x h` BGRA region (`stride` bytes per row) to tight NV12
/// (BT.709 limited range). `w` and `h` must be even.
pub fn bgra_to_nv12(src: &[u8], stride: usize, w: u32, h: u32, out: &mut Vec<u8>) {
    let (wu, hu) = (w as usize, h as usize);
    out.resize(nv12_len(w, h), 0);
    let (yp, uvp) = out.split_at_mut(wu * hu);
    let pairs = hu / 2;
    let threads = conv_threads(wu * hu);
    if threads <= 1 || pairs < threads {
        bgra_to_nv12_band(src, stride, wu, 0, yp, uvp);
        return;
    }
    let per = pairs.div_ceil(threads);
    std::thread::scope(|sc| {
        for (i, (yb, uvb)) in yp
            .chunks_mut(per * 2 * wu)
            .zip(uvp.chunks_mut(per * wu))
            .enumerate()
        {
            sc.spawn(move || bgra_to_nv12_band(src, stride, wu, i * per, yb, uvb));
        }
    });
}

/// Rows `2*y2_0..` of [`bgra_to_nv12`]: `yp` holds whole row pairs, `uvp`
/// the matching chroma rows.
fn bgra_to_nv12_band(
    src: &[u8],
    stride: usize,
    wu: usize,
    y2_0: usize,
    yp: &mut [u8],
    uvp: &mut [u8],
) {
    for (k, (ypair, uv)) in yp.chunks_mut(2 * wu).zip(uvp.chunks_mut(wu)).enumerate() {
        let y2 = y2_0 + k;
        let r0 = &src[(2 * y2) * stride..(2 * y2) * stride + wu * 4];
        let r1 = &src[(2 * y2 + 1) * stride..(2 * y2 + 1) * stride + wu * 4];
        let (ya, yb) = ypair.split_at_mut(wu);
        for x2 in 0..wu / 2 {
            let mut sr = 0i32;
            let mut sg = 0i32;
            let mut sb = 0i32;
            for (row, yrow) in [(r0, &mut *ya), (r1, &mut *yb)] {
                for dx in 0..2 {
                    let i = (2 * x2 + dx) * 4;
                    let (b, g, r) = (
                        i32::from(row[i]),
                        i32::from(row[i + 1]),
                        i32::from(row[i + 2]),
                    );
                    yrow[2 * x2 + dx] = (((47 * r + 157 * g + 16 * b + 128) >> 8) + 16) as u8;
                    sr += r;
                    sg += g;
                    sb += b;
                }
            }
            // Averages of the 2x2 block, kept x4 for precision.
            let u = ((-26 * sr - 87 * sg + 112 * sb + 512) >> 10) + 128;
            let v = ((112 * sr - 102 * sg - 10 * sb + 512) >> 10) + 128;
            uv[2 * x2] = u.clamp(0, 255) as u8;
            uv[2 * x2 + 1] = v.clamp(0, 255) as u8;
        }
    }
}

/// Convert NV12 (`y_stride` bytes per luma row; the chroma plane starts at
/// `uv_offset` with the same stride) to a tight `w x h` BGRA picture (`w`,
/// `h` even). Errors (never panics) on a buffer too small for the geometry.
pub fn nv12_to_bgra(
    nv12: &[u8],
    y_stride: usize,
    uv_offset: usize,
    w: u32,
    h: u32,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let (wu, hu) = (w as usize, h as usize);
    if wu == 0 || hu == 0 || wu % 2 != 0 || hu % 2 != 0 {
        return Err(format!("NV12 picture {w}x{h} must be even and non-empty"));
    }
    if y_stride < wu
        || uv_offset < y_stride * hu
        || nv12.len() < uv_offset + y_stride * (hu / 2 - 1) + wu
    {
        return Err(format!(
            "NV12 buffer too small for {w}x{h} (stride {y_stride})"
        ));
    }
    out.resize(wu * hu * 4, 0);
    let threads = conv_threads(wu * hu);
    if threads <= 1 {
        nv12_to_bgra_band(nv12, y_stride, uv_offset, wu, 0, out);
        return Ok(());
    }
    // Bands of an even number of rows (chroma rows are shared by pairs).
    let per = (hu / 2).div_ceil(threads) * 2;
    std::thread::scope(|sc| {
        for (i, band) in out.chunks_mut(per * wu * 4).enumerate() {
            sc.spawn(move || nv12_to_bgra_band(nv12, y_stride, uv_offset, wu, i * per, band));
        }
    });
    Ok(())
}

/// Rows `y0..` of [`nv12_to_bgra`] into `out` (whole BGRA rows).
fn nv12_to_bgra_band(
    nv12: &[u8],
    y_stride: usize,
    uv_offset: usize,
    wu: usize,
    y0: usize,
    out: &mut [u8],
) {
    let clamp = |v: i32| (v >> 10).clamp(0, 255) as u8;
    for (k, orow) in out.chunks_mut(wu * 4).enumerate() {
        let y = y0 + k;
        let yrow = &nv12[y * y_stride..y * y_stride + wu];
        let uo = uv_offset + (y / 2) * y_stride;
        let uvrow = &nv12[uo..uo + wu];
        for (x, &yv) in yrow.iter().enumerate() {
            let c = 1192 * (i32::from(yv) - 16);
            let ci = x & !1;
            let u = i32::from(uvrow[ci]) - 128;
            let v = i32::from(uvrow[ci + 1]) - 128;
            let o = x * 4;
            orow[o] = clamp(c + 2163 * u);
            orow[o + 1] = clamp(c - 218 * u - 546 * v);
            orow[o + 2] = clamp(c + 1836 * v);
            orow[o + 3] = 255;
        }
    }
}

/// PSNR (dB) over the B, G, R channels of two equal-size BGRA buffers
/// (`f64::INFINITY` when identical).
pub fn psnr_bgra(a: &[u8], b: &[u8]) -> f64 {
    let mut se = 0u64;
    let mut n = 0u64;
    for (pa, pb) in a.chunks_exact(4).zip(b.chunks_exact(4)) {
        for c in 0..3 {
            let d = i64::from(pa[c]) - i64::from(pb[c]);
            se += (d * d) as u64;
        }
        n += 3;
    }
    if se == 0 || n == 0 {
        return f64::INFINITY;
    }
    let mse = se as f64 / n as f64;
    10.0 * (255.0f64 * 255.0 / mse).log10()
}

// ---------------------------------------------------------------------------
// Codec traits
// ---------------------------------------------------------------------------

/// One encoded access unit (Annex-B).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedAu {
    pub data: Vec<u8>,
    pub keyframe: bool,
    /// The `pts_us` of the input picture this unit encodes.
    pub pts_us: u64,
}

/// A video encoder (the host's H.264 MFT; a fake in tests).
pub trait VideoEncode {
    /// Encode one `w x h` BGRA picture (`stride` bytes per row; the size was
    /// fixed at creation). May return zero units (pipeline latency) or more
    /// than one. `force_idr` asks for an IDR now.
    fn encode(
        &mut self,
        bgra: &[u8],
        stride: usize,
        force_idr: bool,
        pts_us: u64,
    ) -> Result<Vec<EncodedAu>, String>;
    /// Retarget the mean bitrate (bits/s); best effort.
    fn set_bitrate(&mut self, bps: u32);
    /// Section 5: encode the `rect` crop `(x, y, w, h)` of a GPU-resident
    /// capture (`frame` is the platform's surface type, e.g.
    /// `mfcodec::GpuFrame`). `None` = not supported for this frame: the
    /// caller falls back to [`encode`](Self::encode) with the CPU pixels.
    fn encode_gpu(
        &mut self,
        frame: &dyn std::any::Any,
        rect: (u32, u32, u32, u32),
        force_idr: bool,
        pts_us: u64,
    ) -> Option<Result<Vec<EncodedAu>, String>> {
        let _ = (frame, rect, force_idr, pts_us);
        None
    }
    /// Short description for logs ("NVIDIA H.264 Encoder MFT (hw)").
    fn name(&self) -> String;
}

/// Builds an encoder for `(w, h, fps, bitrate)`.
pub type EncoderFactory =
    Box<dyn FnMut(u32, u32, u32, u32) -> Result<Box<dyn VideoEncode>, String>>;

/// A video decoder (the desktop viewer's H.264 MFT).
pub trait VideoDecode {
    /// Decode one Annex-B access unit of a `w x h` picture into tight BGRA
    /// `out` (`w * h * 4`). `Ok(true)` when a picture came out.
    fn decode(&mut self, annexb: &[u8], w: u32, h: u32, out: &mut Vec<u8>) -> Result<bool, String>;
}

// ---------------------------------------------------------------------------
// Motion detection (host)
// ---------------------------------------------------------------------------

/// Thresholds of the [`MotionDetector`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MotionConfig {
    /// A compact dirty region at least this many pixels (bounding box) is
    /// "large" (320x240).
    pub min_area_px: u32,
    /// A sample dirtying at least this fraction of all tiles is "large"
    /// whatever its shape.
    pub screen_frac: f32,
    /// Dirty tiles / bounding-box tiles needed for a compact region (keeps
    /// scattered small updates — a clock and a caret — out).
    pub density: f32,
    /// Large samples must keep coming this long before motion starts.
    pub enter_after: Duration,
    /// Longest gap between large samples that still counts as continuous
    /// (a 24 fps video sampled at 30 fps has repeated frames).
    pub max_gap: Duration,
    /// The motion region must stay still this long before tiles return.
    pub exit_after: Duration,
    /// A motion rect covering this fraction of the screen snaps to the whole
    /// screen (one stable encoder size).
    pub whole_screen_frac: f32,
    /// Minimum time between two rect growths (each one restarts the
    /// encoder with an IDR).
    pub grow_every: Duration,
}

impl Default for MotionConfig {
    fn default() -> Self {
        Self {
            min_area_px: 320 * 240,
            screen_frac: 0.25,
            density: 0.5,
            // Wave 1: 65 ms (was 150): three samples at 30 fps (0, 33,
            // 66 ms) — a video or a drag switches to H.264 before many
            // lossless frames of moving content are sent.
            enter_after: Duration::from_millis(65),
            max_gap: Duration::from_millis(250),
            exit_after: Duration::from_secs(1),
            whole_screen_frac: 0.6,
            grow_every: Duration::from_secs(1),
        }
    }
}

/// What a [`MotionDetector`] sample changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotionEvent {
    None,
    /// Motion started in this (tile-aligned) rect.
    Enter(TileRect),
    /// The motion rect grew to this rect.
    Grow(TileRect),
    /// Motion in this rect stopped.
    Exit(TileRect),
}

/// A box in tile units, `[c0, c1) x [r0, r1)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TBox {
    c0: u32,
    r0: u32,
    c1: u32,
    r1: u32,
}

impl TBox {
    fn tiles(&self) -> usize {
        ((self.c1 - self.c0) * (self.r1 - self.r0)) as usize
    }
    fn union(&self, o: &TBox) -> TBox {
        TBox {
            c0: self.c0.min(o.c0),
            r0: self.r0.min(o.r0),
            c1: self.c1.max(o.c1),
            r1: self.r1.max(o.r1),
        }
    }
    fn contains(&self, o: &TBox) -> bool {
        o.c0 >= self.c0 && o.r0 >= self.r0 && o.c1 <= self.c1 && o.r1 <= self.r1
    }
    fn px_rect(&self, w: u32, h: u32) -> TileRect {
        let x0 = (self.c0 * TILE).min(w);
        let y0 = (self.r0 * TILE).min(h);
        let x1 = (self.c1 * TILE).min(w);
        let y1 = (self.r1 * TILE).min(h);
        TileRect {
            x: x0 as u16,
            y: y0 as u16,
            w: (x1 - x0) as u16,
            h: (y1 - y0) as u16,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum MotionState {
    Idle {
        /// First large sample of the current run.
        since: Option<Instant>,
        last: Option<Instant>,
        acc: Option<TBox>,
        samples: u32,
    },
    Motion {
        bx: TBox,
        last_motion: Instant,
        last_grow: Instant,
    },
}

/// `b`, or the whole screen when it covers enough of it.
fn snap_box(cfg: &MotionConfig, b: TBox, cols: u32, rows: u32) -> TBox {
    if b.tiles() as f32 >= (cols * rows) as f32 * cfg.whole_screen_frac {
        TBox {
            c0: 0,
            r0: 0,
            c1: cols,
            r1: rows,
        }
    } else {
        b
    }
}

/// Host-side hysteresis state machine: feeds on the per-capture dirty-tile
/// map (consecutive captures) and reports when a large region starts moving,
/// grows, or comes to rest. Pure; the caller passes `now`.
#[derive(Debug, Clone)]
pub struct MotionDetector {
    cfg: MotionConfig,
    state: MotionState,
    dims: (u32, u32),
}

impl Default for MotionDetector {
    fn default() -> Self {
        Self::new(MotionConfig::default())
    }
}

impl MotionDetector {
    pub fn new(cfg: MotionConfig) -> Self {
        Self {
            cfg,
            state: MotionState::Idle {
                since: None,
                last: None,
                acc: None,
                samples: 0,
            },
            dims: (0, 0),
        }
    }

    pub fn config(&self) -> &MotionConfig {
        &self.cfg
    }

    /// Back to idle (no event).
    pub fn reset(&mut self) {
        self.state = MotionState::Idle {
            since: None,
            last: None,
            acc: None,
            samples: 0,
        };
    }

    /// The current motion rect, if in motion.
    pub fn motion_rect(&self) -> Option<TileRect> {
        match self.state {
            MotionState::Motion { bx, .. } => Some(bx.px_rect(self.dims.0, self.dims.1)),
            MotionState::Idle { .. } => None,
        }
    }

    /// Feed one capture's dirty-tile map (tiles that changed since the
    /// previous capture) of a `w x h` screen.
    pub fn observe(&mut self, now: Instant, map: &TileMap, w: u32, h: u32) -> MotionEvent {
        if map.is_empty() || w == 0 || h == 0 {
            return MotionEvent::None;
        }
        if self.dims != (w, h) {
            // A new geometry: whatever was moving is gone.
            let was = self.motion_rect();
            self.dims = (w, h);
            self.reset();
            if let Some(r) = was {
                return MotionEvent::Exit(r);
            }
        }
        let (cols, rows) = (map.cols(), map.rows());
        let cfg = self.cfg;
        // Dirty count + bounding box.
        let mut count = 0usize;
        let mut bb: Option<TBox> = None;
        for r in 0..rows {
            for c in 0..cols {
                if map.get(c, r) {
                    count += 1;
                    let t = TBox {
                        c0: c,
                        r0: r,
                        c1: c + 1,
                        r1: r + 1,
                    };
                    bb = Some(bb.map_or(t, |b| b.union(&t)));
                }
            }
        }
        let tile_px = (TILE * TILE) as f32;
        let large = bb.is_some_and(|b| {
            let n = count as f32;
            n >= map.len() as f32 * cfg.screen_frac
                || (b.tiles() as f32 * tile_px >= cfg.min_area_px as f32
                    && n >= b.tiles() as f32 * cfg.density
                    && n * tile_px * 2.0 >= cfg.min_area_px as f32)
        });
        match &mut self.state {
            MotionState::Idle {
                since,
                last,
                acc,
                samples,
            } => {
                if large {
                    let b = bb.expect("large implies a box");
                    let fresh_run =
                        last.is_none_or(|l| now.saturating_duration_since(l) > cfg.max_gap);
                    if fresh_run {
                        *since = Some(now);
                        *acc = Some(b);
                        *samples = 1;
                    } else {
                        *acc = Some(acc.map_or(b, |a| a.union(&b)));
                        *samples += 1;
                    }
                    *last = Some(now);
                    let run = since.map_or(Duration::ZERO, |s| now.saturating_duration_since(s));
                    if run >= cfg.enter_after && *samples >= 3 {
                        let bx = snap_box(&cfg, acc.expect("set above"), cols, rows);
                        self.state = MotionState::Motion {
                            bx,
                            last_motion: now,
                            last_grow: now,
                        };
                        return MotionEvent::Enter(bx.px_rect(w, h));
                    }
                } else if last.is_some_and(|l| now.saturating_duration_since(l) > cfg.max_gap) {
                    *since = None;
                    *last = None;
                    *acc = None;
                    *samples = 0;
                }
                MotionEvent::None
            }
            MotionState::Motion {
                bx,
                last_motion,
                last_grow,
            } => {
                let mut inside = 0usize;
                for r in bx.r0..bx.r1.min(rows) {
                    for c in bx.c0..bx.c1.min(cols) {
                        inside += usize::from(map.get(c, r));
                    }
                }
                // A blinking caret inside the region is not motion.
                let min_inside = if bx.tiles() <= 4 { 1 } else { 2 };
                if inside >= min_inside {
                    *last_motion = now;
                }
                if let Some(b) = bb {
                    if large
                        && !bx.contains(&b)
                        && now.saturating_duration_since(*last_grow) >= cfg.grow_every
                    {
                        let grown = snap_box(&cfg, bx.union(&b), cols, rows);
                        *bx = grown;
                        *last_grow = now;
                        *last_motion = now;
                        return MotionEvent::Grow(grown.px_rect(w, h));
                    }
                }
                if now.saturating_duration_since(*last_motion) >= cfg.exit_after {
                    let r = bx.px_rect(w, h);
                    self.reset();
                    return MotionEvent::Exit(r);
                }
                MotionEvent::None
            }
        }
    }
}

/// Dirty tiles of `map` inside the pixel rect `r`.
pub fn tiles_in_rect(map: &TileMap, r: &TileRect) -> usize {
    if map.is_empty() || r.w == 0 || r.h == 0 {
        return 0;
    }
    let c0 = u32::from(r.x) / TILE;
    let r0 = u32::from(r.y) / TILE;
    let c1 = (u32::from(r.x) + u32::from(r.w))
        .div_ceil(TILE)
        .min(map.cols());
    let r1 = (u32::from(r.y) + u32::from(r.h))
        .div_ceil(TILE)
        .min(map.rows());
    let mut n = 0;
    for row in r0..r1 {
        for col in c0..c1 {
            n += usize::from(map.get(col, row));
        }
    }
    n
}

/// Wave 1: H.264 encoder sizes are rounded up to this (px) so a growing
/// motion rect rarely forces a new encoder.
pub const ENCODER_BUCKET: u32 = 128;

/// The even-sized coded rect for a motion rect (H.264 4:2:0 needs even
/// dimensions; at an odd screen edge the last row/column stays with tiles).
pub fn coded_rect(r: TileRect) -> TileRect {
    TileRect {
        x: r.x,
        y: r.y,
        w: r.w & !1,
        h: r.h & !1,
    }
}

// ---------------------------------------------------------------------------
// Host video session
// ---------------------------------------------------------------------------

/// Counters of a [`VideoSession`].
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct VideoStats {
    pub frames: u64,
    pub keyframes: u64,
    pub bytes: u64,
    pub encode_time: Duration,
    pub enters: u64,
    pub grows: u64,
    pub exits: u64,
    /// Captures not encoded because the video queue was full.
    pub skipped: u64,
    pub encoder_failures: u64,
}

/// Host-side H.264 state: drives the [`MotionDetector`], owns the encoder
/// (created lazily through the factory, cached across motion periods of
/// the same size) and queues access units / END markers in the
/// [`FrameMailbox`] video FIFO.
///
/// Per capture the host calls, in order:
/// 1. `UpdateEncoder::submit_masked(.., mask = active_rect(), ..)`,
/// 2. [`on_capture`](Self::on_capture) with the encoder's fresh-tile map,
/// 3. when [`take_refresh`](Self::take_refresh) yields a rect, `submit_masked`
///    again with it as the forced refresh (lossless, after `VIDEO_END`).
pub struct VideoSession {
    detector: MotionDetector,
    factory: EncoderFactory,
    enc: Option<(u32, u32, Box<dyn VideoEncode>)>,
    /// Tile-aligned motion rect (the mask), while in motion.
    active: Option<TileRect>,
    /// The screen size `active` refers to.
    active_screen: (u32, u32),
    seq: u64,
    want_idr: bool,
    refresh: Option<TileRect>,
    retry_at: Option<Instant>,
    fps: u32,
    /// The pacer's delivery-rate estimate (bytes/s), if any.
    link_rate: Option<f64>,
    applied_bitrate: u32,
    stats: VideoStats,
    last_error: Option<String>,
    /// Section 5: the GPU copy of the current capture (see
    /// [`set_gpu_frame`](Self::set_gpu_frame)).
    gpu_frame: Option<Box<dyn std::any::Any>>,
    /// Bits per pixel per frame of the area-based bitrate (default 0.2).
    bpp: f64,
}

impl VideoSession {
    pub fn new(factory: EncoderFactory, fps: u32) -> Self {
        Self::with_config(factory, fps, MotionConfig::default())
    }

    pub fn with_config(factory: EncoderFactory, fps: u32, cfg: MotionConfig) -> Self {
        Self {
            detector: MotionDetector::new(cfg),
            factory,
            enc: None,
            active: None,
            active_screen: (0, 0),
            seq: 0,
            want_idr: false,
            refresh: None,
            retry_at: None,
            fps: fps.max(1),
            link_rate: None,
            applied_bitrate: 0,
            stats: VideoStats::default(),
            last_error: None,
            gpu_frame: None,
            bpp: 0.2,
        }
    }

    /// Wave 3: the area-based bitrate budget in bits per pixel per frame
    /// (default 0.2; benches raise it).
    pub fn set_bits_per_pixel(&mut self, bpp: f64) {
        if bpp.is_finite() && bpp > 0.0 {
            self.bpp = bpp;
        }
    }

    /// The region currently streamed as video (mask it out of tile updates).
    pub fn active_rect(&self) -> Option<TileRect> {
        self.active
    }

    /// A region that left video mode and needs a lossless tile refresh.
    pub fn take_refresh(&mut self) -> Option<TileRect> {
        self.refresh.take()
    }

    /// Next encoded picture is an IDR (keyframe / IDR request).
    pub fn request_idr(&mut self) {
        if self.active.is_some() {
            self.want_idr = true;
        }
    }

    /// The link pacer's delivery-rate estimate (bytes/s; `None` = none
    /// yet): the encoder's bitrate follows [`target_bitrate`] of it.
    pub fn set_link_rate(&mut self, bytes_per_sec: Option<f64>) {
        self.link_rate = bytes_per_sec;
    }

    /// The bitrate the encoder was last set to (bits/s; 0 = no encoder).
    pub fn bitrate(&self) -> u32 {
        self.applied_bitrate
    }

    /// Section 5: the GPU-resident copy of the next capture (the same
    /// pixels as `on_capture`'s `px`), used by encoders that take
    /// [`VideoEncode::encode_gpu`]. `None` = CPU pixels only.
    pub fn set_gpu_frame(&mut self, frame: Option<Box<dyn std::any::Any>>) {
        self.gpu_frame = frame;
    }

    pub fn set_fps(&mut self, fps: u32) {
        self.fps = fps.max(1);
    }

    pub fn stats(&self) -> VideoStats {
        self.stats
    }

    /// The encoder in use (for logs).
    pub fn encoder_name(&self) -> Option<String> {
        self.enc.as_ref().map(|(_, _, e)| e.name())
    }

    /// The last encoder error (cleared when read).
    pub fn take_error(&mut self) -> Option<String> {
        self.last_error.take()
    }

    /// Stop video mode (monitor switch, viewer without H.264, ...): queue
    /// `VIDEO_END`; with `refresh` the region also gets a lossless refresh.
    pub fn stop(&mut self, mailbox: &FrameMailbox, refresh: bool) {
        self.detector.reset();
        self.end(mailbox, refresh);
    }

    fn end(&mut self, mailbox: &FrameMailbox, refresh: bool) {
        let Some(r) = self.active.take() else {
            return;
        };
        let (sw, sh) = self.active_screen;
        let header = VideoHeader {
            seq: self.seq,
            flags: VIDEO_END,
            codec: CODEC_H264,
            rect: r,
            screen_w: sw,
            screen_h: sh,
            pts_us: unix_micros(),
        };
        self.seq = self.seq.wrapping_add(1);
        mailbox.push_video(EncodedFrame::video(header, Vec::new()));
        if refresh {
            self.refresh = Some(r);
        }
        self.want_idr = false;
        self.stats.exits += 1;
    }

    /// One capture: `px` is the `w x h` screen (`stride` bytes per row),
    /// `fresh` the tiles that changed since the previous capture
    /// (`fresh_valid` false when it was force-filled, e.g. a keyframe).
    /// `enabled` = the viewer advertised H.264. Returns whether a video
    /// picture was encoded (the capture counts as a change).
    #[allow(clippy::too_many_arguments)]
    pub fn on_capture(
        &mut self,
        now: Instant,
        px: &[u8],
        w: u32,
        h: u32,
        stride: usize,
        fresh: &TileMap,
        fresh_valid: bool,
        enabled: bool,
        mailbox: &FrameMailbox,
    ) -> bool {
        if !enabled {
            if self.active.is_some() {
                self.stop(mailbox, true);
            }
            return false;
        }
        let ev = if fresh_valid {
            self.detector.observe(now, fresh, w, h)
        } else {
            if self.detector.dims != (w, h) {
                // Forced (new geometry): the old region is meaningless; the
                // full frame that went out covers everything. (END first: its
                // rect refers to the old screen.)
                self.end(mailbox, false);
                self.detector.dims = (w, h);
                self.detector.reset();
            }
            MotionEvent::None
        };
        match ev {
            MotionEvent::Enter(r) | MotionEvent::Grow(r) => {
                if self.retry_at.is_some_and(|t| now < t) {
                    // The encoder failed recently: stay on tiles.
                    self.detector.reset();
                } else {
                    if matches!(ev, MotionEvent::Enter(_)) {
                        self.stats.enters += 1;
                    } else {
                        self.stats.grows += 1;
                    }
                    self.active = Some(r);
                    self.active_screen = (w, h);
                    self.want_idr = true;
                }
            }
            MotionEvent::Exit(_) => self.end(mailbox, true),
            MotionEvent::None => {}
        }
        let Some(rect) = self.active else {
            return false;
        };
        let changed = !fresh_valid || tiles_in_rect(fresh, &rect) > 0;
        if !changed && !self.want_idr {
            return false;
        }
        if mailbox.video_queued() >= MAX_VIDEO_QUEUE {
            self.stats.skipped += 1;
            return false;
        }
        let need = coded_rect(rect);
        if need.w < 16 || need.h < 16 || px.len() < stride * h as usize {
            return false;
        }
        // Wave 1: the encoder size comes in 128 px buckets and is kept while
        // the motion rect grows inside it (no MF re-open + IDR per Grow);
        // the coded rect is placed to contain the motion rect, and becomes
        // the mask (the margin streams as video too, not as tiles).
        let cr = self.encoder_rect(need, w, h);
        if self.active != Some(cr) {
            self.active = Some(cr);
        }
        let (cw, ch) = (u32::from(cr.w), u32::from(cr.h));
        let b = target_bitrate_bpp(cw, ch, self.fps, self.link_rate, self.bpp);
        if self
            .enc
            .as_ref()
            .is_none_or(|(ew, eh, _)| (*ew, *eh) != (cw, ch))
        {
            self.enc = None;
            match (self.factory)(cw, ch, self.fps, b) {
                Ok(e) => {
                    self.enc = Some((cw, ch, e));
                    self.applied_bitrate = b;
                    self.want_idr = true;
                }
                Err(e) => return self.fail(now, mailbox, e),
            }
        }
        let Some((_, _, enc)) = self.enc.as_mut() else {
            return false;
        };
        if b.abs_diff(self.applied_bitrate) * 100 > self.applied_bitrate.max(1) * 15 {
            enc.set_bitrate(b);
            self.applied_bitrate = b;
        }
        let off = usize::from(cr.y) * stride + usize::from(cr.x) * 4;
        let t0 = Instant::now();
        let force = self.want_idr;
        let ts = unix_micros();
        let rect = (
            u32::from(cr.x),
            u32::from(cr.y),
            u32::from(cr.w),
            u32::from(cr.h),
        );
        let gpu = self
            .gpu_frame
            .as_deref()
            .and_then(|g| enc.encode_gpu(g, rect, force, ts));
        let aus = match gpu.unwrap_or_else(|| enc.encode(&px[off..], stride, force, ts)) {
            Ok(a) => a,
            Err(e) => return self.fail(now, mailbox, e),
        };
        self.stats.encode_time += t0.elapsed();
        if force {
            self.want_idr = false;
        }
        let mut any = false;
        for au in aus {
            let header = VideoHeader {
                seq: self.seq,
                flags: if au.keyframe { VIDEO_KEYFRAME } else { 0 },
                codec: CODEC_H264,
                rect: cr,
                screen_w: w,
                screen_h: h,
                pts_us: au.pts_us,
            };
            self.seq = self.seq.wrapping_add(1);
            self.stats.frames += 1;
            self.stats.keyframes += u64::from(au.keyframe);
            self.stats.bytes += au.data.len() as u64;
            mailbox.push_video(EncodedFrame::video(header, au.data));
            any = true;
        }
        any
    }

    /// The coded rect for motion rect `need` (even, inside the `sw x sh`
    /// screen): the current encoder's size when `need` fits in it (and it
    /// is not wastefully large), else `need` rounded up to 128 px buckets;
    /// placed tile-aligned where possible. Falls back to `need` itself.
    fn encoder_rect(&self, need: TileRect, sw: u32, sh: u32) -> TileRect {
        let bucket =
            |v: u16, s: u32| (u32::from(v).div_ceil(ENCODER_BUCKET) * ENCODER_BUCKET).min(s & !1);
        let (mut ew, mut eh) = (bucket(need.w, sw), bucket(need.h, sh));
        if let Some((w0, h0, _)) = &self.enc {
            let fits =
                *w0 >= u32::from(need.w) && *h0 >= u32::from(need.h) && *w0 <= sw && *h0 <= sh;
            if fits && u64::from(*w0) * u64::from(*h0) <= u64::from(ew) * u64::from(eh) * 2 {
                (ew, eh) = (*w0, *h0);
            }
        }
        let place = |pos: u16, len: u16, e: u32, s: u32| -> Option<u32> {
            let end = u32::from(pos) + u32::from(len);
            let mut x = u32::from(pos) / TILE * TILE;
            if x + e > s {
                x = s.checked_sub(e)?;
            }
            (x <= u32::from(pos) && x + e >= end).then_some(x)
        };
        match (place(need.x, need.w, ew, sw), place(need.y, need.h, eh, sh)) {
            (Some(x), Some(y)) => TileRect {
                x: x as u16,
                y: y as u16,
                w: ew as u16,
                h: eh as u16,
            },
            _ => need,
        }
    }

    fn fail(&mut self, now: Instant, mailbox: &FrameMailbox, e: String) -> bool {
        self.enc = None;
        self.stats.encoder_failures += 1;
        self.last_error = Some(e);
        self.retry_at = Some(now + Duration::from_secs(5));
        self.stop(mailbox, true);
        false
    }
}

/// The video bitrate (bits/s) for a `w x h` region at `fps`, given the
/// pacer's delivery-rate estimate (bytes/s, `None` before one exists): about
/// 0.2 bit per pixel per frame, at most 70% of the measured link, within
/// [0.5, 40] Mbps.
pub fn target_bitrate(w: u32, h: u32, fps: u32, link_bytes_per_sec: Option<f64>) -> u32 {
    target_bitrate_bpp(w, h, fps, link_bytes_per_sec, 0.2)
}

/// [`target_bitrate`] with an explicit bits-per-pixel-per-frame budget
/// (Wave 3 benches raise it to load the link).
pub fn target_bitrate_bpp(
    w: u32,
    h: u32,
    fps: u32,
    link_bytes_per_sec: Option<f64>,
    bpp: f64,
) -> u32 {
    let by_area = f64::from(w) * f64::from(h) * f64::from(fps.max(1)) * bpp;
    let by_link = link_bytes_per_sec.map_or(f64::INFINITY, |r| r * 8.0 * 0.7);
    by_area.min(by_link).clamp(500_000.0, 40_000_000.0) as u32
}

// ---------------------------------------------------------------------------
// Viewer-side sequencing
// ---------------------------------------------------------------------------

/// What a viewer does with a received video unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoVerdict {
    /// Decode it.
    Deliver,
    /// Motion over (no payload).
    End,
    /// Undecodable (gap / waiting for a keyframe): drop it; ask for an IDR.
    Drop,
    /// Wave 3: older than a unit already seen (it took the other channel —
    /// media vs reliable — and lost the race): drop it silently, never paint
    /// stale pictures over newer content.
    Stale,
}

/// Viewer-side continuity check of the video stream: P-frames are only
/// decoded on top of an unbroken chain from a keyframe.
#[derive(Debug, Clone, Default)]
pub struct VideoRx {
    expect: Option<u64>,
    /// Waiting for a keyframe.
    synced: bool,
    pub dropped: u64,
}

impl VideoRx {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn accept(&mut self, h: &VideoHeader) -> VideoVerdict {
        if let Some(e) = self.expect {
            let behind = e.wrapping_sub(h.seq);
            if (1..1 << 20).contains(&behind) {
                return VideoVerdict::Stale;
            }
        }
        let in_order = self.expect == Some(h.seq);
        self.expect = Some(h.seq.wrapping_add(1));
        if h.is_end() {
            // The next motion period starts with an IDR.
            self.synced = false;
            return VideoVerdict::End;
        }
        if h.is_keyframe() {
            self.synced = true;
            return VideoVerdict::Deliver;
        }
        if self.synced && in_order {
            return VideoVerdict::Deliver;
        }
        self.synced = false;
        self.dropped += 1;
        VideoVerdict::Drop
    }

    /// The decoder failed: drop P-frames until the next keyframe.
    pub fn mark_broken(&mut self) {
        self.synced = false;
    }
}

// ---------------------------------------------------------------------------
// Synthetic content (host `--test-pattern`, tests, benches)
// ---------------------------------------------------------------------------

/// A desktop-like static background: light window chrome with dark "text"
/// runs (deterministic).
pub fn synth_desktop(px: &mut [u8], w: u32, h: u32) {
    let stride = w as usize * 4;
    for y in 0..h as usize {
        for x in 0..w as usize {
            let o = y * stride + x * 4;
            let band = (y / 18) % 7;
            let glyph = ((x / 7) * 31 + y / 18 * 17) % 5;
            let ink = band != 0
                && (y % 18) > 4
                && (y % 18) < 14
                && glyph < 3
                && (x % 7) < 5
                && (x / 300) % 2 == 0;
            let v = if ink {
                40
            } else if (x / 300) % 2 == 1 {
                230
            } else {
                250
            };
            px[o] = v;
            px[o + 1] = v;
            px[o + 2] = if ink { 40 } else { v.saturating_sub(5) };
            px[o + 3] = 255;
        }
    }
}

/// Paint frame `t` of a moving, video-like picture into `rect`: drifting
/// gradients, a moving fine texture and a bouncing disc (compressible like
/// real video, different every frame).
pub fn synth_video(px: &mut [u8], w: u32, rect: TileRect, t: u32) {
    let stride = w as usize * 4;
    let (rw, rh) = (u32::from(rect.w), u32::from(rect.h));
    let cx = ((t * 7) % (rw.max(2) * 2)) as i32;
    let cx = if cx >= rw as i32 {
        2 * rw as i32 - cx
    } else {
        cx
    };
    let cy = (f64::from(t) * 0.11)
        .sin()
        .mul_add(f64::from(rh) * 0.35, f64::from(rh) * 0.5) as i32;
    let rad = (rh / 5).max(4) as i32;
    for y in 0..rh {
        let row = (usize::from(rect.y) + y as usize) * stride;
        for x in 0..rw {
            let o = row + (usize::from(rect.x) + x as usize) * 4;
            let dx = x as i32 - cx;
            let dy = y as i32 - cy;
            let disc = dx * dx + dy * dy < rad * rad;
            // A texture panning 2 px per frame (detail the encoder must track).
            let tx = x + 2 * t;
            let texture = ((((tx / 4) ^ (y / 4)) & 3) * 12) as i32;
            let r = ((x + t * 3) % 256) as i32;
            let g = ((y * 2 + t * 2) % 256) as i32;
            let b = (((x + y) / 2 + t * 5) % 256) as i32;
            let (r, g, b) = if disc { (240, 200, 40) } else { (r, g, b) };
            px[o] = (b / 2 + texture).clamp(0, 255) as u8;
            px[o + 1] = (g / 2 + texture).clamp(0, 255) as u8;
            px[o + 2] = (r / 2 + texture).clamp(0, 255) as u8;
            px[o + 3] = 255;
        }
    }
}

/// Frame `t` of a page scrolling up by `speed` pixels per frame inside
/// `rect` (text-like rows on white).
pub fn synth_scroll(px: &mut [u8], w: u32, rect: TileRect, t: u32, speed: u32) {
    let stride = w as usize * 4;
    let off = t.wrapping_mul(speed);
    for y in 0..u32::from(rect.h) {
        let py = y.wrapping_add(off); // page coordinate
        let row = (usize::from(rect.y) + y as usize) * stride;
        let line = py / 22;
        let in_line = py % 22;
        for x in 0..u32::from(rect.w) {
            let o = row + (usize::from(rect.x) + x as usize) * 4;
            let word = (x / 9 + line * 13) % 11;
            let ink = in_line > 5
                && in_line < 17
                && word < 8
                && (x % 9) < 6
                && ((x / 9 * 7 + line * 3 + (x % 9)) % 4) != 0
                && x > 20
                && x + 20 < u32::from(rect.w)
                && (line % 9) != 0;
            let heading = line % 9 == 1;
            let v: u8 = if ink {
                if heading {
                    20
                } else {
                    60
                }
            } else {
                255
            };
            px[o] = if ink && heading { 160 } else { v };
            px[o + 1] = v;
            px[o + 2] = v;
            px[o + 3] = 255;
        }
    }
}

// ---------------------------------------------------------------------------
// SPS VUI colour signalling (Section 4: colour consistency)
// ---------------------------------------------------------------------------

/// H.264 SPS parsing / rewriting for the VUI colour description. The host
/// converts BGRA to NV12 with BT.709 limited-range coefficients
/// ([`bgra_to_nv12`]); decoders that guess (VideoToolbox defaults to BT.601
/// for SD sizes, some for everything) shift every colour unless the SPS
/// says so explicitly. [`sps::colour`] reads what an SPS signals;
/// [`sps::force_bt709_limited`] rewrites one to signal
/// `video_full_range_flag = 0` and colour primaries / transfer / matrix = 1
/// (BT.709), inserting a VUI when the encoder wrote none and keeping every
/// other field (timing, HRD, bitstream restriction) bit-exact.
pub mod sps {
    /// Colour fields of an SPS VUI.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Colour {
        pub vui_present: bool,
        pub video_signal_type_present: bool,
        pub video_format: u8,
        pub full_range: bool,
        pub colour_description_present: bool,
        pub primaries: u8,
        pub transfer: u8,
        pub matrix: u8,
    }

    impl Colour {
        /// BT.709 primaries / transfer / matrix, limited range.
        pub fn is_bt709_limited(&self) -> bool {
            self.video_signal_type_present
                && !self.full_range
                && self.colour_description_present
                && self.primaries == 1
                && self.transfer == 1
                && self.matrix == 1
        }
    }

    /// Summary of an SPS (the fields tests and logs care about).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Summary {
        pub profile_idc: u8,
        pub level_idc: u8,
        pub width_mbs: u32,
        pub height_map_units: u32,
        pub frame_mbs_only: bool,
        pub max_num_ref_frames: u32,
        pub colour: Colour,
        /// `(num_units_in_tick, time_scale)` when timing info is present.
        pub timing: Option<(u32, u32)>,
        /// `max_num_reorder_frames` when the bitstream restriction is present.
        pub max_num_reorder_frames: Option<u32>,
    }

    /// Strip emulation-prevention bytes (`00 00 03` -> `00 00`).
    pub fn unescape(nal: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(nal.len());
        let mut zeros = 0;
        for &b in nal {
            if zeros >= 2 && b == 3 {
                zeros = 0;
                continue;
            }
            zeros = if b == 0 { zeros + 1 } else { 0 };
            out.push(b);
        }
        out
    }

    /// Insert emulation-prevention bytes.
    pub fn escape(rbsp: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(rbsp.len() + 8);
        let mut zeros = 0;
        for &b in rbsp {
            if zeros >= 2 && b <= 3 {
                out.push(3);
                zeros = 0;
            }
            zeros = if b == 0 { zeros + 1 } else { 0 };
            out.push(b);
        }
        out
    }

    struct Reader<'a> {
        d: &'a [u8],
        pos: usize,
    }

    impl Reader<'_> {
        fn bit(&mut self) -> Option<u32> {
            let byte = *self.d.get(self.pos / 8)?;
            let b = (byte >> (7 - self.pos % 8)) & 1;
            self.pos += 1;
            Some(u32::from(b))
        }
        fn bits(&mut self, n: u32) -> Option<u32> {
            let mut v = 0u32;
            for _ in 0..n {
                v = (v << 1) | self.bit()?;
            }
            Some(v)
        }
        fn flag(&mut self) -> Option<bool> {
            Some(self.bit()? == 1)
        }
        fn ue(&mut self) -> Option<u32> {
            let mut zeros = 0;
            while self.bit()? == 0 {
                zeros += 1;
                if zeros > 31 {
                    return None;
                }
            }
            Some(((1u64 << zeros) - 1 + u64::from(self.bits(zeros)?)) as u32)
        }
        fn se(&mut self) -> Option<i32> {
            let k = self.ue()?;
            Some(if k % 2 == 1 {
                (k as i32 + 1) / 2
            } else {
                -((k / 2) as i32)
            })
        }
    }

    /// A bit writer.
    #[derive(Default)]
    pub struct Writer {
        out: Vec<u8>,
        n: usize,
    }

    impl Writer {
        pub fn bit(&mut self, b: u32) {
            if self.n.is_multiple_of(8) {
                self.out.push(0);
            }
            if b & 1 == 1 {
                let i = self.out.len() - 1;
                self.out[i] |= 1 << (7 - self.n % 8);
            }
            self.n += 1;
        }
        pub fn bits(&mut self, v: u32, n: u32) {
            for i in (0..n).rev() {
                self.bit((v >> i) & 1);
            }
        }
        pub fn ue(&mut self, v: u32) {
            let x = u64::from(v) + 1;
            let len = 64 - x.leading_zeros();
            self.bits(0, len - 1);
            for i in (0..len).rev() {
                self.bit(((x >> i) & 1) as u32);
            }
        }
        pub fn se(&mut self, v: i32) {
            self.ue(if v > 0 {
                (2 * v - 1) as u32
            } else {
                (-2 * v) as u32
            });
        }
        /// Copy bits `[from, to)` of `src`.
        fn copy(&mut self, src: &[u8], from: usize, to: usize) {
            for p in from..to {
                self.bit(u32::from((src[p / 8] >> (7 - p % 8)) & 1));
            }
        }
        /// `rbsp_trailing_bits` and the bytes.
        pub fn finish(mut self) -> Vec<u8> {
            self.bit(1);
            while !self.n.is_multiple_of(8) {
                self.bit(0);
            }
            self.out
        }
    }

    /// Bit positions of the parsed SPS (in the unescaped RBSP, after the
    /// one-byte NAL header).
    struct Layout {
        summary: Summary,
        /// Position of `vui_parameters_present_flag`.
        vui_flag: usize,
        /// Colour block `[video_signal_type_present_flag .. end)` inside the VUI.
        signal: Option<(usize, usize)>,
        /// End of the VUI.
        vui_end: usize,
    }

    fn skip_scaling_list(r: &mut Reader, size: usize) -> Option<()> {
        let (mut last, mut next) = (8i32, 8i32);
        for _ in 0..size {
            if next != 0 {
                let delta = r.se()?;
                next = (last + delta + 256) % 256;
            }
            last = if next == 0 { last } else { next };
        }
        Some(())
    }

    fn skip_hrd(r: &mut Reader) -> Option<()> {
        let cnt = r.ue()?;
        if cnt > 31 {
            return None;
        }
        r.bits(8)?; // bit_rate_scale + cpb_size_scale
        for _ in 0..=cnt {
            r.ue()?;
            r.ue()?;
            r.bit()?;
        }
        r.bits(20)?; // four 5-bit lengths
        Some(())
    }

    fn parse(rbsp: &[u8]) -> Option<Layout> {
        let mut r = Reader { d: rbsp, pos: 8 };
        let profile_idc = r.bits(8)? as u8;
        r.bits(8)?; // constraint flags
        let level_idc = r.bits(8)? as u8;
        r.ue()?; // seq_parameter_set_id
        if matches!(
            profile_idc,
            100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
        ) {
            let chroma = r.ue()?;
            if chroma == 3 {
                r.bit()?;
            }
            r.ue()?;
            r.ue()?;
            r.bit()?;
            if r.flag()? {
                for i in 0..if chroma == 3 { 12 } else { 8 } {
                    if r.flag()? {
                        skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
                    }
                }
            }
        }
        r.ue()?; // log2_max_frame_num_minus4
        match r.ue()? {
            0 => {
                r.ue()?;
            }
            1 => {
                r.bit()?;
                r.se()?;
                r.se()?;
                let n = r.ue()?;
                if n > 255 {
                    return None;
                }
                for _ in 0..n {
                    r.se()?;
                }
            }
            2 => {}
            _ => return None,
        }
        let max_num_ref_frames = r.ue()?;
        r.bit()?;
        let width_mbs = r.ue()? + 1;
        let height_map_units = r.ue()? + 1;
        let frame_mbs_only = r.flag()?;
        if !frame_mbs_only {
            r.bit()?;
        }
        r.bit()?; // direct_8x8_inference
        if r.flag()? {
            for _ in 0..4 {
                r.ue()?;
            }
        }
        let vui_flag = r.pos;
        let vui_present = r.flag()?;
        let mut colour = Colour {
            vui_present,
            video_signal_type_present: false,
            video_format: 5,
            full_range: false,
            colour_description_present: false,
            primaries: 2,
            transfer: 2,
            matrix: 2,
        };
        let mut signal = None;
        let mut timing = None;
        let mut reorder = None;
        if vui_present {
            if r.flag()? && r.bits(8)? == 255 {
                r.bits(32)?;
            }
            if r.flag()? {
                r.bit()?;
            }
            let s0 = r.pos;
            if r.flag()? {
                colour.video_signal_type_present = true;
                colour.video_format = r.bits(3)? as u8;
                colour.full_range = r.flag()?;
                if r.flag()? {
                    colour.colour_description_present = true;
                    colour.primaries = r.bits(8)? as u8;
                    colour.transfer = r.bits(8)? as u8;
                    colour.matrix = r.bits(8)? as u8;
                }
            }
            signal = Some((s0, r.pos));
            if r.flag()? {
                r.ue()?;
                r.ue()?;
            }
            if r.flag()? {
                let n = r.bits(32)?;
                let t = r.bits(32)?;
                r.bit()?;
                timing = Some((n, t));
            }
            let nal = r.flag()?;
            if nal {
                skip_hrd(&mut r)?;
            }
            let vcl = r.flag()?;
            if vcl {
                skip_hrd(&mut r)?;
            }
            if nal || vcl {
                r.bit()?;
            }
            r.bit()?; // pic_struct_present
            if r.flag()? {
                r.bit()?;
                r.ue()?;
                r.ue()?;
                r.ue()?;
                r.ue()?;
                reorder = Some(r.ue()?);
                r.ue()?;
            }
        }
        let vui_end = r.pos;
        // The rest must be rbsp_trailing_bits: a 1 then zeros.
        if r.bit()? != 1 {
            return None;
        }
        Some(Layout {
            summary: Summary {
                profile_idc,
                level_idc,
                width_mbs,
                height_map_units,
                frame_mbs_only,
                max_num_ref_frames,
                colour,
                timing,
                max_num_reorder_frames: reorder,
            },
            vui_flag,
            signal,
            vui_end,
        })
    }

    /// Parse an SPS NAL unit (header byte included, emulation prevention
    /// still in). `None` = not an SPS or malformed.
    pub fn summary(nal: &[u8]) -> Option<Summary> {
        if nal.first().map(|b| b & 0x1F) != Some(super::NAL_SPS) {
            return None;
        }
        parse(&unescape(nal)).map(|l| l.summary)
    }

    /// The colour description an SPS signals.
    pub fn colour(nal: &[u8]) -> Option<Colour> {
        summary(nal).map(|s| s.colour)
    }

    fn write_signal(w: &mut Writer) {
        w.bit(1); // video_signal_type_present_flag
        w.bits(5, 3); // video_format: unspecified
        w.bit(0); // video_full_range_flag: limited
        w.bit(1); // colour_description_present_flag
        w.bits(1, 8); // colour_primaries: BT.709
        w.bits(1, 8); // transfer_characteristics: BT.709
        w.bits(1, 8); // matrix_coefficients: BT.709
    }

    /// Rewrite an SPS NAL to signal BT.709 limited range. `None` when it
    /// already does, or cannot be parsed (left untouched by callers).
    pub fn force_bt709_limited(nal: &[u8]) -> Option<Vec<u8>> {
        if nal.first().map(|b| b & 0x1F) != Some(super::NAL_SPS) {
            return None;
        }
        let rbsp = unescape(nal);
        let l = parse(&rbsp)?;
        if l.summary.colour.is_bt709_limited() {
            return None;
        }
        let mut w = Writer::default();
        match l.signal {
            Some((s0, s1)) => {
                // Keep everything, swap the colour block.
                w.copy(&rbsp, 0, s0);
                write_signal(&mut w);
                w.copy(&rbsp, s1, l.vui_end);
            }
            None => {
                // No VUI: add one carrying the colour block, the timing-free
                // defaults, and a bitstream restriction declaring no
                // reordering (the encoder never uses B-frames), which lets
                // decoders output each picture immediately.
                w.copy(&rbsp, 0, l.vui_flag);
                w.bit(1); // vui_parameters_present_flag
                w.bit(0); // aspect_ratio_info_present_flag
                w.bit(0); // overscan_info_present_flag
                write_signal(&mut w);
                w.bit(0); // chroma_loc_info_present_flag
                w.bit(0); // timing_info_present_flag
                w.bit(0); // nal_hrd_parameters_present_flag
                w.bit(0); // vcl_hrd_parameters_present_flag
                w.bit(0); // pic_struct_present_flag
                w.bit(1); // bitstream_restriction_flag
                w.bit(1); // motion_vectors_over_pic_boundaries_flag
                w.ue(2); // max_bytes_per_pic_denom (spec default)
                w.ue(1); // max_bits_per_mb_denom (spec default)
                w.ue(16); // log2_max_mv_length_horizontal
                w.ue(16); // log2_max_mv_length_vertical
                w.ue(0); // max_num_reorder_frames
                w.ue(l.summary.max_num_ref_frames.max(1)); // max_dec_frame_buffering
            }
        }
        Some(escape(&w.finish()))
    }

    /// Rewrite every SPS in an Annex-B access unit (see
    /// [`force_bt709_limited`]); other NAL units are copied unchanged.
    pub fn fix_annexb(au: &[u8]) -> Option<Vec<u8>> {
        let nals = super::annexb_nals(au);
        let fixed: Vec<Option<Vec<u8>>> = nals.iter().map(|n| force_bt709_limited(n)).collect();
        if fixed.iter().all(Option::is_none) {
            return None;
        }
        let mut out = Vec::with_capacity(au.len() + 16);
        for (n, f) in nals.iter().zip(fixed) {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(f.as_deref().unwrap_or(n));
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Main-profile SPS (1280x720 in MBs) written field by field, with an
    /// optional VUI (`full_range`, timing, bitstream restriction).
    fn make_sps(vui: Option<(bool, u8)>) -> Vec<u8> {
        let mut w = sps::Writer::default();
        w.bits(0x67, 8); // NAL header: SPS
        w.bits(77, 8); // profile_idc Main
        w.bits(0, 8); // constraint flags
        w.bits(31, 8); // level 3.1
        w.ue(0); // sps id
        w.ue(0); // log2_max_frame_num_minus4
        w.ue(0); // poc type 0
        w.ue(2); // log2_max_poc_lsb_minus4
        w.ue(1); // max_num_ref_frames
        w.bit(0); // gaps
        w.ue(79); // width 80 MBs
        w.ue(44); // height 45 map units
        w.bit(1); // frame_mbs_only
        w.bit(1); // direct_8x8
        w.bit(0); // no cropping
        match vui {
            None => w.bit(0),
            Some((full, matrix)) => {
                w.bit(1);
                w.bit(1); // aspect ratio present
                w.bits(1, 8); // 1:1
                w.bit(0); // overscan
                w.bit(1); // video signal type
                w.bits(5, 3);
                w.bit(full as u32);
                w.bit(1);
                w.bits(2, 8);
                w.bits(2, 8);
                w.bits(u32::from(matrix), 8);
                w.bit(0); // chroma loc
                w.bit(1); // timing
                w.bits(1, 32);
                w.bits(60, 32);
                w.bit(0);
                w.bit(1); // nal hrd
                w.ue(0);
                w.bits(4, 4);
                w.bits(6, 4);
                w.ue(4999);
                w.ue(9999);
                w.bit(1);
                w.bits(23, 5);
                w.bits(23, 5);
                w.bits(23, 5);
                w.bits(24, 5);
                w.bit(0); // vcl hrd
                w.bit(1); // low_delay_hrd
                w.bit(0); // pic struct
                w.bit(1); // bitstream restriction
                w.bit(1);
                w.ue(0);
                w.ue(0);
                w.ue(16);
                w.ue(16);
                w.ue(0); // max_num_reorder_frames
                w.ue(1);
            }
        }
        sps::escape(&w.finish())
    }

    #[test]
    fn sps_without_vui_gains_bt709_limited() {
        let raw = make_sps(None);
        let before = sps::summary(&raw).unwrap();
        assert!(!before.colour.vui_present);
        let fixed = sps::force_bt709_limited(&raw).unwrap();
        let after = sps::summary(&fixed).unwrap();
        assert!(after.colour.is_bt709_limited(), "{:?}", after.colour);
        assert_eq!((after.width_mbs, after.height_map_units), (80, 45));
        assert_eq!(after.profile_idc, 77);
        assert_eq!(after.max_num_reorder_frames, Some(0));
        // Idempotent.
        assert!(sps::force_bt709_limited(&fixed).is_none());
    }

    #[test]
    fn sps_vui_colour_is_replaced_and_the_rest_kept() {
        let raw = make_sps(Some((true, 6)));
        let before = sps::summary(&raw).unwrap();
        assert!(before.colour.full_range && before.colour.matrix == 6);
        let fixed = sps::force_bt709_limited(&raw).unwrap();
        let after = sps::summary(&fixed).unwrap();
        assert!(after.colour.is_bt709_limited());
        assert_eq!(after.timing, Some((1, 60)), "timing info preserved");
        assert_eq!(
            after.max_num_reorder_frames,
            Some(0),
            "bitstream restriction preserved"
        );
        assert_eq!((after.width_mbs, after.height_map_units), (80, 45));
    }

    #[test]
    fn sps_emulation_prevention_round_trips() {
        let rbsp = [0x67u8, 0, 0, 1, 0, 0, 0, 2, 0, 0, 3, 7];
        let esc = sps::escape(&rbsp);
        assert_eq!(esc, vec![0x67, 0, 0, 3, 1, 0, 0, 3, 0, 2, 0, 0, 3, 3, 7]);
        assert_eq!(sps::unescape(&esc), rbsp);
    }

    #[test]
    fn param_set_cache_rewrites_sps_colour() {
        let mut c = ParamSetCache::default();
        let mut au = vec![0, 0, 0, 1];
        au.extend_from_slice(&make_sps(None));
        au.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xCE, 0x3C, 0x80, 0, 0, 0, 1, 0x65, 0x88]);
        let out = c.fix(au);
        let parsed = AccessUnit::from_annexb(&out);
        assert!(sps::colour(parsed.sps.as_deref().unwrap())
            .unwrap()
            .is_bt709_limited());
        assert_eq!(parsed.pps.as_deref(), Some(&[0x68, 0xCE, 0x3C, 0x80][..]));
        assert!(parsed.idr);
    }

    fn map_with(w: u32, h: u32, rect: TileRect) -> TileMap {
        let mut m = TileMap::new(w, h);
        m.mark_rect(&rect);
        m
    }

    const VID: TileRect = TileRect {
        x: 640,
        y: 320,
        w: 640,
        h: 360,
    };

    #[test]
    fn video_header_round_trips_and_validates() {
        let h = VideoHeader {
            seq: 77,
            flags: VIDEO_KEYFRAME,
            codec: CODEC_H264,
            rect: TileRect {
                x: 64,
                y: 128,
                w: 640,
                h: 360,
            },
            screen_w: 1920,
            screen_h: 1080,
            pts_us: 1_700_000_000_123_456,
        };
        let mut b = Vec::new();
        h.encode(4321, &mut b);
        assert_eq!(b.len(), VIDEO_HEADER_LEN);
        assert_eq!(VideoHeader::decode(&b).unwrap(), (h, 4321));
        // Rect outside the screen / unknown codec / bad length: refused.
        let mut bad = h;
        bad.rect.x = 1800;
        let mut b2 = Vec::new();
        bad.encode(0, &mut b2);
        assert!(VideoHeader::decode(&b2).is_err());
        let mut b3 = b.clone();
        b3[9] = 9;
        assert!(VideoHeader::decode(&b3).is_err());
        assert!(VideoHeader::decode(&b[..20]).is_err());
    }

    #[test]
    fn annexb_split_and_avcc() {
        // AUD, SPS, PPS (3-byte start code), IDR with an emulation-free tail.
        let mut s = vec![0, 0, 0, 1, 0x09, 0xF0];
        s.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1F]);
        s.extend_from_slice(&[0, 0, 1, 0x68, 0xCE, 0x3C, 0x80]);
        s.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x10]);
        let nals = annexb_nals(&s);
        assert_eq!(nals.len(), 4);
        assert_eq!(
            nals.iter().map(|n| nal_type(n)).collect::<Vec<_>>(),
            [9, 7, 8, 5]
        );
        assert!(annexb_is_idr(&s));
        let au = AccessUnit::from_annexb(&s);
        assert_eq!(au.sps.as_deref(), Some(&[0x67, 0x42, 0x00, 0x1F][..]));
        assert_eq!(au.pps.as_deref(), Some(&[0x68, 0xCE, 0x3C, 0x80][..]));
        assert!(au.idr);
        assert_eq!(au.avcc, [0, 0, 0, 5, 0x65, 0x88, 0x84, 0x00, 0x10]);
        // A P-frame: no parameter sets.
        let p = [0, 0, 0, 1, 0x41, 0x9A, 0x02];
        let au = AccessUnit::from_annexb(&p);
        assert!(au.sps.is_none() && !au.idr);
        assert_eq!(au.avcc, [0, 0, 0, 3, 0x41, 0x9A, 0x02]);
    }

    #[test]
    fn param_set_cache_completes_a_bare_idr() {
        let mut c = ParamSetCache::default();
        let first = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9,
        ]
        .to_vec();
        assert_eq!(c.fix(first.clone()), first);
        let p = [0, 0, 0, 1, 0x41, 7].to_vec();
        assert_eq!(c.fix(p.clone()), p);
        let bare = [0, 0, 0, 1, 0x65, 8].to_vec();
        let fixed = c.fix(bare);
        let au = AccessUnit::from_annexb(&fixed);
        assert_eq!(au.sps.as_deref(), Some(&[0x67, 1, 2][..]));
        assert_eq!(au.pps.as_deref(), Some(&[0x68, 3][..]));
        assert!(au.idr);
    }

    #[test]
    fn colour_round_trip_is_close() {
        let (w, h) = (64u32, 32u32);
        let mut src = vec![0u8; (w * h * 4) as usize];
        synth_video(
            &mut src,
            w,
            TileRect {
                x: 0,
                y: 0,
                w: 64,
                h: 32,
            },
            5,
        );
        let mut nv = Vec::new();
        bgra_to_nv12(&src, w as usize * 4, w, h, &mut nv);
        assert_eq!(nv.len(), nv12_len(w, h));
        let mut back = Vec::new();
        nv12_to_bgra(&nv, w as usize, (w * h) as usize, w, h, &mut back).unwrap();
        // 4:2:0 subsampling of the grain costs a few dB, nothing more.
        let p = psnr_bgra(&src, &back);
        assert!(p > 30.0, "psnr {p}");
        // Flat colours survive almost exactly.
        let flat: Vec<u8> = [30u8, 140, 220, 255].repeat((w * h) as usize);
        bgra_to_nv12(&flat, w as usize * 4, w, h, &mut nv);
        nv12_to_bgra(&nv, w as usize, (w * h) as usize, w, h, &mut back).unwrap();
        assert!(psnr_bgra(&flat, &back) > 40.0);
        // Too small a buffer is an error, never a panic.
        assert!(nv12_to_bgra(&nv[..100], w as usize, (w * h) as usize, w, h, &mut back).is_err());
    }

    #[test]
    fn threaded_conversions_match_single_band() {
        let (w, h) = (1280u32, 720u32);
        let mut src = vec![0u8; (w * h * 4) as usize];
        synth_video(
            &mut src,
            w,
            TileRect {
                x: 0,
                y: 0,
                w: 1280,
                h: 720,
            },
            9,
        );
        let mut threaded = Vec::new();
        bgra_to_nv12(&src, w as usize * 4, w, h, &mut threaded);
        let mut single = vec![0u8; nv12_len(w, h)];
        let (yp, uvp) = single.split_at_mut((w * h) as usize);
        bgra_to_nv12_band(&src, w as usize * 4, w as usize, 0, yp, uvp);
        assert_eq!(threaded, single);
        let mut a = Vec::new();
        nv12_to_bgra(&single, w as usize, (w * h) as usize, w, h, &mut a).unwrap();
        let mut b = vec![0u8; (w * h * 4) as usize];
        nv12_to_bgra_band(&single, w as usize, (w * h) as usize, w as usize, 0, &mut b);
        assert_eq!(a, b);
    }

    #[test]
    fn motion_enters_after_sustained_large_change_and_exits_after_rest() {
        let (w, h) = (1920, 1080);
        let mut d = MotionDetector::default();
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let video = map_with(w, h, VID);
        let quiet = TileMap::new(w, h);
        // Two samples of 30 fps change: not yet.
        let mut t = 0;
        while t < 66 {
            assert_eq!(
                d.observe(ms(t), &video, w, h),
                MotionEvent::None,
                "at {t} ms"
            );
            t += 33;
        }
        // Crossing 65 ms (Wave 1; was 150): enter, with the tile-aligned rect.
        let mut entered = None;
        while t < 400 && entered.is_none() {
            if let MotionEvent::Enter(r) = d.observe(ms(t), &video, w, h) {
                entered = Some((t, r));
            }
            t += 33;
        }
        let (te, r) = entered.expect("entered motion");
        assert!((60..=100).contains(&te), "entered at {te} ms");
        assert_eq!(
            r,
            TileRect {
                x: 640,
                y: 320,
                w: 640,
                h: 384
            }
        );
        assert_eq!(d.motion_rect(), Some(r));
        // Keeps playing: stays in motion.
        for _ in 0..30 {
            assert_eq!(d.observe(ms(t), &video, w, h), MotionEvent::None);
            t += 33;
        }
        // Paused: exit only after ~1 s of rest (hysteresis).
        let pause = t;
        let mut exited = None;
        while t < pause + 2000 && exited.is_none() {
            if let MotionEvent::Exit(x) = d.observe(ms(t), &quiet, w, h) {
                exited = Some((t, x));
            }
            t += 33;
        }
        let (tx, x) = exited.expect("exited");
        assert!(
            tx - pause >= 990 && tx - pause <= 1100,
            "exit after {} ms",
            tx - pause
        );
        assert_eq!(x, r);
        assert_eq!(d.motion_rect(), None);
    }

    #[test]
    fn small_or_scattered_changes_never_enter() {
        let (w, h) = (1920, 1080);
        let mut d = MotionDetector::default();
        let t0 = Instant::now();
        // Typing: one tile, for ages.
        let caret = map_with(
            w,
            h,
            TileRect {
                x: 100,
                y: 100,
                w: 10,
                h: 16,
            },
        );
        // A clock + a caret at opposite corners: huge bbox, 2 tiles.
        let mut scattered = map_with(
            w,
            h,
            TileRect {
                x: 0,
                y: 0,
                w: 8,
                h: 8,
            },
        );
        scattered.mark_rect(&TileRect {
            x: 1900,
            y: 1060,
            w: 8,
            h: 8,
        });
        for i in 0..200 {
            let now = t0 + Duration::from_millis(i * 33);
            let m = if i % 2 == 0 { &caret } else { &scattered };
            assert_eq!(d.observe(now, m, w, h), MotionEvent::None);
        }
        // A short burst (two samples, < 65 ms) does not enter either.
        let big = map_with(
            w,
            h,
            TileRect {
                x: 0,
                y: 0,
                w: 1200,
                h: 800,
            },
        );
        for i in 0..2 {
            assert_eq!(
                d.observe(t0 + Duration::from_millis(10_000 + i * 33), &big, w, h),
                MotionEvent::None
            );
        }
        let quiet = TileMap::new(w, h);
        for i in 0..20 {
            assert_eq!(
                d.observe(t0 + Duration::from_millis(11_000 + i * 33), &quiet, w, h),
                MotionEvent::None
            );
        }
    }

    #[test]
    fn short_gaps_keep_the_run_and_a_big_area_snaps_to_full_screen() {
        let (w, h) = (1920, 1080);
        let mut d = MotionDetector::default();
        let t0 = Instant::now();
        // 24 fps content sampled at 30 fps: every 5th sample repeats.
        let page = map_with(
            w,
            h,
            TileRect {
                x: 0,
                y: 0,
                w: 1600,
                h: 1000,
            },
        );
        let quiet = TileMap::new(w, h);
        let mut ev = MotionEvent::None;
        for i in 0..30u64 {
            let m = if i % 5 == 4 { &quiet } else { &page };
            let e = d.observe(t0 + Duration::from_millis(i * 33), m, w, h);
            if e != MotionEvent::None {
                ev = e;
                break;
            }
        }
        assert_eq!(
            ev,
            MotionEvent::Enter(TileRect {
                x: 0,
                y: 0,
                w: 1920,
                h: 1080
            })
        );
    }

    #[test]
    fn motion_rect_grows_at_most_once_per_interval() {
        let (w, h) = (1920, 1080);
        let mut d = MotionDetector::default();
        let t0 = Instant::now();
        let a = map_with(
            w,
            h,
            TileRect {
                x: 0,
                y: 0,
                w: 640,
                h: 384,
            },
        );
        let mut t = 0u64;
        loop {
            if let MotionEvent::Enter(_) = d.observe(t0 + Duration::from_millis(t), &a, w, h) {
                break;
            }
            t += 33;
        }
        let b = map_with(
            w,
            h,
            TileRect {
                x: 0,
                y: 0,
                w: 640,
                h: 640,
            },
        );
        let mut grows = Vec::new();
        for _ in 0..90 {
            t += 33;
            if let MotionEvent::Grow(r) = d.observe(t0 + Duration::from_millis(t), &b, w, h) {
                grows.push(r);
            }
        }
        assert_eq!(
            grows,
            [TileRect {
                x: 0,
                y: 0,
                w: 640,
                h: 640
            }],
            "grew once, then contained"
        );
    }

    #[test]
    fn video_rx_drops_until_keyframe_after_a_gap() {
        let mk = |seq, flags| VideoHeader {
            seq,
            flags,
            codec: CODEC_H264,
            ..Default::default()
        };
        let mut rx = VideoRx::new();
        assert_eq!(
            rx.accept(&mk(0, 0)),
            VideoVerdict::Drop,
            "P-frame before any keyframe"
        );
        assert_eq!(rx.accept(&mk(1, VIDEO_KEYFRAME)), VideoVerdict::Deliver);
        assert_eq!(rx.accept(&mk(2, 0)), VideoVerdict::Deliver);
        assert_eq!(rx.accept(&mk(4, 0)), VideoVerdict::Drop, "gap");
        assert_eq!(rx.accept(&mk(5, 0)), VideoVerdict::Drop, "still broken");
        assert_eq!(rx.accept(&mk(6, VIDEO_KEYFRAME)), VideoVerdict::Deliver);
        assert_eq!(rx.accept(&mk(7, 0)), VideoVerdict::Deliver);
        assert_eq!(rx.accept(&mk(8, VIDEO_END)), VideoVerdict::End);
        assert_eq!(
            rx.accept(&mk(9, 0)),
            VideoVerdict::Drop,
            "a new period starts with an IDR"
        );
        assert_eq!(rx.accept(&mk(10, VIDEO_KEYFRAME)), VideoVerdict::Deliver);
        rx.mark_broken();
        assert_eq!(rx.accept(&mk(11, 0)), VideoVerdict::Drop);
        assert_eq!(rx.dropped, 5);
        // Wave 3: a unit older than what was already seen is stale (no IDR
        // request, no paint, state untouched).
        assert_eq!(rx.accept(&mk(12, VIDEO_KEYFRAME)), VideoVerdict::Deliver);
        assert_eq!(rx.accept(&mk(9, VIDEO_KEYFRAME)), VideoVerdict::Stale);
        assert_eq!(rx.accept(&mk(13, 0)), VideoVerdict::Deliver);
    }

    #[test]
    fn bitrate_follows_area_and_link() {
        assert_eq!(target_bitrate(1920, 1080, 30, None), 12_441_600);
        assert_eq!(target_bitrate(1920, 1080, 30, Some(1_000_000.0)), 5_600_000);
        assert_eq!(target_bitrate(64, 64, 30, Some(1.0)), 500_000);
        assert_eq!(target_bitrate(3840, 2160, 60, None), 40_000_000);
    }

    /// A fake encoder: "IDR" units start with 0x65, P units with 0x41; it
    /// records what it was asked to do.
    struct Fake {
        log: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
        first: bool,
    }
    impl VideoEncode for Fake {
        fn encode(
            &mut self,
            _bgra: &[u8],
            _stride: usize,
            force_idr: bool,
            pts_us: u64,
        ) -> Result<Vec<EncodedAu>, String> {
            let key = force_idr || self.first;
            self.first = false;
            self.log
                .borrow_mut()
                .push(if key { "I".into() } else { "P".into() });
            Ok(vec![EncodedAu {
                data: vec![0, 0, 0, 1, if key { 0x65 } else { 0x41 }, 1],
                keyframe: key,
                pts_us,
            }])
        }
        fn set_bitrate(&mut self, bps: u32) {
            self.log.borrow_mut().push(format!("rate {bps}"));
        }
        fn name(&self) -> String {
            "fake".into()
        }
    }

    fn fake_factory(log: std::rc::Rc<std::cell::RefCell<Vec<String>>>) -> EncoderFactory {
        Box::new(move |w, h, _fps, _b| {
            log.borrow_mut().push(format!("new {w}x{h}"));
            Ok(Box::new(Fake {
                log: log.clone(),
                first: true,
            }) as Box<dyn VideoEncode>)
        })
    }

    /// Wave 1: encoder sizes come in 128 px buckets and survive growth
    /// inside them; the coded rect always contains the motion rect.
    #[test]
    fn encoder_rect_buckets_and_reuses_the_encoder_size() {
        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut vs = VideoSession::new(fake_factory(log.clone()), 30);
        let (sw, sh) = (1920u32, 1080u32);
        let need = TileRect {
            x: 640,
            y: 320,
            w: 576,
            h: 320,
        };
        let r = vs.encoder_rect(need, sw, sh);
        assert_eq!((r.w, r.h), (640, 384));
        let contains = |r: TileRect, n: TileRect| {
            r.x <= n.x && r.y <= n.y && r.x + r.w >= n.x + n.w && r.y + r.h >= n.y + n.h
        };
        assert!(contains(r, need) && u32::from(r.x) + u32::from(r.w) <= sw);
        // An encoder of that size exists: a grown rect that still fits keeps it.
        vs.enc = Some((640, 384, (vs.factory)(640, 384, 30, 1).unwrap()));
        let grown = TileRect {
            x: 576,
            y: 320,
            w: 640,
            h: 384,
        };
        let r2 = vs.encoder_rect(grown, sw, sh);
        assert_eq!((r2.w, r2.h), (640, 384));
        assert!(contains(r2, grown));
        // Growing past it: the next bucket.
        let big = TileRect {
            x: 576,
            y: 256,
            w: 704,
            h: 448,
        };
        let r3 = vs.encoder_rect(big, sw, sh);
        assert_eq!((r3.w, r3.h), (768, 512));
        assert!(contains(r3, big));
        // At the screen edge the rect is pinned inside the screen.
        let edge = TileRect {
            x: 1856,
            y: 1024,
            w: 64,
            h: 56,
        };
        let r4 = vs.encoder_rect(edge, sw, sh);
        assert!(
            contains(r4, edge) && u32::from(r4.x + r4.w) <= sw && u32::from(r4.y + r4.h) <= sh,
            "{r4:?}"
        );
    }

    #[test]
    fn video_session_enters_streams_and_ends_with_a_refresh() {
        let (w, h) = (1920u32, 1080u32);
        let px = vec![0u8; (w * h * 4) as usize];
        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut vs = VideoSession::new(fake_factory(log.clone()), 30);
        let mb = FrameMailbox::default();
        let t0 = Instant::now();
        let video = map_with(w, h, VID);
        let quiet = TileMap::new(w, h);
        let mut t = 0u64;
        let mut first_video_at = None;
        while t < 1500 {
            let now = t0 + Duration::from_millis(t);
            if vs.on_capture(now, &px, w, h, w as usize * 4, &video, true, true, &mb)
                && first_video_at.is_none()
            {
                first_video_at = Some(t);
            }
            while mb.take().is_some() {} // the link drains everything
            t += 33;
        }
        assert!(
            first_video_at.is_some_and(|t| (60..=100).contains(&t)),
            "{first_video_at:?}"
        );
        assert_eq!(
            vs.active_rect(),
            Some(TileRect {
                x: 640,
                y: 320,
                w: 640,
                h: 384
            })
        );
        assert_eq!(log.borrow()[0], "new 640x384");
        assert_eq!(log.borrow()[1], "I");
        assert!(log.borrow()[2..].iter().all(|s| s == "P"));
        // A keyframe request -> the next picture is an IDR.
        vs.request_idr();
        vs.on_capture(
            t0 + Duration::from_millis(t),
            &px,
            w,
            h,
            w as usize * 4,
            &video,
            true,
            true,
            &mb,
        );
        let f = mb.take().expect("video unit");
        assert!(f.video.as_ref().is_some_and(|v| v.is_keyframe()));
        // Rest: END queued, then a refresh of the whole region.
        let mut end = None;
        for _ in 0..40 {
            t += 33;
            vs.on_capture(
                t0 + Duration::from_millis(t),
                &px,
                w,
                h,
                w as usize * 4,
                &quiet,
                true,
                true,
                &mb,
            );
            if let Some(f) = mb.take() {
                end = f.video;
                break;
            }
        }
        let end = end.expect("END marker");
        assert!(end.is_end());
        assert_eq!(
            end.rect,
            TileRect {
                x: 640,
                y: 320,
                w: 640,
                h: 384
            }
        );
        assert_eq!(vs.take_refresh(), Some(end.rect));
        assert_eq!(vs.active_rect(), None);
        let st = vs.stats();
        assert_eq!((st.enters, st.exits), (1, 1));
        assert!(st.frames >= 25, "{st:?}");
    }

    #[test]
    fn video_session_bitrate_follows_the_link() {
        let (w, h) = (1280u32, 720u32);
        let px = vec![0u8; (w * h * 4) as usize];
        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let cfg = MotionConfig {
            enter_after: Duration::ZERO,
            ..MotionConfig::default()
        };
        let mut vs = VideoSession::with_config(fake_factory(log.clone()), 30, cfg);
        let mb = FrameMailbox::default();
        let all = map_with(
            w,
            h,
            TileRect {
                x: 0,
                y: 0,
                w: 1280,
                h: 720,
            },
        );
        let t0 = Instant::now();
        let step = |vs: &mut VideoSession, i: u64| {
            vs.on_capture(
                t0 + Duration::from_millis(i * 33),
                &px,
                w,
                h,
                w as usize * 4,
                &all,
                true,
                true,
                &mb,
            );
            while mb.take().is_some() {}
        };
        for i in 0..4 {
            step(&mut vs, i);
        }
        assert_eq!(vs.bitrate(), target_bitrate(1280, 720, 30, None));
        // The pacer measures a 500 KB/s link: 70% of it, retargeted live.
        vs.set_link_rate(Some(500_000.0));
        step(&mut vs, 4);
        assert_eq!(vs.bitrate(), 2_800_000);
        assert!(log.borrow().iter().any(|l| l == "rate 2800000"));
        // Small wobble (< 15%): no churn.
        vs.set_link_rate(Some(520_000.0));
        step(&mut vs, 5);
        assert_eq!(vs.bitrate(), 2_800_000);
    }

    #[test]
    fn video_session_backs_off_when_the_queue_is_full_and_stops_without_h264() {
        let (w, h) = (1280u32, 720u32);
        let px = vec![0u8; (w * h * 4) as usize];
        let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let cfg = MotionConfig {
            enter_after: Duration::ZERO,
            ..MotionConfig::default()
        };
        let mut vs = VideoSession::with_config(fake_factory(log.clone()), 30, cfg);
        let mb = FrameMailbox::default();
        let all = map_with(
            w,
            h,
            TileRect {
                x: 0,
                y: 0,
                w: 1280,
                h: 720,
            },
        );
        let t0 = Instant::now();
        for i in 0..10u64 {
            vs.on_capture(
                t0 + Duration::from_millis(i * 33),
                &px,
                w,
                h,
                w as usize * 4,
                &all,
                true,
                true,
                &mb,
            );
        }
        // Nothing drained: the queue caps at MAX_VIDEO_QUEUE, never dropped.
        assert_eq!(mb.video_queued(), MAX_VIDEO_QUEUE);
        assert!(vs.stats().skipped > 0);
        // The viewer turns out not to take H.264 (or a caps change): END.
        vs.on_capture(
            t0 + Duration::from_millis(400),
            &px,
            w,
            h,
            w as usize * 4,
            &all,
            true,
            false,
            &mb,
        );
        assert_eq!(mb.video_queued(), MAX_VIDEO_QUEUE + 1);
        assert_eq!(vs.active_rect(), None);
        assert!(vs.take_refresh().is_some());
    }

    #[test]
    fn encoder_failure_falls_back_to_tiles() {
        let (w, h) = (1280u32, 720u32);
        let px = vec![0u8; (w * h * 4) as usize];
        let cfg = MotionConfig {
            enter_after: Duration::ZERO,
            ..MotionConfig::default()
        };
        let mut vs =
            VideoSession::with_config(Box::new(|_, _, _, _| Err("no encoder".to_owned())), 30, cfg);
        let mb = FrameMailbox::default();
        let all = map_with(
            w,
            h,
            TileRect {
                x: 0,
                y: 0,
                w: 1280,
                h: 720,
            },
        );
        let t0 = Instant::now();
        for i in 0..5u64 {
            assert!(!vs.on_capture(
                t0 + Duration::from_millis(i * 33),
                &px,
                w,
                h,
                w as usize * 4,
                &all,
                true,
                true,
                &mb
            ));
        }
        assert_eq!(vs.active_rect(), None);
        assert_eq!(vs.stats().encoder_failures, 1, "retry is rate-limited");
        assert_eq!(vs.take_error().as_deref(), Some("no encoder"));
    }
}
