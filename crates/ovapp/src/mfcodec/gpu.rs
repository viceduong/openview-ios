//! GPU input: D3D11 BGRA textures -> video processor NV12 -> encoder MFT.

use std::mem::ManuallyDrop;

use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
    DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Media::MediaFoundation::*;

use super::encoder::H264Encoder;
use super::mft::{friendly_name, hardware_encoders};
use super::{err, startup};
use crate::error::AppError;
use crate::video::EncodedAu;

/// A captured picture on the GPU: the D3D11 device that owns it and a BGRA
/// texture (the host's desktop-duplication mirror). Handed to
/// [`VideoEncode::encode_gpu`](crate::video::VideoEncode::encode_gpu) as `&dyn Any`.
#[derive(Clone)]
pub struct GpuFrame {
    /// The device that owns `texture` (also the encoder's device).
    pub device: ID3D11Device,
    /// The BGRA picture.
    pub texture: ID3D11Texture2D,
}

/// Create a hardware D3D11 device suitable for capture + GPU encode
/// (BGRA + video support, multithread-protected).
pub fn gpu_device() -> Result<ID3D11Device, AppError> {
    // SAFETY: D3D11CreateDevice only writes the `device` out-pointer (a valid
    // local); every other pointer argument is None.
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
        let device = device.ok_or_else(|| AppError::platform("no D3D11 device"))?;
        protect(&device);
        Ok(device)
    }
}

/// The MFT and the capture thread share the device: serialise its
/// immediate context.
fn protect(device: &ID3D11Device) {
    if let Ok(mt) = device.cast::<ID3D11Multithread>() {
        // SAFETY: a plain flag setter on a live interface of `device`.
        unsafe {
            let _ = mt.SetMultithreadProtected(true);
        }
    }
}

/// NV12 surfaces in flight (the async MFT may still read the previous ones).
const GPU_RING: usize = 4;

/// The D3D11 side of a GPU-input encoder: the device manager handed to the
/// MFT, the video processor and the NV12 surface ring.
pub(super) struct GpuState {
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
    /// Wrap `device` (made multithread-protected) and create the DXGI device
    /// manager the MFT is given.
    pub(super) fn new(device: &ID3D11Device) -> Result<(Self, IMFDXGIDeviceManager), AppError> {
        // SAFETY: COM/MF calls on the caller's live `device` and on objects created
        // from it here; the out-pointers are valid locals.
        unsafe {
            protect(device);
            let mut token = 0u32;
            let mut mgr: Option<IMFDXGIDeviceManager> = None;
            MFCreateDXGIDeviceManager(&mut token, &mut mgr)
                .map_err(|e| err("MFCreateDXGIDeviceManager", e))?;
            let mgr = mgr.ok_or_else(|| AppError::platform("no DXGI device manager"))?;
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
    }

    /// BGRA `src` (crop `rect`) -> the next NV12 ring surface (BT.709
    /// limited range, like `bgra_to_nv12`).
    pub(super) fn convert(
        &mut self,
        src: &ID3D11Texture2D,
        rect: (u32, u32, u32, u32),
        w: u32,
        h: u32,
    ) -> Result<ID3D11Texture2D, AppError> {
        // SAFETY: D3D11 video calls on the processor, views and textures owned by
        // `self` and on the caller's live `src` texture. The stream's
        // `ManuallyDrop` input view is a clone released exactly once after
        // `VideoProcessorBlt`.
        unsafe {
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
                    let tex = tex.ok_or_else(|| AppError::platform("no NV12 texture"))?;
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
                    self.ring
                        .push((tex, ov.ok_or_else(|| AppError::platform("no output view"))?));
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
                self.input = Some((
                    src.clone(),
                    iv.ok_or_else(|| AppError::platform("no input view"))?,
                ));
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
    ) -> Result<Self, AppError> {
        if w < 16 || h < 16 || !w.is_multiple_of(2) || !h.is_multiple_of(2) {
            return Err(AppError::invalid(format!(
                "H.264 picture {w}x{h} must be even and at least 16x16"
            )));
        }
        startup()?;
        let mut why = Vec::new();
        // SAFETY: COM/MF calls on activation objects and transforms this
        // function owns and on the caller's live `device`; MF + COM were
        // initialised by `startup` above.
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
        Err(AppError::platform(why.join("; ")))
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
    ) -> Result<Vec<EncodedAu>, AppError> {
        let (w, h) = (self.w, self.h);
        let gpu = self
            .gpu
            .as_mut()
            .ok_or_else(|| AppError::invalid("not a GPU-input encoder"))?;
        let nv12 = gpu.convert(src, rect, w, h)?;
        // SAFETY: MF calls wrapping the live NV12 ring texture (MF AddRefs
        // it) in a sample owned by this function.
        unsafe {
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
