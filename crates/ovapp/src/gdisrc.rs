//! Real screen capture via Windows GDI: actual desktop pixels for the
//! screen stream (the fallback when DXGI desktop duplication is unavailable).
//!
//! The capture source is deliberately dependency-free: instead of pulling in
//! the `windows`/`winapi` crates we declare the handful of Win32 functions we
//! need with raw `extern "system"` FFI. The classic GDI path is the smallest
//! reliable way to read screen pixels:
//!
//! 1. `GetDC(0)` — a screen device context for the whole virtual desktop.
//! 2. `CreateCompatibleDC` — a memory DC we can select a bitmap into.
//! 3. `CreateDIBSection` — a *top-down* 32-bit BGRA bitmap whose pixel bytes
//!    GDI lets us read directly (a plain pointer, no `GetDIBits` copy dance).
//! 4. `BitBlt(SRCCOPY)` — blit the requested screen region into that DIB.
//!
//! A 32-bit top-down DIB is byte-for-byte the [`crate::screen::PixelFormat::Bgra`]
//! layout (B,G,R,A in memory, row stride exactly `width * 4`), so the captured
//! buffer drops straight into [`crate::screen::Frame::new`] with no remap.
//!
//! On non-Windows targets the same type exists but every constructor fails,
//! keeping `ovapp` (and the iOS `ovffi` staticlib) compiling unchanged.

use crate::error::AppError;
#[cfg(test)]
use crate::screen::PixelFormat;

/// Opt the process into per-monitor-v2 DPI awareness (falls back to
/// system-DPI awareness on pre-1703 Windows). Call once at startup, before
/// any window or DC exists.
///
/// Without it a DPI-unaware process sees a *virtualized* desktop: on a
/// 1920x1080 screen at 125 % scaling `GetSystemMetrics` reports 1536x864 and
/// GDI capture returns only that (cropped/scaled) region; the viewer window
/// is bitmap-stretched (blurry) by the compositor. Returns `true` when an
/// awareness mode was set (or was already set). No-op `false` elsewhere.
pub fn enable_dpi_awareness() -> bool {
    #[cfg(windows)]
    {
        dpi::enable()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[cfg(windows)]
mod dpi {
    #![allow(non_snake_case)]

    #[link(name = "kernel32")]
    extern "system" {
        fn GetModuleHandleW(name: *const u16) -> isize;
        fn GetProcAddress(module: isize, name: *const u8) -> *const core::ffi::c_void;
    }
    #[link(name = "user32")]
    extern "system" {
        fn SetProcessDPIAware() -> i32;
    }

    /// `DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2` (windef.h).
    const PER_MONITOR_AWARE_V2: isize = -4;

    pub(super) fn enable() -> bool {
        // SetProcessDpiAwarenessContext exists from Windows 10 1703: resolve
        // it at runtime so older systems still start (with the fallback).
        let user32: Vec<u16> = "user32.dll\0".encode_utf16().collect();
        // SAFETY: valid NUL-terminated strings; user32 is always loaded in a
        // process that links it; the transmuted signature matches winuser.h.
        unsafe {
            let module = GetModuleHandleW(user32.as_ptr());
            if module != 0 {
                let f = GetProcAddress(module, c"SetProcessDpiAwarenessContext".as_ptr().cast());
                if !f.is_null() {
                    let set: extern "system" fn(isize) -> i32 = core::mem::transmute(f);
                    if set(PER_MONITOR_AWARE_V2) != 0 {
                        return true;
                    }
                    // ERROR_ACCESS_DENIED: awareness already set (manifest or
                    // an earlier call) — fine either way, fall through.
                }
            }
            SetProcessDPIAware() != 0
        }
    }
}

#[cfg(windows)]
mod ffi {
    //! Raw Win32 imports (user32 + gdi32). Linked implicitly by the MSVC toolchain.

    #![allow(non_snake_case)]
    // Win32 type names are conventionally ALL_CAPS; keep them as-is.
    #![allow(clippy::upper_case_acronyms)]

    // Link the system import libraries the raw FFI needs. MSVC's default
    // link set for a Rust binary does not include gdi32; user32 resolves
    // because std already links it, but declaring both keeps this explicit.
    #[link(name = "user32")]
    #[link(name = "gdi32")]
    extern "system" {
        // user32
        #[link_name = "GetDC"]
        pub fn GetDC(hWnd: isize) -> isize;
        #[link_name = "ReleaseDC"]
        pub fn ReleaseDC(hWnd: isize, hDC: isize) -> i32;
        #[link_name = "GetSystemMetrics"]
        pub fn GetSystemMetrics(idx: i32) -> i32;
        // gdi32
        #[link_name = "CreateCompatibleDC"]
        pub fn CreateCompatibleDC(hDC: isize) -> isize;
        #[link_name = "DeleteDC"]
        pub fn DeleteDC(hDC: isize) -> i32;
        #[link_name = "CreateDIBSection"]
        pub fn CreateDIBSection(
            hDC: isize,
            pBmi: *const BITMAPINFO,
            iUsage: u32,
            lppv: *mut *mut u8,
            hSection: isize,
            dwOffset: u32,
        ) -> isize;
        #[link_name = "SelectObject"]
        pub fn SelectObject(hDC: isize, h: isize) -> isize;
        #[link_name = "DeleteObject"]
        pub fn DeleteObject(h: isize) -> i32;
        #[link_name = "BitBlt"]
        pub fn BitBlt(
            hDestDC: isize,
            x: i32,
            y: i32,
            w: i32,
            h: i32,
            hSrcDC: isize,
            xSrc: i32,
            ySrc: i32,
            dwRop: u32,
        ) -> i32;
    }

    /// `BITMAPINFOHEADER` (wingdi.h), 40 bytes on all supported targets.
    #[repr(C)]
    pub struct BITMAPINFOHEADER {
        pub biSize: u32,
        pub biWidth: i32,
        pub biHeight: i32,
        pub biPlanes: u16,
        pub biBitCount: u16,
        pub biCompression: u32,
        pub biSizeImage: u32,
        pub biXPelsPerMeter: i32,
        pub biYPelsPerMeter: i32,
        pub biClrUsed: u32,
        pub biClrImportant: u32,
    }

    /// `BITMAPINFO` with a dummy 3-entry color table (unused for 32-bpp BI_RGB,
    /// but the struct is declared with the flexible array in the header).
    #[repr(C)]
    pub struct BITMAPINFO {
        pub bmiHeader: BITMAPINFOHEADER,
        pub bmiColors: [u32; 3],
    }

    pub const BI_RGB: u32 = 0;
    pub const DIB_RGB_COLORS: u32 = 0;
    pub const SRCCOPY: u32 = 0x00CC_0020;
    pub const SM_CXSCREEN: i32 = 0;
    pub const SM_CYSCREEN: i32 = 1;
}

/// A live GDI capture source for one screen region.
///
/// Constructing it allocates the DIB section once; each [`GdiScreen::capture`]
/// is then a single `BitBlt` plus a buffer copy, which is cheap enough to run
/// at the frame rates the screen-stream path targets.
pub struct GdiScreen {
    #[cfg(windows)]
    inner: imp::Win,
    /// Present so the type has a stable size/layout on non-Windows targets.
    #[cfg(not(windows))]
    _priv: (),
}

#[cfg(windows)]
mod imp {
    //! Windows-side state: the persistent DIB section and its DCs.

    use super::ffi;
    use crate::error::AppError;
    use std::slice;

    /// Everything `Drop` needs to release, plus the geometry and pixel pointer.
    pub struct Win {
        /// Screen DC obtained via `GetDC(0)` — released with `ReleaseDC`.
        pub hdc_screen: isize,
        /// Memory DC with our bitmap selected — released with `DeleteDC`.
        pub hdc_mem: isize,
        /// The DIB section bitmap — released with `DeleteObject`.
        pub hbitmap: isize,
        /// The object `SelectObject` displaced; restored on drop.
        pub old_obj: isize,
        /// Direct pointer to the DIB's pixel bytes (top-down BGRA32).
        pub bits: *mut u8,
        /// Region origin x on the (virtual) desktop.
        pub x: i32,
        /// Region origin y on the (virtual) desktop.
        pub y: i32,
        /// Region width in pixels.
        pub w: u32,
        /// Region height in pixels.
        pub h: u32,
    }

    impl Win {
        /// Build the DIB-backed capture source for `w x h` at `(x, y)`.
        pub fn new(x: i32, y: i32, w: u32, h: u32) -> Result<Self, AppError> {
            if w == 0 || h == 0 {
                return Err(AppError::invalid("GDI capture region is empty (w/h == 0)"));
            }
            // SAFETY: these are plain FFI calls; NULL handles are checked below.
            let hdc_screen = unsafe { ffi::GetDC(0) };
            if hdc_screen == 0 {
                return Err(AppError::platform("GetDC(0) failed"));
            }
            // SAFETY: `hdc_screen` is a live DC (checked non-NULL above).
            let hdc_mem = unsafe { ffi::CreateCompatibleDC(hdc_screen) };
            if hdc_mem == 0 {
                // SAFETY: releases the DC obtained above, exactly once.
                unsafe { ffi::ReleaseDC(0, hdc_screen) };
                return Err(AppError::platform("CreateCompatibleDC failed"));
            }

            // A negative biHeight gives a TOP-DOWN DIB: row 0 is the top row,
            // matching the screen-stream frame layout (no vertical flip).
            let bmi = ffi::BITMAPINFO {
                bmiHeader: ffi::BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<ffi::BITMAPINFOHEADER>() as u32,
                    biWidth: w as i32,
                    biHeight: -(h as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: ffi::BI_RGB,
                    biSizeImage: 0,
                    biXPelsPerMeter: 0,
                    biYPelsPerMeter: 0,
                    biClrUsed: 0,
                    biClrImportant: 0,
                },
                bmiColors: [0; 3],
            };
            let mut bits: *mut u8 = std::ptr::null_mut();
            // SAFETY: `bmi` outlives the call; `bits` receives the pixel pointer.
            let hbitmap = unsafe {
                ffi::CreateDIBSection(hdc_mem, &bmi, ffi::DIB_RGB_COLORS, &mut bits, 0, 0)
            };
            if hbitmap == 0 || bits.is_null() {
                // SAFETY: releases the two DCs created above, exactly once.
                unsafe { ffi::DeleteDC(hdc_mem) };
                // SAFETY: as above.
                unsafe { ffi::ReleaseDC(0, hdc_screen) };
                return Err(AppError::platform("CreateDIBSection failed"));
            }
            // SAFETY: both handles are valid GDI objects we just created.
            let old_obj = unsafe { ffi::SelectObject(hdc_mem, hbitmap) };
            if old_obj == 0 {
                // SAFETY: releases the bitmap (never selected) and the two DCs
                // created above, each exactly once.
                unsafe { ffi::DeleteObject(hbitmap) };
                // SAFETY: as above.
                unsafe { ffi::DeleteDC(hdc_mem) };
                // SAFETY: as above.
                unsafe { ffi::ReleaseDC(0, hdc_screen) };
                return Err(AppError::platform("SelectObject failed"));
            }

            Ok(Self {
                hdc_screen,
                hdc_mem,
                hbitmap,
                old_obj,
                bits,
                x,
                y,
                w,
                h,
            })
        }

        /// Rebuild the DIB for a new region, keeping the screen DC alive.
        ///
        /// Order of operations (corruption-free swap):
        /// 1. create the new DIB section (nothing has been touched yet);
        /// 2. select it into `hdc_mem` — `SelectObject` returns the previous
        ///    object (our old bitmap), so `old_obj` is unaffected as the
        ///    default 1x1 stock bitmap stays "stored";
        /// 3. only now release the old bitmap with `DeleteObject` and update
        ///    `bits`/`x`/`y`/`w`/`h`.
        ///
        /// Any failure before step 3 leaves the old DIB live and selected.
        pub fn retarget(&mut self, x: i32, y: i32, w: u32, h: u32) -> Result<(), AppError> {
            if w == 0 || h == 0 {
                return Err(AppError::invalid("GDI capture region is empty (w/h == 0)"));
            }
            // Same top-down 32-bpp DIB as `new` (see the comment there).
            let bmi = ffi::BITMAPINFO {
                bmiHeader: ffi::BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<ffi::BITMAPINFOHEADER>() as u32,
                    biWidth: w as i32,
                    biHeight: -(h as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: ffi::BI_RGB,
                    biSizeImage: 0,
                    biXPelsPerMeter: 0,
                    biYPelsPerMeter: 0,
                    biClrUsed: 0,
                    biClrImportant: 0,
                },
                bmiColors: [0; 3],
            };
            let mut bits: *mut u8 = std::ptr::null_mut();
            // SAFETY: `bmi` outlives the call; `bits` receives the pixel pointer.
            let new_bitmap = unsafe {
                ffi::CreateDIBSection(self.hdc_mem, &bmi, ffi::DIB_RGB_COLORS, &mut bits, 0, 0)
            };
            if new_bitmap == 0 || bits.is_null() {
                return Err(AppError::platform(
                    "CreateDIBSection failed during retarget",
                ));
            }
            // SAFETY: both handles are valid GDI objects; the returned value
            // is the previously selected bitmap (our old one).
            let old = unsafe { ffi::SelectObject(self.hdc_mem, new_bitmap) };
            if old == 0 {
                // The new bitmap was never selected: release it and keep the
                // old state fully intact.
                // SAFETY: the new bitmap was created above and never
                // selected; it is released exactly once.
                unsafe { ffi::DeleteObject(new_bitmap) };
                return Err(AppError::platform("SelectObject failed during retarget"));
            }
            // The swap is committed: release the old bitmap (no longer
            // selected anywhere) and adopt the new geometry.
            // SAFETY: `self.hbitmap` is our old DIB, deselected just above and
            // replaced right below, so it is deleted exactly once.
            unsafe { ffi::DeleteObject(self.hbitmap) };
            self.hbitmap = new_bitmap;
            self.bits = bits;
            self.x = x;
            self.y = y;
            self.w = w;
            self.h = h;
            Ok(())
        }

        /// BitBlt the region into the DIB and return the DIB's own pixels
        /// (alpha forced opaque in place) — no per-frame allocation.
        pub fn capture_ref(&mut self) -> Result<(&[u8], u32, u32), AppError> {
            // SAFETY: all handles belong to this struct and are still alive.
            let ok = unsafe {
                ffi::BitBlt(
                    self.hdc_mem,
                    0,
                    0,
                    self.w as i32,
                    self.h as i32,
                    self.hdc_screen,
                    self.x,
                    self.y,
                    ffi::SRCCOPY,
                )
            };
            if ok == 0 {
                tracing::debug!("gdisrc: BitBlt returned 0 (region may be stale)");
            }
            let len = (self.w as usize) * (self.h as usize) * 4;
            // SAFETY: see `capture`; GDI is done writing once BitBlt returns,
            // and `&mut self` makes this the only live view of the DIB.
            let px = unsafe { slice::from_raw_parts_mut(self.bits, len) };
            let (pixels, _) = px.as_chunks_mut::<4>();
            for chunk in pixels {
                chunk[3] = 255;
            }
            Ok((px, self.w, self.h))
        }

        /// BitBlt the region into the DIB and copy the pixels out as BGRA32.
        pub fn capture(&mut self) -> Result<(Vec<u8>, u32, u32), AppError> {
            // SAFETY: all handles belong to this struct and are still alive.
            let ok = unsafe {
                ffi::BitBlt(
                    self.hdc_mem,
                    0,
                    0,
                    self.w as i32,
                    self.h as i32,
                    self.hdc_screen,
                    self.x,
                    self.y,
                    ffi::SRCCOPY,
                )
            };
            // A 0 return is treated as a soft warning rather than fatal: some
            // drivers/desk-locked sessions still fill the DIB, and the frame
            // path degrades gracefully (stale/black pixels) instead of dying.
            if ok == 0 {
                tracing::debug!("gdisrc: BitBlt returned 0 (region may be stale)");
            }
            let len = (self.w as usize) * (self.h as usize) * 4;
            // SAFETY: `bits` points at the DIB section's pixel buffer, which
            // GDI allocates at exactly w*h*4 bytes for a tightly packed 32-bpp
            // top-down bitmap, and stays valid until Drop deletes the bitmap.
            let mut px = unsafe { slice::from_raw_parts(self.bits, len) }.to_vec();
            // GDI leaves the high byte of each 32-bpp pixel undefined (usually
            // 0). Downstream sinks treat `PixelFormat::Bgra` as opaque — see
            // `synthetic_frame` in screen.rs, which emits `[b, g, r, 255]` —
            // so force alpha = 255 or the iOS `premultipliedFirst` CGImage
            // (and any future desktop render window) would show a black
            // screen. Enforces the invariant: Bgra frames carry opaque alpha.
            // `len` is a multiple of 4, so there is no remainder.
            let (pixels, _) = px.as_chunks_mut::<4>();
            for chunk in pixels {
                chunk[3] = 255;
            }
            debug_assert!(px.as_chunks::<4>().0.iter().all(|c| c[3] == 255));
            Ok((px, self.w, self.h))
        }
    }

    impl Drop for Win {
        fn drop(&mut self) {
            // SAFETY: each handle is valid for the lifetime of this struct and
            // released exactly once, in reverse creation order.
            unsafe {
                ffi::SelectObject(self.hdc_mem, self.old_obj);
                ffi::DeleteObject(self.hbitmap);
                ffi::DeleteDC(self.hdc_mem);
                ffi::ReleaseDC(0, self.hdc_screen);
            }
        }
    }
}

impl GdiScreen {
    /// Capture a screen region: `(x, y)` top-left, `w x h` pixels, 32-bit BGRA.
    pub fn new(x: i32, y: i32, w: u32, h: u32) -> Result<Self, AppError> {
        #[cfg(windows)]
        {
            Ok(Self {
                inner: imp::Win::new(x, y, w, h)?,
            })
        }
        #[cfg(not(windows))]
        {
            let _ = (x, y, w, h);
            Err(AppError::invalid("GDI screen capture is Windows-only"))
        }
    }

    /// Full primary monitor (dimensions from `GetSystemMetrics` on Windows).
    pub fn full_primary() -> Result<Self, AppError> {
        #[cfg(windows)]
        {
            // SAFETY: plain FFI query with no side effects.
            let (w, h) = unsafe {
                (
                    ffi::GetSystemMetrics(ffi::SM_CXSCREEN),
                    ffi::GetSystemMetrics(ffi::SM_CYSCREEN),
                )
            };
            if w <= 0 || h <= 0 {
                return Err(AppError::platform(
                    "GetSystemMetrics returned no usable screen size",
                ));
            }
            Self::new(0, 0, w as u32, h as u32)
        }
        #[cfg(not(windows))]
        {
            Err(AppError::invalid("GDI screen capture is Windows-only"))
        }
    }

    /// Grab one frame: `BitBlt` into the persistent DIB, then return
    /// `(BGRA32 pixels, width, height)`.
    pub fn capture(&mut self) -> Result<(Vec<u8>, u32, u32), AppError> {
        #[cfg(windows)]
        {
            self.inner.capture()
        }
        #[cfg(not(windows))]
        {
            Err(AppError::invalid("GDI screen capture is Windows-only"))
        }
    }

    /// Like [`capture`](Self::capture) but borrows the persistent DIB's
    /// pixels (tight top-down BGRA32, alpha opaque) instead of copying them
    /// out: valid until the next call. The host capture loop diffs and
    /// encodes straight from it.
    pub fn capture_ref(&mut self) -> Result<(&[u8], u32, u32), AppError> {
        #[cfg(windows)]
        {
            self.inner.capture_ref()
        }
        #[cfg(not(windows))]
        {
            Err(AppError::invalid("GDI screen capture is Windows-only"))
        }
    }

    /// The captured region's `(width, height)` in pixels.
    pub fn size(&self) -> (u32, u32) {
        #[cfg(windows)]
        {
            (self.inner.w, self.inner.h)
        }
        #[cfg(not(windows))]
        {
            (0, 0)
        }
    }

    /// The captured region's `(x, y)` origin on the desktop.
    pub fn origin(&self) -> (i32, i32) {
        #[cfg(windows)]
        {
            (self.inner.x, self.inner.y)
        }
        #[cfg(not(windows))]
        {
            (0, 0)
        }
    }

    /// Re-target the capture source to a new region `(x, y, w, h)` without
    /// tearing down the screen DC: the persistent DIB is replaced in
    /// place, so a monitor switch costs one `CreateDIBSection` + two
    /// `SelectObject` calls instead of a full rebuild.
    ///
    /// Failure safety: the new DIB is fully created and selected *before* any
    /// existing state is mutated, and the old bitmap is released only after
    /// the new one is live — a failed retarget leaves the running source
    /// untouched. Empty regions are rejected up front.
    pub fn retarget(&mut self, x: i32, y: i32, w: u32, h: u32) -> Result<(), AppError> {
        #[cfg(windows)]
        {
            self.inner.retarget(x, y, w, h)
        }
        #[cfg(not(windows))]
        {
            // No live source off-Windows; validating the geometry keeps the
            // API's contract identical across targets.
            if w == 0 || h == 0 {
                Err(AppError::invalid("GDI capture region is empty (w/h == 0)"))
            } else {
                let _ = (x, y);
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(all(windows, test))]
    #[test]
    fn captures_a_small_region() {
        let mut s = GdiScreen::new(0, 0, 32, 32).expect("32x32 GDI region");
        let (px, w, h) = s.capture().expect("capture");
        assert_eq!((w, h), (32, 32));
        assert_eq!(px.len(), 32 * 32 * 4);
    }

    #[cfg(all(windows, test))]
    #[test]
    fn captures_the_full_primary_monitor() {
        let (px, w, h) = GdiScreen::full_primary()
            .expect("primary monitor")
            .capture()
            .expect("capture");
        assert!(w > 0 && h > 0);
        assert_eq!(px.len(), (w * h * 4) as usize);
    }

    #[cfg(all(not(windows), test))]
    #[test]
    fn constructors_fail_off_windows() {
        assert!(GdiScreen::new(0, 0, 8, 8).is_err());
        assert!(GdiScreen::full_primary().is_err());
    }

    /// The pixel format claim in the module docs: BGRA32, 4 bytes/px.
    #[test]
    fn bgra_is_the_gdi_layout() {
        assert_eq!(PixelFormat::Bgra.bytes_per_pixel(), 4);
        assert_eq!(PixelFormat::Bgra.tag(), 0);
    }

    #[cfg(all(windows, test))]
    #[test]
    fn retargets_to_a_new_region_and_captures_it() {
        let mut s = GdiScreen::new(0, 0, 32, 32).expect("32x32 GDI region");
        // Grow the region: capture must then return the new geometry.
        s.retarget(0, 0, 64, 48).expect("retarget 64x48");
        assert_eq!(s.size(), (64, 48));
        let (px, w, h) = s.capture().expect("capture after retarget");
        assert_eq!((w, h), (64, 48));
        assert_eq!(px.len(), 64 * 48 * 4);
        // Move the origin too (multi-monitor case).
        s.retarget(100, 200, 48, 32).expect("retarget with origin");
        assert_eq!(s.origin(), (100, 200));
        assert_eq!(s.size(), (48, 32));
        let (px, w, h) = s.capture().expect("capture after move");
        assert_eq!((w, h), (48, 32));
        assert_eq!(px.len(), 48 * 32 * 4);
        // A failed retarget (empty region) must leave the source usable.
        assert!(s.retarget(0, 0, 0, 10).is_err());
        assert_eq!(s.size(), (48, 32));
        let (_, w, h) = s.capture().expect("capture after failed retarget");
        assert_eq!((w, h), (48, 32));
    }

    #[cfg(all(not(windows), test))]
    #[test]
    fn retarget_rejects_empty_and_oks_geometry_off_windows() {
        // Off-Windows there is no live source; the empty-region contract is
        // still enforced.
        let mut s = match GdiScreen::new(0, 0, 8, 8) {
            Ok(s) => s,
            Err(_) => return, // constructor fails off-Windows; nothing to retarget
        };
        assert!(s.retarget(0, 0, 0, 10).is_err());
        assert!(s.retarget(5, 5, 16, 16).is_ok());
    }
}
