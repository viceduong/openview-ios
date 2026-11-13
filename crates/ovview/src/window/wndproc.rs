//! Win32 plumbing: window class + creation, the message pump, and the
//! `WndProc` that paints, queues local input and turns `WM_CLOSE` into the
//! quit flag.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use ovapp::terminal::{MouseButton, PointerKind};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, InvalidateRect, UpdateWindow, PAINTSTRUCT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect, GetWindowLongPtrW,
    LoadCursorW, LoadIconW, PeekMessageW, PostQuitMessage, RegisterClassExW, SetWindowLongPtrW,
    ShowWindow, TranslateMessage, CS_DBLCLKS, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, GWLP_USERDATA,
    IDC_ARROW, MSG, PM_REMOVE, SW_SHOW, WINDOW_EX_STYLE, WM_CLOSE, WM_ERASEBKGND, WM_KEYDOWN,
    WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL,
    WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_PAINT, WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN,
    WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSEXW, WS_OVERLAPPEDWINDOW,
};

use super::{d2d, paint, RendererPref, WindowState, RENDERER};
use crate::localinput::LocalEvent;

/// Title of the render window (live sessions and playback alike).
const TITLE: PCWSTR = w!("OpenView — Remote");

/// The window class name.
const CLASS_NAME: PCWSTR = w!("OVVIEW_RENDERER");

/// Resource id of the app icon (embedded by build.rs via `assets/winres.rs`).
const APP_ICON_ID: u16 = 1;

/// `MK_LBUTTON | MK_RBUTTON | MK_MBUTTON | MK_XBUTTON1 | MK_XBUTTON2`: any
/// mouse button still held (`wParam` of a mouse message).
const MK_ANY_BUTTON: usize = 0x73;

/// `XBUTTON2` in the high word of a `WM_XBUTTON*` `wParam`.
const XBUTTON2: usize = 2;

/// Signed `LOWORD` of a message `lParam` (client x; negative only when
/// capture drags past an edge).
fn loword(l: LPARAM) -> i32 {
    i32::from((l.0 as u32 & 0xFFFF) as u16 as i16)
}

/// Signed `HIWORD` of a message `lParam` (client y).
fn hiword(l: LPARAM) -> i32 {
    i32::from(((l.0 as u32) >> 16) as u16 as i16)
}

impl WindowState {
    /// The window handle (null until `create_window` parked it).
    pub(super) fn hwnd(&self) -> HWND {
        HWND(self.hwnd.load(Ordering::Relaxed))
    }

    /// Current client-area size (w, h) in pixels.
    pub(super) fn client_size(&self) -> (i32, i32) {
        let hwnd = self.hwnd();
        if hwnd.is_invalid() {
            return (0, 0);
        }
        let mut rc = RECT::default();
        // SAFETY: `hwnd` is our live window and `rc` a valid out-param.
        if unsafe { GetClientRect(hwnd, &mut rc) }.is_err() {
            return (0, 0);
        }
        (rc.right - rc.left, rc.bottom - rc.top)
    }

    /// Schedule a repaint of the whole client area.
    pub(super) fn invalidate(&self) {
        let hwnd = self.hwnd();
        if !hwnd.is_invalid() {
            // SAFETY: `hwnd` is our live window; no rect = the whole client
            // area. No erase: our WM_ERASEBKGND returns 1 (no flicker).
            let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
        }
    }

    /// Drain the message queue: dispatch everything pending, then return.
    pub(super) fn pump_messages(&self) {
        let hwnd = self.hwnd();
        // A present refused at vsync: paint again now.
        if self.repaint_pending.swap(false, Ordering::Relaxed) {
            self.invalidate();
        }
        let mut msg = MSG::default();
        // SAFETY: `msg` is a valid out-param; the filter is the window's own
        // messages (a null handle before creation reads the thread queue).
        while unsafe { PeekMessageW(&mut msg, Some(hwnd), 0, 0, PM_REMOVE) }.as_bool() {
            if msg.message == WM_QUIT {
                self.quit.store(true, Ordering::Relaxed);
                return;
            }
            // SAFETY: the standard translate/dispatch pair on a popped MSG.
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    /// Queue one local input event for the viewer loop.
    fn push_event(&self, ev: LocalEvent) {
        if let Ok(mut q) = self.events.lock() {
            q.push(ev);
        }
    }
}

/// The static `WndProc`: hands each message to [`dispatch`] with the
/// window's state.
///
/// # Safety
///
/// Called by Win32 only, with a valid `hwnd` of our class. Its
/// `GWLP_USERDATA` slot is either null (messages dispatched *during*
/// creation, `WM_NCCREATE`/`WM_CREATE`/..., which fall through to
/// `DefWindowProcW` — the documented-correct behavior for them) or the
/// `Arc<WindowState>` pointer `create_window` parked there, which holds one
/// strong count for the window's lifetime.
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: `hwnd` is valid (contract above); reading the userdata slot
    // has no other precondition.
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const WindowState;
    // SAFETY: a non-null slot is the pointer from `Arc::into_raw` in
    // `create_window`, whose strong count the window owns and never
    // releases, so the state outlives this call.
    let state = unsafe { ptr.as_ref() };
    dispatch(hwnd, msg, wparam, lparam, state)
}

/// The actual message handling. Returns the value `WndProc` should return.
fn dispatch(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    state: Option<&WindowState>,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            // SAFETY: `hwnd` is the window being painted and `ps` a valid
            // PAINTSTRUCT; EndPaint below pairs on the same hwnd.
            let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
            if !hdc.is_invalid() {
                if let Some(s) = state {
                    // SAFETY: `hdc` is the live DC from BeginPaint.
                    unsafe { paint::paint(hdc, s) };
                }
                // SAFETY: paired with BeginPaint above.
                let _ = unsafe { EndPaint(hwnd, &ps) };
            }
            LRESULT(0)
        }
        // Every paint covers the whole content rect; skipping the erase
        // avoids flicker.
        WM_ERASEBKGND => LRESULT(1),
        // WM_SYSKEY* too: Alt combinations (Alt+Tab inside the remote,
        // Alt+F4 of a remote app, F10) go to the host.
        WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP => {
            if let Some(s) = state {
                let l = lparam.0 as usize;
                s.push_event(LocalEvent::Key {
                    vk: wparam.0 as u16,
                    scan: ((l >> 16) & 0xFF) as u16,
                    extended: (l >> 24) & 1 != 0,
                    down: msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN,
                });
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            if let Some(s) = state {
                // HIWORD(wParam): signed delta in WHEEL_DELTA units.
                let delta = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16;
                s.push_event(if msg == WM_MOUSEWHEEL {
                    LocalEvent::Wheel { dy: delta, dx: 0 }
                } else {
                    LocalEvent::Wheel { dy: 0, dx: delta }
                });
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE | WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP
        | WM_MBUTTONDOWN | WM_MBUTTONUP | WM_XBUTTONDOWN | WM_XBUTTONUP => {
            if let Some(s) = state {
                mouse_event(s, hwnd, msg, wparam, lparam);
            }
            // WM_XBUTTON* must return TRUE.
            LRESULT(isize::from(msg == WM_XBUTTONDOWN || msg == WM_XBUTTONUP))
        }
        WM_CLOSE => {
            if let Some(s) = state {
                s.quit.store(true, Ordering::Relaxed);
            }
            // SAFETY: posts WM_QUIT to this thread's queue for the pump; we
            // return 0 so the window is not destroyed behind the loop's back.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // SAFETY: default processing of a message Win32 sent to `hwnd`.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Queue a mouse move/button event, capturing the mouse while a button is
/// held so the UP arrives even when it happens outside the window.
fn mouse_event(s: &WindowState, hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) {
    let (kind, button) = match msg {
        WM_LBUTTONDOWN => (PointerKind::Down, MouseButton::Left),
        WM_LBUTTONUP => (PointerKind::Up, MouseButton::Left),
        WM_RBUTTONDOWN => (PointerKind::Down, MouseButton::Right),
        WM_RBUTTONUP => (PointerKind::Up, MouseButton::Right),
        WM_MBUTTONDOWN => (PointerKind::Down, MouseButton::Middle),
        WM_MBUTTONUP => (PointerKind::Up, MouseButton::Middle),
        WM_XBUTTONDOWN | WM_XBUTTONUP => {
            let b = if (wparam.0 >> 16) & 0xFFFF == XBUTTON2 {
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
    match kind {
        PointerKind::Down => {
            // SAFETY: `hwnd` is our live window, on its own thread.
            unsafe { SetCapture(hwnd) };
        }
        PointerKind::Up if wparam.0 & MK_ANY_BUTTON == 0 => {
            // SAFETY: no preconditions; releasing without capture is a no-op.
            let _ = unsafe { ReleaseCapture() };
        }
        _ => {}
    }
    s.push_event(LocalEvent::Pointer {
        kind,
        button,
        client_x: loword(lparam),
        client_y: hiword(lparam),
    });
}

/// Register the class, create the window with its state as userdata, start
/// the preferred presenter, and show it. Returns the shared state.
pub(super) fn create_window() -> Result<Arc<WindowState>, String> {
    let state = Arc::new(WindowState::new());
    // SAFETY: a null module name returns our exe's module handle.
    let hinstance: HINSTANCE = unsafe { GetModuleHandleW(None) }
        .map_err(|_| "GetModuleHandleW failed".to_owned())?
        .into();
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
        lpfnWndProc: Some(wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance,
        // SAFETY: MAKEINTRESOURCE(APP_ICON_ID) of our own module; the icon is
        // embedded by build.rs. A missing resource yields the default icon.
        hIcon: unsafe {
            LoadIconW(
                Some(hinstance),
                PCWSTR(usize::from(APP_ICON_ID) as *const u16),
            )
        }
        .unwrap_or_default(),
        // SAFETY: the standard arrow cursor, a system resource.
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
        // We paint everything ourselves.
        hbrBackground: Default::default(),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: CLASS_NAME,
        // Null: Windows derives the small icon from the same ICO resource.
        hIconSm: Default::default(),
    };
    // SAFETY: `class` is fully initialized and `CLASS_NAME` is static. A
    // zero return is only reported if CreateWindowExW then fails: an
    // already-registered class (ERROR_CLASS_ALREADY_EXISTS) still works.
    let atom = unsafe { RegisterClassExW(&class) };
    let class_err = if atom == 0 {
        // SAFETY: plain thread-local error read.
        unsafe { GetLastError() }.0
    } else {
        0
    };
    // Hand one strong count to the window: parked in GWLP_USERDATA right
    // after creation and intentionally kept for the window's lifetime.
    let state_raw = Arc::into_raw(Arc::clone(&state));
    // SAFETY: `CLASS_NAME` and `TITLE` are static; `state_raw` is a valid
    // pointer passed through as the creation parameter.
    let created = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS_NAME,
            TITLE,
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            None,
            None,
            Some(hinstance),
            Some(state_raw.cast()),
        )
    };
    let hwnd = match created {
        Ok(hwnd) => hwnd,
        Err(_) => {
            // SAFETY: plain thread-local error read.
            let err = unsafe { GetLastError() }.0;
            // SAFETY: reclaims the strong count handed to the never-created
            // window; nothing else holds `state_raw`.
            drop(unsafe { Arc::from_raw(state_raw) });
            return Err(format!(
                "CreateWindowExW failed (error {err}, class error {class_err})"
            ));
        }
    };
    // SAFETY: `hwnd` is the window we just created; GWLP_USERDATA is the
    // documented app-data slot (pointer-sized on x86_64).
    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_raw as isize) };
    state.hwnd.store(hwnd.0, Ordering::Relaxed);
    // The Direct2D presenter (GDI when it cannot start).
    if RENDERER
        .lock()
        .map(|r| *r == RendererPref::Direct2D)
        .unwrap_or(false)
    {
        match d2d::D2dRenderer::new(hwnd) {
            Ok(r) => {
                if let Ok(mut slot) = state.d2d.lock() {
                    *slot = Some(r);
                }
            }
            Err(e) => tracing::warn!("Direct2D unavailable ({e}); using GDI"),
        }
    }
    // SAFETY: the standard show/update pair for a freshly created window.
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = UpdateWindow(hwnd);
    }
    Ok(state)
}
