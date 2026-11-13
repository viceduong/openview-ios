//! Prioritized outbound path for the host's screen session.
//!
//! The capture side and the session side run on different threads:
//!
//! - a **capture thread** grabs + change-detects + compresses frames
//!   ([`EncodedFrame::compress`]) and drops each one into a single-slot
//!   [`FrameMailbox`] — a newer frame *replaces* an older one that has not
//!   started sending yet, so a slow link always gets the freshest pixels and
//!   never a backlog;
//! - the **session thread** owns the [`TerminalSession`] (encryption,
//!   framing, the carrier) and drives a [`PrioritySender`]: control commands
//!   (PONG, cursor, audio, clipboard, chat echo, ...) always go first, and
//!   frame records are interleaved one at a time, so a control message never
//!   waits behind more than one frame chunk. At most one frame is in flight:
//!   the mailbox is only consulted once the previous frame fully drained.
//!
//! The sender also measures how long each frame takes to drain
//! ([`DrainMeter`]) so the capture thread can pace itself to what the link
//! actually carries instead of compressing frames that would be replaced
//! before they are sent.
//!
//! # Dirty-tile deltas through the mailbox
//!
//! [`UpdateEncoder`] (capture thread) turns each capture into a full frame
//! or a tile delta. A delta must be diffed against the frame the viewer will
//! actually hold when it arrives — the last update the sender **took** from
//! the mailbox — not against the previous capture, because an unsent update
//! can be replaced. So the encoder keeps the tiles dirty since that base
//! (`pending`) and, while its previous update still sits unsent in the
//! mailbox, a new delta carries the union of both (the replaced one's tiles
//! are re-sent with the newest pixels). `FrameMailbox::put_if_taken`
//! makes the "which base?" decision atomic with the hand-off: if the sender
//! took the previous update in between, the put is refused and the encoder
//! re-diffs against that one instead.
//!
//! # Link pacing
//!
//! The sender owns a [`LinkPacer`]: while frames flow it interleaves tagged
//! probe PINGs, and the PONGs (routed back via [`PrioritySender::on_pong`])
//! give it a delivery rate and min RTT. Frame records are only released
//! while the estimated bytes in flight stay within `rate x (min RTT + 15
//! ms)`, so the kernel/relay buffers never build a standing queue in front
//! of control records (see [`crate::pacing`]).

#[cfg(doc)]
use crate::{pacing::LinkPacer, terminal::TerminalSession};

/// Default FRAME_DATA chunk size: small enough that one chunk occupies a
/// 10 Mbps link for only ~3 ms (the most a control message waits behind
/// frame data at the application layer).
pub const DEFAULT_CHUNK: usize = 4096;

mod encoder;
mod mailbox;
mod refine;
mod sender;

#[cfg(test)]
mod tests;

pub use encoder::*;
pub use mailbox::*;
pub use sender::*;
