//! Reliable, ordered record delivery over UDP (Phase 4) — a sans-IO state
//! machine shared by [`crate::UdpP2pCarrier`] and the iOS FFI datagram
//! carrier.
//!
//! # Why
//!
//! The record layer (`ovcore::session`) requires **in-order, lossless**
//! delivery: every record carries the next expected sequence number and a
//! gap desyncs the session for good. Raw UDP gave neither: a 400 KB frame
//! burst of 4 KiB datagrams (IP-fragmented, default ~64 KB socket buffers)
//! lost 75–97 % of a frame, and one lost record ended the session.
//!
//! # Wire format (inside one UDP datagram, all big-endian)
//!
//! ```text
//! DATA: magic "\xB7RU1" | 0x01 | flags | seq u32 | raw_before u32 | payload (<= 1200 B)
//! ACK:  magic "\xB7RU1" | 0x02 | cum u32 | sack u64
//! ```
//!
//! A record is cut into fragments of at most [`MAX_PAYLOAD`] bytes (flags
//! `FIRST`/`LAST`), so a datagram never exceeds ~1.2 KB and is never IP
//! fragmented. Each fragment gets a packet sequence number; the receiver
//! reassembles in sequence order and delivers whole records in order. ACKs
//! carry the cumulative next-expected sequence plus a 64-bit selective-ack
//! bitmap of the packets after it.
//!
//! Sender: a congestion window (slow start, then +1 packet per RTT; halved
//! on loss, once per window) bounds packets in flight; a token bucket paces
//! bursts (at most [`BURST`] packets back to back, refilled at 2 x cwnd per
//! smoothed RTT); a packet is retransmitted when three later packets were
//! selectively acked (fast retransmit) or its RTO expires (RTO from a
//! Jacobson/Karels RTT estimate, exponential backoff).
//!
//! # Negotiation (backward compatible)
//!
//! A link starts in **raw mode**: records go out as bare datagrams exactly
//! like before Phase 4, and inbound datagrams without the magic are
//! delivered as-is. The framing is switched on per direction with
//! [`ReliableLink::enable`] once the peer's `CAPS` advertised `CAP_RUDP`, or
//! automatically when the first framed datagram arrives (the peer already
//! switched, so it understands our framing). An old peer never advertises
//! the bit and never sends framed datagrams, so it only ever sees raw
//! records. At the switch, the first framed packet carries how many raw
//! datagrams were sent before it (`raw_before`); the receiver holds framed
//! records back until that many raw ones were delivered (or
//! [`RELEASE_TIMEOUT`] passes), so records straddling the switch stay in
//! order.

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::time::{Duration, Instant};

/// Datagram magic of framed packets (never the start of a raw handshake
/// message; a raw record would need its sender id to match all 4 bytes).
pub const MAGIC: [u8; 4] = [0xB7, b'R', b'U', b'1'];
const T_DATA: u8 = 1;
const T_ACK: u8 = 2;
const F_FIRST: u8 = 1;
const F_LAST: u8 = 2;
/// DATA header bytes.
pub const DATA_HEADER: usize = 14;
/// ACK packet bytes.
pub const ACK_LEN: usize = 17;
/// Largest fragment payload: header + payload stays well under a 1280-byte
/// IPv6 minimum MTU (no IP fragmentation).
pub const MAX_PAYLOAD: usize = 1200;
/// Token-bucket depth: the longest back-to-back burst, in packets.
pub const BURST: f64 = 32.0;
/// How long framed records wait for raw records sent before the switch.
pub const RELEASE_TIMEOUT: Duration = Duration::from_millis(300);

const INIT_CWND: f64 = 32.0;
const MIN_CWND: f64 = 4.0;
const MAX_CWND: f64 = 8192.0;
const RCV_WINDOW: u32 = 16384;
const DUP_THRESH: u32 = 3;
const INIT_RTO: Duration = Duration::from_millis(200);
const MIN_RTO: Duration = Duration::from_millis(20);
const MAX_RTO: Duration = Duration::from_secs(2);
/// ACK at least every this many data packets inside one receive batch.
const ACK_EVERY: u32 = 16;

/// Whether a datagram is a framed reliable-link packet.
pub fn is_framed(d: &[u8]) -> bool {
    d.len() >= 5 && d[..4] == MAGIC
}

/// Whether a datagram is a framed ACK (Wave 3: the direct path's pacer lets
/// ACKs bypass its queue).
pub fn is_ack(d: &[u8]) -> bool {
    d.len() >= ACK_LEN && d[..4] == MAGIC && d[4] == T_ACK
}

/// The packet sequence number of a framed DATA datagram.
pub fn data_seq(d: &[u8]) -> Option<u32> {
    (d.len() >= DATA_HEADER && d[..4] == MAGIC && d[4] == T_DATA)
        .then(|| u32::from_be_bytes([d[6], d[7], d[8], d[9]]))
}

/// Counters for logs and tests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LinkStats {
    pub data_sent: u64,
    pub retransmits: u64,
    pub fast_retransmits: u64,
    pub timeouts: u64,
    pub acks_sent: u64,
    pub acks_rx: u64,
    pub dup_rx: u64,
    pub out_of_order_rx: u64,
    pub raw_tx: u64,
    pub raw_rx: u64,
    pub records_rx: u64,
}

#[derive(Debug)]
struct Sent {
    pkt: Vec<u8>,
    sent_at: Instant,
    retx: u32,
}

/// The reliable-link state machine. Pure: the caller passes `now` and an
/// `out` callback that puts one datagram on the wire (returning a
/// `WouldBlock` error when the socket is full, which keeps the packet queued).
#[derive(Debug)]
pub struct ReliableLink {
    enabled: bool,
    auto_enable: bool,
    // ---- raw mode ----
    raw_queue: VecDeque<Vec<u8>>,
    raw_sent: u32,
    // ---- sender ----
    next_seq: u32,
    snd_una: u32,
    /// Index i = seq `snd_una + i`; `None` = selectively acked.
    inflight: VecDeque<Option<Sent>>,
    /// `Some` entries in `inflight` (packets on the wire, unacknowledged).
    outstanding: usize,
    queue: VecDeque<Vec<u8>>,
    queued_bytes: usize,
    cwnd: f64,
    ssthresh: f64,
    recovery_until: Option<u32>,
    srtt: Option<Duration>,
    rttvar: Duration,
    rto: Duration,
    tokens: f64,
    last_refill: Option<Instant>,
    highest_sacked: Option<u32>,
    /// Send time of the most recently sent packet known delivered (RACK).
    rack_sent_at: Option<Instant>,
    // ---- receiver ----
    rcv_next: u32,
    ooo: BTreeMap<u32, (u8, Vec<u8>)>,
    reasm: Vec<u8>,
    reasm_active: bool,
    ready: VecDeque<Vec<u8>>,
    raw_rx: u32,
    peer_raw_before: Option<u32>,
    first_framed_at: Option<Instant>,
    ack_due: bool,
    unacked_rx: u32,
    /// Wave 3: the window is set from outside (the direct path's delay-based
    /// controller, [`crate::cc`]); loss no longer shrinks it and the token
    /// bucket is off (the engine's pacer spaces the packets).
    governed: bool,
    stats: LinkStats,
}

impl Default for ReliableLink {
    fn default() -> Self {
        Self::new()
    }
}

type Out<'a> = &'a mut dyn FnMut(&[u8]) -> io::Result<()>;

impl ReliableLink {
    /// A link in raw mode that switches on the first framed datagram.
    pub fn new() -> Self {
        Self {
            enabled: false,
            auto_enable: true,
            raw_queue: VecDeque::new(),
            raw_sent: 0,
            next_seq: 0,
            snd_una: 0,
            inflight: VecDeque::new(),
            outstanding: 0,
            queue: VecDeque::new(),
            queued_bytes: 0,
            cwnd: INIT_CWND,
            ssthresh: MAX_CWND,
            recovery_until: None,
            srtt: None,
            rttvar: Duration::ZERO,
            rto: INIT_RTO,
            tokens: BURST,
            last_refill: None,
            highest_sacked: None,
            rack_sent_at: None,
            rcv_next: 0,
            ooo: BTreeMap::new(),
            reasm: Vec::new(),
            reasm_active: false,
            ready: VecDeque::new(),
            raw_rx: 0,
            peer_raw_before: None,
            first_framed_at: None,
            ack_due: false,
            unacked_rx: 0,
            governed: false,
            stats: LinkStats::default(),
        }
    }

    /// Wave 3: hand the congestion window to an external controller (call
    /// again whenever its estimate changes). In governed mode random loss
    /// is repaired (fast retransmit / RTO) without shrinking the window, so
    /// a lossy path keeps its throughput; the controller's delay signal is
    /// what backs off.
    pub fn set_governed_cwnd(&mut self, packets: f64) {
        self.governed = true;
        self.cwnd = packets.clamp(MIN_CWND, MAX_CWND);
    }

    /// Whether the window is externally governed.
    pub fn is_governed(&self) -> bool {
        self.governed
    }

    /// Switch our sending to the framed reliable mode (the peer advertised
    /// it can receive it). Idempotent.
    pub fn enable(&mut self) {
        self.enabled = true;
    }

    /// Whether we send framed packets.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Disable the automatic switch on the first framed datagram (tests).
    pub fn set_auto_enable(&mut self, on: bool) {
        self.auto_enable = on;
    }

    pub fn stats(&self) -> LinkStats {
        self.stats
    }

    /// Outbound bytes accepted but not yet on the wire (raw backlog plus
    /// fragments waiting for the window / pacer).
    pub fn pending_bytes(&self) -> usize {
        self.queued_bytes
    }

    /// Packets sent and not yet acknowledged.
    pub fn in_flight(&self) -> usize {
        self.outstanding
    }

    /// Current congestion window (packets).
    pub fn cwnd(&self) -> f64 {
        self.cwnd
    }

    /// Smoothed RTT, once measured.
    pub fn srtt(&self) -> Option<Duration> {
        self.srtt
    }

    /// Queue one record. Raw mode: sent as one bare datagram (queued on
    /// `WouldBlock`; other socket errors are returned). Framed mode:
    /// fragmented, sequenced and sent as the window allows.
    pub fn send(&mut self, record: &[u8], now: Instant, out: Out<'_>) -> io::Result<()> {
        if !self.enabled {
            self.raw_sent = self.raw_sent.wrapping_add(1);
            self.stats.raw_tx += 1;
            if self.raw_queue.is_empty() {
                match out(record) {
                    Ok(()) => return Ok(()),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e),
                }
            }
            self.queued_bytes += record.len();
            self.raw_queue.push_back(record.to_vec());
            return Ok(());
        }
        let mut chunks: Vec<&[u8]> = record.chunks(MAX_PAYLOAD).collect();
        if chunks.is_empty() {
            chunks.push(&[]);
        }
        let n = chunks.len();
        for (i, c) in chunks.into_iter().enumerate() {
            let mut flags = 0;
            if i == 0 {
                flags |= F_FIRST;
            }
            if i + 1 == n {
                flags |= F_LAST;
            }
            let mut pkt = Vec::with_capacity(DATA_HEADER + c.len());
            pkt.extend_from_slice(&MAGIC);
            pkt.push(T_DATA);
            pkt.push(flags);
            pkt.extend_from_slice(&self.next_seq.to_be_bytes());
            pkt.extend_from_slice(&self.raw_sent.to_be_bytes());
            pkt.extend_from_slice(c);
            self.next_seq = self.next_seq.wrapping_add(1);
            self.queued_bytes += c.len();
            self.queue.push_back(pkt);
        }
        self.drive(now, out);
        Ok(())
    }

    /// Feed one inbound datagram. Raw datagrams are delivered immediately;
    /// framed DATA is reassembled (whole records are released by
    /// [`end_batch`](Self::end_batch)); ACKs advance the sender.
    pub fn on_datagram(
        &mut self,
        d: &[u8],
        now: Instant,
        deliver: &mut dyn FnMut(&[u8]),
        out: Out<'_>,
    ) {
        if !is_framed(d) {
            self.raw_rx = self.raw_rx.wrapping_add(1);
            self.stats.raw_rx += 1;
            deliver(d);
            return;
        }
        if self.auto_enable && !self.enabled {
            self.enabled = true;
        }
        match d[4] {
            T_DATA if d.len() >= DATA_HEADER => {
                let flags = d[5];
                let seq = u32::from_be_bytes([d[6], d[7], d[8], d[9]]);
                let raw_before = u32::from_be_bytes([d[10], d[11], d[12], d[13]]);
                self.on_data(flags, seq, raw_before, &d[DATA_HEADER..], now, out);
            }
            T_ACK if d.len() >= ACK_LEN => {
                let cum = u32::from_be_bytes([d[5], d[6], d[7], d[8]]);
                let sack = u64::from_be_bytes(d[9..17].try_into().unwrap_or([0; 8]));
                self.on_ack(cum, sack, now, out);
            }
            _ => {} // malformed: ignore
        }
    }

    fn on_data(
        &mut self,
        flags: u8,
        seq: u32,
        raw_before: u32,
        payload: &[u8],
        now: Instant,
        out: Out<'_>,
    ) {
        self.peer_raw_before = Some(raw_before);
        self.first_framed_at.get_or_insert(now);
        self.unacked_rx += 1;
        let ahead = seq.wrapping_sub(self.rcv_next);
        if ahead >= RCV_WINDOW {
            // Behind us (a duplicate) or absurdly far ahead: re-ack.
            self.stats.dup_rx += 1;
            self.ack_due = true;
        } else if ahead == 0 {
            self.accept(flags, payload.to_vec());
            self.rcv_next = self.rcv_next.wrapping_add(1);
            while let Some((f, p)) = self.ooo.remove(&self.rcv_next) {
                self.accept(f, p);
                self.rcv_next = self.rcv_next.wrapping_add(1);
            }
            if !self.ooo.is_empty() {
                self.ack_due = true;
            }
        } else {
            self.stats.out_of_order_rx += 1;
            self.ooo
                .entry(seq)
                .or_insert_with(|| (flags, payload.to_vec()));
            // A hole: tell the sender now (fast retransmit).
            self.ack_due = true;
        }
        if self.unacked_rx >= ACK_EVERY {
            self.send_ack(out);
        }
    }

    fn accept(&mut self, flags: u8, payload: Vec<u8>) {
        if flags & F_FIRST != 0 {
            self.reasm.clear();
            self.reasm_active = true;
        }
        if !self.reasm_active {
            return; // a tail without its head: cannot happen in order
        }
        if flags & F_FIRST != 0 && flags & F_LAST != 0 {
            self.reasm = payload;
        } else {
            self.reasm.extend_from_slice(&payload);
        }
        if flags & F_LAST != 0 {
            self.reasm_active = false;
            self.ready.push_back(std::mem::take(&mut self.reasm));
        }
    }

    fn send_ack(&mut self, out: Out<'_>) {
        let mut sack = 0u64;
        let base = self.rcv_next.wrapping_add(1);
        for &s in self.ooo.keys() {
            let off = s.wrapping_sub(base);
            if off < 64 {
                sack |= 1 << off;
            }
        }
        let mut pkt = Vec::with_capacity(ACK_LEN);
        pkt.extend_from_slice(&MAGIC);
        pkt.push(T_ACK);
        pkt.extend_from_slice(&self.rcv_next.to_be_bytes());
        pkt.extend_from_slice(&sack.to_be_bytes());
        let _ = out(&pkt);
        self.stats.acks_sent += 1;
        self.ack_due = false;
        self.unacked_rx = 0;
    }

    fn on_ack(&mut self, cum: u32, sack: u64, now: Instant, out: Out<'_>) {
        self.stats.acks_rx += 1;
        let advance = cum.wrapping_sub(self.snd_una);
        let mut newly = 0u32;
        if advance > 0 && (advance as usize) <= self.inflight.len() {
            // RTT from the newest cumulatively acked, never-retransmitted packet.
            if let Some(Some(s)) = self.inflight.get(advance as usize - 1) {
                if s.retx == 0 {
                    self.rtt_sample(now.saturating_duration_since(s.sent_at));
                }
            }
            for _ in 0..advance {
                if let Some(Some(s)) = self.inflight.pop_front() {
                    newly += 1;
                    self.outstanding -= 1;
                    self.note_delivered(s.sent_at);
                }
            }
            self.snd_una = cum;
            // Fresh progress: undo RTO backoff.
            self.rto = self.computed_rto();
        }
        // Selective acks beyond `cum`.
        let base = cum.wrapping_add(1);
        for bit in 0..64u32 {
            if sack & (1 << bit) == 0 {
                continue;
            }
            let seq = base.wrapping_add(bit);
            let idx = seq.wrapping_sub(self.snd_una) as usize;
            if let Some(slot) = self.inflight.get_mut(idx) {
                if let Some(s) = slot.take() {
                    if s.retx == 0 {
                        self.rtt_sample(now.saturating_duration_since(s.sent_at));
                    }
                    newly += 1;
                    self.outstanding -= 1;
                    let at = s.sent_at;
                    self.note_delivered(at);
                }
            }
            let ahead = self
                .highest_sacked
                .is_none_or(|h| seq.wrapping_sub(h) < RCV_WINDOW && seq != h);
            if ahead {
                self.highest_sacked = Some(seq);
            }
        }
        // Window growth (Reno; an external controller owns it when governed).
        if newly > 0 && !self.governed {
            if self.cwnd < self.ssthresh {
                self.cwnd += f64::from(newly);
            } else {
                self.cwnd += f64::from(newly) / self.cwnd;
            }
            self.cwnd = self.cwnd.min(MAX_CWND);
        }
        self.fast_retransmit(now, out);
        self.drive(now, out);
    }

    fn note_delivered(&mut self, sent_at: Instant) {
        if self.rack_sent_at.is_none_or(|t| sent_at > t) {
            self.rack_sent_at = Some(sent_at);
        }
    }

    fn rtt_sample(&mut self, r: Duration) {
        match self.srtt {
            None => {
                self.srtt = Some(r);
                self.rttvar = r / 2;
            }
            Some(s) => {
                let diff = s.abs_diff(r);
                self.rttvar = (self.rttvar * 3 + diff) / 4;
                self.srtt = Some((s * 7 + r) / 8);
            }
        }
        self.rto = self.computed_rto();
    }

    fn computed_rto(&self) -> Duration {
        match self.srtt {
            Some(s) => (s + self.rttvar * 4 + Duration::from_millis(5)).clamp(MIN_RTO, MAX_RTO),
            None => INIT_RTO,
        }
    }

    fn on_loss(&mut self) {
        if self.governed {
            return;
        }
        let in_recovery = self.recovery_until.is_some_and(|r| {
            r.wrapping_sub(self.snd_una) < RCV_WINDOW * 4 && self.snd_una != r.wrapping_add(1)
        });
        if !in_recovery {
            self.ssthresh = (self.cwnd / 2.0).max(MIN_CWND);
            self.cwnd = self.ssthresh;
            self.recovery_until = Some(self.next_seq.wrapping_sub(1));
        }
    }

    /// Loss detection on an ACK: a packet is lost when [`DUP_THRESH`] later
    /// packets were selectively acked, or (RACK, time-based — catches tail
    /// losses without waiting for an RTO) when a packet sent more than a
    /// reordering window after it was already delivered.
    fn fast_retransmit(&mut self, now: Instant, out: Out<'_>) {
        let srtt = self.srtt.unwrap_or(MIN_RTO);
        let min_gap = srtt;
        let reorder = (srtt / 4).max(Duration::from_millis(1));
        let limit = self
            .highest_sacked
            .map(|h| h.wrapping_sub(self.snd_una))
            .filter(|&l| (l as usize) <= self.inflight.len())
            .unwrap_or(0);
        let rack = self.rack_sent_at;
        let mut lost = false;
        for i in 0..self.inflight.len() {
            let dup_lost = (i as u32).saturating_add(DUP_THRESH) <= limit;
            if let Some(s) = self.inflight[i].as_mut() {
                let time_lost = rack.is_some_and(|t| t > s.sent_at + reorder);
                if !(dup_lost || time_lost) {
                    continue;
                }
                if now.saturating_duration_since(s.sent_at) < min_gap {
                    continue; // already retransmitted within the last RTT
                }
                if out(&s.pkt).is_ok() {
                    s.sent_at = now;
                    s.retx += 1;
                    self.stats.retransmits += 1;
                    self.stats.fast_retransmits += 1;
                    lost = true;
                }
            }
        }
        if lost {
            self.on_loss();
        }
    }

    /// Timers: retransmit packets whose RTO expired (with backoff), release
    /// framed records held for the switch-over, and send what the window
    /// allows. Call on every send/poll.
    pub fn poll(&mut self, now: Instant, deliver: &mut dyn FnMut(&[u8]), out: Out<'_>) {
        let rto = self.rto;
        let mut expired = 0;
        for slot in self.inflight.iter_mut() {
            let Some(s) = slot.as_mut() else { continue };
            if now.saturating_duration_since(s.sent_at) < rto {
                continue;
            }
            if out(&s.pkt).is_err() {
                break;
            }
            s.sent_at = now;
            s.retx += 1;
            expired += 1;
            if expired >= BURST as usize && !self.governed {
                break;
            }
        }
        if expired > 0 {
            self.stats.retransmits += expired as u64;
            self.stats.timeouts += 1;
            if !self.governed {
                self.ssthresh = (self.cwnd / 2.0).max(MIN_CWND);
                self.cwnd = MIN_CWND.max(self.cwnd / 4.0);
                self.recovery_until = Some(self.next_seq.wrapping_sub(1));
            }
            // Back off, but never beyond 4x the measured RTO.
            self.rto = (self.rto * 2).min(MAX_RTO).min(self.computed_rto() * 4);
        }
        self.release(now, deliver);
        self.drive(now, out);
    }

    /// Finish a receive batch: send the owed ACK, release whole records in
    /// order, and run the timers.
    pub fn end_batch(&mut self, now: Instant, deliver: &mut dyn FnMut(&[u8]), out: Out<'_>) {
        if self.ack_due || self.unacked_rx > 0 {
            self.send_ack(out);
        }
        self.poll(now, deliver, out);
    }

    fn release(&mut self, now: Instant, deliver: &mut dyn FnMut(&[u8])) {
        if self.ready.is_empty() {
            return;
        }
        let caught_up = self
            .peer_raw_before
            .is_none_or(|n| self.raw_rx.wrapping_sub(n) < RCV_WINDOW * 4);
        let timed_out = self
            .first_framed_at
            .is_some_and(|t| now.saturating_duration_since(t) >= RELEASE_TIMEOUT);
        if !(caught_up || timed_out) {
            return;
        }
        while let Some(r) = self.ready.pop_front() {
            self.stats.records_rx += 1;
            deliver(&r);
        }
    }

    /// Send queued raw datagrams, then framed packets within the window and
    /// the pacing budget.
    fn drive(&mut self, now: Instant, out: Out<'_>) {
        while let Some(d) = self.raw_queue.front() {
            match out(d) {
                Ok(()) => {
                    self.queued_bytes = self.queued_bytes.saturating_sub(d.len());
                    self.raw_queue.pop_front();
                }
                Err(_) => return,
            }
        }
        // Token bucket: 2 x cwnd per smoothed RTT, BURST deep.
        let srtt = self
            .srtt
            .unwrap_or(Duration::from_millis(50))
            .max(Duration::from_micros(200));
        if let Some(last) = self.last_refill {
            let rate = 2.0 * self.cwnd / srtt.as_secs_f64();
            self.tokens =
                (self.tokens + rate * now.saturating_duration_since(last).as_secs_f64()).min(BURST);
        }
        self.last_refill = Some(now);
        while let Some(pkt) = self.queue.front() {
            // The window counts packets actually outstanding (selectively
            // acked holes do not block new data — that keeps the ACK clock
            // and time-based loss detection running), and the span from
            // the oldest unacked packet stays within the receive window.
            if self.outstanding as f64 >= self.cwnd
                || self.inflight.len() as u32 >= RCV_WINDOW - 1
                || (!self.governed && self.tokens < 1.0)
            {
                break;
            }
            match out(pkt) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => {} // a send error is a loss: the RTO recovers it
            }
            let pkt = self.queue.pop_front().unwrap_or_default();
            self.queued_bytes = self
                .queued_bytes
                .saturating_sub(pkt.len().saturating_sub(DATA_HEADER));
            if !self.governed {
                self.tokens -= 1.0;
            }
            self.stats.data_sent += 1;
            self.outstanding += 1;
            self.inflight.push_back(Some(Sent {
                pkt,
                sent_at: now,
                retx: 0,
            }));
        }
        // Drop acked slots at the front (kept as `None` for indexing).
        while matches!(self.inflight.front(), Some(None)) {
            self.inflight.pop_front();
            self.snd_una = self.snd_una.wrapping_add(1);
        }
    }

    /// When the link next needs `poll` (an RTO, queued data waiting for
    /// pacing tokens, or held records): callers bound their waits by it.
    pub fn next_deadline(&self, now: Instant) -> Option<Duration> {
        let mut d: Option<Duration> = None;
        let mut take = |x: Duration| d = Some(d.map_or(x, |y| y.min(x)));
        if let Some(s) = self.inflight.iter().flatten().map(|s| s.sent_at).min() {
            take((s + self.rto).saturating_duration_since(now));
        }
        if !self.queue.is_empty() && (self.outstanding as f64) < self.cwnd {
            take(Duration::from_millis(1));
        }
        if !self.ready.is_empty() {
            if let Some(t) = self.first_framed_at {
                take((t + RELEASE_TIMEOUT).saturating_duration_since(now));
            }
        }
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// xorshift for deterministic loss/reorder.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn chance(&mut self, pct: u64) -> bool {
            self.next() % 100 < pct
        }
    }

    /// A lossy, reordering, delaying datagram path between two links.
    struct Net {
        rng: Rng,
        loss: u64,
        reorder: u64,
        delay: Duration,
        /// (arrival, to_b, datagram)
        q: Vec<(Instant, bool, Vec<u8>)>,
        sent: u64,
        dropped: u64,
    }

    impl Net {
        fn push(&mut self, now: Instant, to_b: bool, d: &[u8]) {
            self.sent += 1;
            if self.rng.chance(self.loss) {
                self.dropped += 1;
                return;
            }
            let mut at = now + self.delay;
            if self.rng.chance(self.reorder) {
                at += Duration::from_millis(1 + self.rng.next() % 8);
            }
            self.q.push((at, to_b, d.to_vec()));
        }
        fn due(&mut self, now: Instant) -> Vec<(bool, Vec<u8>)> {
            self.q.sort_by_key(|e| e.0);
            let n = self.q.iter().take_while(|e| e.0 <= now).count();
            self.q.drain(..n).map(|(_, b, d)| (b, d)).collect()
        }
    }

    fn records(n: usize, seed: u64) -> Vec<Vec<u8>> {
        let mut r = Rng(seed);
        (0..n)
            .map(|i| {
                let len = match i % 5 {
                    0 => 9,
                    1 => 4096 + 29,
                    2 => 0,
                    3 => (r.next() % 20_000) as usize,
                    _ => 1200,
                };
                (0..len).map(|j| (i * 31 + j) as u8).collect()
            })
            .collect()
    }

    /// Push `recs` A -> B over `net` (both links framed from the start) and
    /// return what B delivered plus A's stats.
    fn transfer(recs: &[Vec<u8>], mut net: Net) -> (Vec<Vec<u8>>, LinkStats, Net) {
        let mut a = ReliableLink::new();
        let mut b = ReliableLink::new();
        a.enable();
        b.enable();
        let t0 = Instant::now();
        let mut now = t0;
        let mut got: Vec<Vec<u8>> = Vec::new();
        let mut next = 0;
        while got.len() < recs.len() {
            assert!(
                now - t0 < Duration::from_secs(120),
                "stalled at {}/{}",
                got.len(),
                recs.len()
            );
            // App: keep A's queue fed (a few records per tick).
            while next < recs.len() && a.pending_bytes() < 64 * 1024 {
                let mut o = |d: &[u8]| {
                    net.push(now, true, d);
                    Ok(())
                };
                a.send(&recs[next], now, &mut o).unwrap();
                next += 1;
            }
            for (to_b, d) in net.due(now) {
                let mut sent = Vec::new();
                let mut o = |p: &[u8]| {
                    sent.push(p.to_vec());
                    Ok(())
                };
                if to_b {
                    b.on_datagram(&d, now, &mut |r| got.push(r.to_vec()), &mut o);
                } else {
                    a.on_datagram(&d, now, &mut |_| panic!("B sends no records"), &mut o);
                }
                for p in sent {
                    net.push(now, !to_b, &p);
                }
            }
            let mut sent_b = Vec::new();
            b.end_batch(now, &mut |r| got.push(r.to_vec()), &mut |p| {
                sent_b.push(p.to_vec());
                Ok(())
            });
            for p in sent_b {
                net.push(now, false, &p);
            }
            let mut sent_a = Vec::new();
            a.poll(now, &mut |_| {}, &mut |p| {
                sent_a.push(p.to_vec());
                Ok(())
            });
            for p in sent_a {
                net.push(now, true, &p);
            }
            now += Duration::from_micros(500);
        }
        (got, a.stats(), net)
    }

    fn net(loss: u64, reorder: u64, seed: u64) -> Net {
        Net {
            rng: Rng(seed),
            loss,
            reorder,
            delay: Duration::from_millis(5),
            q: Vec::new(),
            sent: 0,
            dropped: 0,
        }
    }

    #[test]
    fn lossless_path_delivers_in_order_with_small_datagrams() {
        let recs = records(200, 7);
        let (got, st, n) = transfer(&recs, net(0, 0, 1));
        assert_eq!(got, recs);
        assert_eq!(st.retransmits, 0);
        assert!(n.sent > 0);
    }

    #[test]
    fn recovers_from_loss_and_reordering_in_order() {
        for (loss, reorder, seed) in [(5, 0, 3), (10, 20, 11), (30, 30, 99)] {
            let recs = records(300, seed);
            let (got, st, n) = transfer(&recs, net(loss, reorder, seed));
            assert_eq!(got.len(), recs.len());
            assert!(
                got == recs,
                "loss {loss}% reorder {reorder}%: records corrupted or reordered"
            );
            assert!(st.retransmits > 0, "loss {loss}%: nothing retransmitted?");
            eprintln!(
                "loss {loss}% reorder {reorder}%: {} datagrams, {} dropped, {} retx ({} fast, {} RTO)",
                n.sent, n.dropped, st.retransmits, st.fast_retransmits, st.timeouts
            );
        }
    }

    #[test]
    fn datagrams_never_exceed_the_mtu_budget() {
        let mut a = ReliableLink::new();
        a.enable();
        let mut max = 0;
        let now = Instant::now();
        a.send(&vec![1u8; 65_000], now, &mut |d| {
            max = max.max(d.len());
            Ok(())
        })
        .unwrap();
        assert!(max <= DATA_HEADER + MAX_PAYLOAD, "{max}");
        // 55 fragments, the window lets INIT_CWND go now, the bucket BURST.
        assert!(a.in_flight() as f64 <= INIT_CWND.min(BURST));
        assert!(a.pending_bytes() > 0, "the rest waits for acks");
    }

    #[test]
    fn raw_mode_is_byte_identical_to_plain_udp_and_switches_on_framed_rx() {
        let now = Instant::now();
        let mut a = ReliableLink::new();
        let mut wire = Vec::new();
        a.send(b"hello", now, &mut |d| {
            wire.push(d.to_vec());
            Ok(())
        })
        .unwrap();
        assert_eq!(
            wire,
            vec![b"hello".to_vec()],
            "raw mode sends the bare record"
        );
        // An old peer's raw datagram is delivered as-is.
        let mut got = Vec::new();
        a.on_datagram(
            b"\x01legacy",
            now,
            &mut |r| got.push(r.to_vec()),
            &mut |_| Ok(()),
        );
        assert_eq!(got, vec![b"\x01legacy".to_vec()]);
        assert!(!a.is_enabled());
        // A framed ACK from a new peer flips us to framed sending.
        let mut ack = MAGIC.to_vec();
        ack.push(T_ACK);
        ack.extend_from_slice(&[0; 12]);
        a.on_datagram(&ack, now, &mut |_| {}, &mut |_| Ok(()));
        assert!(a.is_enabled());
    }

    #[test]
    fn framed_records_wait_for_raw_records_sent_before_the_switch() {
        let now = Instant::now();
        let mut a = ReliableLink::new();
        let mut wire = Vec::new();
        let mut o = |d: &[u8]| {
            wire.push(d.to_vec());
            Ok(())
        };
        a.send(b"r0", now, &mut o).unwrap();
        a.send(b"r1", now, &mut o).unwrap();
        a.enable();
        a.send(b"f2", now, &mut o).unwrap();
        // B receives the framed one first, then the raw ones (reordered).
        let mut b = ReliableLink::new();
        let mut got = Vec::new();
        b.on_datagram(
            &wire[2],
            now,
            &mut |r| got.push(r.to_vec()),
            &mut |_| Ok(()),
        );
        b.end_batch(now, &mut |r| got.push(r.to_vec()), &mut |_| Ok(()));
        assert!(got.is_empty(), "held until both raw records arrived");
        b.on_datagram(
            &wire[0],
            now,
            &mut |r| got.push(r.to_vec()),
            &mut |_| Ok(()),
        );
        b.on_datagram(
            &wire[1],
            now,
            &mut |r| got.push(r.to_vec()),
            &mut |_| Ok(()),
        );
        b.end_batch(now, &mut |r| got.push(r.to_vec()), &mut |_| Ok(()));
        assert_eq!(got, vec![b"r0".to_vec(), b"r1".to_vec(), b"f2".to_vec()]);
    }

    #[test]
    fn would_block_keeps_packets_queued() {
        let now = Instant::now();
        let mut a = ReliableLink::new();
        a.enable();
        a.send(&[7u8; 3000], now, &mut |_| {
            Err(io::ErrorKind::WouldBlock.into())
        })
        .unwrap();
        assert_eq!(a.in_flight(), 0);
        assert_eq!(a.pending_bytes(), 3000);
        let mut n = 0;
        a.poll(now, &mut |_| {}, &mut |_| {
            n += 1;
            Ok(())
        });
        assert_eq!(n, 3);
        assert_eq!(a.pending_bytes(), 0);
    }
}
