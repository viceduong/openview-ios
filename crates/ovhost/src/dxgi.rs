//! DXGI Desktop Duplication capture.
//!
//! `IDXGIOutputDuplication::AcquireNextFrame` hands us the composed desktop
//! image of one output plus the OS's dirty and move rects since the last
//! acquire. We copy only those rects (GPU `CopySubresourceRegion` into a
//! persistent staging texture, then `Map` and copy the rows into a
//! persistent CPU mirror) and pass the rects on as diff *hints* — the tile
//! encoder still verifies them with a memcmp, so a spurious OS rect costs a
//! compare, never wire bytes. No per-frame allocations: the staging
//! texture, the mirror and the rect buffers live as long as the duplication.
//!
//! The cursor is not part of the duplicated image (it is sent separately as
//! CURSOR / CURSOR_SHAPE). The process is per-monitor DPI aware
//! (`ovapp::gdisrc::enable_dpi_awareness`), so output desktop coordinates
//! are physical pixels — the same space as the monitor list and GDI.
//!
//! Errors split in two: [`DxgiError::Lost`] (ACCESS_LOST on a desktop
//! switch / mode change / UAC secure desktop, or any failed acquire) means
//! "re-create the duplication, maybe later"; creation itself failing (RDP
//! session, no output covering the region, rotated output, secure desktop)
//! makes the caller fall back to GDI.

use ovapp::tiles::TileRect;
use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_UNSPECIFIED,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication,
    IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_MORE_DATA, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_MOVE_RECT,
};

/// Above this many changed rects in one acquire, copy the whole region in
/// one GPU call instead of one call per rect.
const MAX_RECT_COPIES: usize = 32;
/// Every this many damaged frames, re-copy the whole region (and hand the
/// encoder a whole-region hint): self-heals the mirror should the OS ever
/// under-report a dirty rect. Costs one full copy + diff (~1-2 ms).
const FULL_REFRESH_EVERY: u32 = 300;

/// GPU encode input: keep a GPU-resident copy of the region for the H.264
/// encoder's D3D11 input. On by default (`--no-gpu-encode` turns it off);
/// a device without video support, or an encoder that rejects the
/// textures, falls back to the CPU mirror automatically.
pub(crate) static GPU_ENCODE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

/// A failed acquire: re-create the duplication (now or after a backoff).
#[derive(Debug)]
pub(crate) enum DxgiError {
    Lost(String),
}

impl std::fmt::Display for DxgiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DxgiError::Lost(e) => write!(f, "desktop duplication lost: {e}"),
        }
    }
}

/// One output's desktop duplication, cropped to a capture region.
pub(crate) struct DxgiCapture {
    _device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    dup: IDXGIOutputDuplication,
    /// CPU-readable copy targets, the size of the crop region, used
    /// alternately: a frame's copy never waits on the previous
    /// frame's Unmap, and the duplication frame is released before Map.
    staging: [ID3D11Texture2D; 2],
    /// Which staging texture the next frame copies into.
    stage_idx: usize,
    /// QPC present time of the oldest image change not yet handed out by
    /// [`take_present_qpc`](Self::take_present_qpc) (0 = none): the
    /// capture-phase latency probe.
    first_present: i64,
    /// The OS move rects of the last update (region coordinates,
    /// source and destination inside the region): copy-rect candidates
    /// the encoder verifies against its mirror.
    moves: Vec<ovapp::bands::MoveRect>,
    /// Output desktop size (the duplicated texture's size).
    out_w: u32,
    out_h: u32,
    /// Capture region inside the output (output pixels).
    crop_x: u32,
    crop_y: u32,
    w: u32,
    h: u32,
    /// The region's current pixels (tight BGRA, alpha opaque).
    mirror: Vec<u8>,
    have_image: bool,
    /// Last update's changed rects (region coordinates); `None` = unknown
    /// (first image: compare everything).
    hints: Vec<TileRect>,
    hints_valid: bool,
    dirty_buf: Vec<RECT>,
    move_buf: Vec<DXGI_OUTDUPL_MOVE_RECT>,
    /// Scratch: changed rects in output coordinates, clipped to the crop.
    changed: Vec<RECT>,
    /// Damaged frames since the last whole-region copy.
    since_full: u32,
    /// GPU encode input: the region on the GPU (same pixels as
    /// `mirror`), for the encoder's D3D11 input.
    gpu_mirror: Option<ID3D11Texture2D>,
    /// Event-driven capture: [`wait`](Self::wait) already
    /// pulled a changed image into the mirror; the next
    /// [`update`](Self::update) keeps (and extends) its hints.
    fresh: bool,
}

fn hr_msg(what: &str, e: &windows::core::Error) -> String {
    format!("{what}: {:#010x} {}", e.code().0 as u32, e.message())
}

impl DxgiCapture {
    /// Duplicate the output that fully contains the desktop rect
    /// `(x, y, w, h)` and crop to it; fails unless the initial desktop
    /// image arrives within ~1 s (the tests' strict constructor; the
    /// capture source uses [`open_region`](Self::open_region)).
    #[cfg(test)]
    pub(crate) fn new(x: i32, y: i32, w: u32, h: u32) -> Result<Self, String> {
        let me = Self::open_region(x, y, w, h, 1000)?;
        if me.have_image {
            Ok(me)
        } else {
            Err("DXGI delivered no initial desktop image".into())
        }
    }

    /// Like `new`, but waits at most `wait_ms` for the initial
    /// image and returns the duplication even without one
    /// ([`has_image`](Self::has_image) false): DWM presents nothing while
    /// the output has no active monitor (TV off, display asleep), and only
    /// the first present after that fills the mirror. Polling such a
    /// duplication with [`update`](Self::update) never blocks.
    pub(crate) fn open_region(
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        wait_ms: u32,
    ) -> Result<Self, String> {
        if w == 0 || h == 0 {
            return Err("empty capture region".into());
        }
        // SAFETY: plain COM calls; every returned interface is owned by a
        // smart pointer and released on drop.
        unsafe {
            let factory: IDXGIFactory1 =
                CreateDXGIFactory1().map_err(|e| hr_msg("CreateDXGIFactory1", &e))?;
            let mut ai = 0;
            while let Ok(adapter) = factory.EnumAdapters1(ai) {
                ai += 1;
                let mut oi = 0;
                while let Ok(output) = adapter.EnumOutputs(oi) {
                    oi += 1;
                    let Ok(desc) = output.GetDesc() else { continue };
                    let r = desc.DesktopCoordinates;
                    let inside = x >= r.left
                        && y >= r.top
                        && x + w as i32 <= r.right
                        && y + h as i32 <= r.bottom;
                    if !inside || !desc.AttachedToDesktop.as_bool() {
                        continue;
                    }
                    if desc.Rotation != DXGI_MODE_ROTATION_IDENTITY
                        && desc.Rotation != DXGI_MODE_ROTATION_UNSPECIFIED
                    {
                        return Err("rotated output (not supported by DXGI path)".into());
                    }
                    let output1: IDXGIOutput1 =
                        output.cast().map_err(|e| hr_msg("IDXGIOutput1", &e))?;
                    return Self::open(
                        &adapter,
                        &output1,
                        ((r.right - r.left) as u32, (r.bottom - r.top) as u32),
                        ((x - r.left) as u32, (y - r.top) as u32),
                        (w, h),
                        wait_ms,
                    );
                }
            }
        }
        Err(format!("no DXGI output contains {w}x{h} at ({x},{y})"))
    }

    /// Create the device, the duplication of `output` and the staging
    /// textures, then wait up to `wait_ms` for the first image.
    fn open(
        adapter: &IDXGIAdapter1,
        output: &IDXGIOutput1,
        (out_w, out_h): (u32, u32),
        (crop_x, crop_y): (u32, u32),
        (w, h): (u32, u32),
        wait_ms: u32,
    ) -> Result<Self, String> {
        let mut device: Option<ID3D11Device> = None;
        let mut ctx: Option<ID3D11DeviceContext> = None;
        let mut gpu = GPU_ENCODE.load(std::sync::atomic::Ordering::Relaxed);
        let create =
            |flags, device: &mut Option<ID3D11Device>, ctx: &mut Option<ID3D11DeviceContext>| {
                // SAFETY: a live adapter, no software module, and valid
                // out-slots for the device and its immediate context.
                unsafe {
                    D3D11CreateDevice(
                        adapter,
                        D3D_DRIVER_TYPE_UNKNOWN,
                        HMODULE::default(),
                        flags,
                        None,
                        D3D11_SDK_VERSION,
                        Some(device),
                        None,
                        Some(ctx),
                    )
                }
            };
        // The H.264 MFT shares this device (video processor + encoder):
        // try with video support first, without it as the fallback.
        let mut made = if gpu {
            create(
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                &mut device,
                &mut ctx,
            )
        } else {
            Err(windows::core::Error::empty())
        };
        if made.is_err() {
            gpu = false;
            made = create(D3D11_CREATE_DEVICE_BGRA_SUPPORT, &mut device, &mut ctx);
        }
        made.map_err(|e| hr_msg("D3D11CreateDevice", &e))?;
        let device = device.ok_or("D3D11CreateDevice returned no device")?;
        let ctx = ctx.ok_or("D3D11CreateDevice returned no context")?;
        // Fails with E_ACCESSDENIED on the secure desktop / in some RDP
        // sessions, DXGI_ERROR_UNSUPPORTED on some drivers: GDI fallback.
        // SAFETY: COM call on live objects (the device belongs to the
        // output's adapter).
        let dup = unsafe { output.DuplicateOutput(&device) }
            .map_err(|e| hr_msg("DuplicateOutput", &e))?;
        let desc = D3D11_TEXTURE2D_DESC {
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
        let mk = || -> Result<ID3D11Texture2D, String> {
            let mut t: Option<ID3D11Texture2D> = None;
            // SAFETY: a valid texture description, no initial data, and a
            // valid out-slot.
            unsafe { device.CreateTexture2D(&desc, None, Some(&mut t)) }
                .map_err(|e| hr_msg("CreateTexture2D(staging)", &e))?;
            t.ok_or_else(|| "CreateTexture2D returned no texture".to_string())
        };
        let staging = [mk()?, mk()?];
        let gpu_mirror = if gpu {
            if let Ok(mt) = device.cast::<ID3D11Multithread>() {
                // SAFETY: COM call on a live interface of our device.
                let _ = unsafe { mt.SetMultithreadProtected(true) };
            }
            let gd = D3D11_TEXTURE2D_DESC {
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                ..desc
            };
            let mut t: Option<ID3D11Texture2D> = None;
            // SAFETY: as for the staging textures above.
            unsafe { device.CreateTexture2D(&gd, None, Some(&mut t)) }
                .ok()
                .and(t)
        } else {
            None
        };
        let mut me = Self {
            gpu_mirror,
            _device: device,
            ctx,
            dup,
            staging,
            stage_idx: 0,
            first_present: 0,
            moves: Vec::new(),
            out_w,
            out_h,
            crop_x,
            crop_y,
            w,
            h,
            mirror: vec![0; w as usize * h as usize * 4],
            have_image: false,
            hints: Vec::new(),
            hints_valid: false,
            dirty_buf: Vec::new(),
            move_buf: Vec::new(),
            changed: Vec::new(),
            since_full: 0,
            fresh: false,
        };
        // The first acquire after DuplicateOutput delivers the whole
        // desktop as soon as DWM presents (at once on a live display); wait
        // up to `wait_ms` for it so the source starts with a valid image.
        let t0 = std::time::Instant::now();
        loop {
            let left = u128::from(wait_ms).saturating_sub(t0.elapsed().as_millis());
            me.acquire(left.min(200) as u32)
                .map_err(|e| e.to_string())?;
            if me.have_image || left == 0 {
                return Ok(me);
            }
        }
    }

    /// Whether the mirror holds a desktop image yet (see
    /// [`open_region`](Self::open_region)).
    pub(crate) fn has_image(&self) -> bool {
        self.have_image
    }

    /// Poll for a new desktop image (non-blocking). Afterwards
    /// [`frame`](Self::frame) is the current image.
    pub(crate) fn update(&mut self) -> Result<(), DxgiError> {
        let r = self.acquire(0).map(|_| ());
        self.fresh = false;
        r
    }

    /// GPU encode input: the region's GPU copy (valid after
    /// [`update`](Self::update), like [`frame`](Self::frame)).
    pub(crate) fn gpu_frame(&self) -> Option<ovapp::mfcodec::GpuFrame> {
        Some(ovapp::mfcodec::GpuFrame {
            device: self._device.clone(),
            texture: self.gpu_mirror.clone()?,
        })
    }

    /// Event-driven capture: block up to `timeout_ms` for the desktop image
    /// to change. A change is pulled into the mirror right away (its hints
    /// kept for the next [`update`](Self::update)); returns whether one
    /// arrived (pointer-only updates do not count).
    pub(crate) fn wait(&mut self, timeout_ms: u32) -> Result<bool, DxgiError> {
        let changed = self.acquire(timeout_ms)?;
        if changed {
            self.fresh = true;
        }
        Ok(changed)
    }

    /// The current image (tight BGRA, `w x h`) and the rects that changed
    /// in the last [`update`](Self::update) (`None` = compare everything).
    pub(crate) fn frame(&self) -> (&[u8], u32, u32, Option<&[TileRect]>) {
        let hints = self.hints_valid.then_some(&self.hints[..]);
        (&self.mirror, self.w, self.h, hints)
    }

    /// The OS move rects since the last [`update`](Self::update)
    /// (region coordinates; candidates only, verify before use). `None`
    /// when the hints are unknown.
    pub(crate) fn moves(&self) -> Option<&[ovapp::bands::MoveRect]> {
        self.hints_valid.then_some(&self.moves[..])
    }

    /// QPC timestamp at which the oldest not-yet-consumed image change was
    /// presented (then reset): `now - it` is how long the change waited
    /// for the capture.
    pub(crate) fn take_present_qpc(&mut self) -> Option<i64> {
        let t = std::mem::take(&mut self.first_present);
        (t != 0).then_some(t)
    }

    fn acquire(&mut self, timeout_ms: u32) -> Result<bool, DxgiError> {
        if !self.fresh {
            self.hints.clear();
            self.moves.clear();
            self.hints_valid = self.have_image;
        }
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut res: Option<IDXGIResource> = None;
        // SAFETY: COM call with valid out-pointers.
        match unsafe { self.dup.AcquireNextFrame(timeout_ms, &mut info, &mut res) } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(false),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                return Err(DxgiError::Lost(
                    "ACCESS_LOST (desktop switch / mode change)".into(),
                ))
            }
            Err(e) => return Err(DxgiError::Lost(hr_msg("AcquireNextFrame", &e))),
        }
        // From here the frame is held: always release it. The GPU copies
        // are queued (and flushed) before the release; the CPU read-back
        // (Map, which waits for them) runs after it, so DWM can already
        // compose the next frame meanwhile.
        let out = self.process(&info, res);
        // SAFETY: a frame is held (AcquireNextFrame succeeded).
        let rel = unsafe { self.dup.ReleaseFrame() };
        let copied = out?;
        if let Err(e) = rel {
            return Err(DxgiError::Lost(hr_msg("ReleaseFrame", &e)));
        }
        if !copied {
            return Ok(false);
        }
        self.read_back()?;
        Ok(true)
    }

    /// Map the staging texture the last `process` copied into and pull the
    /// changed rows into the mirror (alpha forced opaque in the same pass).
    fn read_back(&mut self) -> Result<(), DxgiError> {
        let staging = self.staging[self.stage_idx].clone();
        self.stage_idx ^= 1;
        // SAFETY: COM calls on live objects; the mapped rows are read within
        // the texture's extent (rects were clipped to the crop region).
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|e| DxgiError::Lost(hr_msg("Map(staging)", &e)))?;
            let pitch = mapped.RowPitch as usize;
            let src = mapped.pData as *const u8;
            let stride = self.w as usize * 4;
            for r in &self.changed {
                let x0 = (r.left as u32 - self.crop_x) as usize;
                let x1 = (r.right as u32 - self.crop_x) as usize;
                for y in
                    (r.top as u32 - self.crop_y) as usize..(r.bottom as u32 - self.crop_y) as usize
                {
                    let s = std::slice::from_raw_parts(src.add(y * pitch + x0 * 4), (x1 - x0) * 4);
                    let d = &mut self.mirror[y * stride + x0 * 4..y * stride + x1 * 4];
                    copy_opaque(d, s);
                }
                self.hints.push(TileRect {
                    x: x0 as u16,
                    y: (r.top as u32 - self.crop_y) as u16,
                    w: (x1 - x0) as u16,
                    h: (r.bottom - r.top) as u16,
                });
            }
            self.ctx.Unmap(&staging, 0);
        }
        self.have_image = true;
        Ok(())
    }

    fn process(
        &mut self,
        info: &DXGI_OUTDUPL_FRAME_INFO,
        res: Option<IDXGIResource>,
    ) -> Result<bool, DxgiError> {
        // Pointer-only update: the desktop image did not change.
        if info.LastPresentTime == 0 {
            return Ok(false);
        }
        if self.first_present == 0 {
            self.first_present = info.LastPresentTime;
        }
        let Some(res) = res else {
            return Ok(false);
        };
        let tex: ID3D11Texture2D = res
            .cast()
            .map_err(|e| DxgiError::Lost(hr_msg("desktop texture", &e)))?;
        let mut td = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: valid out-pointer.
        unsafe { tex.GetDesc(&mut td) };
        if td.Width != self.out_w || td.Height != self.out_h {
            return Err(DxgiError::Lost(format!(
                "output resized to {}x{} (was {}x{})",
                td.Width, td.Height, self.out_w, self.out_h
            )));
        }
        let crop = RECT {
            left: self.crop_x as i32,
            top: self.crop_y as i32,
            right: (self.crop_x + self.w) as i32,
            bottom: (self.crop_y + self.h) as i32,
        };
        self.changed.clear();
        self.since_full += 1;
        let mut full = !self.have_image || self.since_full >= FULL_REFRESH_EVERY;
        if !full {
            match self.collect_rects(info.TotalMetadataBufferSize) {
                Ok(()) => {}
                Err(_) => full = true, // metadata unavailable: copy everything
            }
            // The image was presented anew but no rect says where (no
            // metadata): copy everything rather than drop the change — the
            // next frame's rects would not cover it, leaving it stale until
            // something else redraws that area.
            if self.changed.len() > MAX_RECT_COPIES || self.changed.is_empty() {
                full = true;
            }
        }
        if full {
            self.since_full = 0;
            self.changed.clear();
            self.changed.push(crop);
        } else {
            // Clip to the crop; drop what falls outside.
            let mut kept = 0;
            for i in 0..self.changed.len() {
                let r = self.changed[i];
                let c = RECT {
                    left: r.left.max(crop.left),
                    top: r.top.max(crop.top),
                    right: r.right.min(crop.right),
                    bottom: r.bottom.min(crop.bottom),
                };
                if c.left < c.right && c.top < c.bottom {
                    self.changed[kept] = c;
                    kept += 1;
                }
            }
            self.changed.truncate(kept);
            if self.changed.is_empty() {
                return Ok(false);
            }
        }
        // GPU: copy the changed rects into this frame's staging texture
        // (region coordinates); `read_back` maps it after ReleaseFrame.
        let staging = self.staging[self.stage_idx].clone();
        // SAFETY: COM calls on live objects; boxes are inside both textures.
        unsafe {
            for r in &self.changed {
                let b = D3D11_BOX {
                    left: r.left as u32,
                    top: r.top as u32,
                    front: 0,
                    right: r.right as u32,
                    bottom: r.bottom as u32,
                    back: 1,
                };
                self.ctx.CopySubresourceRegion(
                    &staging,
                    0,
                    r.left as u32 - self.crop_x,
                    r.top as u32 - self.crop_y,
                    0,
                    &tex,
                    0,
                    Some(&b),
                );
                if let Some(g) = &self.gpu_mirror {
                    self.ctx.CopySubresourceRegion(
                        g,
                        0,
                        r.left as u32 - self.crop_x,
                        r.top as u32 - self.crop_y,
                        0,
                        &tex,
                        0,
                        Some(&b),
                    );
                }
            }
            self.ctx.Flush();
        }
        Ok(true)
    }

    /// Move-rect destinations + dirty rects of the held frame (output
    /// coordinates) into `self.changed`.
    fn collect_rects(&mut self, total: u32) -> Result<(), String> {
        let cap = (total as usize).max(64);
        // Move rects first (their destinations changed too).
        let mv = std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>();
        self.move_buf
            .resize(cap / mv + 1, DXGI_OUTDUPL_MOVE_RECT::default());
        let mut need = 0u32;
        loop {
            // SAFETY: buffer pointer/size describe `move_buf`.
            let r = unsafe {
                self.dup.GetFrameMoveRects(
                    (self.move_buf.len() * mv) as u32,
                    self.move_buf.as_mut_ptr(),
                    &mut need,
                )
            };
            match r {
                Ok(()) => break,
                Err(e) if e.code() == DXGI_ERROR_MORE_DATA => {
                    self.move_buf
                        .resize(need as usize / mv + 1, DXGI_OUTDUPL_MOVE_RECT::default());
                }
                Err(e) => return Err(hr_msg("GetFrameMoveRects", &e)),
            }
        }
        let crop = (
            self.crop_x as i32,
            self.crop_y as i32,
            (self.crop_x + self.w) as i32,
            (self.crop_y + self.h) as i32,
        );
        let inside = |l: i32, t: i32, r: i32, b: i32| {
            l >= crop.0 && t >= crop.1 && r <= crop.2 && b <= crop.3 && l < r && t < b
        };
        for m in &self.move_buf[..need as usize / mv] {
            self.changed.push(m.DestinationRect);
            // Copy-rect candidate: both rects inside the region.
            let d = m.DestinationRect;
            let (dx, dy) = (d.left - m.SourcePoint.x, d.top - m.SourcePoint.y);
            if inside(d.left, d.top, d.right, d.bottom)
                && inside(d.left - dx, d.top - dy, d.right - dx, d.bottom - dy)
                && i16::try_from(dx).is_ok()
                && i16::try_from(dy).is_ok()
            {
                self.moves.push(ovapp::bands::MoveRect {
                    x: (d.left - crop.0) as u16,
                    y: (d.top - crop.1) as u16,
                    w: (d.right - d.left) as u16,
                    h: (d.bottom - d.top) as u16,
                    dx: dx as i16,
                    dy: dy as i16,
                });
            }
        }
        let rs = std::mem::size_of::<RECT>();
        self.dirty_buf.resize(cap / rs + 1, RECT::default());
        loop {
            // SAFETY: buffer pointer/size describe `dirty_buf`.
            let r = unsafe {
                self.dup.GetFrameDirtyRects(
                    (self.dirty_buf.len() * rs) as u32,
                    self.dirty_buf.as_mut_ptr(),
                    &mut need,
                )
            };
            match r {
                Ok(()) => break,
                Err(e) if e.code() == DXGI_ERROR_MORE_DATA => {
                    self.dirty_buf
                        .resize(need as usize / rs + 1, RECT::default());
                }
                Err(e) => return Err(hr_msg("GetFrameDirtyRects", &e)),
            }
        }
        self.changed
            .extend_from_slice(&self.dirty_buf[..need as usize / rs]);
        Ok(())
    }
}

/// Copy BGRA pixels and force alpha opaque (the desktop image is opaque,
/// but alpha is not guaranteed and downstream treats Bgra as opaque): one
/// u32 OR per pixel on the aligned middle (vectorizes), bytes at the ends.
fn copy_opaque(dst: &mut [u8], src: &[u8]) {
    dst.copy_from_slice(src);
    // SAFETY: u8 -> u32 reinterpretation of plain bytes; align_to_mut keeps
    // the unaligned head/tail as bytes.
    let (head, mid, tail) = unsafe { dst.align_to_mut::<u32>() };
    let hl = head.len();
    for (i, v) in head.iter_mut().enumerate() {
        if i % 4 == 3 {
            *v = 255;
        }
    }
    // Byte k of a word is pixel byte (hl + k) % 4: alpha sits at k with
    // (hl + k) % 4 == 3 (little-endian word).
    let k = (3 + 4 - hl % 4) % 4;
    let mask = 0xFFu32 << (k * 8);
    mid.iter_mut().for_each(|p| *p |= mask);
    let start = hl + mid.len() * 4;
    for (i, v) in tail.iter_mut().enumerate() {
        if (start + i) % 4 == 3 {
            *v = 255;
        }
    }
}

#[cfg(test)]
mod tests;
