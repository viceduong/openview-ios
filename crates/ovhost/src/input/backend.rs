//! The [`InputBackend`]s: the real `SendInput` one and the logging dry run.

use super::geometry::{from_virtual_desk, VirtualScreen};
use super::{keyflags, InputBackend, InputOp};

/// A human-readable line per op (dry-run log).
#[must_use]
fn describe(op: &InputOp, vs: &VirtualScreen) -> String {
    match *op {
        InputOp::MoveAbs { nx, ny } => {
            let (px, py) = from_virtual_desk(nx, ny, vs);
            format!("mouse move abs ({nx},{ny}) virtualdesk -> pixel ({px},{py})")
        }
        InputOp::MoveRaw { dx, dy } => format!("mouse move rel ({dx},{dy})"),
        InputOp::Button { button, down } => {
            format!("mouse {button:?} {}", if down { "down" } else { "up" })
        }
        InputOp::Wheel(d) => format!("wheel {d}"),
        InputOp::HWheel(d) => format!("hwheel {d}"),
        InputOp::Key { vk, scan, flags } => format!(
            "key vk=0x{vk:02x} scan=0x{scan:02x}{}{} {}",
            if flags & keyflags::EXTENDEDKEY != 0 {
                " ext"
            } else {
                ""
            },
            if flags & keyflags::SCANCODE != 0 {
                " scancode"
            } else {
                ""
            },
            if flags & keyflags::KEYUP != 0 {
                "up"
            } else {
                "down"
            }
        ),
        InputOp::Unicode { unit, up } => {
            format!("unicode U+{unit:04X} {}", if up { "up" } else { "down" })
        }
    }
}

/// Logs every op instead of injecting it (`ovhost --dry-input`). Tracks a
/// virtual cursor so relative moves resolve like on a real desktop.
pub(crate) struct DryRunBackend {
    vs: VirtualScreen,
    cursor: (i32, i32),
}

impl DryRunBackend {
    pub(crate) fn new(vs: VirtualScreen) -> Self {
        Self {
            vs,
            cursor: (vs.x + vs.w as i32 / 2, vs.y + vs.h as i32 / 2),
        }
    }
}

impl InputBackend for DryRunBackend {
    fn send(&mut self, ops: &[InputOp]) -> bool {
        for op in ops {
            match *op {
                InputOp::MoveAbs { nx, ny } => self.cursor = from_virtual_desk(nx, ny, &self.vs),
                InputOp::MoveRaw { dx, dy } => {
                    self.cursor = (
                        self.cursor.0.saturating_add(dx),
                        self.cursor.1.saturating_add(dy),
                    )
                }
                _ => {}
            }
            tracing::debug!("[dry-input] SendInput {}", describe(op, &self.vs));
        }
        true
    }

    fn cursor_pos(&mut self) -> Option<(i32, i32)> {
        Some(self.cursor)
    }

    fn send_sas(&mut self) -> Result<&'static str, String> {
        let how = sas_capability()?;
        tracing::info!("[dry-input] would call {how}");
        Ok("SendSAS (dry run)")
    }
}

/// The real backend: `SendInput` / `GetCursorPos` / `SendSAS`.
#[derive(Default)]
pub(crate) struct SendInputBackend;

impl InputBackend for SendInputBackend {
    fn send(&mut self, ops: &[InputOp]) -> bool {
        #[cfg(windows)]
        {
            super::win32::send(ops)
        }
        #[cfg(not(windows))]
        {
            let _ = ops;
            true // no-op stub: nothing to inject off-Windows
        }
    }

    fn cursor_pos(&mut self) -> Option<(i32, i32)> {
        #[cfg(windows)]
        {
            super::win32::cursor_pos()
        }
        #[cfg(not(windows))]
        {
            None
        }
    }

    fn send_sas(&mut self) -> Result<&'static str, String> {
        let how = sas_capability()?;
        // `SendSAS(FALSE)` is the service form.
        #[cfg(windows)]
        {
            super::win32::send_sas(false)?;
        }
        Ok(how)
    }
}

/// Whether this process can generate a real SAS: `SendSAS` must exist
/// (sas.dll) and we must run as a service (session 0) with the
/// `SoftwareSASGeneration` policy allowing services (1 or 3). Returns the
/// method or the reason it is unavailable.
fn sas_capability() -> Result<&'static str, String> {
    #[cfg(windows)]
    {
        use super::win32;
        if !win32::sas_available() {
            return Err("sas.dll / SendSAS not present".into());
        }
        let policy = win32::sas_policy();
        let session = win32::session_id();
        if session != Some(0) {
            return Err(format!(
                "not running as a service (session {}), SoftwareSASGeneration={}",
                session.map_or_else(|| "?".into(), |s| s.to_string()),
                policy.map_or_else(|| "unset".into(), |p| p.to_string())
            ));
        }
        match policy {
            Some(1) | Some(3) => Ok("SendSAS (service)"),
            other => Err(format!(
                "SoftwareSASGeneration={} does not allow services (needs 1 or 3)",
                other.map_or_else(|| "unset".into(), |p| p.to_string())
            )),
        }
    }
    #[cfg(not(windows))]
    {
        Err("no SAS on this platform".into())
    }
}
