//! H.264 SPS parsing / rewriting for the VUI colour description. The host
//! converts BGRA to NV12 with BT.709 limited-range coefficients
//! ([`super::bgra_to_nv12`]); decoders that guess (VideoToolbox defaults to
//! BT.601 for SD sizes, some for everything) shift every colour unless the SPS
//! says so explicitly. [`colour`] reads what an SPS signals;
//! `force_bt709_limited` rewrites one to signal
//! `video_full_range_flag = 0` and colour primaries / transfer / matrix = 1
//! (BT.709), inserting a VUI when the encoder wrote none and keeping every
//! other field (timing, HRD, bitstream restriction) bit-exact.

/// Colour fields of an SPS VUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Colour {
    /// `vui_parameters_present_flag`.
    pub vui_present: bool,
    /// `video_signal_type_present_flag`.
    pub video_signal_type_present: bool,
    /// `video_format` (5 = unspecified, the default).
    pub video_format: u8,
    /// `video_full_range_flag`.
    pub full_range: bool,
    /// `colour_description_present_flag`.
    pub colour_description_present: bool,
    /// `colour_primaries` (1 = BT.709, 2 = unspecified).
    pub primaries: u8,
    /// `transfer_characteristics` (1 = BT.709, 2 = unspecified).
    pub transfer: u8,
    /// `matrix_coefficients` (1 = BT.709, 2 = unspecified).
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
    /// `profile_idc` (66 Baseline, 77 Main, 100 High, ...).
    pub profile_idc: u8,
    /// `level_idc` (level x 10).
    pub level_idc: u8,
    /// `pic_width_in_mbs_minus1 + 1`.
    pub width_mbs: u32,
    /// `pic_height_in_map_units_minus1 + 1`.
    pub height_map_units: u32,
    /// `frame_mbs_only_flag`.
    pub frame_mbs_only: bool,
    /// `max_num_ref_frames`.
    pub max_num_ref_frames: u32,
    /// The VUI colour description.
    pub colour: Colour,
    /// `(num_units_in_tick, time_scale)` when timing info is present.
    pub timing: Option<(u32, u32)>,
    /// `max_num_reorder_frames` when the bitstream restriction is present.
    pub max_num_reorder_frames: Option<u32>,
}

/// Strip emulation-prevention bytes (`00 00 03` -> `00 00`).
pub(crate) fn unescape(nal: &[u8]) -> Vec<u8> {
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
pub(crate) fn escape(rbsp: &[u8]) -> Vec<u8> {
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
            // Wrapping: a malformed 32-bit code must not panic (debug builds).
            (k as i32).wrapping_add(1) / 2
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
    /// Append the low bit of `b`.
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
    /// Append the low `n` bits of `v`, most significant first.
    pub fn bits(&mut self, v: u32, n: u32) {
        for i in (0..n).rev() {
            self.bit((v >> i) & 1);
        }
    }
    /// Append `v` as unsigned Exp-Golomb (`ue(v)`).
    pub fn ue(&mut self, v: u32) {
        let x = u64::from(v) + 1;
        let len = 64 - x.leading_zeros();
        self.bits(0, len - 1);
        for i in (0..len).rev() {
            self.bit(((x >> i) & 1) as u32);
        }
    }
    /// Append `v` as signed Exp-Golomb (`se(v)`).
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
pub(crate) fn force_bt709_limited(nal: &[u8]) -> Option<Vec<u8>> {
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
pub(crate) fn fix_annexb(au: &[u8]) -> Option<Vec<u8>> {
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
