//! Phase 5: Media Foundation H.264 encoder / decoder MFTs (Windows, `mf`
//! feature — `ovhost` encodes, `ovview` decodes).
//!
//! **Encoder** ([`H264Encoder`]): a hardware MFT first (NVENC / AMF / Quick
//! Sync through `MFTEnumEx(MFT_ENUM_FLAG_HARDWARE)`, driven with the async
//! event model), falling back to Microsoft's software MFT (synchronous).
//! Low-latency mode, no B-frames, CBR at the requested bitrate (retargeted
//! at run time from the link pacer), a long GOP and IDR on demand
//! (`CODECAPI_AVEncVideoForceKeyFrame`). Input is system-memory NV12
//! converted from the captured BGRA on the CPU ([`crate::video::bgra_to_nv12`]).
//! Output is Annex-B; every IDR is made self-contained (SPS/PPS) by
//! [`ParamSetCache`].
//!
//! **Decoder** ([`H264Decoder`]): Microsoft's H.264 decoder MFT (software,
//! low-latency mode: one access unit in, one picture out), NV12 converted to
//! BGRA on the CPU.

use std::cell::Cell;
use std::mem::ManuallyDrop;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use windows::core::{Interface, GUID, PWSTR};
use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
    DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::System::Variant::VARIANT;

use crate::video::{
    annexb_is_idr, bgra_to_nv12, nv12_to_bgra, EncodedAu, ParamSetCache, VideoDecode, VideoEncode,
};

/// Why [`available`] is false, for logs and errors.
pub const UNAVAILABLE: &str =
    "Media Foundation is not installed (Windows N/KN without the Media Feature Pack)";

/// Whether Media Foundation (`mfplat.dll`) is present. Windows N/KN editions
/// lack it until the user installs the Media Feature Pack; `ovhost` and
/// `ovview` **delay-load** `mfplat.dll` (see their build.rs), so no MF
/// function may run before this probe succeeded — [`startup`] checks it,
/// and every public constructor here calls [`startup`] first.
pub fn available() -> bool {
    use windows::core::w;
    use windows::Win32::System::LibraryLoader::{LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    // SAFETY: loading a system DLL by name from System32 only; the module
    // stays loaded for the process (the delay-load stub then binds to it).
    *AVAILABLE.get_or_init(|| unsafe {
        LoadLibraryExW(w!("mfplat.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32).is_ok()
    })
}

/// MF + COM for this thread (COM once per thread; MF once per process).
/// Fails with [`UNAVAILABLE`] when Media Foundation is missing.
fn startup() -> Result<(), String> {
    if !available() {
        return Err(UNAVAILABLE.to_owned());
    }
    thread_local! {
        static COM: Cell<bool> = const { Cell::new(false) };
    }
    COM.with(|c| {
        if !c.get() {
            // RPC_E_CHANGED_MODE (an STA thread) is fine: COM is usable.
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            c.set(true);
        }
    });
    static MF: OnceLock<Result<(), String>> = OnceLock::new();
    MF.get_or_init(|| {
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) }.map_err(|e| format!("MFStartup: {e}"))
    })
    .clone()
}

fn err(what: &str, e: windows::core::Error) -> String {
    format!("{what}: {e}")
}

/// Which encoder [`H264Encoder::new`] may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderPref {
    /// Hardware first, software fallback.
    Auto,
    Hardware,
    Software,
}

/// Pack two u32s into an MF "size"/"ratio" attribute.
fn pack(hi: u32, lo: u32) -> u64 {
    (u64::from(hi) << 32) | u64::from(lo)
}

unsafe fn friendly_name(a: &IMFActivate) -> String {
    let mut p = PWSTR::null();
    let mut n = 0u32;
    if a.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut p, &mut n)
        .is_ok()
        && !p.is_null()
    {
        let s = p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    } else {
        "unnamed MFT".into()
    }
}

/// Registered hardware H.264 encoders (best first).
unsafe fn hardware_encoders() -> Vec<IMFActivate> {
    let out = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let mut pp: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut n = 0u32;
    let flags = MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0);
    if MFTEnumEx(
        MFT_CATEGORY_VIDEO_ENCODER,
        flags,
        None,
        Some(&out),
        &mut pp,
        &mut n,
    )
    .is_err()
        || pp.is_null()
    {
        return Vec::new();
    }
    let mut v = Vec::with_capacity(n as usize);
    for i in 0..n as usize {
        if let Some(a) = (*pp.add(i)).take() {
            v.push(a);
        }
    }
    CoTaskMemFree(Some(pp as *const _));
    v
}

unsafe fn video_type(
    subtype: &GUID,
    w: u32,
    h: u32,
    fps: u32,
) -> windows::core::Result<IMFMediaType> {
    let t = MFCreateMediaType()?;
    t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    t.SetGUID(&MF_MT_SUBTYPE, subtype)?;
    t.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h))?;
    t.SetUINT64(&MF_MT_FRAME_RATE, pack(fps.max(1), 1))?;
    t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
    t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    // BT.709 limited range: what `bgra_to_nv12` produces.
    let _ = t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32);
    let _ = t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32);
    // Section 4: full colour description (the SPS VUI the encoder writes;
    // `video::sps` rewrites the SPS when an encoder ignores these).
    let _ = t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32);
    let _ = t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32);
    Ok(t)
}

unsafe fn memory_sample(data: &[u8], cap: usize) -> windows::core::Result<IMFSample> {
    let buf = MFCreateMemoryBuffer(cap.max(data.len()).max(1) as u32)?;
    if !data.is_empty() {
        let mut p: *mut u8 = std::ptr::null_mut();
        buf.Lock(&mut p, None, None)?;
        std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
        buf.Unlock()?;
    }
    buf.SetCurrentLength(data.len() as u32)?;
    let s = MFCreateSample()?;
    s.AddBuffer(&buf)?;
    Ok(s)
}

/// Copy a sample's bytes out (contiguous).
unsafe fn sample_bytes(s: &IMFSample, out: &mut Vec<u8>) -> windows::core::Result<()> {
    let buf = s.ConvertToContiguousBuffer()?;
    let mut p: *mut u8 = std::ptr::null_mut();
    let mut len = 0u32;
    buf.Lock(&mut p, None, Some(&mut len))?;
    out.clear();
    if !p.is_null() {
        out.extend_from_slice(std::slice::from_raw_parts(p, len as usize));
    }
    buf.Unlock()?;
    Ok(())
}

enum Pulled {
    Sample(IMFSample),
    NeedInput,
    StreamChange,
}

/// One `ProcessOutput` call on stream 0.
unsafe fn pull(
    mft: &IMFTransform,
    info: &MFT_OUTPUT_STREAM_INFO,
    fallback_cap: usize,
) -> Result<Pulled, String> {
    let provides = info.dwFlags
        & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32
        != 0;
    let ours = if provides {
        None
    } else {
        let cap = (info.cbSize as usize).max(fallback_cap);
        Some(memory_sample(&[], cap).map_err(|e| err("output sample", e))?)
    };
    let mut odb = [MFT_OUTPUT_DATA_BUFFER {
        dwStreamID: 0,
        pSample: ManuallyDrop::new(ours),
        dwStatus: 0,
        pEvents: ManuallyDrop::new(None),
    }];
    let mut status = 0u32;
    let r = mft.ProcessOutput(0, &mut odb, &mut status);
    let sample = ManuallyDrop::take(&mut odb[0].pSample);
    drop(ManuallyDrop::take(&mut odb[0].pEvents));
    match r {
        Ok(()) => sample
            .map(Pulled::Sample)
            .ok_or_else(|| "ProcessOutput returned no sample".to_owned()),
        Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(Pulled::NeedInput),
        Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => Ok(Pulled::StreamChange),
        Err(e) => Err(err("ProcessOutput", e)),
    }
}

/// Wave 1: the rate-control buffer (VBV, bits) for `bitrate` at `fps`: one
/// frame's worth (floor 64 kbit, so tiny bitrates still fit an I-macroblock
/// row or two).
pub fn vbv_bits(bitrate: u32, fps: u32) -> u32 {
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
    w: u32,
    h: u32,
    fps: u32,
    nv12: Vec<u8>,
    out_info: MFT_OUTPUT_STREAM_INFO,
    /// Async MFT: METransformNeedInput credits.
    need_input: u32,
    outputs: u64,
    ps: ParamSetCache,
    scratch: Vec<u8>,
    last_time: i64,
    /// Section 5: D3D11 input (video processor + NV12 surface ring).
    gpu: Option<GpuState>,
}

impl H264Encoder {
    /// An encoder for `w x h` (even) pictures at `fps`, `bitrate` bits/s.
    pub fn new(w: u32, h: u32, fps: u32, bitrate: u32, pref: EncoderPref) -> Result<Self, String> {
        if w < 16 || h < 16 || !w.is_multiple_of(2) || !h.is_multiple_of(2) {
            return Err(format!(
                "H.264 picture {w}x{h} must be even and at least 16x16"
            ));
        }
        startup()?;
        let mut why = Vec::new();
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
                .map_err(|e| format!("{e} (hardware: {})", why.join("; ")));
            }
        }
        Err(why.join("; "))
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn configure(
        mft: IMFTransform,
        activate: Option<IMFActivate>,
        name: String,
        hardware: bool,
        w: u32,
        h: u32,
        fps: u32,
        bitrate: u32,
        d3d: Option<&IMFDXGIDeviceManager>,
    ) -> Result<Self, String> {
        let mut is_async = false;
        if let Ok(a) = mft.GetAttributes() {
            is_async = a.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0;
            if is_async {
                a.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                    .map_err(|e| err("async unlock", e))?;
            }
            let _ = a.SetUINT32(&MF_LOW_LATENCY, 1);
        }
        // Section 5: D3D11 input — the MFT takes our device before its types.
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
            // Wave 1: VBV of about one frame of bits + a peak cap, so a
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
        let out = video_type(&MFVideoFormat_H264, w, h, fps).map_err(|e| err("output type", e))?;
        out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)
            .map_err(|e| err("bitrate", e))?;
        out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)
            .map_err(|e| err("profile", e))?;
        mft.SetOutputType(0, &out, 0)
            .map_err(|e| err("SetOutputType", e))?;
        let inp = video_type(&MFVideoFormat_NV12, w, h, fps).map_err(|e| err("input type", e))?;
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

    pub fn is_hardware(&self) -> bool {
        self.hardware
    }

    pub fn size(&self) -> (u32, u32) {
        (self.w, self.h)
    }

    /// Encode one NV12 picture already in `nv12` (see [`encode`](VideoEncode::encode)).
    pub fn encode_nv12(
        &mut self,
        nv12: &[u8],
        force_idr: bool,
        pts_us: u64,
    ) -> Result<Vec<EncodedAu>, String> {
        let want = crate::video::nv12_len(self.w, self.h);
        if nv12.len() < want {
            return Err(format!("NV12 input is {} bytes, want {want}", nv12.len()));
        }
        unsafe {
            let sample = memory_sample(&nv12[..want], want).map_err(|e| err("input sample", e))?;
            self.submit(sample, force_idr, pts_us)
        }
    }

    /// Timestamp, optionally force an IDR, and run one input sample
    /// through the MFT (sync or async).
    unsafe fn submit(
        &mut self,
        sample: IMFSample,
        force_idr: bool,
        pts_us: u64,
    ) -> Result<Vec<EncodedAu>, String> {
        {
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

    unsafe fn encode_sync(
        &mut self,
        sample: &IMFSample,
        out: &mut Vec<EncodedAu>,
    ) -> Result<(), String> {
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

    /// Pull every output available now (sync MFT).
    unsafe fn drain(&mut self, out: &mut Vec<EncodedAu>) -> Result<(), String> {
        for _ in 0..16 {
            match self.take_output()? {
                Some(au) => out.push(au),
                None => return Ok(()),
            }
        }
        Ok(())
    }

    /// One ProcessOutput, handling a stream change. `None` = needs input.
    unsafe fn take_output(&mut self) -> Result<Option<EncodedAu>, String> {
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
        Err("encoder keeps changing its output type".into())
    }

    /// Handle one pending async event. `Ok(false)` = none available.
    unsafe fn poll_event(
        &mut self,
        ev: &IMFMediaEventGenerator,
        out: &mut Vec<EncodedAu>,
    ) -> Result<bool, String> {
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

    unsafe fn encode_async(
        &mut self,
        ev: &IMFMediaEventGenerator,
        sample: &IMFSample,
        out: &mut Vec<EncodedAu>,
    ) -> Result<(), String> {
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
                    return Err("hardware encoder never asked for input".into());
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
        r
    }

    fn set_bitrate(&mut self, bps: u32) {
        if let Some(api) = &self.api {
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
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Some(a) = &self.activate {
                let _ = a.ShutdownObject();
            }
        }
    }
}

/// Microsoft's H.264 decoder MFT, NV12 out (see the module docs).
pub struct H264Decoder {
    mft: IMFTransform,
    out_info: MFT_OUTPUT_STREAM_INFO,
    /// Output picture geometry (coded size, e.g. 1920x1088) and pitch.
    out_w: u32,
    out_h: u32,
    stride: u32,
    nv12: Vec<u8>,
    time: i64,
}

impl H264Decoder {
    pub fn new() -> Result<Self, String> {
        startup()?;
        unsafe {
            let mft: IMFTransform =
                CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_INPROC_SERVER)
                    .map_err(|e| err("H.264 decoder", e))?;
            if let Ok(a) = mft.GetAttributes() {
                let _ = a.SetUINT32(&MF_LOW_LATENCY, 1);
            }
            if let Ok(api) = mft.cast::<ICodecAPI>() {
                let _ = api.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true));
            }
            let t = MFCreateMediaType().map_err(|e| err("media type", e))?;
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| err("type", e))?;
            t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
                .map_err(|e| err("type", e))?;
            mft.SetInputType(0, &t, 0)
                .map_err(|e| err("SetInputType(H264)", e))?;
            let mut d = Self {
                mft,
                out_info: MFT_OUTPUT_STREAM_INFO::default(),
                out_w: 0,
                out_h: 0,
                stride: 0,
                nv12: Vec::new(),
                time: 0,
            };
            d.pick_output()?;
            d.mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|e| err("begin streaming", e))?;
            d.mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|e| err("start of stream", e))?;
            Ok(d)
        }
    }

    unsafe fn pick_output(&mut self) -> Result<(), String> {
        for i in 0..64 {
            let Ok(t) = self.mft.GetOutputAvailableType(0, i) else {
                break;
            };
            if t.GetGUID(&MF_MT_SUBTYPE).ok() != Some(MFVideoFormat_NV12) {
                continue;
            }
            self.mft
                .SetOutputType(0, &t, 0)
                .map_err(|e| err("SetOutputType(NV12)", e))?;
            let fs = t.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
            self.out_w = (fs >> 32) as u32;
            self.out_h = fs as u32;
            let stride = t
                .GetUINT32(&MF_MT_DEFAULT_STRIDE)
                .map(|s| (s as i32).unsigned_abs())
                .unwrap_or(0);
            self.stride = if stride >= self.out_w {
                stride
            } else {
                self.out_w
            };
            self.out_info = self
                .mft
                .GetOutputStreamInfo(0)
                .map_err(|e| err("output info", e))?;
            return Ok(());
        }
        Err("the H.264 decoder offers no NV12 output".into())
    }

    /// Decode one Annex-B access unit. `Ok(true)` when a picture came out
    /// (see [`picture`](Self::picture)).
    pub fn decode_nv12(&mut self, annexb: &[u8]) -> Result<bool, String> {
        unsafe {
            let s = memory_sample(annexb, annexb.len()).map_err(|e| err("input sample", e))?;
            self.time += 333_333;
            let _ = s.SetSampleTime(self.time);
            if let Err(e) = self.mft.ProcessInput(0, &s, 0) {
                if e.code() != MF_E_NOTACCEPTING {
                    return Err(err("ProcessInput", e));
                }
                self.pull_all()?;
                self.mft
                    .ProcessInput(0, &s, 0)
                    .map_err(|e| err("ProcessInput", e))?;
            }
            self.pull_all()
        }
    }

    /// Pull every picture available now; keeps the newest. Whether any came.
    unsafe fn pull_all(&mut self) -> Result<bool, String> {
        let mut got = false;
        for _ in 0..16 {
            let cap = (self.out_w.max(16) * self.out_h.max(16) * 3 / 2) as usize;
            match pull(&self.mft, &self.out_info, cap)? {
                Pulled::NeedInput => return Ok(got),
                Pulled::StreamChange => self.pick_output()?,
                Pulled::Sample(s) => {
                    sample_bytes(&s, &mut self.nv12).map_err(|e| err("picture bytes", e))?;
                    // A tight contiguous copy has pitch = width.
                    if self.nv12.len() == (self.out_w * self.out_h * 3 / 2) as usize {
                        self.stride = self.out_w;
                    }
                    got = true;
                }
            }
        }
        Ok(got)
    }

    /// The last decoded picture: `(nv12, pitch, coded_w, coded_h)`.
    pub fn picture(&self) -> (&[u8], u32, u32, u32) {
        (&self.nv12, self.stride, self.out_w, self.out_h)
    }
}

impl VideoDecode for H264Decoder {
    fn decode(&mut self, annexb: &[u8], w: u32, h: u32, out: &mut Vec<u8>) -> Result<bool, String> {
        if !self.decode_nv12(annexb)? {
            return Ok(false);
        }
        if w > self.out_w || h > self.out_h {
            return Err(format!(
                "decoded picture {}x{} smaller than the declared {w}x{h}",
                self.out_w, self.out_h
            ));
        }
        let uv = self.stride as usize * self.out_h as usize;
        nv12_to_bgra(&self.nv12, self.stride as usize, uv, w, h, out)?;
        Ok(true)
    }
}

impl Drop for H264Decoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
    }
}

// ---------------------------------------------------------------------------
// Section 5: GPU input (D3D11 textures -> video processor NV12 -> MFT)
// ---------------------------------------------------------------------------

/// A captured picture on the GPU: the D3D11 device that owns it and a BGRA
/// texture (the host's desktop-duplication mirror). Handed to
/// [`VideoEncode::encode_gpu`] as `&dyn Any`.
#[derive(Clone)]
pub struct GpuFrame {
    pub device: ID3D11Device,
    pub texture: ID3D11Texture2D,
}

/// Create a hardware D3D11 device suitable for capture + GPU encode
/// (BGRA + video support, multithread-protected).
pub fn gpu_device() -> Result<ID3D11Device, String> {
    unsafe {
        let mut device: Option<ID3D11Device> = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
        .map_err(|e| err("D3D11CreateDevice", e))?;
        let device = device.ok_or("no D3D11 device")?;
        protect(&device);
        Ok(device)
    }
}

/// The MFT and the capture thread share the device: serialise its
/// immediate context.
fn protect(device: &ID3D11Device) {
    if let Ok(mt) = device.cast::<ID3D11Multithread>() {
        unsafe {
            let _ = mt.SetMultithreadProtected(true);
        }
    }
}

/// NV12 surfaces in flight (the async MFT may still read the previous ones).
const GPU_RING: usize = 4;

struct GpuState {
    _mgr: IMFDXGIDeviceManager,
    device: ID3D11Device,
    vdev: ID3D11VideoDevice,
    vctx: ID3D11VideoContext,
    /// Video processor for (input w, h) -> encoder size.
    vp: Option<(
        (u32, u32),
        ID3D11VideoProcessorEnumerator,
        ID3D11VideoProcessor,
    )>,
    /// The last input view and the texture it was made for.
    input: Option<(ID3D11Texture2D, ID3D11VideoProcessorInputView)>,
    ring: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
}

impl GpuState {
    unsafe fn new(device: &ID3D11Device) -> Result<(Self, IMFDXGIDeviceManager), String> {
        protect(device);
        let mut token = 0u32;
        let mut mgr: Option<IMFDXGIDeviceManager> = None;
        MFCreateDXGIDeviceManager(&mut token, &mut mgr)
            .map_err(|e| err("MFCreateDXGIDeviceManager", e))?;
        let mgr = mgr.ok_or("no DXGI device manager")?;
        mgr.ResetDevice(device, token)
            .map_err(|e| err("ResetDevice", e))?;
        let vdev: ID3D11VideoDevice = device.cast().map_err(|e| err("ID3D11VideoDevice", e))?;
        let ctx = device
            .GetImmediateContext()
            .map_err(|e| err("immediate context", e))?;
        let vctx: ID3D11VideoContext = ctx.cast().map_err(|e| err("ID3D11VideoContext", e))?;
        Ok((
            Self {
                _mgr: mgr.clone(),
                device: device.clone(),
                vdev,
                vctx,
                vp: None,
                input: None,
                ring: Vec::new(),
                next: 0,
            },
            mgr,
        ))
    }

    /// BGRA `src` (crop `rect`) -> the next NV12 ring surface (BT.709
    /// limited range, like `bgra_to_nv12`).
    unsafe fn convert(
        &mut self,
        src: &ID3D11Texture2D,
        rect: (u32, u32, u32, u32),
        w: u32,
        h: u32,
    ) -> Result<ID3D11Texture2D, String> {
        let mut sd = D3D11_TEXTURE2D_DESC::default();
        src.GetDesc(&mut sd);
        let in_size = (sd.Width, sd.Height);
        if self.vp.as_ref().is_none_or(|(s, _, _)| *s != in_size) {
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: DXGI_RATIONAL {
                    Numerator: 60,
                    Denominator: 1,
                },
                InputWidth: sd.Width,
                InputHeight: sd.Height,
                OutputFrameRate: DXGI_RATIONAL {
                    Numerator: 60,
                    Denominator: 1,
                },
                OutputWidth: w,
                OutputHeight: h,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
            };
            let en = self
                .vdev
                .CreateVideoProcessorEnumerator(&desc)
                .map_err(|e| err("CreateVideoProcessorEnumerator", e))?;
            let vp = self
                .vdev
                .CreateVideoProcessor(&en, 0)
                .map_err(|e| err("CreateVideoProcessor", e))?;
            // Colour: full-range sRGB in, BT.709 studio-range YCbCr out.
            if let Ok(v1) = self.vctx.cast::<ID3D11VideoContext1>() {
                v1.VideoProcessorSetStreamColorSpace1(
                    &vp,
                    0,
                    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
                );
                v1.VideoProcessorSetOutputColorSpace1(
                    &vp,
                    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
                );
            } else {
                // Usage 0, RGB_Range 0 (full) / YCbCr_Matrix 1 (BT.709),
                // Nominal_Range 1 (16-235).
                let input = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
                let output = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                    _bitfield: (1 << 2) | (1 << 4),
                };
                self.vctx.VideoProcessorSetStreamColorSpace(&vp, 0, &input);
                self.vctx.VideoProcessorSetOutputColorSpace(&vp, &output);
            }
            self.vctx.VideoProcessorSetStreamFrameFormat(
                &vp,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            self.vctx
                .VideoProcessorSetStreamAutoProcessingMode(&vp, 0, false);
            self.vp = Some((in_size, en, vp));
            self.input = None;
            self.ring.clear();
        }
        let Some((_, en, vp)) = self.vp.as_ref() else {
            unreachable!()
        };
        if self.ring.is_empty() {
            for _ in 0..GPU_RING {
                let td = D3D11_TEXTURE2D_DESC {
                    Width: w,
                    Height: h,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_NV12,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DEFAULT,
                    // VIDEO_ENCODER: the MFT can read the surface in place.
                    BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_VIDEO_ENCODER.0) as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };
                let mut tex: Option<ID3D11Texture2D> = None;
                self.device
                    .CreateTexture2D(&td, None, Some(&mut tex))
                    .map_err(|e| err("CreateTexture2D(NV12)", e))?;
                let tex = tex.ok_or("no NV12 texture")?;
                let od = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                    ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                    },
                };
                let mut ov: Option<ID3D11VideoProcessorOutputView> = None;
                self.vdev
                    .CreateVideoProcessorOutputView(&tex, en, &od, Some(&mut ov))
                    .map_err(|e| err("CreateVideoProcessorOutputView", e))?;
                self.ring.push((tex, ov.ok_or("no output view")?));
            }
        }
        if self.input.as_ref().is_none_or(|(t, _)| t != src) {
            let id = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: 0,
                    },
                },
            };
            let mut iv: Option<ID3D11VideoProcessorInputView> = None;
            self.vdev
                .CreateVideoProcessorInputView(src, en, &id, Some(&mut iv))
                .map_err(|e| err("CreateVideoProcessorInputView", e))?;
            self.input = Some((src.clone(), iv.ok_or("no input view")?));
        }
        let Some((_, iv)) = self.input.as_ref() else {
            unreachable!()
        };
        let (x, y, rw, rh) = rect;
        let srect = RECT {
            left: x as i32,
            top: y as i32,
            right: (x + rw) as i32,
            bottom: (y + rh) as i32,
        };
        let drect = RECT {
            left: 0,
            top: 0,
            right: w as i32,
            bottom: h as i32,
        };
        self.vctx
            .VideoProcessorSetStreamSourceRect(vp, 0, true, Some(&srect));
        self.vctx
            .VideoProcessorSetStreamDestRect(vp, 0, true, Some(&drect));
        self.vctx
            .VideoProcessorSetOutputTargetRect(vp, true, Some(&drect));
        let (tex, ov) = &self.ring[self.next % self.ring.len()];
        self.next += 1;
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: std::ptr::null_mut(),
            pInputSurface: ManuallyDrop::new(Some(iv.clone())),
            ppFutureSurfaces: std::ptr::null_mut(),
            ppPastSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: ManuallyDrop::new(None),
            ppFutureSurfacesRight: std::ptr::null_mut(),
        };
        let mut streams = [stream];
        let r = self.vctx.VideoProcessorBlt(vp, ov, 0, &streams);
        ManuallyDrop::drop(&mut streams[0].pInputSurface);
        r.map_err(|e| err("VideoProcessorBlt", e))?;
        Ok(tex.clone())
    }
}

impl H264Encoder {
    /// A hardware encoder fed with D3D11 NV12 surfaces on `device` (the
    /// capture device): BGRA -> NV12 on the GPU, no system-memory copy.
    pub fn new_gpu(
        device: &ID3D11Device,
        w: u32,
        h: u32,
        fps: u32,
        bitrate: u32,
    ) -> Result<Self, String> {
        if w < 16 || h < 16 || !w.is_multiple_of(2) || !h.is_multiple_of(2) {
            return Err(format!(
                "H.264 picture {w}x{h} must be even and at least 16x16"
            ));
        }
        startup()?;
        let mut why = Vec::new();
        unsafe {
            for a in hardware_encoders() {
                let name = friendly_name(&a);
                let (gpu, mgr) = GpuState::new(device)?;
                match a.ActivateObject::<IMFTransform>() {
                    Ok(mft) => {
                        match Self::configure(
                            mft,
                            Some(a.clone()),
                            format!("{name} (hw, GPU input)"),
                            true,
                            w,
                            h,
                            fps,
                            bitrate,
                            Some(&mgr),
                        ) {
                            Ok(mut e) => {
                                e.gpu = Some(gpu);
                                return Ok(e);
                            }
                            Err(e) => {
                                let _ = a.ShutdownObject();
                                why.push(format!("{name}: {e}"));
                            }
                        }
                    }
                    Err(e) => why.push(format!("{name}: activate: {e}")),
                }
            }
        }
        if why.is_empty() {
            why.push("no hardware H.264 encoder registered".into());
        }
        Err(why.join("; "))
    }

    /// Whether this encoder takes D3D11 surfaces ([`new_gpu`](Self::new_gpu)).
    pub fn is_gpu(&self) -> bool {
        self.gpu.is_some()
    }

    /// Encode the `rect` crop `(x, y, w, h)` of a BGRA texture on the
    /// encoder's device (scaled to the encoder size when they differ).
    pub fn encode_texture(
        &mut self,
        src: &ID3D11Texture2D,
        rect: (u32, u32, u32, u32),
        force_idr: bool,
        pts_us: u64,
    ) -> Result<Vec<EncodedAu>, String> {
        let (w, h) = (self.w, self.h);
        unsafe {
            let gpu = self.gpu.as_mut().ok_or("not a GPU-input encoder")?;
            let nv12 = gpu.convert(src, rect, w, h)?;
            let buf = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &nv12, 0, false)
                .map_err(|e| err("MFCreateDXGISurfaceBuffer", e))?;
            if let Ok(len) = buf.GetMaxLength() {
                let _ = buf.SetCurrentLength(len);
            }
            let sample = MFCreateSample().map_err(|e| err("sample", e))?;
            sample.AddBuffer(&buf).map_err(|e| err("AddBuffer", e))?;
            self.submit(sample, force_idr, pts_us)
        }
    }
}

/// The host's encoder: CPU (system-memory NV12) until the first GPU frame
/// arrives, then — on a hardware MFT that accepts the capture device — GPU
/// input; any GPU failure falls back to the CPU path for good.
pub struct MfVideoEncoder {
    enc: H264Encoder,
    w: u32,
    h: u32,
    fps: u32,
    bitrate: u32,
    pref: EncoderPref,
    gpu_failed: bool,
    pub gpu_error: Option<String>,
}

impl MfVideoEncoder {
    pub fn new(w: u32, h: u32, fps: u32, bitrate: u32, pref: EncoderPref) -> Result<Self, String> {
        Ok(Self {
            enc: H264Encoder::new(w, h, fps, bitrate, pref)?,
            w,
            h,
            fps,
            bitrate,
            pref,
            gpu_failed: pref == EncoderPref::Software,
            gpu_error: None,
        })
    }
}

impl VideoEncode for MfVideoEncoder {
    fn encode(
        &mut self,
        bgra: &[u8],
        stride: usize,
        force_idr: bool,
        pts_us: u64,
    ) -> Result<Vec<EncodedAu>, String> {
        if self.enc.is_gpu() {
            // A GPU encoder fed from memory (no GPU frame this time): the
            // texture path is unavailable for this capture; back to CPU.
            self.enc = H264Encoder::new(self.w, self.h, self.fps, self.bitrate, self.pref)?;
            self.gpu_failed = true;
        }
        self.enc.encode(bgra, stride, force_idr, pts_us)
    }

    fn encode_gpu(
        &mut self,
        frame: &dyn std::any::Any,
        rect: (u32, u32, u32, u32),
        force_idr: bool,
        pts_us: u64,
    ) -> Option<Result<Vec<EncodedAu>, String>> {
        if self.gpu_failed {
            return None;
        }
        let f = frame.downcast_ref::<GpuFrame>()?;
        if !self.enc.is_gpu() {
            match H264Encoder::new_gpu(&f.device, self.w, self.h, self.fps, self.bitrate) {
                Ok(e) => self.enc = e,
                Err(e) => {
                    self.gpu_failed = true;
                    self.gpu_error = Some(e);
                    return None;
                }
            }
        }
        match self.enc.encode_texture(&f.texture, rect, force_idr, pts_us) {
            Ok(v) => Some(Ok(v)),
            Err(e) => {
                // Fall back to the CPU encoder (a fresh IDR follows).
                self.gpu_failed = true;
                self.gpu_error = Some(e);
                match H264Encoder::new(self.w, self.h, self.fps, self.bitrate, self.pref) {
                    Ok(c) => {
                        self.enc = c;
                        None
                    }
                    Err(e) => Some(Err(e)),
                }
            }
        }
    }

    fn set_bitrate(&mut self, bps: u32) {
        self.bitrate = bps;
        self.enc.set_bitrate(bps);
    }

    fn name(&self) -> String {
        self.enc.name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tiles::TileRect;
    use crate::video::{psnr_bgra, synth_scroll, synth_video, AccessUnit};

    fn picture(w: u32, h: u32, t: u32) -> Vec<u8> {
        let mut px = vec![0u8; (w * h * 4) as usize];
        synth_video(
            &mut px,
            w,
            TileRect {
                x: 0,
                y: 0,
                w: w as u16,
                h: h as u16,
            },
            t,
        );
        px
    }

    /// Encode N synthetic frames: non-empty Annex-B, the first an IDR with
    /// SPS + PPS, a forced IDR honoured. Skips when no encoder exists.
    fn smoke(pref: EncoderPref) {
        let (w, h) = (640, 360);
        let mut enc = match H264Encoder::new(w, h, 30, 4_000_000, pref) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("skipping {pref:?} encoder smoke test: {e}");
                return;
            }
        };
        eprintln!("encoder: {}", enc.name());
        let mut aus = Vec::new();
        for t in 0..30 {
            let px = picture(w, h, t);
            aus.extend(
                enc.encode(
                    &px,
                    w as usize * 4,
                    t == 20,
                    1_000_000 + u64::from(t) * 33_333,
                )
                .unwrap(),
            );
        }
        // Async hardware may still hold the last picture; one more flushes it.
        assert!(aus.len() >= 28, "only {} units for 30 pictures", aus.len());
        assert!(aus.iter().all(|a| !a.data.is_empty()));
        let first = AccessUnit::from_annexb(&aus[0].data);
        assert!(aus[0].keyframe && first.idr, "first unit is an IDR");
        assert!(
            first.sps.is_some() && first.pps.is_some(),
            "IDR carries SPS/PPS"
        );
        // Section 4: the SPS on the wire signals BT.709 limited range.
        let sps_nal = first.sps.as_deref().unwrap();
        let summary = crate::video::sps::summary(sps_nal).expect("SPS parses");
        eprintln!("SPS: {summary:?}");
        assert!(summary.colour.is_bt709_limited(), "{:?}", summary.colour);
        assert!(
            aus[1..15].iter().all(|a| !a.keyframe),
            "no spontaneous IDRs"
        );
        let forced: Vec<_> = aus[15..].iter().filter(|a| a.keyframe).collect();
        assert!(!forced.is_empty(), "forced IDR honoured");
        for k in forced {
            let au = AccessUnit::from_annexb(&k.data);
            assert!(
                au.sps.is_some() && au.pps.is_some(),
                "forced IDR self-contained"
            );
        }
    }

    #[test]
    fn hardware_encoder_smoke() {
        smoke(EncoderPref::Hardware);
    }

    #[test]
    fn software_encoder_smoke() {
        smoke(EncoderPref::Software);
    }

    /// Section 5: GPU input — a BGRA texture through the video processor
    /// into the hardware MFT decodes back close to the source (colours
    /// included: BT.709 limited on both ends). Skips without hardware.
    #[test]
    fn gpu_input_round_trip_psnr() {
        let (w, h) = (640u32, 384u32);
        let Ok(device) = gpu_device() else {
            eprintln!("skipping: no D3D11 device");
            return;
        };
        let mut enc = match H264Encoder::new_gpu(&device, w, h, 30, 8_000_000) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("skipping: no GPU-input encoder ({e})");
                return;
            }
        };
        let mut dec = H264Decoder::new().unwrap();
        let tex = test_texture(&device, w, h);
        let ctx = unsafe { device.GetImmediateContext().unwrap() };
        let mut out = Vec::new();
        let mut psnrs = Vec::new();
        let mut sent = std::collections::HashMap::new();
        for t in 0..40u32 {
            let px = picture(w, h, t);
            unsafe { ctx.UpdateSubresource(&tex, 0, None, px.as_ptr().cast(), w * 4, 0) };
            let pts = 5_000_000 + u64::from(t) * 33_333;
            sent.insert(pts, px);
            for au in enc.encode_texture(&tex, (0, 0, w, h), false, pts).unwrap() {
                if dec.decode(&au.data, w, h, &mut out).unwrap() {
                    if let Some(src) = sent.get(&au.pts_us) {
                        psnrs.push(psnr_bgra(src, &out));
                    }
                }
            }
        }
        let tail = &psnrs[psnrs.len().saturating_sub(20)..];
        let avg = tail.iter().sum::<f64>() / tail.len().max(1) as f64;
        eprintln!(
            "GPU-input round trip via {}: {} pictures, PSNR avg {avg:.1} dB",
            enc.name(),
            psnrs.len()
        );
        assert!(
            psnrs.len() >= 35 && avg > 33.0,
            "PSNR {avg:.1} dB over {}",
            psnrs.len()
        );
    }

    /// Wait until the GPU finished everything queued so far.
    fn gpu_sync(device: &ID3D11Device, ctx: &ID3D11DeviceContext) {
        unsafe {
            let qd = D3D11_QUERY_DESC {
                Query: D3D11_QUERY_EVENT,
                MiscFlags: 0,
            };
            let mut q: Option<ID3D11Query> = None;
            device.CreateQuery(&qd, Some(&mut q)).unwrap();
            let q = q.unwrap();
            ctx.End(&q);
            ctx.Flush();
            let mut done: i32 = 0;
            while ctx
                .GetData(&q, Some((&mut done as *mut i32).cast()), 4, 0)
                .is_err()
                || done == 0
            {
                std::thread::yield_now();
            }
        }
    }

    fn test_texture(device: &ID3D11Device, w: u32, h: u32) -> ID3D11Texture2D {
        let td = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex = None;
        unsafe { device.CreateTexture2D(&td, None, Some(&mut tex)).unwrap() };
        tex.unwrap()
    }

    /// Section 5 measurement (`--release --features mf -- --ignored
    /// --nocapture`): per-frame encode latency of the hardware MFT with
    /// system-memory NV12 (CPU BGRA->NV12 + upload) vs D3D11 input (GPU
    /// video-processor conversion), 1080p and a 640x360 region.
    #[test]
    #[ignore]
    fn bench_gpu_vs_cpu_input() {
        let Ok(device) = gpu_device() else {
            println!("no D3D11 device");
            return;
        };
        let ctx = unsafe { device.GetImmediateContext().unwrap() };
        for (w, h) in [(1920u32, 1080u32), (640, 360), (1920, 1080), (640, 360)] {
            let frames = 120u32;
            let bitrate = crate::video::target_bitrate(w, h, 30, None);
            let pics: Vec<Vec<u8>> = (0..frames).map(|t| picture(w, h, t)).collect();
            let tex = test_texture(&device, w, h);
            let ms = |d: Duration| d.as_secs_f64() * 1000.0 / f64::from(frames);
            if let Ok(mut cpu) = H264Encoder::new(w, h, 30, bitrate, EncoderPref::Hardware) {
                let mut total = Duration::ZERO;
                let mut n = 0;
                for (t, px) in pics.iter().enumerate() {
                    let t0 = Instant::now();
                    n += cpu
                        .encode(px, w as usize * 4, false, 1_000_000 + t as u64 * 33_333)
                        .unwrap()
                        .len();
                    total += t0.elapsed();
                }
                println!(
                    "{w}x{h} {}: CPU BGRA->NV12 + system-memory input: {:.2} ms/frame ({n} units)",
                    cpu.name(),
                    ms(total)
                );
            }
            match H264Encoder::new_gpu(&device, w, h, 30, bitrate) {
                Ok(mut gpu) => {
                    let mut total = Duration::ZERO;
                    let mut n = 0;
                    for (t, px) in pics.iter().enumerate() {
                        // The upload stands in for the duplication copy that
                        // already happened on the GPU in the host.
                        unsafe {
                            ctx.UpdateSubresource(&tex, 0, None, px.as_ptr().cast(), w * 4, 0)
                        };
                        gpu_sync(&device, &ctx);
                        let t0 = Instant::now();
                        n += gpu
                            .encode_texture(
                                &tex,
                                (0, 0, w, h),
                                false,
                                1_000_000 + t as u64 * 33_333,
                            )
                            .unwrap()
                            .len();
                        total += t0.elapsed();
                    }
                    println!(
                        "{w}x{h} {}: D3D11 texture input: {:.2} ms/frame ({n} units)",
                        gpu.name(),
                        ms(total)
                    );
                }
                Err(e) => println!("{w}x{h}: GPU input unavailable: {e}"),
            }
        }
    }

    /// encode -> decode round trip on the desktop: PSNR well above 35 dB at
    /// a generous bitrate.
    #[test]
    fn encode_decode_round_trip_psnr() {
        let (w, h) = (640, 384);
        let mut enc = match H264Encoder::new(w, h, 30, 8_000_000, EncoderPref::Auto) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("skipping round trip: no encoder ({e})");
                return;
            }
        };
        let mut dec = match H264Decoder::new() {
            Ok(d) => d,
            Err(e) => {
                eprintln!("skipping round trip: no decoder ({e})");
                return;
            }
        };
        let mut out = Vec::new();
        let mut psnrs = Vec::new();
        let mut decoded = 0;
        let mut sent = std::collections::HashMap::new();
        for t in 0..40u32 {
            let px = picture(w, h, t);
            let pts = 5_000_000 + u64::from(t) * 33_333;
            sent.insert(pts, px.clone());
            for au in enc.encode(&px, w as usize * 4, false, pts).unwrap() {
                if dec.decode(&au.data, w, h, &mut out).unwrap() {
                    decoded += 1;
                    if let Some(src) = sent.get(&au.pts_us) {
                        psnrs.push(psnr_bgra(src, &out));
                    }
                }
            }
        }
        assert!(decoded >= 35, "decoded {decoded} of 40");
        let tail = &psnrs[psnrs.len().saturating_sub(20)..];
        let avg = tail.iter().sum::<f64>() / tail.len().max(1) as f64;
        eprintln!(
            "round trip via {}: {decoded} pictures, PSNR avg {avg:.1} dB",
            enc.name()
        );
        assert!(avg > 35.0, "PSNR {avg:.1} dB");
    }

    /// Measurements (run with `--release --features mf,zstd-encode --
    /// --ignored --nocapture`): encode latency hw vs sw, decode latency,
    /// bitrate at 30 fps for a 640x360 video region (inside a 1080p
    /// desktop) and a full-screen scrolling page, against lossless tiles on
    /// the very same content.
    #[test]
    #[ignore]
    fn bench_h264() {
        use crate::sendq::{FrameMailbox, UpdateEncoder};
        use crate::video::synth_desktop;
        let (sw, sh) = (1920u32, 1080u32);
        let frames = 150u32;
        let secs = f64::from(frames) / 30.0;
        let content = |px: &mut Vec<u8>, scroll: bool, r: TileRect, t: u32| {
            if scroll {
                synth_scroll(px, sw, r, t, 8);
            } else {
                synth_video(px, sw, r, t);
            }
        };
        for (label, scroll, r) in [
            (
                "video region 640x360",
                false,
                TileRect {
                    x: 640,
                    y: 320,
                    w: 640,
                    h: 360,
                },
            ),
            (
                "scrolling page 1920x1080",
                true,
                TileRect {
                    x: 0,
                    y: 0,
                    w: 1920,
                    h: 1080,
                },
            ),
            (
                "full-screen video 1920x1080",
                false,
                TileRect {
                    x: 0,
                    y: 0,
                    w: 1920,
                    h: 1080,
                },
            ),
        ] {
            // Lossless tiles (zstd when built with zstd-encode) baseline.
            let mut px = vec![0u8; (sw * sh * 4) as usize];
            synth_desktop(&mut px, sw, sh);
            let mut enc = UpdateEncoder::new();
            enc.set_peer_caps(crate::terminal::LOCAL_CAPS);
            let mb = FrameMailbox::default();
            enc.submit(&px, sw, sh, None, &mb);
            mb.take();
            let mut lossless = 0usize;
            let t0 = Instant::now();
            for t in 0..frames {
                content(&mut px, scroll, r, t);
                enc.submit(&px, sw, sh, None, &mb);
                if let Some(f) = mb.take() {
                    lossless += f.data.len();
                }
            }
            println!(
                "{label}: lossless tiles {:.0} KB/s ({:.2} ms/frame encode)",
                lossless as f64 / 1024.0 / secs,
                t0.elapsed().as_secs_f64() * 1000.0 / f64::from(frames)
            );
            for pref in [EncoderPref::Hardware, EncoderPref::Software] {
                let (w, h) = (u32::from(r.w), u32::from(r.h));
                let bitrate = crate::video::target_bitrate(w, h, 30, None);
                let mut enc = match H264Encoder::new(w, h, 30, bitrate, pref) {
                    Ok(e) => e,
                    Err(e) => {
                        println!("  {pref:?}: unavailable: {e}");
                        continue;
                    }
                };
                let mut dec = H264Decoder::new().unwrap();
                let (mut bytes, mut n, mut psnr, mut pn) = (0usize, 0u32, 0f64, 0u32);
                let (mut conv, mut enc_t, mut dec_t, mut back) = (
                    Duration::ZERO,
                    Duration::ZERO,
                    Duration::ZERO,
                    Duration::ZERO,
                );
                let mut nv = Vec::new();
                let mut out = Vec::new();
                let off = usize::from(r.y) * sw as usize * 4 + usize::from(r.x) * 4;
                for t in 0..frames {
                    content(&mut px, scroll, r, t);
                    let c0 = Instant::now();
                    bgra_to_nv12(&px[off..], sw as usize * 4, w, h, &mut nv);
                    conv += c0.elapsed();
                    let e0 = Instant::now();
                    let aus = enc
                        .encode_nv12(&nv, false, 1_000_000 + u64::from(t) * 33_333)
                        .unwrap();
                    enc_t += e0.elapsed();
                    for au in aus {
                        bytes += au.data.len();
                        let d0 = Instant::now();
                        if dec.decode_nv12(&au.data).unwrap() {
                            dec_t += d0.elapsed();
                            let b0 = Instant::now();
                            let (nv12, pitch, _, ch) = dec.picture();
                            nv12_to_bgra(
                                nv12,
                                pitch as usize,
                                pitch as usize * ch as usize,
                                w,
                                h,
                                &mut out,
                            )
                            .unwrap();
                            back += b0.elapsed();
                            n += 1;
                            if t + 10 >= frames && au.pts_us == 1_000_000 + u64::from(t) * 33_333 {
                                let mut src = Vec::with_capacity(out.len());
                                for y in 0..h as usize {
                                    let o = off + y * sw as usize * 4;
                                    src.extend_from_slice(&px[o..o + w as usize * 4]);
                                }
                                psnr += psnr_bgra(&src, &out);
                                pn += 1;
                            }
                        }
                    }
                }
                let ms = |d: Duration, k: u32| d.as_secs_f64() * 1000.0 / f64::from(k.max(1));
                println!(
                    "  {}: encode {:.2} ms/frame + BGRA->NV12 {:.2} ms; decode {:.2} ms + NV12->BGRA {:.2} ms; {:.0} KB/s ({:.2} Mbps, target {:.1}) = {:.1}x less than lossless; PSNR {:.1} dB",
                    enc.name(),
                    ms(enc_t, frames),
                    ms(conv, frames),
                    ms(dec_t, n),
                    ms(back, n),
                    bytes as f64 / 1024.0 / secs,
                    bytes as f64 * 8.0 / 1e6 / secs,
                    f64::from(bitrate) / 1e6,
                    lossless as f64 / bytes.max(1) as f64,
                    psnr / f64::from(pn.max(1))
                );
            }
        }
    }
}
