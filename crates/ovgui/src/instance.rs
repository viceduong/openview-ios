//! Single instance: a named mutex. A second launch signals the running
//! instance through named events, then exits:
//!   `OpenView.exe`         -> the running instance shows its window
//!   `OpenView.exe --quit`  -> the running instance quits (stops its host)

use windows::core::HSTRING;
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, OpenEventW, SetEvent, WaitForMultipleObjects, EVENT_MODIFY_STATE,
    INFINITE,
};

use crate::events::{post, Event, TrayCmd};

/// Held for the life of the process.
pub struct Guard(#[allow(dead_code)] HANDLE);

/// `Some(guard)` when we are the first instance (and not asked to quit).
/// Otherwise the running instance has been signalled; the caller exits.
pub fn acquire(name: &str, quit: bool) -> Option<Guard> {
    let mutex_name = HSTRING::from(format!("Local\\{name}.Instance"));
    let show_name = HSTRING::from(format!("Local\\{name}.Show"));
    let quit_name = HSTRING::from(format!("Local\\{name}.Quit"));
    // SAFETY: named kernel objects; handles intentionally live until exit.
    unsafe {
        let m = CreateMutexW(None, true, &mutex_name).ok()?;
        if GetLastError() == ERROR_ALREADY_EXISTS || quit {
            let target = if quit { &quit_name } else { &show_name };
            if let Ok(ev) = OpenEventW(EVENT_MODIFY_STATE, false, target) {
                let _ = SetEvent(ev);
            }
            return None;
        }
        let show = CreateEventW(None, false, false, &show_name).ok();
        let quit_ev = CreateEventW(None, false, false, &quit_name).ok();
        if let (Some(show), Some(quit_ev)) = (show, quit_ev) {
            let raw = [show.0 as isize, quit_ev.0 as isize];
            std::thread::spawn(move || loop {
                let handles = [HANDLE(raw[0] as *mut _), HANDLE(raw[1] as *mut _)];
                let r = WaitForMultipleObjects(&handles, false, INFINITE);
                if r == WAIT_OBJECT_0 {
                    crate::shell::post_show_request();
                } else if r.0 == WAIT_OBJECT_0.0 + 1 {
                    post(Event::Tray(TrayCmd::Quit));
                } else {
                    break;
                }
            });
        }
        Some(Guard(m))
    }
}
