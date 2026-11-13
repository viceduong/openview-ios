//! Remote input injection via Win32 `SendInput` (spec v0.11 "remote control",
//! input v2).
//!
//! Like [`crate::capture`], this module declares the handful of Win32
//! functions and structs it needs with raw `extern "system"` FFI — no
//! `windows`/`winapi` crates. The work splits into three layers:
//!
//! 1. **Pure mapping** (unit-tested everywhere): a normalized POINTER point
//!    (0..=65535 in the viewed frame) becomes a desktop pixel inside the
//!    captured [`Region`] ([`map_to_region`]), and a desktop pixel becomes a
//!    `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK` coordinate spanning
//!    the whole [`VirtualScreen`] — negative origins (a monitor left of /
//!    above the primary) included ([`to_virtual_desk`]).
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
//! Off Windows the real backend is a no-op, so the crate (and the iOS
//! staticlib build of its dependencies) compiles unchanged.

use std::time::{Duration, Instant};

use ovapp::terminal::{
    Command, MouseButton, PointerAction, PointerKind, KEY_DOWN, KEY_EXTENDED, POINTER_HAS_POS,
    POINTER_RAW,
};

/// A captured screen region: `(x, y)` origin on the desktop plus `w x h` size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    /// Left edge of the region on the desktop.
    pub x: i32,
    /// Top edge of the region on the desktop.
    pub y: i32,
    /// Region width in pixels.
    pub w: u32,
    /// Region height in pixels.
    pub h: u32,
}

/// The virtual screen (all monitors): origin `(x, y)` and size `(w, h)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualScreen {
    /// Left edge of the virtual screen (can be negative with side monitors).
    pub x: i32,
    /// Top edge of the virtual screen.
    pub y: i32,
    /// Total width of the virtual screen.
    pub w: u32,
    /// Total height of the virtual screen.
    pub h: u32,
}

impl VirtualScreen {
    /// Query the desktop's virtual-screen geometry via `GetSystemMetrics`
    /// (`SM_XVIRTUALSCREEN`/`SM_YVIRTUALSCREEN`/`SM_CXVIRTUALSCREEN`/
    /// `SM_CYVIRTUALSCREEN`). Falls back to a 1x1 screen when the metrics
    /// report nothing usable.
    #[must_use]
    pub fn query() -> Self {
        #[cfg(windows)]
        {
            let (x, y, w, h) = unsafe {
                (
                    ffi::GetSystemMetrics(ffi::SM_XVIRTUALSCREEN),
                    ffi::GetSystemMetrics(ffi::SM_YVIRTUALSCREEN),
                    ffi::GetSystemMetrics(ffi::SM_CXVIRTUALSCREEN),
                    ffi::GetSystemMetrics(ffi::SM_CYVIRTUALSCREEN),
                )
            };
            if w > 0 && h > 0 {
                Self {
                    x,
                    y,
                    w: w as u32,
                    h: h as u32,
                }
            } else {
                Self::primary(w, h)
            }
        }
        #[cfg(not(windows))]
        {
            Self::primary(0, 0)
        }
    }

    /// A fallback virtual screen from (possibly invalid) primary-monitor
    /// metrics: degenerate sizes collapse to a 1x1 screen so the mapping math
    /// never divides by zero.
    #[must_use]
    pub fn primary(w: i32, h: i32) -> Self {
        Self {
            x: 0,
            y: 0,
            w: w.max(1) as u32,
            h: h.max(1) as u32,
        }
    }
}

/// Map a normalized POINTER point (0..=65535 in the viewed frame, 65535 =
/// the last pixel) to the desktop pixel it designates inside `region`
/// (`region.x + round(x * (w - 1) / 65535)`, always inside the region).
#[must_use]
pub fn map_to_region(x: u32, y: u32, region: &Region) -> (i32, i32) {
    let axis = |n: u32, origin: i32, span: u32| -> i32 {
        let last = i64::from(span.max(1) - 1);
        let n = i64::from(n.min(65_535));
        let off = (n * last + 32_767) / 65_535;
        (i64::from(origin) + off) as i32
    };
    (axis(x, region.x, region.w), axis(y, region.y, region.h))
}

/// Map a desktop pixel to `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`
/// coordinates (0..=65535 across the virtual screen, whose origin may be
/// negative). Windows maps such a coordinate `n` back to pixel
/// `origin + floor(n * span / 65536)`; this picks the smallest `n` that lands
/// exactly on the requested pixel (`ceil(off * 65536 / span)`). Pixels off
/// the virtual screen clamp to its edge.
#[must_use]
pub fn to_virtual_desk(px: i32, py: i32, vs: &VirtualScreen) -> (i32, i32) {
    let axis = |p: i32, origin: i32, span: u32| -> i32 {
        let span = i64::from(span.max(1));
        let off = (i64::from(p) - i64::from(origin)).clamp(0, span - 1);
        ((off * 65_536 + span - 1) / span).min(65_535) as i32
    };
    (axis(px, vs.x, vs.w), axis(py, vs.y, vs.h))
}

/// The pixel Windows lands on for a VIRTUALDESK coordinate (inverse of
/// [`to_virtual_desk`]; used by the dry-run backend and the tests).
#[must_use]
pub fn from_virtual_desk(nx: i32, ny: i32, vs: &VirtualScreen) -> (i32, i32) {
    let axis = |n: i32, origin: i32, span: u32| -> i32 {
        (i64::from(origin) + i64::from(n.clamp(0, 65_535)) * i64::from(span.max(1)) / 65_536) as i32
    };
    (axis(nx, vs.x, vs.w), axis(ny, vs.y, vs.h))
}

/// Map a normalized POINTER point straight to VIRTUALDESK coordinates
/// ([`map_to_region`] then [`to_virtual_desk`]).
#[must_use]
pub fn map_to_absolute(
    x: u32,
    y: u32,
    region: &Region,
    virtual_screen: &VirtualScreen,
) -> (i32, i32) {
    let (px, py) = map_to_region(x, y, region);
    to_virtual_desk(px, py, virtual_screen)
}

/// `KEYBDINPUT.dwFlags` bits (winuser.h), kept outside the FFI module so the
/// pure [`InputOp`] mapping is testable everywhere.
pub mod keyflags {
    pub const EXTENDEDKEY: u32 = 0x0001;
    pub const KEYUP: u32 = 0x0002;
    pub const UNICODE: u32 = 0x0004;
    pub const SCANCODE: u32 = 0x0008;
}

/// Virtual-key codes the controller itself needs.
pub mod vk {
    pub const TAB: u16 = 0x09;
    pub const RETURN: u16 = 0x0D;
    pub const CONTROL: u16 = 0x11;
    pub const MENU: u16 = 0x12;
    pub const DELETE: u16 = 0x2E;
}

/// Whether a virtual key is an *extended* key (E0-prefixed scancode):
/// without `KEYEVENTF_EXTENDEDKEY` the arrows/Ins/Del/Home/End/PgUp/PgDn
/// arrive as their numeric-keypad twins (wrong with NumLock on), right
/// Ctrl/Alt as the left ones, and so on.
#[must_use]
pub fn is_extended_vk(vk: u16) -> bool {
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
pub enum InputOp {
    /// Absolute move, VIRTUALDESK-normalized (see [`to_virtual_desk`]).
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
pub trait InputBackend: Send {
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
pub const STUCK_RELEASE_AFTER: Duration = Duration::from_secs(30);

/// A key the controller pressed and has not released yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HeldKey {
    vk: u16,
    scan: u16,
    flags: u32,
}

impl HeldKey {
    /// Bookkeeping identity: the vk, or the scancode when there is none.
    fn id(&self) -> u32 {
        if self.vk != 0 {
            u32::from(self.vk)
        } else {
            0x1_0000 | u32::from(self.scan) | ((self.flags & keyflags::EXTENDEDKEY) << 16)
        }
    }
}

/// Wire commands → [`InputOp`]s, with release-all bookkeeping.
pub struct InputController {
    backend: Box<dyn InputBackend>,
    region: Region,
    vs: VirtualScreen,
    keys: Vec<HeldKey>,
    buttons: Vec<MouseButton>,
    last_input: Option<Instant>,
    /// Ops performed so far (stats / tests).
    pub ops_sent: u64,
}

impl InputController {
    pub fn new(backend: Box<dyn InputBackend>, region: Region, vs: VirtualScreen) -> Self {
        Self {
            backend,
            region,
            vs,
            keys: Vec::new(),
            buttons: Vec::new(),
            last_input: None,
            ops_sent: 0,
        }
    }

    /// The captured region changed (CAPTURE_REGION retarget).
    pub fn set_region(&mut self, region: Region) {
        self.region = region;
    }

    /// The virtual screen changed (monitor hot-plug).
    pub fn set_virtual_screen(&mut self, vs: VirtualScreen) {
        self.vs = vs;
    }

    /// Whether any key or button is currently held for the viewer.
    pub fn holding(&self) -> bool {
        !self.keys.is_empty() || !self.buttons.is_empty()
    }

    /// Handle one inbound command. Returns `None` for non-input commands,
    /// `Some(ok)` for input (`ok = false`: the OS refused an injection).
    pub fn handle(&mut self, cmd: &Command, now: Instant) -> Option<bool> {
        let ops = match cmd {
            Command::Pointer { kind, x, y } => {
                let (nx, ny) = map_to_absolute(*x, *y, &self.region, &self.vs);
                let mut ops = vec![InputOp::MoveAbs { nx, ny }];
                match kind {
                    PointerKind::Move => {}
                    PointerKind::Down => ops.push(self.button(MouseButton::Left, true)),
                    PointerKind::Up => ops.push(self.button(MouseButton::Left, false)),
                }
                ops
            }
            Command::PointerEx {
                action,
                button,
                flags,
                x,
                y,
            } => {
                let abs = |s: &Self| {
                    let (nx, ny) = map_to_absolute(
                        (*x).clamp(0, 65_535) as u32,
                        (*y).clamp(0, 65_535) as u32,
                        &s.region,
                        &s.vs,
                    );
                    InputOp::MoveAbs { nx, ny }
                };
                match action {
                    PointerAction::MoveAbs => vec![abs(self)],
                    PointerAction::MoveRel => vec![self.relative(*x, *y, flags & POINTER_RAW != 0)],
                    PointerAction::Down | PointerAction::Up => {
                        let mut ops = Vec::with_capacity(2);
                        if flags & POINTER_HAS_POS != 0 {
                            ops.push(abs(self));
                        }
                        ops.push(self.button(*button, *action == PointerAction::Down));
                        ops
                    }
                }
            }
            Command::Wheel { dy, dx } => {
                let mut ops = Vec::with_capacity(2);
                if *dy != 0 {
                    ops.push(InputOp::Wheel(i32::from(*dy)));
                }
                if *dx != 0 {
                    ops.push(InputOp::HWheel(i32::from(*dx)));
                }
                ops
            }
            Command::Key { vk, down } => vec![self.key(*vk, 0, is_extended_vk(*vk), *down)],
            Command::KeyEx { vk, scan, flags } => {
                // A scancode-less event's extended bit is derived from the vk
                // (the viewer may not know it); otherwise the viewer's flag
                // is authoritative.
                let ext = if *scan == 0 {
                    flags & KEY_EXTENDED != 0 || is_extended_vk(*vk)
                } else {
                    flags & KEY_EXTENDED != 0
                };
                vec![self.key(*vk, *scan, ext, flags & KEY_DOWN != 0)]
            }
            Command::Text(text) => text_ops(text),
            Command::ReleaseAll => {
                self.last_input = Some(now);
                let n = self.release_all();
                return Some(n.is_some());
            }
            Command::Sas => {
                self.last_input = Some(now);
                return Some(self.sas());
            }
            _ => return None,
        };
        self.last_input = Some(now);
        Some(self.perform(&ops))
    }

    /// Release every key and button pressed for the viewer (RELEASE_ALL,
    /// session end, stuck-input timeout). Returns how many were released
    /// (`None` = the OS refused the batch).
    pub fn release_all(&mut self) -> Option<usize> {
        let mut ops = Vec::with_capacity(self.keys.len() + self.buttons.len());
        for b in std::mem::take(&mut self.buttons) {
            ops.push(InputOp::Button {
                button: b,
                down: false,
            });
        }
        // Reverse press order: modifiers pressed first are released last.
        for k in std::mem::take(&mut self.keys).into_iter().rev() {
            ops.push(InputOp::Key {
                vk: k.vk,
                scan: k.scan,
                flags: k.flags | keyflags::KEYUP,
            });
        }
        let n = ops.len();
        if n == 0 {
            return Some(0);
        }
        self.perform(&ops).then_some(n)
    }

    /// Periodic check: something held and no input for
    /// [`STUCK_RELEASE_AFTER`] → release it. Returns the number released.
    pub fn tick(&mut self, now: Instant) -> usize {
        if !self.holding() {
            return 0;
        }
        let silent = self
            .last_input
            .is_none_or(|t| now.saturating_duration_since(t) >= STUCK_RELEASE_AFTER);
        if silent {
            self.release_all().unwrap_or(0)
        } else {
            0
        }
    }

    fn perform(&mut self, ops: &[InputOp]) -> bool {
        if ops.is_empty() {
            return true;
        }
        self.ops_sent += ops.len() as u64;
        self.backend.send(ops)
    }

    fn button(&mut self, button: MouseButton, down: bool) -> InputOp {
        if down {
            if !self.buttons.contains(&button) {
                self.buttons.push(button);
            }
        } else {
            self.buttons.retain(|b| *b != button);
        }
        InputOp::Button { button, down }
    }

    fn key(&mut self, vk: u16, scan: u16, ext: bool, down: bool) -> InputOp {
        let mut flags = if ext { keyflags::EXTENDEDKEY } else { 0 };
        if vk == 0 && scan != 0 {
            flags |= keyflags::SCANCODE;
        }
        let held = HeldKey { vk, scan, flags };
        let id = held.id();
        if down {
            if !self.keys.iter().any(|k| k.id() == id) {
                self.keys.push(held);
            }
        } else {
            self.keys.retain(|k| k.id() != id);
            flags |= keyflags::KEYUP;
        }
        InputOp::Key { vk, scan, flags }
    }

    /// A relative move: exact pixels from the current cursor (clamped to
    /// the captured region, so the cursor stays where the viewer sees it),
    /// or raw motion when asked / when the cursor position is unknown.
    fn relative(&mut self, dx: i32, dy: i32, raw: bool) -> InputOp {
        if !raw {
            if let Some((cx, cy)) = self.backend.cursor_pos() {
                let r = self.region;
                let max_x = r.x.saturating_add(r.w.max(1) as i32 - 1);
                let max_y = r.y.saturating_add(r.h.max(1) as i32 - 1);
                let px = cx.saturating_add(dx).clamp(r.x, max_x);
                let py = cy.saturating_add(dy).clamp(r.y, max_y);
                let (nx, ny) = to_virtual_desk(px, py, &self.vs);
                return InputOp::MoveAbs { nx, ny };
            }
        }
        InputOp::MoveRaw { dx, dy }
    }

    /// Ctrl+Alt+Del: `SendSAS` when this process may generate it, else
    /// the plain keystrokes (Winlogon ignores injected ones, but a remote
    /// session / VM inside the host's desktop still gets them).
    fn sas(&mut self) -> bool {
        match self.backend.send_sas() {
            Ok(how) => {
                tracing::info!("Ctrl+Alt+Del via {how}");
                true
            }
            Err(why) => {
                tracing::warn!(
                    "Ctrl+Alt+Del: SendSAS unavailable ({why}); injected the keystrokes \
                     instead (Winlogon ignores those — run ovhost as a service with the \
                     SoftwareSASGeneration policy for a real SAS)"
                );
                let ops = [
                    InputOp::Key {
                        vk: vk::CONTROL,
                        scan: 0x1D,
                        flags: 0,
                    },
                    InputOp::Key {
                        vk: vk::MENU,
                        scan: 0x38,
                        flags: 0,
                    },
                    InputOp::Key {
                        vk: vk::DELETE,
                        scan: 0x53,
                        flags: keyflags::EXTENDEDKEY,
                    },
                    InputOp::Key {
                        vk: vk::DELETE,
                        scan: 0x53,
                        flags: keyflags::EXTENDEDKEY | keyflags::KEYUP,
                    },
                    InputOp::Key {
                        vk: vk::MENU,
                        scan: 0x38,
                        flags: keyflags::KEYUP,
                    },
                    InputOp::Key {
                        vk: vk::CONTROL,
                        scan: 0x1D,
                        flags: keyflags::KEYUP,
                    },
                ];
                self.perform(&ops)
            }
        }
    }
}

impl Drop for InputController {
    /// Never leave anything held behind when the session's controller goes.
    fn drop(&mut self) {
        let _ = self.release_all();
    }
}

/// TEXT → Unicode keystrokes: every UTF-16 unit (both halves of a surrogate
/// pair for astral characters such as emoji) as a `KEYEVENTF_UNICODE`
/// down/up; `\n` / `\r\n` / `\r` as Enter and `\t` as Tab (apps treat those
/// as keys, not characters).
#[must_use]
pub fn text_ops(text: &str) -> Vec<InputOp> {
    let mut ops = Vec::with_capacity(text.len() * 2);
    let key = |ops: &mut Vec<InputOp>, vk: u16, scan: u16| {
        ops.push(InputOp::Key { vk, scan, flags: 0 });
        ops.push(InputOp::Key {
            vk,
            scan,
            flags: keyflags::KEYUP,
        });
    };
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                key(&mut ops, vk::RETURN, 0x1C);
            }
            '\n' => key(&mut ops, vk::RETURN, 0x1C),
            '\t' => key(&mut ops, vk::TAB, 0x0F),
            c => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf).iter() {
                    ops.push(InputOp::Unicode {
                        unit: *unit,
                        up: false,
                    });
                    ops.push(InputOp::Unicode {
                        unit: *unit,
                        up: true,
                    });
                }
            }
        }
    }
    ops
}

/// A human-readable line per op (dry-run log).
#[must_use]
pub fn describe(op: &InputOp, vs: &VirtualScreen) -> String {
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
pub struct DryRunBackend {
    vs: VirtualScreen,
    cursor: (i32, i32),
}

impl DryRunBackend {
    pub fn new(vs: VirtualScreen) -> Self {
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
        match sas_capability() {
            Ok(how) => {
                tracing::info!("[dry-input] would call {how}");
                Ok("SendSAS (dry run)")
            }
            Err(e) => Err(e),
        }
    }
}

/// The real backend: `SendInput` / `GetCursorPos` / `SendSAS`.
#[derive(Default)]
pub struct SendInputBackend;

impl InputBackend for SendInputBackend {
    fn send(&mut self, ops: &[InputOp]) -> bool {
        #[cfg(windows)]
        {
            let inputs: Vec<ffi::INPUT> = ops.iter().map(ffi::to_input).collect();
            // SAFETY: `inputs` is a fully initialized array of INPUTs.
            let n = unsafe {
                ffi::SendInput(
                    inputs.len() as u32,
                    inputs.as_ptr(),
                    std::mem::size_of::<ffi::INPUT>() as i32,
                )
            };
            n as usize == inputs.len()
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
            let mut p = ffi::POINT { x: 0, y: 0 };
            // SAFETY: `p` is a valid out-pointer.
            (unsafe { ffi::GetCursorPos(&mut p) } != 0).then_some((p.x, p.y))
        }
        #[cfg(not(windows))]
        {
            None
        }
    }

    fn send_sas(&mut self) -> Result<&'static str, String> {
        let how = sas_capability()?;
        #[cfg(windows)]
        {
            // SAFETY: sas_capability resolved the export; SendSAS(FALSE)
            // is the service form.
            unsafe { ffi::send_sas(false)? };
        }
        Ok(how)
    }
}

/// Whether this process can generate a real SAS: `SendSAS` must exist
/// (sas.dll) and either we run as a service (session 0) with the
/// `SoftwareSASGeneration` policy allowing services (1 or 3). Returns the
/// method or the reason it is unavailable.
pub fn sas_capability() -> Result<&'static str, String> {
    #[cfg(windows)]
    {
        if !ffi::sas_available() {
            return Err("sas.dll / SendSAS not present".into());
        }
        let policy = ffi::sas_policy();
        let session = ffi::session_id();
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

#[cfg(windows)]
mod ffi {
    //! Raw Win32 imports (user32, kernel32, advapi32). Linked implicitly by
    //! the MSVC toolchain.

    #![allow(non_snake_case)]
    // Win32 type names are conventionally ALL_CAPS; keep them as-is.
    #![allow(clippy::upper_case_acronyms)]

    use super::{keyflags, InputOp};
    use ovapp::terminal::MouseButton;

    #[link(name = "user32")]
    extern "system" {
        pub fn SendInput(cInputs: u32, pInputs: *const INPUT, cbSize: i32) -> u32;
        pub fn GetSystemMetrics(idx: i32) -> i32;
        pub fn GetCursorPos(lpPoint: *mut POINT) -> i32;
        pub fn MapVirtualKeyW(uCode: u32, uMapType: u32) -> u32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryW(name: *const u16) -> isize;
        fn GetProcAddress(module: isize, name: *const u8) -> *const core::ffi::c_void;
        fn GetCurrentProcessId() -> u32;
        fn ProcessIdToSessionId(pid: u32, session: *mut u32) -> i32;
    }

    #[link(name = "advapi32")]
    extern "system" {
        fn RegGetValueW(
            hkey: isize,
            sub_key: *const u16,
            value: *const u16,
            flags: u32,
            ty: *mut u32,
            data: *mut core::ffi::c_void,
            len: *mut u32,
        ) -> i32;
    }

    #[repr(C)]
    pub struct POINT {
        pub x: i32,
        pub y: i32,
    }

    /// `MOUSEINPUT` (winuser.h). `dwExtraInfo` is `ULONG_PTR` → `usize`.
    #[derive(Clone, Copy)]
    #[repr(C)]
    pub struct MOUSEINPUT {
        pub dx: i32,
        pub dy: i32,
        pub mouseData: u32,
        pub dwFlags: u32,
        pub time: u32,
        pub dwExtraInfo: usize,
    }

    /// `KEYBDINPUT` (winuser.h).
    #[derive(Clone, Copy)]
    #[repr(C)]
    pub struct KEYBDINPUT {
        pub wVk: u16,
        pub wScan: u16,
        pub dwFlags: u32,
        pub time: u32,
        pub dwExtraInfo: usize,
    }

    /// The `INPUT` union, reduced to the two variants we inject (the mouse
    /// variant is the larger of the two on all supported targets).
    #[repr(C)]
    pub union INPUT_UNION {
        pub mi: MOUSEINPUT,
        pub ki: KEYBDINPUT,
    }

    /// `INPUT` (winuser.h): a type tag followed by the payload union.
    #[repr(C)]
    pub struct INPUT {
        pub type_: u32,
        pub u: INPUT_UNION,
    }

    pub const INPUT_MOUSE: u32 = 0;
    pub const INPUT_KEYBOARD: u32 = 1;
    pub const MOUSEEVENTF_MOVE: u32 = 0x0001;
    pub const MOUSEEVENTF_LEFTDOWN: u32 = 0x0002;
    pub const MOUSEEVENTF_LEFTUP: u32 = 0x0004;
    pub const MOUSEEVENTF_RIGHTDOWN: u32 = 0x0008;
    pub const MOUSEEVENTF_RIGHTUP: u32 = 0x0010;
    pub const MOUSEEVENTF_MIDDLEDOWN: u32 = 0x0020;
    pub const MOUSEEVENTF_MIDDLEUP: u32 = 0x0040;
    pub const MOUSEEVENTF_XDOWN: u32 = 0x0080;
    pub const MOUSEEVENTF_XUP: u32 = 0x0100;
    pub const MOUSEEVENTF_WHEEL: u32 = 0x0800;
    pub const MOUSEEVENTF_HWHEEL: u32 = 0x1000;
    pub const MOUSEEVENTF_VIRTUALDESK: u32 = 0x4000;
    pub const MOUSEEVENTF_ABSOLUTE: u32 = 0x8000;
    pub const XBUTTON1: u32 = 0x0001;
    pub const XBUTTON2: u32 = 0x0002;
    pub const MAPVK_VK_TO_VSC: u32 = 0;
    pub const SM_XVIRTUALSCREEN: i32 = 76;
    pub const SM_YVIRTUALSCREEN: i32 = 77;
    pub const SM_CXVIRTUALSCREEN: i32 = 78;
    pub const SM_CYVIRTUALSCREEN: i32 = 79;

    fn mouse(dx: i32, dy: i32, data: u32, flags: u32) -> INPUT {
        INPUT {
            type_: INPUT_MOUSE,
            u: INPUT_UNION {
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
            type_: INPUT_KEYBOARD,
            u: INPUT_UNION {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    pub fn to_input(op: &InputOp) -> INPUT {
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
                    MouseButton::X1 => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, XBUTTON1),
                    MouseButton::X2 => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, XBUTTON2),
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
                    // SAFETY: plain value lookup.
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

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    type SendSasFn = unsafe extern "system" fn(as_user: i32);

    fn send_sas_fn() -> Option<SendSasFn> {
        // SAFETY: LoadLibraryW/GetProcAddress with NUL-terminated names; the
        // module stays loaded for the process lifetime (never freed).
        unsafe {
            let module = LoadLibraryW(wide("sas.dll").as_ptr());
            if module == 0 {
                return None;
            }
            let f = GetProcAddress(module, c"SendSAS".as_ptr().cast());
            if f.is_null() {
                None
            } else {
                Some(std::mem::transmute::<*const core::ffi::c_void, SendSasFn>(
                    f,
                ))
            }
        }
    }

    pub fn sas_available() -> bool {
        send_sas_fn().is_some()
    }

    /// # Safety
    /// Calls into sas.dll.
    pub unsafe fn send_sas(as_user: bool) -> Result<(), String> {
        let f = send_sas_fn().ok_or("SendSAS not present")?;
        f(i32::from(as_user));
        Ok(())
    }

    /// `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System`
    /// `SoftwareSASGeneration` (DWORD), if set.
    pub fn sas_policy() -> Option<u32> {
        const HKEY_LOCAL_MACHINE: isize = 0x8000_0002_u32 as i32 as isize;
        const RRF_RT_REG_DWORD: u32 = 0x0000_0010;
        let key = wide("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Policies\\System");
        let value = wide("SoftwareSASGeneration");
        let mut data: u32 = 0;
        let mut len: u32 = 4;
        // SAFETY: valid NUL-terminated strings and a 4-byte out buffer.
        let rc = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                key.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_DWORD,
                std::ptr::null_mut(),
                (&mut data as *mut u32).cast(),
                &mut len,
            )
        };
        (rc == 0).then_some(data)
    }

    /// The Terminal Services session of this process (0 = services).
    pub fn session_id() -> Option<u32> {
        let mut sid = 0u32;
        // SAFETY: valid out-pointer.
        (unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut sid) } != 0).then_some(sid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    const FULL: Region = Region {
        x: 0,
        y: 0,
        w: 1920,
        h: 1080,
    };
    const VS: VirtualScreen = VirtualScreen {
        x: 0,
        y: 0,
        w: 1920,
        h: 1080,
    };

    /// Records every op; the cursor follows absolute moves.
    #[derive(Clone)]
    struct Rec {
        ops: Arc<Mutex<Vec<InputOp>>>,
        vs: VirtualScreen,
        cursor: Arc<Mutex<Option<(i32, i32)>>>,
        sas_ok: bool,
    }

    impl Rec {
        fn new(vs: VirtualScreen) -> Self {
            Self {
                ops: Arc::default(),
                vs,
                cursor: Arc::new(Mutex::new(Some((100, 100)))),
                sas_ok: false,
            }
        }
        fn take(&self) -> Vec<InputOp> {
            std::mem::take(&mut *self.ops.lock().unwrap())
        }
    }

    impl InputBackend for Rec {
        fn send(&mut self, ops: &[InputOp]) -> bool {
            for op in ops {
                if let InputOp::MoveAbs { nx, ny } = op {
                    *self.cursor.lock().unwrap() = Some(from_virtual_desk(*nx, *ny, &self.vs));
                }
            }
            self.ops.lock().unwrap().extend_from_slice(ops);
            true
        }
        fn cursor_pos(&mut self) -> Option<(i32, i32)> {
            *self.cursor.lock().unwrap()
        }
        fn send_sas(&mut self) -> Result<&'static str, String> {
            if self.sas_ok {
                Ok("test SAS")
            } else {
                Err("test: no SAS".into())
            }
        }
    }

    fn controller(region: Region, vs: VirtualScreen) -> (InputController, Rec) {
        let rec = Rec::new(vs);
        (InputController::new(Box::new(rec.clone()), region, vs), rec)
    }

    #[test]
    fn region_mapping_hits_first_and_last_pixel() {
        let r = Region {
            x: 10,
            y: 20,
            w: 100,
            h: 50,
        };
        assert_eq!(map_to_region(0, 0, &r), (10, 20));
        assert_eq!(
            map_to_region(65_535, 65_535, &r),
            (109, 69),
            "65535 = last pixel"
        );
        assert_eq!(map_to_region(u32::MAX, u32::MAX, &r), (109, 69), "clamped");
        // The viewer's px * 65535 / (w - 1) round-trips every pixel.
        for px in 0..100u32 {
            let n = (px * 65_535 + 49) / 99;
            assert_eq!(map_to_region(n, 0, &r).0, 10 + px as i32, "px {px}");
        }
    }

    #[test]
    fn virtual_desk_round_trips_every_pixel_incl_negative_origins() {
        for vs in [
            VS,
            // Secondary monitor left of the primary: origin -1920.
            VirtualScreen {
                x: -1920,
                y: 0,
                w: 3840,
                h: 1080,
            },
            // Monitor above-left, odd sizes.
            VirtualScreen {
                x: -1280,
                y: -1024,
                w: 3200,
                h: 2104,
            },
            VirtualScreen {
                x: -2560,
                y: -300,
                w: 7680,
                h: 1740,
            },
        ] {
            for px in (vs.x..vs.x + vs.w as i32)
                .step_by(7)
                .chain([vs.x + vs.w as i32 - 1])
            {
                let (nx, _) = to_virtual_desk(px, vs.y, &vs);
                assert!((0..=65_535).contains(&nx));
                assert_eq!(from_virtual_desk(nx, 0, &vs).0, px, "vs {vs:?} px {px}");
            }
            for py in (vs.y..vs.y + vs.h as i32)
                .step_by(5)
                .chain([vs.y + vs.h as i32 - 1])
            {
                let (_, ny) = to_virtual_desk(vs.x, py, &vs);
                assert_eq!(from_virtual_desk(0, ny, &vs).1, py, "vs {vs:?} py {py}");
            }
            // Off-screen pixels clamp to the edge.
            assert_eq!(to_virtual_desk(vs.x - 50, vs.y - 50, &vs), (0, 0));
        }
    }

    #[test]
    fn selected_region_on_a_negative_monitor_maps_correctly() {
        // The viewer picked the left monitor (-1920..-1) of a 2-monitor desk.
        let vs = VirtualScreen {
            x: -1920,
            y: -200,
            w: 3840,
            h: 1280,
        };
        let left = Region {
            x: -1920,
            y: -200,
            w: 1920,
            h: 1080,
        };
        let (nx, ny) = map_to_absolute(0, 0, &left, &vs);
        assert_eq!(from_virtual_desk(nx, ny, &vs), (-1920, -200));
        let (nx, ny) = map_to_absolute(65_535, 65_535, &left, &vs);
        assert_eq!(from_virtual_desk(nx, ny, &vs), (-1, 879));
        // Center of the left monitor.
        let (nx, ny) = map_to_absolute(32_768, 32_768, &left, &vs);
        assert_eq!(from_virtual_desk(nx, ny, &vs), (-960, 340));
        // The primary (right) monitor region.
        let right = Region {
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
        };
        let (nx, ny) = map_to_absolute(0, 0, &right, &vs);
        assert_eq!(from_virtual_desk(nx, ny, &vs), (0, 0));
        let (nx, ny) = map_to_absolute(65_535, 65_535, &right, &vs);
        assert_eq!(from_virtual_desk(nx, ny, &vs), (1919, 1079));
    }

    #[test]
    fn full_screen_region_is_near_identity() {
        for (x, y) in [(0u32, 0u32), (32_767, 32_767), (65_535, 65_535)] {
            let (ax, ay) = map_to_absolute(x, y, &FULL, &VS);
            assert!((i64::from(ax) - i64::from(x)).abs() <= 40, "x {x} -> {ax}");
            assert!((i64::from(ay) - i64::from(y)).abs() <= 70, "y {y} -> {ay}");
        }
    }

    #[test]
    fn legacy_pointer_and_key_keep_working() {
        let (mut c, rec) = controller(FULL, VS);
        let now = Instant::now();
        c.handle(
            &Command::Pointer {
                kind: PointerKind::Down,
                x: 0,
                y: 0,
            },
            now,
        );
        assert_eq!(
            rec.take(),
            vec![
                InputOp::MoveAbs { nx: 0, ny: 0 },
                InputOp::Button {
                    button: MouseButton::Left,
                    down: true
                }
            ]
        );
        assert!(c.holding());
        c.handle(
            &Command::Pointer {
                kind: PointerKind::Up,
                x: 0,
                y: 0,
            },
            now,
        );
        assert!(!c.holding());
        rec.take();
        // Legacy KEY on an arrow gets the extended flag (NumLock-proof).
        c.handle(
            &Command::Key {
                vk: 0x25,
                down: true,
            },
            now,
        );
        assert_eq!(
            rec.take(),
            vec![InputOp::Key {
                vk: 0x25,
                scan: 0,
                flags: keyflags::EXTENDEDKEY
            }]
        );
        c.handle(
            &Command::Key {
                vk: 0x41,
                down: false,
            },
            now,
        );
        assert_eq!(
            rec.take(),
            vec![InputOp::Key {
                vk: 0x41,
                scan: 0,
                flags: keyflags::KEYUP
            }]
        );
        // Non-input commands are not ours.
        assert_eq!(c.handle(&Command::Ping(1), now), None);
    }

    #[test]
    fn buttons_wheel_and_relative_moves() {
        let (mut c, rec) = controller(FULL, VS);
        let now = Instant::now();
        let pex = |action, button, flags, x, y| Command::PointerEx {
            action,
            button,
            flags,
            x,
            y,
        };
        c.handle(&pex(PointerAction::Down, MouseButton::Right, 0, 0, 0), now);
        c.handle(&pex(PointerAction::Up, MouseButton::Right, 0, 0, 0), now);
        assert_eq!(
            rec.take(),
            vec![
                InputOp::Button {
                    button: MouseButton::Right,
                    down: true
                },
                InputOp::Button {
                    button: MouseButton::Right,
                    down: false
                }
            ]
        );
        c.handle(
            &pex(
                PointerAction::Down,
                MouseButton::Middle,
                POINTER_HAS_POS,
                65_535,
                0,
            ),
            now,
        );
        let ops = rec.take();
        assert!(matches!(ops[0], InputOp::MoveAbs { .. }));
        assert_eq!(
            ops[1],
            InputOp::Button {
                button: MouseButton::Middle,
                down: true
            }
        );
        c.handle(&pex(PointerAction::Up, MouseButton::Middle, 0, 0, 0), now);
        rec.take();
        // Relative: exact pixels from the cursor (100,100) -> (90,130).
        *rec.cursor.lock().unwrap() = Some((100, 100));
        c.handle(
            &pex(PointerAction::MoveRel, MouseButton::Left, 0, -10, 30),
            now,
        );
        let ops = rec.take();
        let InputOp::MoveAbs { nx, ny } = ops[0] else {
            panic!("{ops:?}")
        };
        assert_eq!(from_virtual_desk(nx, ny, &VS), (90, 130));
        // Clamped to the region.
        c.handle(
            &pex(PointerAction::MoveRel, MouseButton::Left, 0, -5000, 5000),
            now,
        );
        let InputOp::MoveAbs { nx, ny } = rec.take()[0] else {
            panic!()
        };
        assert_eq!(from_virtual_desk(nx, ny, &VS), (0, 1079));
        // RAW: real relative motion.
        c.handle(
            &pex(
                PointerAction::MoveRel,
                MouseButton::Left,
                POINTER_RAW,
                3,
                -4,
            ),
            now,
        );
        assert_eq!(rec.take(), vec![InputOp::MoveRaw { dx: 3, dy: -4 }]);
        // Wheel: vertical then horizontal.
        c.handle(&Command::Wheel { dy: -240, dx: 60 }, now);
        assert_eq!(rec.take(), vec![InputOp::Wheel(-240), InputOp::HWheel(60)]);
        c.handle(&Command::Wheel { dy: 0, dx: 0 }, now);
        assert!(rec.take().is_empty());
    }

    #[test]
    fn key_ex_extended_and_scancode_only() {
        let (mut c, rec) = controller(FULL, VS);
        let now = Instant::now();
        // Right Ctrl: vk A3, scan 1D, extended per the viewer.
        c.handle(
            &Command::KeyEx {
                vk: 0xA3,
                scan: 0x1D,
                flags: KEY_DOWN | KEY_EXTENDED,
            },
            now,
        );
        // Numpad Enter: vk 0D + extended scan 1C.
        c.handle(
            &Command::KeyEx {
                vk: 0x0D,
                scan: 0x1C,
                flags: KEY_DOWN | KEY_EXTENDED,
            },
            now,
        );
        // Scancode-only key.
        c.handle(
            &Command::KeyEx {
                vk: 0,
                scan: 0x1E,
                flags: KEY_DOWN,
            },
            now,
        );
        // Scan given, no ext flag: the viewer is authoritative (main Enter).
        c.handle(
            &Command::KeyEx {
                vk: 0x2E,
                scan: 0x53,
                flags: KEY_DOWN,
            },
            now,
        );
        // Scan absent: ext derived from vk (Delete).
        c.handle(
            &Command::KeyEx {
                vk: 0x2E,
                scan: 0,
                flags: 0,
            },
            now,
        );
        assert_eq!(
            rec.take(),
            vec![
                InputOp::Key {
                    vk: 0xA3,
                    scan: 0x1D,
                    flags: keyflags::EXTENDEDKEY
                },
                InputOp::Key {
                    vk: 0x0D,
                    scan: 0x1C,
                    flags: keyflags::EXTENDEDKEY
                },
                InputOp::Key {
                    vk: 0,
                    scan: 0x1E,
                    flags: keyflags::SCANCODE
                },
                InputOp::Key {
                    vk: 0x2E,
                    scan: 0x53,
                    flags: 0
                },
                InputOp::Key {
                    vk: 0x2E,
                    scan: 0,
                    flags: keyflags::EXTENDEDKEY | keyflags::KEYUP
                },
            ]
        );
        // Delete was released by the last event; A3, 0D, scan 1E remain.
        assert_eq!(c.keys.len(), 3);
    }

    #[test]
    fn text_is_unicode_with_surrogate_pairs() {
        let ops = text_ops("hé😀\r\nx\t");
        let units: Vec<(u16, bool)> = ops
            .iter()
            .filter_map(|o| match o {
                InputOp::Unicode { unit, up } => Some((*unit, *up)),
                _ => None,
            })
            .collect();
        assert_eq!(
            units,
            vec![
                (0x68, false),
                (0x68, true),
                (0xE9, false),
                (0xE9, true),
                (0xD83D, false),
                (0xD83D, true),
                (0xDE00, false),
                (0xDE00, true),
                (0x78, false),
                (0x78, true),
            ]
        );
        // "\r\n" is ONE Enter, "\t" a Tab.
        let keys: Vec<&InputOp> = ops
            .iter()
            .filter(|o| matches!(o, InputOp::Key { .. }))
            .collect();
        assert_eq!(keys.len(), 4);
        assert!(matches!(keys[0], InputOp::Key { vk: vk::RETURN, .. }));
        assert!(matches!(keys[2], InputOp::Key { vk: vk::TAB, .. }));
        // Text never leaves anything held.
        let (mut c, _rec) = controller(FULL, VS);
        c.handle(&Command::Text("héllo 😀".into()), Instant::now());
        assert!(!c.holding());
    }

    #[test]
    fn release_all_releases_everything_it_pressed() {
        let (mut c, rec) = controller(FULL, VS);
        let now = Instant::now();
        c.handle(
            &Command::KeyEx {
                vk: 0xA2,
                scan: 0x1D,
                flags: KEY_DOWN,
            },
            now,
        );
        c.handle(
            &Command::KeyEx {
                vk: 0x10,
                scan: 0x2A,
                flags: KEY_DOWN,
            },
            now,
        );
        c.handle(
            &Command::PointerEx {
                action: PointerAction::Down,
                button: MouseButton::Left,
                flags: 0,
                x: 0,
                y: 0,
            },
            now,
        );
        // A repeated down (auto-repeat) is tracked once.
        c.handle(
            &Command::KeyEx {
                vk: 0x10,
                scan: 0x2A,
                flags: KEY_DOWN,
            },
            now,
        );
        rec.take();
        assert_eq!(c.handle(&Command::ReleaseAll, now), Some(true));
        assert_eq!(
            rec.take(),
            vec![
                InputOp::Button {
                    button: MouseButton::Left,
                    down: false
                },
                InputOp::Key {
                    vk: 0x10,
                    scan: 0x2A,
                    flags: keyflags::KEYUP
                },
                InputOp::Key {
                    vk: 0xA2,
                    scan: 0x1D,
                    flags: keyflags::KEYUP
                },
            ]
        );
        assert!(!c.holding());
        // Nothing held: RELEASE_ALL is a no-op.
        c.handle(&Command::ReleaseAll, now);
        assert!(rec.take().is_empty());
    }

    #[test]
    fn stuck_input_is_released_after_silence_and_on_drop() {
        let (mut c, rec) = controller(FULL, VS);
        let t0 = Instant::now();
        c.handle(
            &Command::Key {
                vk: 0x11,
                down: true,
            },
            t0,
        );
        rec.take();
        assert_eq!(
            c.tick(t0 + Duration::from_secs(29)),
            0,
            "still within the window"
        );
        assert!(c.holding());
        assert_eq!(c.tick(t0 + STUCK_RELEASE_AFTER), 1);
        assert_eq!(
            rec.take(),
            vec![InputOp::Key {
                vk: 0x11,
                scan: 0,
                flags: keyflags::KEYUP
            }]
        );
        // Activity resets the clock.
        c.handle(
            &Command::Key {
                vk: 0x12,
                down: true,
            },
            t0 + Duration::from_secs(40),
        );
        assert_eq!(c.tick(t0 + Duration::from_secs(60)), 0);
        rec.take();
        // Session end: dropping the controller releases.
        drop(c);
        assert_eq!(
            rec.take(),
            vec![InputOp::Key {
                vk: 0x12,
                scan: 0,
                flags: keyflags::KEYUP
            }]
        );
    }

    #[test]
    fn sas_falls_back_to_keystrokes() {
        let (mut c, rec) = controller(FULL, VS);
        assert_eq!(c.handle(&Command::Sas, Instant::now()), Some(true));
        let ops = rec.take();
        assert_eq!(ops.len(), 6, "ctrl alt del down + up");
        assert!(!c.holding(), "the fallback leaves nothing held");
        let mut rec2 = Rec::new(VS);
        rec2.sas_ok = true;
        let mut c2 = InputController::new(Box::new(rec2.clone()), FULL, VS);
        c2.handle(&Command::Sas, Instant::now());
        assert!(rec2.take().is_empty(), "real SAS: no keystrokes");
    }

    #[test]
    fn extended_vk_table() {
        for v in [
            0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x2D, 0x2E, 0x5B, 0x5C, 0xA3, 0xA5,
        ] {
            assert!(is_extended_vk(v), "{v:#x}");
        }
        for v in [0x0D, 0x41, 0x11, 0xA2, 0xA4, 0x60, 0x70] {
            assert!(!is_extended_vk(v), "{v:#x}");
        }
    }

    #[test]
    fn dry_run_backend_tracks_the_cursor() {
        let vs = VirtualScreen {
            x: -100,
            y: 0,
            w: 300,
            h: 100,
        };
        let mut b = DryRunBackend::new(vs);
        let (nx, ny) = to_virtual_desk(-50, 20, &vs);
        assert!(b.send(&[
            InputOp::MoveAbs { nx, ny },
            InputOp::MoveRaw { dx: 5, dy: 5 }
        ]));
        assert_eq!(b.cursor_pos(), Some((-45, 25)));
    }
}
