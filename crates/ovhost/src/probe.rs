//! `--latency-probe` (hidden): an objective input-to-photon target.
//!
//! Every input *press* the viewer sends (mouse button down, key down)
//! flips a 64x64 patch between two colours at the captured screen's
//! top-left corner: drawn into the image by `--test-pattern`, otherwise
//! painted by a small topmost host window at the primary monitor's origin
//! (so the flip goes through the real compositor → capture path). `ovview
//! --latency-probe N` presses a key N times and times each press until the
//! patch flips on its screen (see docs/USAGE.md, "Benchmarks").

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ovapp::terminal::{Command, PointerAction, PointerKind};

/// Patch edge, pixels.
pub(crate) const PATCH: u32 = 64;

static ENABLED: AtomicBool = AtomicBool::new(false);
static FLIPS: AtomicU64 = AtomicU64::new(0);

/// The patch colour (BGRA) after `flips` flips: magenta, then green — far
/// apart in every codec (the viewer tells them apart by the green channel).
pub(crate) fn color(flips: u64) -> [u8; 4] {
    if flips.is_multiple_of(2) {
        [255, 0, 255, 255]
    } else {
        [0, 255, 0, 255]
    }
}

/// Turn the probe on. `window`: paint the patch with a host window (real
/// desktop capture); `false` when the test pattern draws it.
pub(crate) fn enable(window: bool) {
    ENABLED.store(true, Ordering::Relaxed);
    tracing::info!(
        "latency probe: input presses flip a {PATCH}x{PATCH} patch at the top-left ({})",
        if window {
            "host window"
        } else {
            "test pattern"
        }
    );
    #[cfg(windows)]
    if window {
        win::spawn();
    }
}

/// Whether `--latency-probe` is on.
pub(crate) fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Flips so far.
pub(crate) fn flips() -> u64 {
    FLIPS.load(Ordering::Relaxed)
}

/// An input command arrived from the viewer: a press flips the patch.
pub(crate) fn on_input(cmd: &Command) {
    if !enabled() || !is_press(cmd) {
        return;
    }
    let n = FLIPS.fetch_add(1, Ordering::Relaxed) + 1;
    tracing::debug!("latency probe: flip {n}");
    #[cfg(windows)]
    win::repaint();
}

/// A button-down or key-down event.
fn is_press(cmd: &Command) -> bool {
    match cmd {
        Command::Pointer { kind, .. } => *kind == PointerKind::Down,
        Command::PointerEx { action, .. } => *action == PointerAction::Down,
        Command::Key { down, .. } => *down,
        Command::KeyEx { flags, .. } => flags & 1 != 0,
        _ => false,
    }
}

/// Draw the patch (colour for the current flips) into a BGRA image at
/// its top-left corner. Returns the patch rect's size (clipped).
pub(crate) fn draw(px: &mut [u8], w: u32, h: u32) -> (u32, u32) {
    let (pw, ph) = (PATCH.min(w), PATCH.min(h));
    let c = color(flips());
    let stride = w as usize * 4;
    for y in 0..ph as usize {
        let row = &mut px[y * stride..y * stride + pw as usize * 4];
        for p in row.as_chunks_mut::<4>().0 {
            *p = c;
        }
    }
    (pw, ph)
}

/// The topmost patch window.
#[cfg(windows)]
mod win {
    use std::sync::atomic::{AtomicIsize, Ordering};

    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, InvalidateRect, HGDIOBJ,
        PAINTSTRUCT,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, PostMessageW,
        RegisterClassW, TranslateMessage, MSG, WM_APP, WM_PAINT, WNDCLASSW, WS_EX_NOACTIVATE,
        WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
    };

    use super::{color, flips, PATCH};

    /// The window (0 = not created).
    static HWND_RAW: AtomicIsize = AtomicIsize::new(0);
    const WM_FLIP: u32 = WM_APP + 1;

    /// Create the window on its own thread (message loop until exit).
    pub(super) fn spawn() {
        let _ = std::thread::Builder::new()
            .name("latency-probe".into())
            .spawn(|| {
                if let Err(e) = run() {
                    tracing::warn!("latency probe window: {e}");
                }
            });
    }

    /// Ask the window to repaint with the new colour.
    pub(super) fn repaint() {
        let raw = HWND_RAW.load(Ordering::Relaxed);
        if raw != 0 {
            // SAFETY: posting to a window handle is safe even if it died
            // meanwhile (the call then just fails).
            let _ =
                unsafe { PostMessageW(Some(HWND(raw as *mut _)), WM_FLIP, WPARAM(0), LPARAM(0)) };
        }
    }

    fn run() -> Result<(), String> {
        // SAFETY: a null module name returns our exe's module handle.
        let hinstance = unsafe { GetModuleHandleW(None) }.map_err(|e| e.to_string())?;
        let class_name: PCWSTR = w!("OpenViewLatencyProbe");
        let class = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance.into(),
            lpszClassName: class_name,
            ..Default::default()
        };
        // SAFETY: `class` is fully initialized; the name is static.
        unsafe { RegisterClassW(&class) };
        // SAFETY: static class/title strings; no creation parameter.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                class_name,
                w!("OpenView latency probe"),
                WS_POPUP | WS_VISIBLE,
                0,
                0,
                PATCH as i32,
                PATCH as i32,
                None,
                None,
                Some(hinstance.into()),
                None,
            )
        }
        .map_err(|e| e.to_string())?;
        HWND_RAW.store(hwnd.0 as isize, Ordering::Relaxed);
        let mut msg = MSG::default();
        // SAFETY: the standard message loop of this thread's window.
        while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
            // SAFETY: translate/dispatch a message GetMessageW returned.
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        Ok(())
    }

    /// # Safety
    /// Called by Windows with a live window handle.
    unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        match msg {
            WM_FLIP => {
                // SAFETY: `hwnd` is our live window.
                let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
                LRESULT(0)
            }
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                // SAFETY: the BeginPaint/EndPaint pair on our window; the
                // brush is created and deleted inside it.
                unsafe {
                    let hdc = BeginPaint(hwnd, &mut ps);
                    let [b, g, r, _] = color(flips());
                    let brush = CreateSolidBrush(COLORREF(
                        u32::from(r) | u32::from(g) << 8 | u32::from(b) << 16,
                    ));
                    FillRect(hdc, &ps.rcPaint, brush);
                    let _ = DeleteObject(HGDIOBJ(brush.0));
                    let _ = EndPaint(hwnd, &ps);
                }
                LRESULT(0)
            }
            // SAFETY: default handling with the arguments we were given.
            _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presses_are_buttons_and_keys_down() {
        assert!(is_press(&Command::Key {
            vk: 0x87,
            down: true
        }));
        assert!(!is_press(&Command::Key {
            vk: 0x87,
            down: false
        }));
        assert!(is_press(&Command::KeyEx {
            vk: 0x87,
            scan: 0,
            flags: 1
        }));
        assert!(!is_press(&Command::KeyEx {
            vk: 0x87,
            scan: 0,
            flags: 0
        }));
        assert!(!is_press(&Command::Wheel { dy: 1, dx: 0 }));
    }

    #[test]
    fn patch_colours_alternate_and_draw_clipped() {
        assert_ne!(color(0), color(1));
        assert_eq!(color(0), color(2));
        let mut px = vec![0u8; 32 * 16 * 4];
        assert_eq!(draw(&mut px, 32, 16), (32, 16));
        assert!(px.as_chunks::<4>().0.iter().all(|p| *p == color(flips())));
    }
}
