//! [`InputController`]: wire commands → [`InputOp`]s, with release-all
//! bookkeeping.

use std::time::Instant;

use ovapp::terminal::{
    Command, MouseButton, PointerAction, PointerKind, KEY_DOWN, KEY_EXTENDED, POINTER_HAS_POS,
    POINTER_RAW,
};

use super::geometry::{map_to_absolute, to_virtual_desk, Region, VirtualScreen};
use super::{is_extended_vk, keyflags, vk, InputBackend, InputOp, STUCK_RELEASE_AFTER};

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
pub(crate) struct InputController {
    backend: Box<dyn InputBackend>,
    region: Region,
    vs: VirtualScreen,
    keys: Vec<HeldKey>,
    buttons: Vec<MouseButton>,
    last_input: Option<Instant>,
}

impl InputController {
    pub(crate) fn new(backend: Box<dyn InputBackend>, region: Region, vs: VirtualScreen) -> Self {
        Self {
            backend,
            region,
            vs,
            keys: Vec::new(),
            buttons: Vec::new(),
            last_input: None,
        }
    }

    /// The captured region changed (CAPTURE_REGION retarget).
    pub(crate) fn set_region(&mut self, region: Region) {
        self.region = region;
    }

    /// The virtual screen changed (monitor hot-plug).
    pub(crate) fn set_virtual_screen(&mut self, vs: VirtualScreen) {
        self.vs = vs;
    }

    /// Whether any key or button is currently held for the viewer.
    pub(crate) fn holding(&self) -> bool {
        !self.keys.is_empty() || !self.buttons.is_empty()
    }

    /// How many keys are currently held (tests).
    #[cfg(test)]
    pub(super) fn held_key_count(&self) -> usize {
        self.keys.len()
    }

    /// Handle one inbound command. Returns `None` for non-input commands,
    /// `Some(ok)` for input (`ok = false`: the OS refused an injection).
    pub(crate) fn handle(&mut self, cmd: &Command, now: Instant) -> Option<bool> {
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
    pub(crate) fn release_all(&mut self) -> Option<usize> {
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
    pub(crate) fn tick(&mut self, now: Instant) -> usize {
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
pub(super) fn text_ops(text: &str) -> Vec<InputOp> {
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
