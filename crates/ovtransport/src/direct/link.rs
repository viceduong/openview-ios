//! One direct path's link state: the reliable-UDP link plus, with media
//! enabled, the v2 framing, congestion controller and pacer.

use std::io;
use std::time::{Duration, Instant};

use super::media_state::MediaTx;
use super::DirectIo;
use crate::cc::{FeedbackRx, Gcc, Lane, Pacer, SentHistory};
use crate::media;
use crate::rudp::{self, ReliableLink};

pub(super) struct Link {
    pub(super) path: u32,
    pub(super) rudp: ReliableLink,
    pub(super) last_rx: Instant,
    pub(super) last_tx: Instant,
    /// Viewer: the host acknowledged the COMMIT (sent us anything on the
    /// path); until then each keepalive repeats the COMMIT.
    pub(super) confirmed: bool,
    pub(super) last_dack: Instant,
    pub(super) dacked: u64,
    /// Viewer: when the last REBIND went out.
    pub(super) last_rebind: Option<Instant>,
    /// v2 framing + congestion control (media enabled).
    pub(super) cc: Option<LinkCc>,
}

/// A datagram waiting in the pacer: a rudp packet, or a media plaintext
/// (sealed when it leaves, so the AAD covers its final `tseq`).
pub(super) enum Paced {
    Rudp(Vec<u8>),
    Media(Vec<u8>),
}

impl Paced {
    pub(super) fn wire_len(&self) -> usize {
        match self {
            Paced::Rudp(p) => media::V2_HEADER + p.len(),
            Paced::Media(p) => media::MEDIA_OVERHEAD + p.len(),
        }
    }
}

/// Per-link v2 state: transport sequence numbers, the sent history, the
/// controller, the pacer and the receiver's feedback recorder.
pub(super) struct LinkCc {
    pub(super) gcc: Gcc,
    pub(super) pacer: Pacer<Paced>,
    pub(super) hist: SentHistory,
    pub(super) next_tseq: u32,
    pub(super) fb: FeedbackRx,
    pub(super) epoch: Instant,
    pub(super) datagrams_tx: u64,
    /// Highest rudp DATA seq handed to the pacer (retransmit detection).
    pub(super) rudp_top: Option<u32>,
}

impl LinkCc {
    pub(super) fn new(epoch: Instant) -> Self {
        Self {
            gcc: Gcc::new(),
            pacer: Pacer::new(),
            hist: SentHistory::new(),
            next_tseq: 0,
            fb: FeedbackRx::new(),
            epoch,
            datagrams_tx: 0,
            rudp_top: None,
        }
    }

    pub(super) fn us(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.epoch).as_micros() as u64
    }

    /// Frame and send one datagram now (no pacing). A `WouldBlock` leaves
    /// `tseq` unconsumed; a media plaintext always consumes a packet number.
    pub(super) fn emit(
        &mut self,
        tx: Option<&mut MediaTx>,
        now: Instant,
        path: u32,
        item: &Paced,
        io: &mut dyn DirectIo,
    ) -> io::Result<()> {
        let tseq = self.next_tseq;
        let send_us = self.us(now);
        let mut d = Vec::with_capacity(item.wire_len());
        d.push(media::V2_MAGIC);
        d.push(match item {
            Paced::Rudp(_) => media::KIND_RUDP,
            Paced::Media(_) => media::KIND_MEDIA,
        });
        d.extend_from_slice(&tseq.to_be_bytes());
        d.extend_from_slice(&(send_us as u32).to_be_bytes());
        match item {
            Paced::Rudp(p) => d.extend_from_slice(p),
            Paced::Media(p) => {
                let Some(tx) = tx else {
                    return Ok(()); // media disabled meanwhile: drop
                };
                let pn = tx.next_pn;
                tx.next_pn += 1;
                media::seal_into(&tx.key, pn, p, &mut d);
            }
        }
        let r = io.direct_send(path, &d);
        if !matches!(&r, Err(e) if e.kind() == io::ErrorKind::WouldBlock) {
            self.hist.record(tseq, send_us, d.len());
            self.next_tseq = tseq.wrapping_add(1);
            self.datagrams_tx += 1;
        }
        r
    }

    /// Send now and charge the pacer's budget (urgent datagrams: ACKs,
    /// audio, feedback, NACKs).
    pub(super) fn send_now(
        &mut self,
        tx: Option<&mut MediaTx>,
        now: Instant,
        path: u32,
        item: &Paced,
        io: &mut dyn DirectIo,
    ) -> io::Result<()> {
        self.pacer.charge(now, item.wire_len());
        self.emit(tx, now, path, item, io)
    }
}

/// The rudp `out` callback of a link: v1 = straight to the socket; v2 = ACKs
/// framed and sent at once, everything else into the pacer's reliable lane.
pub(super) fn rudp_out<'a, 'b: 'a>(
    cc: &'a mut Option<LinkCc>,
    io: &'a mut (dyn DirectIo + 'b),
    path: u32,
    now: Instant,
) -> impl FnMut(&[u8]) -> io::Result<()> + use<'a, 'b> {
    move |d: &[u8]| match cc.as_mut() {
        None => io.direct_send(path, d),
        Some(cc) => {
            if rudp::is_ack(d) {
                let _ = cc.send_now(None, now, path, &Paced::Rudp(d.to_vec()), io);
            } else {
                // A retransmission (seq at or below the highest sent) jumps
                // the reliable lane: everything after it in the ordered
                // stream - PONGs and input acks included - waits for it.
                let seq = rudp::data_seq(d);
                let retx =
                    seq.is_some_and(|s| cc.rudp_top.is_some_and(|t| t.wrapping_sub(s) < 1 << 31));
                if let Some(s) = seq.filter(|_| !retx) {
                    cc.rudp_top = Some(s);
                }
                let item = Paced::Rudp(d.to_vec());
                if retx {
                    cc.pacer.push_front(Lane::Reliable, item.wire_len(), item);
                } else {
                    cc.pacer.push(Lane::Reliable, item.wire_len(), item);
                }
            }
            Ok(())
        }
    }
}

/// Point the pacer and the governed rudp window at the controller's
/// current target.
pub(super) fn apply_rate(link: &mut Link) {
    let Some(cc) = link.cc.as_mut() else { return };
    let target = cc.gcc.target_bps();
    cc.pacer.set_rate(cc.gcc.pacing_bps());
    let srtt = link
        .rudp
        .srtt()
        .unwrap_or(Duration::from_millis(50))
        .max(Duration::from_millis(5));
    // Two BDPs of reliable data in flight (plus slack): the delay signal,
    // not the window, is what limits the rate.
    let cwnd = target / 8.0 * (srtt.as_secs_f64() * 2.0 + 0.010) / rudp::MAX_PAYLOAD as f64;
    // Floor: a full-screen tile update (~300 KB) plus the PONGs / input
    // acks behind it fit in flight; the pacer still spaces them, so this
    // never bursts, it only keeps a low estimate from serializing the
    // reliable lane behind one window per RTT.
    link.rudp.set_governed_cwnd(cwnd.max(GOVERNED_CWND_FLOOR));
}

/// Smallest governed rudp window (packets of <= 1200 B): ~300 KB.
const GOVERNED_CWND_FLOOR: f64 = 256.0;

pub(super) fn new_link(path: u32, now: Instant, confirmed: bool) -> Link {
    let mut rudp = ReliableLink::new();
    rudp.enable();
    Link {
        path,
        rudp,
        last_rx: now,
        last_tx: now,
        confirmed,
        last_dack: now,
        dacked: 0,
        last_rebind: None,
        cc: None,
    }
}
