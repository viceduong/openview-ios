//! Two-way clock sync (NTP-style) between two peers' monotonic clocks.
//!
//! Each side stamps events on its own [`mono_us`] clock (microseconds since a
//! process-local anchor). A probe is the classic four-timestamp exchange:
//!
//! ```text
//! t1 local send ──PING──▶ t2 peer receive
//! t4 local recv ◀──PONG── t3 peer send      (t2, t3 ride back in the PONG)
//! ```
//!
//! giving `rtt = (t4 − t1) − (t3 − t2)` and
//! `offset = ((t2 − t1) + (t3 − t4)) / 2` (peer clock minus local clock).
//! The offset error is at most half the path asymmetry, which is smallest on
//! the probe that saw the least queueing — so [`ClockSync`] keeps a sliding
//! window of samples and trusts the **minimum-RTT** one. A short window also
//! tracks the slow drift between two crystal clocks (tens of ppm).

use std::collections::VecDeque;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// The process-local anchor of [`mono_us`].
fn anchor() -> Instant {
    static ANCHOR: OnceLock<Instant> = OnceLock::new();
    *ANCHOR.get_or_init(Instant::now)
}

/// This process's monotonic clock in microseconds (since a process-local
/// anchor, pinned by the first call; only differences, or values corrected
/// by a [`ClockSync`] offset, mean anything to a peer). Call it once early
/// (the latency types do on construction): [`instant_us`] reads instants
/// from before the anchor as 0.
pub fn mono_us() -> u64 {
    instant_us(Instant::now())
}

/// `t` on the [`mono_us`] clock (instants before the anchor clamp to 0).
pub fn instant_us(t: Instant) -> u64 {
    t.saturating_duration_since(anchor()).as_micros() as u64
}

/// The [`Instant`] a [`mono_us`] reading stands for.
pub fn us_instant(us: u64) -> Instant {
    anchor() + Duration::from_micros(us)
}

/// One completed probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockSample {
    /// Peer clock minus local clock, µs (positive = peer ahead).
    pub offset_us: i64,
    /// Round trip without the peer's turnaround time, µs.
    pub rtt_us: u64,
}

impl ClockSample {
    /// Compute a sample from the four timestamps (`t1`/`t4` local, `t2`/`t3`
    /// peer, all µs). `None` when they are inconsistent (the reply left the
    /// peer before it arrived, or came back before we sent).
    pub fn from_probe(t1: u64, t2: u64, t3: u64, t4: u64) -> Option<Self> {
        if t4 < t1 || t3 < t2 {
            return None;
        }
        let rtt_us = (t4 - t1).saturating_sub(t3 - t2);
        let (t1, t2, t3, t4) = (
            i128::from(t1),
            i128::from(t2),
            i128::from(t3),
            i128::from(t4),
        );
        let offset = ((t2 - t1) + (t3 - t4)) / 2;
        let offset_us = i64::try_from(offset).ok()?;
        Some(Self { offset_us, rtt_us })
    }
}

/// Peer-clock estimate: the minimum-RTT sample of the last
/// [`window`](Self::with_window) probes.
#[derive(Debug, Clone)]
pub struct ClockSync {
    window: usize,
    samples: VecDeque<ClockSample>,
}

impl ClockSync {
    /// Default window: 16 probes (≈ 16 s at one probe per second — short
    /// enough that crystal drift stays well under a millisecond).
    pub const DEFAULT_WINDOW: usize = 16;

    /// An estimator over [`DEFAULT_WINDOW`](Self::DEFAULT_WINDOW) probes.
    pub fn new() -> Self {
        Self::with_window(Self::DEFAULT_WINDOW)
    }

    /// An estimator over the last `window` probes (at least 1).
    pub fn with_window(window: usize) -> Self {
        Self {
            window: window.max(1),
            samples: VecDeque::new(),
        }
    }

    /// Add a completed probe (see [`ClockSample::from_probe`]); returns the
    /// sample, or `None` when the timestamps were inconsistent (ignored).
    pub fn add_probe(&mut self, t1: u64, t2: u64, t3: u64, t4: u64) -> Option<ClockSample> {
        let s = ClockSample::from_probe(t1, t2, t3, t4)?;
        self.add(s);
        Some(s)
    }

    /// Add a precomputed sample, evicting the oldest past the window.
    pub fn add(&mut self, s: ClockSample) {
        if self.samples.len() == self.window {
            self.samples.pop_front();
        }
        self.samples.push_back(s);
    }

    /// The trusted sample: the minimum RTT in the window (the newest on a
    /// tie, so drift is followed).
    pub fn best(&self) -> Option<ClockSample> {
        self.samples.iter().rev().min_by_key(|s| s.rtt_us).copied()
    }

    /// Peer clock minus local clock, µs (`None` before the first probe).
    pub fn offset_us(&self) -> Option<i64> {
        self.best().map(|s| s.offset_us)
    }

    /// The minimum RTT in the window, µs.
    pub fn rtt_us(&self) -> Option<u64> {
        self.best().map(|s| s.rtt_us)
    }

    /// A peer-clock reading converted to the local clock.
    pub fn to_local(&self, peer_us: u64) -> Option<u64> {
        let off = self.offset_us()?;
        Some((i128::from(peer_us) - i128::from(off)).max(0) as u64)
    }

    /// Probes in the window.
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// No probe completed yet.
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Forget every sample (e.g. the peer changed).
    pub fn clear(&mut self) {
        self.samples.clear();
    }
}

impl Default for ClockSync {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symmetric_path_recovers_the_offset_exactly() {
        // Peer 1000 µs ahead, 500 µs each way, 20 µs turnaround.
        let s = ClockSample::from_probe(0, 1500, 1520, 1020).unwrap();
        assert_eq!(s.offset_us, 1000);
        assert_eq!(s.rtt_us, 1000);
    }

    #[test]
    fn negative_offset_and_large_clocks() {
        // Peer 2 s behind, 3 ms each way, on clocks far from zero.
        let base = 9_000_000_000u64;
        let t1 = base + 2_000_000;
        let t2 = t1 + 3_000 - 2_000_000;
        let t3 = t2 + 100;
        let t4 = t1 + 6_100;
        let s = ClockSample::from_probe(t1, t2, t3, t4).unwrap();
        assert_eq!(s.offset_us, -2_000_000);
        assert_eq!(s.rtt_us, 6_000);
    }

    #[test]
    fn asymmetry_error_is_half_the_difference() {
        // True offset 0; forward 9 ms, back 1 ms: offset reads +4 ms.
        let s = ClockSample::from_probe(0, 9_000, 9_000, 10_000).unwrap();
        assert_eq!(s.offset_us, 4_000);
        assert_eq!(s.rtt_us, 10_000);
    }

    #[test]
    fn inconsistent_timestamps_are_rejected() {
        assert_eq!(ClockSample::from_probe(10, 5, 4, 20), None);
        assert_eq!(ClockSample::from_probe(10, 5, 6, 9), None);
        let mut c = ClockSync::new();
        assert!(c.add_probe(10, 5, 4, 20).is_none());
        assert!(c.is_empty());
        assert_eq!(c.offset_us(), None);
    }

    #[test]
    fn min_rtt_sample_wins_and_slides_out() {
        let mut c = ClockSync::with_window(3);
        // A queued probe (40 ms, 15 ms asymmetry), then a clean one (2 ms).
        c.add_probe(0, 30_000, 30_000, 40_000).unwrap();
        c.add_probe(100_000, 101_000 + 500, 101_000 + 500, 102_000)
            .unwrap();
        assert_eq!(c.rtt_us(), Some(2_000));
        assert_eq!(c.offset_us(), Some(500));
        assert_eq!(c.to_local(10_500), Some(10_000));
        // Two more noisy probes keep the clean one; a third evicts it.
        c.add(ClockSample {
            offset_us: 9,
            rtt_us: 50_000,
        });
        c.add(ClockSample {
            offset_us: 8,
            rtt_us: 60_000,
        });
        assert_eq!(c.offset_us(), Some(500));
        c.add(ClockSample {
            offset_us: 7,
            rtt_us: 70_000,
        });
        assert_eq!(c.len(), 3);
        assert_eq!(c.offset_us(), Some(9), "window holds 50/60/70 ms now");
    }

    #[test]
    fn ties_prefer_the_newest_sample() {
        let mut c = ClockSync::new();
        c.add(ClockSample {
            offset_us: 1,
            rtt_us: 100,
        });
        c.add(ClockSample {
            offset_us: 2,
            rtt_us: 100,
        });
        assert_eq!(c.offset_us(), Some(2));
    }

    #[test]
    fn mono_clock_round_trips_instants() {
        let a = mono_us();
        let t = us_instant(a);
        assert_eq!(instant_us(t), a);
        assert!(mono_us() >= a);
    }
}
