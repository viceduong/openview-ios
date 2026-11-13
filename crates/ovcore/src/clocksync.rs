//! Two-way clock sync (NTP-style): measures the peer↔local clock offset and the
//! round-trip time, used for latency stats and aligning media/remote-input timestamps.

use std::time::{SystemTime, UNIX_EPOCH};

/// Peer clock estimate from NTP-style probes: offset and round-trip time.
pub struct ClockSync {
    offset_ns: i64, // peer_clock - local_clock (positive = peer ahead)
    rtt_ns: i64,
    probed: bool,
}

impl ClockSync {
    /// A fresh estimator with zero offset/RTT and no probe applied yet.
    pub fn new() -> Self {
        Self {
            offset_ns: 0,
            rtt_ns: 0,
            probed: false,
        }
    }

    /// Local wall clock as ns since Unix epoch.
    pub fn now_ns() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    }

    /// Apply a completed probe: t0 = client send, t1 = server recv, t2 = server
    /// send, t3 = client recv (all in their own local epochs).
    pub fn apply_reply(&mut self, t0: u64, t1: u64, t2: u64, t3: u64) {
        let a = t1 as i64 - t0 as i64;
        let b = t2 as i64 - t3 as i64;
        self.offset_ns = (a + b) / 2;
        self.rtt_ns = t3.saturating_sub(t0) as i64;
        self.probed = true;
    }

    /// Peer clock minus local clock, in ns (positive = peer ahead).
    pub fn offset_ns(&self) -> i64 {
        self.offset_ns
    }
    /// Round-trip time of the last probe, in ns.
    pub fn rtt_ns(&self) -> i64 {
        self.rtt_ns
    }
    /// Round-trip time of the last probe, in ms.
    pub fn rtt_ms(&self) -> f64 {
        self.rtt_ns as f64 / 1e6
    }
    /// Whether at least one probe reply has been applied.
    pub fn probed(&self) -> bool {
        self.probed
    }
    /// Peer's current wall clock, estimated from the local clock + offset.
    pub fn peer_now_ns(&self) -> u64 {
        (Self::now_ns() as i64 + self.offset_ns).max(0) as u64
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
    fn offset_and_rtt() {
        let mut c = ClockSync::new();
        // simulate: server 1000ns ahead, 500ns one-way
        c.apply_reply(0, 1500, 2500, 1000); // t0=0 t1=1500 t2=2500 t3=1000
        assert_eq!(c.rtt_ns(), 1000);
        assert_eq!(c.offset_ns(), 1500); // ((1500-0) + (2500-1000)) / 2
        assert!(c.probed());
    }
}
