//! `--latency-probe N`: objective input-to-photon measurement against
//! `ovhost --latency-probe`.
//!
//! The host flips a 64x64 patch at its screen's top-left between magenta
//! and green on every input press. We press a key (F24, which nothing binds)
//! N times, one at a time, and time each press from the moment it was sent
//! until the first presented frame whose patch shows the other colour.

use std::time::{Duration, Instant};

use ovapp::latency::quantile;
use ovapp::screen::Frame;

/// The key pressed (VK_F24: harmless on the host).
pub(super) const PROBE_VK: u16 = 0x87;
/// Pause between one press being seen and the next press.
const GAP: Duration = Duration::from_millis(300);
/// A press whose flip never shows counts as a timeout after this long.
const TIMEOUT: Duration = Duration::from_secs(3);
/// Patch centre sampled (inside the 64x64 patch).
const SAMPLE_AT: u32 = 32;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    /// Waiting for a frame that shows the patch (the baseline colour).
    Baseline,
    /// Next press due at the instant.
    Idle(Instant),
    /// A press went out at `sent`; waiting for the patch to leave `from`.
    Waiting { sent: Instant, from: bool },
    /// The flip is in a frame handed to the window; it counts when that
    /// frame is presented.
    Flipped { sent: Instant, now: bool },
}

/// One `--latency-probe` run.
pub(super) struct LatencyProbe {
    left: u32,
    total: u32,
    phase: Phase,
    /// The last colour seen (`true` = green).
    shown: Option<bool>,
    samples_ms: Vec<f64>,
    timeouts: u32,
}

impl LatencyProbe {
    /// A run of `n` presses.
    pub(super) fn new(n: u32) -> Self {
        Self {
            left: n,
            total: n,
            phase: Phase::Baseline,
            shown: None,
            samples_ms: Vec::new(),
            timeouts: 0,
        }
    }

    /// The patch colour in `f` (`true` = green, `false` = magenta, `None`:
    /// frame too small or neither colour).
    pub(super) fn patch(f: &Frame) -> Option<bool> {
        if f.width <= SAMPLE_AT || f.height <= SAMPLE_AT {
            return None;
        }
        let i = (SAMPLE_AT as usize) * f.stride as usize + SAMPLE_AT as usize * 4;
        let px = f.pixels.get(i..i + 4)?;
        let (b, g, r) = (px[0], px[1], px[2]);
        match (g > 160, r > 160 && b > 160) {
            (true, false) => Some(true),
            (false, true) => Some(false),
            _ => None,
        }
    }

    /// A frame is about to be presented: note the patch colour.
    pub(super) fn observe(&mut self, f: &Frame, now: Instant) {
        let Some(c) = Self::patch(f) else {
            return;
        };
        self.shown = Some(c);
        match self.phase {
            Phase::Baseline => self.phase = Phase::Idle(now + GAP),
            Phase::Waiting { sent, from } if c != from => {
                self.phase = Phase::Flipped { sent, now: c };
            }
            _ => {}
        }
    }

    /// The frame handed over last reached the screen at `at`.
    pub(super) fn presented(&mut self, at: Instant) {
        if let Phase::Flipped { sent, .. } = self.phase {
            self.samples_ms
                .push(at.saturating_duration_since(sent).as_secs_f64() * 1000.0);
            self.left -= 1;
            self.phase = Phase::Idle(at + GAP);
        }
    }

    /// Whether a press is due now (the caller sends it and calls
    /// [`sent`](Self::sent)); also turns a stuck press into a timeout.
    pub(super) fn due(&mut self, now: Instant) -> bool {
        match self.phase {
            Phase::Idle(at) => self.left > 0 && now >= at,
            Phase::Waiting { sent, .. } if now.saturating_duration_since(sent) > TIMEOUT => {
                self.timeouts += 1;
                self.left -= 1;
                self.phase = Phase::Idle(now);
                false
            }
            _ => false,
        }
    }

    /// The press went out at `at`.
    pub(super) fn sent(&mut self, at: Instant) {
        if let Some(from) = self.shown {
            self.phase = Phase::Waiting { sent: at, from };
        }
    }

    /// All presses measured (or timed out).
    pub(super) fn done(&self) -> bool {
        self.left == 0
    }

    /// The result line.
    pub(super) fn report(&self) -> String {
        let mut v = self.samples_ms.clone();
        v.sort_by(f64::total_cmp);
        let us: Vec<u32> = v.iter().map(|ms| (ms * 1000.0) as u32).collect();
        let q = |p: f64| quantile(&us, p).map_or(0.0, |x| f64::from(x) / 1000.0);
        format!(
            "latency probe: input-to-photon p50 {:.1} ms p95 {:.1} ms p99 {:.1} ms \
             (n {} of {}, {} timeout(s))",
            q(0.5),
            q(0.95),
            q(0.99),
            us.len(),
            self.total,
            self.timeouts
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ovapp::screen::PixelFormat;

    fn frame(bgra: [u8; 4]) -> Frame {
        let (w, h) = (64u32, 64u32);
        Frame {
            width: w,
            height: h,
            stride: w * 4,
            format: PixelFormat::Bgra,
            seq: 0,
            pixels: bgra.repeat((w * h) as usize),
        }
    }

    #[test]
    fn colours_are_classified_with_codec_slack() {
        assert_eq!(
            LatencyProbe::patch(&frame([250, 10, 245, 255])),
            Some(false)
        );
        assert_eq!(LatencyProbe::patch(&frame([12, 240, 8, 255])), Some(true));
        assert_eq!(LatencyProbe::patch(&frame([128, 128, 128, 255])), None);
    }

    #[test]
    fn a_press_is_timed_to_the_present_of_the_flipped_frame() {
        let t = Instant::now();
        let ms = |n: u64| t + Duration::from_millis(n);
        let mut p = LatencyProbe::new(2);
        assert!(!p.due(ms(0)), "no baseline yet");
        p.observe(&frame([255, 0, 255, 255]), ms(0));
        assert!(!p.due(ms(100)));
        assert!(p.due(ms(300)));
        p.sent(ms(300));
        // Same colour again (another update): still waiting.
        p.observe(&frame([255, 0, 255, 255]), ms(310));
        p.presented(ms(311));
        p.observe(&frame([0, 255, 0, 255]), ms(330));
        p.presented(ms(342));
        assert!(!p.done());
        // The second press never shows: timeout.
        assert!(p.due(ms(642)));
        p.sent(ms(642));
        assert!(!p.due(ms(1000)));
        assert!(!p.due(ms(642 + 3_001)));
        assert!(p.done());
        let r = p.report();
        assert!(r.contains("p50 42.0 ms"), "{r}");
        assert!(r.contains("n 1 of 2, 1 timeout(s)"), "{r}");
    }
}
