//! Peer liveness: keepalive PONG bookkeeping and the dead / gone verdicts.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use ovapp::liveness::{Keepalive, DEAD_AFTER, SESSION_TIMEOUT};
use ovcommon::status::{self, StatusEvent};
use tracing::info;

use super::hooks::HostHooks;

/// Feed the PONG echoes the hook collected into our keepalive; a matched
/// echo updates the smoothed RTT.
pub(super) fn absorb_pongs(hooks: &HostHooks, keepalive: &mut Keepalive) {
    absorb_pongs_with(hooks, keepalive, |_| false);
}

/// [`absorb_pongs`], first offering each echo to `pacer` (the screen
/// sender's probe PINGs come back as PONGs too; it consumes its own).
pub(super) fn absorb_pongs_with(
    hooks: &HostHooks,
    keepalive: &mut Keepalive,
    mut pacer: impl FnMut(u64) -> bool,
) {
    let pongs = hooks
        .pong_seq
        .lock()
        .map(|mut q| std::mem::take(&mut *q))
        .unwrap_or_default();
    for seq in pongs {
        if pacer(seq) {
            continue;
        }
        if let Some(rtt) = keepalive.on_pong(seq, Instant::now()) {
            hooks
                .rtt_us
                .store(rtt.as_micros() as u64, Ordering::Relaxed);
        }
    }
}

/// Peer-liveness verdict for one loop iteration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PeerState {
    Alive,
    /// No inbound traffic for [`DEAD_AFTER`]: stop producing media, keep
    /// the session in case traffic resumes.
    Dead,
    /// No inbound traffic for [`SESSION_TIMEOUT`]: give the session up.
    Gone,
}

/// Classify the time since the last inbound record.
fn peer_state(since_inbound: Duration) -> PeerState {
    if since_inbound > SESSION_TIMEOUT {
        PeerState::Gone
    } else if since_inbound > DEAD_AFTER {
        PeerState::Dead
    } else {
        PeerState::Alive
    }
}

/// Track liveness transitions and log them once. Returns the new state.
pub(super) fn check_peer(hooks: &HostHooks, was: PeerState) -> PeerState {
    let since = hooks
        .liveness
        .lock()
        .map(|l| l.since_inbound(Instant::now()))
        .unwrap_or_default();
    let now = peer_state(since);
    match (was, now) {
        (PeerState::Alive, PeerState::Dead) => {
            info!("peer silent for {DEAD_AFTER:?}; pausing the stream")
        }
        (PeerState::Dead, PeerState::Alive) => info!("peer liveness recovered"),
        (_, PeerState::Gone) => {
            session_over(&format!("peer silent for {SESSION_TIMEOUT:?}"));
        }
        _ => {}
    }
    now
}

/// The session is over: log it and tell the GUI (status channel).
pub(super) fn session_over(reason: &str) {
    info!("peer disconnected ({reason})");
    status::emit(&StatusEvent::PeerDisconnected {
        reason: reason.to_owned(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silent_peer_goes_dead_then_ends_the_session() {
        assert_eq!(peer_state(Duration::from_secs(1)), PeerState::Alive);
        assert_eq!(
            peer_state(DEAD_AFTER + Duration::from_millis(1)),
            PeerState::Dead
        );
        assert_eq!(
            peer_state(SESSION_TIMEOUT - Duration::from_millis(1)),
            PeerState::Dead
        );
        assert_eq!(
            peer_state(SESSION_TIMEOUT + Duration::from_millis(1)),
            PeerState::Gone
        );
    }
}
