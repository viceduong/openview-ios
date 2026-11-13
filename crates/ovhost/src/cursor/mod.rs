//! Remote-cursor tracking (the cursor overlay channel).
//!
//! The screen frames stream at the adaptive capture rate, so at idle
//! the captured cursor would be frozen between frames. This module tracks the
//! real cursor separately and feeds cheap `CURSOR` (position) /
//! `CURSOR_SHAPE` (bitmap) commands to the viewer, which composites the
//! cursor onto the presented frame — the usual remote-desktop split.
//!
//! The decision logic ([`CursorTracker`]) and the shape composition
//! ([`shape`]) are pure and unit-tested everywhere; the Win32 polling
//! (`GetCursorInfo`, `GetIconInfo`, `GetDIBits`) lives in `win32`.
//!
//! Coordinates: cursor positions are sent **relative to the captured region**
//! (the region origin the GDI capture uses), so the viewer maps them straight
//! through the frame scale with no knowledge of the desktop's monitor layout.
//!
//! Wire-rate control: position updates are sent only when the cursor moved,
//! at most [`CURSOR_MAX_HZ`] per second (120 Hz: smooth on ProMotion /
//! 120 Hz viewers; an 8-byte record per move, ~1 KB/s while moving and
//! nothing while still).

use std::time::{Duration, Instant};

use ovapp::terminal::Command;

mod shape;
#[cfg(windows)]
mod win32;

#[cfg(test)]
mod tests;

#[cfg(windows)]
pub(crate) use win32::{poll, render_shape};

/// Cap on cursor-position sends per second (sent only on movement).
pub(crate) const CURSOR_MAX_HZ: u32 = 120;

/// The minimum spacing between two cursor-position sends.
pub(crate) const CURSOR_MIN_INTERVAL: Duration =
    Duration::from_nanos(1_000_000_000 / CURSOR_MAX_HZ as u64);

/// Send-decision state for the cursor channel: what to emit on this pump
/// tick, given the polled cursor state and the throttle clock.
///
/// Pure and clock-injecting (the caller supplies `now`), so tests drive it
/// deterministically without sleeping.
#[derive(Debug, Default)]
pub(crate) struct CursorTracker {
    /// Last position we *sent* (region-relative pixels).
    last_pos: Option<(i32, i32)>,
    /// Last position we *polled* (may differ: throttled sends coalesce).
    pending_pos: Option<(i32, i32)>,
    /// Last `Instant` we sent a position update.
    last_send: Option<Instant>,
    /// Last cursor-shape handle we *sent* (opaque, compared by value).
    last_shape: Option<u64>,
}

/// What [`CursorTracker::update`] decided to send this tick.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct CursorOut {
    /// A `CURSOR` position command to send (region-relative pixels).
    pub(crate) pos: Option<(i32, i32)>,
    /// A `CURSOR_SHAPE` command to send (fresh bitmap already rendered).
    pub(crate) shape: Option<Command>,
}

impl CursorTracker {
    /// Feed one poll: `pos` is the region-relative cursor position, `shape`
    /// the current shape handle (`None` when the cursor is hidden/off-screen
    /// or the poll failed) and `render` a closure that renders the shape
    /// handle to a `CURSOR_SHAPE` command (only invoked when the handle
    /// changed — rendering is the expensive part).
    ///
    /// Position sends are throttled to [`CURSOR_MAX_HZ`]: a changed position
    /// inside the throttle window is held in `pending_pos` and emitted by the
    /// first later tick that clears the window.
    pub(crate) fn update(
        &mut self,
        now: Instant,
        pos: Option<(i32, i32)>,
        shape: Option<u64>,
        mut render: impl FnMut(u64) -> Option<Command>,
    ) -> CursorOut {
        let mut out = CursorOut::default();

        // Shape first: it changes rarely, so it is never throttled, and the
        // viewer gets the new bitmap before/with the next position move.
        if let Some(handle) = shape {
            if self.last_shape != Some(handle) {
                if let Some(cmd) = render(handle) {
                    self.last_shape = Some(handle);
                    out.shape = Some(cmd);
                }
                // A failed render leaves `last_shape` untouched so the next
                // tick retries the same handle.
            }
        }

        // Position: only when it moved since the last *sent* position.
        if let Some(p) = pos {
            if self.last_pos != Some(p) {
                let throttled = self
                    .last_send
                    .is_some_and(|t| now.saturating_duration_since(t) < CURSOR_MIN_INTERVAL);
                if throttled {
                    self.pending_pos = Some(p);
                } else {
                    self.last_pos = Some(p);
                    self.last_send = Some(now);
                    self.pending_pos = None;
                    out.pos = Some(p);
                }
            } else if self.pending_pos.is_some() {
                // No further movement, but a throttled update is still owed.
                let throttled = self
                    .last_send
                    .is_some_and(|t| now.saturating_duration_since(t) < CURSOR_MIN_INTERVAL);
                if !throttled {
                    self.last_send = Some(now);
                    out.pos = self.pending_pos.take();
                }
            }
        }
        out
    }
}
