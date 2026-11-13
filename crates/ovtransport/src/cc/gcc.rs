//! The delay-based (GCC trendline) + loss-based rate controller.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::*;

// ---------------------------------------------------------------------------
// Trendline (delay gradient) estimator
// ---------------------------------------------------------------------------

/// Bandwidth usage hypothesis of the delay detector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Usage {
    /// No significant delay trend.
    Normal,
    /// Queueing delay is growing: the path is over capacity.
    Overuse,
    /// Queueing delay is draining.
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
    /// Feedback messages processed.
    pub feedbacks: u64,
    /// Datagrams covered by feedback.
    pub reported: u64,
    /// Reported datagrams that never arrived.
    pub lost: u64,
    /// Overuse detections that cut the target rate.
    pub overuses: u64,
    /// Rate cuts caused by sustained loss.
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
    /// (arrival, queueing delay) of the last `STANDING_WINDOW_US`.
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
    /// A controller at `START_RATE`, in startup.
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
    /// The pacing rate: `PACING_GAIN` x the target, but never above the
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

    /// Measured receive (acknowledged) rate, bits/s, once known.
    pub fn acked_bps(&self) -> Option<f64> {
        self.acked
    }

    /// Smoothed one-way queueing delay estimate (excess over the minimum).
    pub fn queue_delay(&self) -> Duration {
        Duration::from_micros(self.queue_us.max(0.0) as u64)
    }

    /// The delay detector's current hypothesis.
    pub fn usage(&self) -> Usage {
        self.trend.state
    }

    /// Whether the controller is still in its fast-startup phase.
    pub fn in_startup(&self) -> bool {
        self.startup
    }

    /// Counters.
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

    /// The minimum queueing delay over the last `STANDING_WINDOW_US` of
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
