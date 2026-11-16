//! Annex-B / AVCC access-unit helpers: NAL splitting, out-of-band
//! parameter sets, and keeping every keyframe self-contained.

use super::codec::{ParamSet, VideoCodec};
use super::sps;

// NAL unit types we care about.
/// `nal_unit_type`: coded slice of a non-IDR picture.
pub const NAL_SLICE: u8 = 1;
/// `nal_unit_type`: coded slice of an IDR picture.
pub(crate) const NAL_IDR: u8 = 5;
/// `nal_unit_type`: supplemental enhancement information.
pub const NAL_SEI: u8 = 6;
/// `nal_unit_type`: sequence parameter set.
pub(crate) const NAL_SPS: u8 = 7;
/// `nal_unit_type`: picture parameter set.
pub(crate) const NAL_PPS: u8 = 8;
/// `nal_unit_type`: access unit delimiter.
pub(crate) const NAL_AUD: u8 = 9;

/// The NAL units of an Annex-B byte stream (start codes stripped; trailing
/// zero bytes of a unit belong to the next start code and are dropped).
pub(crate) fn annexb_nals(data: &[u8]) -> Vec<&[u8]> {
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
pub(crate) fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |b| b & 0x1F)
}

/// Whether an Annex-B access unit contains an IDR slice.
pub fn annexb_is_idr(data: &[u8]) -> bool {
    annexb_nals(data).iter().any(|n| nal_type(n) == NAL_IDR)
}

/// One access unit split for a decoder that wants parameter sets out of band
/// (VideoToolbox): VPS (HEVC) / SPS / PPS (first of each, if present) and the
/// remaining NAL units each prefixed by its 4-byte big-endian length (AVCC /
/// HVCC sample layout). Access unit delimiters are dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccessUnit {
    /// The first VPS NAL unit (HEVC only; header bytes included), if any.
    pub vps: Option<Vec<u8>>,
    /// The first SPS NAL unit (header byte(s) included), if any.
    pub sps: Option<Vec<u8>>,
    /// The first PPS NAL unit (header byte(s) included), if any.
    pub pps: Option<Vec<u8>>,
    /// Every other NAL unit, each prefixed by its 4-byte BE length.
    pub avcc: Vec<u8>,
    /// Whether a keyframe slice (H.264 IDR / HEVC IRAP) is among them.
    pub idr: bool,
}

impl AccessUnit {
    /// Split an H.264 Annex-B access unit.
    pub fn from_annexb(data: &[u8]) -> Self {
        Self::from_annexb_codec(VideoCodec::H264, data)
    }

    /// Split an Annex-B access unit of `codec`.
    pub fn from_annexb_codec(codec: VideoCodec, data: &[u8]) -> Self {
        let mut au = AccessUnit::default();
        for nal in annexb_nals(data) {
            let t = codec.nal_type(nal);
            let slot = match codec.param_set(t) {
                Some(ParamSet::Vps) => &mut au.vps,
                Some(ParamSet::Sps) => &mut au.sps,
                Some(ParamSet::Pps) => &mut au.pps,
                None if codec.is_aud(t) => continue,
                None => {
                    au.idr |= codec.is_keyframe_nal(t);
                    au.avcc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                    au.avcc.extend_from_slice(nal);
                    continue;
                }
            };
            slot.get_or_insert_with(|| nal.to_vec());
        }
        au
    }

    /// Whether every parameter set a decoder of `codec` needs to start
    /// (H.264: SPS + PPS; HEVC: VPS + SPS + PPS) is present.
    pub fn has_param_sets(&self, codec: VideoCodec) -> bool {
        self.sps.is_some()
            && self.pps.is_some()
            && (codec != VideoCodec::Hevc || self.vps.is_some())
    }
}

/// Encoder-side helper: remembers the last parameter sets seen and
/// prepends them to a keyframe access unit that lacks them (some encoders
/// only emit parameter sets with the first IDR, not with a forced one), so
/// every keyframe on the wire is self-contained.
#[derive(Debug, Clone, Default)]
pub struct ParamSetCache {
    codec: VideoCodec,
    vps: Option<Vec<u8>>,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

impl ParamSetCache {
    /// A cache for a `codec` stream ([`Default`] = H.264).
    pub fn new(codec: VideoCodec) -> Self {
        Self {
            codec,
            ..Self::default()
        }
    }

    /// Remember `au`'s parameter sets, rewrite its SPS colour signalling
    /// (H.264), and prepend the cached parameter sets to a keyframe that
    /// lacks them.
    pub fn fix(&mut self, au: Vec<u8>) -> Vec<u8> {
        let codec = self.codec;
        // Every H.264 SPS on the wire signals BT.709 limited range (what
        // `bgra_to_nv12` produces), whatever the encoder wrote. (HEVC: the
        // encoder's media type carries the colour description.)
        let au = if codec == VideoCodec::H264 {
            sps::fix_annexb(&au).unwrap_or(au)
        } else {
            au
        };
        let mut have = [false; 3];
        let mut key = false;
        for n in annexb_nals(&au) {
            let t = codec.nal_type(n);
            match codec.param_set(t) {
                Some(ps) => {
                    have[ps as usize] = true;
                    *self.slot(ps) = Some(n.to_vec());
                }
                None => key |= codec.is_keyframe_nal(t),
            }
        }
        let required = codec.required_param_sets();
        if !key || required.iter().all(|&ps| have[ps as usize]) {
            return au;
        }
        let mut sets = Vec::with_capacity(required.len());
        for &ps in required {
            match self.slot(ps) {
                Some(n) => sets.push(n.clone()),
                None => return au,
            }
        }
        let extra: usize = sets.iter().map(|s| s.len() + 4).sum();
        let mut out = Vec::with_capacity(au.len() + extra);
        for ps in sets {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&ps);
        }
        out.extend_from_slice(&au);
        out
    }

    fn slot(&mut self, ps: ParamSet) -> &mut Option<Vec<u8>> {
        match ps {
            ParamSet::Vps => &mut self.vps,
            ParamSet::Sps => &mut self.sps,
            ParamSet::Pps => &mut self.pps,
        }
    }
}
