//! Direct2D presentation: the remote frame is
//! drawn with `D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC` (a pre-filtered
//! downscale, cubic upscale), the remote cursor
//! as its own premultiplied layer on top (never baked into the frame), and
//! the HUD with DirectWrite, into a flip-model swap chain presented on
//! vsync without blocking the viewer loop (`DXGI_PRESENT_DO_NOT_WAIT`;
//! a refused present is retried on the next loop turn).
//!
//! Any failure (no D3D11 device, device removed) makes the window drop the
//! renderer and fall back to the GDI path for good.
//!
//! Threading: the factory is single-threaded; every call runs on the window
//! thread, serialized by the `Mutex` the renderer lives in.

use windows::core::{w, Interface};
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_IGNORE, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
    D2D_RECT_F, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Bitmap1, ID2D1DeviceContext, ID2D1Factory1, ID2D1SolidColorBrush,
    D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_NONE, D2D1_BITMAP_OPTIONS_TARGET,
    D2D1_BITMAP_PROPERTIES1, D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_DRAW_TEXT_OPTIONS_NONE,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC,
    D2D1_INTERPOLATION_MODE_LINEAR,
};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat, DWRITE_FACTORY_TYPE_SHARED,
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_NORMAL,
    DWRITE_MEASURING_MODE_NATURAL,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIAdapter, IDXGIDevice1, IDXGIFactory2, IDXGISurface, IDXGISwapChain1,
    DXGI_ERROR_WAS_STILL_DRAWING, DXGI_MWA_NO_ALT_ENTER, DXGI_PRESENT_DO_NOT_WAIT,
    DXGI_SCALING_NONE, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_FLIP_DISCARD,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
};

use super::{CursorBmp, CursorOverlay, HudState};
use crate::localinput::Rect;
use ovapp::screen::Frame;

#[cfg(test)]
mod tests;

/// The renderer state bound to one window.
pub(super) struct D2dRenderer {
    swap: IDXGISwapChain1,
    dc: ID2D1DeviceContext,
    text: IDWriteTextFormat,
    hud_brush: ID2D1SolidColorBrush,
    dead_brush: ID2D1SolidColorBrush,
    panel_brush: ID2D1SolidColorBrush,
    /// The back buffer as a D2D target, rebuilt on resize.
    target: Option<ID2D1Bitmap1>,
    size: (u32, u32),
    /// The frame bitmap and the seq it holds.
    frame_bmp: Option<(ID2D1Bitmap1, u32, u32, u64)>,
    /// The cursor bitmap and the shape it was built from.
    cursor_bmp: Option<(ID2D1Bitmap1, CursorBmp)>,
    pub(super) name: String,
}

fn hr(what: &str, e: windows::core::Error) -> String {
    format!("{what}: {e}")
}

fn color(r: f32, g: f32, b: f32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r, g, b, a }
}

/// A D3D11 device of `driver_type` with BGRA support.
fn d3d11_device(driver_type: D3D_DRIVER_TYPE) -> windows::core::Result<Option<ID3D11Device>> {
    let mut device: Option<ID3D11Device> = None;
    // SAFETY: no adapter, no software module, default feature levels; the
    // only out-param is `device`, a valid `Option<ID3D11Device>` slot.
    unsafe {
        D3D11CreateDevice(
            None,
            driver_type,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
    }?;
    Ok(device)
}

/// A D3D11 device (hardware, else WARP) with BGRA support.
fn create_device() -> Result<(ID3D11Device, &'static str), String> {
    if let Ok(Some(device)) = d3d11_device(D3D_DRIVER_TYPE_HARDWARE) {
        return Ok((device, "hardware"));
    }
    let device = d3d11_device(D3D_DRIVER_TYPE_WARP).map_err(|e| hr("D3D11CreateDevice", e))?;
    Ok((device.ok_or("no D3D11 device")?, "WARP"))
}

/// A D2D device context on `device`.
fn create_dc(dxgi: &IDXGIDevice1) -> Result<ID2D1DeviceContext, String> {
    // SAFETY: plain COM object creation on live interfaces; every result is
    // checked.
    unsafe {
        let d2d: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)
            .map_err(|e| hr("D2D1CreateFactory", e))?;
        let d2d_dev = d2d.CreateDevice(dxgi).map_err(|e| hr("ID2D1Device", e))?;
        d2d_dev
            .CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)
            .map_err(|e| hr("CreateDeviceContext", e))
    }
}

impl D2dRenderer {
    /// Bind a flip-model swap chain + D2D device context to `hwnd`.
    pub(super) fn new(hwnd: HWND) -> Result<Self, String> {
        let (device, driver) = create_device()?;
        // SAFETY: plain COM/D3D object creation on live interfaces; `hwnd` is
        // the caller's live window; every result is checked.
        unsafe {
            let dxgi: IDXGIDevice1 = device.cast().map_err(|e| hr("IDXGIDevice1", e))?;
            // One frame in flight: the newest picture is what gets shown.
            let _ = dxgi.SetMaximumFrameLatency(1);
            let adapter: IDXGIAdapter = dxgi.GetAdapter().map_err(|e| hr("GetAdapter", e))?;
            let adapter_name = adapter
                .GetDesc()
                .map(|d| {
                    String::from_utf16_lossy(&d.Description)
                        .trim_end_matches('\0')
                        .to_owned()
                })
                .unwrap_or_default();
            let factory: IDXGIFactory2 = adapter.GetParent().map_err(|e| hr("IDXGIFactory2", e))?;
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: 0,
                Height: 0,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                Stereo: false.into(),
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_NONE,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                Flags: 0,
            };
            let swap = factory
                .CreateSwapChainForHwnd(&device, hwnd, &desc, None, None)
                .map_err(|e| hr("CreateSwapChainForHwnd", e))?;
            let _ = factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER);
            let dc = create_dc(&dxgi)?;
            let dw: IDWriteFactory =
                DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED).map_err(|e| hr("DWrite", e))?;
            let text = dw
                .CreateTextFormat(
                    w!("Consolas"),
                    None,
                    DWRITE_FONT_WEIGHT_NORMAL,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    13.0,
                    w!(""),
                )
                .map_err(|e| hr("CreateTextFormat", e))?;
            let hud_brush = dc
                .CreateSolidColorBrush(&color(0.565, 1.0, 0.69, 1.0), None)
                .map_err(|e| hr("brush", e))?;
            let dead_brush = dc
                .CreateSolidColorBrush(&color(1.0, 0.30, 0.0, 1.0), None)
                .map_err(|e| hr("brush", e))?;
            let panel_brush = dc
                .CreateSolidColorBrush(&color(0.0, 0.0, 0.0, 0.55), None)
                .map_err(|e| hr("brush", e))?;
            Ok(Self {
                swap,
                dc,
                text,
                hud_brush,
                dead_brush,
                panel_brush,
                target: None,
                size: (0, 0),
                frame_bmp: None,
                cursor_bmp: None,
                name: format!(
                    "Direct2D on {adapter_name} ({driver}), flip-model vsync, high-quality cubic"
                ),
            })
        }
    }

    /// Bind the swap chain's back buffer (resized to the client area) as
    /// the device context's target.
    ///
    /// # Safety
    ///
    /// Must run on the window thread that owns the single-threaded factory,
    /// outside a `BeginDraw`/`EndDraw` pair.
    unsafe fn ensure_target(&mut self, cw: u32, ch: u32) -> Result<(), String> {
        if self.target.is_some() && self.size == (cw, ch) {
            return Ok(());
        }
        // SAFETY: (this and the blocks below) live COM interfaces, called on
        // the owning thread outside a draw, per this function's contract;
        // `props` outlives the call that reads it.
        unsafe { self.dc.SetTarget(None) };
        self.target = None;
        if self.size != (cw, ch) || self.size == (0, 0) {
            // SAFETY: as above (the target was just released).
            let resized = unsafe {
                self.swap
                    .ResizeBuffers(0, cw, ch, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG(0))
            };
            // First use only matches the client size exactly: not fatal.
            if self.size != (0, 0) {
                resized.map_err(|e| hr("ResizeBuffers", e))?;
            }
        }
        // SAFETY: as above.
        let surface: IDXGISurface =
            unsafe { self.swap.GetBuffer(0) }.map_err(|e| hr("GetBuffer", e))?;
        let props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_IGNORE,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
            colorContext: std::mem::ManuallyDrop::new(None),
        };
        // SAFETY: as above.
        let bmp = unsafe { self.dc.CreateBitmapFromDxgiSurface(&surface, Some(&props)) }
            .map_err(|e| hr("CreateBitmapFromDxgiSurface", e))?;
        // SAFETY: as above.
        unsafe { self.dc.SetTarget(&bmp) };
        self.target = Some(bmp);
        self.size = (cw, ch);
        Ok(())
    }

    /// Copy `f` into the frame bitmap (re-created when the size changes;
    /// skipped when it already holds this seq).
    ///
    /// # Safety
    ///
    /// Must run on the window thread that owns the single-threaded factory;
    /// `f` must be complete (`stride * height` bytes of pixels).
    unsafe fn upload_frame(&mut self, f: &Frame) -> Result<(), String> {
        let fresh = match &self.frame_bmp {
            Some((_, w, h, _)) => *w != f.width || *h != f.height,
            None => true,
        };
        if fresh {
            let props = D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_IGNORE,
                },
                dpiX: 96.0,
                dpiY: 96.0,
                bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
                colorContext: std::mem::ManuallyDrop::new(None),
            };
            // SAFETY: live device context on the owning thread; no initial
            // data is read.
            let bmp = unsafe {
                self.dc.CreateBitmap(
                    D2D_SIZE_U {
                        width: f.width,
                        height: f.height,
                    },
                    None,
                    0,
                    &props,
                )
            }
            .map_err(|e| hr("CreateBitmap(frame)", e))?;
            self.frame_bmp = Some((bmp, f.width, f.height, u64::MAX));
        }
        if let Some((bmp, _, _, seq)) = self.frame_bmp.as_mut() {
            if *seq != f.seq || fresh {
                // SAFETY: the bitmap is `width x height` and `f.pixels` holds
                // `stride * height` bytes (a complete frame, per the
                // contract), so the copy reads only inside the buffer.
                unsafe { bmp.CopyFromMemory(None, f.pixels.as_ptr().cast(), f.stride) }
                    .map_err(|e| hr("CopyFromMemory", e))?;
                *seq = f.seq;
            }
        }
        Ok(())
    }

    /// The cursor bitmap for `shape` (cached while the shape is unchanged);
    /// `None` for an empty or truncated shape.
    ///
    /// # Safety
    ///
    /// Must run on the window thread that owns the single-threaded factory.
    unsafe fn cursor_bitmap(&mut self, shape: &CursorBmp) -> Result<Option<ID2D1Bitmap1>, String> {
        if shape.w == 0
            || shape.h == 0
            || shape.bgra.len() < usize::from(shape.w) * usize::from(shape.h) * 4
        {
            return Ok(None);
        }
        if let Some((b, s)) = &self.cursor_bmp {
            if s == shape {
                return Ok(Some(b.clone()));
            }
        }
        let props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
            colorContext: std::mem::ManuallyDrop::new(None),
        };
        // SAFETY: the length check above guarantees `bgra` holds
        // `w * 4 * h` bytes, exactly what a `w x h` bitmap with pitch `w * 4`
        // reads; the device context is used on its owning thread (contract).
        let b = unsafe {
            self.dc.CreateBitmap(
                D2D_SIZE_U {
                    width: u32::from(shape.w),
                    height: u32::from(shape.h),
                },
                Some(shape.bgra.as_ptr().cast()),
                u32::from(shape.w) * 4,
                &props,
            )
        }
        .map_err(|e| hr("CreateBitmap(cursor)", e))?;
        self.cursor_bmp = Some((b.clone(), shape.clone()));
        Ok(Some(b))
    }

    /// Draw and present. `Ok(false)`: the swap chain was still busy with the
    /// previous frame (vsync); the caller repaints shortly.
    pub(super) fn render(
        &mut self,
        frame: Option<&Frame>,
        cursor: &CursorOverlay,
        hud: &HudState,
        rect: Rect,
        client: (i32, i32),
    ) -> Result<bool, String> {
        let (cw, ch) = (client.0.max(1) as u32, client.1.max(1) as u32);
        // SAFETY: all D2D calls run on the window thread that owns the
        // single-threaded factory (the renderer is only reached from the
        // `WndProc`); the frame is only uploaded when complete; buffers and
        // rects outlive the calls that read them.
        unsafe {
            self.ensure_target(cw, ch)?;
            if let Some(f) = frame.filter(|f| f.is_complete() && f.width > 0 && f.height > 0) {
                self.upload_frame(f)?;
            }
            self.dc.BeginDraw();
            self.dc.Clear(Some(&color(0.0, 0.0, 0.0, 1.0)));
            let dest = D2D_RECT_F {
                left: rect.x as f32,
                top: rect.y as f32,
                right: (rect.x + rect.w) as f32,
                bottom: (rect.y + rect.h) as f32,
            };
            let mut scale = 1.0f32;
            if let (Some(f), Some((bmp, ..))) = (frame, self.frame_bmp.as_ref()) {
                scale = rect.w as f32 / f.width.max(1) as f32;
                let mode = if (rect.w as u32, rect.h as u32) == (f.width, f.height) {
                    D2D1_INTERPOLATION_MODE_LINEAR // 1:1, exact either way
                } else {
                    D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC
                };
                self.dc.DrawBitmap(
                    bmp,
                    Some(&dest),
                    if hud.dead { 0.5 } else { 1.0 },
                    mode,
                    None,
                    None,
                );
            }
            // The remote cursor: its own layer, scaled with the frame.
            if let (Some((x, y)), Some(shape)) = (cursor.pos, cursor.shape.as_ref()) {
                if let Some(cb) = self.cursor_bitmap(shape)? {
                    let s = scale.max(0.25);
                    let left = rect.x as f32 + (x - i32::from(shape.hotspot_x)) as f32 * s;
                    let top = rect.y as f32 + (y - i32::from(shape.hotspot_y)) as f32 * s;
                    let r = D2D_RECT_F {
                        left,
                        top,
                        right: left + f32::from(shape.w) * s,
                        bottom: top + f32::from(shape.h) * s,
                    };
                    self.dc.DrawBitmap(
                        &cb,
                        Some(&r),
                        1.0,
                        D2D1_INTERPOLATION_MODE_LINEAR,
                        None,
                        None,
                    );
                }
            }
            // HUD.
            let (x0, y0) = (rect.x as f32 + 8.0, rect.y as f32 + 8.0);
            if hud.visible {
                let panel = D2D_RECT_F {
                    left: x0 - 4.0,
                    top: y0 - 2.0,
                    right: x0 + 560.0,
                    bottom: y0 + hud.lines.len() as f32 * 16.0 + 2.0,
                };
                self.dc.FillRectangle(&panel, &self.panel_brush);
                for (i, line) in hud.lines.iter().enumerate() {
                    let wl: Vec<u16> = line.encode_utf16().collect();
                    let r = D2D_RECT_F {
                        left: x0,
                        top: y0 + i as f32 * 16.0,
                        right: x0 + 2000.0,
                        bottom: y0 + i as f32 * 16.0 + 16.0,
                    };
                    self.dc.DrawText(
                        &wl,
                        &self.text,
                        &r,
                        &self.hud_brush,
                        D2D1_DRAW_TEXT_OPTIONS_NONE,
                        DWRITE_MEASURING_MODE_NATURAL,
                    );
                }
            }
            if hud.dead {
                let wl: Vec<u16> = "Peer disconnected \u{2014} waiting for traffic\u{2026}"
                    .encode_utf16()
                    .collect();
                let by = if hud.visible {
                    y0 + hud.lines.len() as f32 * 16.0 + 12.0
                } else {
                    y0 + 12.0
                };
                let r = D2D_RECT_F {
                    left: cw as f32 / 2.0 - 160.0,
                    top: by,
                    right: cw as f32,
                    bottom: by + 20.0,
                };
                self.dc.DrawText(
                    &wl,
                    &self.text,
                    &r,
                    &self.dead_brush,
                    D2D1_DRAW_TEXT_OPTIONS_NONE,
                    DWRITE_MEASURING_MODE_NATURAL,
                );
            }
            self.dc.EndDraw(None, None).map_err(|e| hr("EndDraw", e))?;
            let rc = self.swap.Present(1, DXGI_PRESENT_DO_NOT_WAIT);
            if rc == DXGI_ERROR_WAS_STILL_DRAWING {
                return Ok(false);
            }
            rc.ok().map_err(|e| hr("Present", e))?;
            Ok(true)
        }
    }
}
