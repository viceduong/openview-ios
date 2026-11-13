//! Transport-wide feedback: the sender's [`SentHistory`] and the receiver's
//! [`FeedbackRx`] recorder / wire format.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use super::*;

/// Sender-side record of sent datagrams, indexed by `tseq`.
pub(crate) struct SentHistory {
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
    /// Transport-wide sequence number.
    pub tseq: u32,
    /// Sender clock, microseconds.
    pub send_us: u64,
    /// Datagram size on the wire, bytes.
    pub size: u32,
    /// Receiver clock (unwrapped), microseconds; `None` = not received.
    pub arrival_us: Option<i64>,
}

// ---------------------------------------------------------------------------
// Receiver: arrival recording + feedback messages
// ---------------------------------------------------------------------------

/// Receiver side: arrival times by `tseq`, turned into feedback messages.
#[derive(Debug, Default)]
pub(crate) struct FeedbackRx {
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
pub(crate) fn parse_feedback(p: &[u8]) -> Option<Vec<(u32, Option<u32>)>> {
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
