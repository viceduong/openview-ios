//! The desktop render window (spec v0.12 "desktop render window").
//!
//! Like [`ovapp::gdisrc`] and [`ovhost::input`], this module declares the
//! handful of Win32 functions and structs it needs with raw
//! `extern "system"` FFI — no `windows`/`winapi` crates, no GUI framework.
//!
//! [`RenderWindow`] is a plain top-level window that:
//!
//! - paints the latest [`Frame`] with `StretchDIBits` into the client area,
//!   letterboxed by aspect ratio (`WM_PAINT`);
//! - queues local mouse/keyboard events as [`LocalEvent`]s for the viewer
//!   loop to turn into POINTER/KEY commands (`WM_MOUSEMOVE`,
//!   `WM_LBUTTONDOWN`/`UP`, `WM_KEYDOWN`/`UP`);
//! - sets a quit flag on `WM_CLOSE` so the loop can tear down cleanly.
//!
//! Window state lives in a heap `Arc<WindowState>` parked in the window's
//! `GWLP_USERDATA` slot so the static `WndProc` can reach it.
//!
//! On non-Windows targets every method is a no-op stub so the crate — and the
//! viewer loop in `main.rs` — compiles unchanged; the pure normalization math
//! it relies on lives in [`crate::localinput`] and is tested everywhere.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[cfg(windows)]
use std::sync::atomic::AtomicIsize;

use crate::localinput::{letterbox_rect, LocalEvent, Rect};
use ovapp::screen::Frame;

#[cfg(windows)]
mod d2d;

/// Which presenter the window uses (Section 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RendererPref {
    /// Direct2D high-quality cubic + flip-model vsync; GDI on failure.
    Direct2D,
    /// GDI `StretchDIBits`, with the CPU area/sharpen scaler
    /// ([`ovapp::scale`]: area + sharpen 0.5) for downscaling.
    Gdi,
}

static RENDERER: Mutex<RendererPref> = Mutex::new(RendererPref::Direct2D);

/// Choose the presenter for windows created afterwards.
pub fn set_renderer(p: RendererPref) {
    if let Ok(mut r) = RENDERER.lock() {
        *r = p;
    }
}

/// A remote-cursor bitmap (`CURSOR_SHAPE` payload): 32-bit BGRA, premultiplied
/// alpha, with the hotspot — the pixel under the logical cursor tip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorBmp {
    pub hotspot_x: u16,
    pub hotspot_y: u16,
    pub w: u16,
    pub h: u16,
    pub bgra: Vec<u8>,
}

/// The remote cursor overlay state, updated from `CURSOR`/`CURSOR_SHAPE`
/// commands. Positions are in *frame pixel space* (the captured region), so
/// they composite straight onto the frame buffer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct CursorOverlay {
    pos: Option<(i32, i32)>,
    shape: Option<CursorBmp>,
}

/// Blend a premultiplied-BGRA cursor bitmap over a BGRA frame buffer at the
/// hotspot-anchored position `x, y` (top-left of the cursor bitmap is
/// `(x - hotspot_x, y - hotspot_y)`). Pixels outside the frame are clipped.
///
/// Pure and target-independent: unit-tested on all platforms.
fn blend_cursor(dst: &mut [u8], stride: usize, fw: u32, fh: u32, cur: &CursorBmp, x: i32, y: i32) {
    let (cw, ch) = (i32::from(cur.w), i32::from(cur.h));
    let (fw, fh) = (fw as i32, fh as i32);
    if cw == 0 || ch == 0 {
        return;
    }
    // Top-left corner of the bitmap in frame coordinates.
    let (ox, oy) = (x - i32::from(cur.hotspot_x), y - i32::from(cur.hotspot_y));
    for cy in 0..ch {
        let fy = oy + cy;
        if fy < 0 || fy >= fh {
            continue;
        }
        for cx in 0..cw {
            let fx = ox + cx;
            if fx < 0 || fx >= fw {
                continue;
            }
            let s = ((cy * cw + cx) * 4) as usize;
            if s + 4 > cur.bgra.len() {
                return; // malformed bitmap: stop rather than read out of bounds
            }
            let (sb, sg, sr, sa) = (
                cur.bgra[s],
                cur.bgra[s + 1],
                cur.bgra[s + 2],
                cur.bgra[s + 3],
            );
            let inv = 255 - i32::from(sa);
            let d = (fy as usize * stride) + fx as usize * 4;
            // Premultiplied alpha OVER: out = src + dst * (1 - a) / 255.
            dst[d] = sb + ((i32::from(dst[d]) * inv) / 255) as u8;
            dst[d + 1] = sg + ((i32::from(dst[d + 1]) * inv) / 255) as u8;
            dst[d + 2] = sr + ((i32::from(dst[d + 2]) * inv) / 255) as u8;
            dst[d + 3] = 255;
        }
    }
}

/// Copy the frame pixels under `cur` placed at `(x, y)` (hotspot-anchored,
/// clipped like [`blend_cursor`]) from `src` back into `dst` — undoes a
/// previous blend without touching the rest of the buffer.
#[allow(clippy::too_many_arguments)]
fn restore_cursor_rect(
    dst: &mut [u8],
    src: &[u8],
    stride: usize,
    fw: u32,
    fh: u32,
    cur: &CursorBmp,
    x: i32,
    y: i32,
) {
    let (ox, oy) = (x - i32::from(cur.hotspot_x), y - i32::from(cur.hotspot_y));
    let x0 = ox.clamp(0, fw as i32) as usize;
    let x1 = (ox + i32::from(cur.w)).clamp(0, fw as i32) as usize;
    let y0 = oy.clamp(0, fh as i32) as usize;
    let y1 = (oy + i32::from(cur.h)).clamp(0, fh as i32) as usize;
    if x0 >= x1 {
        return;
    }
    for row in y0..y1 {
        let a = row * stride + x0 * 4;
        let b = row * stride + x1 * 4;
        if b > dst.len() || b > src.len() {
            return;
        }
        dst[a..b].copy_from_slice(&src[a..b]);
    }
}

/// Composite cache for the paint path: `(frame seq, overlay, buffer)`.
type Display = Option<(u64, CursorOverlay, Vec<u8>)>;

/// Return the frame with the cursor overlay blended in, reusing the cached
/// buffer: unchanged -> reuse as is; same frame, moved/changed cursor ->
/// restore the old cursor rect and blend the new one; new frame -> copy into
/// the existing allocation and blend.
#[cfg_attr(not(windows), allow(dead_code))]
fn compose<'a>(disp: &'a mut Display, frame: &Frame, overlay: CursorOverlay) -> &'a [u8] {
    let stride = frame.stride as usize;
    let prev = disp.take();
    let mut buf = match prev {
        Some((seq, old, buf)) if seq == frame.seq && old == overlay => {
            return &disp.insert((seq, old, buf)).2;
        }
        Some((seq, old, mut buf)) if seq == frame.seq && buf.len() == frame.pixels.len() => {
            if let (Some((ox, oy)), Some(oc)) = (old.pos, old.shape.as_ref()) {
                restore_cursor_rect(
                    &mut buf,
                    &frame.pixels,
                    stride,
                    frame.width,
                    frame.height,
                    oc,
                    ox,
                    oy,
                );
            }
            buf
        }
        other => {
            let mut buf = other.map(|(_, _, b)| b).unwrap_or_default();
            buf.clear();
            buf.extend_from_slice(&frame.pixels);
            buf
        }
    };
    if let (Some((x, y)), Some(cur)) = (overlay.pos, overlay.shape.as_ref()) {
        blend_cursor(&mut buf, stride, frame.width, frame.height, cur, x, y);
    }
    &disp.insert((frame.seq, overlay, buf)).2
}

/// The HUD overlay state (v0.16): the four health lines plus the dead-peer
/// banner. Shared between the viewer loop (which updates it) and `WM_PAINT`
/// (which draws it).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct HudState {
    lines: [String; 6],
    /// Peer considered dead (no inbound for `DEAD_AFTER`): show the banner
    /// and dim the frame.
    dead: bool,
    /// Panel visibility (toggled with H / F3).
    visible: bool,
}

/// Title of the render window.
#[cfg(windows)]
const TITLE: &str = "OpenView — Remote";

/// The GDI path's CPU-scaled picture: scaler, key, pixels.
#[cfg(windows)]
type ScaledCache = (
    ovapp::scale::Scaler,
    (u64, CursorOverlay, i32, i32),
    Vec<u8>,
);

/// Mutable state shared between the `WndProc` and the public methods.
#[cfg(windows)]
struct WindowState {
    /// The window handle, parked after `CreateWindowExW` returns (the `WndProc`
    /// sees a null handle slot only for messages dispatched during creation,
    /// which it handles fine).
    hwnd: AtomicIsize,
    /// The most recently presented frame (moved in by
    /// [`RenderWindow::present_owned`]).
    last_frame: Mutex<Option<Frame>>,
    /// Remote cursor overlay state (`CURSOR`/`CURSOR_SHAPE` commands).
    cursor: Mutex<CursorOverlay>,
    /// The composited (frame + cursor) pixel buffer last used for painting,
    /// plus the `(frame seq, overlay)` key it was built from. Recomputed only
    /// when the frame or the cursor changes, so a cursor move costs one frame
    /// copy + blend, not a repaint of stale composites.
    display: Mutex<Option<(u64, CursorOverlay, Vec<u8>)>>,
    /// The letterboxed content rect of `last_frame` inside the client area.
    content_rect: Mutex<Rect>,
    /// Local input events queued by the `WndProc`, drained by `take_input`.
    events: Mutex<Vec<LocalEvent>>,
    /// HUD overlay content (v0.16), updated by the viewer loop.
    hud: Mutex<HudState>,
    /// Set on `WM_CLOSE` (or by [`RenderWindow::close`]).
    quit: AtomicBool,
    /// Section 4: the Direct2D presenter (`None` = GDI path).
    d2d: Mutex<Option<d2d::D2dRenderer>>,
    /// A vsync-refused present is waiting to be retried.
    repaint_pending: AtomicBool,
    /// GDI path: the CPU-scaled (area + sharpen) picture and its
    /// key `(frame seq, overlay, dst w, dst h)`.
    scaled: Mutex<Option<ScaledCache>>,
}

#[cfg(windows)]
impl WindowState {
    fn new() -> Self {
        Self {
            hwnd: AtomicIsize::new(0),
            last_frame: Mutex::new(None),
            cursor: Mutex::new(CursorOverlay::default()),
            display: Mutex::new(None),
            content_rect: Mutex::new(Rect {
                x: 0,
                y: 0,
                w: 1,
                h: 1,
            }),
            events: Mutex::new(Vec::new()),
            hud: Mutex::new(HudState::default()),
            quit: AtomicBool::new(false),
            d2d: Mutex::new(None),
            repaint_pending: AtomicBool::new(false),
            scaled: Mutex::new(None),
        }
    }
}

/// A render window. On Windows a real `HWND`-backed top-level window; on
/// other platforms a no-op shell so the viewer loop still runs headless.
pub struct RenderWindow {
    #[cfg(windows)]
    state: Arc<WindowState>,
    /// Keeps the non-Windows shape honest (no fields needed there).
    #[cfg(not(windows))]
    _private: (),
}

impl RenderWindow {
    /// Create and show the window. On non-Windows this is a no-op that always
    /// succeeds (the viewer loop then runs headless).
    pub fn new(title: &str) -> Result<Self, String> {
        #[cfg(windows)]
        {
            let _ = title; // the visible title is the fixed TITLE below
            ffi::create_window(TITLE)
                .map(|state| Self { state })
                .map_err(|e| format!("window creation failed: {e}"))
        }
        #[cfg(not(windows))]
        {
            let _ = title;
            Ok(Self { _private: () })
        }
    }

    /// Present a new frame (copying it). Prefer
    /// [`present_owned`](Self::present_owned) on the hot path. No-op
    /// off-Windows.
    pub fn present(&self, frame: &Frame) {
        let _ = self.present_owned(frame.clone());
    }

    /// Present a new frame by value (no pixel copy): compute the letterbox
    /// rect for its aspect inside the current client area, store the frame,
    /// and schedule a repaint. Returns the previously presented frame so the
    /// caller can recycle its buffer. Off-Windows the frame is handed back.
    pub fn present_owned(&self, frame: Frame) -> Option<Frame> {
        #[cfg(windows)]
        {
            let (cw, ch) = self.state.client_size();
            let rect = letterbox_rect(cw, ch, frame.width, frame.height);
            if let Ok(mut slot) = self.state.content_rect.lock() {
                *slot = rect;
            }
            let old = match self.state.last_frame.lock() {
                Ok(mut slot) => slot.replace(frame),
                Err(_) => Some(frame),
            };
            self.state.invalidate();
            old
        }
        #[cfg(not(windows))]
        {
            Some(frame)
        }
    }

    /// Update the remote cursor position (frame pixel space) and repaint.
    /// No-op off-Windows.
    pub fn set_cursor(&self, x: i32, y: i32) {
        #[cfg(windows)]
        {
            if let Ok(mut c) = self.state.cursor.lock() {
                c.pos = Some((x, y));
                drop(c);
                self.state.invalidate();
            }
        }
        #[cfg(not(windows))]
        {
            let _ = (x, y);
        }
    }

    /// Set the remote cursor bitmap (a `CURSOR_SHAPE` payload) and repaint.
    /// No-op off-Windows.
    pub fn set_cursor_shape(&self, shape: CursorBmp) {
        #[cfg(windows)]
        {
            if let Ok(mut c) = self.state.cursor.lock() {
                c.shape = Some(shape);
                drop(c);
                self.state.invalidate();
            }
        }
        #[cfg(not(windows))]
        {
            let _ = shape;
        }
    }

    /// Update the HUD overlay content and repaint (the overlay is drawn on
    /// the next paint). The dead banner always shows; the health panel only
    /// when `visible`. No-op off-Windows.
    pub fn set_hud(&self, lines: [String; 6], dead: bool, visible: bool) {
        #[cfg(windows)]
        {
            if let Ok(mut h) = self.state.hud.lock() {
                if h.lines != lines || h.dead != dead || h.visible != visible {
                    *h = HudState {
                        lines,
                        dead,
                        visible,
                    };
                    drop(h);
                    self.state.invalidate();
                }
            }
        }
        #[cfg(not(windows))]
        {
            let _ = (lines, dead, visible);
        }
    }

    /// Drain pending window messages (non-blocking `PeekMessageW` loop).
    /// No-op off-Windows.
    pub fn pump_messages(&self) {
        #[cfg(windows)]
        self.state.pump_messages();
    }

    /// Drain the local input queue collected by the `WndProc`.
    pub fn take_input(&self) -> Vec<LocalEvent> {
        #[cfg(windows)]
        {
            self.state
                .events
                .lock()
                .map(|mut q| std::mem::take(&mut *q))
                .unwrap_or_default()
        }
        #[cfg(not(windows))]
        {
            Vec::new()
        }
    }

    /// True once the user closed the window (or [`RenderWindow::close`]).
    pub fn should_quit(&self) -> bool {
        #[cfg(windows)]
        {
            self.state.quit.load(Ordering::Relaxed)
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    /// A vsync-deferred repaint is pending: the loop should come back within
    /// a few milliseconds (`pump_messages` re-issues it).
    pub fn repaint_pending(&self) -> bool {
        #[cfg(windows)]
        {
            self.state.repaint_pending.load(Ordering::Relaxed)
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    /// The active presenter, for logs.
    pub fn renderer_name(&self) -> String {
        #[cfg(windows)]
        {
            match self
                .state
                .d2d
                .lock()
                .ok()
                .and_then(|g| g.as_ref().map(|r| r.name.clone()))
            {
                Some(n) => n,
                None => "GDI StretchDIBits + CPU scaler (area + sharpen 0.5)".into(),
            }
        }
        #[cfg(not(windows))]
        {
            "none".into()
        }
    }

    /// Ask the loop to quit (also invoked by `WM_CLOSE`).
    pub fn close(&self) {
        #[cfg(windows)]
        self.state.quit.store(true, Ordering::Relaxed);
    }

    /// The letterboxed content rect currently stored (client coordinates).
    /// The viewer loop reads this to normalize pointer events. Identity rect
    /// off-Windows.
    pub fn content_rect(&self) -> Rect {
        #[cfg(windows)]
        {
            self.state.content_rect.lock().map(|r| *r).unwrap_or(Rect {
                x: 0,
                y: 0,
                w: 1,
                h: 1,
            })
        }
        #[cfg(not(windows))]
        {
            Rect {
                x: 0,
                y: 0,
                w: 1,
                h: 1,
            }
        }
    }
}

// --------------------------------------------------------------------------------
// Windows implementation: raw Win32 FFI in the house style of ovhost::input /
// ovapp::gdisrc — no `windows`/`winapi` crates.
// --------------------------------------------------------------------------------

#[cfg(windows)]
mod ffi {
    //! Raw Win32 imports (user32 + gdi32). The `#[link]` attributes name the
    //! import libraries explicitly; the MSVC toolchain links them implicitly.

    #![allow(non_snake_case)]
    // Win32 type names are conventionally ALL_CAPS; keep them as-is.
    #![allow(clippy::upper_case_acronyms)]

    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    use super::{HudState, WindowState};
    use crate::localinput::LocalEvent;
    use ovapp::terminal::{MouseButton, PointerKind};

    pub const WM_CLOSE: u32 = 0x0010;
    pub const WM_PAINT: u32 = 0x000F;
    pub const WM_ERASEBKGND: u32 = 0x0014;
    pub const WM_KEYDOWN: u32 = 0x0100;
    pub const WM_KEYUP: u32 = 0x0101;
    pub const WM_MOUSEMOVE: u32 = 0x0200;
    pub const WM_LBUTTONDOWN: u32 = 0x0201;
    pub const WM_LBUTTONUP: u32 = 0x0202;
    pub const WM_RBUTTONDOWN: u32 = 0x0204;
    pub const WM_RBUTTONUP: u32 = 0x0205;
    pub const WM_MBUTTONDOWN: u32 = 0x0207;
    pub const WM_MBUTTONUP: u32 = 0x0208;
    pub const WM_MOUSEWHEEL: u32 = 0x020A;
    pub const WM_XBUTTONDOWN: u32 = 0x020B;
    pub const WM_XBUTTONUP: u32 = 0x020C;
    pub const WM_MOUSEHWHEEL: u32 = 0x020E;
    pub const WM_SYSKEYDOWN: u32 = 0x0104;
    pub const WM_SYSKEYUP: u32 = 0x0105;
    pub const WM_QUIT: u32 = 0x0012;
    pub const SW_SHOW: i32 = 5;
    pub const DIB_RGB_COLORS: u32 = 0;
    pub const BI_RGB: u32 = 0;
    pub const CS_HREDRAW: u32 = 0x0002;
    pub const CS_VREDRAW: u32 = 0x0001;
    pub const CS_DBLCLKS: u32 = 0x0008;
    pub const WS_OVERLAPPEDWINDOW: u32 = 0x00CF_0000;
    pub const GWLP_USERDATA: i32 = -21;
    pub const CW_USEDEFAULT: i32 = 0x8000_0000u32 as i32;
    pub const IDC_ARROW: *const u16 = 32512 as *const u16;
    pub const PM_REMOVE: u32 = 1;

    #[link(name = "kernel32")]
    extern "system" {
        pub fn GetLastError() -> u32;
    }

    #[link(name = "user32")]
    extern "system" {
        pub fn RegisterClassExW(lpwcx: *const WNDCLASSEXW) -> u16;
        pub fn CreateWindowExW(
            dwExStyle: u32,
            lpClassName: *const u16,
            lpWindowName: *const u16,
            dwStyle: u32,
            x: i32,
            y: i32,
            nWidth: i32,
            nHeight: i32,
            hWndParent: HWND,
            hMenu: isize,
            hInstance: isize,
            lpParam: *const core::ffi::c_void,
        ) -> HWND;
        pub fn DefWindowProcW(hWnd: HWND, msg: u32, wparam: usize, lparam: isize) -> isize;
        pub fn ShowWindow(hWnd: HWND, nCmdShow: i32) -> i32;
        pub fn UpdateWindow(hWnd: HWND) -> i32;
        pub fn InvalidateRect(hWnd: HWND, lpRect: *const RECT, bErase: i32) -> i32;
        pub fn GetClientRect(hWnd: HWND, lpRect: *mut RECT) -> i32;
        pub fn PostQuitMessage(nExitCode: i32);
        pub fn SetCapture(hWnd: HWND) -> HWND;
        pub fn ReleaseCapture() -> i32;
        pub fn PeekMessageW(lpMsg: *mut MSG, hWnd: HWND, min: u32, max: u32, remove: u32) -> i32;
        pub fn TranslateMessage(lpMsg: *const MSG) -> i32;
        pub fn DispatchMessageW(lpMsg: *const MSG) -> isize;
        pub fn SetWindowLongPtrW(hWnd: HWND, nIndex: i32, dwNewLong: isize) -> isize;
        pub fn GetWindowLongPtrW(hWnd: HWND, nIndex: i32) -> isize;
        pub fn BeginPaint(hWnd: HWND, lpPaint: *mut PAINTSTRUCT) -> HDC;
        pub fn EndPaint(hWnd: HWND, lpPaint: *const PAINTSTRUCT) -> i32;
        pub fn TextOutW(hdc: HDC, x: i32, y: i32, lpString: *const u16, c: i32) -> i32;
        pub fn SetTextColor(hdc: HDC, color: u32) -> u32;
        pub fn SetBkMode(hdc: HDC, mode: i32) -> i32;
        pub fn GetTextExtentPoint32W(
            hdc: HDC,
            lpString: *const u16,
            c: i32,
            lpSize: *mut SIZE,
        ) -> i32;
        pub fn GetModuleHandleW(lpModuleName: *const u16) -> isize;
        pub fn LoadCursorW(hInstance: isize, lpCursorName: *const u16) -> isize;
        pub fn LoadIconW(hInstance: isize, lpIconName: *const u16) -> isize;
    }

    #[link(name = "gdi32")]
    extern "system" {
        pub fn SetStretchBltMode(hdc: HDC, mode: i32) -> i32;
        pub fn SetBrushOrgEx(hdc: HDC, x: i32, y: i32, lppt: *mut core::ffi::c_void) -> i32;
        pub fn StretchDIBits(
            hdc: HDC,
            xDest: i32,
            yDest: i32,
            destW: i32,
            destH: i32,
            xSrc: i32,
            ySrc: i32,
            srcW: i32,
            srcH: i32,
            lpBits: *const core::ffi::c_void,
            lpbmi: *const BITMAPINFO,
            iUsage: u32,
            dwRop: u32,
        ) -> i32;
    }

    pub type HWND = isize;
    pub type HDC = isize;
    /// `SetStretchBltMode` modes (wingdi.h).
    pub const COLORONCOLOR: i32 = 3;
    pub const HALFTONE: i32 = 4;

    /// `RECT` (windef.h).
    #[derive(Clone, Copy, Default)]
    #[repr(C)]
    pub struct RECT {
        pub left: i32,
        pub top: i32,
        pub right: i32,
        pub bottom: i32,
    }

    /// `POINT` (windef.h).
    #[derive(Clone, Copy, Default)]
    #[repr(C)]
    pub struct POINT {
        pub x: i32,
        pub y: i32,
    }

    /// `SIZE` (windef.h) — text extents.
    #[derive(Clone, Copy, Default)]
    #[repr(C)]
    pub struct SIZE {
        pub cx: i32,
        pub cy: i32,
    }

    /// `PAINTSTRUCT` (winuser.h).
    #[repr(C)]
    pub struct PAINTSTRUCT {
        pub hdc: HDC,
        pub fErase: i32,
        pub rcPaint: RECT,
        pub fRestore: i32,
        pub fIncUpdate: i32,
        pub rgbReserved: [u8; 32],
    }

    /// `MSG` (winuser.h).
    #[repr(C)]
    pub struct MSG {
        pub hwnd: HWND,
        pub message: u32,
        pub wParam: usize,
        pub lParam: isize,
        pub time: u32,
        pub pt: POINT,
    }

    /// `WNDPROC` signature.
    pub type WndProc =
        unsafe extern "system" fn(hwnd: HWND, msg: u32, wparam: usize, lparam: isize) -> isize;

    /// `WNDCLASSEXW` (winuser.h).
    #[repr(C)]
    pub struct WNDCLASSEXW {
        pub cbSize: u32,
        pub style: u32,
        pub lpfnWndProc: WndProc,
        pub cbClsExtra: i32,
        pub cbWndExtra: i32,
        pub hInstance: isize,
        pub hIcon: isize,
        pub hCursor: isize,
        pub hbrBackground: isize,
        pub lpszMenuName: *const u16,
        pub lpszClassName: *const u16,
        pub hIconSm: isize,
    }

    /// `BITMAPINFOHEADER` (wingdi.h). `biHeight` is negative for top-down
    /// DIBs — our BGRA frame buffers are top-down, so present uses `-height`.
    #[repr(C)]
    pub struct BITMAPINFOHEADER {
        pub biSize: u32,
        pub biWidth: i32,
        pub biHeight: i32,
        pub biPlanes: u16,
        pub biBitCount: u16,
        pub biCompression: u32,
        pub biSizeImage: u32,
        pub biXPelsPerMeter: u32,
        pub biYPelsPerMeter: u32,
        pub biClrUsed: u32,
        pub biClrImportant: u32,
    }

    /// `BITMAPINFO` with a one-entry color table (`DIB_RGB_COLORS` reads no
    /// palette entries for a 32-bpp `BI_RGB` DIB).
    #[repr(C)]
    pub struct BITMAPINFO {
        pub bmiHeader: BITMAPINFOHEADER,
        pub bmiColors: [u32; 1],
    }

    /// UTF-16 wide string with a NUL terminator.
    pub fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// `LOWORD`/`HIWORD` of a message `lParam` (signed client coordinates:
    /// negative values only when capture drags past an edge).
    pub fn loword(l: isize) -> i32 {
        (l as u32 & 0xFFFF) as u16 as i16 as i32
    }
    pub fn hiword(l: isize) -> i32 {
        ((l as u32) >> 16) as u16 as i16 as i32
    }

    /// Resource id of the app icon (embedded by build.rs via `assets/winres.rs`).
    pub const APP_ICON_ID: u16 = 1;

    /// The window class name, NUL-terminated UTF-16 ("OVVIEW_RENDERER").
    pub const CLASS_NAME: &[u16] = &[
        0x4F, 0x56, 0x56, 0x49, 0x45, 0x57, 0x5F, 0x52, 0x45, 0x4E, 0x44, 0x45, 0x52, 0x45, 0x52, 0,
    ];

    /// The static `WndProc`: paint, queue input, quit on close.
    ///
    /// SAFETY: Win32 calls this with a valid `hwnd`. The `GWLP_USERDATA`
    /// pointer is installed by `create_window` immediately after
    /// `CreateWindowExW` returns; messages dispatched *during* creation
    /// (`WM_CREATE`, `WM_NCCREATE`, ...) see a null slot and fall through to
    /// `DefWindowProcW`, which is the documented-correct behavior for them.
    pub unsafe extern "system" fn wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: usize,
        lparam: isize,
    ) -> isize {
        // Reconstitute the Arc from the raw pointer without touching the
        // refcount: `from_raw` claims the window's refcount and `forget`
        // hands it straight back on every path.
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WindowState;
        let state = if ptr.is_null() {
            None
        } else {
            Some(Arc::from_raw(ptr))
        };
        let result = dispatch(hwnd, msg, wparam, lparam, state.as_deref());
        std::mem::forget(state);
        result
    }

    /// The actual message handling, split out so the refcount dance above
    /// stays in one place. Returns the value `WndProc` should return.
    unsafe fn dispatch(
        hwnd: HWND,
        msg: u32,
        wparam: usize,
        lparam: isize,
        state: Option<&WindowState>,
    ) -> isize {
        match msg {
            WM_PAINT => {
                let mut ps: PAINTSTRUCT = std::mem::zeroed();
                // SAFETY: `ps` is a valid PAINTSTRUCT for this hwnd; BeginPaint
                // and EndPaint are paired on the same hwnd as required.
                let hdc = BeginPaint(hwnd, &mut ps);
                if hdc != 0 {
                    if let Some(s) = state {
                        // SAFETY: `hdc` is a live DC from BeginPaint; the
                        // frame buffer outlives the call (see `paint`).
                        unsafe { paint(hdc, s) };
                    }
                    // SAFETY: paired with BeginPaint above.
                    EndPaint(hwnd, &ps);
                }
                0
            }
            WM_ERASEBKGND => {
                // StretchDIBits covers the whole content rect every paint;
                // skipping the erase avoids flicker.
                1
            }
            // WM_SYSKEY* too: Alt combinations (Alt+Tab inside the remote,
            // Alt+F4 of a remote app, F10) go to the host.
            WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP => {
                if let Some(s) = state {
                    if let Ok(mut q) = s.events.lock() {
                        q.push(LocalEvent::Key {
                            vk: wparam as u16,
                            scan: ((lparam as usize >> 16) & 0xFF) as u16,
                            extended: (lparam as usize >> 24) & 1 != 0,
                            down: msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN,
                        });
                    }
                }
                0
            }
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                if let Some(s) = state {
                    // HIWORD(wParam): signed delta in WHEEL_DELTA units.
                    let delta = ((wparam >> 16) & 0xFFFF) as u16 as i16;
                    if let Ok(mut q) = s.events.lock() {
                        q.push(if msg == WM_MOUSEWHEEL {
                            LocalEvent::Wheel { dy: delta, dx: 0 }
                        } else {
                            LocalEvent::Wheel { dy: 0, dx: delta }
                        });
                    }
                }
                0
            }
            WM_MOUSEMOVE | WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP
            | WM_MBUTTONDOWN | WM_MBUTTONUP | WM_XBUTTONDOWN | WM_XBUTTONUP => {
                if let Some(s) = state {
                    let (kind, button) = match msg {
                        WM_LBUTTONDOWN => (PointerKind::Down, MouseButton::Left),
                        WM_LBUTTONUP => (PointerKind::Up, MouseButton::Left),
                        WM_RBUTTONDOWN => (PointerKind::Down, MouseButton::Right),
                        WM_RBUTTONUP => (PointerKind::Up, MouseButton::Right),
                        WM_MBUTTONDOWN => (PointerKind::Down, MouseButton::Middle),
                        WM_MBUTTONUP => (PointerKind::Up, MouseButton::Middle),
                        WM_XBUTTONDOWN | WM_XBUTTONUP => {
                            let b = if (wparam >> 16) & 0xFFFF == 2 {
                                MouseButton::X2
                            } else {
                                MouseButton::X1
                            };
                            let k = if msg == WM_XBUTTONDOWN {
                                PointerKind::Down
                            } else {
                                PointerKind::Up
                            };
                            (k, b)
                        }
                        _ => (PointerKind::Move, MouseButton::Left),
                    };
                    // Capture the mouse while a button is held, so the UP
                    // arrives even when it happens outside the window.
                    // MK_LBUTTON|MK_RBUTTON|MK_MBUTTON|MK_XBUTTON1|MK_XBUTTON2.
                    match kind {
                        PointerKind::Down => {
                            SetCapture(hwnd);
                        }
                        PointerKind::Up if wparam & 0x73 == 0 => {
                            ReleaseCapture();
                        }
                        _ => {}
                    }
                    if let Ok(mut q) = s.events.lock() {
                        q.push(LocalEvent::Pointer {
                            kind,
                            button,
                            client_x: loword(lparam),
                            client_y: hiword(lparam),
                        });
                    }
                }
                // WM_XBUTTON* must return TRUE.
                if msg == WM_XBUTTONDOWN || msg == WM_XBUTTONUP {
                    1
                } else {
                    0
                }
            }
            WM_CLOSE => {
                if let Some(s) = state {
                    s.quit.store(true, Ordering::Relaxed);
                }
                // SAFETY: posts WM_QUIT to this thread's message queue; the
                // pump loop picks it up (we also return 0 so the window is
                // not destroyed behind the loop's back).
                PostQuitMessage(0);
                0
            }
            _ => {
                // SAFETY: default processing for unhandled messages.
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
    }

    /// Repaint the stored frame with the cursor overlay composited on top,
    /// scaled to the stored content rect (letterboxed by aspect).
    ///
    /// Without a visible cursor overlay the frame is blitted straight from
    /// its own buffer (no copy). Otherwise the composite is cached in
    /// `state.display` keyed by `(frame seq, overlay)` and its buffer is
    /// reused: a new frame costs one copy into the existing allocation, and a
    /// cursor move between frames only restores the old cursor rectangle from
    /// the frame and blends the new one — O(cursor area), not O(frame).
    ///
    /// SAFETY: `hdc` is a live DC from `BeginPaint`; `frame.pixels` is a
    /// complete BGRA buffer of `stride * height` bytes (checked by the
    /// reassembler via `is_complete`) whose backing memory outlives the call.
    unsafe fn paint(hdc: HDC, state: &WindowState) {
        let Ok(guard) = state.last_frame.lock() else {
            return;
        };
        let hud = state.hud.lock().map(|h| h.clone()).unwrap_or_default();
        // Section 4: Direct2D first; any failure drops to GDI for good.
        if let Ok(mut slot) = state.d2d.lock() {
            if let Some(r) = slot.as_mut() {
                let overlay = state.cursor.lock().map(|c| c.clone()).unwrap_or_default();
                let rect = state
                    .content_rect
                    .lock()
                    .map(|r| *r)
                    .unwrap_or(super::Rect {
                        x: 0,
                        y: 0,
                        w: 1,
                        h: 1,
                    });
                match r.render(guard.as_ref(), &overlay, &hud, rect, state.client_size()) {
                    Ok(true) => return,
                    Ok(false) => {
                        state.repaint_pending.store(true, Ordering::Relaxed);
                        return;
                    }
                    Err(e) => {
                        tracing::warn!("Direct2D presenter failed ({e}); falling back to GDI");
                        *slot = None;
                    }
                }
            }
        }
        let Some(frame) = guard.as_ref() else {
            // No frame yet: still surface the dead-peer banner.
            if hud.dead {
                draw_hud(hdc, &hud, 8, 8, 600);
            }
            return;
        };
        if !frame.is_complete() || frame.width == 0 || frame.height == 0 {
            return;
        }
        let Ok(rect) = state.content_rect.lock() else {
            return;
        };
        let overlay = state.cursor.lock().map(|c| c.clone()).unwrap_or_default();
        // Build (or reuse) the composited display buffer.
        let Ok(mut disp) = state.display.lock() else {
            return;
        };
        let key_overlay = overlay.clone();
        let mut pixels: &[u8] = if overlay.pos.is_none() || overlay.shape.is_none() {
            // Nothing to composite: blit the frame itself.
            &frame.pixels
        } else {
            crate::window::compose(&mut disp, frame, overlay)
        };
        // Section 4: a real downscale goes through the CPU area/sharpen
        // scaler (area + sharpen 0.5) and is then blitted 1:1.
        let mut src_w = frame.width as i32;
        let mut src_h = frame.height as i32;
        let mut src_stride = frame.stride as usize;
        let shrink = (rect.w as f32 / frame.width as f32).min(rect.h as f32 / frame.height as f32);
        let Ok(mut scaled) = state.scaled.lock() else {
            return;
        };
        if shrink < 0.97 && rect.w > 0 && rect.h > 0 {
            let key = (frame.seq, key_overlay, rect.w, rect.h);
            let stale = match scaled.as_ref() {
                Some((sc, k, _)) => {
                    *k != key
                        || sc.dims() != (frame.width, frame.height, rect.w as u32, rect.h as u32)
                }
                None => true,
            };
            if stale {
                let (mut sc, mut buf) = match scaled.take() {
                    Some((sc, _, buf))
                        if sc.dims()
                            == (frame.width, frame.height, rect.w as u32, rect.h as u32) =>
                    {
                        (sc, buf)
                    }
                    Some((_, _, buf)) => (
                        ovapp::scale::Scaler::area_sharpen(
                            frame.width,
                            frame.height,
                            rect.w as u32,
                            rect.h as u32,
                        ),
                        buf,
                    ),
                    None => (
                        ovapp::scale::Scaler::area_sharpen(
                            frame.width,
                            frame.height,
                            rect.w as u32,
                            rect.h as u32,
                        ),
                        Vec::new(),
                    ),
                };
                sc.scale(pixels, frame.stride as usize, &mut buf);
                *scaled = Some((sc, key, buf));
            }
            if let Some((_, _, buf)) = scaled.as_ref() {
                pixels = buf;
                src_w = rect.w;
                src_h = rect.h;
                src_stride = rect.w as usize * 4;
            }
        }
        let _ = src_stride;
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: src_w,
                biHeight: -src_h, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                biSizeImage: pixels.len() as u32,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            bmiColors: [0],
        };
        // COLORONCOLOR: fast, artifact-free for the usual ~1:1 / upscale case
        // (the default BLACKONWHITE ANDs dropped rows into dark smears);
        // HALFTONE (needs a brush-origin reset) only for a strong downscale
        // where its filtering visibly helps text.
        // (A downscale was already done by the CPU scaler above; HALFTONE
        // remains for a mild one it skipped.)
        let downscale = (rect.w as f32 / src_w as f32).min(rect.h as f32 / src_h as f32);
        if downscale < 0.75 {
            SetStretchBltMode(hdc, HALFTONE);
            SetBrushOrgEx(hdc, 0, 0, std::ptr::null_mut());
        } else {
            SetStretchBltMode(hdc, COLORONCOLOR);
        }
        // SAFETY: all pointers/rects valid; BGRA 32-bpp top-down maps directly.
        StretchDIBits(
            hdc,
            rect.x,
            rect.y,
            rect.w,
            rect.h, // destination (letterboxed content rect)
            0,
            0,
            src_w,
            src_h, // source (whole frame, or its pre-scaled copy)
            pixels.as_ptr().cast(),
            &bmi,
            DIB_RGB_COLORS,
            0, // SRCCOPY
        );
        // HUD overlay (v0.16): the health panel top-left, and the dead-peer
        // banner centered above it when the peer is gone. Drawn on top of the
        // blitted frame every paint.
        let (cw, _) = state.client_size();
        draw_hud(hdc, &hud, rect.x + 8, rect.y + 8, cw);
    }

    /// Draw the HUD lines (mono-ish default font, low-opacity via dark text
    /// over the frame) and, when the peer is dead, the centered banner.
    /// Pure Win32 `TextOutW` — no new deps, matches the house FFI style.
    ///
    /// SAFETY: `hdc` is a live DC from `BeginPaint`.
    unsafe fn draw_hud(hdc: HDC, hud: &HudState, x: i32, y: i32, area_w: i32) {
        // TRANSPARENT background so the frame shows through the text.
        unsafe {
            SetBkMode(hdc, 1 /* TRANSPARENT */)
        };
        unsafe { SetTextColor(hdc, 0x00B0_FF90) }; // pale green (BGR)
        if hud.visible {
            for (i, line) in hud.lines.iter().enumerate() {
                let w = wide(line);
                // SAFETY: `w` outlives the call; hdc is live.
                unsafe {
                    TextOutW(hdc, x, y + (i as i32) * 16, w.as_ptr(), w.len() as i32);
                }
            }
        }
        if hud.dead {
            let banner = "Peer disconnected — waiting for traffic…";
            let w = wide(banner);
            let mut size = SIZE::default();
            // SAFETY: measure with the same DC the text is drawn on.
            unsafe {
                GetTextExtentPoint32W(hdc, w.as_ptr(), w.len() as i32, &mut size);
            }
            unsafe { SetTextColor(hdc, 0x0000_4DFF) }; // warm red (BGR)
            let bx = ((area_w - size.cx) / 2).max(0);
            let by = if hud.visible {
                y + hud.lines.len() as i32 * 16 + 12
            } else {
                y + 12
            };
            unsafe {
                TextOutW(hdc, bx, by, w.as_ptr(), w.len() as i32);
            }
        }
    }

    impl WindowState {
        /// Current client-area size (w, h) in pixels.
        pub(super) fn client_size(&self) -> (i32, i32) {
            let hwnd = self.hwnd.load(Ordering::Relaxed);
            if hwnd == 0 {
                return (0, 0);
            }
            let mut rc = RECT::default();
            // SAFETY: `rc` is a valid RECT for this live hwnd.
            if unsafe { GetClientRect(hwnd, &mut rc) } == 0 {
                return (0, 0);
            }
            (rc.right - rc.left, rc.bottom - rc.top)
        }

        /// Schedule a repaint of the whole client area.
        pub(super) fn invalidate(&self) {
            let hwnd = self.hwnd.load(Ordering::Relaxed);
            if hwnd != 0 {
                // SAFETY: null rect = whole client area; the erase is handled
                // by our WM_ERASEBKGND (returns 1, no flicker).
                unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
            }
        }

        /// Drain the message queue: dispatch everything pending, then return.
        pub(super) fn pump_messages(&self) {
            let hwnd = self.hwnd.load(Ordering::Relaxed);
            // A present refused at vsync: paint again now.
            if self.repaint_pending.swap(false, Ordering::Relaxed) {
                self.invalidate();
            }
            let mut msg = MSG {
                hwnd: 0,
                message: 0,
                wParam: 0,
                lParam: 0,
                time: 0,
                pt: POINT::default(),
            };
            // SAFETY: `msg` is a valid MSG; hwnd = the window's own queue.
            while unsafe { PeekMessageW(&mut msg, hwnd, 0, 0, PM_REMOVE) } != 0 {
                if msg.message == WM_QUIT {
                    self.quit.store(true, Ordering::Relaxed);
                    return;
                }
                // SAFETY: standard translate/dispatch pair on a popped MSG.
                unsafe {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }
    }

    /// Register the class, create the window with `state` as its userdata,
    /// and show it. Returns the shared state on success.
    pub fn create_window(title: &str) -> Result<Arc<WindowState>, String> {
        let state = Arc::new(WindowState::new());
        // SAFETY: GetModuleHandleW(null) returns our exe's module handle.
        let hinstance = unsafe { GetModuleHandleW(std::ptr::null()) };
        if hinstance == 0 {
            return Err("GetModuleHandleW failed".into());
        }
        let class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
            lpfnWndProc: wnd_proc,
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            // SAFETY: MAKEINTRESOURCE(APP_ICON_ID); the icon is embedded by
            // build.rs. A missing resource yields 0 (default icon).
            hIcon: unsafe { LoadIconW(hinstance, APP_ICON_ID as usize as *const u16) },
            // SAFETY: LoadCursorW(IDC_ARROW) is the standard arrow cursor.
            hCursor: unsafe { LoadCursorW(0, IDC_ARROW) },
            hbrBackground: 0, // we paint everything ourselves
            lpszMenuName: std::ptr::null(),
            lpszClassName: CLASS_NAME.as_ptr(),
            // 0: Windows derives the small icon from the same ICO resource.
            hIconSm: 0,
        };
        // SAFETY: `class` is fully initialized and `CLASS_NAME` is static.
        // A zero return is only reported if CreateWindowExW then fails: an
        // already-registered class (ERROR_CLASS_ALREADY_EXISTS) still works.
        let atom = unsafe { RegisterClassExW(&class) };
        // SAFETY: plain thread-local error read.
        let class_err = if atom == 0 {
            unsafe { GetLastError() }
        } else {
            0
        };
        let title_w = wide(title);
        // Hand one refcount to the window: parked in GWLP_USERDATA right after
        // creation and intentionally kept for the window's lifetime.
        let state_raw = Arc::into_raw(state.clone());
        // SAFETY: `state_raw` is a valid *const WindowState; CLASS_NAME and
        // `title_w` outlive the call.
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                CLASS_NAME.as_ptr(),
                title_w.as_ptr(),
                WS_OVERLAPPEDWINDOW,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                0,
                0,
                hinstance,
                state_raw.cast(),
            )
        };
        if hwnd == 0 {
            // Reclaim the refcount handed to the (never-created) window.
            drop(unsafe { Arc::from_raw(state_raw) });
            // SAFETY: plain thread-local error read.
            let err = unsafe { GetLastError() };
            return Err(format!(
                "CreateWindowExW failed (error {err}, class error {class_err})"
            ));
        }
        // SAFETY: `hwnd` is the window we just created; GWLP_USERDATA is the
        // documented app-data slot and the `Ptr` variants are correct because
        // we only build x86_64-pc-windows-msvc.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_raw as isize) };
        state.hwnd.store(hwnd, Ordering::Relaxed);
        // Section 4: the Direct2D presenter (GDI when it cannot start).
        if super::RENDERER
            .lock()
            .map(|r| *r == super::RendererPref::Direct2D)
            .unwrap_or(false)
        {
            match super::d2d::D2dRenderer::new(hwnd) {
                Ok(r) => {
                    if let Ok(mut slot) = state.d2d.lock() {
                        *slot = Some(r);
                    }
                }
                Err(e) => tracing::warn!("Direct2D unavailable ({e}); using GDI"),
            }
        }
        // SAFETY: standard show/update pair for a freshly created window.
        unsafe {
            ShowWindow(hwnd, SW_SHOW);
            UpdateWindow(hwnd);
        }
        Ok(state)
    }
}

#[cfg(test)]
mod cursor_tests {
    use super::*;

    fn solid(w: u16, h: u16, bgra: [u8; 4]) -> CursorBmp {
        CursorBmp {
            hotspot_x: 1,
            hotspot_y: 1,
            w,
            h,
            bgra: bgra
                .iter()
                .flat_map(|&b| [b; 1])
                .cycle()
                .take(w as usize * h as usize * 4)
                .collect(),
        }
    }

    #[test]
    fn opaque_cursor_replaces_pixels() {
        // 2x2 fully-opaque red cursor on a 4x4 gray frame.
        let mut frame = vec![64u8; 4 * 4 * 4];
        let cur = solid(2, 2, [0, 0, 255, 255]); // BGRA red
        blend_cursor(&mut frame, 16, 4, 4, &cur, 2, 2);
        // Hotspot (1,1) means the bitmap covers (1,1)..(3,3); top-left pixel
        // of the frame is untouched (frame alpha stays 64).
        assert_eq!(&frame[0..4], &[64, 64, 64, 64][..]);
        // Pixel (1,1) -> byte offset 1*16 + 1*4 = 20: red, alpha forced 255.
        assert_eq!(&frame[20..24], &[0, 0, 255, 255][..]);
        // Pixel (2,2) -> offset (2*4 + 2) * 4 = 40.
        assert_eq!(&frame[40..44], &[0, 0, 255, 255][..]);
        // Pixel (3,3) outside the bitmap: untouched (frame alpha stays 64).
        assert_eq!(
            &frame[(3 * 4 + 3) * 4..(3 * 4 + 3) * 4 + 4],
            &[64, 64, 64, 64][..]
        );
    }

    #[test]
    fn translucent_cursor_blends_premultiplied() {
        // 1x1 cursor, 50% alpha, premultiplied half-red (b=0, g=0, r=128).
        let mut frame = vec![0u8; 4]; // single black pixel (B=0)
        let cur = CursorBmp {
            hotspot_x: 0,
            hotspot_y: 0,
            w: 1,
            h: 1,
            bgra: vec![0, 0, 128, 128],
        };
        blend_cursor(&mut frame, 4, 1, 1, &cur, 0, 0);
        // out = src + dst*(127)/255 = 0 + 0 = 0 for b/g; r = 128 + 0; a = 255.
        assert_eq!(frame, [0, 0, 128, 255]);
    }

    #[test]
    fn cursor_is_clipped_at_frame_edges() {
        // 2x2 opaque cursor whose bottom-right hangs off a 2x2 frame.
        let mut frame = vec![10u8; 2 * 2 * 4];
        let cur = solid(2, 2, [0, 0, 255, 255]);
        blend_cursor(&mut frame, 8, 2, 2, &cur, 1, 1);
        // Bitmap covers all four pixels (hotspot 1,1 → origin 0,0).
        assert_eq!(&frame[0..4], &[0, 0, 255, 255][..]);
        assert_eq!(&frame[4..8], &[0, 0, 255, 255][..]);
    }

    #[test]
    fn malformed_bitmap_does_not_panic() {
        // Claims 2x2 but carries too few bytes: clipped loop must stop.
        let mut frame = vec![0u8; 4 * 4];
        let cur = CursorBmp {
            hotspot_x: 0,
            hotspot_y: 0,
            w: 2,
            h: 2,
            bgra: vec![255; 5],
        };
        blend_cursor(&mut frame, 16, 1, 1, &cur, 0, 0);
        assert!(frame.iter().all(|&b| b == 0 || b == 255));
    }

    #[test]
    fn zero_sized_cursor_is_a_noop() {
        let mut frame = vec![7u8; 4];
        let cur = CursorBmp {
            hotspot_x: 0,
            hotspot_y: 0,
            w: 0,
            h: 0,
            bgra: vec![],
        };
        blend_cursor(&mut frame, 4, 1, 1, &cur, 0, 0);
        assert_eq!(frame, [7, 7, 7, 7]);
    }

    #[test]
    fn compose_incremental_cursor_move_matches_a_fresh_composite() {
        use ovapp::screen::{Frame, PixelFormat};
        let frame = Frame::new(
            16,
            8,
            PixelFormat::Bgra,
            1,
            (0..16 * 8 * 4).map(|i| (i % 251) as u8).collect(),
        );
        let cur = solid(3, 3, [200, 100, 50, 255]);
        let at = |x, y| CursorOverlay {
            pos: Some((x, y)),
            shape: Some(cur.clone()),
        };
        let mut disp: Display = None;
        let _ = compose(&mut disp, &frame, at(2, 2));
        // Same frame, cursor moved (incl. partially off-frame): restore + blend.
        for (x, y) in [(10, 5), (15, 7), (0, 0), (-1, 3)] {
            let inc = compose(&mut disp, &frame, at(x, y)).to_vec();
            let mut fresh: Display = None;
            let full = compose(&mut fresh, &frame, at(x, y)).to_vec();
            assert_eq!(inc, full, "cursor at ({x},{y})");
        }
        // A new frame reuses the allocation and still composites correctly.
        let frame2 = Frame::new(16, 8, PixelFormat::Bgra, 2, vec![9; 16 * 8 * 4]);
        let out = compose(&mut disp, &frame2, at(4, 4)).to_vec();
        let mut fresh: Display = None;
        assert_eq!(out, compose(&mut fresh, &frame2, at(4, 4)));
    }
}
