//! Wave 3 (B2): per-packet congestion control and pacing for the direct UDP
//! path — sans-IO, shared by the desktop [`crate::DirectCarrier`] and the
//! iOS FFI engine.
//!
//! Every datagram the [`crate::direct`] engine puts on a v2 link carries a
//! transport-wide sequence number (`tseq`) and its send timestamp
//! ([`crate::media`] wire format). The sender remembers `(tseq, send time,
//! size)` in a [`SentHistory`]; the receiver ([`FeedbackRx`]) records the
//! arrival time of every `tseq` and returns TWCC-style feedback every
//! [`FEEDBACK_INTERVAL`]. [`Gcc`] turns the feedback into a target rate:
//!
//! - **delay-based** (a port of the GCC trendline estimator): packets are
//!   grouped by send time (5 ms bursts); the inter-group delay variation
//!   `d = Δarrival - Δsend` is accumulated, smoothed, and a least-squares
//!   slope over the last [`TREND_WINDOW`] groups is compared against an
//!   adaptive threshold → overuse / normal / underuse;
//! - **AIMD**: overuse cuts the target to 0.85 x the measured receive rate
//!   (at most once per RTT); normal grows it (fast startup, then
//!   multiplicative, additive near the last known capacity); the target
//!   never runs far ahead of what the receiver actually gets (app-limited
//!   senders keep a sane estimate);
//! - **loss**: only a sustained loss rate above [`LOSS_HIGH`] lowers the rate
//!   (`x (1 - 0.5 loss)`). Random loss below that is the FEC's problem, not
//!   a congestion signal — throughput does not collapse on a lossy Wi-Fi.
//!
//! [`Pacer`] spreads the sender's datagrams at `PACING_GAIN x target` with a
//! small burst budget, so a video frame leaves over about half a frame
//! interval instead of as a line-rate burst that overflows a shallow
//! bottleneck queue.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

/// How often the receiver returns feedback.
pub const FEEDBACK_INTERVAL: Duration = Duration::from_millis(25);
/// Feedback arrival-delta resolution (microseconds per unit).
pub const FEEDBACK_UNIT_US: u64 = 50;
/// Most packets one feedback message reports.
pub const FEEDBACK_MAX: usize = 500;
/// Pacing rate = this x the target rate. The encoder spends ~70 % of the
/// target (plus FEC), so a frame leaves within ~half its interval; a higher
/// gain only adds self-inflicted queueing at the bottleneck once the target
/// approaches the capacity.
pub const PACING_GAIN: f64 = 1.5;
/// Window of the standing-queue filter (see [Gcc]): about three frames.
pub const STANDING_WINDOW_US: i64 = 100_000;
/// A trendline overuse only counts while the queue has not drained for
/// [STANDING_WINDOW_US] — at least this much standing delay. Paced video
/// bursts that exceed the capacity for a few ms inside a frame (and drain
/// before the next one) are not congestion.
pub const STANDING_MIN_US: f64 = 4_000.0;
/// Start rate before any feedback (bits/s).
pub const START_RATE: f64 = 6_000_000.0;
/// Rate floor. Live Wi-Fi sessions showed what a low floor costs: an idle
/// sender's estimate ratcheted down to 0.3 Mbps on jitter, and the next
/// full-screen update (with the PONGs and input acks queued behind it on the
/// reliable lane) took seconds. 2 Mbps keeps any update within ~1 s even
/// in the worst case; a genuinely slower path still sees the delay signal.
pub const MIN_RATE: f64 = 2_000_000.0;
/// An app-limited sender (receiving well below its target) only treats an
/// overuse as congestion once the standing queue exceeds this: Wi-Fi
/// aggregation / power-save spikes of 20-40 ms on a few packets are not.
pub const APP_LIMITED_STANDING_US: f64 = 25_000.0;
/// ... and only if that queue never drained for this long.
pub const APP_LIMITED_WINDOW_US: i64 = 400_000;
/// How long a learned capacity is trusted for the fast restore after idle.
pub const CAPACITY_MEMORY: Duration = Duration::from_secs(60);
pub const MAX_RATE: f64 = 200_000_000.0;
/// Loss rate above which the controller backs off (random loss below it is
/// left to FEC / retransmission).
pub const LOSS_HIGH: f64 = 0.10;
/// Window the loss rate is measured over.
pub const LOSS_WINDOW: Duration = Duration::from_secs(1);
/// Groups the trendline regression spans.
pub const TREND_WINDOW: usize = 20;
const TREND_SMOOTHING: f64 = 0.9;
const TREND_GAIN: f64 = 4.0;
const THRESH_INIT: f64 = 12.5;
const K_UP: f64 = 0.0087;
const K_DOWN: f64 = 0.039;
const OVERUSE_TIME_MS: f64 = 10.0;
const BURST_US: u64 = 5_000;
const HISTORY: usize = 8192;

/// Sender-side record of sent datagrams, indexed by `tseq`.
pub struct SentHistory {
    ring: Vec<(u32, u64, u32, bool)>,
}

impl Default for SentHistory {
    fn default() -> Self {
        Self::new()
    }
}

impl SentHistory {
    pub fn new() -> Self {
        Self {
            ring: vec![(0, 0, 0, false); HISTORY],
        }
    }

    /// Datagram `tseq` of `size` bytes left at `send_us` (sender clock).
    pub fn record(&mut self, tseq: u32, send_us: u64, size: usize) {
        self.ring[tseq as usize % HISTORY] = (tseq, send_us, size as u32, true);
    }

    /// `(send_us, size)` of `tseq`, if still remembered.
    pub fn get(&self, tseq: u32) -> Option<(u64, u32)> {
        let e = self.ring[tseq as usize % HISTORY];
        (e.3 && e.0 == tseq).then_some((e.1, e.2))
    }
}

/// One datagram as reported by feedback, joined with the sender's record.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PacketReport {
    pub tseq: u32,
    /// Sender clock, microseconds.
    pub send_us: u64,
    pub size: u32,
    /// Receiver clock (unwrapped), microseconds; `None` = not received.
    pub arrival_us: Option<i64>,
}

// ---------------------------------------------------------------------------
// Receiver: arrival recording + feedback messages
// ---------------------------------------------------------------------------

/// Receiver side: arrival times by `tseq`, turned into feedback messages.
#[derive(Debug, Default)]
pub struct FeedbackRx {
    /// First `tseq` not yet reported.
    base: Option<u32>,
    /// Offset from `base` -> arrival (receiver clock, us).
    arrivals: BTreeMap<u32, u64>,
    last_sent: Option<Instant>,
}

impl FeedbackRx {
    pub fn new() -> Self {
        Self::default()
    }

    /// One v2 datagram arrived.
    pub fn on_packet(&mut self, tseq: u32, arrival_us: u64) {
        let base = *self.base.get_or_insert(tseq);
        let off = tseq.wrapping_sub(base);
        if off >= 1 << 30 {
            return; // older than what was already reported
        }
        if off > 65_536 {
            // A huge jump (peer restart / long outage): restart reporting.
            self.arrivals.clear();
            self.base = Some(tseq);
            self.arrivals.insert(0, arrival_us);
            return;
        }
        self.arrivals.entry(off).or_insert(arrival_us);
    }

    /// Whether feedback is owed now.
    pub fn due(&self, now: Instant) -> bool {
        !self.arrivals.is_empty()
            && self
                .last_sent
                .is_none_or(|t| now.saturating_duration_since(t) >= FEEDBACK_INTERVAL)
    }

    /// When feedback is next owed (`None` = nothing to report).
    pub fn next_due(&self, now: Instant) -> Option<Duration> {
        if self.arrivals.is_empty() {
            return None;
        }
        Some(self.last_sent.map_or(Duration::ZERO, |t| {
            (t + FEEDBACK_INTERVAL).saturating_duration_since(now)
        }))
    }

    /// Build the feedback messages covering everything from `base` up to the
    /// highest `tseq` received (unreceived ones reported lost) and advance.
    /// Each message: `type(3) | base u32 | count u16 | ref_us u32 |
    /// count x u16 delta` (`(arrival - ref_us) / 50 us`, `0xFFFF` = lost).
    pub fn build(&mut self, now: Instant, msg_type: u8) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let Some(base) = self.base else { return out };
        let Some((&last, _)) = self.arrivals.last_key_value() else {
            return out;
        };
        self.last_sent = Some(now);
        let total = last as usize + 1;
        let mut start = 0usize;
        while start < total {
            let count = (total - start).min(FEEDBACK_MAX);
            let range = start as u32..(start + count) as u32;
            let refv = self
                .arrivals
                .range(range.clone())
                .map(|(_, a)| *a)
                .min()
                .unwrap_or(0);
            let mut m = Vec::with_capacity(11 + count * 2);
            m.push(msg_type);
            m.extend_from_slice(&base.wrapping_add(start as u32).to_be_bytes());
            m.extend_from_slice(&(count as u16).to_be_bytes());
            m.extend_from_slice(&(refv as u32).to_be_bytes());
            for off in range {
                let v = match self.arrivals.get(&off) {
                    Some(a) => ((a - refv) / FEEDBACK_UNIT_US).min(0xFFFE) as u16,
                    None => 0xFFFF,
                };
                m.extend_from_slice(&v.to_be_bytes());
            }
            out.push(m);
            start += count;
        }
        self.base = Some(base.wrapping_add(total as u32));
        self.arrivals.clear();
        out
    }
}

/// Parse one feedback message body (after the type byte): `(tseq, arrival
/// on the receiver's u32 microsecond clock)` per reported packet.
pub fn parse_feedback(p: &[u8]) -> Option<Vec<(u32, Option<u32>)>> {
    if p.len() < 10 {
        return None;
    }
    let base = u32::from_be_bytes(p[0..4].try_into().ok()?);
    let count = u16::from_be_bytes([p[4], p[5]]) as usize;
    let refv = u32::from_be_bytes(p[6..10].try_into().ok()?);
    if p.len() < 10 + count * 2 || count > FEEDBACK_MAX {
        return None;
    }
    Some(
        (0..count)
            .map(|i| {
                let v = u16::from_be_bytes([p[10 + 2 * i], p[11 + 2 * i]]);
                let t = base.wrapping_add(i as u32);
                (
                    t,
                    (v != 0xFFFF)
                        .then(|| refv.wrapping_add(u32::from(v) * FEEDBACK_UNIT_US as u32)),
                )
            })
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// Trendline (delay gradient) estimator
// ---------------------------------------------------------------------------

/// Bandwidth usage hypothesis of the delay detector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Usage {
    Normal,
    Overuse,
    Underuse,
}

#[derive(Debug)]
struct Trendline {
    first_arrival_ms: Option<f64>,
    accumulated: f64,
    smoothed: f64,
    history: VecDeque<(f64, f64)>,
    num_deltas: u32,
    trend: f64,
    prev_trend: f64,
    threshold: f64,
    last_thresh_ms: Option<f64>,
    time_over_using: f64,
    overuse_count: u32,
    state: Usage,
}

impl Trendline {
    fn new() -> Self {
        Self {
            first_arrival_ms: None,
            accumulated: 0.0,
            smoothed: 0.0,
            history: VecDeque::new(),
            num_deltas: 0,
            trend: 0.0,
            prev_trend: 0.0,
            threshold: THRESH_INIT,
            last_thresh_ms: None,
            time_over_using: -1.0,
            overuse_count: 0,
            state: Usage::Normal,
        }
    }

    fn update(&mut self, recv_delta_ms: f64, send_delta_ms: f64, arrival_ms: f64) {
        let delta = recv_delta_ms - send_delta_ms;
        self.num_deltas = (self.num_deltas + 1).min(1000);
        let first = *self.first_arrival_ms.get_or_insert(arrival_ms);
        self.accumulated += delta;
        self.smoothed =
            TREND_SMOOTHING * self.smoothed + (1.0 - TREND_SMOOTHING) * self.accumulated;
        self.history.push_back((arrival_ms - first, self.smoothed));
        if self.history.len() > TREND_WINDOW {
            self.history.pop_front();
        }
        if self.history.len() == TREND_WINDOW {
            if let Some(s) = slope(&self.history) {
                self.trend = s;
            }
        }
        self.detect(send_delta_ms, arrival_ms);
    }

    fn detect(&mut self, ts_delta_ms: f64, now_ms: f64) {
        if self.num_deltas < 2 {
            self.state = Usage::Normal;
            return;
        }
        let modified = f64::from(self.num_deltas.min(60)) * self.trend * TREND_GAIN;
        if modified > self.threshold {
            if self.time_over_using < 0.0 {
                self.time_over_using = ts_delta_ms / 2.0;
            } else {
                self.time_over_using += ts_delta_ms;
            }
            self.overuse_count += 1;
            if self.time_over_using > OVERUSE_TIME_MS
                && self.overuse_count > 1
                && self.trend >= self.prev_trend
            {
                self.time_over_using = 0.0;
                self.overuse_count = 0;
                self.state = Usage::Overuse;
            }
        } else if modified < -self.threshold {
            self.time_over_using = -1.0;
            self.overuse_count = 0;
            self.state = Usage::Underuse;
        } else {
            self.time_over_using = -1.0;
            self.overuse_count = 0;
            self.state = Usage::Normal;
        }
        self.prev_trend = self.trend;
        // Adaptive threshold (GCC): follows |modified| slowly upward, fast
        // downward, ignoring outliers.
        let last = *self.last_thresh_ms.get_or_insert(now_ms);
        if modified.abs() > self.threshold + 15.0 {
            self.last_thresh_ms = Some(now_ms);
            return;
        }
        let k = if modified.abs() < self.threshold {
            K_DOWN
        } else {
            K_UP
        };
        let dt = (now_ms - last).clamp(0.0, 100.0);
        self.threshold =
            (self.threshold + k * (modified.abs() - self.threshold) * dt).clamp(6.0, 600.0);
        self.last_thresh_ms = Some(now_ms);
    }
}

/// Least-squares slope of `(x, y)` points.
fn slope(pts: &VecDeque<(f64, f64)>) -> Option<f64> {
    let n = pts.len() as f64;
    let (sx, sy) = pts.iter().fold((0.0, 0.0), |(a, b), (x, y)| (a + x, b + y));
    let (mx, my) = (sx / n, sy / n);
    let (mut num, mut den) = (0.0, 0.0);
    for (x, y) in pts {
        num += (x - mx) * (y - my);
        den += (x - mx) * (x - mx);
    }
    (den > 0.0).then(|| num / den)
}

#[derive(Debug, Clone, Copy)]
struct Group {
    first_send: u64,
    last_send: u64,
    last_arrival: i64,
}

// ---------------------------------------------------------------------------
// GCC-like controller
// ---------------------------------------------------------------------------

/// Counters of a [`Gcc`].
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct CcStats {
    pub feedbacks: u64,
    pub reported: u64,
    pub lost: u64,
    pub overuses: u64,
    pub loss_cuts: u64,
}

/// The sender's rate controller (see the module docs).
#[derive(Debug)]
pub struct Gcc {
    target: f64,
    startup: bool,
    trend: Trendline,
    group: Option<Group>,
    prev_group: Option<Group>,
    last_update: Option<Instant>,
    last_decrease: Option<Instant>,
    last_loss_cut: Option<Instant>,
    /// Receive-rate window: (arrival_us, bytes).
    recv: VecDeque<(i64, u32)>,
    acked: Option<f64>,
    acked_short: Option<f64>,
    loss_win: VecDeque<(Instant, u32, u32)>,
    /// Last capacity estimate (receive rate at the last overuse) and its
    /// variance, normalized (GCC's "link capacity").
    avg_max: Option<f64>,
    var_max: f64,
    loss: f64,
    /// One-way delay (arrival - send, arbitrary offset): windowed minimum
    /// and the smoothed excess over it.
    owd_min: [(i64, Instant); 2],
    queue_us: f64,
    /// (arrival, queueing delay) of the last [`STANDING_WINDOW_US`].
    standing: VecDeque<(i64, f64)>,
    /// A cut happened and the overuse has not cleared since.
    in_episode: bool,
    /// Windowed max of the short-window receive rate (bits/s, bucket start).
    burst_max: [(f64, Instant); 2],
    /// The receive rate seen under real (not app-limited) load, and when.
    capacity: Option<(f64, Instant)>,
    /// The last cut happened while app-limited (eligible for the fast
    /// restore).
    idle_cut: bool,
    last_arrival: Option<i64>,
    remote_clock: Option<(u32, i64)>,
    stats: CcStats,
}

impl Default for Gcc {
    fn default() -> Self {
        Self::new()
    }
}

impl Gcc {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            target: START_RATE,
            startup: true,
            trend: Trendline::new(),
            group: None,
            prev_group: None,
            last_update: None,
            last_decrease: None,
            last_loss_cut: None,
            recv: VecDeque::new(),
            acked: None,
            acked_short: None,
            loss_win: VecDeque::new(),
            avg_max: None,
            var_max: 0.4,
            loss: 0.0,
            owd_min: [(i64::MAX, now); 2],
            queue_us: 0.0,
            standing: VecDeque::new(),
            in_episode: false,
            burst_max: [(0.0, now); 2],
            capacity: None,
            idle_cut: false,
            last_arrival: None,
            remote_clock: None,
            stats: CcStats::default(),
        }
    }

    /// The target send rate, bits/s.
    pub fn target_bps(&self) -> f64 {
        self.target
    }

    /// Smoothed loss rate from feedback (0..1).
    pub fn loss(&self) -> f64 {
        self.loss
    }

    /// The measured receive rate (bits/s), once known.
    /// The pacing rate: [`PACING_GAIN`] x the target, but never above the
    /// bottleneck capacity seen in saturated bursts (pacing faster than the
    /// link only builds a queue there), and never below the target.
    pub fn pacing_bps(&self) -> f64 {
        let paced = self.target * PACING_GAIN;
        // The bottleneck rate seen under saturation: the 500 ms receive rate
        // under real load (robust to Wi-Fi clumps), or the burst rate.
        let burst = self.burst_max[0].0.max(self.burst_max[1].0);
        let cap = match self.capacity {
            Some((c, _)) if burst > 0.0 => c.min(burst),
            Some((c, _)) => c,
            None => burst,
        };
        // Only a burst rate below the paced rate shows saturation.
        if cap > 0.0 && cap < paced {
            paced.min((cap * 0.97).max(self.target * 1.05))
        } else {
            paced
        }
    }

    pub fn acked_bps(&self) -> Option<f64> {
        self.acked
    }

    /// Smoothed one-way queueing delay estimate (excess over the minimum).
    pub fn queue_delay(&self) -> Duration {
        Duration::from_micros(self.queue_us.max(0.0) as u64)
    }

    pub fn usage(&self) -> Usage {
        self.trend.state
    }

    pub fn in_startup(&self) -> bool {
        self.startup
    }

    pub fn stats(&self) -> CcStats {
        self.stats
    }

    /// Unwrap the receiver's u32 microsecond clock.
    pub fn unwrap_remote(&mut self, t: u32) -> i64 {
        match self.remote_clock {
            None => {
                self.remote_clock = Some((t, i64::from(t)));
                i64::from(t)
            }
            Some((last, unwrapped)) => {
                let d = i64::from(t.wrapping_sub(last) as i32);
                let v = unwrapped + d;
                if d > 0 {
                    self.remote_clock = Some((t, v));
                }
                v
            }
        }
    }

    /// Process one feedback message's packets (in `tseq` order).
    pub fn on_feedback(&mut self, now: Instant, reports: &[PacketReport], rtt: Option<Duration>) {
        if reports.is_empty() {
            return;
        }
        self.stats.feedbacks += 1;
        let total = reports.len();
        let lost = reports.iter().filter(|r| r.arrival_us.is_none()).count();
        self.stats.reported += total as u64;
        self.stats.lost += lost as u64;
        // Loss over the last second of reports (at least 50 packets).
        self.loss_win.push_back((now, total as u32, lost as u32));
        while self
            .loss_win
            .front()
            .is_some_and(|(t, _, _)| now.saturating_duration_since(*t) > LOSS_WINDOW)
        {
            self.loss_win.pop_front();
        }
        let (n, l) = self
            .loss_win
            .iter()
            .fold((0u32, 0u32), |(a, b), (_, t, x)| (a + t, b + x));
        if n >= 50 {
            self.loss = f64::from(l) / f64::from(n);
        }
        for r in reports {
            let Some(arr) = r.arrival_us else { continue };
            self.recv.push_back((arr, r.size));
            self.note_owd(now, arr, arr - r.send_us as i64);
            self.last_arrival = Some(self.last_arrival.map_or(arr, |a| a.max(arr)));
            match self.group.as_mut() {
                Some(g) if r.send_us.saturating_sub(g.first_send) <= BURST_US => {
                    g.last_send = g.last_send.max(r.send_us);
                    g.last_arrival = g.last_arrival.max(arr);
                }
                _ => {
                    if let (Some(g), Some(p)) = (self.group, self.prev_group) {
                        let send_delta = (g.last_send as f64 - p.last_send as f64) / 1000.0;
                        let recv_delta = (g.last_arrival - p.last_arrival) as f64 / 1000.0;
                        if send_delta >= 0.0 {
                            self.trend.update(
                                recv_delta,
                                send_delta,
                                g.last_arrival as f64 / 1000.0,
                            );
                        }
                    }
                    if self.group.is_some() {
                        self.prev_group = self.group;
                    }
                    self.group = Some(Group {
                        first_send: r.send_us,
                        last_send: r.send_us,
                        last_arrival: arr,
                    });
                }
            }
        }
        // Receive rate over the last ~500 ms of arrivals.
        if let Some(last) = self.last_arrival {
            while self.recv.front().is_some_and(|(t, _)| last - *t > 500_000) {
                self.recv.pop_front();
            }
            if let Some(&(first, _)) = self.recv.front() {
                let span = (last - first) as f64 / 1e6;
                if span >= 0.1 {
                    let bytes: u64 = self.recv.iter().map(|(_, b)| u64::from(*b)).sum();
                    self.acked = Some(bytes as f64 * 8.0 / span);
                }
            }
            // The short-window rate: what a burst actually got through (the
            // overuse cut must not collapse to an app-limited average).
            let recent: Vec<&(i64, u32)> = self
                .recv
                .iter()
                .filter(|(t, _)| last - *t <= 200_000)
                .collect();
            if let (Some(f), true) = (recent.first(), recent.len() >= 8) {
                let span = (last - f.0) as f64 / 1e6;
                if span >= 0.02 {
                    let bytes: u64 = recent.iter().map(|(_, b)| u64::from(*b)).sum();
                    let r = bytes as f64 * 8.0 / span;
                    self.acked_short = Some(r);
                    // Windowed max (two 5 s buckets) of what bursts got
                    // through: once a burst saturates the bottleneck this
                    // is its capacity.
                    if now.saturating_duration_since(self.burst_max[1].1) >= Duration::from_secs(5)
                    {
                        self.burst_max[0] = self.burst_max[1];
                        self.burst_max[1] = (0.0, now);
                    }
                    self.burst_max[1].0 = self.burst_max[1].0.max(r);
                }
            }
        }
        self.update_rate(now, rtt.unwrap_or(Duration::from_millis(100)));
    }

    /// The minimum queueing delay over the last [`STANDING_WINDOW_US`] of
    /// arrivals: a queue that never drained in that window.
    pub fn standing_queue(&self) -> Duration {
        Duration::from_micros(self.standing_over(STANDING_WINDOW_US) as u64)
    }

    /// Minimum queueing delay (us) over the last window_us of arrivals.
    fn standing_over(&self, window_us: i64) -> f64 {
        let Some(&(last, _)) = self.standing.back() else {
            return 0.0;
        };
        let m = self
            .standing
            .iter()
            .rev()
            .take_while(|(t, _)| last - *t <= window_us)
            .map(|(_, q)| *q)
            .fold(f64::INFINITY, f64::min);
        if m.is_finite() {
            m.max(0.0)
        } else {
            0.0
        }
    }

    fn note_owd(&mut self, now: Instant, arrival: i64, owd: i64) {
        // Two 5 s buckets: the minimum over the last 5-10 s.
        if now.saturating_duration_since(self.owd_min[1].1) >= Duration::from_secs(5) {
            self.owd_min[0] = self.owd_min[1];
            self.owd_min[1] = (owd, now);
        }
        if owd < self.owd_min[1].0 {
            self.owd_min[1].0 = owd;
        }
        let min = self.owd_min[0].0.min(self.owd_min[1].0);
        let q = (owd - min).max(0) as f64;
        self.queue_us = 0.9 * self.queue_us + 0.1 * q;
        self.standing.push_back((arrival, q));
        while self
            .standing
            .front()
            .is_some_and(|(t, _)| arrival - *t > APP_LIMITED_WINDOW_US)
        {
            self.standing.pop_front();
        }
    }

    fn update_rate(&mut self, now: Instant, rtt: Duration) {
        let dt = self.last_update.map_or(0.0, |t| {
            now.saturating_duration_since(t).as_secs_f64().min(1.0)
        });
        self.last_update = Some(now);
        // Loss: only a high sustained loss rate is congestion.
        if self.loss > LOSS_HIGH
            && self.last_loss_cut.is_none_or(|t| {
                now.saturating_duration_since(t) >= rtt.max(Duration::from_millis(300))
            })
        {
            self.target = (self.target * (1.0 - 0.5 * self.loss)).max(MIN_RATE);
            self.last_loss_cut = Some(now);
            self.startup = false;
            self.stats.loss_cuts += 1;
            return;
        }
        let base = match (self.acked, self.acked_short) {
            (Some(a), Some(s)) => a.max(s),
            (a, s) => a.or(s).unwrap_or(self.target),
        };
        // Receiving well below the target: the sender is idle or app-limited.
        let app_limited = base < 0.6 * self.target;
        // Real load: remember what the path carried (for the fast restore
        // after an idle period).
        if !app_limited {
            // The 500 ms receive rate: a short window over-reads the clump a
            // Wi-Fi delay spike releases at once.
            if let Some(s) = self.acked {
                if self.capacity.is_none_or(|(c, at)| {
                    s > c || now.saturating_duration_since(at) > CAPACITY_MEMORY / 2
                }) {
                    self.capacity = Some((s.min(MAX_RATE), now));
                }
            }
        }
        if self
            .capacity
            .is_some_and(|(_, at)| now.saturating_duration_since(at) > CAPACITY_MEMORY)
        {
            self.capacity = None;
        }
        // An overuse without a standing queue is intra-frame burstiness; an
        // idle sender needs a large one (Wi-Fi jitter is not congestion).
        // An idle sender must see the large queue persist across a longer
        // window too (one Wi-Fi spike lasts up to ~150 ms).
        let standing = if app_limited {
            self.standing_over(APP_LIMITED_WINDOW_US)
        } else {
            self.standing_over(STANDING_WINDOW_US)
        };
        let need = if app_limited {
            APP_LIMITED_STANDING_US
        } else {
            STANDING_MIN_US
        };
        let state = match self.trend.state {
            Usage::Overuse if standing < need => Usage::Normal,
            s => s,
        };
        match state {
            Usage::Overuse => {
                // One cut per congestion episode: the queue needs about an
                // RTT plus the standing window to drain after a cut; cut again
                // only if the overuse outlives that.
                let gap = if self.in_episode {
                    (rtt * 2 + Duration::from_micros(STANDING_WINDOW_US as u64))
                        .max(Duration::from_millis(300))
                } else {
                    Duration::ZERO
                };
                if self
                    .last_decrease
                    .is_none_or(|t| now.saturating_duration_since(t) >= gap)
                {
                    // An app-limited sender (receive rate well below the
                    // target) says little about capacity: cut at most 30 %
                    // per episode then, and keep it out of the capacity
                    // estimate. A real capacity drop is still followed
                    // within a few episodes.
                    let mut cut = 0.85 * base;
                    if app_limited {
                        cut = cut.max(0.7 * self.target);
                        self.idle_cut = true;
                    } else {
                        self.note_max(base);
                        self.idle_cut = false;
                    }
                    let cut = cut.max(MIN_RATE);
                    if cut < self.target {
                        self.target = cut;
                    }
                    self.last_decrease = Some(now);
                    self.in_episode = true;
                    self.startup = false;
                    self.stats.overuses += 1;
                }
            }
            Usage::Underuse => self.in_episode = false, // queues draining: hold
            Usage::Normal => {
                self.in_episode = false;
                if self
                    .last_decrease
                    .is_some_and(|t| now.saturating_duration_since(t) < rtt)
                {
                    return; // let the cut take effect first
                }
                // Far above the old capacity estimate: it is stale.
                if let (Some(m), Some(a)) = (self.avg_max, self.acked) {
                    let std = (self.var_max * m / 1e3).sqrt() * 1e3;
                    if a > m + 3.0 * std {
                        self.avg_max = None;
                    }
                }
                let near_max = self.avg_max.is_some_and(|m| {
                    let std = (self.var_max * m).sqrt();
                    self.target / 1e3 > m / 1e3 - 3.0 * std
                        && self.target / 1e3 < m / 1e3 + 3.0 * std
                });
                // Back from idle (or after a cut caused by a transient):
                // return to the capacity the path recently carried quickly.
                // Only after a cut taken while idle: a cut under real load is
                // real congestion and recovers at the normal pace.
                let restore =
                    self.idle_cut && self.capacity.is_some_and(|(c, _)| self.target < 0.85 * c);
                let grow = if self.startup {
                    self.target * dt * 1.0
                } else if restore {
                    self.target * dt * 2.0
                } else if near_max {
                    // Additive: ~ one packet per RTT, at least 5 %/s.
                    let per_rtt = 1200.0 * 8.0 / rtt.as_secs_f64().max(0.01);
                    (per_rtt * dt).max(self.target * 0.05 * dt)
                } else {
                    self.target * 0.25 * dt
                };
                let mut t = self.target + grow;
                // Never grow far ahead of what actually arrives (an
                // app-limited sender holds its estimate instead).
                if let Some(a) = self.acked {
                    let cap =
                        (1.5 * a + 1_000_000.0).max(self.capacity.map_or(0.0, |(c, _)| 0.9 * c));
                    if t > cap {
                        t = self.target.max(cap.min(t));
                    }
                }
                self.target = t.clamp(MIN_RATE, MAX_RATE);
            }
        }
    }

    /// GCC link-capacity tracking (kbps domain, normalized variance).
    fn note_max(&mut self, bps: f64) {
        let x = bps / 1e3;
        match self.avg_max {
            None => self.avg_max = Some(bps),
            Some(m) => {
                let mk = m / 1e3;
                let alpha = 0.05;
                let avg = (1.0 - alpha) * mk + alpha * x;
                let norm = avg.max(1.0);
                self.var_max = ((1.0 - alpha) * self.var_max
                    + alpha * (avg - x) * (avg - x) / norm)
                    .clamp(0.4, 2.5);
                self.avg_max = Some(avg * 1e3);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Pacer
// ---------------------------------------------------------------------------

/// Pacer lanes, highest priority first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// The reliable channel (control, input, tiles; retransmissions).
    Reliable = 0,
    /// Video shards.
    Media = 1,
}

/// A two-lane leaky-bucket pacer (see the module docs). `T` is the queued
/// item; its wire size is given at enqueue.
#[derive(Debug)]
pub struct Pacer<T> {
    rate_bps: f64,
    budget: f64,
    last: Option<Instant>,
    lanes: [VecDeque<(usize, T)>; 2],
    bytes: usize,
}

impl<T> Default for Pacer<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Pacer<T> {
    pub fn new() -> Self {
        Self {
            rate_bps: START_RATE * PACING_GAIN,
            budget: 0.0,
            last: None,
            lanes: [VecDeque::new(), VecDeque::new()],
            bytes: 0,
        }
    }

    /// Set the pacing rate (bits/s).
    pub fn set_rate(&mut self, bps: f64) {
        self.rate_bps = bps.max(MIN_RATE);
    }

    pub fn rate(&self) -> f64 {
        self.rate_bps
    }

    /// Bytes queued.
    pub fn queued_bytes(&self) -> usize {
        self.bytes
    }

    pub fn queued(&self, lane: Lane) -> usize {
        self.lanes[lane as usize].len()
    }

    pub fn is_empty(&self) -> bool {
        self.lanes.iter().all(|l| l.is_empty())
    }

    fn burst(&self) -> f64 {
        // 4 ms worth, at least two full datagrams.
        (self.rate_bps / 8.0 * 0.004).max(2.0 * 1500.0)
    }

    fn refill(&mut self, now: Instant) {
        if let Some(l) = self.last {
            let dt = now.saturating_duration_since(l).as_secs_f64();
            self.budget = (self.budget + self.rate_bps / 8.0 * dt).min(self.burst());
        } else {
            self.budget = self.burst();
        }
        self.last = Some(now);
    }

    pub fn push(&mut self, lane: Lane, size: usize, item: T) {
        self.bytes += size;
        self.lanes[lane as usize].push_back((size, item));
    }

    /// Put an item back at the head of its lane (the socket was full).
    pub fn push_front(&mut self, lane: Lane, size: usize, item: T) {
        self.bytes += size;
        self.lanes[lane as usize].push_front((size, item));
    }

    /// Account bytes sent outside the queue (immediate datagrams) against
    /// the budget.
    pub fn charge(&mut self, now: Instant, size: usize) {
        self.refill(now);
        self.budget -= size as f64;
    }

    /// The next item allowed out now (reliable lane first), with its lane.
    pub fn pop(&mut self, now: Instant) -> Option<(Lane, T)> {
        self.refill(now);
        if self.budget <= 0.0 {
            return None;
        }
        for (i, lane) in [Lane::Reliable, Lane::Media].into_iter().enumerate() {
            if let Some((size, item)) = self.lanes[i].pop_front() {
                self.bytes -= size;
                self.budget -= size as f64;
                return Some((lane, item));
            }
        }
        None
    }

    /// Drop everything queued.
    pub fn clear(&mut self) {
        self.lanes.iter_mut().for_each(VecDeque::clear);
        self.bytes = 0;
    }

    /// Time until the next queued item may go (`None` = queue empty).
    pub fn next_release(&self, now: Instant) -> Option<Duration> {
        if self.is_empty() {
            return None;
        }
        let elapsed = self
            .last
            .map_or(0.0, |l| now.saturating_duration_since(l).as_secs_f64());
        let budget = self.budget + self.rate_bps / 8.0 * elapsed;
        if budget > 0.0 {
            return Some(Duration::ZERO);
        }
        Some(Duration::from_secs_f64(
            (-budget + 1.0) / (self.rate_bps / 8.0),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A simulated bottleneck: `rate` bits/s, FIFO queue, base one-way
    /// delay, random loss. Drives a Gcc with a saturating sender and returns
    /// (target after `secs`, mean queue delay ms over the last half).
    fn simulate(rate: f64, loss_pct: u64, secs: f64) -> (f64, f64, Gcc) {
        let mut gcc = Gcc::new();
        let mut hist = SentHistory::new();
        let mut rx = FeedbackRx::new();
        let mut pacer: Pacer<(u32, usize)> = Pacer::new();
        let t0 = Instant::now();
        let mut now = t0;
        let mut tseq = 0u32;
        let mut link_free = 0.0f64; // seconds
        let base = 0.020;
        let mut in_flight: VecDeque<(f64, u32)> = VecDeque::new(); // (arrival s, tseq)
        let mut rng = 0x1234_5678_9abc_def1u64;
        let mut qd_sum = 0.0f64;
        let mut qd_n = 0.0f64;
        let mut fb_queue: VecDeque<(f64, Vec<u8>)> = VecDeque::new();
        let step = 0.0005;
        let mut t = 0.0;
        while t < secs {
            // Saturating source: keep ~10 packets queued in the pacer.
            while pacer.queued(Lane::Media) < 10 {
                pacer.push(Lane::Media, 1200, (0, 1200));
            }
            // A saturating source sends at the target (the real sender is the
            // encoder at its bitrate; the pacer only spaces it).
            pacer.set_rate(gcc.target_bps());
            while let Some((_, (_, size))) = pacer.pop(now) {
                let send_us = (t * 1e6) as u64;
                hist.record(tseq, send_us, size);
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                if rng % 100 >= loss_pct {
                    let start = link_free.max(t);
                    if start - t < 0.2 {
                        link_free = start + size as f64 * 8.0 / rate;
                        in_flight.push_back((link_free + base, tseq));
                        if t > secs / 2.0 {
                            qd_sum += (start - t) * 1000.0;
                            qd_n += 1.0;
                        }
                    }
                }
                tseq += 1;
            }
            while in_flight.front().is_some_and(|(a, _)| *a <= t) {
                let (a, s) = in_flight.pop_front().unwrap();
                rx.on_packet(s, (a * 1e6) as u64);
            }
            if rx.due(now) {
                for m in rx.build(now, 3) {
                    fb_queue.push_back((t + base, m));
                }
            }
            while fb_queue.front().is_some_and(|(a, _)| *a <= t) {
                let (_, m) = fb_queue.pop_front().unwrap();
                let pk = parse_feedback(&m[1..]).unwrap();
                let reports: Vec<PacketReport> = pk
                    .iter()
                    .filter_map(|&(ts, arr)| {
                        let (send_us, size) = hist.get(ts)?;
                        let arrival_us = arr.map(|a| gcc.unwrap_remote(a));
                        Some(PacketReport {
                            tseq: ts,
                            send_us,
                            size,
                            arrival_us,
                        })
                    })
                    .collect();
                gcc.on_feedback(now, &reports, Some(Duration::from_millis(40)));
            }
            t += step;
            now = t0 + Duration::from_secs_f64(t);
        }
        (gcc.target_bps(), qd_sum / qd_n.max(1.0), gcc)
    }

    #[test]
    fn converges_near_capacity_with_a_short_queue() {
        let (target, qd, gcc) = simulate(20e6, 0, 20.0);
        eprintln!(
            "0% loss: target {:.1} Mbps, queue {:.1} ms, {:?}",
            target / 1e6,
            qd,
            gcc.stats()
        );
        assert!(target > 12e6 && target < 26e6, "target {target}");
        assert!(qd < 60.0, "queue {qd} ms");
        assert!(gcc.stats().overuses > 0, "the delay detector never fired");
    }

    #[test]
    fn random_loss_does_not_collapse_the_rate() {
        for loss in [1u64, 3, 5] {
            let (target, qd, gcc) = simulate(20e6, loss, 20.0);
            eprintln!(
                "{loss}% loss: target {:.1} Mbps, queue {:.1} ms, loss est {:.3}",
                target / 1e6,
                qd,
                gcc.loss()
            );
            assert!(target > 10e6, "{loss}% loss collapsed the rate to {target}");
        }
    }

    /// Paced video frames (an encoder at 80 % of the target, frames every
    /// 33 ms, paced at PACING_GAIN x target): the in-frame bursts that briefly
    /// exceed the capacity must not keep the estimate at half the link.
    #[test]
    fn paced_video_bursts_do_not_halve_the_estimate() {
        let rate = 20e6;
        let mut gcc = Gcc::new();
        let mut hist = SentHistory::new();
        let mut rx = FeedbackRx::new();
        let mut pacer: Pacer<usize> = Pacer::new();
        let t0 = Instant::now();
        let (mut t, step, base) = (0.0f64, 0.0005, 0.020);
        let mut tseq = 0u32;
        let mut link_free = 0.0f64;
        let mut in_flight: VecDeque<(f64, u32)> = VecDeque::new();
        let mut fb: VecDeque<(f64, Vec<u8>)> = VecDeque::new();
        let mut next_frame = 0.0;
        let mut max_q = 0.0f64;
        while t < 20.0 {
            let now = t0 + Duration::from_secs_f64(t);
            if t >= next_frame {
                next_frame += 1.0 / 30.0;
                let bytes = (gcc.target_bps() * 0.8 / 8.0 / 30.0) as usize;
                for _ in 0..bytes.div_ceil(1200) {
                    pacer.push(Lane::Media, 1200, 1200);
                }
            }
            pacer.set_rate(gcc.pacing_bps());
            while let Some((_, size)) = pacer.pop(now) {
                hist.record(tseq, (t * 1e6) as u64, size);
                let start = link_free.max(t);
                if t > 10.0 {
                    max_q = max_q.max(start - t);
                }
                link_free = start + size as f64 * 8.0 / rate;
                in_flight.push_back((link_free + base, tseq));
                tseq += 1;
            }
            while in_flight.front().is_some_and(|(a, _)| *a <= t) {
                let (a, s) = in_flight.pop_front().unwrap();
                rx.on_packet(s, (a * 1e6) as u64);
            }
            if rx.due(now) {
                for m in rx.build(now, 3) {
                    fb.push_back((t + base, m));
                }
            }
            while fb.front().is_some_and(|(a, _)| *a <= t) {
                let (_, m) = fb.pop_front().unwrap();
                let reports: Vec<PacketReport> = parse_feedback(&m[1..])
                    .unwrap()
                    .iter()
                    .filter_map(|&(ts, arr)| {
                        let (send_us, size) = hist.get(ts)?;
                        Some(PacketReport {
                            tseq: ts,
                            send_us,
                            size,
                            arrival_us: arr.map(|a| gcc.unwrap_remote(a)),
                        })
                    })
                    .collect();
                gcc.on_feedback(now, &reports, Some(Duration::from_millis(40)));
            }
            t += step;
        }
        eprintln!(
            "paced video: target {:.1} Mbps, max queue {:.1} ms, {:?}",
            gcc.target_bps() / 1e6,
            max_q * 1000.0,
            gcc.stats()
        );
        assert!(
            gcc.target_bps() > 15e6,
            "estimate stuck at {:.1} Mbps",
            gcc.target_bps() / 1e6
        );
        assert!(max_q < 0.060, "queue {max_q}");
    }

    /// A sender + Wi-Fi-like bottleneck driven in 0.5 ms steps: FIFO link at
    /// `rate`, 5 ms base one-way delay, and delay spikes (every ~1.5 s a
    /// 50-150 ms episode adding 20-40 ms, order preserved) when `jitter`.
    struct WifiSim {
        gcc: Gcc,
        hist: SentHistory,
        rx: FeedbackRx,
        pacer: Pacer<usize>,
        t0: Instant,
        t: f64,
        tseq: u32,
        rate: f64,
        link_free: f64,
        last_arrival: f64,
        in_flight: VecDeque<(f64, u32)>,
        fb: VecDeque<(f64, Vec<u8>)>,
        rng: u64,
        spike_until: f64,
        spike_add: f64,
        next_spike: f64,
        jitter: bool,
        delivered_bytes: u64,
    }

    impl WifiSim {
        fn new(rate: f64, jitter: bool) -> Self {
            Self {
                gcc: Gcc::new(),
                hist: SentHistory::new(),
                rx: FeedbackRx::new(),
                pacer: Pacer::new(),
                t0: Instant::now(),
                t: 0.0,
                tseq: 0,
                rate,
                link_free: 0.0,
                last_arrival: 0.0,
                in_flight: VecDeque::new(),
                fb: VecDeque::new(),
                rng: 0x9E37_79B9_7F4A_7C15,
                spike_until: 0.0,
                spike_add: 0.0,
                next_spike: 1.0,
                jitter,
                delivered_bytes: 0,
            }
        }
        fn rand(&mut self) -> f64 {
            self.rng ^= self.rng << 13;
            self.rng ^= self.rng >> 7;
            self.rng ^= self.rng << 17;
            (self.rng >> 11) as f64 / (1u64 << 53) as f64
        }
        /// Advance one step; `queue` gets the bytes to enqueue now.
        fn step(&mut self, enqueue: usize) {
            let base = 0.005;
            let now = self.t0 + Duration::from_secs_f64(self.t);
            for _ in 0..enqueue.div_ceil(1200) {
                self.pacer.push(Lane::Reliable, 1200, 1200);
            }
            if self.jitter && self.t >= self.next_spike {
                self.spike_until = self.t + 0.05 + 0.1 * self.rand();
                self.spike_add = 0.020 + 0.020 * self.rand();
                self.next_spike = self.t + 1.0 + self.rand();
            }
            self.pacer.set_rate(self.gcc.pacing_bps());
            while let Some((_, size)) = self.pacer.pop(now) {
                self.hist.record(self.tseq, (self.t * 1e6) as u64, size);
                let start = self.link_free.max(self.t);
                self.link_free = start + size as f64 * 8.0 / self.rate;
                let extra = if self.t < self.spike_until {
                    self.spike_add
                } else {
                    0.0
                };
                let arr = (self.link_free + base + extra).max(self.last_arrival);
                self.last_arrival = arr;
                self.in_flight.push_back((arr, self.tseq));
                self.tseq += 1;
            }
            while self.in_flight.front().is_some_and(|(a, _)| *a <= self.t) {
                let (a, s) = self.in_flight.pop_front().unwrap();
                self.delivered_bytes += 1200;
                self.rx.on_packet(s, (a * 1e6) as u64);
            }
            if self.rx.due(now) {
                for m in self.rx.build(now, 3) {
                    self.fb.push_back((self.t + base, m));
                }
            }
            while self.fb.front().is_some_and(|(a, _)| *a <= self.t) {
                let (_, m) = self.fb.pop_front().unwrap();
                let reports: Vec<PacketReport> = parse_feedback(&m[1..])
                    .unwrap()
                    .iter()
                    .filter_map(|&(ts, arr)| {
                        let (send_us, size) = self.hist.get(ts)?;
                        Some(PacketReport {
                            tseq: ts,
                            send_us,
                            size,
                            arrival_us: arr.map(|a| self.gcc.unwrap_remote(a)),
                        })
                    })
                    .collect();
                self.gcc
                    .on_feedback(now, &reports, Some(Duration::from_millis(12)));
            }
            self.t += 0.0005;
        }
    }

    /// Regression (live Wi-Fi sessions collapsed to 0.3 Mbps while idle,
    /// starving input and pings): an app-limited sender (keepalives, audio,
    /// a cursor - ~100 small packets/s) on a jittery Wi-Fi path keeps its
    /// estimate, and a 300 KB tile burst after 60 s idle still crosses a
    /// 20 Mbps link in a few hundred ms.
    #[test]
    fn idle_wifi_jitter_does_not_collapse_the_estimate() {
        let mut s = WifiSim::new(20e6, true);
        // 5 s of real load: the controller learns the capacity.
        while s.t < 5.0 {
            let q = s.pacer.queued(Lane::Reliable);
            s.step(if q < 20 { 12_000 } else { 0 });
        }
        let learned = s.gcc.target_bps();
        // 60 s idle: one ~200 B packet every 10 ms.
        let mut k = 0u64;
        let mut min_target = f64::MAX;
        while s.t < 65.0 {
            k += 1;
            s.step(if k.is_multiple_of(20) { 200 } else { 0 });
            min_target = min_target.min(s.gcc.target_bps());
        }
        let before_burst = s.gcc.target_bps();
        // A 300 KB burst.
        let start = s.t;
        let goal = s.delivered_bytes + 300_000;
        s.step(300_000);
        while s.delivered_bytes < goal && s.t < start + 10.0 {
            s.step(0);
        }
        let took = s.t - start;
        eprintln!(
            "idle Wi-Fi: learned {:.1} Mbps, min while idle {:.1}, before burst {:.1}, 300 KB in {:.0} ms, {:?}",
            learned / 1e6,
            min_target / 1e6,
            before_burst / 1e6,
            took * 1000.0,
            s.gcc.stats()
        );
        assert!(min_target >= MIN_RATE, "below the floor");
        assert!(
            before_burst >= 0.5 * learned,
            "idle collapse: {:.1} -> {:.1} Mbps",
            learned / 1e6,
            before_burst / 1e6
        );
        assert!(took < 0.4, "300 KB took {:.0} ms", took * 1000.0);
    }

    #[test]
    fn heavy_loss_backs_off() {
        let (target, _, gcc) = simulate(20e6, 25, 10.0);
        assert!(gcc.stats().loss_cuts > 0);
        assert!(target < 20e6);
    }

    #[test]
    fn feedback_round_trips_and_reports_holes() {
        let mut rx = FeedbackRx::new();
        let now = Instant::now();
        for (t, a) in [(10u32, 1000u64), (11, 1100), (13, 1300), (14, 1250)] {
            rx.on_packet(t, a);
        }
        let msgs = rx.build(now, 3);
        assert_eq!(msgs.len(), 1);
        let p = parse_feedback(&msgs[0][1..]).unwrap();
        assert_eq!(p.len(), 5);
        assert_eq!(p[0], (10, Some(1000)));
        assert_eq!(p[2], (12, None));
        assert_eq!(p[4], (14, Some(1250)));
        // Reported: a late packet is ignored, new ones continue from 15.
        rx.on_packet(12, 2000);
        rx.on_packet(15, 2100);
        let p = parse_feedback(&rx.build(now, 3)[0][1..]).unwrap();
        assert_eq!(p, vec![(15, Some(2100))]);
        assert!(parse_feedback(&[0; 5]).is_none());
    }

    #[test]
    fn pacer_spreads_a_burst_and_prioritizes_reliable() {
        let mut p: Pacer<u32> = Pacer::new();
        p.set_rate(8e6); // 1 MB/s
        let t0 = Instant::now();
        for i in 0..100 {
            p.push(Lane::Media, 1000, i);
        }
        p.push(Lane::Reliable, 100, 999);
        let mut out = Vec::new();
        let mut now = t0;
        while out.len() < 101 {
            while let Some((_, x)) = p.pop(now) {
                out.push((now, x));
            }
            now += Duration::from_micros(500);
        }
        assert_eq!(out[0].1, 999, "reliable first");
        let span = out.last().unwrap().0 - t0;
        // 100 KB at 1 MB/s ~ 100 ms (minus the initial burst budget).
        assert!(
            span >= Duration::from_millis(90) && span <= Duration::from_millis(110),
            "{span:?}"
        );
        assert!(p.next_release(now).is_none());
    }
}
