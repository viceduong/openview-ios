//! Remote input injection via Win32 `SendInput`.
//!
//! The work splits into three layers:
//!
//! 1. **Pure mapping** ([`geometry`], unit-tested everywhere): a normalized
//!    POINTER point (0..=65535 in the viewed frame) becomes a desktop pixel
//!    inside the captured [`Region`], and a desktop pixel becomes a
//!    `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK` coordinate spanning
//!    the whole [`VirtualScreen`] — negative origins (a monitor left of /
//!    above the primary) included.
//! 2. **[`InputController`]** (pure, unit-tested): turns wire commands —
//!    legacy `POINTER`/`KEY` and the input-v2 set (`POINTER_EX`, `WHEEL`,
//!    `KEY_EX`, `TEXT`, `RELEASE_ALL`, `SAS`) — into [`InputOp`]s, and keeps
//!    the bookkeeping of every key and button it pressed so they can all be
//!    released when the viewer asks, when the session ends, or after
//!    [`STUCK_RELEASE_AFTER`] of silence while something is held.
//! 3. **[`InputBackend`]s**: [`SendInputBackend`] performs the ops
//!    (`SendInput`, `GetCursorPos`, `SendSAS`); [`DryRunBackend`] only logs
//!    them (`ovhost --dry-input`, smoke tests on a live desktop).
//!
//! Off Windows the real backend is a no-op, so the crate compiles
//! unchanged.

use std::time::Duration;

use ovapp::terminal::MouseButton;

mod backend;
mod controller;
mod geometry;
#[cfg(windows)]
mod win32;

#[cfg(test)]
mod tests;

pub(crate) use backend::{DryRunBackend, SendInputBackend};
pub(crate) use controller::InputController;
pub(crate) use geometry::{Region, VirtualScreen};

/// `KEYBDINPUT.dwFlags` bits (winuser.h), kept outside the Win32 module so
/// the pure [`InputOp`] mapping is testable everywhere.
pub(crate) mod keyflags {
    pub(crate) const EXTENDEDKEY: u32 = 0x0001;
    pub(crate) const KEYUP: u32 = 0x0002;
    pub(crate) const UNICODE: u32 = 0x0004;
    pub(crate) const SCANCODE: u32 = 0x0008;
}

/// Virtual-key codes the controller itself needs.
mod vk {
    pub(super) const TAB: u16 = 0x09;
    pub(super) const RETURN: u16 = 0x0D;
    pub(super) const CONTROL: u16 = 0x11;
    pub(super) const MENU: u16 = 0x12;
    pub(super) const DELETE: u16 = 0x2E;
}

/// Whether a virtual key is an *extended* key (E0-prefixed scancode):
/// without `KEYEVENTF_EXTENDEDKEY` the arrows/Ins/Del/Home/End/PgUp/PgDn
/// arrive as their numeric-keypad twins (wrong with NumLock on), right
/// Ctrl/Alt as the left ones, and so on.
#[must_use]
fn is_extended_vk(vk: u16) -> bool {
    matches!(
        vk,
        0x21..=0x28 // PgUp PgDn End Home Left Up Right Down
            | 0x2C // PrintScreen
            | 0x2D | 0x2E // Insert Delete
            | 0x5B | 0x5C | 0x5D // LWin RWin Apps
            | 0x6F // Numpad divide
            | 0x90 // NumLock
            | 0xA3 | 0xA5 // RControl RMenu
            | 0xA6..=0xB7 // browser / volume / media keys
    )
}

/// One primitive injection, in `SendInput` terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputOp {
    /// Absolute move, VIRTUALDESK-normalized (see
    /// [`geometry::to_virtual_desk`]).
    MoveAbs { nx: i32, ny: i32 },
    /// Raw relative motion (`MOUSEEVENTF_MOVE`, host acceleration applies).
    MoveRaw { dx: i32, dy: i32 },
    /// Press / release a mouse button at the current cursor.
    Button { button: MouseButton, down: bool },
    /// Vertical wheel, 1/120 units (positive = away from the user).
    Wheel(i32),
    /// Horizontal wheel, 1/120 units (positive = right).
    HWheel(i32),
    /// A keyboard event (`flags` = [`keyflags`] bits).
    Key { vk: u16, scan: u16, flags: u32 },
    /// One UTF-16 code unit typed via `KEYEVENTF_UNICODE`.
    Unicode { unit: u16, up: bool },
}

/// What performs the [`InputOp`]s.
pub(crate) trait InputBackend: Send {
    /// Inject `ops` in order (one `SendInput` batch). `false` when the OS
    /// refused (blocked desktop, UIPI, ...).
    fn send(&mut self, ops: &[InputOp]) -> bool;
    /// The cursor's desktop position (relative moves are applied to it).
    fn cursor_pos(&mut self) -> Option<(i32, i32)>;
    /// Trigger the secure attention sequence. `Ok(how)` when it was
    /// generated, `Err(why)` when this process cannot (the caller falls
    /// back to plain keystrokes).
    fn send_sas(&mut self) -> Result<&'static str, String>;
}

/// How long something may stay held without any input before the host
/// releases it (a viewer that vanished mid-drag must not leave a button or
/// Ctrl stuck down on the host).
pub(crate) const STUCK_RELEASE_AFTER: Duration = Duration::from_secs(30);
