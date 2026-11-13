//! Session liveness (v0.16): keepalive pacing, RTT measurement, dead-peer
//! detection and link-health statistics.
//!
//! A low-rate PING keeps the
//! session alive when the screen is idle (change detection means a static
//! screen sends **no** frames, so without keepalive a quiet session cannot be
//! distinguished from a dead peer), and the echoed PING nonce measures RTT.
//!
//! Everything here is pure — it takes `Instant`s in and makes no syscalls —
//! so the host/viewer/FFI loops can wire it up and the tests can drive it
//! with synthetic clocks.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Keepalive PING interval (both directions). ~5 s keeps an idle session
/// comfortably inside the dead window at trivial wire cost (one 9-byte
/// record per direction per interval).
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);

/// Declare the peer dead after this long with **no inbound record of any
/// kind** (PONG, frame, input, ...). 15 s = 3 missed keepalives: one lost
/// PING/PONG must not kill a healthy session.
pub const DEAD_AFTER: Duration = Duration::from_secs(15);

/// End the session after this long with no inbound record of any kind
/// (2 x [`DEAD_AFTER`]): the peer is first reported dead, and if nothing
/// revives it the loop gives the session up instead of idling forever on a
/// silently-dead path.
pub const SESSION_TIMEOUT: Duration = Duration::from_secs(30);

/// Exponential moving average factor for the smoothed RTT (0..=1]. Each
/// sample moves the average 25% toward the new value — smooths out the
/// jitter of a single round trip without hiding real changes.
pub const RTT_SMOOTHING: f64 = 0.25;

/// Keepalive pacing + RTT measurement over the PING/PONG command pair.
///
/// The wire format is the existing one: PING carries an 8-byte u64-BE nonce
/// and PONG echoes it back. We use a monotonically increasing sequence
/// number as the nonce and remember when each PING was sent, so the echoed
/// sequence measures the round trip. Only the most recent PING is matched —
/// a PONG for an older (already superseded or lost) sequence is stale and
/// ignored.
#[derive(Debug, Clone)]
pub struct Keepalive {
    interval: Duration,
    /// Monotonic PING sequence (the wire nonce).
    seq: u64,
    /// The in-flight PING: `(sequence, send time)`. `None` until the first
    /// send; overwritten by each new PING (an unanswered older one is lost).
    outstanding: Option<(u64, Instant)>,
    /// When the next PING is due.
    next_send: Instant,
    /// Last measured round trip.
    last_rtt: Option<Duration>,
    /// EWMA of the measured RTTs (seconds).
    smooth_rtt: Option<f64>,
}

impl Keepalive {
    /// Start a keepalive clock: the first PING is due `interval` from `now`.
    pub fn new(interval: Duration, now: Instant) -> Self {
        Self {
            interval,
            seq: 0,
            outstanding: None,
            next_send: now + interval,
            last_rtt: None,
            smooth_rtt: None,
        }
    }

    /// If a PING is due at `now`, send it: returns the sequence to put on the
    /// wire (and marks it in flight). `None` when the interval has not
    /// elapsed yet.
    pub fn tick(&mut self, now: Instant) -> Option<u64> {
        if now < self.next_send {
            return None;
        }
        self.next_send = now + self.interval;
        self.seq = self.seq.wrapping_add(1);
        self.outstanding = Some((self.seq, now));
        Some(self.seq)
    }

    /// Feed a PONG (the echoed PING sequence) received at `now`. Returns the
    /// measured RTT when the echo matches the in-flight PING; `None` for a
    /// stale/duplicate/unsolicited echo.
    pub fn on_pong(&mut self, seq: u64, now: Instant) -> Option<Duration> {
        let (sent_seq, sent_at) = self.outstanding?;
        if sent_seq != seq {
            return None; // stale echo for a superseded PING
        }
        self.outstanding = None;
        let rtt = now.saturating_duration_since(sent_at);
        self.last_rtt = Some(rtt);
        let secs = rtt.as_secs_f64();
        self.smooth_rtt = Some(match self.smooth_rtt {
            Some(prev) => prev * (1.0 - RTT_SMOOTHING) + secs * RTT_SMOOTHING,
            None => secs,
        });
        Some(rtt)
    }

    /// Time left until the next PING is due (zero when overdue) — the
    /// longest an event-driven loop may block without missing it.
    pub fn until_due(&self, now: Instant) -> Duration {
        self.next_send.saturating_duration_since(now)
    }

    /// The last measured RTT (raw), if any PONG has matched.
    pub fn last_rtt(&self) -> Option<Duration> {
        self.last_rtt
    }

    /// The smoothed RTT (EWMA over [`RTT_SMOOTHING`]), if any PONG has matched.
    pub fn smoothed_rtt(&self) -> Option<Duration> {
        self.smooth_rtt.map(Duration::from_secs_f64)
    }

    /// Whether a PING is currently unanswered.
    pub fn awaiting_pong(&self) -> bool {
        self.outstanding.is_some()
    }
}

/// Dead-peer detection: track the last inbound record of ANY kind and decide
/// whether the peer should be considered gone.
///
/// Any inbound traffic (a screen frame, a PONG, a keystroke echo) counts —
/// so an actively streaming session stays alive even if PONGs are dropped,
/// and the keepalive PING is what keeps a *static* screen alive.
#[derive(Debug, Clone)]
pub struct Liveness {
    /// Monotonic time of the last inbound record. Seeded with the session's
    /// start so a freshly established session is never born dead.
    last_inbound: Instant,
    /// Last cumulative inbound counter seen by [`observe_count`](Self::observe_count).
    last_count: u64,
}

impl Liveness {
    /// Start tracking at `now` (session establishment).
    pub fn new(now: Instant) -> Self {
        Self {
            last_inbound: now,
            last_count: 0,
        }
    }

    /// Feed a cumulative inbound counter (e.g. `TerminalStats::cmds_rx`):
    /// liveness refreshes only when it **changed** since the last call — a
    /// counter that is merely non-zero says nothing about the peer now.
    /// Returns `true` when new traffic was seen.
    pub fn observe_count(&mut self, count: u64, now: Instant) -> bool {
        if count == self.last_count {
            return false;
        }
        self.last_count = count;
        self.last_inbound = now;
        true
    }

    /// Record inbound traffic of any kind at `now`. Also clears a dead
    /// state implicitly: the next `is_dead` simply computes from the new
    /// timestamp, so resumed traffic revives the session.
    pub fn record_inbound(&mut self, now: Instant) {
        self.last_inbound = now;
    }

    /// True when no inbound record has been seen for `dead_after`. This is
    /// the dead-peer verdict.
    pub fn is_dead(&self, now: Instant, dead_after: Duration) -> bool {
        now.saturating_duration_since(self.last_inbound) > dead_after
    }

    /// Time since the last inbound record (for display).
    pub fn since_inbound(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.last_inbound)
    }
}

/// Rolling inbound-frame statistics for the link-health HUD: frames/s and
/// payload bytes/s over a sliding sample window.
///
/// Samples are `(time, frame bytes)`; `snapshot` prunes samples older than
/// the window, so a stalled stream decays to zero instead of freezing at its
/// last rate.
#[derive(Debug, Clone)]
pub struct LinkStats {
    window: Duration,
    samples: VecDeque<(Instant, u64)>,
}

impl LinkStats {
    /// A rolling window of `window` (e.g. 2 s).
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            samples: VecDeque::new(),
        }
    }

    /// Record one inbound frame of `bytes` payload at `now`.
    pub fn frame_rx(&mut self, now: Instant, bytes: u64) {
        self.samples.push_back((now, bytes));
    }

    /// Prune and summarize: `(frames per second, payload bytes per second)`
    /// over the window. Both are `0.0` when nothing arrived inside it.
    pub fn snapshot(&mut self, now: Instant) -> (f64, f64) {
        let cutoff = now - self.window;
        while let Some(&(t, _)) = self.samples.front() {
            if t < cutoff {
                self.samples.pop_front();
            } else {
                break;
            }
        }
        if self.samples.is_empty() {
            return (0.0, 0.0);
        }
        // The covered span is from the oldest surviving sample to now,
        // clamped to the window and to a small floor so a burst of frames
        // in one instant cannot divide by ~0.
        let span = now
            .saturating_duration_since(self.samples.front().expect("non-empty").0)
            .min(self.window)
            .max(Duration::from_millis(50))
            .as_secs_f64();
        let (frames, bytes) = self
            .samples
            .iter()
            .fold((0u64, 0u64), |(f, b), &(_, sz)| (f + 1, b + sz));
        (frames as f64 / span, bytes as f64 / span)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn cumulative_counter_refreshes_only_on_change() {
        let t = t0();
        let mut l = Liveness::new(t);
        // First traffic: the counter moved, the peer is alive.
        assert!(l.observe_count(3, t + Duration::from_secs(1)));
        // The counter stays non-zero but frozen: that is NOT fresh traffic,
        // so the peer goes dead after DEAD_AFTER (the old `> 0` check kept
        // it alive forever after the first command).
        for s in 2..40 {
            assert!(!l.observe_count(3, t + Duration::from_secs(s)));
        }
        assert!(l.is_dead(t + Duration::from_secs(17), DEAD_AFTER));
        assert!(l.is_dead(t + Duration::from_secs(32), SESSION_TIMEOUT));
        // New traffic revives it.
        assert!(l.observe_count(4, t + Duration::from_secs(40)));
        assert!(!l.is_dead(t + Duration::from_secs(41), DEAD_AFTER));
    }

    #[test]
    fn session_timeout_is_a_multiple_of_dead_after() {
        assert!(SESSION_TIMEOUT >= DEAD_AFTER * 2);
    }

    #[test]
    fn keepalive_ticks_at_the_interval() {
        let t = t0();
        let mut ka = Keepalive::new(Duration::from_secs(5), t);
        assert_eq!(ka.tick(t), None, "first PING is due one interval out");
        assert_eq!(ka.tick(t + Duration::from_secs(4)), None);
        let seq = ka.tick(t + Duration::from_secs(5)).expect("due now");
        assert_eq!(seq, 1, "sequences start at 1 and increase");
        assert_eq!(ka.tick(t + Duration::from_secs(6)), None);
        assert_eq!(ka.tick(t + Duration::from_secs(10)), Some(2));
    }

    #[test]
    fn pong_echo_measures_rtt() {
        let t = t0();
        let mut ka = Keepalive::new(KEEPALIVE_INTERVAL, t);
        let seq = ka.tick(t + Duration::from_secs(5)).unwrap();
        // PONG 120 ms later.
        let rtt = ka
            .on_pong(seq, t + Duration::from_secs(5) + Duration::from_millis(120))
            .expect("matching echo measures RTT");
        assert_eq!(rtt, Duration::from_millis(120));
        assert_eq!(ka.last_rtt(), Some(Duration::from_millis(120)));
        assert_eq!(ka.smoothed_rtt(), Some(Duration::from_millis(120)));
        assert!(!ka.awaiting_pong(), "echo clears the in-flight PING");
    }

    #[test]
    fn stale_or_unsolicited_pongs_are_ignored() {
        let t = t0();
        let mut ka = Keepalive::new(KEEPALIVE_INTERVAL, t);
        assert_eq!(
            ka.on_pong(99, t),
            None,
            "PONG with no PING in flight is ignored"
        );
        let s1 = ka.tick(t + Duration::from_secs(5)).unwrap();
        let s2 = ka.tick(t + Duration::from_secs(10)).unwrap();
        // s2 superseded s1: an echo of s1 is stale.
        assert_eq!(ka.on_pong(s1, t + Duration::from_secs(11)), None);
        assert!(
            ka.awaiting_pong(),
            "stale echo must not clear the in-flight PING"
        );
        let rtt = ka.on_pong(s2, t + Duration::from_secs(11)).unwrap();
        assert!(rtt > Duration::ZERO, "matching echo still measures");
        // A duplicate echo of the same sequence is also stale.
        assert_eq!(ka.on_pong(s2, t + Duration::from_secs(12)), None);
    }

    #[test]
    fn smoothed_rtt_is_an_ewma() {
        let t = t0();
        let mut ka = Keepalive::new(KEEPALIVE_INTERVAL, t);
        let s1 = ka.tick(t + Duration::from_secs(5)).unwrap();
        ka.on_pong(s1, t + Duration::from_secs(5) + Duration::from_millis(100));
        let s2 = ka.tick(t + Duration::from_secs(10)).unwrap();
        ka.on_pong(s2, t + Duration::from_secs(10) + Duration::from_millis(300));
        // EWMA: 100 * 0.75 + 300 * 0.25 = 150 ms.
        assert_eq!(ka.smoothed_rtt(), Some(Duration::from_millis(150)));
    }

    #[test]
    fn liveness_declares_dead_after_the_window_and_recovers() {
        let t = t0();
        let mut live = Liveness::new(t);
        assert!(!live.is_dead(t + Duration::from_secs(14), DEAD_AFTER));
        assert!(live.is_dead(t + Duration::from_secs(16), DEAD_AFTER));
        // Any inbound traffic revives the session.
        live.record_inbound(t + Duration::from_secs(16));
        assert!(!live.is_dead(t + Duration::from_secs(16), DEAD_AFTER));
        assert!(live.is_dead(t + Duration::from_secs(32), DEAD_AFTER));
        assert_eq!(
            live.since_inbound(t + Duration::from_secs(17)),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn link_stats_reports_fps_and_bandwidth_over_the_window() {
        let t = t0();
        let mut stats = LinkStats::new(Duration::from_secs(2));
        assert_eq!(stats.snapshot(t + Duration::from_secs(1)), (0.0, 0.0));
        // 10 frames of 1000 bytes over 1 second.
        for i in 0..10 {
            stats.frame_rx(t + Duration::from_millis(100 * i), 1000);
        }
        let (fps, bps) = stats.snapshot(t + Duration::from_millis(1000));
        assert!((fps - 10.0).abs() < 0.5, "fps {fps}");
        assert!((bps - 10_000.0).abs() < 500.0, "bytes/s {bps}");
    }

    #[test]
    fn link_stats_decays_after_the_stream_stalls() {
        let t = t0();
        let mut stats = LinkStats::new(Duration::from_secs(2));
        stats.frame_rx(t, 8000);
        // 3 s later the sample has fallen out of the 2 s window.
        let (fps, bps) = stats.snapshot(t + Duration::from_secs(3));
        assert_eq!((fps, bps), (0.0, 0.0), "stale samples must be pruned");
    }
}
