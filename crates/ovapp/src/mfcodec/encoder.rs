//! The H.264 / HEVC encoder MFT (hardware first; H.264 falls back to
//! Microsoft's software MFT). Optional long-term-reference control
//! ([`EncodeParams::ltr_slots`], [`FrameRefs`]) for loss recovery without
//! an IDR.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use windows::core::{Interface, GUID};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Variant::VARIANT;

use super::gpu::GpuState;
use super::mft::{
    friendly_name, hardware_encoders, memory_sample, pull, sample_bytes, video_type, Pulled,
};
use super::{err, pack, startup, EncoderPref};
use crate::error::AppError;
use crate::video::{
    bgra_to_nv12, EncodedAu, FrameRefs, LtrInfo, ParamSetCache, VideoCodec, VideoEncode,
};

/// The rate-control buffer (VBV, bits) for `bitrate` at `fps`: one
/// frame's worth (floor 64 kbit, so tiny bitrates still fit an I-macroblock
/// row or two).
pub(crate) fn vbv_bits(bitrate: u32, fps: u32) -> u32 {
    (bitrate / fps.max(1)).max(64_000)
}

/// The H.264 profile requested on the encoder's output type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum H264Profile {
    /// Main (`profile_idc` 77): CABAC, no 8x8 transform.
    Main,
    /// High (`profile_idc` 100): Main plus the 8x8 transform and scaling
    /// matrices. Decoded by every H.264 decoder we target (MF,
    /// VideoToolbox).
    High,
}

impl H264Profile {
    /// The `eAVEncH264VProfile` value for `MF_MT_MPEG2_PROFILE`.
    fn mf(self) -> u32 {
        match self {
            Self::Main => eAVEncH264VProfile_Main.0 as u32,
            Self::High => eAVEncH264VProfile_High.0 as u32,
        }
    }

    /// The SPS `profile_idc` this profile signals.
    pub fn profile_idc(self) -> u8 {
        match self {
            Self::Main => 77,
            Self::High => 100,
        }
    }
}

/// What the encoder is asked for: picture size, rate, profile and (bench
/// only) a constant QP instead of CBR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct EncodeParams {
    pub w: u32,
    pub h: u32,
    pub fps: u32,
    pub bitrate: u32,
    /// Preferred profile; Main is used if the MFT refuses it.
    pub profile: H264Profile,
    /// Constant-QP rate control (quality mode) instead of CBR, for
    /// equal-quality size comparisons in benchmarks.
    pub qp: Option<u32>,
    /// H.264 or HEVC (HEVC: hardware MFTs only, Main profile).
    pub codec: VideoCodec,
    /// Application-controlled long-term reference slots (0 = off).
    pub ltr_slots: u8,
}

impl EncodeParams {
    /// The production setup: H.264 CBR at `bitrate`, High profile, no LTR.
    pub fn new(w: u32, h: u32, fps: u32, bitrate: u32) -> Self {
        Self {
            w,
            h,
            fps,
            bitrate,
            profile: H264Profile::High,
            qp: None,
            codec: VideoCodec::H264,
            ltr_slots: 0,
        }
    }

    /// The `MF_MT_SUBTYPE` of the encoder's output.
    pub(super) fn subtype(&self) -> GUID {
        match self.codec {
            VideoCodec::H264 => MFVideoFormat_H264,
            VideoCodec::Hevc => MFVideoFormat_HEVC,
        }
    }
}

/// Optional encoder features the active MFT reports through
/// `ICodecAPI::IsSupported` (probed once at creation, logged at debug).
/// Decides whether loss recovery can use long-term references (LTR) or
/// gradual intra refresh instead of a full IDR.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EncoderCaps {
    /// `CODECAPI_AVEncVideoLTRBufferControl` (number of LTR slots).
    pub ltr_buffer_control: bool,
    /// The LTR slot count currently configured (low 16 bits of the
    /// `LTRBufferControl` value; 0 = LTR off until set), when readable.
    pub ltr_slots: Option<u32>,
    /// `CODECAPI_AVEncVideoMarkLTRFrame` (mark the next frame as an LTR).
    pub mark_ltr: bool,
    /// `CODECAPI_AVEncVideoUseLTRFrame` (predict the next frame from LTRs).
    pub use_ltr: bool,
    /// `CODECAPI_AVEncVideoGradualIntraRefresh`.
    pub gradual_intra_refresh: bool,
    /// `CODECAPI_AVEncVideoTemporalLayerCount`.
    pub temporal_layers: bool,
    /// `CODECAPI_AVEncVideoMaxNumRefFrame`.
    pub max_ref_frames: bool,
    /// `CODECAPI_AVEncVideoForceKeyFrame`.
    pub force_keyframe: bool,
}

impl EncoderCaps {
    /// Probe `api` (no codec API at all = nothing supported).
    fn probe(api: Option<&ICodecAPI>) -> Self {
        let Some(api) = api else {
            return Self::default();
        };
        // SAFETY: `IsSupported` on a live ICodecAPI with a pointer to a
        // 'static GUID constant, which it only reads.
        let has = |g: &GUID| unsafe { api.IsSupported(g) }.is_ok();
        // SAFETY: as above; the returned VARIANT is owned (and freed) here.
        let ltr_slots = unsafe { api.GetValue(&CODECAPI_AVEncVideoLTRBufferControl) }
            .ok()
            .and_then(|v| u32::try_from(&v).ok())
            .map(|v| v & 0xFFFF);
        Self {
            ltr_slots,
            ltr_buffer_control: has(&CODECAPI_AVEncVideoLTRBufferControl),
            mark_ltr: has(&CODECAPI_AVEncVideoMarkLTRFrame),
            use_ltr: has(&CODECAPI_AVEncVideoUseLTRFrame),
            gradual_intra_refresh: has(&CODECAPI_AVEncVideoGradualIntraRefresh),
            temporal_layers: has(&CODECAPI_AVEncVideoTemporalLayerCount),
            max_ref_frames: has(&CODECAPI_AVEncVideoMaxNumRefFrame),
            force_keyframe: has(&CODECAPI_AVEncVideoForceKeyFrame),
        }
    }

    /// Full long-term-reference control (buffer, mark and use).
    pub fn ltr(&self) -> bool {
        self.ltr_buffer_control && self.mark_ltr && self.use_ltr
    }
}

/// Reject odd or tiny picture sizes before touching Media Foundation.
pub(super) fn check_size(w: u32, h: u32) -> Result<(), AppError> {
    if w < 16 || h < 16 || !w.is_multiple_of(2) || !h.is_multiple_of(2) {
        return Err(AppError::invalid(format!(
            "H.264 picture {w}x{h} must be even and at least 16x16"
        )));
    }
    Ok(())
}

/// Set the H.264 output type at `p.profile`, falling back to Main when the
/// MFT refuses it; returns the profile in effect.
///
/// # Safety
/// `mft` must be a live encoder transform (MF started on this thread).
unsafe fn set_output_type(mft: &IMFTransform, p: &EncodeParams) -> Result<H264Profile, AppError> {
    let mut first = None;
    for profile in [p.profile, H264Profile::Main] {
        let mf_profile = match p.codec {
            VideoCodec::H264 => profile.mf(),
            VideoCodec::Hevc => eAVEncH265VProfile_Main_420_8.0 as u32,
        };
        // SAFETY: MF calls on a media type created here and on the caller's
        // live transform (see the function contract).
        let r = unsafe {
            video_type(&p.subtype(), p.w, p.h, p.fps).and_then(|out| {
                out.SetUINT32(&MF_MT_AVG_BITRATE, p.bitrate)?;
                out.SetUINT32(&MF_MT_MPEG2_PROFILE, mf_profile)?;
                mft.SetOutputType(0, &out, 0)
            })
        };
        match r {
            Ok(()) => return Ok(profile),
            Err(e) => {
                tracing::debug!("H.264 output type at {profile:?} refused: {e}");
                first.get_or_insert(e);
            }
        }
        if p.profile == H264Profile::Main || p.codec == VideoCodec::Hevc {
            break;
        }
    }
    let e = first.unwrap_or_else(windows::core::Error::empty);
    Err(err("SetOutputType", e))
}

/// Hardware-or-software H.264 (or hardware HEVC) encoder MFT (see the
/// module docs).
pub struct H264Encoder {
    mft: IMFTransform,
    activate: Option<IMFActivate>,
    events: Option<IMFMediaEventGenerator>,
    api: Option<ICodecAPI>,
    name: String,
    hardware: bool,
    profile: H264Profile,
    caps: EncoderCaps,
    codec: VideoCodec,
    /// LTR slots in effect (0 = LTR control off / unsupported).
    ltr_slots: u8,
    /// Reference control for the next submitted picture.
    next_refs: FrameRefs,
    /// What was asked per submitted sample time (for MFTs that do not
    /// report `MFSampleExtension_LongTermReferenceFrameInfo`).
    asked: VecDeque<(i64, FrameRefs)>,
    /// LTR slots the MFT auto-marks after a keyframe that are still free
    /// (emulation for MFTs that do not report LTR info).
    auto_marks: u8,
    pub(super) w: u32,
    pub(super) h: u32,
    fps: u32,
    nv12: Vec<u8>,
    out_info: MFT_OUTPUT_STREAM_INFO,
    /// Async MFT: METransformNeedInput credits.
    need_input: u32,
    outputs: u64,
    ps: ParamSetCache,
    scratch: Vec<u8>,
    last_time: i64,
    /// D3D11 input (video processor + NV12 surface ring), when created by
    /// [`new_gpu`](Self::new_gpu).
    pub(super) gpu: Option<GpuState>,
}

impl H264Encoder {
    /// An encoder for `w x h` (even) pictures at `fps`, `bitrate` bits/s
    /// (CBR, High profile when the MFT takes it).
    pub fn new(
        w: u32,
        h: u32,
        fps: u32,
        bitrate: u32,
        pref: EncoderPref,
    ) -> Result<Self, AppError> {
        Self::with_params(&EncodeParams::new(w, h, fps, bitrate), pref)
    }

    /// [`new`](Self::new) with explicit [`EncodeParams`].
    pub(super) fn with_params(p: &EncodeParams, pref: EncoderPref) -> Result<Self, AppError> {
        check_size(p.w, p.h)?;
        startup()?;
        let mut why = Vec::new();
        // SAFETY: COM/MF calls on activation objects and transforms this
        // function owns; MF + COM were initialised by `startup` above.
        unsafe {
            if pref != EncoderPref::Software {
                for a in hardware_encoders(&p.subtype()) {
                    let name = friendly_name(&a);
                    match a.ActivateObject::<IMFTransform>() {
                        Ok(mft) => match Self::configure(
                            mft,
                            Some(a.clone()),
                            format!("{name} (hw)"),
                            true,
                            p,
                            None,
                        ) {
                            Ok(e) => return Ok(e),
                            Err(e) => {
                                let _ = a.ShutdownObject();
                                why.push(format!("{name}: {e}"));
                            }
                        },
                        Err(e) => why.push(format!("{name}: activate: {e}")),
                    }
                }
                if why.is_empty() {
                    why.push(format!("no hardware {} encoder registered", p.codec));
                }
            }
            if pref != EncoderPref::Hardware && p.codec == VideoCodec::H264 {
                let mft: IMFTransform =
                    CoCreateInstance(&CLSID_MSH264EncoderMFT, None, CLSCTX_INPROC_SERVER)
                        .map_err(|e| err("software H.264 encoder", e))?;
                return Self::configure(
                    mft,
                    None,
                    "Microsoft H.264 Encoder MFT (sw)".into(),
                    false,
                    p,
                    None,
                )
                .map_err(|e| AppError::platform(format!("{e} (hardware: {})", why.join("; "))));
            }
        }
        Err(AppError::platform(why.join("; ")))
    }

    pub(super) fn configure(
        mft: IMFTransform,
        activate: Option<IMFActivate>,
        name: String,
        hardware: bool,
        p: &EncodeParams,
        d3d: Option<&IMFDXGIDeviceManager>,
    ) -> Result<Self, AppError> {
        let &EncodeParams {
            w, h, fps, bitrate, ..
        } = p;
        // SAFETY: COM/MF calls on the transform passed in (owned here) and on
        // objects created from it; `d3d` is a live manager the MFT AddRefs.
        unsafe {
            let mut is_async = false;
            if let Ok(a) = mft.GetAttributes() {
                is_async = a.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0;
                if is_async {
                    a.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                        .map_err(|e| err("async unlock", e))?;
                }
                let _ = a.SetUINT32(&MF_LOW_LATENCY, 1);
            }
            // D3D11 input: the MFT takes our device before its types.
            if let Some(m) = d3d {
                mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, m.as_raw() as usize)
                    .map_err(|e| err("D3D manager", e))?;
            }
            let api: Option<ICodecAPI> = mft.cast().ok();
            if let Some(api) = &api {
                // Best effort: not every MFT supports every knob.
                let set = |g: &GUID, v: VARIANT| {
                    let _ = api.SetValue(g, &v);
                };
                set(&CODECAPI_AVLowLatencyMode, VARIANT::from(true));
                if let Some(qp) = p.qp {
                    set(
                        &CODECAPI_AVEncCommonRateControlMode,
                        VARIANT::from(eAVEncCommonRateControlMode_Quality.0 as u32),
                    );
                    set(&CODECAPI_AVEncVideoEncodeQP, VARIANT::from(u64::from(qp)));
                } else {
                    set(
                        &CODECAPI_AVEncCommonRateControlMode,
                        VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32),
                    );
                    set(&CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(bitrate));
                    // VBV of about one frame of bits + a peak cap, so a
                    // single frame (an IDR, a burst of motion) cannot queue
                    // several frame times of data in front of the next one.
                    set(
                        &CODECAPI_AVEncCommonBufferSize,
                        VARIANT::from(vbv_bits(bitrate, fps)),
                    );
                    set(&CODECAPI_AVEncCommonMaxBitRate, VARIANT::from(bitrate));
                }
                set(&CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0u32));
                // Long GOP (an IDR every ~60 s as a safety net); IDRs on demand.
                set(&CODECAPI_AVEncMPVGOPSize, VARIANT::from(fps.max(1) * 60));
                if p.ltr_slots > 0 {
                    // N application-controlled slots, trust mode 1 ("trust
                    // until" invalidated by UseLTRFrame). A static setting:
                    // before the media types.
                    set(
                        &CODECAPI_AVEncVideoLTRBufferControl,
                        VARIANT::from(u32::from(p.ltr_slots) | (1 << 16)),
                    );
                }
            }
            let profile = set_output_type(&mft, p)?;
            let inp =
                video_type(&MFVideoFormat_NV12, w, h, fps).map_err(|e| err("input type", e))?;
            let _ = inp.SetUINT32(&MF_MT_DEFAULT_STRIDE, w);
            if let Err(e) = mft.SetInputType(0, &inp, 0) {
                // Some hardware MFTs only take their own offered type objects.
                let mut done = false;
                for i in 0..64 {
                    let Ok(t) = mft.GetInputAvailableType(0, i) else {
                        break;
                    };
                    if t.GetGUID(&MF_MT_SUBTYPE).ok() == Some(MFVideoFormat_NV12) {
                        let _ = t.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h));
                        let _ = t.SetUINT64(&MF_MT_FRAME_RATE, pack(fps.max(1), 1));
                        if mft.SetInputType(0, &t, 0).is_ok() {
                            done = true;
                            break;
                        }
                    }
                }
                if !done {
                    return Err(err("SetInputType(NV12)", e));
                }
            }
            let _ = mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|e| err("begin streaming", e))?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|e| err("start of stream", e))?;
            let events = if is_async {
                Some(
                    mft.cast::<IMFMediaEventGenerator>()
                        .map_err(|e| err("event generator", e))?,
                )
            } else {
                None
            };
            let out_info = mft
                .GetOutputStreamInfo(0)
                .map_err(|e| err("output stream info", e))?;
            let caps = EncoderCaps::probe(api.as_ref());
            // LTR control only when the MFT supports all three knobs and
            // took the slot count.
            let ltr_slots = if p.ltr_slots > 0 && caps.ltr() {
                match caps.ltr_slots {
                    Some(n) if n > 0 => n.min(u32::from(p.ltr_slots)) as u8,
                    // Not readable: trust the set.
                    None => p.ltr_slots,
                    Some(_) => 0,
                }
            } else {
                0
            };
            tracing::debug!(
                encoder = %name, codec = %p.codec, ?profile, ?caps, ltr_slots,
                "video encoder created"
            );
            Ok(Self {
                mft,
                activate,
                events,
                api,
                name,
                hardware,
                profile,
                caps,
                codec: p.codec,
                ltr_slots,
                next_refs: FrameRefs::default(),
                asked: VecDeque::new(),
                auto_marks: 0,
                w,
                h,
                fps: fps.max(1),
                nv12: Vec::new(),
                out_info,
                need_input: 0,
                outputs: 0,
                ps: ParamSetCache::new(p.codec),
                scratch: Vec::new(),
                last_time: i64::MIN,
                gpu: None,
            })
        }
    }

    /// Whether this is a hardware MFT.
    pub fn is_hardware(&self) -> bool {
        self.hardware
    }

    /// The H.264 profile in effect (High unless the MFT refused it).
    pub fn profile(&self) -> H264Profile {
        self.profile
    }

    /// Optional features of this MFT (LTR, intra refresh, ...).
    pub fn caps(&self) -> EncoderCaps {
        self.caps
    }

    /// The picture size `(w, h)` the encoder was created for.
    pub fn size(&self) -> (u32, u32) {
        (self.w, self.h)
    }

    /// The codec this encoder produces.
    pub fn codec(&self) -> VideoCodec {
        self.codec
    }

    /// LTR slots under application control (0 = off / unsupported).
    pub fn ltr_slots(&self) -> u8 {
        self.ltr_slots
    }

    /// Reference control for the next picture (see [`FrameRefs`]).
    pub fn set_frame_refs(&mut self, refs: FrameRefs) {
        if self.ltr_slots > 0 {
            self.next_refs = refs;
        }
    }

    /// Apply the pending [`FrameRefs`] to the next picture (submitted with
    /// sample time `t`).
    ///
    /// # Safety
    /// Calls the encoder's own live codec API.
    unsafe fn apply_refs(&mut self, t: i64, force_idr: bool) {
        let refs = std::mem::take(&mut self.next_refs);
        let Some(api) = &self.api else {
            return;
        };
        if self.ltr_slots == 0 {
            return;
        }
        let n = self.ltr_slots;
        let mark = refs.mark_ltr.filter(|&k| k < n);
        let use_mask = refs.use_ltr & ((1u16 << n) - 1);
        // SAFETY: best-effort ICodecAPI calls on the encoder's own
        // interface with valid VARIANT references (function contract).
        unsafe {
            if let Some(k) = mark {
                let _ = api.SetValue(
                    &CODECAPI_AVEncVideoMarkLTRFrame,
                    &VARIANT::from(u32::from(k)),
                );
            }
            if use_mask != 0 && !force_idr {
                // Bits 16..31 = 1: later pictures use nothing older.
                let v = u32::from(use_mask) | (1 << 16);
                let _ = api.SetValue(&CODECAPI_AVEncVideoUseLTRFrame, &VARIANT::from(v));
            }
        }
        let asked = FrameRefs {
            mark_ltr: mark,
            use_ltr: if force_idr { 0 } else { use_mask },
        };
        self.asked.push_back((t, asked));
        while self.asked.len() > 16 {
            self.asked.pop_front();
        }
    }

    /// The LTR info of output picture `s` (sample time `t`): what the MFT
    /// reports, or else what was asked for plus the documented auto-marking
    /// of the first `ltr_slots` pictures from a keyframe.
    ///
    /// # Safety
    /// `s` must be a live sample the encoder just produced.
    unsafe fn ltr_info(&mut self, s: &IMFSample, t: i64, keyframe: bool) -> LtrInfo {
        if self.ltr_slots == 0 {
            return LtrInfo::default();
        }
        let asked = self
            .asked
            .iter()
            .position(|&(at, _)| at == t)
            .and_then(|i| self.asked.remove(i))
            .map(|(_, r)| r)
            .unwrap_or_default();
        // SAFETY: an attribute read on the live output sample.
        let reported = unsafe { s.GetUINT32(&MFSampleExtension_LongTermReferenceFrameInfo) };
        if let Ok(v) = reported {
            let idx = (v & 0xFFFF) as u16;
            return LtrInfo {
                marked: (idx != 0xFFFF && idx < u16::from(self.ltr_slots)).then_some(idx as u8),
                used: (v >> 16) as u16,
            };
        }
        // Emulation: a keyframe drops every LTR; the MFT then marks the
        // keyframe and the following pictures into the free slots.
        if keyframe {
            self.auto_marks = 0;
        }
        let marked = asked.mark_ltr.or_else(|| {
            (self.auto_marks < self.ltr_slots).then(|| {
                let k = self.auto_marks;
                self.auto_marks += 1;
                k
            })
        });
        if let Some(k) = asked.mark_ltr {
            self.auto_marks = self.auto_marks.max(k + 1);
        }
        LtrInfo {
            marked,
            used: asked.use_ltr,
        }
    }

    /// Encode one NV12 picture already in `nv12` (see [`encode`](VideoEncode::encode)).
    pub fn encode_nv12(
        &mut self,
        nv12: &[u8],
        force_idr: bool,
        pts_us: u64,
    ) -> Result<Vec<EncodedAu>, AppError> {
        let want = crate::video::nv12_len(self.w, self.h);
        if nv12.len() < want {
            return Err(AppError::invalid(format!(
                "NV12 input is {} bytes, want {want}",
                nv12.len()
            )));
        }
        let sample = memory_sample(&nv12[..want], want).map_err(|e| err("input sample", e))?;
        self.submit(sample, force_idr, pts_us)
    }

    /// Timestamp, optionally force an IDR, and run one input sample
    /// through the MFT (sync or async).
    pub(super) fn submit(
        &mut self,
        sample: IMFSample,
        force_idr: bool,
        pts_us: u64,
    ) -> Result<Vec<EncodedAu>, AppError> {
        // SAFETY: COM calls on the encoder's own transform and codec API, and on
        // the live sample passed in.
        unsafe {
            // Strictly increasing 100 ns timestamps (rate control uses them).
            let mut t = (pts_us as i64).saturating_mul(10);
            if t <= self.last_time {
                t = self.last_time + 1;
            }
            self.last_time = t;
            let _ = sample.SetSampleTime(t);
            let _ = sample.SetSampleDuration(10_000_000 / i64::from(self.fps));
            if force_idr {
                if let Some(api) = &self.api {
                    let _ = api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32));
                }
            }
            self.apply_refs(t, force_idr);
            let mut out = Vec::new();
            match self.events.clone() {
                Some(ev) => self.encode_async(&ev, &sample, &mut out)?,
                None => self.encode_sync(&sample, &mut out)?,
            }
            Ok(out)
        }
    }

    fn encode_sync(
        &mut self,
        sample: &IMFSample,
        out: &mut Vec<EncodedAu>,
    ) -> Result<(), AppError> {
        // SAFETY: `ProcessInput` on the encoder's own transform with a live sample.
        unsafe {
            if let Err(e) = self.mft.ProcessInput(0, sample, 0) {
                if e.code() != MF_E_NOTACCEPTING {
                    return Err(err("ProcessInput", e));
                }
                self.drain(out)?;
                self.mft
                    .ProcessInput(0, sample, 0)
                    .map_err(|e| err("ProcessInput", e))?;
            }
            self.drain(out)
        }
    }

    /// Pull every output available now (sync MFT).
    fn drain(&mut self, out: &mut Vec<EncodedAu>) -> Result<(), AppError> {
        for _ in 0..16 {
            match self.take_output()? {
                Some(au) => out.push(au),
                None => return Ok(()),
            }
        }
        Ok(())
    }

    /// One ProcessOutput, handling a stream change. `None` = needs input.
    fn take_output(&mut self) -> Result<Option<EncodedAu>, AppError> {
        // SAFETY: COM calls on the encoder's own transform and on the sample it
        // just returned.
        unsafe {
            for _ in 0..3 {
                let cap = (self.w * self.h) as usize;
                match pull(&self.mft, &self.out_info, cap)? {
                    Pulled::NeedInput => return Ok(None),
                    Pulled::StreamChange => {
                        let t = self
                            .mft
                            .GetOutputAvailableType(0, 0)
                            .map_err(|e| err("output type after change", e))?;
                        self.mft
                            .SetOutputType(0, &t, 0)
                            .map_err(|e| err("SetOutputType", e))?;
                        self.out_info = self
                            .mft
                            .GetOutputStreamInfo(0)
                            .map_err(|e| err("output info", e))?;
                    }
                    Pulled::Sample(s) => {
                        let t = s.GetSampleTime().unwrap_or(0);
                        sample_bytes(&s, &mut self.scratch).map_err(|e| err("output bytes", e))?;
                        let data = self.ps.fix(std::mem::take(&mut self.scratch));
                        // The bitstream is the truth (MFSampleExtension_CleanPoint
                        // is not set by every MFT).
                        let keyframe = self.codec.annexb_is_keyframe(&data);
                        let ltr = self.ltr_info(&s, t, keyframe);
                        self.outputs += 1;
                        return Ok(Some(EncodedAu {
                            data,
                            keyframe,
                            pts_us: (t / 10).max(0) as u64,
                            ltr,
                        }));
                    }
                }
            }
            Err(AppError::codec("encoder keeps changing its output type"))
        }
    }

    /// Handle one pending async event. `Ok(false)` = none available.
    fn poll_event(
        &mut self,
        ev: &IMFMediaEventGenerator,
        out: &mut Vec<EncodedAu>,
    ) -> Result<bool, AppError> {
        // SAFETY: COM calls on the encoder's own event generator and the event it
        // returned.
        unsafe {
            let e = match ev.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                Ok(e) => e,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(false),
                Err(e) => return Err(err("GetEvent", e)),
            };
            let ty = MF_EVENT_TYPE(e.GetType().map_err(|e| err("event type", e))? as i32);
            if ty == METransformNeedInput {
                self.need_input += 1;
            } else if ty == METransformHaveOutput {
                if let Some(au) = self.take_output()? {
                    out.push(au);
                }
            }
            Ok(true)
        }
    }

    fn encode_async(
        &mut self,
        ev: &IMFMediaEventGenerator,
        sample: &IMFSample,
        out: &mut Vec<EncodedAu>,
    ) -> Result<(), AppError> {
        // SAFETY: `ProcessInput` on the encoder's own transform with a live sample.
        unsafe {
            let wait = |since: Instant| {
                if since.elapsed() < Duration::from_millis(10) {
                    std::thread::yield_now();
                } else {
                    std::thread::sleep(Duration::from_micros(200));
                }
            };
            // 1. An input slot.
            let t0 = Instant::now();
            while self.need_input == 0 {
                if !self.poll_event(ev, out)? {
                    if t0.elapsed() > Duration::from_millis(500) {
                        return Err(AppError::codec("hardware encoder never asked for input"));
                    }
                    wait(t0);
                }
            }
            self.mft
                .ProcessInput(0, sample, 0)
                .map_err(|e| err("ProcessInput", e))?;
            self.need_input -= 1;
            // 2. This picture's output (low latency: one in, one out), bounded.
            let before = self.outputs;
            let t1 = Instant::now();
            while self.outputs == before && t1.elapsed() < Duration::from_millis(100) {
                if !self.poll_event(ev, out)? {
                    wait(t1);
                }
            }
            // 3. Whatever else is ready right now (input credits included).
            while self.poll_event(ev, out)? {}
            Ok(())
        }
    }
}

impl VideoEncode for H264Encoder {
    fn encode(
        &mut self,
        bgra: &[u8],
        stride: usize,
        force_idr: bool,
        pts_us: u64,
    ) -> Result<Vec<EncodedAu>, String> {
        let need = stride * (self.h as usize - 1) + self.w as usize * 4;
        if bgra.len() < need {
            return Err(format!("BGRA input is {} bytes, want {need}", bgra.len()));
        }
        let mut nv12 = std::mem::take(&mut self.nv12);
        bgra_to_nv12(bgra, stride, self.w, self.h, &mut nv12);
        let r = self.encode_nv12(&nv12, force_idr, pts_us);
        self.nv12 = nv12;
        r.map_err(String::from)
    }

    fn set_bitrate(&mut self, bps: u32) {
        if let Some(api) = &self.api {
            // SAFETY: best-effort ICodecAPI calls on the encoder's own
            // interface with valid VARIANT references.
            unsafe {
                let _ = api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &VARIANT::from(bps));
                let _ = api.SetValue(&CODECAPI_AVEncCommonMaxBitRate, &VARIANT::from(bps));
                let _ = api.SetValue(
                    &CODECAPI_AVEncCommonBufferSize,
                    &VARIANT::from(vbv_bits(bps, self.fps)),
                );
            }
        }
    }

    fn name(&self) -> String {
        self.name.clone()
    }

    fn codec(&self) -> VideoCodec {
        self.codec
    }

    fn ltr_slots(&self) -> u8 {
        self.ltr_slots
    }

    fn set_frame_refs(&mut self, refs: FrameRefs) {
        H264Encoder::set_frame_refs(self, refs);
    }
}

impl Drop for H264Encoder {
    fn drop(&mut self) {
        // SAFETY: end-of-stream messages to the transform we own, and
        // shutting down its activation object, once, as it is dropped.
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Some(a) = &self.activate {
                let _ = a.ShutdownObject();
            }
        }
    }
}
