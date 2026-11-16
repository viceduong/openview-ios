//! Decoder-side D3D11: the picture slot a presenter draws from, and the
//! video-processor conversion of the decoder's NV12 surfaces to BGRA.
//!
//! A D3D11-accelerated [`MfVideoDecoder`](super::MfVideoDecoder) shares the
//! presenter's device ([`GpuTarget`]). Every picture it decodes stays on the
//! GPU: the decoder's NV12 surface (a slice of its texture array) is
//! converted by `ID3D11VideoProcessor` into one BGRA texture
//! ([`Nv12Converter`]) that is published in a [`GpuPictureSlot`]; the
//! presenter draws that texture over the framebuffer at the picture's rect.
//! The CPU never touches the pixels, except for an explicit
//! [`Nv12Converter::readback`] (a staging copy) when the picture must reach
//! the framebuffer (motion ended, recording).

use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use windows::core::Interface;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};

use super::err;
use crate::error::AppError;
use crate::tiles::TileRect;

/// A decoded picture on the GPU, ready to draw.
#[derive(Clone, Debug)]
pub struct GpuPicture {
    /// `rect.w x rect.h` BGRA (full-range sRGB), bindable as a shader
    /// resource on the presenter's device.
    pub texture: ID3D11Texture2D,
    /// Where it goes in the screen (the video unit's rect).
    pub rect: TileRect,
    /// The screen size `rect` refers to.
    pub screen_w: u32,
    /// The screen size `rect` refers to.
    pub screen_h: u32,
}

struct SlotInner {
    picture: Mutex<Option<GpuPicture>>,
    enabled: AtomicBool,
}

/// The decoder -> presenter hand-off: the GPU picture to draw over the
/// framebuffer (if any), and whether the presenter can draw one at all.
///
/// Cheap to clone (shared). The presenter disables it when it cannot draw
/// GPU pictures (no Direct2D, recording, latency probe): the decoder then
/// produces CPU pixels again and hides what it showed.
#[derive(Clone)]
pub struct GpuPictureSlot(Arc<SlotInner>);

impl Default for GpuPictureSlot {
    fn default() -> Self {
        Self::new()
    }
}

impl GpuPictureSlot {
    /// An empty, enabled slot.
    pub fn new() -> Self {
        Self(Arc::new(SlotInner {
            picture: Mutex::new(None),
            enabled: AtomicBool::new(true),
        }))
    }

    /// Whether the presenter draws GPU pictures.
    pub fn enabled(&self) -> bool {
        self.0.enabled.load(Ordering::Relaxed)
    }

    /// Let the decoder present on the GPU (`true`) or make it paint the
    /// framebuffer on the CPU (`false`).
    pub fn set_enabled(&self, on: bool) {
        self.0.enabled.store(on, Ordering::Relaxed);
    }

    /// The picture to draw now, if any.
    pub fn current(&self) -> Option<GpuPicture> {
        self.0.picture.lock().ok().and_then(|p| p.clone())
    }

    /// Show `p` (replacing the previous picture).
    pub(super) fn show(&self, p: GpuPicture) {
        if let Ok(mut slot) = self.0.picture.lock() {
            *slot = Some(p);
        }
    }

    /// Stop showing a picture; returns the one that was shown.
    pub(super) fn hide(&self) -> Option<GpuPicture> {
        self.0.picture.lock().ok().and_then(|mut p| p.take())
    }
}

/// Where a D3D11-accelerated decoder presents: the presenter's device
/// (created with `D3D11_CREATE_DEVICE_VIDEO_SUPPORT`, e.g. by
/// [`gpu_device`](super::gpu_device)) and its picture slot.
#[derive(Clone)]
pub struct GpuTarget {
    /// The device the decoder decodes on and the presenter draws with.
    pub device: ID3D11Device,
    /// The decoder -> presenter hand-off.
    pub slot: GpuPictureSlot,
}

/// Input views cached per decoder surface (the MFT's pool is small).
const MAX_INPUT_VIEWS: usize = 48;

/// The video processor and its geometry: `(in w, in h, out w, out h)`.
type Processor = (
    (u32, u32, u32, u32),
    ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessor,
);

/// NV12 decoder surfaces -> one BGRA texture, on the GPU.
pub(super) struct Nv12Converter {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    vdev: ID3D11VideoDevice,
    vctx: ID3D11VideoContext,
    vp: Option<Processor>,
    /// Input views by (surface, array slice).
    views: Vec<(ID3D11Texture2D, u32, ID3D11VideoProcessorInputView)>,
    /// The BGRA output and its view, `(w, h)`.
    out: Option<((u32, u32), ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    /// CPU-readable copy for [`readback`](Self::readback).
    staging: Option<((u32, u32), ID3D11Texture2D)>,
}

impl Nv12Converter {
    /// The video device/context of `device`.
    pub(super) fn new(device: &ID3D11Device) -> Result<Self, AppError> {
        // SAFETY: interface queries on the caller's live device.
        unsafe {
            let vdev: ID3D11VideoDevice = device.cast().map_err(|e| err("ID3D11VideoDevice", e))?;
            let ctx = device
                .GetImmediateContext()
                .map_err(|e| err("immediate context", e))?;
            let vctx: ID3D11VideoContext = ctx.cast().map_err(|e| err("ID3D11VideoContext", e))?;
            Ok(Self {
                device: device.clone(),
                ctx,
                vdev,
                vctx,
                vp: None,
                views: Vec::new(),
                out: None,
                staging: None,
            })
        }
    }

    /// The processor for `src_size` (the coded surface) -> `w x h`.
    fn processor(&mut self, src_size: (u32, u32), w: u32, h: u32) -> Result<(), AppError> {
        let key = (src_size.0, src_size.1, w, h);
        if self.vp.as_ref().is_some_and(|(k, ..)| *k == key) {
            return Ok(());
        }
        let rate = DXGI_RATIONAL {
            Numerator: 60,
            Denominator: 1,
        };
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: src_size.0,
            InputHeight: src_size.1,
            OutputFrameRate: rate,
            OutputWidth: w,
            OutputHeight: h,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        // SAFETY: D3D11 video calls on our own video device/context and the
        // processor created here; `desc` outlives the call reading it.
        unsafe {
            let en = self
                .vdev
                .CreateVideoProcessorEnumerator(&desc)
                .map_err(|e| err("CreateVideoProcessorEnumerator", e))?;
            let vp = self
                .vdev
                .CreateVideoProcessor(&en, 0)
                .map_err(|e| err("CreateVideoProcessor", e))?;
            // Colour: BT.709 studio-range YCbCr in (what the host encodes),
            // full-range sRGB out — the same maths as `nv12_to_bgra`.
            if let Ok(v1) = self.vctx.cast::<ID3D11VideoContext1>() {
                v1.VideoProcessorSetStreamColorSpace1(
                    &vp,
                    0,
                    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
                );
                v1.VideoProcessorSetOutputColorSpace1(&vp, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
            } else {
                // YCbCr_Matrix 1 (BT.709), Nominal_Range 1 (16-235) in;
                // RGB_Range 0 (full) out.
                let input = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                    _bitfield: (1 << 2) | (1 << 4),
                };
                let output = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
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
            self.vp = Some((key, en, vp));
        }
        // Views belong to the old enumerator.
        self.views.clear();
        self.out = None;
        Ok(())
    }

    /// The BGRA output texture (+ view) for `w x h`.
    fn output(&mut self, w: u32, h: u32) -> Result<(), AppError> {
        if self.out.as_ref().is_some_and(|(s, ..)| *s == (w, h)) {
            return Ok(());
        }
        let Some((_, en, _)) = self.vp.as_ref() else {
            return Err(AppError::platform("no video processor"));
        };
        let td = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            // RENDER_TARGET: the processor writes it; SHADER_RESOURCE: the
            // presenter draws it (a Direct2D bitmap on its DXGI surface).
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let od = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        // SAFETY: resource creation on our device; the descriptors outlive
        // the calls and the out-pointers are valid locals.
        unsafe {
            let mut tex: Option<ID3D11Texture2D> = None;
            self.device
                .CreateTexture2D(&td, None, Some(&mut tex))
                .map_err(|e| err("CreateTexture2D(BGRA)", e))?;
            let tex = tex.ok_or_else(|| AppError::platform("no BGRA texture"))?;
            let mut ov: Option<ID3D11VideoProcessorOutputView> = None;
            self.vdev
                .CreateVideoProcessorOutputView(&tex, en, &od, Some(&mut ov))
                .map_err(|e| err("CreateVideoProcessorOutputView", e))?;
            let ov = ov.ok_or_else(|| AppError::platform("no output view"))?;
            self.out = Some(((w, h), tex, ov));
        }
        Ok(())
    }

    /// The input view of `src` slice `slice` (cached).
    fn input_view(
        &mut self,
        src: &ID3D11Texture2D,
        slice: u32,
    ) -> Result<ID3D11VideoProcessorInputView, AppError> {
        if let Some((.., v)) = self.views.iter().find(|(t, s, _)| t == src && *s == slice) {
            return Ok(v.clone());
        }
        let Some((_, en, _)) = self.vp.as_ref() else {
            return Err(AppError::platform("no video processor"));
        };
        let id = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: slice,
                },
            },
        };
        let mut iv: Option<ID3D11VideoProcessorInputView> = None;
        // SAFETY: a view of the caller's live decoder surface on our device;
        // the descriptor and out-pointer are valid locals.
        unsafe {
            self.vdev
                .CreateVideoProcessorInputView(src, en, &id, Some(&mut iv))
                .map_err(|e| err("CreateVideoProcessorInputView", e))?;
        }
        let iv = iv.ok_or_else(|| AppError::platform("no input view"))?;
        if self.views.len() >= MAX_INPUT_VIEWS {
            self.views.remove(0);
        }
        self.views.push((src.clone(), slice, iv.clone()));
        Ok(iv)
    }

    /// Convert the top-left `w x h` of decoder surface `src` (array slice
    /// `slice`) into the BGRA output texture, returned.
    pub(super) fn convert(
        &mut self,
        src: &ID3D11Texture2D,
        slice: u32,
        w: u32,
        h: u32,
    ) -> Result<ID3D11Texture2D, AppError> {
        let mut sd = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: a descriptor query on the caller's live texture.
        unsafe { src.GetDesc(&mut sd) };
        if w == 0 || h == 0 || w > sd.Width || h > sd.Height {
            return Err(AppError::codec(format!(
                "decoded surface {}x{} smaller than the declared {w}x{h}",
                sd.Width, sd.Height
            )));
        }
        self.processor((sd.Width, sd.Height), w, h)?;
        self.output(w, h)?;
        let iv = self.input_view(src, slice)?;
        let (Some((_, _, vp)), Some((_, tex, ov))) = (self.vp.as_ref(), self.out.as_ref()) else {
            return Err(AppError::platform("no video processor"));
        };
        let r = RECT {
            left: 0,
            top: 0,
            right: w as i32,
            bottom: h as i32,
        };
        let mut streams = [D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: std::ptr::null_mut(),
            pInputSurface: ManuallyDrop::new(Some(iv)),
            ppFutureSurfaces: std::ptr::null_mut(),
            ppPastSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: ManuallyDrop::new(None),
            ppFutureSurfacesRight: std::ptr::null_mut(),
        }];
        // SAFETY: D3D11 video calls on our processor and views; the rects
        // and the stream array outlive the calls. The stream's input view is
        // an owned clone released exactly once, after the blit.
        let blt = unsafe {
            self.vctx
                .VideoProcessorSetStreamSourceRect(vp, 0, true, Some(&r));
            self.vctx
                .VideoProcessorSetStreamDestRect(vp, 0, true, Some(&r));
            self.vctx
                .VideoProcessorSetOutputTargetRect(vp, true, Some(&r));
            let blt = self.vctx.VideoProcessorBlt(vp, ov, 0, &streams);
            ManuallyDrop::drop(&mut streams[0].pInputSurface);
            blt
        };
        blt.map_err(|e| err("VideoProcessorBlt", e))?;
        Ok(tex.clone())
    }

    /// Copy the `w x h` BGRA texture `tex` (on our device) into `out`,
    /// tight (`w * h * 4` bytes): a staging copy, then a mapped read.
    pub(super) fn readback(
        &mut self,
        tex: &ID3D11Texture2D,
        w: u32,
        h: u32,
        out: &mut Vec<u8>,
    ) -> Result<(), AppError> {
        if self.staging.as_ref().is_none_or(|(s, _)| *s != (w, h)) {
            let td = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut st: Option<ID3D11Texture2D> = None;
            // SAFETY: resource creation on our device; valid locals.
            unsafe { self.device.CreateTexture2D(&td, None, Some(&mut st)) }
                .map_err(|e| err("CreateTexture2D(staging)", e))?;
            let st = st.ok_or_else(|| AppError::platform("no staging texture"))?;
            self.staging = Some(((w, h), st));
        }
        let Some((_, st)) = self.staging.as_ref() else {
            return Err(AppError::platform("no staging texture"));
        };
        let (wu, hu) = (w as usize, h as usize);
        out.resize(wu * hu * 4, 0);
        // SAFETY: `st` and `tex` are same-size BGRA textures on our device.
        // While mapped, `pData` points at `RowPitch * h` readable bytes
        // (RowPitch >= w * 4); every row read stays inside that, and the
        // texture is unmapped before returning.
        unsafe {
            self.ctx.CopyResource(st, tex);
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx
                .Map(st, 0, D3D11_MAP_READ, 0, Some(&mut m))
                .map_err(|e| err("Map(staging)", e))?;
            let pitch = m.RowPitch as usize;
            let src = m.pData.cast::<u8>();
            for (y, row) in out.chunks_exact_mut(wu * 4).enumerate() {
                std::ptr::copy_nonoverlapping(src.add(y * pitch), row.as_mut_ptr(), wu * 4);
            }
            self.ctx.Unmap(st, 0);
        }
        Ok(())
    }
}
