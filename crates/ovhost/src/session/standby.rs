//! Standby park: while an unattended session runs, the host keeps a second
//! responder park on the relay under its (stable) session id, so a viewer
//! dialing meanwhile is attached at once instead of waiting for the host to
//! notice that its current viewer is gone.
//!
//! The host serves one session at a time. A viewer that vanishes without a
//! goodbye (app killed, phone switched networks, a VPN dropping the relay
//! socket while the direct path carries the session) is otherwise only
//! given up after the liveness timeout, and only then would the host park
//! again. With the standby park:
//!
//! 1. a background thread holds the park (re-parking after the relay's idle
//!    timeout, backing off while the relay is unreachable);
//! 2. when a viewer attaches to it, the session loop probes its current
//!    viewer ([`Takeover`]: PINGs every [`PROBE_EVERY`]) and ends the session
//!    if that viewer has not been heard from for [`TAKEOVER_GRACE`] — a
//!    live viewer answers within one round trip and keeps its session;
//! 3. the next `listen_once` runs the handshake on the already-attached
//!    carrier ([`Handoff::Attached`]), or keeps the still-parked one
//!    ([`Handoff::Parked`]) so the relay never sees a gap.
//!
//! Attended hosts have no standby: their one-time pairing code (and with it
//! the relay session id) is burned by the session that used it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ovcommon::relay_addr::RelayAddr;
use ovtransport::{Carrier, RelayTunnelCarrier, TunnelRole};
use tracing::debug;

use super::{dial, MAX_RELAY_RETRY, MIN_PARK, PARK_WAIT};

/// How long the current viewer may stay silent once another viewer is
/// waiting before its session is ended in the newcomer's favour.
pub(super) const TAKEOVER_GRACE: Duration = Duration::from_millis(2500);
/// PING cadence toward the current viewer while another one waits.
pub(super) const PROBE_EVERY: Duration = Duration::from_millis(500);
/// Most datagrams buffered from a waiting viewer (its handshake needs one
/// or two); more means it is not a viewer waiting for us.
const MAX_BACKLOG: usize = 16;
/// How often the standby thread checks for stop / the waiting viewer.
const POLL: Duration = Duration::from_millis(50);
/// How long [`Standby::finish`] waits for the thread to hand over.
const FINISH_WAIT: Duration = Duration::from_millis(500);

/// What a finished standby hands to the next session.
pub(crate) enum Handoff {
    /// Still parked, nobody attached yet: keep the park (`since` = when it
    /// started, for the park window).
    Parked {
        carrier: Box<RelayTunnelCarrier>,
        since: Instant,
    },
    /// A viewer attached and waits for the handshake; `backlog` holds what
    /// it already sent (its HELLO).
    Attached {
        carrier: Box<RelayTunnelCarrier>,
        backlog: Vec<Vec<u8>>,
    },
}

/// The standby park thread of one running session.
pub(crate) struct Standby {
    stop: Arc<AtomicBool>,
    waiting: Arc<AtomicBool>,
    thread: Option<JoinHandle<Option<Handoff>>>,
}

impl Standby {
    /// Start holding a responder park for `session_id` on `relay`.
    pub(crate) fn start(relay: RelayAddr, session_id: u32) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let waiting = Arc::new(AtomicBool::new(false));
        let thread = {
            let (stop, waiting) = (Arc::clone(&stop), Arc::clone(&waiting));
            std::thread::Builder::new()
                .name("ovhost-standby".into())
                .spawn(move || standby_loop(&relay, session_id, &stop, &waiting))
                .map_err(|e| debug!("standby park unavailable: {e}"))
                .ok()
        };
        Self {
            stop,
            waiting,
            thread,
        }
    }

    /// A viewer attached to the standby park and waits for this session to
    /// end.
    pub(crate) fn viewer_waiting(&self) -> bool {
        self.waiting.load(Ordering::Relaxed)
    }

    /// Stop the thread and take its park (or waiting viewer) for the next
    /// session. `None` when there is nothing to hand over or the thread is
    /// stuck dialing (it then closes what it gets on its own).
    pub(crate) fn finish(mut self) -> Option<Handoff> {
        self.stop.store(true, Ordering::Relaxed);
        let thread = self.thread.take()?;
        let deadline = Instant::now() + FINISH_WAIT;
        while !thread.is_finished() {
            if Instant::now() >= deadline {
                return None; // detached: it exits after its dial returns
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        thread.join().ok().flatten()
    }
}

impl Drop for Standby {
    fn drop(&mut self) {
        // Not joined: a dial in progress may take seconds. The thread sees
        // the flag and closes whatever it holds.
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Sleep up to `d`, waking early on `stop`. Returns `true` when stopped.
fn nap(d: Duration, stop: &AtomicBool) -> bool {
    let end = Instant::now() + d;
    while Instant::now() < end {
        if stop.load(Ordering::Relaxed) {
            return true;
        }
        std::thread::sleep(POLL.min(end.saturating_duration_since(Instant::now())));
    }
    stop.load(Ordering::Relaxed)
}

/// Park, re-park, and hold an attached viewer until stopped.
fn standby_loop(
    relay: &RelayAddr,
    session_id: u32,
    stop: &AtomicBool,
    waiting: &AtomicBool,
) -> Option<Handoff> {
    let mut retry = Duration::from_secs(1);
    while !stop.load(Ordering::Relaxed) {
        let since = Instant::now();
        let mut carrier = match dial(relay, session_id, TunnelRole::Responder) {
            Ok(c) => {
                retry = Duration::from_secs(1);
                c
            }
            Err(e) => {
                debug!("standby park: relay {relay}: {e}; retrying in {retry:?}");
                if nap(retry, stop) {
                    return None;
                }
                retry = (retry * 2).min(MAX_RELAY_RETRY);
                continue;
            }
        };
        if stop.load(Ordering::Relaxed) {
            return Some(Handoff::Parked {
                carrier: Box::new(carrier),
                since,
            });
        }
        // Parked: wait for a viewer in short slices (stop stays responsive).
        let attached = loop {
            if stop.load(Ordering::Relaxed) {
                return Some(Handoff::Parked {
                    carrier: Box::new(carrier),
                    since,
                });
            }
            match carrier.wait_attached(POLL) {
                Ok(true) => break true,
                Ok(false) if since.elapsed() < PARK_WAIT => {}
                // Relay silent for the whole window, or it timed the park
                // out (`XT`): re-park.
                Ok(false) | Err(_) => break false,
            }
        };
        if !attached {
            let _ = carrier.close();
            if since.elapsed() < MIN_PARK && nap(Duration::from_secs(1), stop) {
                return None;
            }
            continue;
        }
        debug!("standby park: a viewer is waiting");
        waiting.store(true, Ordering::Relaxed);
        let mut backlog = Vec::new();
        loop {
            if stop.load(Ordering::Relaxed) {
                return Some(Handoff::Attached {
                    carrier: Box::new(carrier),
                    backlog,
                });
            }
            let alive = carrier.poll_recv(&mut |d| backlog.push(d.to_vec())).is_ok();
            if !alive || backlog.len() > MAX_BACKLOG {
                // The waiting viewer gave up (or is not a viewer): park
                // again for the next one.
                debug!("standby park: the waiting viewer left");
                waiting.store(false, Ordering::Relaxed);
                let _ = carrier.close();
                break;
            }
            std::thread::sleep(POLL);
        }
    }
    None
}

/// The session loop's side of a standby park: probe the current viewer
/// while another one waits, and decide when the session must make way.
#[derive(Debug, Default)]
pub(super) struct Takeover {
    /// When the current probe round started (`None` = nobody waiting).
    round: Option<Instant>,
    next_probe: Option<Instant>,
    seq: u64,
}

/// What the session loop does this iteration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TakeoverStep {
    /// Nothing to do.
    Idle,
    /// Send `PING(nonce)` to the current viewer.
    Probe(u64),
    /// The current viewer is gone: end the session for the waiting one.
    End,
}

impl Takeover {
    /// One step. `waiting` = a viewer waits on the standby park;
    /// `since_inbound` = time since the current viewer was last heard.
    pub(super) fn step(
        &mut self,
        waiting: bool,
        since_inbound: Duration,
        now: Instant,
    ) -> TakeoverStep {
        if !waiting {
            *self = Self {
                seq: self.seq,
                ..Self::default()
            };
            return TakeoverStep::Idle;
        }
        let round = *self.round.get_or_insert(now);
        let probing = now.saturating_duration_since(round);
        if since_inbound < probing {
            // Heard from since the round began: the viewer is alive and
            // keeps its session; check again in a fresh round.
            self.round = Some(now);
        } else if since_inbound >= TAKEOVER_GRACE {
            // Silent for the whole grace (possibly long before anyone
            // waited: a dead carrier ends at once).
            return TakeoverStep::End;
        }
        if self.next_probe.is_none_or(|t| now >= t) {
            self.next_probe = Some(now + PROBE_EVERY);
            self.seq = self.seq.wrapping_add(1);
            // Far from the keepalive / pacer sequence space, so neither
            // mistakes the echo for its own.
            return TakeoverStep::Probe(u64::MAX - self.seq);
        }
        TakeoverStep::Idle
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_silent_viewer_makes_way_after_the_grace_a_live_one_stays() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut t = Takeover::default();
        assert_eq!(t.step(false, ms(10_000), t0), TakeoverStep::Idle);
        // Someone waits; the current viewer was heard 100 ms ago: probe.
        assert!(matches!(t.step(true, ms(100), t0), TakeoverStep::Probe(_)));
        // Silent since, but the grace has not run out yet; the next probe
        // goes out on its cadence.
        assert_eq!(t.step(true, ms(400), t0 + ms(300)), TakeoverStep::Idle);
        assert!(matches!(
            t.step(true, ms(1100), t0 + ms(1000)),
            TakeoverStep::Probe(_)
        ));
        assert_eq!(
            t.step(true, TAKEOVER_GRACE, t0 + TAKEOVER_GRACE - ms(100)),
            TakeoverStep::End
        );

        // A live viewer answers the probe: never ended.
        let mut t = Takeover::default();
        let mut now = t0;
        for _ in 0..100 {
            assert_ne!(t.step(true, ms(40), now), TakeoverStep::End);
            now += ms(50);
        }
    }

    #[test]
    fn an_already_dead_viewer_makes_way_at_once() {
        let mut t = Takeover::default();
        assert_eq!(
            t.step(true, Duration::from_secs(5), Instant::now()),
            TakeoverStep::End
        );
    }
}
