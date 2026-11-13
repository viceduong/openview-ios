//! The Win32 side of input injection: `SendInput`, `GetCursorPos`,
//! `GetSystemMetrics`, and `SendSAS` (resolved from sas.dll at run time).

use ovapp::terminal::MouseButton;
use windows::core::{s, w};
use windows::Win32::Foundation::POINT;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MapVirtualKeyW, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYBD_EVENT_FLAGS, MAPVK_VK_TO_VSC, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
    MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, MOUSE_EVENT_FLAGS,
    VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, XBUTTON1, XBUTTON2,
};

use super::{keyflags, InputOp};

/// `(x, y, w, h)` of the virtual screen, as `GetSystemMetrics` reports it.
pub(super) fn virtual_screen_metrics() -> (i32, i32, i32, i32) {
    // SAFETY: GetSystemMetrics only reads a system value; any index is
    // valid (unknown ones return 0).
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

/// Inject `ops` as one `SendInput` batch; `false` unless all were injected.
pub(super) fn send(ops: &[InputOp]) -> bool {
    let inputs: Vec<INPUT> = ops.iter().map(to_input).collect();
    // SAFETY: `inputs` is a fully initialized slice of INPUTs and `cbsize`
    // is the size of one element, as SendInput requires.
    let n = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    n as usize == inputs.len()
}

/// The cursor's desktop position.
pub(super) fn cursor_pos() -> Option<(i32, i32)> {
    let mut p = POINT::default();
    // SAFETY: `p` is a valid, writable POINT.
    unsafe { GetCursorPos(&mut p) }
        .is_ok()
        .then_some((p.x, p.y))
}

fn mouse(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn key(vk: u16, scan: u16, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: scan,
                dwFlags: KEYBD_EVENT_FLAGS(flags),
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn to_input(op: &InputOp) -> INPUT {
    match *op {
        InputOp::MoveAbs { nx, ny } => mouse(
            nx,
            ny,
            0,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        ),
        InputOp::MoveRaw { dx, dy } => mouse(dx, dy, 0, MOUSEEVENTF_MOVE),
        InputOp::Button { button, down } => {
            let (d, u, data) = match button {
                MouseButton::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, 0),
                MouseButton::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, 0),
                MouseButton::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, 0),
                MouseButton::X1 => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, u32::from(XBUTTON1)),
                MouseButton::X2 => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, u32::from(XBUTTON2)),
            };
            mouse(0, 0, data, if down { d } else { u })
        }
        // mouseData is a DWORD holding a signed delta.
        InputOp::Wheel(d) => mouse(0, 0, d as u32, MOUSEEVENTF_WHEEL),
        InputOp::HWheel(d) => mouse(0, 0, d as u32, MOUSEEVENTF_HWHEEL),
        InputOp::Key { vk, scan, flags } => {
            // Fill in the scancode apps (games, RDP clients) read when
            // the viewer did not send one.
            let scan = if scan == 0 && vk != 0 {
                // SAFETY: a pure table lookup with no pointers involved.
                unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_VSC) as u16 }
            } else {
                scan
            };
            key(vk, scan, flags)
        }
        InputOp::Unicode { unit, up } => key(
            0,
            unit,
            keyflags::UNICODE | if up { keyflags::KEYUP } else { 0 },
        ),
    }
}

/// `VOID SendSAS(BOOL AsUser)` (sas.h).
type SendSasFn = unsafe extern "system" fn(as_user: i32);

/// Resolve `SendSAS` from sas.dll (`None` when either is missing). The
/// module is never freed, so the pointer stays valid for the process.
fn send_sas_fn() -> Option<SendSasFn> {
    // SAFETY: LoadLibraryW / GetProcAddress with static NUL-terminated
    // names; the module stays loaded for the process lifetime.
    let f = unsafe {
        let module = LoadLibraryW(w!("sas.dll")).ok()?;
        GetProcAddress(module, s!("SendSAS"))?
    };
    // SAFETY: the export named `SendSAS` has the documented signature
    // `VOID WINAPI SendSAS(BOOL)`, which is what `SendSasFn` declares; both
    // are plain function pointers of the same size.
    Some(unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, SendSasFn>(f) })
}

/// Whether sas.dll exports `SendSAS`.
pub(super) fn sas_available() -> bool {
    send_sas_fn().is_some()
}

/// Generate the secure attention sequence (`as_user = false`: the service
/// form). Errors when `SendSAS` is not present.
pub(super) fn send_sas(as_user: bool) -> Result<(), String> {
    let f = send_sas_fn().ok_or("SendSAS not present")?;
    // SAFETY: `f` is sas.dll's `SendSAS` (see `send_sas_fn`), called with
    // its one BOOL argument.
    unsafe { f(i32::from(as_user)) };
    Ok(())
}

/// `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System`
/// `SoftwareSASGeneration` (DWORD), if set.
pub(super) fn sas_policy() -> Option<u32> {
    let mut data: u32 = 0;
    let mut len: u32 = 4;
    // SAFETY: static NUL-terminated key/value names, and a 4-byte out
    // buffer whose size is passed in `len`.
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Policies\\System"),
            w!("SoftwareSASGeneration"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut data as *mut u32).cast()),
            Some(&mut len),
        )
    };
    rc.is_ok().then_some(data)
}

/// The Terminal Services session of this process (0 = services).
pub(super) fn session_id() -> Option<u32> {
    let mut sid = 0u32;
    // SAFETY: `sid` is a valid out-pointer; GetCurrentProcessId has no
    // preconditions.
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut sid) }
        .is_ok()
        .then_some(sid)
}
