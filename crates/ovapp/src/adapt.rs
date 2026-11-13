//! `ovapp::adapt` — change detection + adaptive capture rate (v0.14).
//!
//! Desktops are mostly static; re-capturing and re-sending an unchanged
//! screen at full fps wastes GDI cycles, DEFLATE encode time and bandwidth.
//! This module holds the two pure cores the host capture loop uses:
//!
//! - [`frame_changed`] / [`frame_changed_fast`] — decide whether a freshly
//!   captured frame differs from the last *sent* frame.
//! - [`RateGovernor`] — eases the capture rate from `base_fps` down to
//!   `idle_fps` while the screen is static and snaps back to base on any
//!   change.
//! - [`AutoQuality`] — the AUTO-quality rule: picks the base rate and the
//!   H.264 bitrate scale from the link pacer's queueing-delay signal.
//!
//! All are pure (no I/O, no clock) so they are trivially unit-testable;
//! the host loop supplies the timing.

use std::time::Duration;

/// Exact frame comparison: the frames differ iff at least one pixel byte
/// differs. Length mismatch (different dims / stride) counts as a change.
///
/// This is the *authoritative* check — [`frame_changed_fast`] is only a
/// cheap pre-filter that may report a false "maybe different", never a
/// false "same" on differing frames (see its soundness note).
pub fn frame_changed(prev: &[u8], next: &[u8]) -> bool {
    prev.len() != next.len() || prev.iter().zip(next).any(|(a, b)| a != b)
}

/// Cheap pre-filter for [`frame_changed`].
///
/// Compares only every `stride`-th byte (plus the total length). Cost is
/// `len / stride` instead of `len`, so an 8 MB 1080p BGRA frame is touched
/// at ~64 KB for stride 128 instead of a full scan.
///
/// Soundness contract:
/// - **Identical frames always return `false`** (never a false "changed").
/// - **Any difference *on the sampled grid* returns `true`** — the caller
///   may trust a `true` directly, or confirm with the full compare.
/// - A difference *between* sampled bytes **may be missed** (`false` for
///   frames that actually differ). That is acceptable here: the question
///   the host asks is "is it worth re-sending the screen?", and a change
///   confined to unsampled bytes is, by construction, a tiny fraction of
///   the frame — skipping it costs at most one frame of visual latency
///   that the next sampled change (or any larger motion) flushes. The full
///   [`frame_changed`] remains exact for callers that need certainty.
///
/// `stride == 0` is treated as "sample everything" (falls back to the full
/// compare semantics on the sampled set = all bytes).
pub fn frame_changed_fast(prev: &[u8], next: &[u8], stride: u32) -> bool {
    if prev.len() != next.len() {
        return true;
    }
    let step = stride.max(1) as usize;
    prev.iter().zip(next).step_by(step).any(|(a, b)| a != b)
}

/// Pure decision the host capture loop applies to each fresh capture:
/// should this frame be encoded and sent?
///
/// - No previous frame (first capture) → send.
/// - Dimensions changed → send (the receiver needs a new geometry).
/// - Pre-filter finds a sampled difference → the frame definitely changed →
///   send immediately (the exact compare is implied, so it's skipped).
/// - Pre-filter sees no sampled difference → confirm with the exact full
///   compare, so a change confined to unsampled bytes is still caught.
///
/// `prev` is the last **sent** frame's pixels; `dims_changed` compares the
/// capture geometry against the geometry `prev` was captured at.
pub fn should_send(prev: Option<(&[u8], (u32, u32))>, dims_changed: bool, next: &[u8]) -> bool {
    match prev {
        None => true,
        Some((prev_px, _)) => {
            dims_changed
                || frame_changed_fast(prev_px, next, SAMPLE_STRIDE)
                || frame_changed(prev_px, next)
        }
    }
}

/// Byte stride used by the host's cheap pre-filter: sample roughly one byte
/// in 128 (~0.8% of an 8 MB frame ≈ 64 KB touched per idle capture).
pub const SAMPLE_STRIDE: u32 = 128;

/// Adaptive capture-rate governor.
///
/// State machine over one number: the current capture interval.
/// - [`on_change`](RateGovernor::on_change) snaps the interval back to
///   `1/base_fps` — motion is captured at full rate immediately.
/// - [`on_idle`](RateGovernor::on_idle) multiplies the interval by
///   [`IDLE_EASE`](RateGovernor::IDLE_EASE) each idle step, capped at
///   `1/idle_fps` — a static screen converges to the idle rate in a few
///   steps instead of jumping straight there (so a reappearing change is
///   picked up within ~one idle interval at worst).
///
/// Pure: it returns the interval to wait; the caller owns the clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateGovernor {
    base_interval: Duration,
    idle_interval: Duration,
    interval: Duration,
    idle_streak: u32,
}

impl RateGovernor {
    /// Easing factor applied to the capture interval per consecutive idle
    /// capture (1.5 → reaches a 30x slowdown in ~9 steps).
    pub const IDLE_EASE: f64 = 1.5;

    /// `base_fps` is the full-motion capture rate (10–30 typical);
    /// `idle_fps` the static-screen rate (2–5). `idle_fps` must be <
    /// `base_fps`; it is clamped so every produced interval is valid.
    pub fn new(base_fps: u32, idle_fps: u32) -> Self {
        let base_fps = base_fps.max(1);
        let idle_fps = idle_fps.max(1).min(base_fps);
        Self {
            base_interval: Duration::from_nanos(1_000_000_000 / u64::from(base_fps)),
            idle_interval: Duration::from_nanos(1_000_000_000 / u64::from(idle_fps)),
            interval: Duration::from_nanos(1_000_000_000 / u64::from(base_fps)),
            idle_streak: 0,
        }
    }

    /// A changed frame was just sent: return the interval until the next
    /// capture (full base rate).
    pub fn on_change(&mut self) -> Duration {
        self.interval = self.base_interval;
        self.idle_streak = 0;
        self.interval
    }

    /// An unchanged capture: ease the interval toward `1/idle_fps` and
    /// return it.
    pub fn on_idle(&mut self) -> Duration {
        self.idle_streak = self.idle_streak.saturating_add(1);
        if self.interval < self.idle_interval {
            let eased = (self.interval.as_secs_f64() * Self::IDLE_EASE)
                .min(self.idle_interval.as_secs_f64());
            self.interval = Duration::from_secs_f64(eased);
        }
        self.interval
    }

    /// Interval to wait before the next capture (whatever happened last).
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// Retarget the full-motion rate (v0.17): the auto-quality loop calls
    /// this every capture tick with the RTT-derived (or manually requested)
    /// base fps. Only the base target changes — the current interval and the
    /// idle streak are left alone, so the idle ease-out continues smoothly
    /// and the next [`on_change`](Self::on_change) snaps to the new base.
    pub fn set_base_fps(&mut self, base_fps: u32) {
        self.base_interval = Duration::from_nanos(1_000_000_000 / u64::from(base_fps.max(1)));
    }

    /// Metrics snapshot for logging: idle streak, current interval, and
    /// whether the idle rate has been reached.
    pub fn stats(&self) -> RateStats {
        RateStats {
            idle_streak: self.idle_streak,
            interval: self.interval,
            at_idle_rate: self.interval >= self.idle_interval,
        }
    }
}

/// Snapshot of governor state for metrics/logging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateStats {
    /// Consecutive unchanged captures.
    pub idle_streak: u32,
    /// Current capture interval.
    pub interval: Duration,
    /// True once the interval has reached the idle cap.
    pub at_idle_rate: bool,
}

/// Thresholds and rates of the auto-quality rule ([`AutoQuality`]).
pub mod qos {
    use std::time::Duration;

    /// Excess queueing delay below which the link is healthy: the target
    /// fps is kept (and restored), the H.264 bitrate scale recovers.
    pub const KEEP_BELOW: Duration = Duration::from_millis(50);
    /// Excess queueing delay above which the link is congested: once the
    /// bitrate scale is at its floor, the fps is reduced gradually.
    pub const CUT_ABOVE: Duration = Duration::from_millis(150);
    /// The auto rule never goes below this rate (or `base`, if lower).
    pub const MIN_FPS: u32 = 5;
    /// Floor of the H.264 bitrate scale (fraction of the link-derived rate).
    pub const MIN_BITRATE_SCALE: f64 = 0.5;
    /// Bitrate-scale decrease per second in the 50–150 ms band.
    pub const BITRATE_DOWN_PER_S: f64 = 0.25;
    /// Bitrate-scale decrease per second above 150 ms.
    pub const BITRATE_DOWN_FAST_PER_S: f64 = 0.5;
    /// Bitrate-scale recovery per second below 50 ms.
    pub const BITRATE_UP_PER_S: f64 = 0.2;
    /// Fps reduction per second above 150 ms (fraction of the current fps).
    pub const FPS_DOWN_PER_S: f64 = 0.3;
    /// Fps recovery per second below 50 ms (fraction of `base`).
    pub const FPS_UP_PER_S: f64 = 0.25;
    /// Longest tick the rule integrates over (a stalled caller must not
    /// make one huge step).
    pub const MAX_STEP: Duration = Duration::from_millis(500);
}

/// One [`AutoQuality`] verdict.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QualityDecision {
    /// The base capture rate to use.
    pub fps: u32,
    /// Scale for the H.264 bitrate (multiplies the pacer's delivery-rate
    /// estimate the encoder bitrate follows), `MIN_BITRATE_SCALE..=1`.
    pub bitrate_scale: f64,
    /// The rule is currently holding quality below the target (bitrate or
    /// fps) — callers should not force a higher rate on top (e.g. the
    /// H.264 motion minimum).
    pub degraded: bool,
}

/// Auto-quality rule (AUTO mode; replaces the v0.17 RTT bands), in the
/// spirit of RustDesk's `video_qos`: it is driven by the **excess queueing
/// delay** the link pacer measures ([`crate::pacing::LinkPacer::queue_delay`],
/// RTT above the path's minimum) — never by the raw RTT, so a long but
/// uncongested relay path keeps the full rate. The pacer's **delivery
/// rate** sets the H.264 bitrate; this rule scales it.
///
/// | excess queue delay  | action                                              |
/// |---------------------|-----------------------------------------------------|
/// | none / `< 50 ms`    | recover: fps back toward `base` (+25 % of base/s), then bitrate scale (+0.2/s) |
/// | `50 ..= 150 ms`     | hold fps; lower the H.264 bitrate scale (−0.25/s, floor 0.5) |
/// | `> 150 ms`          | lower the bitrate scale fast (−0.5/s); once at its floor, cut fps by 30 %/s (floor 5 fps) |
///
/// So a congested link first gives up picture quality, and only sustained
/// congestion (≳ 1 s past 150 ms) costs frames. Pure: the caller passes
/// `now`.
#[derive(Debug, Clone, PartialEq)]
pub struct AutoQuality {
    /// Current fps as a fraction of `base` (so a base change keeps state).
    ratio: f64,
    bitrate_scale: f64,
    last: Option<std::time::Instant>,
}

impl Default for AutoQuality {
    fn default() -> Self {
        Self::new()
    }
}

impl AutoQuality {
    pub fn new() -> Self {
        Self {
            ratio: 1.0,
            bitrate_scale: 1.0,
            last: None,
        }
    }

    /// Advance the rule to `now` with the current congestion signal and
    /// return the decision for target rate `base`.
    pub fn update(
        &mut self,
        now: std::time::Instant,
        base: u32,
        queue_delay: Option<Duration>,
    ) -> QualityDecision {
        let base = base.max(1);
        let floor_fps = qos::MIN_FPS.min(base);
        let min_ratio = f64::from(floor_fps) / f64::from(base);
        let dt = self
            .last
            .map_or(Duration::ZERO, |t| now.saturating_duration_since(t))
            .min(qos::MAX_STEP)
            .as_secs_f64();
        self.last = Some(now);
        let delay = queue_delay.unwrap_or(Duration::ZERO);
        if delay < qos::KEEP_BELOW {
            if self.ratio < 1.0 {
                self.ratio = (self.ratio + qos::FPS_UP_PER_S * dt).min(1.0);
            } else {
                self.bitrate_scale = (self.bitrate_scale + qos::BITRATE_UP_PER_S * dt).min(1.0);
            }
        } else if delay <= qos::CUT_ABOVE {
            self.bitrate_scale =
                (self.bitrate_scale - qos::BITRATE_DOWN_PER_S * dt).max(qos::MIN_BITRATE_SCALE);
        } else if self.bitrate_scale > qos::MIN_BITRATE_SCALE {
            self.bitrate_scale = (self.bitrate_scale - qos::BITRATE_DOWN_FAST_PER_S * dt)
                .max(qos::MIN_BITRATE_SCALE);
        } else {
            self.ratio = (self.ratio * (1.0 - qos::FPS_DOWN_PER_S * dt)).max(min_ratio);
        }
        let fps = ((f64::from(base) * self.ratio).round() as u32).clamp(floor_fps, base);
        QualityDecision {
            fps,
            bitrate_scale: self.bitrate_scale,
            degraded: fps < base || self.bitrate_scale < 1.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_frames_are_unchanged() {
        let a = vec![7u8; 4096];
        assert!(!frame_changed(&a, &a));
        assert!(!frame_changed_fast(&a, &a.clone(), 128));
    }

    #[test]
    fn one_byte_difference_is_a_change() {
        let a = vec![0u8; 4096];
        let mut b = a.clone();
        b[2048] = 1; // 2048 = 128*16, a sampled position
        assert!(frame_changed(&a, &b));
        // Sampled position → the fast filter catches it outright.
        assert!(frame_changed_fast(&a, &b, 128));
        // Unsampled position → the fast filter may miss it (documented),
        // but must never *claim* a change that isn't there, and the full
        // compare is exact.
        let mut c = a.clone();
        c[2001] = 1;
        if !frame_changed_fast(&a, &c, 128) {
            assert!(frame_changed(&a, &c), "full compare must be exact");
        }
    }

    #[test]
    fn length_mismatch_is_a_change() {
        assert!(frame_changed(&[1, 2, 3], &[1, 2, 3, 4]));
        assert!(frame_changed_fast(&[1, 2, 3], &[1, 2, 3, 4], 2));
    }

    #[test]
    fn fast_filter_never_false_positives_on_identical() {
        let a: Vec<u8> = (0..=255u8).cycle().take(100_000).collect();
        assert!(!frame_changed_fast(&a, &a, 1));
        assert!(!frame_changed_fast(&a, &a, 7));
        assert!(!frame_changed_fast(&a, &a, 0)); // stride 0 = sample all
    }

    #[test]
    fn governor_starts_at_base_rate() {
        let g = RateGovernor::new(20, 4);
        assert_eq!(g.interval(), Duration::from_millis(50));
        assert!(!g.stats().at_idle_rate);
    }

    #[test]
    fn governor_idle_eases_to_idle_rate_and_stays() {
        let idle = Duration::from_millis(250); // 4 fps
        let mut g = RateGovernor::new(20, 4);
        let mut last = g.interval();
        for _ in 0..100 {
            let next = g.on_idle();
            assert!(
                next >= last || last >= idle,
                "interval must never shrink while idling"
            );
            assert!(next <= idle, "interval must never exceed 1/idle_fps");
            last = next;
        }
        assert_eq!(last, idle, "must converge to the idle rate");
        assert!(g.stats().at_idle_rate);
        assert_eq!(g.stats().idle_streak, 100);
    }

    #[test]
    fn governor_change_snaps_back_to_base() {
        let mut g = RateGovernor::new(20, 4);
        for _ in 0..50 {
            g.on_idle();
        }
        assert_eq!(g.interval(), Duration::from_millis(250));
        assert_eq!(g.on_change(), Duration::from_millis(50));
        assert_eq!(g.stats().idle_streak, 0);
        // And idling again eases from base, not from idle.
        let next = g.on_idle();
        assert!(next > Duration::from_millis(50) && next <= Duration::from_millis(250));
    }

    #[test]
    fn governor_clamps_bad_fps() {
        // idle_fps >= base_fps is clamped to base; zero fps clamps to 1.
        let mut g = RateGovernor::new(0, 999);
        assert_eq!(g.interval(), Duration::from_secs(1));
        assert_eq!(g.on_idle(), Duration::from_secs(1));
    }

    #[test]
    fn should_send_first_frame_and_dims_change() {
        let px = vec![0u8; 16];
        assert!(should_send(None, false, &px), "first frame always sends");
        assert!(
            should_send(Some((&px, (4, 4))), true, &px),
            "dims change always sends"
        );
    }

    #[test]
    fn should_send_catches_unsampled_change() {
        // A change confined to a byte the stride-128 pre-filter skips must
        // still be sent: the pre-filter's "identical on the grid" is resolved
        // by the exact full compare. Regression for the old `&&` form, which
        // short-circuited to "unchanged" and never consulted the exact compare.
        let prev = vec![0u8; 4096];
        let mut next = prev.clone();
        next[2001] = 1; // 2001 % 128 != 0 → not on the sample grid
        assert!(
            should_send(Some((prev.as_slice(), (4, 4))), false, &next),
            "an unsampled change must still be sent (exact fallback)"
        );
    }

    #[test]
    fn should_send_static_screen_skips_after_first() {
        let px = vec![3u8; 64 * 64 * 4];
        let prev = (&px[..], (64u32, 64u32));
        assert!(!should_send(Some(prev), false, &px), "static → skip");
    }

    #[test]
    fn should_send_change_confirms_with_full_compare() {
        let px = vec![3u8; 64 * 64 * 4];
        let mut moved = px.clone();
        moved[10] ^= 0xff; // unsampled at stride 128 → pre-filter may miss
        let prev = (&px[..], (64u32, 64u32));
        if !should_send(Some(prev), false, &moved) {
            // Pre-filter missed a single unsampled byte — allowed, but the
            // full compare must still report the truth.
            assert!(frame_changed(&px, &moved));
        }
        // A sampled-position change is always sent.
        let mut moved2 = px.clone();
        moved2[0] ^= 0xff;
        assert!(should_send(Some(prev), false, &moved2));
    }

    /// Drive the rule at 20 Hz for `secs` with a constant signal; returns
    /// every decision.
    fn run(
        q: &mut AutoQuality,
        t: &mut std::time::Instant,
        base: u32,
        delay: Option<Duration>,
        secs: f64,
    ) -> Vec<QualityDecision> {
        let ticks = (secs * 20.0).round() as usize;
        (0..ticks)
            .map(|_| {
                *t += Duration::from_millis(50);
                q.update(*t, base, delay)
            })
            .collect()
    }

    fn ms(v: u64) -> Option<Duration> {
        Some(Duration::from_millis(v))
    }

    #[test]
    fn auto_quality_keeps_full_rate_on_a_long_but_clean_path() {
        // A relayed session: 200 ms RTT but a stable one — the pacer
        // reports ~0-20 ms of *excess* delay. Raw RTT must not cut fps.
        let mut q = AutoQuality::new();
        let mut t = std::time::Instant::now();
        for d in run(&mut q, &mut t, 30, ms(20), 10.0) {
            assert_eq!(d.fps, 30);
            assert_eq!(d.bitrate_scale, 1.0);
            assert!(!d.degraded);
        }
        // No signal at all (static screen, nothing probed): full rate.
        let d = run(&mut q, &mut t, 30, None, 1.0);
        assert_eq!(d.last().unwrap().fps, 30);
    }

    #[test]
    fn auto_quality_lowers_bitrate_before_fps() {
        let mut q = AutoQuality::new();
        let mut t = std::time::Instant::now();
        // 50..150 ms: bitrate gives way, fps holds — indefinitely.
        let d = run(&mut q, &mut t, 30, ms(100), 10.0);
        assert!(
            d.iter().all(|d| d.fps == 30),
            "the middle band never cuts fps"
        );
        let last = *d.last().unwrap();
        assert_eq!(last.bitrate_scale, qos::MIN_BITRATE_SCALE);
        assert!(last.degraded);

        // > 150 ms from a fresh state: the first ~second only costs bitrate.
        let mut q = AutoQuality::new();
        let d = run(&mut q, &mut t, 30, ms(300), 0.5);
        let last = *d.last().unwrap();
        assert_eq!(last.fps, 30, "fps kept while the bitrate can still give");
        assert!(last.bitrate_scale < 1.0 && last.bitrate_scale > qos::MIN_BITRATE_SCALE);
        // Sustained: fps then decreases gradually, never increasing.
        let d = run(&mut q, &mut t, 30, ms(300), 3.0);
        let mut prev = 30;
        for x in &d {
            assert!(x.fps <= prev, "fps never rises under congestion");
            prev = x.fps;
        }
        let after3 = d.last().unwrap().fps;
        assert!(after3 < 30 && after3 > 10, "gradual, not a cliff: {after3}");
        // ... down to the floor.
        let d = run(&mut q, &mut t, 30, ms(300), 20.0);
        assert_eq!(d.last().unwrap().fps, qos::MIN_FPS);
    }

    #[test]
    fn auto_quality_recovers_fps_first_then_bitrate() {
        let mut q = AutoQuality::new();
        let mut t = std::time::Instant::now();
        run(&mut q, &mut t, 30, ms(400), 30.0);
        // Healthy again: fps comes back within ~4 s, bitrate after it.
        let d = run(&mut q, &mut t, 30, ms(10), 3.0);
        let mid = *d.last().unwrap();
        assert!(mid.fps > qos::MIN_FPS);
        assert_eq!(mid.bitrate_scale, qos::MIN_BITRATE_SCALE, "fps first");
        let d = run(&mut q, &mut t, 30, ms(10), 6.0);
        let done = *d.last().unwrap();
        assert_eq!(
            (done.fps, done.bitrate_scale, done.degraded),
            (30, 1.0, false)
        );
    }

    #[test]
    fn auto_quality_steps_are_bounded_and_base_changes_keep_state() {
        let mut q = AutoQuality::new();
        let t0 = std::time::Instant::now();
        q.update(t0, 30, ms(400));
        // A 10 s gap integrates as at most MAX_STEP.
        let d = q.update(t0 + Duration::from_secs(10), 30, ms(400));
        assert!(d.bitrate_scale >= 1.0 - qos::BITRATE_DOWN_FAST_PER_S * 0.5 - 1e-9);
        // A tiny base is never raised above itself.
        let mut q = AutoQuality::new();
        let d = q.update(t0, 2, ms(900));
        assert_eq!(d.fps, 2);
        assert_eq!(q.update(t0, 0, None).fps, 1, "base 0 clamps to 1");
    }
    #[test]
    fn set_base_fps_retargets_without_disturbing_the_idle_state() {
        let mut g = RateGovernor::new(20, 4);
        for _ in 0..50 {
            g.on_idle();
        }
        let eased = g.interval();
        // Retarget the base: the eased interval and idle streak stay put.
        g.set_base_fps(60);
        assert_eq!(
            g.interval(),
            eased,
            "set_base_fps must not reset the interval"
        );
        assert_eq!(g.stats().idle_streak, 50, "idle streak survives a retarget");
        // The next change snaps to the NEW base rate.
        assert_eq!(g.on_change(), Duration::from_nanos(1_000_000_000 / 60));
        // Zero fps is clamped to 1 like in the constructor.
        g.set_base_fps(0);
        assert_eq!(g.on_change(), Duration::from_secs(1));
    }
}
