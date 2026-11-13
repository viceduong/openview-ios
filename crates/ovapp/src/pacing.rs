//! Delay-based link pacing for the host's screen sender.
//!
//! # Problem
//!
//! On a saturated link the host's frame data does not queue in *our*
//! scheduler but below it: in the kernel send buffer, the relay's socket
//! buffers and the bottleneck (~250 KB on the measured relay path). Every
//! PONG, cursor update and audio tick then waits behind that standing
//! queue: a relay-path bench measured a 241 ms PONG RTT at 10 Mbps (47 ms at
//! 50 Mbps) although `PrioritySender` already sends control first.
//!
//! # Approach (BBR/LEDBAT-like, deliberately simple)
//!
//! - **Acks from PING/PONG.** While frames flow, the sender slips a small
//!   PING (a tagged nonce, [`PROBE_TAG`]) into the stream every
//!   [`PROBE_INTERVAL`] and remembers how many bytes had entered the path
//!   at that point. Every peer — old ones included — echoes PINGs in
//!   order, so a PONG proves that everything before its PING was delivered:
//!   it is a cumulative byte ack plus an RTT sample, with no protocol change.
//! - **Delivery rate** = acked bytes / elapsed time over a span of at least
//!   250 ms (and one RTT), median-filtered over [`RATE_WINDOW`] (samples
//!   taken while the sender was application-limited only count if they
//!   raise the estimate).
//! - **Min RTT** = minimum RTT sample over [`MIN_RTT_WINDOW`]; RTT above it
//!   is queueing delay.
//! - **Window**: frame data may be sent only while the estimated bytes in
//!   flight stay below `rate x (min_rtt + TARGET_QUEUE_DELAY)` — the path's
//!   BDP plus a small queue. Between acks the in-flight estimate assumes
//!   the path drains at `rate x GAIN` (1.25), which keeps probing for more
//!   bandwidth: on a faster path the next acks raise the rate estimate, so
//!   throughput is never capped on high-BDP paths; on a saturated path the
//!   standing queue stays near `TARGET_QUEUE_DELAY`.
//!
//! Control records (PONG, input echo, cursor, audio) are never gated; only
//! frame data waits. Before the first rate estimate nothing is gated (the
//! sender behaves as if pacing were off).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// High-bit tag on probe PING nonces (`"OVP"` + 40-bit counter), so the
/// host's keepalive (small sequential nonces) and the pacer never confuse
/// each other's echoes.
pub const PROBE_TAG: u64 = 0x4F56_5000_0000_0000;
const PROBE_MASK: u64 = 0xFFFF_FF00_0000_0000;

/// Probe PING spacing while frame data is flowing (~25 B per probe).
pub const PROBE_INTERVAL: Duration = Duration::from_millis(20);
/// Queueing delay we allow on top of the path's minimum RTT.
pub const TARGET_QUEUE_DELAY: Duration = Duration::from_millis(15);
/// Delivery-rate max-filter window.
pub const RATE_WINDOW: Duration = Duration::from_secs(1);
/// Min-RTT filter window.
pub const MIN_RTT_WINDOW: Duration = Duration::from_secs(10);
/// Bandwidth probing gain applied between acks.
pub const GAIN: f64 = 1.25;
/// Start-up gain (BBR's STARTUP, simplified): until the rate estimate stops
/// growing by 25%, the path is assumed to drain twice as fast as measured
/// and the max (not the median) of the samples is used, so a high-BDP path
/// ramps up in a few RTTs.
pub const STARTUP_GAIN: f64 = 2.0;
/// A rate estimate without a path-limited sample this recent is stale.
pub const STALE_AFTER: Duration = Duration::from_secs(2);
/// Smallest window (bytes): a few records always fit.
pub const MIN_WINDOW: u64 = 16 * 1024;
/// Shortest span a delivery-rate sample is measured over: acks arrive in
/// bursts (the peer echoes several probes after a busy decode), so short
/// spans overestimate the rate.
const MIN_SAMPLE_SPAN: Duration = Duration::from_millis(250);
/// Window before the first rate estimate: bounds the start-up queue.
pub const INITIAL_WINDOW: u64 = 128 * 1024;
/// How recent an RTT sample must be for [`LinkPacer::queue_delay`].
pub const QUEUE_SIGNAL_FRESH: Duration = Duration::from_secs(1);
/// Outstanding probes remembered (older ones are presumed lost).
const MAX_OUTSTANDING: usize = 256;

/// Whether a PING/PONG nonce belongs to the pacer.
pub fn is_probe(nonce: u64) -> bool {
    nonce & PROBE_MASK == PROBE_TAG
}

#[derive(Debug, Clone, Copy)]
struct Probe {
    nonce: u64,
    sent: Instant,
    /// Path bytes (sent minus carrier-queued) right after the PING.
    bytes: u64,
}

#[derive(Debug, Clone, Copy)]
struct Ack {
    at: Instant,
    bytes: u64,
    /// The sender was held back (by us or the carrier) since the previous
    /// ack: the sample measures the path, not the application.
    limited: bool,
}

/// Snapshot of the pacer's model (logs, tests, the bench).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PacerStats {
    /// Estimated delivery rate, bytes/s.
    pub rate: Option<f64>,
    /// Minimum RTT over [`MIN_RTT_WINDOW`] (the path's base delay).
    pub min_rtt: Option<Duration>,
    /// Last RTT sample.
    pub rtt: Option<Duration>,
    /// Current window, bytes (None = not gating yet).
    pub window: Option<u64>,
    /// Times frame data was held back by the window.
    pub holds: u64,
    /// Probe PINGs sent.
    pub probes_sent: u64,
    /// Probe PINGs whose PONG came back.
    pub probes_acked: u64,
}

/// The pacer state machine (pure: the caller passes `now` and byte counts).
#[derive(Debug, Clone)]
pub struct LinkPacer {
    next_probe: Option<Instant>,
    counter: u64,
    outstanding: VecDeque<Probe>,
    rtts: VecDeque<(Instant, Duration)>,
    acks: VecDeque<Ack>,
    rate_samples: VecDeque<(Instant, f64)>,
    rate: Option<f64>,
    last_rtt: Option<Duration>,
    /// Path bytes when probing started (the ack baseline before any PONG).
    start_bytes: Option<u64>,
    /// Start-up phase: (best rate so far, when it last grew by 25%).
    startup: Option<(f64, Instant)>,
    /// When a path-limited (not application-limited) rate sample was last
    /// taken: only then does the rate describe the path.
    path_limited_at: Option<Instant>,
    /// Time of the latest ack (the pacer's "now" for freshness checks).
    last_ack_at: Option<Instant>,
    limited: bool,
    holds: u64,
    probes_sent: u64,
    probes_acked: u64,
    enabled: bool,
}

impl Default for LinkPacer {
    fn default() -> Self {
        Self::new()
    }
}

impl LinkPacer {
    /// A pacer with no path model yet (nothing is gated until the first echo).
    pub fn new() -> Self {
        Self {
            next_probe: None,
            counter: 0,
            outstanding: VecDeque::new(),
            rtts: VecDeque::new(),
            acks: VecDeque::new(),
            rate_samples: VecDeque::new(),
            rate: None,
            last_rtt: None,
            start_bytes: None,
            startup: Some((0.0, Instant::now())),
            path_limited_at: None,
            last_ack_at: None,
            limited: false,
            holds: 0,
            probes_sent: 0,
            probes_acked: 0,
            enabled: true,
        }
    }

    /// Turn gating and probing on or off (off: frame data is never held back).
    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
    }

    /// Whether gating and probing are on.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Whether a probe PING should go out now (`active` = frame data is
    /// flowing or waiting).
    pub fn probe_due(&self, now: Instant, active: bool) -> bool {
        self.enabled && active && self.next_probe.is_none_or(|t| now >= t)
    }

    /// The nonce for the next probe PING (call [`probe_sent`] right after
    /// sending it).
    ///
    /// [`probe_sent`]: Self::probe_sent
    pub fn next_nonce(&mut self) -> u64 {
        self.counter = (self.counter + 1) & !PROBE_MASK;
        PROBE_TAG | self.counter
    }

    /// Record a probe PING just handed to the carrier; `stream_bytes` = the
    /// carrier's cumulative accepted bytes after the PING (its position in
    /// the ordered stream — an echo acks everything up to it).
    pub fn probe_sent(&mut self, nonce: u64, now: Instant, stream_bytes: u64) {
        let path_bytes = stream_bytes;
        self.next_probe = Some(now + PROBE_INTERVAL);
        self.start_bytes.get_or_insert(path_bytes);
        self.outstanding.push_back(Probe {
            nonce,
            sent: now,
            bytes: path_bytes,
        });
        while self.outstanding.len() > MAX_OUTSTANDING {
            self.outstanding.pop_front();
        }
        self.probes_sent += 1;
    }

    /// Note that the sender wanted to send frame data but the carrier (or
    /// the window) held it back.
    pub fn note_limited(&mut self) {
        self.limited = true;
    }

    /// Feed a PONG. Returns `true` when the nonce was one of our probes
    /// (consumed); `false` = not ours, hand it to the keepalive.
    pub fn on_pong(&mut self, nonce: u64, now: Instant) -> bool {
        if !is_probe(nonce) {
            return false;
        }
        let Some(pos) = self.outstanding.iter().position(|p| p.nonce == nonce) else {
            return true; // stale / duplicate echo of ours
        };
        // In-order path: every older probe is implicitly acked (or lost).
        let probe = self.outstanding[pos];
        self.outstanding.drain(..=pos);
        self.probes_acked += 1;
        let rtt = now.saturating_duration_since(probe.sent);
        self.last_rtt = Some(rtt);
        self.rtts.push_back((now, rtt));
        while self
            .rtts
            .front()
            .is_some_and(|(t, _)| now.saturating_duration_since(*t) > MIN_RTT_WINDOW)
        {
            self.rtts.pop_front();
        }
        self.last_ack_at = Some(now);
        let ack = Ack {
            at: now,
            bytes: probe.bytes,
            limited: std::mem::take(&mut self.limited),
        };
        self.sample_rate(ack);
        self.acks.push_back(ack);
        while self.acks.len() > 2
            && self
                .acks
                .front()
                .is_some_and(|a| now.saturating_duration_since(a.at) > RATE_WINDOW * 2)
        {
            self.acks.pop_front();
        }
        true
    }

    fn sample_rate(&mut self, ack: Ack) {
        let span = self.min_rtt().unwrap_or_default().max(MIN_SAMPLE_SPAN);
        // The newest earlier ack at least `span` before this one.
        let Some(idx) = self
            .acks
            .iter()
            .rposition(|a| ack.at.saturating_duration_since(a.at) >= span)
        else {
            return;
        };
        let base = self.acks[idx];
        let dt = ack.at.saturating_duration_since(base.at).as_secs_f64();
        if dt <= 0.0 || ack.bytes < base.bytes {
            return;
        }
        let sample = (ack.bytes - base.bytes) as f64 / dt;
        let limited = ack.limited || self.acks.iter().skip(idx + 1).any(|a| a.limited);
        if limited {
            self.path_limited_at = Some(ack.at);
        }
        // An application-limited sample only says "at least this much".
        if !limited && self.rate.is_some_and(|r| sample < r) {
            return;
        }
        self.rate_samples.push_back((ack.at, sample));
        while self
            .rate_samples
            .front()
            .is_some_and(|(t, _)| ack.at.saturating_duration_since(*t) > RATE_WINDOW)
        {
            self.rate_samples.pop_front();
        }
        // Median, not max: ack timing is noisy (the peer echoes in bursts
        // after a long decode), and a max filter locks onto the outliers.
        // Probing for more bandwidth is the job of GAIN, not the filter —
        // except during start-up, which takes the max to ramp quickly.
        let mut v: Vec<f64> = self.rate_samples.iter().map(|&(_, r)| r).collect();
        v.sort_by(|a, b| a.total_cmp(b));
        let max = v.last().copied().unwrap_or(sample);
        if let Some((best, at)) = self.startup {
            if max >= best * 1.25 {
                self.startup = Some((max, ack.at));
            } else {
                let rtt = self.min_rtt().unwrap_or_default();
                if ack.at.saturating_duration_since(at) > (rtt * 3).max(Duration::from_millis(300))
                {
                    self.startup = None; // plateau: steady state
                }
            }
        }
        self.rate = if self.startup.is_some() {
            Some(max)
        } else {
            v.get(v.len() / 2).copied()
        };
    }

    fn gain(&self) -> f64 {
        if self.startup.is_some() {
            STARTUP_GAIN
        } else {
            GAIN
        }
    }

    /// Whether the pacer is still in its start-up (fast ramp) phase.
    pub fn in_startup(&self) -> bool {
        self.startup.is_some()
    }

    /// Minimum RTT over the filter window.
    pub fn min_rtt(&self) -> Option<Duration> {
        self.rtts.iter().map(|&(_, r)| r).min()
    }

    /// The current window in bytes: `rate x (min RTT + target delay)`, or
    /// [`INITIAL_WINDOW`] once the peer echoed a probe but no rate is known
    /// yet; `None` before the first echo (no gating at all — a peer that
    /// has not pumped yet must not stall the sender).
    ///
    /// A rate measured only while the sender was application-limited (a
    /// static screen) says nothing about the path, so once no path-limited
    /// sample was seen for [`STALE_AFTER`] the window opens (no gating)
    /// until the next burst fills the path and yields fresh samples —
    /// otherwise a burst after an idle minute would be throttled to the
    /// idle trickle.
    pub fn window(&self) -> Option<u64> {
        if self.rate.is_some() {
            let fresh = match (self.path_limited_at, self.last_ack_at) {
                (Some(p), Some(a)) => a.saturating_duration_since(p) <= STALE_AFTER,
                _ => false,
            };
            if !fresh {
                return None;
            }
        }
        match (self.rate, self.min_rtt()) {
            (Some(rate), Some(min_rtt)) => {
                let w = rate * (min_rtt + TARGET_QUEUE_DELAY).as_secs_f64();
                Some((w as u64).max(MIN_WINDOW))
            }
            _ => (!self.acks.is_empty()).then_some(INITIAL_WINDOW),
        }
    }

    /// Estimated bytes still in the path at `now` (`path_bytes` = cumulative
    /// bytes handed to the OS so far): everything past the last cumulative
    /// ack, minus what the path drained since (at `rate x GAIN`).
    pub fn in_flight(&self, now: Instant, path_bytes: u64) -> u64 {
        let delivered = match (self.rate, self.acks.back()) {
            (Some(rate), Some(ack)) => {
                ack.bytes as f64
                    + rate * self.gain() * now.saturating_duration_since(ack.at).as_secs_f64()
            }
            (None, Some(ack)) => ack.bytes as f64,
            (_, None) => match self.start_bytes {
                Some(b) => b as f64,
                None => return 0,
            },
        };
        path_bytes.saturating_sub(delivered.min(path_bytes as f64) as u64)
    }

    /// May the next frame record go out? (`path_bytes` as for
    /// [`in_flight`](Self::in_flight).) A refusal counts as a hold and
    /// marks the sender limited.
    pub fn allow(&mut self, now: Instant, path_bytes: u64) -> bool {
        if !self.enabled {
            return true;
        }
        let Some(window) = self.window() else {
            return true;
        };
        if self.in_flight(now, path_bytes) < window {
            return true;
        }
        self.holds += 1;
        self.limited = true;
        false
    }

    /// How long until the window opens again (`None` = open / not gating):
    /// a sender loop waits at most this long before retrying.
    pub fn reopen_in(&self, now: Instant, path_bytes: u64) -> Option<Duration> {
        if !self.enabled {
            return None;
        }
        let window = self.window()?;
        let excess = self.in_flight(now, path_bytes).checked_sub(window)?;
        let Some(rate) = self.rate else {
            // No rate yet: the next ack (probe echo) reopens the window.
            return Some(PROBE_INTERVAL);
        };
        let secs = (excess as f64 + 1.0) / (rate * self.gain()).max(1.0);
        Some(Duration::from_secs_f64(secs).clamp(Duration::from_millis(1), PROBE_INTERVAL))
    }

    /// Excess queueing delay on the path: how far the round trip currently
    /// exceeds the path's minimum RTT — the congestion signal the
    /// auto-quality rule ([`crate::adapt::AutoQuality`]) reads. A relayed
    /// path's long but *stable* RTT is not congestion and reads ~0.
    ///
    /// Two inputs, the larger wins: the last echoed probe's RTT (if acked
    /// within [`QUEUE_SIGNAL_FRESH`]), and the age of the oldest probe still
    /// unanswered (a lower bound on its RTT — a stalling path shows up before
    /// its echo returns; probes older than 2 x [`QUEUE_SIGNAL_FRESH`] are
    /// presumed lost and ignored). `None` without a min-RTT or a fresh
    /// sample (e.g. a static screen: nothing is probed).
    pub fn queue_delay(&self, now: Instant) -> Option<Duration> {
        let min = self.min_rtt()?;
        let mut out: Option<Duration> = None;
        if let (Some(rtt), Some(at)) = (self.last_rtt, self.last_ack_at) {
            if now.saturating_duration_since(at) <= QUEUE_SIGNAL_FRESH {
                out = Some(rtt.saturating_sub(min));
            }
        }
        if let Some(p) = self
            .outstanding
            .iter()
            .find(|p| now.saturating_duration_since(p.sent) <= QUEUE_SIGNAL_FRESH * 2)
        {
            let pending = now.saturating_duration_since(p.sent).saturating_sub(min);
            out = Some(out.map_or(pending, |d| d.max(pending)));
        }
        out
    }

    /// When the next probe is due (for loop timeouts while frames flow).
    pub fn next_probe_at(&self) -> Option<Instant> {
        self.next_probe
    }

    /// Snapshot of the current model for logs and tests.
    pub fn stats(&self) -> PacerStats {
        PacerStats {
            rate: self.rate,
            min_rtt: self.min_rtt(),
            rtt: self.last_rtt,
            window: self.window(),
            holds: self.holds,
            probes_sent: self.probes_sent,
            probes_acked: self.probes_acked,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bottleneck link simulation: `rate` bytes/s, `base` one-way delay
    /// each way, unbounded FIFO queue. The sender writes `chunk`-byte frame
    /// records whenever the pacer allows (always backlogged) and probes per
    /// the pacer; returns (avg throughput B/s, p95 probe RTT) over the run.
    fn simulate(rate: f64, base: Duration, paced: bool, secs: u64) -> (f64, Duration, PacerStats) {
        const OS_BUFFER: u64 = 256 * 1024;
        let t0 = Instant::now();
        let step = Duration::from_micros(250);
        let chunk = 4096u64;
        // Probing always runs (to measure RTT); only `paced` gates sends.
        let mut p = LinkPacer::new();
        let mut sent: u64 = 0; // bytes into the path
        let mut delivered: u64 = 0;
        let mut link_free_at = t0; // when the bottleneck drains its queue
                                   // (arrival at the receiver, bytes, Some(nonce) for a probe).
        let mut path: VecDeque<(Instant, u64, Option<u64>)> = VecDeque::new();
        let mut pongs: VecDeque<(Instant, u64)> = VecDeque::new();
        let mut sent_at = std::collections::HashMap::new();
        let mut rtts = Vec::new();
        let end = t0 + Duration::from_secs(secs);
        let mut now = t0;
        let enqueue = |bytes: u64, now: Instant, free: &mut Instant| {
            let start = (*free).max(now);
            *free = start + Duration::from_secs_f64(bytes as f64 / rate);
            *free + base
        };
        while now < end {
            while path.front().is_some_and(|(t, _, _)| *t <= now) {
                let (t, b, probe) = path.pop_front().unwrap();
                delivered += b;
                if let Some(n) = probe {
                    pongs.push_back((t + base, n));
                }
            }
            while pongs.front().is_some_and(|(t, _)| *t <= now) {
                let (_, n) = pongs.pop_front().unwrap();
                rtts.push(now - sent_at[&n]);
                assert!(p.on_pong(n, now));
            }
            if p.probe_due(now, true) {
                let n = p.next_nonce();
                sent += 25;
                let at = enqueue(25, now, &mut link_free_at);
                path.push_back((at, 25, Some(n)));
                sent_at.insert(n, now);
                p.probe_sent(n, now, sent);
            }
            loop {
                if sent - delivered + chunk > OS_BUFFER {
                    p.note_limited();
                    break;
                }
                if paced && !p.allow(now, sent) {
                    break;
                }
                sent += chunk;
                let at = enqueue(chunk, now, &mut link_free_at);
                path.push_back((at, chunk, None));
            }
            now += step;
        }
        // Skip the first second (start-up burst before the first estimate).
        let skip = rtts.len() / secs as usize;
        let mut steady: Vec<Duration> = rtts[skip..].to_vec();
        steady.sort();
        let p95 = steady
            .get(steady.len() * 95 / 100)
            .copied()
            .unwrap_or_default();
        (delivered as f64 / secs as f64, p95, p.stats())
    }

    #[test]
    fn queue_delay_is_rtt_over_min_and_sees_stalls() {
        let mut p = LinkPacer::new();
        let t0 = Instant::now();
        assert_eq!(p.queue_delay(t0), None, "no samples");
        // A long but stable relay path: 120 ms every time -> no queueing.
        let mut now = t0;
        for i in 0..5u64 {
            let n = p.next_nonce();
            p.probe_sent(n, now, i * 1000);
            now += Duration::from_millis(120);
            assert!(p.on_pong(n, now));
        }
        assert_eq!(p.queue_delay(now), Some(Duration::ZERO));
        // Queue builds: the next echo takes 300 ms -> 180 ms excess.
        let n = p.next_nonce();
        p.probe_sent(n, now, 10_000);
        now += Duration::from_millis(300);
        assert!(p.on_pong(n, now));
        assert_eq!(p.queue_delay(now), Some(Duration::from_millis(180)));
        // A probe stuck for 700 ms with no echo yet: at least 580 ms.
        let n = p.next_nonce();
        p.probe_sent(n, now, 20_000);
        assert_eq!(
            p.queue_delay(now + Duration::from_millis(700)),
            Some(Duration::from_millis(580))
        );
        // Long silence: the stale sample and the presumed-lost probe expire.
        assert_eq!(p.queue_delay(now + Duration::from_secs(5)), None);
    }

    #[test]
    fn probe_nonces_are_tagged_and_never_collide_with_keepalive() {
        let mut p = LinkPacer::new();
        let n = p.next_nonce();
        assert!(is_probe(n));
        assert!(!is_probe(1) && !is_probe(12345));
        assert!(
            !p.on_pong(7, Instant::now()),
            "keepalive echoes are not ours"
        );
        assert!(p.on_pong(n, Instant::now()), "our (stale) echo is consumed");
    }

    #[test]
    fn no_gating_before_probing_then_an_initial_window() {
        let mut p = LinkPacer::new();
        let now = Instant::now();
        assert!(p.allow(now, 10_000_000));
        assert_eq!(p.window(), None);
        assert_eq!(p.reopen_in(now, 10_000_000), None);
        // Probing starts at 10 MB on the path; nothing is gated until the
        // peer echoes, then 128 KB past the acked point until a rate exists.
        let n = p.next_nonce();
        p.probe_sent(n, now, 10_000_000);
        assert!(
            p.allow(now, 10_000_000 + 10 * INITIAL_WINDOW),
            "no echo yet: no gating"
        );
        assert!(p.on_pong(n, now + Duration::from_millis(1)));
        assert!(p.allow(now, 10_000_000 + INITIAL_WINDOW - 1));
        assert!(!p.allow(now, 10_000_000 + INITIAL_WINDOW));
        // A probe behind those 128 KB is echoed: everything up to it was
        // delivered, so the window reopens.
        let n2 = p.next_nonce();
        p.probe_sent(n2, now, 10_000_000 + INITIAL_WINDOW);
        assert!(p.on_pong(n2, now + Duration::from_millis(5)));
        assert!(p.allow(now, 10_000_000 + INITIAL_WINDOW + 1));
    }

    #[test]
    fn pacing_bounds_queue_delay_on_a_10_mbps_link() {
        let rate = 10e6 / 8.0;
        let base = Duration::from_millis(2);
        let (tput_u, p95_u, _) = simulate(rate, base, false, 4);
        let (tput_p, p95_p, st) = simulate(rate, base, true, 4);
        eprintln!(
            "10 Mbps: unpaced p95 RTT {p95_u:?} at {:.2} Mbps; paced p95 RTT {p95_p:?} at {:.2} Mbps; {st:?}",
            tput_u * 8e-6,
            tput_p * 8e-6
        );
        // Unpaced, the 256 KB buffer adds ~200 ms of queueing.
        assert!(p95_u > Duration::from_millis(150), "{p95_u:?}");
        // Paced: queueing stays near the target, throughput near the link.
        assert!(p95_p < Duration::from_millis(60), "{p95_p:?}");
        assert!(tput_p > rate * 0.85, "throughput {:.2} Mbps", tput_p * 8e-6);
    }

    #[test]
    fn an_app_limited_rate_never_throttles_the_next_burst() {
        // A static screen: tiny probes acked every 20 ms, nothing else sent.
        let mut p = LinkPacer::new();
        let t0 = Instant::now();
        let mut bytes = 0u64;
        let mut now = t0;
        for _ in 0..200 {
            let n = p.next_nonce();
            bytes += 2_000;
            p.probe_sent(n, now, bytes);
            now += Duration::from_millis(20);
            assert!(p.on_pong(n, now));
        }
        assert!(
            p.stats().rate.unwrap() < 200_000.0,
            "the idle trickle was measured"
        );
        // Then a 1 MB full frame: no window may hold it back on stale data.
        assert_eq!(p.window(), None);
        assert!(p.allow(now, bytes + 1_000_000));
    }

    #[test]
    fn pacing_does_not_cap_a_high_bdp_path() {
        // 100 Mbps x 80 ms RTT: BDP = 1 MB, far above the minimum window.
        let rate = 100e6 / 8.0;
        let base = Duration::from_millis(40);
        let (tput, p95, st) = simulate(rate, base, true, 6);
        eprintln!(
            "100 Mbps/80 ms: {:.1} Mbps, p95 RTT {p95:?}, {st:?}",
            tput * 8e-6
        );
        // The 256 KB OS buffer is the only cap here (256 KB / 80 ms = 26 Mbps
        // unpaced too) — the pacer must not lower it further.
        let (tput_u, _, _) = simulate(rate, base, false, 6);
        assert!(
            tput >= tput_u * 0.9,
            "paced {:.1} vs unpaced {:.1} Mbps",
            tput * 8e-6,
            tput_u * 8e-6
        );
    }
}
