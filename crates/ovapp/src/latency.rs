//! Per-stage latency telemetry ([`CAP_TIMING`]).
//!
//! Where does a screen update's latency go? The host stamps every update on
//! its own monotonic clock ([`ovcore::clocksync::mono_us`]) and sends the
//! stamps in a small `TIMING` record ([`FrameTiming`]) right before the
//! update's first byte; the viewer stamps reassembly, decode and present on
//! its clock, and converts the host stamps with the NTP-style offset that
//! [`ClockProbe`] measures over the PING/PONG pair (timed PONGs carry the
//! host's receive/send times). [`LatencyTracker`] joins the two per update
//! and keeps rolling percentiles per [`Stage`]:
//!
//! ```text
//! host:   capture ─cap─▶ grabbed ─enc─▶ encoded ─send─▶ first byte out
//! viewer:                         ─net─▶ reassembled ─dec─▶ decoded ─pres─▶ presented
//! total = presented − capture (both on the viewer clock)
//! ```
//!
//! `net` is the one-way time from the host handing the first byte to the
//! transport until the whole update is reassembled (it includes the
//! update's serialization on the link); it and `total` need the clock
//! offset, every other stage is a same-clock difference.
//!
//! [`CAP_TIMING`]: crate::terminal::CAP_TIMING

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use ovcore::clocksync::{instant_us, ClockSample, ClockSync};

/// [`FrameTiming::kind`]: a screen update (FRAME_BEGIN / TILES_BEGIN; `seq`
/// is the update sequence).
pub const KIND_SCREEN: u8 = 0;
/// [`FrameTiming::kind`]: an H.264 access unit (`seq` is the video sequence).
pub const KIND_VIDEO: u8 = 1;

/// Host-side stamps of one update, on the host's monotonic µs clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HostStamps {
    /// When the captured image was presented on the host (DXGI
    /// `LastPresentTime`), or the grab start when unknown.
    pub capture_us: u64,
    /// When the grab returned the pixels.
    pub grabbed_us: u64,
    /// When the encoded update was handed to the sender (0 = not yet).
    pub encoded_us: u64,
}

/// The `TIMING` record: one update's host stamps, as a capture time plus
/// compact stage durations.
///
/// Wire form (29 bytes, BE): `1B kind` + `8B seq` + `8B capture_us` +
/// `4B grab_us` + `4B encode_us` + `4B queue_us`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameTiming {
    /// [`KIND_SCREEN`] or [`KIND_VIDEO`].
    pub kind: u8,
    /// The update's sequence (screen or video sequence, per `kind`).
    pub seq: u64,
    /// Capture time on the host clock, µs.
    pub capture_us: u64,
    /// Capture → grab done, µs.
    pub grab_us: u32,
    /// Grab done → encode done, µs.
    pub encode_us: u32,
    /// Encode done → first byte handed to the transport, µs.
    pub queue_us: u32,
}

impl FrameTiming {
    /// Wire length of the record payload.
    pub const LEN: usize = 29;

    /// The record for an update with `stamps`, its first byte going out at
    /// `sent_us` (host clock). Stages saturate (a clock read out of order
    /// becomes 0, a stage over ~71 minutes caps at `u32::MAX`).
    pub fn from_stamps(kind: u8, seq: u64, stamps: &HostStamps, sent_us: u64) -> Self {
        let span = |a: u64, b: u64| b.saturating_sub(a).min(u64::from(u32::MAX)) as u32;
        let grabbed = stamps.grabbed_us.max(stamps.capture_us);
        let encoded = stamps.encoded_us.max(grabbed);
        Self {
            kind,
            seq,
            capture_us: stamps.capture_us,
            grab_us: span(stamps.capture_us, grabbed),
            encode_us: span(grabbed, encoded),
            queue_us: span(encoded, sent_us.max(encoded)),
        }
    }

    /// When the first byte went out, on the host clock.
    pub fn sent_us(&self) -> u64 {
        self.capture_us
            + u64::from(self.grab_us)
            + u64::from(self.encode_us)
            + u64::from(self.queue_us)
    }

    /// Append the wire form.
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.push(self.kind);
        out.extend_from_slice(&self.seq.to_be_bytes());
        out.extend_from_slice(&self.capture_us.to_be_bytes());
        out.extend_from_slice(&self.grab_us.to_be_bytes());
        out.extend_from_slice(&self.encode_us.to_be_bytes());
        out.extend_from_slice(&self.queue_us.to_be_bytes());
    }

    /// Parse [`encode`](Self::encode)'s output (exactly [`LEN`](Self::LEN)
    /// bytes).
    pub fn decode(p: &[u8]) -> Result<Self, &'static str> {
        if p.len() != Self::LEN {
            return Err("TIMING payload must be 29 bytes");
        }
        // Length checked above: every fixed sub-slice is in range.
        let u32_at = |i: usize| u32::from_be_bytes(p[i..i + 4].try_into().unwrap());
        let u64_at = |i: usize| u64::from_be_bytes(p[i..i + 8].try_into().unwrap());
        Ok(Self {
            kind: p[0],
            seq: u64_at(1),
            capture_us: u64_at(9),
            grab_us: u32_at(17),
            encode_us: u32_at(21),
            queue_us: u32_at(25),
        })
    }
}

/// High-bit tag of clock-probe PING nonces (`"OVT"` + 40-bit counter), so
/// keepalive and pacer PONGs are never mistaken for them.
pub const PROBE_TAG: u64 = 0x4F_56_54 << 40;
const PROBE_MASK: u64 = 0xFF_FF_FF << 40;

/// Whether a PING/PONG nonce is a clock probe's.
pub fn is_clock_probe(nonce: u64) -> bool {
    nonce & PROBE_MASK == PROBE_TAG
}

/// Clock probes sent quickly at the start (to converge fast)...
const PROBE_FAST: u32 = 8;
/// ...this far apart...
const PROBE_FAST_EVERY: Duration = Duration::from_millis(100);
/// ...then one per second (crystal drift stays far below a millisecond
/// over the 16-probe window).
const PROBE_EVERY: Duration = Duration::from_secs(1);
/// PINGs remembered for matching their PONG.
const PROBE_SENT_KEEP: usize = 16;

/// Clock-offset measurement over PING/PONG: paces dedicated probe PINGs,
/// remembers when every PING left, and turns timed PONGs into
/// [`ClockSync`] samples (peer clock − local clock).
#[derive(Debug, Clone)]
pub struct ClockProbe {
    sync: ClockSync,
    /// `(nonce, t1)` of recent PINGs.
    sent: VecDeque<(u64, u64)>,
    counter: u64,
    next_due: Option<Instant>,
}

impl ClockProbe {
    /// No probe sent, no sample yet.
    pub fn new() -> Self {
        // Pin the mono clock's anchor now: instants from before it would
        // all read as 0.
        let _ = ovcore::clocksync::mono_us();
        Self {
            sync: ClockSync::new(),
            sent: VecDeque::new(),
            counter: 0,
            next_due: None,
        }
    }

    /// If a clock probe is due at `now`, the nonce to send as a PING. The
    /// caller records the actual send with [`note_ping`](Self::note_ping).
    pub fn due(&mut self, now: Instant) -> Option<u64> {
        if self.next_due.is_some_and(|t| now < t) {
            return None;
        }
        self.counter += 1;
        let every = if self.counter <= u64::from(PROBE_FAST) {
            PROBE_FAST_EVERY
        } else {
            PROBE_EVERY
        };
        self.next_due = Some(now + every);
        Some(PROBE_TAG | (self.counter & !PROBE_MASK))
    }

    /// Time until the next probe is due (zero when due now).
    pub fn until_due(&self, now: Instant) -> Duration {
        self.next_due
            .map_or(Duration::ZERO, |t| t.saturating_duration_since(now))
    }

    /// Remember that PING `nonce` left at `t1_us` (local clock), so a timed
    /// PONG for it yields a sample — any PING, not just the probes.
    pub fn note_ping(&mut self, nonce: u64, t1_us: u64) {
        if self.sent.len() == PROBE_SENT_KEEP {
            self.sent.pop_front();
        }
        self.sent.push_back((nonce, t1_us));
    }

    /// A timed PONG for `nonce` (peer receive `t2`, peer send `t3`) arrived
    /// at local `t4`. Returns the sample when it matched a remembered PING.
    pub fn on_pong(&mut self, nonce: u64, t2: u64, t3: u64, t4: u64) -> Option<ClockSample> {
        let i = self.sent.iter().position(|&(n, _)| n == nonce)?;
        let (_, t1) = self.sent.remove(i)?;
        self.sync.add_probe(t1, t2, t3, t4)
    }

    /// The offset estimate.
    pub fn sync(&self) -> &ClockSync {
        &self.sync
    }
}

impl Default for ClockProbe {
    fn default() -> Self {
        Self::new()
    }
}

/// One measured stage of an update's path (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Host: present on the host → grab done.
    Capture = 0,
    /// Host: grab done → encode done.
    Encode = 1,
    /// Host: encode done → first byte handed to the transport.
    Send = 2,
    /// Host first byte out → update reassembled on the viewer (one-way,
    /// needs the clock offset).
    Network = 3,
    /// Viewer: reassembled → decoded (applied to the framebuffer).
    Decode = 4,
    /// Viewer: decoded → presented.
    Present = 5,
    /// Host capture → viewer present (needs the clock offset).
    Total = 6,
}

impl Stage {
    /// Every stage, in path order (`Total` last).
    pub const ALL: [Stage; 7] = [
        Stage::Capture,
        Stage::Encode,
        Stage::Send,
        Stage::Network,
        Stage::Decode,
        Stage::Present,
        Stage::Total,
    ];

    /// Short label (HUD / metrics).
    pub fn label(self) -> &'static str {
        match self {
            Stage::Capture => "cap",
            Stage::Encode => "enc",
            Stage::Send => "send",
            Stage::Network => "net",
            Stage::Decode => "dec",
            Stage::Present => "pres",
            Stage::Total => "total",
        }
    }
}

/// Number of [`Stage`]s.
pub const STAGES: usize = Stage::ALL.len();

/// The `q` quantile (0..=1) of an ascending slice by nearest rank.
pub fn quantile(sorted: &[u32], q: f64) -> Option<u32> {
    if sorted.is_empty() {
        return None;
    }
    let i = ((sorted.len() as f64 - 1.0) * q.clamp(0.0, 1.0)).round() as usize;
    Some(sorted[i.min(sorted.len() - 1)])
}

/// p50 / p95 of one stage, µs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StagePercentiles {
    /// Median.
    pub p50_us: u32,
    /// 95th percentile.
    pub p95_us: u32,
    /// Samples behind the numbers (0 = no data: both are 0).
    pub n: u32,
}

/// Rolling per-stage percentiles plus clock state, at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LatencySnapshot {
    /// Per stage, indexed by `Stage as usize`.
    pub stages: [StagePercentiles; STAGES],
    /// Updates measured since the session started.
    pub frames: u64,
    /// Clock offset (peer − local, µs), when known.
    pub offset_us: Option<i64>,
    /// RTT of the trusted clock sample, µs.
    pub clock_rtt_us: Option<u64>,
}

impl LatencySnapshot {
    /// One stage's percentiles.
    pub fn stage(&self, s: Stage) -> StagePercentiles {
        self.stages[s as usize]
    }

    /// The compact HUD line, e.g. `cap 3 · enc 4 · send 0 · net 12 · dec 2
    /// · pres 8 = 29 ms` (p50s). `None` before the first measured update;
    /// stages without data show `?`.
    pub fn hud_line(&self) -> Option<String> {
        if self.frames == 0 {
            return None;
        }
        let ms = |s: Stage| {
            let p = self.stage(s);
            if p.n == 0 {
                "?".to_owned()
            } else {
                fmt_ms(p.p50_us)
            }
        };
        let parts: Vec<String> = Stage::ALL[..STAGES - 1]
            .iter()
            .map(|&s| format!("{} {}", s.label(), ms(s)))
            .collect();
        Some(format!("{} = {} ms", parts.join(" · "), ms(Stage::Total)))
    }

    /// The `metrics:`-style fragment: `stage p50/p95` for every stage, ms.
    pub fn metrics_fragment(&self) -> String {
        let parts: Vec<String> = Stage::ALL
            .iter()
            .map(|&s| {
                let p = self.stage(s);
                format!(
                    "{} {:.1}/{:.1}",
                    s.label(),
                    f64::from(p.p50_us) / 1000.0,
                    f64::from(p.p95_us) / 1000.0
                )
            })
            .collect();
        format!("lat_ms p50/p95 {} n {}", parts.join(" "), self.frames)
    }
}

/// `µs` as milliseconds for the HUD: one decimal below 10 ms, whole above.
fn fmt_ms(us: u32) -> String {
    let ms = f64::from(us) / 1000.0;
    if ms < 9.95 {
        format!("{ms:.1}")
    } else {
        format!("{ms:.0}")
    }
}

/// A ring of the last `cap` samples.
#[derive(Debug, Clone)]
struct Ring {
    cap: usize,
    v: VecDeque<u32>,
}

impl Ring {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            v: VecDeque::with_capacity(cap),
        }
    }

    fn push(&mut self, x: u32) {
        if self.v.len() == self.cap {
            self.v.pop_front();
        }
        self.v.push_back(x);
    }

    fn percentiles(&self, scratch: &mut Vec<u32>) -> StagePercentiles {
        scratch.clear();
        scratch.extend(self.v.iter().copied());
        scratch.sort_unstable();
        StagePercentiles {
            p50_us: quantile(scratch, 0.5).unwrap_or(0),
            p95_us: quantile(scratch, 0.95).unwrap_or(0),
            n: scratch.len() as u32,
        }
    }
}

/// Rolling window per stage (updates; ≈ 2 s at 60 fps).
pub const ROLLING_WINDOW: usize = 120;
/// Updates awaiting their present at most (older ones are forgotten).
const MAX_PENDING: usize = 64;
/// Session-long samples kept per stage for end-of-session percentiles.
pub const MAX_SESSION_SAMPLES: usize = 100_000;

/// One update on its way to the screen.
#[derive(Debug, Clone)]
struct Entry {
    kind: u8,
    seq: u64,
    timing: Option<FrameTiming>,
    received: Option<Instant>,
    decoded: Option<Instant>,
}

/// Viewer-side join of host stamps and local reassembly / decode / present
/// times, with rolling and session-long per-stage percentiles.
#[derive(Debug, Clone)]
pub struct LatencyTracker {
    pending: VecDeque<Entry>,
    rolling: Vec<Ring>,
    session: Vec<Vec<u32>>,
    frames: u64,
    scratch: Vec<u32>,
}

impl LatencyTracker {
    /// Empty tracker.
    pub fn new() -> Self {
        let _ = ovcore::clocksync::mono_us(); // pin the clock anchor
        Self {
            pending: VecDeque::new(),
            rolling: (0..STAGES).map(|_| Ring::new(ROLLING_WINDOW)).collect(),
            session: vec![Vec::new(); STAGES],
            frames: 0,
            scratch: Vec::with_capacity(ROLLING_WINDOW),
        }
    }

    fn entry(&mut self, kind: u8, seq: u64) -> &mut Entry {
        if let Some(i) = self
            .pending
            .iter()
            .position(|e| e.kind == kind && e.seq == seq)
        {
            return &mut self.pending[i];
        }
        if self.pending.len() == MAX_PENDING {
            self.pending.pop_front();
        }
        self.pending.push_back(Entry {
            kind,
            seq,
            timing: None,
            received: None,
            decoded: None,
        });
        self.pending.back_mut().expect("just pushed")
    }

    /// The host's `TIMING` record for an update (usually just before it).
    pub fn on_timing(&mut self, t: FrameTiming) {
        self.entry(t.kind, t.seq).timing = Some(t);
    }

    /// Update `(kind, seq)` was fully reassembled at `at`.
    pub fn on_received(&mut self, kind: u8, seq: u64, at: Instant) {
        self.entry(kind, seq).received = Some(at);
    }

    /// Update `(kind, seq)` was decoded (applied) at `at`. Unknown updates
    /// (never received) are ignored.
    pub fn on_decoded(&mut self, kind: u8, seq: u64, at: Instant) {
        if let Some(e) = self
            .pending
            .iter_mut()
            .find(|e| e.kind == kind && e.seq == seq && e.received.is_some())
        {
            e.decoded.get_or_insert(at);
        }
    }

    /// Every received, not yet decoded update was decoded at `at` (a
    /// latest-frame-only consumer applies everything queued at once).
    pub fn on_decoded_all(&mut self, at: Instant) {
        for e in self.pending.iter_mut().filter(|e| e.received.is_some()) {
            e.decoded.get_or_insert(at);
        }
    }

    /// A present that drew everything decoded up to `drawn_before` reached
    /// the screen at `presented_at`: those updates become samples. `clock`
    /// converts the host stamps (`None` / empty: host-clock stages that
    /// need the offset are skipped). Returns the updates measured.
    pub fn on_presented(
        &mut self,
        drawn_before: Instant,
        presented_at: Instant,
        clock: Option<&ClockSync>,
    ) -> usize {
        let offset = clock.and_then(ClockSync::offset_us);
        let mut done = 0;
        let mut i = 0;
        while i < self.pending.len() {
            let ready = self.pending[i].decoded.is_some_and(|d| d <= drawn_before);
            if !ready {
                i += 1;
                continue;
            }
            let e = self.pending.remove(i).expect("index in range");
            self.measure(&e, presented_at, offset);
            done += 1;
        }
        done
    }

    fn measure(&mut self, e: &Entry, presented_at: Instant, offset: Option<i64>) {
        let (Some(rx), Some(dec)) = (e.received, e.decoded) else {
            return;
        };
        let span = |a: Instant, b: Instant| clamp_us(b.saturating_duration_since(a).as_micros());
        self.record(Stage::Decode, span(rx, dec));
        self.record(Stage::Present, span(dec, presented_at));
        if let Some(t) = e.timing {
            self.record(Stage::Capture, t.grab_us);
            self.record(Stage::Encode, t.encode_us);
            self.record(Stage::Send, t.queue_us);
            if let Some(off) = offset {
                let to_local = |host_us: u64| i128::from(host_us) - i128::from(off);
                let rx_us = i128::from(instant_us(rx));
                let pres_us = i128::from(instant_us(presented_at));
                let net = (rx_us - to_local(t.sent_us())).max(0);
                let total = (pres_us - to_local(t.capture_us)).max(0);
                self.record(Stage::Network, clamp_us(net as u128));
                self.record(Stage::Total, clamp_us(total as u128));
            }
        }
        self.frames += 1;
    }

    fn record(&mut self, s: Stage, us: u32) {
        self.rolling[s as usize].push(us);
        let all = &mut self.session[s as usize];
        if all.len() < MAX_SESSION_SAMPLES {
            all.push(us);
        }
    }

    /// Updates measured so far.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Rolling percentiles (last [`ROLLING_WINDOW`] updates per stage).
    pub fn snapshot(&mut self, clock: Option<&ClockSync>) -> LatencySnapshot {
        let mut out = LatencySnapshot {
            frames: self.frames,
            offset_us: clock.and_then(ClockSync::offset_us),
            clock_rtt_us: clock.and_then(ClockSync::rtt_us),
            ..LatencySnapshot::default()
        };
        for s in Stage::ALL {
            out.stages[s as usize] = self.rolling[s as usize].percentiles(&mut self.scratch);
        }
        out
    }

    /// Session-long percentile `q` of a stage, µs (`None`: no samples).
    pub fn session_quantile(&self, s: Stage, q: f64) -> Option<u32> {
        let mut v = self.session[s as usize].clone();
        v.sort_unstable();
        quantile(&v, q)
    }

    /// Session-long samples of a stage.
    pub fn session_len(&self, s: Stage) -> usize {
        self.session[s as usize].len()
    }

    /// Session-long `stage p50/p95` (ms) for every stage, plus the total's
    /// p99 and the updates measured — the end-of-session metrics fragment.
    pub fn session_summary(&self) -> String {
        let ms = |s: Stage, q: f64| {
            self.session_quantile(s, q)
                .map_or(0.0, |us| f64::from(us) / 1000.0)
        };
        let parts: Vec<String> = Stage::ALL
            .iter()
            .map(|&s| format!("{} {:.1}/{:.1}", s.label(), ms(s, 0.5), ms(s, 0.95)))
            .collect();
        format!(
            "lat_ms p50/p95 {} p99 {:.1} n {}",
            parts.join(" "),
            ms(Stage::Total, 0.99),
            self.frames
        )
    }
}

impl Default for LatencyTracker {
    fn default() -> Self {
        Self::new()
    }
}

fn clamp_us(us: u128) -> u32 {
    us.min(u128::from(u32::MAX)) as u32
}

#[cfg(test)]
mod tests;
