//! Local input → remote commands: pointer, wheel and key events from the
//! window, and text lines from stdin.
//!
//! A host that advertised `CAP_INPUT2` gets the input-v2 messages
//! (`POINTER_EX` with all five buttons, `WHEEL`, `KEY_EX` with scancode and
//! extended flag); an older host gets `POINTER` (moves and the left button
//! only) and `KEY`, and no wheel.

use std::sync::mpsc;

use ovapp::terminal::{
    Command, MouseButton, PointerAction, PointerKind, KEY_DOWN, KEY_EXTENDED, MAX_TEXT_PAYLOAD,
    POINTER_HAS_POS,
};

use super::{peer_input2, Viewer};
use crate::localinput::{self, LocalEvent};

/// What the loop does after the local input of one turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flow {
    /// Keep running.
    Continue,
    /// `exit` / `quit` on stdin: close the session right away.
    Exit,
}

impl Viewer {
    /// Turn the queued window input and stdin lines into remote commands.
    pub(super) fn handle_local_input(
        &mut self,
        stdin_rx: &mpsc::Receiver<LocalEvent>,
    ) -> Result<Flow, String> {
        let mut local = self.win.as_ref().map_or_else(Vec::new, |w| w.take_input());
        // stdin lines (terminal path: chat text, `exit` / `quit`).
        local.extend(stdin_rx.try_iter());
        for ev in local {
            match ev {
                LocalEvent::Pointer {
                    kind,
                    button,
                    client_x,
                    client_y,
                } => self.pointer(kind, button, client_x, client_y)?,
                LocalEvent::Wheel { dy, dx } => {
                    // A host without input v2 has no wheel: dropped.
                    if peer_input2(&self.term) {
                        self.send(&Command::Wheel { dy, dx })?;
                    }
                }
                LocalEvent::Key {
                    vk,
                    scan,
                    extended,
                    down,
                } => self.key(vk, scan, extended, down)?,
                LocalEvent::Line(line) => {
                    if line == "exit" || line == "quit" {
                        return Ok(Flow::Exit);
                    }
                    self.type_line(&line)?;
                }
            }
        }
        Ok(Flow::Continue)
    }

    /// Send one command; a send error ends the session.
    pub(super) fn send(&mut self, cmd: &Command) -> Result<(), String> {
        self.term.send(cmd).map_err(|e| format!("send failed: {e}"))
    }

    /// A pointer event at a client pixel, normalized into the letterboxed
    /// content rect.
    fn pointer(
        &mut self,
        kind: PointerKind,
        button: MouseButton,
        client_x: i32,
        client_y: i32,
    ) -> Result<(), String> {
        let Some(w) = &self.win else {
            return Ok(());
        };
        let rect = w.content_rect();
        let input2 = peer_input2(&self.term);
        let bit = 1u8 << (button as u8);
        // A press only starts inside the frame; while a button is held,
        // moves and the release are clamped to it (never dropped: a lost UP
        // sticks the button).
        let pos = match kind {
            PointerKind::Down => localinput::client_to_normalized(client_x, client_y, &rect),
            PointerKind::Up if self.held_buttons & bit != 0 => {
                localinput::client_to_normalized_clamped(client_x, client_y, &rect)
            }
            PointerKind::Up => None,
            PointerKind::Move if self.held_buttons != 0 => {
                localinput::client_to_normalized_clamped(client_x, client_y, &rect)
            }
            PointerKind::Move => localinput::client_to_normalized(client_x, client_y, &rect),
        };
        let Some((nx, ny)) = pos else {
            return Ok(());
        };
        match kind {
            PointerKind::Down => self.held_buttons |= bit,
            PointerKind::Up => self.held_buttons &= !bit,
            PointerKind::Move => {}
        }
        let cmd = if input2 {
            let (action, flags) = match kind {
                PointerKind::Move => (PointerAction::MoveAbs, 0),
                PointerKind::Down => (PointerAction::Down, POINTER_HAS_POS),
                PointerKind::Up => (PointerAction::Up, POINTER_HAS_POS),
            };
            Some(Command::PointerEx {
                action,
                button,
                flags,
                x: nx as i32,
                y: ny as i32,
            })
        } else if kind == PointerKind::Move || button == MouseButton::Left {
            // A host without input v2 only knows the left button.
            Some(Command::Pointer { kind, x: nx, y: ny })
        } else {
            None
        };
        match cmd {
            Some(cmd) => self.send(&cmd),
            None => Ok(()),
        }
    }

    /// A key event: local hotkeys first, then (unless consumed) forwarded.
    fn key(&mut self, vk: u16, scan: u16, extended: bool, down: bool) -> Result<(), String> {
        if self.hotkey(vk, down)? == super::keys::KeyUse::Consumed {
            return Ok(());
        }
        // Input v2: scancode + extended flag (right Ctrl/Alt, arrows with
        // NumLock on, numpad Enter come out right).
        let cmd = if peer_input2(&self.term) {
            Command::KeyEx {
                vk,
                scan,
                flags: if down { KEY_DOWN } else { 0 } | if extended { KEY_EXTENDED } else { 0 },
            }
        } else {
            Command::Key { vk, down }
        };
        self.send(&cmd)
    }

    /// A stdin line as chat text. A pasted giant line is split so each TYPE
    /// record fits the carrier (an oversized record would end the session).
    fn type_line(&mut self, line: &str) -> Result<(), String> {
        for piece in ovapp::terminal::chunk_utf8(line, MAX_TEXT_PAYLOAD) {
            self.send(&Command::Type {
                key: 0,
                mods: 0,
                payload: piece.as_bytes().to_vec(),
            })?;
        }
        Ok(())
    }
}
