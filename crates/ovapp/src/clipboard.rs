//! Clipboard sync state (v0.26 "bidirectional clipboard sync").
//!
//! The wire side is the `CLIPBOARD` command (opcode `0x50`, see
//! [`crate::terminal`]). This module holds the *pure* half of the feature:
//! the echo-loop guard that decides whether a clipboard change observed
//! locally must be sent to the peer, and whether an update received from the
//! peer must be applied. Like [`crate::audio`] and [`crate::file_transfer`],
//! everything here is platform-independent (no OS calls) so it is unit-testable
//! and reusable on iOS via `ovffi`.
//!
//! ## The echo-loop problem
//!
//! Both endpoints poll their own OS clipboard and send any change. When A's
//! "hi" arrives at B, B writes it to B's clipboard — which B's own poller now
//! sees as a *local* change and would send straight back to A, which would
//! apply it, see it change, send it back… forever.
//!
//! The fix: each endpoint remembers
//! the last text it received from the peer ([`ClipboardSync::last_remote`]).
//! A local observation that matches either what we already knew
//! ([`ClipboardSync::last_seen_local`]) or what we just applied from the peer
//! ([`ClipboardSync::last_remote`]) is an echo — not a change — and is
//! swallowed. Only a genuinely new local value is sent, and only once.
//!
//! Clearing (empty text) is just another value and gets the same guard, so a
//! clear propagates exactly once too.

/// Pure echo-loop-safe clipboard sync state for one endpoint of a session.
///
/// The shell drives it each loop iteration:
///
/// 1. read the OS clipboard text; call [`note_local`](Self::note_local) with
///    it. A `Some(text)` return means "send a `CLIPBOARD` command with
///    `text`".
/// 2. on an inbound `CLIPBOARD`, apply the text to the OS clipboard, then
///    call [`note_remote`](Self::note_remote) so the very next `note_local`
///    observation of that same text is recognised as the echo it is.
///
/// No I/O, no clocks, no threads: the caller owns all of that.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClipboardSync {
    /// The last text we observed on the local OS clipboard (whether or not we
    /// sent it). Drives "changed at all" detection.
    last_seen_local: String,
    /// The last text received from the peer and applied locally, if any
    /// (`None` until the first peer update arrives — a fresh endpoint must
    /// not mistake a local *clear* for an echo of an empty baseline).
    /// Local observations equal to this are echoes of the peer's own update.
    last_remote: Option<String>,
    /// How many local changes were judged worth sending (tests/diagnostics).
    pub sent_count: u64,
    /// How many peer updates were applied (tests/diagnostics).
    pub applied_count: u64,
}

impl ClipboardSync {
    /// A fresh sync state. Both endpoints start with empty clipboards, so the
    /// initial "nothing" observation sends nothing — matching a session that
    /// begins with both sides empty.
    pub fn new() -> Self {
        Self::default()
    }

    /// The last text applied from the peer, if any (also useful for UI
    /// "last synced" display and for tests).
    pub fn last_remote(&self) -> Option<&str> {
        self.last_remote.as_deref()
    }

    /// The last text observed on the local clipboard (tests/diagnostics).
    pub fn last_seen_local(&self) -> &str {
        &self.last_seen_local
    }

    /// Observe the current local clipboard text.
    ///
    /// Returns `Some(text)` exactly when this is a genuine local change that
    /// must be sent to the peer: the text differs from what we last observed
    /// locally *and* is not the echo of an update we just applied from the
    /// peer. `last_seen_local` is always updated, so observing the same text
    /// again returns `None` (a clipboard poll is level-triggered, not
    /// edge-triggered, and must not re-send).
    pub fn note_local(&mut self, text: &str) -> Option<String> {
        if text == self.last_seen_local || self.last_remote.as_deref() == Some(text) {
            self.last_seen_local = text.to_owned();
            return None;
        }
        self.last_seen_local = text.to_owned();
        self.sent_count += 1;
        Some(text.to_owned())
    }

    /// Record a peer `CLIPBOARD` update that has been (or is about to be)
    /// applied to the local OS clipboard. The next [`note_local`](Self::note_local)
    /// observation of the same text is then recognised as an echo and
    /// swallowed, breaking the loop.
    pub fn note_remote(&mut self, text: &str) {
        if self.last_remote.as_deref() != Some(text) {
            self.applied_count += 1;
        }
        self.last_remote = Some(text.to_owned());
        // The peer's value is now what our clipboard holds; treat it as also
        // locally observed so a poll before/after the write is a no-op.
        self.last_seen_local = text.to_owned();
    }
}

/// Build the `CLIPBOARD` command for local clipboard `text`, truncated (on
/// a UTF-8 boundary) to [`crate::terminal::MAX_TEXT_PAYLOAD`] so it always
/// fits one record — an over-long clipboard is clipped rather than failing
/// the send (which would otherwise end the session).
pub fn clipboard_command(text: &str) -> crate::terminal::Command {
    let clipped = crate::terminal::truncate_utf8(text, crate::terminal::MAX_TEXT_PAYLOAD);
    crate::terminal::Command::Clipboard {
        kind: 0,
        text: clipped.as_bytes().to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::ClipboardSync;

    #[test]
    fn local_change_sends_once() {
        let mut s = ClipboardSync::new();
        // First observation of a non-empty value is a change.
        assert_eq!(s.note_local("hi").as_deref(), Some("hi"));
        // Polling again at the same value must not re-send.
        assert_eq!(s.note_local("hi"), None);
        assert_eq!(s.note_local("hi"), None);
        assert_eq!(s.sent_count, 1);
    }

    #[test]
    fn echo_loop_is_broken() {
        // A sends "hi"; B receives, applies, then its own poller observes it.
        let mut a = ClipboardSync::new();
        let mut b = ClipboardSync::new();
        let send = a.note_local("hi").expect("A must send its change");
        b.note_remote(&send); // B applies "hi" to its OS clipboard.
        assert_eq!(b.note_local("hi"), None, "B must not echo A's text back");
        assert_eq!(b.sent_count, 0);
        // And A, having observed its own sent value in its own clipboard,
        // stays quiet too.
        a.note_remote(&send); // A applies B's (identical) value — no-op.
        assert_eq!(a.note_local("hi"), None);
    }

    #[test]
    fn distinct_new_local_change_sends() {
        // After receiving "hi" from the peer, a genuinely new local value
        // must still be sent (the guard must not over-suppress).
        let mut s = ClipboardSync::new();
        s.note_remote("hi");
        assert_eq!(s.note_local("hi"), None, "echo of the peer's value");
        assert_eq!(s.note_local("bye").as_deref(), Some("bye"));
        assert_eq!(s.note_local("bye"), None);
        assert_eq!(s.sent_count, 1);
    }

    #[test]
    fn clear_propagates_and_is_echo_guarded() {
        // Empty text = clear; it is a value like any other and must
        // propagate exactly once, and not bounce back.
        let mut a = ClipboardSync::new();
        let mut b = ClipboardSync::new();
        a.note_local("hi");
        b.note_remote("hi");
        let clear = a.note_local("").expect("clear must be sent");
        assert_eq!(clear, "");
        b.note_remote("");
        assert_eq!(b.note_local(""), None, "B must not echo the clear back");
        assert_eq!(a.note_local(""), None, "A must not re-send its own clear");
    }

    #[test]
    fn two_endpoints_converge_without_oscillation() {
        // Drive a full bidirectional session over a fake channel: whatever
        // one endpoint's note_local returns is delivered to the other's
        // note_remote. Neither side may ever send the same value twice, and
        // the exchange must terminate (no ping-pong).
        let mut a = ClipboardSync::new();
        let mut b = ClipboardSync::new();
        let mut wire: Vec<(usize, String)> = Vec::new(); // (sender, text)

        // A sets "echo-test".
        if let Some(t) = a.note_local("echo-test") {
            wire.push((0, t));
        }
        // Deliver everything, letting each side also observe its own
        // clipboard (which now holds the applied text) — the echo trap.
        for round in 0..10 {
            let inflight = std::mem::take(&mut wire);
            for (from, text) in &inflight {
                let (rx, tx) = if *from == 0 {
                    (&mut b, &mut a)
                } else {
                    (&mut a, &mut b)
                };
                rx.note_remote(text);
                // The receiver's OS clipboard now holds `text`; its poller
                // sees it. It must not send it back.
                assert_eq!(rx.note_local(text), None, "echo in round {round}");
                // The sender likewise re-observes its own clipboard.
                assert_eq!(tx.note_local(text), None, "sender re-send in round {round}");
            }
            assert!(
                wire.is_empty(),
                "no new traffic may appear in round {round}"
            );
        }
        // B ended up with A's text, exactly once, and nothing oscillated.
        assert_eq!(b.last_remote(), Some("echo-test"));
        assert_eq!(a.sent_count, 1);
        assert_eq!(b.sent_count, 0);
        assert_eq!(b.applied_count, 1);
        assert_eq!(a.applied_count, 0);

        // A later local change on B flows back the other way, once.
        if let Some(t) = b.note_local("reply text") {
            wire.push((1, t));
        }
        let inflight = std::mem::take(&mut wire);
        for (from, text) in &inflight {
            let (rx, tx) = if *from == 0 {
                (&mut b, &mut a)
            } else {
                (&mut a, &mut b)
            };
            rx.note_remote(text);
            assert_eq!(rx.note_local(text), None);
            assert_eq!(tx.note_local(text), None);
        }
        assert_eq!(a.last_remote(), Some("reply text"));
        assert_eq!(b.sent_count, 1);
        assert_eq!(a.sent_count, 1);
    }

    #[test]
    fn clipboard_command_clips_oversized_text_to_one_record() {
        let big = "é".repeat(50_000); // 100 000 bytes
        let cmd = super::clipboard_command(&big);
        let wire = crate::terminal::encode(&cmd);
        assert!(wire.len() <= crate::terminal::MAX_COMMAND_LEN);
        let crate::terminal::Command::Clipboard { text, .. } = cmd else {
            unreachable!()
        };
        assert!(
            std::str::from_utf8(&text).is_ok(),
            "clipped on a char boundary"
        );
        assert!(text.len() <= crate::terminal::MAX_TEXT_PAYLOAD);
        // Small text is untouched.
        let crate::terminal::Command::Clipboard { text, .. } = super::clipboard_command("hi")
        else {
            unreachable!()
        };
        assert_eq!(text, b"hi");
    }
}
