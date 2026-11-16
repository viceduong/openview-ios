//! Codec identity (H.264 / HEVC) and the per-codec NAL unit vocabulary.

use super::annexb::{annexb_nals, NAL_AUD, NAL_IDR, NAL_PPS, NAL_SPS};

/// `VIDEO_BEGIN` codec byte: H.264, Annex-B byte stream.
pub const CODEC_H264: u8 = 1;
/// `VIDEO_BEGIN` codec byte: HEVC (H.265), Annex-B byte stream (VPS, SPS
/// and PPS in-band on every keyframe). Only sent to a viewer that
/// advertised [`crate::terminal::CAP_HEVC`].
pub const CODEC_HEVC: u8 = 2;

/// HEVC `nal_unit_type`: video parameter set.
pub const HEVC_NAL_VPS: u8 = 32;
/// HEVC `nal_unit_type`: sequence parameter set.
pub const HEVC_NAL_SPS: u8 = 33;
/// HEVC `nal_unit_type`: picture parameter set.
pub const HEVC_NAL_PPS: u8 = 34;
/// HEVC `nal_unit_type`: access unit delimiter.
pub const HEVC_NAL_AUD: u8 = 35;

/// The video codec of a stream (the `VIDEO_BEGIN` codec byte).
///
/// Shared by the host encoder, the desktop decoder (`mfcodec`) and the iOS
/// pass-through (`ovffi`): every place that has to tell the two apart
/// matches on this instead of the raw byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum VideoCodec {
    /// H.264 / AVC ([`CODEC_H264`]).
    #[default]
    H264,
    /// HEVC / H.265 ([`CODEC_HEVC`]).
    Hevc,
}

/// Which parameter set a NAL unit is, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamSet {
    /// HEVC video parameter set.
    Vps,
    /// Sequence parameter set.
    Sps,
    /// Picture parameter set.
    Pps,
}

impl VideoCodec {
    /// Parse the `VIDEO_BEGIN` codec byte (`None` = unknown codec).
    pub fn from_wire(b: u8) -> Option<Self> {
        match b {
            CODEC_H264 => Some(Self::H264),
            CODEC_HEVC => Some(Self::Hevc),
            _ => None,
        }
    }

    /// The `VIDEO_BEGIN` codec byte.
    pub fn wire(self) -> u8 {
        match self {
            Self::H264 => CODEC_H264,
            Self::Hevc => CODEC_HEVC,
        }
    }

    /// Short name for logs ("H.264", "HEVC").
    pub fn name(self) -> &'static str {
        match self {
            Self::H264 => "H.264",
            Self::Hevc => "HEVC",
        }
    }

    /// `nal_unit_type` of a NAL unit (H.264: low 5 bits of the 1-byte
    /// header; HEVC: bits 1..7 of the first byte of the 2-byte header).
    pub fn nal_type(self, nal: &[u8]) -> u8 {
        let b = nal.first().copied().unwrap_or(0);
        match self {
            Self::H264 => b & 0x1F,
            Self::Hevc => (b >> 1) & 0x3F,
        }
    }

    /// Whether `nal_type` is a random-access (keyframe) picture: H.264 IDR;
    /// HEVC IRAP (BLA / IDR / CRA, types 16..=21).
    pub fn is_keyframe_nal(self, nal_type: u8) -> bool {
        match self {
            Self::H264 => nal_type == NAL_IDR,
            Self::Hevc => (16..=21).contains(&nal_type),
        }
    }

    /// Which parameter set `nal_type` is.
    pub fn param_set(self, nal_type: u8) -> Option<ParamSet> {
        match (self, nal_type) {
            (Self::H264, NAL_SPS) | (Self::Hevc, HEVC_NAL_SPS) => Some(ParamSet::Sps),
            (Self::H264, NAL_PPS) | (Self::Hevc, HEVC_NAL_PPS) => Some(ParamSet::Pps),
            (Self::Hevc, HEVC_NAL_VPS) => Some(ParamSet::Vps),
            _ => None,
        }
    }

    /// Whether `nal_type` is an access unit delimiter.
    pub fn is_aud(self, nal_type: u8) -> bool {
        match self {
            Self::H264 => nal_type == NAL_AUD,
            Self::Hevc => nal_type == HEVC_NAL_AUD,
        }
    }

    /// The parameter sets a keyframe must carry to be self-contained.
    pub(crate) fn required_param_sets(self) -> &'static [ParamSet] {
        match self {
            Self::H264 => &[ParamSet::Sps, ParamSet::Pps],
            Self::Hevc => &[ParamSet::Vps, ParamSet::Sps, ParamSet::Pps],
        }
    }

    /// Whether an Annex-B access unit of this codec contains a keyframe
    /// (IDR / IRAP) picture.
    pub fn annexb_is_keyframe(self, data: &[u8]) -> bool {
        annexb_nals(data)
            .iter()
            .any(|n| self.is_keyframe_nal(self.nal_type(n)))
    }
}

impl std::fmt::Display for VideoCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

// ---------------------------------------------------------------------------
// Encoder-side reference control (long-term references)
// ---------------------------------------------------------------------------

/// Long-term-reference (LTR) slots the host asks of an encoder: enough for
/// one confirmed recovery point while the next one is being confirmed.
pub const LTR_SLOTS: u8 = 2;

/// Reference control for **one** picture handed to
/// [`VideoEncode`](super::VideoEncode) (see
/// [`VideoEncode::set_frame_refs`](super::VideoEncode::set_frame_refs)).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameRefs {
    /// Mark this picture as long-term reference slot `n`
    /// (`CODECAPI_AVEncVideoMarkLTRFrame`).
    pub mark_ltr: Option<u8>,
    /// Predict this picture only from the LTR slots in this bitmap (bit n =
    /// slot n), and nothing older than it afterwards
    /// (`CODECAPI_AVEncVideoUseLTRFrame`). 0 = no constraint.
    pub use_ltr: u16,
}

/// What the encoder reported about an output picture's long-term
/// references (`MFSampleExtension_LongTermReferenceFrameInfo`, or what the
/// encoder was asked for when the MFT does not report it).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LtrInfo {
    /// The LTR slot this picture was marked as, if any.
    pub marked: Option<u8>,
    /// Bitmap of LTR slots this picture was predicted from (0 = none /
    /// unknown).
    pub used: u16,
}

/// What a [`super::EncoderFactory`] is asked to build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderSpec {
    /// Picture width (even).
    pub w: u32,
    /// Picture height (even).
    pub h: u32,
    /// Capture rate.
    pub fps: u32,
    /// Initial mean bitrate, bits/s.
    pub bitrate: u32,
    /// The codec wanted. A factory that cannot provide it fails; the
    /// session then retries with H.264.
    pub codec: VideoCodec,
    /// LTR slots wanted (0 = no long-term references).
    pub ltr_slots: u8,
}
