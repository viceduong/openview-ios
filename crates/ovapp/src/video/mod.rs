//! Hardware H.264 / HEVC for high-motion screen content.
//!
//! Lossless dirty tiles ([`crate::tiles`]) stay the default. When a large
//! region keeps changing (video, fast scrolling, games) the host's
//! [`MotionDetector`] switches that **region** to video: the tile encoder
//! masks it out (`crate::sendq::UpdateEncoder::submit_ex`) and a
//! [`VideoSession`] streams it as `VIDEO_BEGIN` access units composited over
//! the viewer's tile framebuffer. Once the region has been still for
//! [`MotionConfig::exit_after`] the host sends a `VIDEO_END` marker followed
//! by a forced lossless tile refresh of the whole region.
//!
//! # Wire form (see [`crate::terminal`] and `docs/MEDIA_PIPELINE.md`)
//!
//! `VIDEO_BEGIN` (opcode `0x25`) carries a [`VideoHeader`]; the access unit
//! follows as `FRAME_DATA` chunks closed by `FRAME_END(seq)` — the same
//! chunking/pacing path as screen frames. The payload is **Annex-B**
//! (start codes, exactly what the Media Foundation encoder emits) in the
//! header's codec ([`VideoCodec`]: H.264, or HEVC to a viewer that
//! advertised [`crate::terminal::CAP_HEVC`]); every keyframe carries its
//! parameter sets (H.264 SPS + PPS, HEVC VPS + SPS + PPS) in-band. Video
//! seqs are consecutive per session: a gap (or a decode error) makes the
//! viewer drop P-frames and ask for a way back in ([`VideoRx::request`]):
//! `VIDEO_IDR_REQUEST` (`0x26`), or — both peers advertised
//! [`crate::terminal::CAP_LTR`] — `VIDEO_RECOVER` (`0x29`) naming the newest
//! long-term reference it decoded, which the host answers with a P-frame
//! predicted only from that reference ([`VIDEO_RECOVERY`]) instead of an
//! IDR. Video access units are never replaced ("newest wins" would break the
//! reference chain): the host instead holds capture while the link is busy
//! and adapts the bitrate to the pacer's delivery-rate estimate.
//!
//! Only viewers that advertised [`crate::terminal::CAP_H264`] ever see any of
//! this. The iOS shell gets AVCC/HVCC (length-prefixed NAL units) + the
//! parameter sets split out by [`AccessUnit::from_annexb_codec`]; desktop
//! decodes Annex-B.
//!
//! Everything here except the codecs themselves is pure and unit-tested; the
//! Media Foundation encoder/decoder live in `crate::mfcodec` (Windows, `mf`
//! feature).
//!
//! Layout: this file holds the wire header, the codec traits and the
//! viewer-side [`VideoRx`]; `annexb` (NAL / AVCC helpers), `codec`
//! ([`VideoCodec`], LTR control types), `convert` (BGRA <-> NV12), `ltr`
//! (host-side [`LtrTracker`]), `motion` ([`MotionDetector`]), `session`
//! ([`VideoSession`]), `synth` (synthetic test content) and [`sps`] (SPS
//! colour signalling) hold the rest; their items are re-exported here.

mod annexb;
mod codec;
mod convert;
mod ltr;
mod motion;
#[cfg(test)]
mod recovery_tests;
mod session;
pub mod sps;
mod synth;
#[cfg(test)]
mod tests;

pub use annexb::*;
pub use codec::*;
pub use convert::*;
pub use ltr::*;
pub use motion::*;
pub use session::*;
pub use synth::*;

use crate::error::AppError;
use crate::tiles::TileRect;

/// [`VideoHeader::flags`]: a keyframe access unit (IDR / IRAP; parameter
/// sets in-band).
pub const VIDEO_KEYFRAME: u8 = 1 << 0;
/// [`VideoHeader::flags`]: motion over — no payload; `rect` is the region
/// the following lossless tile update refreshes. The viewer drops its video
/// overlay once that update is applied.
pub const VIDEO_END: u8 = 1 << 1;
/// [`VideoHeader::flags`] ([`crate::terminal::CAP_LTR`] viewers only): this
/// picture was marked as a long-term reference. A viewer that decoded it
/// answers `VIDEO_LTR_ACK(seq)`; it may later name it in `VIDEO_RECOVER`.
pub const VIDEO_LTR: u8 = 1 << 2;
/// [`VideoHeader::flags`] ([`crate::terminal::CAP_LTR`] viewers only): a
/// recovery picture — a P-frame predicted only from the long-term reference
/// the viewer named in its last `VIDEO_RECOVER` (and every later picture
/// only from it or newer ones). A viewer waiting for recovery resumes
/// decoding here instead of waiting for a keyframe.
pub const VIDEO_RECOVERY: u8 = 1 << 3;
/// [`VideoHeader::flags`], local only (never sent by a host): set by the
/// viewer's session on a media-channel unit that a newer picture of the
/// same delivery burst follows — decode it (the reference chain), but only
/// the newest needs presenting.
pub const VIDEO_SUPERSEDED: u8 = 1 << 7;
/// Wire size of a `VIDEO_BEGIN` payload (without the opcode).
pub const VIDEO_HEADER_LEN: usize = 38;
/// Video access units queued in the mailbox at most; past this the host
/// skips encoding a capture (the link is the bottleneck) instead of piling
/// up a backlog it may not drop.
pub(crate) const MAX_VIDEO_QUEUE: usize = 3;

/// The header of one video access unit (`VIDEO_BEGIN`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VideoHeader {
    /// Consecutive per host session (END markers included).
    pub seq: u64,
    /// [`VIDEO_KEYFRAME`] | [`VIDEO_END`] | [`VIDEO_LTR`] |
    /// [`VIDEO_RECOVERY`]. Unknown bits are ignored.
    pub flags: u8,
    /// [`CODEC_H264`] or [`CODEC_HEVC`] (see [`VideoCodec`]).
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

    /// Whether [`VIDEO_LTR`] is set.
    pub fn is_ltr(&self) -> bool {
        self.flags & VIDEO_LTR != 0
    }

    /// Whether [`VIDEO_RECOVERY`] is set.
    pub fn is_recovery(&self) -> bool {
        self.flags & VIDEO_RECOVERY != 0
    }

    /// The stream's codec (H.264 for an unknown byte; [`decode`](Self::decode)
    /// rejects those).
    pub fn video_codec(&self) -> VideoCodec {
        VideoCodec::from_wire(self.codec).unwrap_or_default()
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
        if VideoCodec::from_wire(h.codec).is_none() {
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EncodedAu {
    /// The Annex-B bytes.
    pub data: Vec<u8>,
    /// Whether this is a keyframe (IDR / IRAP) access unit.
    pub keyframe: bool,
    /// The `pts_us` of the input picture this unit encodes.
    pub pts_us: u64,
    /// Long-term references this picture was marked as / predicted from.
    pub ltr: LtrInfo,
}

/// A video encoder (the host's H.264 / HEVC MFT; a fake in tests).
pub trait VideoEncode {
    /// Encode one `w x h` BGRA picture (`stride` bytes per row; the size was
    /// fixed at creation). May return zero units (pipeline latency) or more
    /// than one. `force_idr` asks for a keyframe now.
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
    /// The codec this encoder produces.
    fn codec(&self) -> VideoCodec {
        VideoCodec::H264
    }
    /// Long-term reference slots under the caller's control (0 = this
    /// encoder cannot mark / use LTR frames; recovery then needs an IDR).
    fn ltr_slots(&self) -> u8 {
        0
    }
    /// Reference control for the **next** picture only (ignored when
    /// [`ltr_slots`](Self::ltr_slots) is 0).
    fn set_frame_refs(&mut self, refs: FrameRefs) {
        let _ = refs;
    }
}

/// Builds an encoder for an [`EncoderSpec`].
pub type EncoderFactory = Box<dyn FnMut(&EncoderSpec) -> Result<Box<dyn VideoEncode>, String>>;

/// A video decoder (the desktop viewer's H.264 / HEVC MFT).
pub trait VideoDecode {
    /// Decode one H.264 Annex-B access unit of a `w x h` picture into tight
    /// BGRA `out` (`w * h * 4`). `Ok(true)` when a picture came out.
    fn decode(&mut self, annexb: &[u8], w: u32, h: u32, out: &mut Vec<u8>) -> Result<bool, String>;
    /// Whether this decoder also takes HEVC ([`decode_codec`](Self::decode_codec)
    /// with [`VideoCodec::Hevc`]); the viewer then advertises
    /// [`crate::terminal::CAP_HEVC`].
    fn hevc(&self) -> bool {
        false
    }
    /// [`decode`](Self::decode) for an access unit of `codec`. The default
    /// handles H.264 only; a decoder whose [`hevc`](Self::hevc) is true
    /// overrides it.
    fn decode_codec(
        &mut self,
        codec: VideoCodec,
        annexb: &[u8],
        w: u32,
        h: u32,
        out: &mut Vec<u8>,
    ) -> Result<bool, String> {
        match codec {
            VideoCodec::H264 => self.decode(annexb, w, h, out),
            other => Err(format!("{other} decode not supported")),
        }
    }
    /// Decode the unit `header` describes (codec, rect): into `out` like
    /// [`decode_codec`](Self::decode_codec) ([`DecodedTo::Cpu`]), or — a
    /// zero-copy decoder — presented on the GPU by the decoder itself
    /// ([`DecodedTo::Gpu`]: the framebuffer under `header.rect` keeps its
    /// old pixels until [`end_gpu`](Self::end_gpu)). The default is the CPU
    /// path.
    fn decode_unit(
        &mut self,
        header: &VideoHeader,
        annexb: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<DecodedTo, String> {
        let codec = VideoCodec::from_wire(header.codec)
            .ok_or_else(|| format!("unknown video codec {}", header.codec))?;
        let (w, h) = (u32::from(header.rect.w), u32::from(header.rect.h));
        Ok(match self.decode_codec(codec, annexb, w, h, out)? {
            true => DecodedTo::Cpu,
            false => DecodedTo::Nothing,
        })
    }
    /// Zero-copy decoders: stop presenting the GPU picture. When one was
    /// shown, its pixels (tight BGRA, the rect of the unit that made it)
    /// are written to `out` and `true` returned — the caller paints them
    /// into the framebuffer. The default (a CPU decoder) shows nothing.
    fn end_gpu(&mut self, out: &mut Vec<u8>) -> bool {
        let _ = out;
        false
    }
}

/// Where [`VideoDecode::decode_unit`] put a picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodedTo {
    /// No picture came out (the decoder wants more input).
    Nothing,
    /// Tight BGRA in the caller's buffer: paint it into the framebuffer.
    Cpu,
    /// Presented on the GPU by the decoder (zero-copy).
    Gpu,
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
    /// Undecodable (gap / waiting for a keyframe or recovery picture): drop
    /// it; ask for a way back in ([`VideoRx::request`]).
    Drop,
    /// Older than a unit already seen (it took the other channel — media vs
    /// reliable — and lost the race): drop it silently, never paint stale
    /// pictures over newer content.
    Stale,
}

/// What a broken viewer asks the host for ([`VideoRx::request`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoRequest {
    /// `VIDEO_IDR_REQUEST`: a full keyframe.
    Idr,
    /// `VIDEO_RECOVER(ltr_seq)`: a recovery P-frame predicted from the
    /// long-term reference `ltr_seq`, which this viewer decoded.
    Recover {
        /// The `seq` of the LTR-marked unit the viewer still holds.
        ltr_seq: u64,
    },
}

/// Viewer-side continuity check of the video stream: P-frames are only
/// decoded on top of an unbroken chain from a keyframe (or from a recovery
/// picture that references a long-term reference this viewer holds).
#[derive(Debug, Clone, Default)]
pub struct VideoRx {
    expect: Option<u64>,
    /// In sync with the stream (false = waiting for a keyframe / recovery).
    synced: bool,
    /// The newest LTR-marked unit decoded since the last keyframe (the
    /// decoder still holds it as a long-term reference).
    ltr_held: Option<u64>,
    /// A `VIDEO_RECOVER` for this LTR was sent and no recovery picture
    /// arrived yet (retries ask for an IDR).
    recovering: Option<u64>,
    /// Units dropped while waiting for a keyframe.
    pub dropped: u64,
    /// Recovery pictures that resynced the stream (no IDR needed).
    pub recovered: u64,
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
            // The next motion period starts with a keyframe (which also
            // drops every long-term reference).
            self.synced = false;
            self.ltr_held = None;
            self.recovering = None;
            return VideoVerdict::End;
        }
        if h.is_keyframe() {
            self.synced = true;
            self.ltr_held = None;
            self.recovering = None;
            return VideoVerdict::Deliver;
        }
        if self.synced && in_order {
            return VideoVerdict::Deliver;
        }
        if h.is_recovery() && self.ltr_held.is_some() {
            // Predicted only from an LTR the host knows we hold (we acked or
            // named it; the decoder kept its references since): decodable
            // whatever was lost in between — also a late answer to an
            // earlier request, after a resync or an escalation to an IDR.
            self.synced = true;
            self.recovering = None;
            self.recovered += 1;
            return VideoVerdict::Deliver;
        }
        self.synced = false;
        self.dropped += 1;
        VideoVerdict::Drop
    }

    /// `h` (a [`Deliver`](VideoVerdict::Deliver)ed unit) decoded fine.
    /// Returns the `seq` to acknowledge with `VIDEO_LTR_ACK` when it was a
    /// long-term reference.
    pub fn decoded(&mut self, h: &VideoHeader) -> Option<u64> {
        if !h.is_ltr() || !self.synced {
            return None;
        }
        // Seqs only grow within a motion period (keyframes reset).
        if self
            .ltr_held
            .is_none_or(|s| h.seq.wrapping_sub(s) < 1 << 62)
        {
            self.ltr_held = Some(h.seq);
        }
        Some(h.seq)
    }

    /// The chain broke through **loss** (a gap, a torn unit, media lost
    /// for good): drop P-frames until a keyframe or a recovery picture.
    /// The decoder's references are intact, so LTR recovery stays possible.
    pub fn mark_broken(&mut self) {
        self.synced = false;
    }

    /// The **decoder** failed (or was reset): its reference pictures are
    /// not trustworthy any more — drop P-frames and recover with a keyframe
    /// only.
    pub fn mark_corrupt(&mut self) {
        self.synced = false;
        self.ltr_held = None;
        self.recovering = None;
    }

    /// Whether the stream is waiting for a keyframe / recovery picture.
    pub fn is_broken(&self) -> bool {
        !self.synced
    }

    /// The newest long-term reference this viewer holds.
    pub fn ltr_held(&self) -> Option<u64> {
        self.ltr_held
    }

    /// What to ask the host for now that the chain is broken. `ltr` = both
    /// peers advertised [`crate::terminal::CAP_LTR`]. A recovery that was
    /// already asked for and did not arrive (the caller rate-limits
    /// requests, so this is the retry) escalates to an IDR — and so does
    /// every retry until a keyframe or a recovery picture arrives.
    pub fn request(&mut self, ltr: bool) -> VideoRequest {
        match (ltr, self.ltr_held, self.recovering) {
            (true, Some(seq), None) => {
                self.recovering = Some(seq);
                VideoRequest::Recover { ltr_seq: seq }
            }
            _ => VideoRequest::Idr,
        }
    }
}
