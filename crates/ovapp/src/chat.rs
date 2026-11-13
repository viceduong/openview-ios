//! Line-based chat over the terminal command channel.
//!
//! One chat line = one [`Command::Type`] record: the line's UTF-8 bytes ride
//! in the TYPE payload with key code `0` and modifier bits `0` (a "synthetic"
//! keystroke that is really a message). The sender strips trailing NULs and
//! the receiver truncates at the first NUL, so C-string-shaped peers
//! interoperate.

use ovcore::session::Session;
use ovtransport::{Carrier, TransportError};

use crate::terminal::{CmdHook, Command, TerminalSession};

/// Callback type for inbound chat lines (see [`ChatSession::on_line`]).
pub type LineHook = Box<dyn FnMut(&str)>;

/// A thin chat wrapper around a [`TerminalSession`]: sends/receives text
/// lines as TYPE commands (1 line = 1 record, UTF-8, cut at the first NUL).
pub struct ChatSession {
    term: TerminalSession,
}

impl ChatSession {
    /// Wrap an established session + carrier (see [`TerminalSession::new`]).
    pub fn new(session: Session, carrier: impl Carrier + 'static) -> Self {
        Self {
            term: TerminalSession::new(session, carrier),
        }
    }

    /// Install the receive hook (delegates to the terminal session).
    pub fn on_line(&mut self, f: Option<LineHook>) {
        self.term.on_cmd(f.map(|mut f| {
            Box::new(move |cmd: &Command| {
                if let Command::Type { payload, .. } = cmd {
                    f(&line_from_payload(payload));
                }
            }) as CmdHook
        }));
    }

    /// Send one line as a TYPE record (split into several records when it
    /// exceeds [`crate::terminal::MAX_TEXT_PAYLOAD`]).
    pub fn send_line(&mut self, line: &str) -> Result<(), TransportError> {
        let line = line.trim_end_matches('\0');
        for piece in crate::terminal::chunk_utf8(line, crate::terminal::MAX_TEXT_PAYLOAD) {
            self.term.send(&Command::Type {
                key: 0,
                mods: 0,
                payload: piece.as_bytes().to_vec(),
            })?;
        }
        Ok(())
    }

    /// Pump the underlying terminal session (drains carrier -> hook).
    pub fn pump(&mut self) -> Result<usize, TransportError> {
        self.term.pump()
    }

    /// Block up to `timeout` for the next inbound line.
    pub fn wait_for_line(&mut self, timeout: std::time::Duration) -> Option<String> {
        self.term.wait_for(timeout, |cmd| match cmd {
            Command::Type { payload, .. } => Some(line_from_payload(payload)),
            _ => None,
        })
    }

    /// Mutable access to the underlying terminal session.
    pub fn terminal_mut(&mut self) -> &mut TerminalSession {
        &mut self.term
    }

    /// Close the underlying carrier.
    pub fn close(&mut self) -> Result<(), TransportError> {
        self.term.close()
    }
}

/// Truncate at the first NUL and decode a TYPE payload as UTF-8 (lossy, so bad bytes never
/// kill the chat).
fn line_from_payload(payload: &[u8]) -> String {
    let end = payload
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(payload.len());
    String::from_utf8_lossy(&payload[..end]).into_owned()
}
