//! Reliable, ordered record delivery over UDP — a sans-IO state
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
//! bursts (at most `BURST` packets back to back, refilled at 2 x cwnd per
//! smoothed RTT); a packet is retransmitted when three later packets were
//! selectively acked (fast retransmit) or its RTO expires (RTO from a
//! Jacobson/Karels RTT estimate, exponential backoff).
//!
//! # Negotiation (backward compatible)
//!
//! A link starts in **raw mode**: records go out as bare datagrams (what a
//! peer without `CAP_RUDP` expects), and inbound datagrams without the magic are
//! delivered as-is. The framing is switched on per direction with
//! [`ReliableLink::enable`] once the peer's `CAPS` advertised `CAP_RUDP`, or
//! automatically when the first framed datagram arrives (the peer already
//! switched, so it understands our framing). An old peer never advertises
//! the bit and never sends framed datagrams, so it only ever sees raw
//! records. At the switch, the first framed packet carries how many raw
//! datagrams were sent before it (`raw_before`); the receiver holds framed
//! records back until that many raw ones were delivered (or
//! `RELEASE_TIMEOUT` passes), so records straddling the switch stay in
//! order.

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::time::{Duration, Instant};

/// Datagram magic of framed packets (never the start of a raw handshake
/// message; a raw record would need its sender id to match all 4 bytes).
pub(crate) const MAGIC: [u8; 4] = [0xB7, b'R', b'U', b'1'];
const T_DATA: u8 = 1;
const T_ACK: u8 = 2;
const F_FIRST: u8 = 1;
const F_LAST: u8 = 2;
/// DATA header bytes.
pub const DATA_HEADER: usize = 14;
/// ACK packet bytes.
pub(crate) const ACK_LEN: usize = 17;
/// Largest fragment payload: header + payload stays well under a 1280-byte
/// IPv6 minimum MTU (no IP fragmentation).
pub const MAX_PAYLOAD: usize = 1200;
/// Token-bucket depth: the longest back-to-back burst, in packets.
pub(crate) const BURST: f64 = 32.0;
/// How long framed records wait for raw records sent before the switch.
pub(crate) const RELEASE_TIMEOUT: Duration = Duration::from_millis(300);

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

/// Whether a datagram is a framed ACK (the direct path's pacer lets ACKs
/// bypass its queue).
pub(crate) fn is_ack(d: &[u8]) -> bool {
    d.len() >= ACK_LEN && d[..4] == MAGIC && d[4] == T_ACK
}

/// The packet sequence number of a framed DATA datagram.
pub(crate) fn data_seq(d: &[u8]) -> Option<u32> {
    (d.len() >= DATA_HEADER && d[..4] == MAGIC && d[4] == T_DATA)
        .then(|| u32::from_be_bytes([d[6], d[7], d[8], d[9]]))
}

/// Counters for logs and tests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LinkStats {
    /// DATA packets sent (first transmissions).
    pub data_sent: u64,
    /// DATA packets retransmitted (all causes).
    pub retransmits: u64,
    /// Retransmissions triggered by duplicate ACKs.
    pub fast_retransmits: u64,
    /// Retransmission timeouts.
    pub timeouts: u64,
    /// ACK packets sent.
    pub acks_sent: u64,
    /// ACK packets received.
    pub acks_rx: u64,
    /// Duplicate DATA packets received.
    pub dup_rx: u64,
    /// DATA packets received ahead of the next expected one.
    pub out_of_order_rx: u64,
    /// Records sent unframed (framing not enabled).
    pub raw_tx: u64,
    /// Unframed datagrams received (passed straight through).
    pub raw_rx: u64,
    /// Whole records reassembled and delivered.
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
    /// The window is set from outside (the direct path's delay-based
    /// controller, [`crate::cc`]); loss does not shrink it and the token
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

    /// Hand the congestion window to an external controller (call
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

    /// Counters.
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
mod tests;
