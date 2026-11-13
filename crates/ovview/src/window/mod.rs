//! The desktop render window.
//!
//! [`RenderWindow`] is a plain top-level Win32 window (bound through the
//! `windows` crate, no GUI framework) that:
//!
//! - presents the latest [`Frame`] letterboxed by aspect ratio into the
//!   client area — with Direct2D ([`d2d`]) or, as the fallback, GDI
//!   `StretchDIBits` ([`paint`]) — plus the remote cursor and the HUD;
//! - queues local mouse/keyboard events as [`LocalEvent`]s for the viewer
//!   loop to turn into POINTER/KEY commands ([`wndproc`]);
//! - sets a quit flag on `WM_CLOSE` so the loop can tear down cleanly.
//!
//! Window state lives in a heap `Arc<WindowState>` parked in the window's
//! `GWLP_USERDATA` slot so the static `WndProc` can reach it.
//!
//! On non-Windows targets every method is a no-op stub so the crate — and the
//! viewer loop — compiles unchanged; the pure normalization math it relies on
//! lives in [`crate::localinput`] and the cursor compositing in [`cursor`],
//! both tested everywhere.

use std::sync::Mutex;
#[cfg(windows)]
use std::sync::{
    atomic::{AtomicBool, AtomicPtr, Ordering},
    Arc,
};

use crate::localinput::{LocalEvent, Rect};
use ovapp::screen::Frame;

mod cursor;
#[cfg(windows)]
mod d2d;
#[cfg(windows)]
mod overlay;
#[cfg(windows)]
mod paint;
#[cfg(windows)]
mod wndproc;

#[cfg(windows)]
use cursor::CursorOverlay;

/// Which presenter the window uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RendererPref {
    /// Direct2D high-quality cubic + flip-model vsync; GDI on failure.
    Direct2D,
    /// GDI `StretchDIBits`, with the CPU area/sharpen scaler
    /// ([`ovapp::scale`]: area + sharpen 0.5) for downscaling.
    Gdi,
}

static RENDERER: Mutex<RendererPref> = Mutex::new(RendererPref::Direct2D);

/// Choose the presenter for windows created afterwards.
pub(crate) fn set_renderer(p: RendererPref) {
    if let Ok(mut r) = RENDERER.lock() {
        *r = p;
    }
}

/// A remote-cursor bitmap (`CURSOR_SHAPE` payload): 32-bit BGRA, premultiplied
/// alpha, with the hotspot — the pixel under the logical cursor tip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CursorBmp {
    pub(crate) hotspot_x: u16,
    pub(crate) hotspot_y: u16,
    pub(crate) w: u16,
    pub(crate) h: u16,
    pub(crate) bgra: Vec<u8>,
}

/// The HUD overlay state: the six health lines plus the dead-peer banner.
/// Shared between the viewer loop (which updates it) and the presenter
/// (which draws it).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
struct HudState {
    lines: [String; 6],
    /// Peer considered dead (no inbound for `DEAD_AFTER`): show the banner
    /// and dim the frame.
    dead: bool,
    /// Panel visibility (toggled with H / F3).
    visible: bool,
}

/// The content rect before the first frame: 1x1 at the client origin.
const UNIT_RECT: Rect = Rect {
    x: 0,
    y: 0,
    w: 1,
    h: 1,
};

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
    /// The window handle (`HWND`), parked after `CreateWindowExW` returns
    /// (the `WndProc` sees a null userdata slot only for messages dispatched
    /// during creation, which it handles fine).
    hwnd: AtomicPtr<core::ffi::c_void>,
    /// The most recently presented frame (moved in by
    /// [`RenderWindow::present_owned`]).
    last_frame: Mutex<Option<Frame>>,
    /// Remote cursor overlay state (`CURSOR`/`CURSOR_SHAPE` commands).
    cursor: Mutex<CursorOverlay>,
    /// The composited (frame + cursor) pixel buffer last used for GDI
    /// painting, plus the `(frame seq, overlay)` key it was built from.
    /// Recomputed only when the frame or the cursor changes, so a cursor
    /// move costs one frame copy + blend, not a repaint of stale composites.
    display: Mutex<cursor::Display>,
    /// The letterboxed content rect of `last_frame` inside the client area.
    content_rect: Mutex<Rect>,
    /// Local input events queued by the `WndProc`, drained by `take_input`.
    events: Mutex<Vec<LocalEvent>>,
    /// HUD overlay content, updated by the viewer loop.
    hud: Mutex<HudState>,
    /// Set on `WM_CLOSE` (or by [`RenderWindow::close`]).
    quit: AtomicBool,
    /// The Direct2D presenter (`None` = GDI path).
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
            hwnd: AtomicPtr::new(std::ptr::null_mut()),
            last_frame: Mutex::new(None),
            cursor: Mutex::new(CursorOverlay::default()),
            display: Mutex::new(None),
            content_rect: Mutex::new(UNIT_RECT),
            events: Mutex::new(Vec::new()),
            hud: Mutex::new(HudState::default()),
            quit: AtomicBool::new(false),
            d2d: Mutex::new(None),
            repaint_pending: AtomicBool::new(false),
            scaled: Mutex::new(None),
        }
    }

    /// The stored content rect (1x1 at the origin if the lock is poisoned).
    fn content_rect(&self) -> Rect {
        self.content_rect.lock().map(|r| *r).unwrap_or(UNIT_RECT)
    }
}

/// A render window. On Windows a real `HWND`-backed top-level window; on
/// other platforms a no-op shell so the viewer loop still runs headless.
pub(crate) struct RenderWindow {
    #[cfg(windows)]
    state: Arc<WindowState>,
    /// Keeps the non-Windows shape honest (no fields needed there).
    #[cfg(not(windows))]
    _private: (),
}

impl RenderWindow {
    /// Create and show the window (titled "OpenView — Remote"). On
    /// non-Windows this is a no-op that always succeeds (the viewer loop then
    /// runs headless).
    pub(crate) fn new() -> Result<Self, String> {
        #[cfg(windows)]
        {
            wndproc::create_window()
                .map(|state| Self { state })
                .map_err(|e| format!("window creation failed: {e}"))
        }
        #[cfg(not(windows))]
        {
            Ok(Self { _private: () })
        }
    }

    /// Present a new frame (copying it). Prefer
    /// [`present_owned`](Self::present_owned) on the hot path. No-op
    /// off-Windows.
    pub(crate) fn present(&self, frame: &Frame) {
        let _ = self.present_owned(frame.clone());
    }

    /// Present a new frame by value (no pixel copy): compute the letterbox
    /// rect for its aspect inside the current client area, store the frame,
    /// and schedule a repaint. Returns the previously presented frame so the
    /// caller can recycle its buffer. Off-Windows the frame is handed back.
    pub(crate) fn present_owned(&self, frame: Frame) -> Option<Frame> {
        #[cfg(windows)]
        {
            let (cw, ch) = self.state.client_size();
            let rect = crate::localinput::letterbox_rect(cw, ch, frame.width, frame.height);
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
    pub(crate) fn set_cursor(&self, x: i32, y: i32) {
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
    pub(crate) fn set_cursor_shape(&self, shape: CursorBmp) {
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
    pub(crate) fn set_hud(&self, lines: [String; 6], dead: bool, visible: bool) {
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
    pub(crate) fn pump_messages(&self) {
        #[cfg(windows)]
        self.state.pump_messages();
    }

    /// Drain the local input queue collected by the `WndProc`.
    pub(crate) fn take_input(&self) -> Vec<LocalEvent> {
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
    pub(crate) fn should_quit(&self) -> bool {
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
    pub(crate) fn repaint_pending(&self) -> bool {
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
    pub(crate) fn renderer_name(&self) -> String {
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
    pub(crate) fn close(&self) {
        #[cfg(windows)]
        self.state.quit.store(true, Ordering::Relaxed);
    }

    /// The letterboxed content rect currently stored (client coordinates).
    /// The viewer loop reads this to normalize pointer events. Identity rect
    /// off-Windows.
    pub(crate) fn content_rect(&self) -> Rect {
        #[cfg(windows)]
        {
            self.state.content_rect()
        }
        #[cfg(not(windows))]
        {
            UNIT_RECT
        }
    }
}
