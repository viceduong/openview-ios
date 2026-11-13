//! The two-lane leaky-bucket pacer.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::*;

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
    /// An empty pacer at the start rate.
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

    /// The pacing rate, bits/s.
    pub fn rate(&self) -> f64 {
        self.rate_bps
    }

    /// Bytes queued.
    pub fn queued_bytes(&self) -> usize {
        self.bytes
    }

    /// Items queued in `lane`.
    pub fn queued(&self, lane: Lane) -> usize {
        self.lanes[lane as usize].len()
    }

    /// Whether both lanes are empty.
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

    /// Queue `item` (`size` wire bytes) at the back of `lane`.
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
