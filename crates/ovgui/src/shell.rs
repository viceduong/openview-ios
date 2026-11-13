//! Native window integration on top of the D3D11 platform's HWND: window
//! icon, dark title bar, close-to-tray, the notification-area icon + menu,
//! balloons, theme-change detection and "show" requests from a second
//! instance. Implemented as a comctl32 window subclass, so the vendored
//! platform code stays untouched.
//!
//! The subclass procedure never touches Slint: it posts [`TrayCmd`]s /
//! events that the UI thread drains.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Mutex;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    DefSubclassProc, SetWindowSubclass, Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE,
    NIF_TIP, NIIF_INFO, NIIF_LARGE_ICON, NIIF_USER, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, GetSystemMetrics, IsIconic,
    LoadImageW, PostMessageW, RegisterWindowMessageW, SendMessageW, SetForegroundWindow,
    ShowWindow, TrackPopupMenu, HICON, ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_DEFAULTCOLOR,
    MF_CHECKED, MF_GRAYED, MF_SEPARATOR, MF_STRING, SM_CXICON, SM_CXSMICON, SW_HIDE, SW_RESTORE,
    SW_SHOW, TPM_BOTTOMALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_CLOSE, WM_LBUTTONDBLCLK,
    WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WM_SETICON, WM_SETTINGCHANGE, WM_USER,
};

use crate::events::{post, Event, TrayCmd};

const WM_TRAY: u32 = WM_APP + 1;
/// Posted by the single-instance watcher: bring the window up.
pub const WM_SHOW_REQUEST: u32 = WM_APP + 2;
const NIN_BALLOONUSERCLICK: u32 = WM_USER + 5;
const SUBCLASS_ID: usize = 0x0F_E7;

const ID_OPEN: usize = 1;
const ID_COPY: usize = 2;
const ID_PAUSE: usize = 3;
const ID_QUIT: usize = 4;

static HWND_RAW: AtomicIsize = AtomicIsize::new(0);
static QUITTING: AtomicBool = AtomicBool::new(false);
static PAUSED: AtomicBool = AtomicBool::new(false);
static HAS_ID: AtomicBool = AtomicBool::new(false);
static HINT_SHOWN: AtomicBool = AtomicBool::new(false);
static TASKBAR_CREATED: AtomicIsize = AtomicIsize::new(0);
static TIP: Mutex<String> = Mutex::new(String::new());

fn hwnd() -> Option<HWND> {
    let h = HWND_RAW.load(Ordering::Acquire);
    (h != 0).then_some(HWND(h as *mut _))
}

fn load_icon(size: i32) -> Option<HICON> {
    // SAFETY: loading resource id 1 (embedded by build.rs) from our module.
    unsafe {
        let module = GetModuleHandleW(None).ok()?;
        let h = LoadImageW(
            Some(module.into()),
            // MAKEINTRESOURCEW(1): the id travels in the pointer value.
            PCWSTR(std::ptr::without_provenance::<u16>(1)),
            IMAGE_ICON,
            size,
            size,
            LR_DEFAULTCOLOR,
        )
        .ok()?;
        Some(HICON(h.0))
    }
}

fn copy_wide(dst: &mut [u16], s: &str) {
    let n = dst.len().saturating_sub(1);
    let mut i = 0;
    for c in s.encode_utf16().take(n) {
        dst[i] = c;
        i += 1;
    }
    dst[i] = 0;
}

fn base_nid(h: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: u32::try_from(std::mem::size_of::<NOTIFYICONDATAW>()).unwrap_or(0),
        hWnd: h,
        uID: 1,
        ..Default::default()
    }
}

fn add_icon(h: HWND) {
    let mut nid = base_nid(h);
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    nid.uCallbackMessage = WM_TRAY;
    // SAFETY: plain metric query.
    nid.hIcon = load_icon(unsafe { GetSystemMetrics(SM_CXSMICON) }).unwrap_or_default();
    copy_wide(
        &mut nid.szTip,
        &TIP.lock().map(|t| t.clone()).unwrap_or_default(),
    );
    // SAFETY: `nid` is fully initialized.
    unsafe {
        let _ = Shell_NotifyIconW(NIM_ADD, &nid);
    }
}

/// Attach to the main window: icon, subclass, tray icon. Call once, on the
/// UI thread, after the window exists.
pub fn install(h: HWND, tip: &str) {
    HWND_RAW.store(h.0 as isize, Ordering::Release);
    if let Ok(mut t) = TIP.lock() {
        tip.clone_into(&mut t);
    }
    // SAFETY: Win32 calls on our own window from its owning thread.
    unsafe {
        TASKBAR_CREATED.store(
            RegisterWindowMessageW(w!("TaskbarCreated")) as isize,
            Ordering::Relaxed,
        );
        if let Some(big) = load_icon(GetSystemMetrics(SM_CXICON)) {
            SendMessageW(
                h,
                WM_SETICON,
                Some(WPARAM(ICON_BIG as usize)),
                Some(LPARAM(big.0 as isize)),
            );
        }
        if let Some(small) = load_icon(GetSystemMetrics(SM_CXSMICON)) {
            SendMessageW(
                h,
                WM_SETICON,
                Some(WPARAM(ICON_SMALL as usize)),
                Some(LPARAM(small.0 as isize)),
            );
        }
        let _ = SetWindowSubclass(h, Some(subclass_proc), SUBCLASS_ID, 0);
    }
    add_icon(h);
}

pub fn remove() {
    if let Some(h) = hwnd() {
        let nid = base_nid(h);
        // SAFETY: deleting our own icon.
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
        }
    }
}

pub fn set_tooltip(tip: &str) {
    let changed = TIP.lock().map(|mut t| {
        if *t == tip {
            false
        } else {
            tip.clone_into(&mut t);
            true
        }
    });
    if !matches!(changed, Ok(true)) {
        return;
    }
    if let Some(h) = hwnd() {
        let mut nid = base_nid(h);
        nid.uFlags = NIF_TIP;
        copy_wide(&mut nid.szTip, tip);
        // SAFETY: modifying our own icon.
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }
}

pub fn set_menu_state(paused: bool, has_id: bool) {
    PAUSED.store(paused, Ordering::Relaxed);
    HAS_ID.store(has_id, Ordering::Relaxed);
}

/// A notification-area balloon (a toast on Windows 10/11).
pub fn balloon(title: &str, text: &str) {
    let Some(h) = hwnd() else { return };
    let mut nid = base_nid(h);
    nid.uFlags = NIF_INFO;
    copy_wide(&mut nid.szInfoTitle, title);
    copy_wide(&mut nid.szInfo, text);
    // SAFETY: plain metric query / icon load.
    let large = load_icon(unsafe { GetSystemMetrics(SM_CXICON) });
    match large {
        Some(icon) => {
            nid.dwInfoFlags = NIIF_USER | NIIF_LARGE_ICON;
            nid.hBalloonIcon = icon;
        }
        None => nid.dwInfoFlags = NIIF_INFO,
    }
    // SAFETY: modifying our own icon.
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

pub fn set_dark_title(dark: bool) {
    let Some(h) = hwnd() else { return };
    let v: i32 = i32::from(dark);
    // SAFETY: 4-byte BOOL attribute on our own window.
    unsafe {
        let _ = DwmSetWindowAttribute(h, DWMWA_USE_IMMERSIVE_DARK_MODE, (&raw const v).cast(), 4);
    }
}

pub fn show_window() {
    let Some(h) = hwnd() else { return };
    // SAFETY: showing our own window.
    unsafe {
        if IsIconic(h).as_bool() {
            let _ = ShowWindow(h, SW_RESTORE);
        } else {
            let _ = ShowWindow(h, SW_SHOW);
        }
        let _ = SetForegroundWindow(h);
    }
    post(Event::Shown);
}

pub fn hide_window() {
    if let Some(h) = hwnd() {
        // SAFETY: hiding our own window.
        unsafe {
            let _ = ShowWindow(h, SW_HIDE);
        }
    }
}

/// Close for real: the subclass lets WM_CLOSE through to winit, which ends
/// the event loop.
pub fn quit() {
    QUITTING.store(true, Ordering::Release);
    if let Some(h) = hwnd() {
        // SAFETY: posting to our own window.
        unsafe {
            let _ = PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
}

/// Wake the window from another thread (second-instance request).
pub fn post_show_request() {
    if let Some(h) = hwnd() {
        // SAFETY: posting to our own window.
        unsafe {
            let _ = PostMessageW(Some(h), WM_SHOW_REQUEST, WPARAM(0), LPARAM(0));
        }
    }
}

fn tray_menu(h: HWND) {
    // SAFETY: a transient popup menu owned and destroyed here.
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let paused = PAUSED.load(Ordering::Relaxed);
        let _ = AppendMenuW(menu, MF_STRING, ID_OPEN, w!("Open OpenView"));
        let copy_flags = if HAS_ID.load(Ordering::Relaxed) {
            MF_STRING
        } else {
            MF_STRING | MF_GRAYED
        };
        let _ = AppendMenuW(menu, copy_flags, ID_COPY, w!("Copy ID"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let pause_flags = if paused {
            MF_STRING | MF_CHECKED
        } else {
            MF_STRING
        };
        let _ = AppendMenuW(menu, pause_flags, ID_PAUSE, w!("Pause remote access"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, ID_QUIT, w!("Quit OpenView"));
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // Required so the menu closes when the user clicks elsewhere.
        let _ = SetForegroundWindow(h);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
            pt.x,
            pt.y,
            None,
            h,
            None,
        );
        let _ = PostMessageW(Some(h), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        let cmd = match usize::try_from(cmd.0).unwrap_or(0) {
            ID_OPEN => Some(TrayCmd::Open),
            ID_COPY => Some(TrayCmd::CopyId),
            ID_PAUSE => Some(TrayCmd::TogglePause),
            ID_QUIT => Some(TrayCmd::Quit),
            _ => None,
        };
        if let Some(c) = cmd {
            if c == TrayCmd::Open {
                show_window();
            }
            post(Event::Tray(c));
        }
    }
}

unsafe extern "system" fn subclass_proc(
    h: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    match msg {
        WM_CLOSE if !QUITTING.load(Ordering::Acquire) => {
            // Close button = hide to tray; the host keeps running.
            // SAFETY: hiding our own window.
            unsafe {
                let _ = ShowWindow(h, SW_HIDE);
            }
            if !HINT_SHOWN.swap(true, Ordering::Relaxed) {
                balloon(
                    "OpenView is still running",
                    "This computer stays reachable. Right-click the tray icon to quit.",
                );
            }
            return LRESULT(0);
        }
        WM_TRAY => {
            let ev = u32::try_from(lp.0 & 0xFFFF).unwrap_or(0);
            match ev {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK | NIN_BALLOONUSERCLICK => show_window(),
                WM_RBUTTONUP => tray_menu(h),
                _ => {}
            }
            return LRESULT(0);
        }
        WM_SHOW_REQUEST => {
            show_window();
            return LRESULT(0);
        }
        WM_SETTINGCHANGE => {
            if lp.0 != 0 {
                // SAFETY: for WM_SETTINGCHANGE, lParam is a NUL-terminated
                // UTF-16 string (or null, handled above).
                let s = unsafe { PCWSTR(lp.0 as *const u16).to_string() }.unwrap_or_default();
                if s == "ImmersiveColorSet" {
                    post(Event::ThemeChanged);
                }
            }
        }
        _ => {
            let tc = TASKBAR_CREATED.load(Ordering::Relaxed);
            if tc != 0 && msg as isize == tc {
                add_icon(h);
            }
        }
    }
    // SAFETY: forwarding to the next window procedure (winit's).
    unsafe { DefSubclassProc(h, msg, wp, lp) }
}
