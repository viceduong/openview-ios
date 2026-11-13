//! The H.264 encoder MFT (hardware first, software fallback).

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
use crate::video::{annexb_is_idr, bgra_to_nv12, EncodedAu, ParamSetCache, VideoEncode};

/// The rate-control buffer (VBV, bits) for `bitrate` at `fps`: one
/// frame's worth (floor 64 kbit, so tiny bitrates still fit an I-macroblock
/// row or two).
pub(crate) fn vbv_bits(bitrate: u32, fps: u32) -> u32 {
    (bitrate / fps.max(1)).max(64_000)
}

/// Hardware-or-software H.264 encoder MFT (see the module docs).
pub struct H264Encoder {
    mft: IMFTransform,
    activate: Option<IMFActivate>,
    events: Option<IMFMediaEventGenerator>,
    api: Option<ICodecAPI>,
    name: String,
    hardware: bool,
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
    /// An encoder for `w x h` (even) pictures at `fps`, `bitrate` bits/s.
    pub fn new(
        w: u32,
        h: u32,
        fps: u32,
        bitrate: u32,
        pref: EncoderPref,
    ) -> Result<Self, AppError> {
        if w < 16 || h < 16 || !w.is_multiple_of(2) || !h.is_multiple_of(2) {
            return Err(AppError::invalid(format!(
                "H.264 picture {w}x{h} must be even and at least 16x16"
            )));
        }
        startup()?;
        let mut why = Vec::new();
        // SAFETY: COM/MF calls on activation objects and transforms this
        // function owns; MF + COM were initialised by `startup` above.
        unsafe {
            if pref != EncoderPref::Software {
                for a in hardware_encoders() {
                    let name = friendly_name(&a);
                    match a.ActivateObject::<IMFTransform>() {
                        Ok(mft) => match Self::configure(
                            mft,
                            Some(a.clone()),
                            format!("{name} (hw)"),
                            true,
                            w,
                            h,
                            fps,
                            bitrate,
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
                    why.push("no hardware H.264 encoder registered".into());
                }
            }
            if pref != EncoderPref::Hardware {
                let mft: IMFTransform =
                    CoCreateInstance(&CLSID_MSH264EncoderMFT, None, CLSCTX_INPROC_SERVER)
                        .map_err(|e| err("software H.264 encoder", e))?;
                return Self::configure(
                    mft,
                    None,
                    "Microsoft H.264 Encoder MFT (sw)".into(),
                    false,
                    w,
                    h,
                    fps,
                    bitrate,
                    None,
                )
                .map_err(|e| AppError::platform(format!("{e} (hardware: {})", why.join("; "))));
            }
        }
        Err(AppError::platform(why.join("; ")))
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn configure(
        mft: IMFTransform,
        activate: Option<IMFActivate>,
        name: String,
        hardware: bool,
        w: u32,
        h: u32,
        fps: u32,
        bitrate: u32,
        d3d: Option<&IMFDXGIDeviceManager>,
    ) -> Result<Self, AppError> {
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
                set(
                    &CODECAPI_AVEncCommonRateControlMode,
                    VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32),
                );
                set(&CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(bitrate));
                // VBV of about one frame of bits + a peak cap, so a
                // single frame (an IDR, a burst of motion) cannot queue several
                // frame times of data in front of the next one.
                set(
                    &CODECAPI_AVEncCommonBufferSize,
                    VARIANT::from(vbv_bits(bitrate, fps)),
                );
                set(&CODECAPI_AVEncCommonMaxBitRate, VARIANT::from(bitrate));
                set(&CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0u32));
                // Long GOP (an IDR every ~60 s as a safety net); IDRs on demand.
                set(&CODECAPI_AVEncMPVGOPSize, VARIANT::from(fps.max(1) * 60));
            }
            let out =
                video_type(&MFVideoFormat_H264, w, h, fps).map_err(|e| err("output type", e))?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)
                .map_err(|e| err("bitrate", e))?;
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)
                .map_err(|e| err("profile", e))?;
            mft.SetOutputType(0, &out, 0)
                .map_err(|e| err("SetOutputType", e))?;
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
            Ok(Self {
                mft,
                activate,
                events,
                api,
                name,
                hardware,
                w,
                h,
                fps: fps.max(1),
                nv12: Vec::new(),
                out_info,
                need_input: 0,
                outputs: 0,
                ps: ParamSetCache::default(),
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

    /// The picture size `(w, h)` the encoder was created for.
    pub fn size(&self) -> (u32, u32) {
        (self.w, self.h)
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
                        let keyframe = annexb_is_idr(&data);
                        self.outputs += 1;
                        return Ok(Some(EncodedAu {
                            data,
                            keyframe,
                            pts_us: (t / 10).max(0) as u64,
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
