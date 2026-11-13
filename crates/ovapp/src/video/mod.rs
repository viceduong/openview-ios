//! Hardware H.264 for high-motion screen content.
//!
//! Lossless dirty tiles ([`crate::tiles`]) stay the default. When a large
//! region keeps changing (video, fast scrolling, games) the host's
//! [`MotionDetector`] switches that **region** to H.264: the tile encoder
//! masks it out (`crate::sendq::UpdateEncoder::submit_masked`) and a
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
//! Media Foundation encoder/decoder live in `crate::mfcodec` (Windows, `mf`
//! feature).
//!
//! Layout: this file holds the wire header, the codec traits and the
//! viewer-side [`VideoRx`]; `annexb` (NAL / AVCC helpers), `convert`
//! (BGRA <-> NV12), `motion` ([`MotionDetector`]), `session`
//! ([`VideoSession`]), `synth` (synthetic test content) and [`sps`] (SPS
//! colour signalling) hold the rest; their items are re-exported here.

mod annexb;
mod convert;
mod motion;
mod session;
pub mod sps;
mod synth;
#[cfg(test)]
mod tests;

pub use annexb::*;
pub use convert::*;
pub use motion::*;
pub use session::*;
pub use synth::*;

use crate::error::AppError;
use crate::tiles::TileRect;

/// `VIDEO_BEGIN` codec byte: H.264, Annex-B byte stream.
pub const CODEC_H264: u8 = 1;
/// [`VideoHeader::flags`]: an IDR access unit (SPS + PPS in-band).
pub const VIDEO_KEYFRAME: u8 = 1 << 0;
/// [`VideoHeader::flags`]: motion over — no payload; `rect` is the region
/// the following lossless tile update refreshes. The viewer drops its video
/// overlay once that update is applied.
pub const VIDEO_END: u8 = 1 << 1;
/// Wire size of a `VIDEO_BEGIN` payload (without the opcode).
pub(crate) const VIDEO_HEADER_LEN: usize = 38;
/// Video access units queued in the mailbox at most; past this the host
/// skips encoding a capture (the link is the bottleneck) instead of piling
/// up a backlog it may not drop.
pub(crate) const MAX_VIDEO_QUEUE: usize = 3;

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
    /// The screen (tile framebuffer) width `rect` refers to.
    pub screen_w: u32,
    /// The screen (tile framebuffer) height `rect` refers to.
    pub screen_h: u32,
    /// Capture time, microseconds since the UNIX epoch (end-to-end latency
    /// on a shared clock; also the presentation timestamp).
    pub pts_us: u64,
}

impl VideoHeader {
    /// Whether [`VIDEO_KEYFRAME`] is set.
    pub fn is_keyframe(&self) -> bool {
        self.flags & VIDEO_KEYFRAME != 0
    }

    /// Whether [`VIDEO_END`] is set.
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
    pub fn decode(p: &[u8]) -> Result<(Self, u32), AppError> {
        if p.len() != VIDEO_HEADER_LEN {
            return Err(AppError::malformed(format!(
                "VIDEO_BEGIN payload is {} bytes, want {VIDEO_HEADER_LEN}",
                p.len()
            )));
        }
        // The length was checked above, so every fixed offset below is in
        // range and the slice-to-array conversions cannot fail.
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
            return Err(AppError::malformed(format!(
                "VIDEO_BEGIN has an unknown codec {}",
                h.codec
            )));
        }
        if !h.rect.fits(h.screen_w, h.screen_h) {
            return Err(AppError::malformed(format!(
                "VIDEO_BEGIN rect {:?} outside {}x{}",
                h.rect, h.screen_w, h.screen_h
            )));
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
// Codec traits
// ---------------------------------------------------------------------------

/// One encoded access unit (Annex-B).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedAu {
    /// The Annex-B bytes.
    pub data: Vec<u8>,
    /// Whether this is an IDR access unit.
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
    /// Encode the `rect` crop `(x, y, w, h)` of a GPU-resident
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
    /// Older than a unit already seen (it took the other channel — media vs
    /// reliable — and lost the race): drop it silently, never paint stale
    /// pictures over newer content.
    Stale,
}

/// Viewer-side continuity check of the video stream: P-frames are only
/// decoded on top of an unbroken chain from a keyframe.
#[derive(Debug, Clone, Default)]
pub struct VideoRx {
    expect: Option<u64>,
    /// In sync with the stream (false = waiting for a keyframe).
    synced: bool,
    /// Units dropped while waiting for a keyframe.
    pub dropped: u64,
}

impl VideoRx {
    /// A receiver waiting for the first keyframe.
    pub fn new() -> Self {
        Self::default()
    }

    /// Classify the next received unit (and advance the expected `seq`).
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
