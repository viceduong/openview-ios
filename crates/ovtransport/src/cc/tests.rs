use std::collections::VecDeque;
use std::time::{Duration, Instant};

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
