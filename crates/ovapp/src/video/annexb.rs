//! Annex-B / AVCC access-unit helpers: NAL splitting, out-of-band
//! parameter sets, and keeping every keyframe self-contained.

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
/// (VideoToolbox): SPS/PPS (first of each, if present) and the remaining NAL
/// units as AVCC — each prefixed by its 4-byte big-endian length. Access
/// unit delimiters are dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccessUnit {
    /// The first SPS NAL unit (header byte included), if any.
    pub sps: Option<Vec<u8>>,
    /// The first PPS NAL unit (header byte included), if any.
    pub pps: Option<Vec<u8>>,
    /// Every other NAL unit, each prefixed by its 4-byte BE length.
    pub avcc: Vec<u8>,
    /// Whether an IDR slice is among them.
    pub idr: bool,
}

impl AccessUnit {
    /// Split an Annex-B access unit.
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
    /// Remember `au`'s parameter sets, rewrite its SPS colour signalling, and
    /// prepend the cached SPS/PPS to an IDR that lacks them.
    pub fn fix(&mut self, au: Vec<u8>) -> Vec<u8> {
        // Every SPS on the wire signals BT.709 limited range
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
