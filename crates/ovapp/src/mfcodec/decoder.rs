//! The video decoder MFT: Microsoft's H.264 decoder, or an HEVC decoder MFT
//! when one is installed (the "HEVC Video Extensions"), in low-latency mode
//! — on the CPU (system-memory NV12), or D3D11-accelerated (DXVA) with the
//! pictures kept on the GPU (see [`super::dxva`]).

use std::sync::OnceLock;

use windows::core::{Interface, GUID};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Multithread, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Variant::VARIANT;

use super::dxva::{GpuPicture, GpuPictureSlot, GpuTarget, Nv12Converter};
use super::mft::{memory_sample, pull, sample_bytes, Pulled};
use super::{err, startup};
use crate::error::AppError;
use crate::video::{nv12_to_bgra, DecodedTo, VideoCodec, VideoDecode, VideoHeader};

/// The H.264-only name of [`MfVideoDecoder`] (`H264Decoder::new()` is the
/// software H.264 decoder).
pub type H264Decoder = MfVideoDecoder;

/// D3D11 acceleration: the device manager handed to the MFT, the NV12 ->
/// BGRA converter and the presenter's slot.
struct Dxva {
    _mgr: IMFDXGIDeviceManager,
    conv: Nv12Converter,
    slot: GpuPictureSlot,
}

/// A Media Foundation video decoder (NV12 out; see the module docs).
pub struct MfVideoDecoder {
    codec: VideoCodec,
    mft: IMFTransform,
    out_info: MFT_OUTPUT_STREAM_INFO,
    /// Output picture geometry (coded size, e.g. 1920x1088) and pitch.
    out_w: u32,
    out_h: u32,
    stride: u32,
    /// The newest picture of the last decode call, not yet consumed.
    sample: Option<IMFSample>,
    /// System-memory NV12 of the last CPU picture.
    nv12: Vec<u8>,
    time: i64,
    /// D3D11 acceleration (`None`: software, system memory).
    gpu: Option<Dxva>,
    /// The presenter given to [`open`](Self::open) (kept to re-open the
    /// MFT for another codec).
    target: Option<GpuTarget>,
}

impl MfVideoDecoder {
    /// Microsoft's H.264 decoder MFT, software, NV12 in system memory.
    pub fn new() -> Result<Self, AppError> {
        Self::open(VideoCodec::H264, None)
    }

    /// A decoder for `codec`. With a `target`, D3D11-accelerated on its
    /// device with zero-copy output to its slot — failing (no fallback
    /// here) when the MFT is not D3D11-aware or refuses the device; see
    /// [`open_best`](Self::open_best).
    pub fn open(codec: VideoCodec, target: Option<&GpuTarget>) -> Result<Self, AppError> {
        startup()?;
        let mft = create_mft(codec)?;
        let gpu = match target {
            Some(t) => Some(attach_device(&mft, t)?),
            None => None,
        };
        // SAFETY: COM/MF calls on the decoder transform created above and
        // on the media type created here; MF + COM were initialised by
        // `startup`.
        unsafe {
            if let Ok(a) = mft.GetAttributes() {
                let _ = a.SetUINT32(&MF_LOW_LATENCY, 1);
            }
            if let Ok(api) = mft.cast::<ICodecAPI>() {
                let _ = api.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true));
            }
            let t = MFCreateMediaType().map_err(|e| err("media type", e))?;
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| err("type", e))?;
            t.SetGUID(&MF_MT_SUBTYPE, &subtype(codec))
                .map_err(|e| err("type", e))?;
            mft.SetInputType(0, &t, 0)
                .map_err(|e| err(&format!("SetInputType({codec})"), e))?;
        }
        let mut d = Self {
            codec,
            mft,
            out_info: MFT_OUTPUT_STREAM_INFO::default(),
            out_w: 0,
            out_h: 0,
            stride: 0,
            sample: None,
            nv12: Vec::new(),
            time: 0,
            gpu,
            target: target.cloned(),
        };
        d.pick_output()?;
        // SAFETY: streaming messages to the transform we own.
        unsafe {
            d.mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|e| err("begin streaming", e))?;
            d.mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|e| err("start of stream", e))?;
        }
        Ok(d)
    }

    /// A decoder for `codec`: D3D11-accelerated on `target` when that
    /// works, else software. The error string says why the GPU path was
    /// not taken (`None` when it was, or when no target was given).
    pub fn open_best(
        codec: VideoCodec,
        target: Option<&GpuTarget>,
    ) -> Result<(Self, Option<String>), AppError> {
        let Some(t) = target else {
            return Ok((Self::open(codec, None)?, None));
        };
        match Self::open(codec, Some(t)) {
            Ok(d) => Ok((d, None)),
            Err(e) => Ok((Self::open(codec, None)?, Some(e.to_string()))),
        }
    }

    /// Whether an HEVC decoder MFT can be created on this machine (probed
    /// once: hardware DXVA through Microsoft's MFT, or the HEVC Video
    /// Extensions' software decoder).
    pub fn hevc_available() -> bool {
        static HEVC: OnceLock<bool> = OnceLock::new();
        *HEVC.get_or_init(|| Self::open(VideoCodec::Hevc, None).is_ok())
    }

    /// Whether pictures are decoded on the GPU (DXVA through D3D11).
    pub fn is_gpu(&self) -> bool {
        self.gpu.is_some()
    }

    /// The codec this decoder takes.
    pub fn codec(&self) -> VideoCodec {
        self.codec
    }

    /// For logs: codec and decode path.
    pub fn name(&self) -> String {
        match self.gpu {
            Some(_) => format!("{} (D3D11 / DXVA, zero-copy present)", self.codec),
            None => format!("{} (software)", self.codec),
        }
    }

    /// Pick the decoder's NV12 output type and record its geometry.
    fn pick_output(&mut self) -> Result<(), AppError> {
        // SAFETY: COM calls on the decoder's own transform and the media types
        // it offers.
        unsafe {
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
            Err(AppError::codec(format!(
                "the {} decoder offers no NV12 output",
                self.codec
            )))
        }
    }

    /// Feed one Annex-B access unit; the newest picture it produced (if
    /// any) waits in `self.sample`. Whether one came out.
    fn feed(&mut self, annexb: &[u8]) -> Result<bool, AppError> {
        self.sample = None;
        let s = memory_sample(annexb, annexb.len()).map_err(|e| err("input sample", e))?;
        self.time += 333_333;
        // SAFETY: COM calls on the decoder's own transform with the live
        // sample created above.
        unsafe {
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
        }
        self.pull_all()?;
        Ok(self.sample.is_some())
    }

    /// Pull every picture available now; keeps the newest in `self.sample`.
    fn pull_all(&mut self) -> Result<(), AppError> {
        for _ in 0..16 {
            let cap = (self.out_w.max(16) * self.out_h.max(16) * 3 / 2) as usize;
            match pull(&self.mft, &self.out_info, cap)? {
                Pulled::NeedInput => return Ok(()),
                Pulled::StreamChange => self.pick_output()?,
                Pulled::Sample(s) => self.sample = Some(s),
            }
        }
        Ok(())
    }

    /// Copy the pending picture's NV12 to `self.nv12` (system memory; a
    /// DXGI surface is read back by Media Foundation).
    fn take_nv12(&mut self) -> Result<bool, AppError> {
        let Some(s) = self.sample.take() else {
            return Ok(false);
        };
        sample_bytes(&s, &mut self.nv12).map_err(|e| err("picture bytes", e))?;
        // A tight contiguous copy has pitch = width.
        if self.nv12.len() == (self.out_w * self.out_h * 3 / 2) as usize {
            self.stride = self.out_w;
        }
        Ok(true)
    }

    /// Decode one Annex-B access unit into system-memory NV12. `Ok(true)`
    /// when a picture came out (see [`picture`](Self::picture)).
    pub fn decode_nv12(&mut self, annexb: &[u8]) -> Result<bool, AppError> {
        if !self.feed(annexb)? {
            return Ok(false);
        }
        self.take_nv12()
    }

    /// The last [`decode_nv12`](Self::decode_nv12) picture:
    /// `(nv12, pitch, coded_w, coded_h)`.
    pub fn picture(&self) -> (&[u8], u32, u32, u32) {
        (&self.nv12, self.stride, self.out_w, self.out_h)
    }

    /// The pending picture as a D3D11 surface (texture + array slice), when
    /// it is one.
    fn pending_surface(&self) -> Option<(ID3D11Texture2D, u32)> {
        let s = self.sample.as_ref()?;
        // SAFETY: COM queries on the live output sample; `p` receives an
        // AddRef'd texture pointer that `from_raw` takes ownership of.
        unsafe {
            let buf = s.GetBufferByIndex(0).ok()?;
            let dxgi: IMFDXGIBuffer = buf.cast().ok()?;
            let mut p = std::ptr::null_mut();
            dxgi.GetResource(&ID3D11Texture2D::IID, &mut p).ok()?;
            if p.is_null() {
                return None;
            }
            let tex = ID3D11Texture2D::from_raw(p);
            let slice = dxgi.GetSubresourceIndex().unwrap_or(0);
            Some((tex, slice))
        }
    }

    /// The pending picture's top-left `w x h` as tight BGRA in `out`: on
    /// the GPU + one readback when it is a D3D11 surface, else NV12 ->
    /// BGRA on the CPU. Hides any GPU picture (the framebuffer gets this
    /// one).
    fn take_cpu(&mut self, w: u32, h: u32, out: &mut Vec<u8>) -> Result<bool, AppError> {
        if let Some(gpu) = &self.gpu {
            gpu.slot.hide();
        }
        if let Some((tex, slice)) = self.pending_surface() {
            if let Some(gpu) = self.gpu.as_mut() {
                self.sample = None;
                let bgra = gpu.conv.convert(&tex, slice, w, h)?;
                gpu.conv.readback(&bgra, w, h, out)?;
                return Ok(true);
            }
        }
        if !self.take_nv12()? {
            return Ok(false);
        }
        if w > self.out_w || h > self.out_h {
            return Err(AppError::codec(format!(
                "decoded picture {}x{} smaller than the declared {w}x{h}",
                self.out_w, self.out_h
            )));
        }
        let uv = self.stride as usize * self.out_h as usize;
        nv12_to_bgra(&self.nv12, self.stride as usize, uv, w, h, out)?;
        Ok(true)
    }

    /// Present the pending picture on the GPU at `header.rect`. `false`
    /// when it is not a D3D11 surface (left pending for the CPU path).
    fn take_gpu(&mut self, header: &VideoHeader) -> Result<bool, AppError> {
        let Some((tex, slice)) = self.pending_surface() else {
            return Ok(false);
        };
        let Some(gpu) = self.gpu.as_mut() else {
            return Ok(false);
        };
        self.sample = None;
        let (w, h) = (u32::from(header.rect.w), u32::from(header.rect.h));
        let texture = gpu.conv.convert(&tex, slice, w, h)?;
        gpu.slot.show(GpuPicture {
            texture,
            rect: header.rect,
            screen_w: header.screen_w,
            screen_h: header.screen_h,
        });
        Ok(true)
    }

    /// Re-open for `codec` (same presenter; software when the GPU path
    /// fails for it).
    fn switch_codec(&mut self, codec: VideoCodec) -> Result<(), AppError> {
        if let Some(gpu) = &self.gpu {
            gpu.slot.hide();
        }
        let target = self.target.take();
        let (d, why) = Self::open_best(codec, target.as_ref())?;
        if let Some(why) = why {
            tracing::info!("{codec} decoder: no D3D11 acceleration ({why}); software");
        }
        *self = d;
        self.target = target;
        Ok(())
    }

    /// A GPU failure (device removed, out of memory): continue in software
    /// for good. The caller asks for a keyframe (the picture chain is lost).
    fn drop_gpu(&mut self, why: &AppError) {
        tracing::warn!("D3D11 video decode failed ({why}); decoding in software");
        if let Some(gpu) = &self.gpu {
            gpu.slot.hide();
        }
        match Self::open(self.codec, None) {
            Ok(d) => *self = d,
            Err(e) => tracing::warn!("software {} decoder: {e}", self.codec),
        }
    }

    /// [`take_cpu`](Self::take_cpu) / [`take_gpu`](Self::take_gpu) after a
    /// GPU-capable decode, falling back to software on a GPU error.
    fn finish<T>(&mut self, r: Result<T, AppError>) -> Result<T, String> {
        r.map_err(|e| {
            if self.gpu.is_some() {
                self.drop_gpu(&e);
            }
            e.to_string()
        })
    }
}

impl VideoDecode for MfVideoDecoder {
    fn decode(&mut self, annexb: &[u8], w: u32, h: u32, out: &mut Vec<u8>) -> Result<bool, String> {
        self.decode_codec(VideoCodec::H264, annexb, w, h, out)
    }

    fn hevc(&self) -> bool {
        Self::hevc_available()
    }

    fn decode_codec(
        &mut self,
        codec: VideoCodec,
        annexb: &[u8],
        w: u32,
        h: u32,
        out: &mut Vec<u8>,
    ) -> Result<bool, String> {
        if codec != self.codec {
            self.switch_codec(codec)?;
        }
        if !self.feed(annexb)? {
            return Ok(false);
        }
        let r = self.take_cpu(w, h, out);
        self.finish(r)
    }

    fn decode_unit(
        &mut self,
        header: &VideoHeader,
        annexb: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<DecodedTo, String> {
        let codec = VideoCodec::from_wire(header.codec)
            .ok_or_else(|| format!("unknown video codec {}", header.codec))?;
        let (w, h) = (u32::from(header.rect.w), u32::from(header.rect.h));
        let on_gpu = self.gpu.as_ref().is_some_and(|g| g.slot.enabled());
        if !on_gpu {
            return Ok(match self.decode_codec(codec, annexb, w, h, out)? {
                true => DecodedTo::Cpu,
                false => DecodedTo::Nothing,
            });
        }
        if codec != self.codec {
            self.switch_codec(codec)?;
        }
        if !self.feed(annexb)? {
            return Ok(DecodedTo::Nothing);
        }
        let r = self.take_gpu(header);
        if self.finish(r)? {
            return Ok(DecodedTo::Gpu);
        }
        // A system-memory picture (the MFT fell back to software decoding).
        let r = self.take_cpu(w, h, out);
        Ok(match self.finish(r)? {
            true => DecodedTo::Cpu,
            false => DecodedTo::Nothing,
        })
    }

    fn end_gpu(&mut self, out: &mut Vec<u8>) -> bool {
        let Some(gpu) = self.gpu.as_mut() else {
            return false;
        };
        let Some(p) = gpu.slot.hide() else {
            return false;
        };
        let (w, h) = (u32::from(p.rect.w), u32::from(p.rect.h));
        match gpu.conv.readback(&p.texture, w, h, out) {
            Ok(()) => true,
            Err(e) => {
                tracing::debug!("GPU picture readback failed: {e}");
                false
            }
        }
    }
}

impl Drop for MfVideoDecoder {
    fn drop(&mut self) {
        if let Some(gpu) = &self.gpu {
            gpu.slot.hide();
        }
        // SAFETY: end-of-stream messages to the transform we own, once, as
        // it is dropped.
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
    }
}

/// The compressed input subtype of `codec`.
fn subtype(codec: VideoCodec) -> GUID {
    match codec {
        VideoCodec::H264 => MFVideoFormat_H264,
        VideoCodec::Hevc => MFVideoFormat_HEVC,
    }
}

/// The decoder transform for `codec`: Microsoft's MFT by CLSID, else (HEVC
/// from the Store extension) the first registered synchronous decoder.
fn create_mft(codec: VideoCodec) -> Result<IMFTransform, AppError> {
    let clsid = match codec {
        VideoCodec::H264 => &CLSID_MSH264DecoderMFT,
        VideoCodec::Hevc => &CLSID_MSH265DecoderMFT,
    };
    // SAFETY: plain in-process COM activation (COM initialised by
    // `startup`).
    let first = unsafe { CoCreateInstance::<_, IMFTransform>(clsid, None, CLSCTX_INPROC_SERVER) };
    match first {
        Ok(m) => Ok(m),
        Err(e) => {
            for a in registered_decoders(&subtype(codec)) {
                // SAFETY: activating a registered MFT through its own
                // activation object.
                if let Ok(m) = unsafe { a.ActivateObject::<IMFTransform>() } {
                    return Ok(m);
                }
            }
            Err(err(&format!("{codec} decoder"), e))
        }
    }
}

/// Registered synchronous decoders taking `subtype` (best first).
fn registered_decoders(subtype: &GUID) -> Vec<IMFActivate> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: *subtype,
    };
    let flags = MFT_ENUM_FLAG(
        MFT_ENUM_FLAG_SYNCMFT.0 | MFT_ENUM_FLAG_LOCALMFT.0 | MFT_ENUM_FLAG_SORTANDFILTER.0,
    );
    let mut pp: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut n = 0u32;
    // SAFETY: MFTEnumEx only writes the two out-pointers (valid locals); the
    // type-info struct outlives the call.
    let r = unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_DECODER,
            flags,
            Some(&input),
            None,
            &mut pp,
            &mut n,
        )
    };
    if r.is_err() || pp.is_null() {
        return Vec::new();
    }
    let mut v = Vec::with_capacity(n as usize);
    for i in 0..n as usize {
        // SAFETY: on success `pp` is a CoTaskMem array of `n` initialised
        // `Option<IMFActivate>`; `take` moves each reference out once.
        if let Some(a) = unsafe { (*pp.add(i)).take() } {
            v.push(a);
        }
    }
    // SAFETY: every element was moved out above (left `None`); the array is
    // freed exactly once.
    unsafe { CoTaskMemFree(Some(pp as *const _)) };
    v
}

/// Hand `target`'s device to the decoder (`MFT_MESSAGE_SET_D3D_MANAGER`):
/// DXVA decoding into D3D11 surfaces.
fn attach_device(mft: &IMFTransform, target: &GpuTarget) -> Result<Dxva, AppError> {
    let device: &ID3D11Device = &target.device;
    // SAFETY: COM/MF calls on the caller's live transform and device and on
    // the device manager created here; out-pointers are valid locals. The
    // manager pointer passed in the message stays alive (`_mgr`) for the
    // decoder's lifetime (the MFT AddRefs it anyway).
    unsafe {
        let aware = mft
            .GetAttributes()
            .and_then(|a| a.GetUINT32(&MF_SA_D3D11_AWARE))
            .unwrap_or(0);
        if aware == 0 {
            return Err(AppError::platform("the decoder MFT is not D3D11-aware"));
        }
        // The decoder's worker threads share the presenter's immediate
        // context.
        if let Ok(mt) = device.cast::<ID3D11Multithread>() {
            let _ = mt.SetMultithreadProtected(true);
        }
        let mut token = 0u32;
        let mut mgr: Option<IMFDXGIDeviceManager> = None;
        MFCreateDXGIDeviceManager(&mut token, &mut mgr)
            .map_err(|e| err("MFCreateDXGIDeviceManager", e))?;
        let mgr = mgr.ok_or_else(|| AppError::platform("no DXGI device manager"))?;
        mgr.ResetDevice(device, token)
            .map_err(|e| err("ResetDevice", e))?;
        mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, mgr.as_raw() as usize)
            .map_err(|e| err("MFT_MESSAGE_SET_D3D_MANAGER", e))?;
        let conv = Nv12Converter::new(device)?;
        Ok(Dxva {
            _mgr: mgr,
            conv,
            slot: target.slot.clone(),
        })
    }
}
