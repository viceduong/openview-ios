//! Direct-path upgrade of a relayed session (Section 5 "direct connection").
//!
//! A session always starts on the relay (the handshake runs there). Once both
//! peers advertised `CAP_DIRECT`, the host sends its candidate addresses
//! (LAN IPv4s, the UPnP-mapped public address, the STUN reflexive address,
//! global IPv6) in a `DIRECT_CANDIDATES` command and the viewer probes all of
//! them in parallel. The first candidate that answers an authenticated probe
//! wins: the viewer COMMITs to it and both sides move their *sending* to a
//! reliable-UDP link ([`crate::rudp::ReliableLink`]) on that path. The relay
//! stays connected as the fallback (with a keepalive); a direct path that
//! goes silent for [`DEAD_AFTER`] is dropped, unacknowledged records are
//! replayed on the relay, and the viewer re-probes later.
//!
//! # Ordering across paths (the record layer needs lossless, in-order)
//!
//! Every record a sender pushes through the engine gets an index (its own
//! counter, starting at 0 when the engine is created). Records on the
//! direct path carry it explicitly; records on the relay carry it
//! implicitly: a relay **SYNC(n)** marker says "the next relay record has
//! index n, and every record below n was already sent (on some path)". The
//! receiver delivers strictly in index order, holding early records and
//! dropping duplicates; before the first SYNC (the peer never left the
//! relay) relay records pass straight through, exactly as without the
//! engine. Promotion sends `SYNC_UP(next)` on the relay and then data on
//! the direct link; demotion sends `SYNC_DOWN(first unacked)` and replays
//! the records the peer has not acknowledged (`DACK`) on the relay — so a
//! path switch never loses, duplicates or reorders a record and the session
//! keys / sequence numbers carry over untouched (the record AEAD is
//! path-agnostic).
//!
//! # Wire formats
//!
//! ```text
//! probe (raw UDP):  "\xB7DP1" | type u8 (1 REQ, 2 RESP, 3 COMMIT) | nonce [16]
//!                   | t_us u64 BE | mac [16]   (HMAC-SHA256(key, all before)[..16])
//! relay marker:     FF FF FF FF FF FF FF FF "OVDX" | kind u8 | n u64 BE
//!                   (kind 1 SYNC_UP, 3 SYNC_DOWN, 2 NOP keepalive)
//! direct record:    rudp-framed; inside: 0x01 | idx u64 BE | record
//!                                     or 0x02 | delivered u64 BE  (DACK / keepalive)
//! ```
//!
//! The probe key is derived from both session AEAD keys
//! ([`derive_direct_key`]), so only the two session peers can produce or
//! validate a probe: a candidate answering is proof it is the session peer.
//! A relay marker can never be mistaken for a record: record headers start
//! with the 8-byte LE sender id, whose top bytes are zero for every OpenView
//! device id.
//!
//! The engine is sans-IO ([`DirectEngine`] + a [`DirectIo`] the caller
//! implements): [`DirectCarrier`] drives it over real UDP sockets for the
//! desktop shells, and the iOS FFI drives it with Swift-owned
//! `NWConnection`s (one numbered path per candidate).
//!
//! # Wave 3 (B2): media channel + delay-based congestion control
//!
//! Once both peers advertised `CAP_MEDIA` the application calls
//! [`DirectEngine::enable_media`] and every datagram on the direct link uses
//! the v2 framing of [`crate::media`]: a transport header (`tseq`,
//! `send_ts`) in front of the unchanged rudp packets, plus authenticated
//! media datagrams (video shards with Reed-Solomon FEC, Opus packets,
//! congestion feedback, NACKs). The link's [`crate::cc::Gcc`] sets the rate:
//! a [`crate::cc::Pacer`] spaces all non-urgent datagrams (reliable lane
//! first, then video), the rudp window is governed by it instead of Reno,
//! and the application reads the target ([`DirectEngine::cc_target`]) for
//! its encoder bitrate. Probes, relay markers and the relay are unchanged.

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use crate::carrier::{Carrier, CarrierStats, RawSock, TransportError};
use crate::cc::{CcStats, FeedbackRx, Gcc, Lane, Pacer, PacketReport, SentHistory};
use crate::media::{self, MediaEvent, MediaKeys, MediaRx, MediaRxStats, ReplayWindow};
use crate::rudp::{self, ReliableLink};

/// Probe datagram magic.
pub const PROBE_MAGIC: [u8; 4] = [0xB7, b'D', b'P', b'1'];
const P_REQ: u8 = 1;
const P_RESP: u8 = 2;
const P_COMMIT: u8 = 3;
/// Viewer -> host: "I am still here, on this path" — sent while the link is
/// silent. A host that receives it from a *new* source address moves the
/// link there (NAT / VPN rebinding: the viewer's mapping changed, so the
/// host's sends to the old address vanish). Authenticated like every probe;
/// old hosts ignore the type.
const P_REBIND: u8 = 4;
/// Viewer: send REBIND once the link was silent this long ...
pub const REBIND_AFTER: Duration = Duration::from_millis(1000);
/// ... and repeat it this often while it stays silent.
pub const REBIND_INTERVAL: Duration = Duration::from_millis(500);
/// Probe datagram length.
pub const PROBE_LEN: usize = 4 + 1 + 16 + 8 + 16;

/// Relay marker prefix (an impossible record header, see module docs).
pub const MARK_PREFIX: [u8; 12] = [
    0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, b'O', b'V', b'D', b'X',
];
const M_SYNC_UP: u8 = 1;
const M_NOP: u8 = 2;
const M_SYNC_DOWN: u8 = 3;
/// Relay marker length.
pub const MARK_LEN: usize = 12 + 1 + 8;

const K_DATA: u8 = 1;
const K_DACK: u8 = 2;

/// Probe retransmit interval within a round.
pub const PROBE_INTERVAL: Duration = Duration::from_millis(100);
/// How long one probe round lasts before giving up (until the re-probe).
pub const PROBE_ROUND: Duration = Duration::from_secs(4);
/// First re-probe delay after a failed round or a demotion (doubles to
/// [`REPROBE_MAX`]).
pub const REPROBE_AFTER: Duration = Duration::from_secs(10);
/// Longest re-probe delay.
pub const REPROBE_MAX: Duration = Duration::from_secs(120);
/// Direct-link keepalive (DACK) interval when idle.
pub const KEEPALIVE: Duration = Duration::from_millis(250);
/// A direct path silent this long is dead: demote to the relay.
pub const DEAD_AFTER: Duration = Duration::from_secs(3);
/// Relay keepalive (NOP marker) interval while sending on the direct path,
/// so the idle relay tunnel stays open as the fallback.
pub const PRIMARY_KEEPALIVE: Duration = Duration::from_secs(5);
/// Once the relay is gone there is no fallback to demote to (and no relay
/// to re-promote over): a silent direct path then gets this long to recover
/// (Wi-Fi power save, a roaming hiccup) before the session is given up.
pub const DEAD_AFTER_RELAYLESS: Duration = Duration::from_secs(15);
/// Send a DACK at least this often while records arrive.
const DACK_INTERVAL: Duration = Duration::from_millis(50);
const DACK_EVERY: u64 = 64;
/// Replay buffer cap: beyond it the oldest records are forgotten (a
/// demotion after that would desync the session; the link would have to
/// be stalled for many seconds of full-rate video first).
pub const REPLAY_LIMIT: usize = 64 << 20;
/// Most out-of-order records held while waiting for the other path.
const PENDING_LIMIT: usize = 65_536;

/// Where the session's sending currently goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DirectState {
    /// Relay only (no candidates yet, or probing failed).
    Relay = 0,
    /// Probing candidates (still sending on the relay).
    Probing = 1,
    /// Sending on the direct path.
    Direct = 2,
}

/// The probe/commit key: HKDF over both session AEAD keys (client's
/// encrypt key first), so both peers derive the same value.
pub fn derive_direct_key(client_key: &[u8; 32], server_key: &[u8; 32]) -> [u8; 32] {
    let mut ikm = [0u8; 64];
    ikm[..32].copy_from_slice(client_key);
    ikm[32..].copy_from_slice(server_key);
    let k = ovcore::crypto::hkdf_sha256(b"ov-direct-v1", &ikm, b"probe", 32);
    let mut out = [0u8; 32];
    out.copy_from_slice(&k);
    out
}

/// The probe key for a session (orders the keys by role).
pub fn session_direct_key(session: &ovcore::session::Session) -> [u8; 32] {
    let k = session.keys();
    if session.role_is_server() {
        derive_direct_key(k.decrypt_key(), k.encrypt_key())
    } else {
        derive_direct_key(k.encrypt_key(), k.decrypt_key())
    }
}

/// Whether a datagram is a direct-path probe.
pub fn is_probe(d: &[u8]) -> bool {
    d.len() == PROBE_LEN && d[..4] == PROBE_MAGIC
}

/// Whether a relay datagram is an engine marker (not a record).
pub fn is_marker(d: &[u8]) -> bool {
    d.len() == MARK_LEN && d[..12] == MARK_PREFIX
}

fn marker(kind: u8, n: u64) -> [u8; MARK_LEN] {
    let mut m = [0u8; MARK_LEN];
    m[..12].copy_from_slice(&MARK_PREFIX);
    m[12] = kind;
    m[13..].copy_from_slice(&n.to_be_bytes());
    m
}

fn probe(key: &[u8; 32], ty: u8, nonce: &[u8; 16], t_us: u64) -> [u8; PROBE_LEN] {
    let mut p = [0u8; PROBE_LEN];
    p[..4].copy_from_slice(&PROBE_MAGIC);
    p[4] = ty;
    p[5..21].copy_from_slice(nonce);
    p[21..29].copy_from_slice(&t_us.to_be_bytes());
    let mac = ovcore::crypto::hmac_sha256(key, &p[..29]);
    p[29..].copy_from_slice(&mac[..16]);
    p
}

/// Validate a probe; returns `(type, nonce, t_us)`.
fn open_probe(key: &[u8; 32], d: &[u8]) -> Option<(u8, [u8; 16], u64)> {
    if !is_probe(d) {
        return None;
    }
    let mac = ovcore::crypto::hmac_sha256(key, &d[..29]);
    // Constant-time compare of the truncated tag.
    let diff = mac[..16]
        .iter()
        .zip(&d[29..])
        .fold(0u8, |a, (x, y)| a | (x ^ y));
    if diff != 0 {
        return None;
    }
    let mut nonce = [0u8; 16];
    nonce.copy_from_slice(&d[5..21]);
    Some((d[4], nonce, u64::from_be_bytes(d[21..29].try_into().ok()?)))
}

/// The I/O the engine needs: the primary (relay) carrier and per-path
/// datagram sends.
pub trait DirectIo {
    /// Send one datagram on the primary (relay) path.
    fn primary_send(&mut self, d: &[u8]) -> Result<(), TransportError>;
    /// Send one datagram on direct path `path`.
    fn direct_send(&mut self, path: u32, d: &[u8]) -> io::Result<()>;
}

/// Counters for logs, the HUD and tests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DirectStats {
    pub promotions: u64,
    pub demotions: u64,
    pub probes_sent: u64,
    pub probe_rounds: u64,
    pub probe_responses: u64,
    pub bad_probes: u64,
    pub records_direct_tx: u64,
    pub records_direct_rx: u64,
    pub records_replayed: u64,
    pub duplicates_dropped: u64,
    /// RTT of the winning probe (request -> response), microseconds.
    pub probe_rtt_us: u64,
    /// REBINDs sent (viewer) / link moves after one (host).
    pub rebinds: u64,
}

struct Link {
    path: u32,
    rudp: ReliableLink,
    last_rx: Instant,
    last_tx: Instant,
    /// Viewer: the host acknowledged the COMMIT (sent us anything on the
    /// path); until then each keepalive repeats the COMMIT.
    confirmed: bool,
    last_dack: Instant,
    dacked: u64,
    /// Viewer: when the last REBIND went out.
    last_rebind: Option<Instant>,
    /// Wave 3: v2 framing + congestion control (media enabled).
    cc: Option<LinkCc>,
}

/// A datagram waiting in the pacer: a rudp packet, or a media plaintext
/// (sealed when it leaves, so the AAD covers its final `tseq`).
enum Paced {
    Rudp(Vec<u8>),
    Media(Vec<u8>),
}

impl Paced {
    fn wire_len(&self) -> usize {
        match self {
            Paced::Rudp(p) => media::V2_HEADER + p.len(),
            Paced::Media(p) => media::MEDIA_OVERHEAD + p.len(),
        }
    }
}

/// Per-link v2 state: transport sequence numbers, the sent history, the
/// controller, the pacer and the receiver's feedback recorder.
struct LinkCc {
    gcc: Gcc,
    pacer: Pacer<Paced>,
    hist: SentHistory,
    next_tseq: u32,
    fb: FeedbackRx,
    epoch: Instant,
    datagrams_tx: u64,
    /// Highest rudp DATA seq handed to the pacer (retransmit detection).
    rudp_top: Option<u32>,
}

impl LinkCc {
    fn new(epoch: Instant) -> Self {
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

    fn us(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.epoch).as_micros() as u64
    }

    /// Frame and send one datagram now (no pacing). A `WouldBlock` leaves
    /// `tseq` unconsumed; a media plaintext always consumes a packet number.
    fn emit(
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
    fn send_now(
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
fn rudp_out<'a, 'b: 'a>(
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

/// Media sender keying (session-wide: survives path changes).
struct MediaTx {
    key: [u8; 32],
    /// Next packet number; starts at 1, never reused.
    next_pn: u64,
}

/// One video frame kept for NACK retransmission.
struct SentFrame {
    id: u32,
    k: u16,
    msgs: Vec<Vec<u8>>,
    at: Instant,
    resends: u8,
}

/// Sender-side media counters.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct MediaStats {
    pub frames_tx: u64,
    pub shards_tx: u64,
    pub parity_tx: u64,
    pub nack_rx: u64,
    pub shards_resent: u64,
    pub audio_tx: u64,
    pub feedback_tx: u64,
    /// Media datagrams that failed authentication / replayed.
    pub bad_rx: u64,
    pub replayed_rx: u64,
    pub rx: MediaRxStats,
    pub cc: CcStats,
    /// Current controller target (bits/s), loss estimate, FEC ratio.
    pub target_bps: f64,
    pub loss: f64,
    pub fec_ratio: f64,
    pub queue_delay_ms: f64,
}

/// Session-wide media state (see [`DirectEngine::enable_media`]).
struct MediaState {
    tx: MediaTx,
    rx_key: [u8; 32],
    replay: ReplayWindow,
    rx: MediaRx,
    next_frame: u32,
    next_aseq: u32,
    sent: VecDeque<SentFrame>,
    sent_bytes: usize,
    stats: MediaStats,
}

/// NACK retransmission memory: frames, bytes, age.
const SENT_FRAMES: usize = 32;
const SENT_BYTES: usize = 8 << 20;
const SENT_AGE: Duration = Duration::from_millis(500);

impl MediaState {
    fn new(keys: MediaKeys) -> Self {
        Self {
            tx: MediaTx {
                key: keys.tx,
                next_pn: 1,
            },
            rx_key: keys.rx,
            replay: ReplayWindow::new(),
            rx: MediaRx::new(),
            next_frame: 0,
            next_aseq: 0,
            sent: VecDeque::new(),
            sent_bytes: 0,
            stats: MediaStats::default(),
        }
    }

    fn remember(&mut self, f: SentFrame, now: Instant) {
        self.sent_bytes += f.msgs.iter().map(Vec::len).sum::<usize>();
        self.sent.push_back(f);
        while self.sent.len() > SENT_FRAMES
            || self.sent_bytes > SENT_BYTES
            || self
                .sent
                .front()
                .is_some_and(|f| now.saturating_duration_since(f.at) > SENT_AGE)
        {
            match self.sent.pop_front() {
                Some(f) => self.sent_bytes -= f.msgs.iter().map(Vec::len).sum::<usize>(),
                None => break,
            }
        }
    }
}

/// Wave 3: point the pacer and the governed rudp window at the controller's
/// current target.
fn apply_rate(link: &mut Link) {
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

struct ProbeRound {
    paths: Vec<u32>,
    nonce: [u8; 16],
    started: Instant,
    last_sent: Option<Instant>,
}

/// The sans-IO direct-path engine (see the module docs).
pub struct DirectEngine {
    is_server: bool,
    key: [u8; 32],
    armed: bool,
    epoch: Instant,
    // ---- sender ----
    next_idx: u64,
    on_direct: bool,
    replay: VecDeque<(u64, Vec<u8>)>,
    replay_bytes: usize,
    last_primary_tx: Instant,
    // ---- receiver ----
    synced: bool,
    delivered: u64,
    relay_next: u64,
    pending: BTreeMap<u64, Vec<u8>>,
    // ---- path ----
    link: Option<Link>,
    probe: Option<ProbeRound>,
    candidates: Vec<u32>,
    reprobe_at: Option<Instant>,
    reprobe_delay: Duration,
    commit_nonce: [u8; 16],
    stats: DirectStats,
    events: Vec<String>,
    /// Wave 3: the media channel (both peers advertised `CAP_MEDIA`).
    media: Option<MediaState>,
    /// The relay (primary) is gone: see [`DEAD_AFTER_RELAYLESS`].
    primary_lost: bool,
    /// When the current link last went active (for path-event logs).
    direct_since: Option<Instant>,
}

impl DirectEngine {
    /// A disarmed engine (pass-through until [`arm`](Self::arm)).
    pub fn new(is_server: bool, key: [u8; 32]) -> Self {
        let now = Instant::now();
        Self {
            is_server,
            key,
            armed: false,
            epoch: now,
            next_idx: 0,
            on_direct: false,
            replay: VecDeque::new(),
            replay_bytes: 0,
            last_primary_tx: now,
            synced: false,
            delivered: 0,
            relay_next: 0,
            pending: BTreeMap::new(),
            link: None,
            probe: None,
            candidates: Vec::new(),
            reprobe_at: None,
            reprobe_delay: REPROBE_AFTER,
            commit_nonce: rand16(),
            stats: DirectStats::default(),
            events: Vec::new(),
            media: None,
            primary_lost: false,
            direct_since: None,
        }
    }

    // ---------------- Wave 3: media channel ----------------

    /// Both peers advertised `CAP_MEDIA`: switch the direct link to the v2
    /// framing (congestion control, pacing) and open the media channel.
    /// Idempotent — the first keys stay (a media sender must never restart
    /// its packet numbers under the same key).
    pub fn enable_media(&mut self, keys: MediaKeys) {
        if self.media.is_some() {
            return;
        }
        self.media = Some(MediaState::new(keys));
        let epoch = self.epoch;
        if let Some(l) = self.link.as_mut() {
            if l.cc.is_none() {
                l.cc = Some(LinkCc::new(epoch));
                apply_rate(l);
            }
        }
        self.events
            .push("media channel + delay-based congestion control enabled".into());
    }

    /// The relay carrier died while the direct path carried the session:
    /// keep the link through longer silences (nothing to fall back to).
    pub fn primary_lost(&mut self, now: Instant) {
        if !self.primary_lost {
            self.primary_lost = true;
            let since = self
                .direct_since
                .map_or(0.0, |t| now.saturating_duration_since(t).as_secs_f64());
            let rx = self.link.as_ref().map_or(f64::NAN, |l| {
                now.saturating_duration_since(l.last_rx).as_secs_f64()
            });
            self.events.push(format!(
                "relay gone {since:.1} s after the direct path went active (direct last heard {rx:.2} s ago); \
                 direct path keeps the session, given up after {} s of silence",
                DEAD_AFTER_RELAYLESS.as_secs()
            ));
        }
    }

    pub fn media_enabled(&self) -> bool {
        self.media.is_some()
    }

    /// Whether media can go out now (v2 link active and confirmed).
    pub fn media_ready(&self) -> bool {
        self.media.is_some()
            && self.on_direct
            && self
                .link
                .as_ref()
                .is_some_and(|l| l.cc.is_some() && l.confirmed)
    }

    /// The congestion controller's target rate (bits/s) while the v2 link
    /// carries the session.
    pub fn cc_target(&self) -> Option<f64> {
        self.media_ready()
            .then(|| self.link.as_ref()?.cc.as_ref().map(|c| c.gcc.target_bps()))
            .flatten()
    }

    /// The controller's one-way queueing-delay estimate.
    pub fn cc_queue_delay(&self) -> Option<Duration> {
        self.media_ready()
            .then(|| self.link.as_ref()?.cc.as_ref().map(|c| c.gcc.queue_delay()))
            .flatten()
    }

    /// The FEC ratio the next video frame gets.
    pub fn fec_ratio(&self) -> f64 {
        media::fec_ratio(
            self.link
                .as_ref()
                .and_then(|l| l.cc.as_ref())
                .map_or(0.0, |c| c.gcc.loss()),
        )
    }

    /// Media counters (`None` = media not enabled).
    pub fn media_stats(&self) -> Option<MediaStats> {
        let m = self.media.as_ref()?;
        let mut s = m.stats;
        s.rx = m.rx.stats();
        if let Some(cc) = self.link.as_ref().and_then(|l| l.cc.as_ref()) {
            s.cc = cc.gcc.stats();
            s.target_bps = cc.gcc.target_bps();
            s.loss = cc.gcc.loss();
            s.queue_delay_ms = cc.gcc.queue_delay().as_secs_f64() * 1000.0;
        }
        s.fec_ratio = self.fec_ratio();
        Some(s)
    }

    /// Media received since the last call (video units in frame order,
    /// losses, audio packets).
    pub fn take_media_events(&mut self) -> Vec<MediaEvent> {
        self.media
            .as_mut()
            .map(|m| m.rx.take_events())
            .unwrap_or_default()
    }

    /// Send one video unit (`VideoHeader || Annex-B`) on the media channel:
    /// FEC-coded shards into the pacer's video lane. `false` = not sent
    /// (media not ready, or the unit is unusable) — send it reliably.
    pub fn send_media_video(
        &mut self,
        unit: &[u8],
        keyframe: bool,
        now: Instant,
        io: &mut dyn DirectIo,
    ) -> bool {
        if !self.media_ready() {
            return false;
        }
        let ratio = self.fec_ratio();
        let (Some(link), Some(ms)) = (self.link.as_mut(), self.media.as_mut()) else {
            return false;
        };
        let Some(cc) = link.cc.as_mut() else {
            return false;
        };
        let id = ms.next_frame;
        let Ok(msgs) = media::fec_encode(unit, id, keyframe, ratio) else {
            return false;
        };
        ms.next_frame = id.wrapping_add(1);
        let k = unit.len().div_ceil(media::MAX_SHARD) as u16;
        for m in &msgs {
            let item = Paced::Media(m.clone());
            cc.pacer.push(Lane::Media, item.wire_len(), item);
        }
        ms.stats.frames_tx += 1;
        ms.stats.shards_tx += msgs.len() as u64;
        ms.stats.parity_tx += (msgs.len() - usize::from(k)) as u64;
        ms.remember(
            SentFrame {
                id,
                k,
                msgs,
                at: now,
                resends: 0,
            },
            now,
        );
        self.pace(now, io);
        true
    }

    /// Send one AUDIO payload on the media channel (unpaced: it is tiny and
    /// latency-critical). `false` = not sent (send it reliably).
    pub fn send_media_audio(
        &mut self,
        payload: &[u8],
        now: Instant,
        io: &mut dyn DirectIo,
    ) -> bool {
        if !self.media_ready() {
            return false;
        }
        let (Some(link), Some(ms)) = (self.link.as_mut(), self.media.as_mut()) else {
            return false;
        };
        let Some(cc) = link.cc.as_mut() else {
            return false;
        };
        let aseq = ms.next_aseq;
        ms.next_aseq = aseq.wrapping_add(1);
        let item = Paced::Media(media::audio_msg(aseq, payload));
        let _ = cc.send_now(Some(&mut ms.tx), now, link.path, &item, io);
        link.last_tx = now;
        ms.stats.audio_tx += 1;
        true
    }

    /// Release what the pacer allows now.
    fn pace(&mut self, now: Instant, io: &mut dyn DirectIo) {
        let Some(link) = self.link.as_mut() else {
            return;
        };
        let Some(cc) = link.cc.as_mut() else { return };
        let path = link.path;
        let mut tx = self.media.as_mut().map(|m| &mut m.tx);
        let mut sent = false;
        while let Some((lane, item)) = cc.pacer.pop(now) {
            match cc.emit(tx.as_deref_mut(), now, path, &item, io) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    cc.pacer.push_front(lane, item.wire_len(), item);
                    break;
                }
                _ => sent = true,
            }
        }
        if sent {
            link.last_tx = now;
        }
    }

    /// A v2 datagram on the link's path.
    fn on_v2(
        &mut self,
        path: u32,
        d: &[u8],
        now: Instant,
        io: &mut dyn DirectIo,
        deliver: &mut dyn FnMut(&[u8]),
    ) {
        if d.len() < media::V2_HEADER {
            return;
        }
        let Some(link) = self.link.as_mut().filter(|l| l.path == path) else {
            return;
        };
        let tseq = u32::from_be_bytes(d[2..6].try_into().unwrap_or([0; 4]));
        match d[1] {
            media::KIND_RUDP => {
                if let Some(cc) = link.cc.as_mut() {
                    let at = cc.us(now);
                    cc.fb.on_packet(tseq, at);
                }
                link.last_rx = now;
                link.confirmed = true;
                let mut recs: Vec<Vec<u8>> = Vec::new();
                let Link { rudp, cc, .. } = link;
                rudp.on_datagram(
                    &d[media::V2_HEADER..],
                    now,
                    &mut |r| recs.push(r.to_vec()),
                    &mut rudp_out(cc, io, path, now),
                );
                self.on_link_records(recs, now, deliver);
            }
            media::KIND_MEDIA => {
                let Some(ms) = self.media.as_mut() else {
                    return;
                };
                let Some((pn, pt)) = media::open(&ms.rx_key, d) else {
                    ms.stats.bad_rx += 1;
                    return;
                };
                if !ms.replay.check(pn) {
                    ms.stats.replayed_rx += 1;
                    return;
                }
                ms.replay.commit(pn);
                link.last_rx = now;
                link.confirmed = true;
                if let Some(cc) = link.cc.as_mut() {
                    let at = cc.us(now);
                    cc.fb.on_packet(tseq, at);
                }
                match pt.first() {
                    Some(&media::M_VIDEO) => ms.rx.on_shard(&pt, now),
                    Some(&media::M_AUDIO) => ms.rx.on_audio(&pt),
                    Some(&media::M_FEEDBACK) => {
                        let rtt = link.rudp.srtt();
                        if let (Some(cc), Some(pk)) =
                            (link.cc.as_mut(), crate::cc::parse_feedback(&pt[1..]))
                        {
                            let reports: Vec<PacketReport> = pk
                                .iter()
                                .filter_map(|&(t, arr)| {
                                    let (send_us, size) = cc.hist.get(t)?;
                                    let arrival_us = arr.map(|a| cc.gcc.unwrap_remote(a));
                                    Some(PacketReport {
                                        tseq: t,
                                        send_us,
                                        size,
                                        arrival_us,
                                    })
                                })
                                .collect();
                            cc.gcc.on_feedback(now, &reports, rtt);
                            apply_rate(link);
                        }
                    }
                    Some(&media::M_NACK) => {
                        if let (Some((f, idx)), Some(cc)) =
                            (media::parse_nack(&pt), link.cc.as_mut())
                        {
                            ms.stats.nack_rx += 1;
                            if let Some(sf) = ms.sent.iter_mut().find(|s| s.id == f) {
                                if sf.resends < 2 {
                                    sf.resends += 1;
                                    let want: Vec<usize> = if idx.is_empty() {
                                        (0..usize::from(sf.k)).collect()
                                    } else {
                                        idx.iter().map(|&i| usize::from(i)).collect()
                                    };
                                    for i in want {
                                        if let Some(m) = sf.msgs.get(i) {
                                            let mut m = m.clone();
                                            m[15] |= media::SHARD_RETX;
                                            let item = Paced::Media(m);
                                            cc.pacer.push(Lane::Reliable, item.wire_len(), item);
                                            ms.stats.shards_resent += 1;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    _ => ms.stats.bad_rx += 1,
                }
            }
            _ => {}
        }
    }

    /// v2 timers: feedback, media loss decisions (NACK), pacing.
    fn poll_v2(&mut self, now: Instant, io: &mut dyn DirectIo) {
        let Some(link) = self.link.as_mut() else {
            return;
        };
        let path = link.path;
        let rtt = link.rudp.srtt();
        let Some(cc) = link.cc.as_mut() else { return };
        if let Some(ms) = self.media.as_mut() {
            if cc.fb.due(now) {
                for m in cc.fb.build(now, media::M_FEEDBACK) {
                    let _ = cc.send_now(Some(&mut ms.tx), now, path, &Paced::Media(m), io);
                    ms.stats.feedback_tx += 1;
                }
            }
            let MediaState { rx, tx, .. } = ms;
            rx.poll(now, rtt, &mut |f, idx| {
                cc.send_now(
                    Some(tx),
                    now,
                    path,
                    &Paced::Media(media::nack_msg(f, idx)),
                    io,
                )
                .is_ok()
            });
        }
        self.pace(now, io);
    }

    fn make_link(&self, path: u32, now: Instant, confirmed: bool) -> Link {
        let mut l = new_link(path, now, confirmed);
        if self.media.is_some() {
            l.cc = Some(LinkCc::new(self.epoch));
            apply_rate(&mut l);
        }
        l
    }

    /// The peer advertised `CAP_DIRECT`: probes and markers may flow.
    pub fn arm(&mut self) {
        self.armed = true;
    }

    pub fn is_armed(&self) -> bool {
        self.armed
    }

    pub fn is_server(&self) -> bool {
        self.is_server
    }

    pub fn state(&self) -> DirectState {
        if self.on_direct {
            DirectState::Direct
        } else if self.probe.is_some() {
            DirectState::Probing
        } else {
            DirectState::Relay
        }
    }

    /// The direct path in use (sending), if any.
    pub fn active_path(&self) -> Option<u32> {
        self.link
            .as_ref()
            .filter(|_| self.on_direct)
            .map(|l| l.path)
    }

    pub fn stats(&self) -> DirectStats {
        self.stats
    }

    /// Smoothed RTT of the direct link, once measured.
    pub fn link_rtt(&self) -> Option<Duration> {
        self.link.as_ref().and_then(|l| l.rudp.srtt())
    }

    /// Human-readable path events since the last call (promotions,
    /// demotions, probe results) for the shells' logs.
    pub fn take_events(&mut self) -> Vec<String> {
        std::mem::take(&mut self.events)
    }

    /// Outbound bytes queued on the direct link.
    pub fn pending_bytes(&self) -> usize {
        self.link.as_ref().map_or(0, |l| {
            l.rudp.pending_bytes() + l.cc.as_ref().map_or(0, |c| c.pacer.queued_bytes())
        })
    }

    fn now_us(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.epoch).as_micros() as u64
    }

    /// Viewer: probe these candidate paths (replaces earlier candidates; a
    /// round starts now).
    pub fn start_probe(&mut self, paths: Vec<u32>, now: Instant) {
        if self.is_server || !self.armed || paths.is_empty() {
            return;
        }
        self.candidates = paths;
        self.begin_round(now);
    }

    fn begin_round(&mut self, now: Instant) {
        if self.candidates.is_empty() {
            return;
        }
        self.stats.probe_rounds += 1;
        self.reprobe_at = None;
        self.probe = Some(ProbeRound {
            paths: self.candidates.clone(),
            nonce: rand16(),
            started: now,
            last_sent: None,
        });
    }

    // ---------------- sending ----------------

    /// Send one record on the current path.
    pub fn send(
        &mut self,
        record: &[u8],
        now: Instant,
        io: &mut dyn DirectIo,
    ) -> Result<(), TransportError> {
        let idx = self.next_idx;
        self.next_idx += 1;
        if self.on_direct {
            if let Some(link) = self.link.as_mut() {
                let mut framed = Vec::with_capacity(9 + record.len());
                framed.push(K_DATA);
                framed.extend_from_slice(&idx.to_be_bytes());
                framed.extend_from_slice(record);
                let path = link.path;
                // A socket error is a loss: rudp's RTO retransmits, and a
                // dead path is demoted by the liveness check.
                let Link { rudp, cc, .. } = &mut *link;
                let _ = rudp.send(&framed, now, &mut rudp_out(cc, io, path, now));
                link.last_tx = now;
                self.stats.records_direct_tx += 1;
                self.replay_bytes += record.len();
                self.replay.push_back((idx, record.to_vec()));
                while self.replay_bytes > REPLAY_LIMIT {
                    match self.replay.pop_front() {
                        Some((_, r)) => self.replay_bytes -= r.len(),
                        None => break,
                    }
                }
                self.pace(now, io);
                return Ok(());
            }
        }
        self.last_primary_tx = now;
        io.primary_send(record)
    }

    fn promote(&mut self, now: Instant, io: &mut dyn DirectIo) {
        if self.on_direct || self.link.is_none() {
            return;
        }
        // Everything below next_idx went out on the relay already.
        if io.primary_send(&marker(M_SYNC_UP, self.next_idx)).is_err() {
            return;
        }
        self.last_primary_tx = now;
        self.on_direct = true;
        self.direct_since = Some(now);
        self.stats.promotions += 1;
        self.reprobe_delay = REPROBE_AFTER;
        let path = self.link.as_ref().map_or(0, |l| l.path);
        self.events.push(format!(
            "direct path#{path} active (relay kept as fallback)"
        ));
    }

    /// Drop the direct link and move sending back to the relay, replaying
    /// what the peer has not acknowledged.
    fn demote(&mut self, now: Instant, io: &mut dyn DirectIo, why: &str) {
        let had_link = self.link.take().map(|l| l.path);
        if self.on_direct {
            self.on_direct = false;
            self.stats.demotions += 1;
            let from = self.replay.front().map_or(self.next_idx, |(i, _)| *i);
            let _ = io.primary_send(&marker(M_SYNC_DOWN, from));
            for (_, r) in std::mem::take(&mut self.replay) {
                self.stats.records_replayed += 1;
                let _ = io.primary_send(&r);
            }
            self.replay_bytes = 0;
            self.last_primary_tx = now;
        }
        if let Some(p) = had_link {
            self.events
                .push(format!("direct path#{p} dropped ({why}); back on relay"));
        }
        if !self.is_server && !self.candidates.is_empty() {
            self.reprobe_at = Some(now + self.reprobe_delay);
            self.reprobe_delay = (self.reprobe_delay * 2).min(REPROBE_MAX);
        }
    }

    // ---------------- receiving ----------------

    fn deliver_indexed(&mut self, idx: u64, rec: Vec<u8>, deliver: &mut dyn FnMut(&[u8])) {
        if !self.synced {
            if self.pending.len() < PENDING_LIMIT {
                self.pending.insert(idx, rec);
            }
            return;
        }
        if idx < self.delivered {
            self.stats.duplicates_dropped += 1;
            return;
        }
        if idx > self.delivered {
            if self.pending.len() < PENDING_LIMIT {
                self.pending.insert(idx, rec);
            }
            return;
        }
        deliver(&rec);
        self.delivered += 1;
        self.flush_pending(deliver);
    }

    fn flush_pending(&mut self, deliver: &mut dyn FnMut(&[u8])) {
        // Drop stale entries, then release the contiguous run.
        while let Some((&i, _)) = self.pending.first_key_value() {
            if i < self.delivered {
                self.pending.pop_first();
                self.stats.duplicates_dropped += 1;
            } else {
                break;
            }
        }
        while let Some(r) = self.pending.remove(&self.delivered) {
            deliver(&r);
            self.delivered += 1;
        }
    }

    /// One datagram from the primary (relay) path: a marker is consumed,
    /// a record is delivered in order.
    pub fn on_primary(
        &mut self,
        d: &[u8],
        now: Instant,
        io: &mut dyn DirectIo,
        deliver: &mut dyn FnMut(&[u8]),
    ) {
        if is_marker(d) {
            let kind = d[12];
            let n = u64::from_be_bytes(d[13..21].try_into().unwrap_or([0; 8]));
            match kind {
                M_SYNC_UP | M_SYNC_DOWN => {
                    if !self.synced {
                        self.synced = true;
                        self.delivered = n;
                    }
                    self.relay_next = n;
                    self.flush_pending(deliver);
                    // The peer gave up on the direct path: follow it (our
                    // own sending there is likely failing too).
                    if kind == M_SYNC_DOWN && self.link.is_some() {
                        self.demote(now, io, "peer fell back to relay");
                    }
                }
                _ => {} // NOP keepalive (or a future kind)
            }
            return;
        }
        if self.synced {
            let idx = self.relay_next;
            self.relay_next += 1;
            self.deliver_indexed(idx, d.to_vec(), deliver);
        } else {
            deliver(d);
        }
    }

    /// One datagram from direct path `path` (probe or rudp packet).
    pub fn on_direct(
        &mut self,
        path: u32,
        d: &[u8],
        now: Instant,
        io: &mut dyn DirectIo,
        deliver: &mut dyn FnMut(&[u8]),
    ) {
        if !self.armed {
            return;
        }
        if is_probe(d) {
            self.on_probe(path, d, now, io);
            return;
        }
        if d.first() == Some(&media::V2_MAGIC) {
            self.on_v2(path, d, now, io, deliver);
            return;
        }
        if !rudp::is_framed(d) {
            return;
        }
        let Some(link) = self.link.as_mut().filter(|l| l.path == path) else {
            return; // not our path (stale / unrelated)
        };
        link.last_rx = now;
        link.confirmed = true;
        let mut recs: Vec<Vec<u8>> = Vec::new();
        let Link { rudp, cc, .. } = link;
        rudp.on_datagram(
            d,
            now,
            &mut |r| recs.push(r.to_vec()),
            &mut rudp_out(cc, io, path, now),
        );
        self.on_link_records(recs, now, deliver);
    }

    fn on_link_records(
        &mut self,
        recs: Vec<Vec<u8>>,
        now: Instant,
        deliver: &mut dyn FnMut(&[u8]),
    ) {
        for r in recs {
            match r.first() {
                Some(&K_DATA) if r.len() >= 9 => {
                    let idx = u64::from_be_bytes(r[1..9].try_into().unwrap_or([0; 8]));
                    self.stats.records_direct_rx += 1;
                    self.deliver_indexed(idx, r[9..].to_vec(), deliver);
                }
                Some(&K_DACK) if r.len() >= 9 => {
                    let n = u64::from_be_bytes(r[1..9].try_into().unwrap_or([0; 8]));
                    while self.replay.front().is_some_and(|(i, _)| *i < n) {
                        if let Some((_, rec)) = self.replay.pop_front() {
                            self.replay_bytes -= rec.len();
                        }
                    }
                }
                _ => {}
            }
        }
        let _ = now;
    }

    fn on_probe(&mut self, path: u32, d: &[u8], now: Instant, io: &mut dyn DirectIo) {
        let Some((ty, nonce, t_us)) = open_probe(&self.key, d) else {
            self.stats.bad_probes += 1;
            return;
        };
        match (self.is_server, ty) {
            (true, P_REQ) => {
                let _ = io.direct_send(path, &probe(&self.key, P_RESP, &nonce, t_us));
            }
            (true, P_REBIND) => {
                if let Some(l) = self.link.as_mut() {
                    if l.path != path {
                        let old = l.path;
                        l.path = path;
                        l.last_rx = now;
                        l.confirmed = true;
                        self.stats.rebinds += 1;
                        self.events.push(format!(
                            "viewer's address changed: path#{old} -> path#{path} (NAT/VPN rebinding); link moved"
                        ));
                    }
                }
            }
            (true, P_COMMIT) => {
                if self.link.as_ref().is_some_and(|l| l.path == path) {
                    return; // repeated COMMIT
                }
                if self.link.is_some() {
                    // The viewer moved to another path: settle the old one
                    // on the relay first, then switch.
                    self.demote(now, io, "viewer committed another path");
                }
                self.link = Some(self.make_link(path, now, true));
                self.events
                    .push(format!("viewer committed direct path#{path}"));
                self.promote(now, io);
            }
            (false, P_RESP) => {
                let Some(round) = self.probe.as_ref() else {
                    return;
                };
                if round.nonce != nonce || !round.paths.contains(&path) {
                    return;
                }
                self.stats.probe_responses += 1;
                if self.link.is_some() {
                    return; // a slower candidate; the first one won
                }
                let rtt = self.now_us(now).saturating_sub(t_us);
                self.stats.probe_rtt_us = rtt;
                self.probe = None;
                self.link = Some(self.make_link(path, now, false));
                self.events.push(format!(
                    "probe answered on path#{path} (rtt {:.2} ms)",
                    rtt as f64 / 1000.0
                ));
                let c = probe(&self.key, P_COMMIT, &self.commit_nonce, self.now_us(now));
                let _ = io.direct_send(path, &c);
                self.promote(now, io);
            }
            _ => {}
        }
    }

    // ---------------- timers ----------------

    /// Run timers: rudp ACK/retransmit, keepalives, liveness, probe rounds.
    /// Call after every receive batch and whenever
    /// [`next_deadline`](Self::next_deadline) expires.
    pub fn poll(&mut self, now: Instant, io: &mut dyn DirectIo, deliver: &mut dyn FnMut(&[u8])) {
        if !self.armed {
            return;
        }
        // Probing.
        if self.probe.is_none() && self.link.is_none() && self.reprobe_at.is_some_and(|t| now >= t)
        {
            self.begin_round(now);
        }
        let key = self.key;
        let t_now = self.now_us(now);
        let mut failed_round = false;
        if let Some(round) = self.probe.as_mut() {
            if now.saturating_duration_since(round.started) >= PROBE_ROUND {
                failed_round = true;
            } else if round
                .last_sent
                .is_none_or(|t| now.saturating_duration_since(t) >= PROBE_INTERVAL)
            {
                round.last_sent = Some(now);
                let p = probe(&key, P_REQ, &round.nonce, t_now);
                for &path in &round.paths {
                    let _ = io.direct_send(path, &p);
                    self.stats.probes_sent += 1;
                }
            }
        }
        if failed_round {
            self.probe = None;
            self.events
                .push("no candidate answered; staying on relay".into());
            self.reprobe_at = Some(now + self.reprobe_delay);
            self.reprobe_delay = (self.reprobe_delay * 2).min(REPROBE_MAX);
        }
        // Link liveness + keepalive + DACKs.
        let dead_after = if self.primary_lost {
            DEAD_AFTER_RELAYLESS
        } else {
            DEAD_AFTER
        };
        let dead = self
            .link
            .as_ref()
            .is_some_and(|l| now.saturating_duration_since(l.last_rx) >= dead_after);
        if dead {
            let why = if self.primary_lost {
                "silent, relay gone too"
            } else {
                "silent"
            };
            self.demote(now, io, why);
        }
        let delivered = self.delivered;
        let commit = (!self.is_server).then(|| probe(&key, P_COMMIT, &self.commit_nonce, t_now));
        let mut recs: Vec<Vec<u8>> = Vec::new();
        if let Some(link) = self.link.as_mut() {
            let path = link.path;
            let idle = now.saturating_duration_since(link.last_tx) >= KEEPALIVE;
            let dack_due = delivered != link.dacked
                && (delivered - link.dacked >= DACK_EVERY
                    || now.saturating_duration_since(link.last_dack) >= DACK_INTERVAL);
            // Viewer: a silent link may mean our NAT / VPN mapping changed
            // (the host's replies go to the old one): tell the host where we
            // are now, authenticated.
            if let Some(c) = commit.as_ref().filter(|_| link.confirmed) {
                let silent = now.saturating_duration_since(link.last_rx) >= REBIND_AFTER;
                if silent
                    && link
                        .last_rebind
                        .is_none_or(|t| now.saturating_duration_since(t) >= REBIND_INTERVAL)
                {
                    link.last_rebind = Some(now);
                    let _ = c;
                    let _ = io.direct_send(path, &probe(&key, P_REBIND, &self.commit_nonce, t_now));
                    self.stats.rebinds += 1;
                }
            }
            if idle || dack_due {
                if idle && !link.confirmed {
                    if let Some(c) = &commit {
                        let _ = io.direct_send(path, c);
                    }
                }
                let mut m = Vec::with_capacity(9);
                m.push(K_DACK);
                m.extend_from_slice(&delivered.to_be_bytes());
                let Link { rudp, cc, .. } = &mut *link;
                let _ = rudp.send(&m, now, &mut rudp_out(cc, io, path, now));
                link.last_tx = now;
                link.last_dack = now;
                link.dacked = delivered;
            }
            let Link { rudp, cc, .. } = &mut *link;
            rudp.end_batch(
                now,
                &mut |r| recs.push(r.to_vec()),
                &mut rudp_out(cc, io, path, now),
            );
            // Everything we sent is acknowledged by the peer's link: the
            // replay buffer is no longer needed.
            if link.rudp.in_flight() == 0
                && link.rudp.pending_bytes() == 0
                && link
                    .cc
                    .as_ref()
                    .is_none_or(|c| c.pacer.queued(Lane::Reliable) == 0)
            {
                self.replay.clear();
                self.replay_bytes = 0;
            }
        }
        self.on_link_records(recs, now, deliver);
        self.poll_v2(now, io);
        // Relay keepalive while it idles behind the direct path.
        if self.on_direct
            && now.saturating_duration_since(self.last_primary_tx) >= PRIMARY_KEEPALIVE
            && io.primary_send(&marker(M_NOP, 0)).is_ok()
        {
            self.last_primary_tx = now;
        }
    }

    /// When [`poll`](Self::poll) is next needed.
    pub fn next_deadline(&self, now: Instant) -> Option<Duration> {
        if !self.armed {
            return None;
        }
        let mut d: Option<Duration> = None;
        let mut take = |x: Duration| d = Some(d.map_or(x, |y: Duration| y.min(x)));
        if let Some(r) = &self.probe {
            take(r.last_sent.map_or(Duration::ZERO, |t| {
                (t + PROBE_INTERVAL).saturating_duration_since(now)
            }));
        }
        if let Some(t) = self.reprobe_at {
            take(t.saturating_duration_since(now));
        }
        if let Some(l) = &self.link {
            take((l.last_tx + KEEPALIVE).saturating_duration_since(now));
            let dead_after = if self.primary_lost {
                DEAD_AFTER_RELAYLESS
            } else {
                DEAD_AFTER
            };
            take((l.last_rx + dead_after).saturating_duration_since(now));
            if !self.is_server && l.confirmed {
                let mut due = l.last_rx + REBIND_AFTER;
                if let Some(t) = l.last_rebind {
                    due = due.max(t + REBIND_INTERVAL);
                }
                take(due.saturating_duration_since(now));
            }
            if l.dacked != self.delivered {
                take((l.last_dack + DACK_INTERVAL).saturating_duration_since(now));
            }
            if let Some(x) = l.rudp.next_deadline(now) {
                take(x);
            }
            if let Some(cc) = &l.cc {
                if let Some(x) = cc.pacer.next_release(now) {
                    take(x);
                }
                if let Some(x) = cc.fb.next_due(now) {
                    take(x);
                }
            }
        }
        if let Some(x) = self.media.as_ref().and_then(|m| m.rx.next_deadline(now)) {
            take(x);
        }
        if self.on_direct {
            take((self.last_primary_tx + PRIMARY_KEEPALIVE).saturating_duration_since(now));
        }
        d
    }
}

fn new_link(path: u32, now: Instant, confirmed: bool) -> Link {
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

fn rand16() -> [u8; 16] {
    let v = ovcore::crypto::random_bytes(16);
    let mut n = [0u8; 16];
    n.copy_from_slice(&v);
    n
}

// =====================================================================
// Desktop carrier: the engine over real UDP sockets.
// =====================================================================

/// The direct-path UDP sockets (IPv4 and, when available, IPv6), unconnected
/// and non-blocking. A host binds them once on its fixed direct port and
/// hands clones to each session; a viewer binds ephemeral ones.
pub struct DirectSockets {
    pub v4: Option<UdpSocket>,
    pub v6: Option<UdpSocket>,
}

impl DirectSockets {
    /// Bind `0.0.0.0:port` and `[::]:port` (v6-only; best effort).
    pub fn bind(port: u16) -> io::Result<Self> {
        let v4 = UdpSocket::bind(("0.0.0.0", port))?;
        let port = v4.local_addr()?.port();
        let v6 = bind_v6(port).ok();
        let s = Self { v4: Some(v4), v6 };
        s.tune();
        Ok(s)
    }

    /// Bind loopback-only sockets (tests).
    pub fn bind_loopback() -> io::Result<Self> {
        let v4 = UdpSocket::bind("127.0.0.1:0")?;
        let s = Self {
            v4: Some(v4),
            v6: None,
        };
        s.tune();
        Ok(s)
    }

    fn tune(&self) {
        for s in [&self.v4, &self.v6].into_iter().flatten() {
            let _ = s.set_nonblocking(true);
            let r = socket2::SockRef::from(s);
            let _ = r.set_recv_buffer_size(crate::udp::SOCKET_BUFFER);
            let _ = r.set_send_buffer_size(crate::udp::SOCKET_BUFFER);
            #[cfg(windows)]
            disable_connreset(s);
        }
    }

    /// The bound IPv4 port.
    pub fn port(&self) -> u16 {
        self.v4
            .as_ref()
            .and_then(|s| s.local_addr().ok())
            .map_or(0, |a| a.port())
    }

    /// Independent handles to the same sockets (one per session).
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            v4: self.v4.as_ref().map(|s| s.try_clone()).transpose()?,
            v6: self
                .v6
                .as_ref()
                .map(|s| s.try_clone())
                .transpose()
                .ok()
                .flatten(),
        })
    }

    fn send_to(&self, d: &[u8], to: SocketAddr) -> io::Result<()> {
        let s = if to.is_ipv4() { &self.v4 } else { &self.v6 };
        match s {
            Some(s) => s.send_to(d, to).map(|_| ()),
            None => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "no socket for address family",
            )),
        }
    }

    fn raw(&self) -> Vec<RawSock> {
        let mut v = Vec::new();
        for s in [&self.v4, &self.v6].into_iter().flatten() {
            #[cfg(windows)]
            {
                use std::os::windows::io::AsRawSocket;
                v.push(s.as_raw_socket());
            }
            #[cfg(not(windows))]
            {
                use std::os::fd::AsRawFd;
                v.push(s.as_raw_fd());
            }
        }
        v
    }
}

fn bind_v6(port: u16) -> io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_only_v6(true)?;
    let addr: SocketAddr = (std::net::Ipv6Addr::UNSPECIFIED, port).into();
    s.bind(&addr.into())?;
    Ok(s.into())
}

/// Windows reports an ICMP port-unreachable as `WSAECONNRESET` on the next
/// `recvfrom` of an unconnected UDP socket, which would abort the drain loop
/// for every dead candidate we probed. `SIO_UDP_CONNRESET = FALSE` turns
/// that off.
#[cfg(windows)]
fn disable_connreset(s: &UdpSocket) {
    use std::os::windows::io::AsRawSocket;
    #[link(name = "ws2_32")]
    extern "system" {
        fn WSAIoctl(
            s: usize,
            code: u32,
            inbuf: *const core::ffi::c_void,
            inlen: u32,
            outbuf: *mut core::ffi::c_void,
            outlen: u32,
            ret: *mut u32,
            overlapped: *mut core::ffi::c_void,
            completion: *mut core::ffi::c_void,
        ) -> i32;
    }
    const SIO_UDP_CONNRESET: u32 = 0x9800_000C;
    let off: u32 = 0;
    let mut ret = 0u32;
    // SAFETY: plain Winsock ioctl on a live socket with a 4-byte input.
    unsafe {
        WSAIoctl(
            s.as_raw_socket() as usize,
            SIO_UDP_CONNRESET,
            (&off as *const u32).cast(),
            4,
            std::ptr::null_mut(),
            0,
            &mut ret,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
    }
}

/// Most distinct source addresses a carrier tracks as paths.
const MAX_PATHS: usize = 256;

struct SockIo<'a> {
    primary: &'a mut dyn Carrier,
    sockets: &'a DirectSockets,
    paths: &'a [SocketAddr],
}

impl DirectIo for SockIo<'_> {
    fn primary_send(&mut self, d: &[u8]) -> Result<(), TransportError> {
        self.primary.send(d).map(|_| ())
    }
    fn direct_send(&mut self, path: u32, d: &[u8]) -> io::Result<()> {
        let Some(&to) = self.paths.get(path as usize) else {
            return Err(io::Error::new(io::ErrorKind::NotFound, "unknown path"));
        };
        self.sockets.send_to(d, to)
    }
}

/// A [`Carrier`] that runs a session on its primary (relay) carrier and
/// upgrades it to a direct UDP path when one validates (see module docs).
pub struct DirectCarrier {
    primary: Box<dyn Carrier>,
    engine: DirectEngine,
    sockets: DirectSockets,
    /// Path id = index.
    paths: Vec<SocketAddr>,
    /// Host: the candidates to advertise.
    candidates: Vec<SocketAddr>,
    primary_dead: bool,
    stats: CarrierStats,
    buf: Vec<u8>,
}

impl DirectCarrier {
    /// Wrap `primary`. `key` comes from [`session_direct_key`].
    pub fn new(
        primary: Box<dyn Carrier>,
        sockets: DirectSockets,
        is_server: bool,
        key: [u8; 32],
    ) -> Self {
        Self {
            primary,
            engine: DirectEngine::new(is_server, key),
            sockets,
            paths: Vec::new(),
            candidates: Vec::new(),
            primary_dead: false,
            stats: CarrierStats::default(),
            buf: vec![0u8; 65_535],
        }
    }

    pub fn is_server(&self) -> bool {
        self.engine.is_server()
    }

    /// Host: the candidates [`candidates`](Self::candidates) reports.
    pub fn set_candidates(&mut self, c: Vec<SocketAddr>) {
        self.candidates = c;
    }

    pub fn candidates(&self) -> &[SocketAddr] {
        &self.candidates
    }

    /// The local direct port.
    pub fn local_port(&self) -> u16 {
        self.sockets.port()
    }

    pub fn arm(&mut self) {
        self.engine.arm();
    }

    pub fn is_armed(&self) -> bool {
        self.engine.is_armed()
    }

    pub fn state(&self) -> DirectState {
        self.engine.state()
    }

    pub fn direct_stats(&self) -> DirectStats {
        self.engine.stats()
    }

    pub fn link_rtt(&self) -> Option<Duration> {
        self.engine.link_rtt()
    }

    pub fn take_events(&mut self) -> Vec<String> {
        let paths = &self.paths;
        self.engine
            .take_events()
            .into_iter()
            .map(|mut e| {
                // Longest ids first so `path#1` never eats `path#10`.
                for (i, a) in paths.iter().enumerate().rev() {
                    e = e.replace(&format!("path#{i}"), &format!("path {a}"));
                }
                e
            })
            .collect()
    }

    /// The direct peer address in use, if sending directly.
    pub fn direct_peer(&self) -> Option<SocketAddr> {
        self.engine
            .active_path()
            .and_then(|p| self.paths.get(p as usize).copied())
    }

    /// Wave 3: open the media channel (see [`DirectEngine::enable_media`]).
    pub fn enable_media(&mut self, keys: MediaKeys) {
        self.engine.enable_media(keys);
    }

    pub fn media_ready(&self) -> bool {
        self.engine.media_ready()
    }

    pub fn cc_target(&self) -> Option<f64> {
        self.engine.cc_target()
    }

    pub fn cc_queue_delay(&self) -> Option<Duration> {
        self.engine.cc_queue_delay()
    }

    pub fn media_stats(&self) -> Option<MediaStats> {
        self.engine.media_stats()
    }

    pub fn take_media_events(&mut self) -> Vec<MediaEvent> {
        self.engine.take_media_events()
    }

    /// Send a video unit on the media channel (`false` = send it reliably).
    pub fn send_media_video(&mut self, unit: &[u8], keyframe: bool) -> bool {
        let mut io = SockIo {
            primary: &mut *self.primary,
            sockets: &self.sockets,
            paths: &self.paths,
        };
        let ok = self
            .engine
            .send_media_video(unit, keyframe, Instant::now(), &mut io);
        if ok {
            self.stats.record_sent(unit.len());
        }
        ok
    }

    /// Send an AUDIO payload on the media channel (`false` = send it
    /// reliably).
    pub fn send_media_audio(&mut self, payload: &[u8]) -> bool {
        let mut io = SockIo {
            primary: &mut *self.primary,
            sockets: &self.sockets,
            paths: &self.paths,
        };
        let ok = self
            .engine
            .send_media_audio(payload, Instant::now(), &mut io);
        if ok {
            self.stats.record_sent(payload.len());
        }
        ok
    }

    fn path_id(&mut self, a: SocketAddr) -> Option<u32> {
        let a = normalize(a);
        if let Some(i) = self.paths.iter().position(|p| *p == a) {
            return Some(i as u32);
        }
        if self.paths.len() >= MAX_PATHS {
            return None;
        }
        self.paths.push(a);
        Some((self.paths.len() - 1) as u32)
    }

    /// Viewer: probe the host's candidates.
    pub fn start_probe(&mut self, addrs: &[SocketAddr]) {
        let (has4, has6) = (self.sockets.v4.is_some(), self.sockets.v6.is_some());
        let ids: Vec<u32> = addrs
            .iter()
            .filter(|a| if a.is_ipv4() { has4 } else { has6 })
            .filter_map(|a| self.path_id(*a))
            .collect();
        self.engine.start_probe(ids, Instant::now());
        self.service(&mut |_| {});
    }

    /// Drain the direct sockets and run the engine timers.
    fn service(&mut self, sink: &mut dyn FnMut(&[u8])) -> usize {
        let now = Instant::now();
        let mut n = 0usize;
        let mut inbound: Vec<(SocketAddr, Vec<u8>)> = Vec::new();
        for s in [&self.sockets.v4, &self.sockets.v6].into_iter().flatten() {
            for _ in 0..4096 {
                match s.recv_from(&mut self.buf) {
                    Ok((len, from)) => inbound.push((from, self.buf[..len].to_vec())),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    // ICMP errors etc. on an unconnected socket: skip.
                    Err(_) => continue,
                }
            }
        }
        let inbound: Vec<(u32, Vec<u8>)> = inbound
            .into_iter()
            .filter_map(|(from, d)| self.path_id(from).map(|p| (p, d)))
            .collect();
        let stats = &mut self.stats;
        let mut deliver = |r: &[u8]| {
            stats.record_recv(r.len());
            n += 1;
            sink(r);
        };
        for (pid, d) in inbound {
            let mut io = SockIo {
                primary: &mut *self.primary,
                sockets: &self.sockets,
                paths: &self.paths,
            };
            self.engine.on_direct(pid, &d, now, &mut io, &mut deliver);
        }
        let mut io = SockIo {
            primary: &mut *self.primary,
            sockets: &self.sockets,
            paths: &self.paths,
        };
        self.engine.poll(now, &mut io, &mut deliver);
        n
    }
}

/// Map an IPv4-mapped IPv6 address to plain IPv4 (paths compare equal).
fn normalize(a: SocketAddr) -> SocketAddr {
    match a {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::new(v4.into(), v6.port()),
            None => a,
        },
        _ => a,
    }
}

impl Carrier for DirectCarrier {
    fn send(&mut self, datagram: &[u8]) -> Result<usize, TransportError> {
        let now = Instant::now();
        let mut io = SockIo {
            primary: &mut *self.primary,
            sockets: &self.sockets,
            paths: &self.paths,
        };
        self.engine.send(datagram, now, &mut io)?;
        self.stats.record_sent(datagram.len());
        Ok(datagram.len())
    }

    fn poll_recv(&mut self, sink: &mut dyn FnMut(&[u8])) -> Result<usize, TransportError> {
        let now = Instant::now();
        let mut relay_in: Vec<Vec<u8>> = Vec::new();
        let mut primary_err = None;
        if !self.primary_dead {
            if let Err(e) = self.primary.poll_recv(&mut |d| relay_in.push(d.to_vec())) {
                primary_err = Some(e);
            }
        }
        let mut n = 0usize;
        {
            let stats = &mut self.stats;
            let mut deliver = |r: &[u8]| {
                stats.record_recv(r.len());
                n += 1;
                sink(r);
            };
            for d in relay_in {
                let mut io = SockIo {
                    primary: &mut *self.primary,
                    sockets: &self.sockets,
                    paths: &self.paths,
                };
                self.engine.on_primary(&d, now, &mut io, &mut deliver);
            }
        }
        n += self.service(sink);
        if let Some(e) = primary_err {
            // The relay died: carry on while the direct path holds.
            if self.engine.state() == DirectState::Direct {
                if !self.primary_dead {
                    self.primary_dead = true;
                    self.engine
                        .events
                        .push(format!("relay lost ({e}); continuing on the direct path"));
                    self.engine.primary_lost(now);
                }
            } else {
                return Err(e);
            }
        }
        if self.primary_dead && self.engine.state() != DirectState::Direct {
            return Err(TransportError::Closed);
        }
        Ok(n)
    }

    fn close(&mut self) -> Result<(), TransportError> {
        self.sockets.v4 = None;
        self.sockets.v6 = None;
        self.primary.close()
    }

    fn is_reliable(&self) -> bool {
        true
    }

    fn stats(&self) -> &CarrierStats {
        &self.stats
    }

    fn pending_send(&self) -> usize {
        if self.engine.state() == DirectState::Direct {
            self.engine.pending_bytes()
        } else {
            self.primary.pending_send()
        }
    }

    fn raw_socket(&self) -> Option<RawSock> {
        self.primary.raw_socket()
    }

    fn extra_sockets(&self) -> Vec<RawSock> {
        self.sockets.raw()
    }

    fn next_deadline(&self) -> Option<Duration> {
        let a = self.primary.next_deadline();
        let b = self.engine.next_deadline(Instant::now());
        match (a, b) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn direct_mut(&mut self) -> Option<&mut DirectCarrier> {
        Some(self)
    }

    fn direct_ref(&self) -> Option<&DirectCarrier> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InMemoryCarrier;

    fn pair(key: [u8; 32]) -> (DirectCarrier, DirectCarrier, SocketAddr) {
        let (a, b) = InMemoryCarrier::pair();
        let host = DirectCarrier::new(
            Box::new(a),
            DirectSockets::bind_loopback().unwrap(),
            true,
            key,
        );
        let view = DirectCarrier::new(
            Box::new(b),
            DirectSockets::bind_loopback().unwrap(),
            false,
            key,
        );
        let addr: SocketAddr = format!("127.0.0.1:{}", host.local_port()).parse().unwrap();
        (host, view, addr)
    }

    fn pump(c: &mut DirectCarrier, got: &mut Vec<Vec<u8>>) {
        c.poll_recv(&mut |d| got.push(d.to_vec())).unwrap();
    }

    fn until(mut f: impl FnMut() -> bool, secs: u64) -> bool {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        false
    }

    #[test]
    fn probe_roundtrip_and_tamper() {
        let k = [7u8; 32];
        let p = probe(&k, P_REQ, &[1; 16], 42);
        assert_eq!(open_probe(&k, &p), Some((P_REQ, [1; 16], 42)));
        let mut bad = p;
        bad[10] ^= 1;
        assert_eq!(open_probe(&k, &bad), None);
        assert_eq!(open_probe(&[8u8; 32], &p), None);
        assert!(is_marker(&marker(M_SYNC_UP, 5)));
        // A record header (8-byte LE device id, top bytes zero) never is.
        let mut rec = vec![0u8; MARK_LEN];
        rec[..8].copy_from_slice(&0x0000_00AB_CDEF_0102u64.to_le_bytes());
        assert!(!is_marker(&rec));
    }

    #[test]
    fn key_is_symmetric() {
        let a = [1u8; 32];
        let b = [2u8; 32];
        assert_eq!(derive_direct_key(&a, &b), derive_direct_key(&a, &b));
        assert_ne!(derive_direct_key(&a, &b), derive_direct_key(&b, &a));
    }

    /// Disarmed engines are a transparent wrapper.
    #[test]
    fn unarmed_is_passthrough() {
        let (mut h, mut v, _) = pair([3; 32]);
        for i in 0..10u8 {
            v.send(&[i; 40]).unwrap();
        }
        let mut got = Vec::new();
        assert!(until(
            || {
                pump(&mut h, &mut got);
                got.len() == 10
            },
            2
        ));
        assert_eq!(got[3], vec![3u8; 40]);
        assert_eq!(h.state(), DirectState::Relay);
    }

    /// Full upgrade: records before, during and after the switch arrive
    /// once each and in order, both directions; then the direct path dies
    /// (sockets closed) and the session falls back to the relay without a
    /// gap.
    #[test]
    fn promote_then_demote_keeps_order() {
        let (mut h, mut v, addr) = pair([9; 32]);
        h.arm();
        v.arm();
        let mut hgot = Vec::new();
        let mut vgot = Vec::new();
        let mut sent_v = 0u32;
        let mut sent_h = 0u32;
        let rec = |i: u32, from: u8| -> Vec<u8> {
            let mut r = vec![from; 300 + (i as usize % 3000)];
            r[..4].copy_from_slice(&i.to_be_bytes());
            r
        };
        for _ in 0..50 {
            v.send(&rec(sent_v, 1)).unwrap();
            sent_v += 1;
            h.send(&rec(sent_h, 2)).unwrap();
            sent_h += 1;
        }
        // Unreachable candidate + the real one.
        v.start_probe(&["127.0.0.1:9".parse().unwrap(), addr]);
        assert!(until(
            || {
                pump(&mut h, &mut hgot);
                pump(&mut v, &mut vgot);
                v.send(&rec(sent_v, 1)).unwrap();
                sent_v += 1;
                h.send(&rec(sent_h, 2)).unwrap();
                sent_h += 1;
                v.state() == DirectState::Direct && h.state() == DirectState::Direct
            },
            5
        ));
        for _ in 0..200 {
            v.send(&rec(sent_v, 1)).unwrap();
            sent_v += 1;
            h.send(&rec(sent_h, 2)).unwrap();
            sent_h += 1;
            pump(&mut h, &mut hgot);
            pump(&mut v, &mut vgot);
        }
        assert!(until(
            || {
                pump(&mut h, &mut hgot);
                pump(&mut v, &mut vgot);
                hgot.len() == sent_v as usize && vgot.len() == sent_h as usize
            },
            5
        ));
        assert!(h.direct_stats().records_direct_rx > 0 && v.direct_stats().records_direct_rx > 0);
        assert!(v.direct_peer().is_some());
        // Kill the direct path on the host side: both demote, replay, and
        // later records still arrive in order on the relay.
        h.sockets.v4 = None;
        for _ in 0..20 {
            v.send(&rec(sent_v, 1)).unwrap();
            sent_v += 1;
            h.send(&rec(sent_h, 2)).unwrap();
            sent_h += 1;
        }
        assert!(until(
            || {
                pump(&mut h, &mut hgot);
                pump(&mut v, &mut vgot);
                v.state() != DirectState::Direct
                    && h.state() != DirectState::Direct
                    && hgot.len() == sent_v as usize
                    && vgot.len() == sent_h as usize
            },
            10
        ));
        for (i, r) in hgot.iter().enumerate() {
            assert_eq!(
                u32::from_be_bytes(r[..4].try_into().unwrap()),
                i as u32,
                "host rx order"
            );
        }
        for (i, r) in vgot.iter().enumerate() {
            assert_eq!(
                u32::from_be_bytes(r[..4].try_into().unwrap()),
                i as u32,
                "viewer rx order"
            );
        }
        assert!(h.direct_stats().demotions >= 1 || v.direct_stats().demotions >= 1);
    }

    #[derive(Default)]
    struct MemIo {
        relay: Vec<Vec<u8>>,
        direct: Vec<Vec<u8>>,
    }

    impl DirectIo for MemIo {
        fn primary_send(&mut self, d: &[u8]) -> Result<(), TransportError> {
            self.relay.push(d.to_vec());
            Ok(())
        }
        fn direct_send(&mut self, _p: u32, d: &[u8]) -> io::Result<()> {
            self.direct.push(d.to_vec());
            Ok(())
        }
    }

    /// Two engines with media enabled and a v2 link on path 0 (no probing).
    fn media_pair(now: Instant) -> (DirectEngine, DirectEngine) {
        let (ck, sk) = ([3u8; 32], [4u8; 32]);
        let mut h = DirectEngine::new(true, [1; 32]);
        let mut v = DirectEngine::new(false, [1; 32]);
        for (e, srv) in [(&mut h, true), (&mut v, false)] {
            e.arm();
            e.enable_media(media::derive_media_keys(&ck, &sk, srv));
            e.link = Some(e.make_link(0, now, true));
            e.on_direct = true;
        }
        (h, v)
    }

    /// Wave 3: video units cross a lossy path in order — FEC repairs most
    /// losses, NACKs (short RTT) the rest; audio gaps are reported; the
    /// controller gets feedback; records still flow reliably next to it.
    #[test]
    fn media_frames_survive_random_loss() {
        let t0 = Instant::now();
        let (mut h, mut v) = media_pair(t0);
        assert!(h.media_ready() && v.media_ready());
        let (mut hio, mut vio) = (MemIo::default(), MemIo::default());
        // What promotion would have sent on the relay: records 0.. go direct.
        v.on_primary(&marker(M_SYNC_UP, 0), t0, &mut vio, &mut |_| {});
        let mut rng = 0x2545_F491_4F6C_DD1Du64;
        let mut drop = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % 100 < 3
        };
        let mut now = t0;
        let (mut units, mut lost, mut audio, mut audio_lost, mut recs) =
            (Vec::new(), 0u32, 0, 0u32, 0);
        for f in 0..150u32 {
            let mut unit = vec![(f % 251) as u8; 2000 + (f as usize * 997) % 30_000];
            unit[..4].copy_from_slice(&f.to_be_bytes());
            assert!(h.send_media_video(&unit, f % 50 == 0, now, &mut hio));
            for _ in 0..3 {
                assert!(h.send_media_audio(&[1, 0, 0, 0, 1, 224, 9], now, &mut hio));
            }
            h.send(&[0xAB; 300], now, &mut hio).unwrap();
            for _ in 0..33 {
                now += Duration::from_millis(1);
                h.poll(now, &mut hio, &mut |_| {});
                for d in std::mem::take(&mut hio.direct) {
                    if !drop() {
                        v.on_direct(0, &d, now, &mut vio, &mut |_| recs += 1);
                    }
                }
                v.poll(now, &mut vio, &mut |_| recs += 1);
                for d in std::mem::take(&mut vio.direct) {
                    if !drop() {
                        h.on_direct(0, &d, now, &mut hio, &mut |_| {});
                    }
                }
                for e in v.take_media_events() {
                    match e {
                        MediaEvent::Video(u) => {
                            units.push(u32::from_be_bytes(u[..4].try_into().unwrap()))
                        }
                        MediaEvent::VideoLost { frames } => lost += frames,
                        MediaEvent::Audio { lost, .. } => {
                            audio += 1;
                            audio_lost += lost;
                        }
                    }
                }
            }
        }
        let hs = h.media_stats().unwrap();
        let vs = v.media_stats().unwrap();
        eprintln!(
            "units {} lost {lost} audio {audio}/{audio_lost} recs {recs}; rx {:?}; tx {:?}",
            units.len(),
            vs.rx,
            hs
        );
        assert!(
            units.windows(2).all(|w| w[0] < w[1]),
            "in order, no duplicates"
        );
        assert!(
            units.len() as u32 + lost >= 149,
            "every frame delivered or reported lost"
        );
        assert!(lost <= 2, "{lost} frames lost at 3% loss");
        assert!(vs.rx.fec_recovered > 10);
        assert!(audio_lost > 0 && audio + audio_lost >= 449);
        assert_eq!(recs, 150, "reliable records next to the media");
        assert!(hs.cc.feedbacks > 50, "feedback reached the sender");
        assert!(hs.parity_tx > 0);
    }

    /// The viewer's NAT / VPN mapping changes mid-session (live: an iPhone
    /// behind a VPN app, relay WebSocket and UDP flow re-established at
    /// once): its datagrams now come from a new source port and the host's
    /// replies to the old one vanish. The viewer's REBIND moves the host's
    /// link; records keep flowing both ways, in order, without the relay.
    #[test]
    fn nat_rebinding_moves_the_link() {
        let (mut h, mut v, addr) = pair([12; 32]);
        h.arm();
        v.arm();
        v.start_probe(&[addr]);
        let (mut hg, mut vg) = (Vec::new(), Vec::new());
        let (mut sh, mut sv) = (0u32, 0u32);
        let rec = |i: u32| -> Vec<u8> {
            let mut r = vec![7u8; 200];
            r[..4].copy_from_slice(&i.to_be_bytes());
            r
        };
        assert!(until(
            || {
                pump(&mut h, &mut hg);
                pump(&mut v, &mut vg);
                v.state() == DirectState::Direct && h.state() == DirectState::Direct
            },
            5
        ));
        // Warm up: the SYNC markers cross the relay, records flow direct.
        assert!(until(
            || {
                h.send(&rec(sh)).unwrap();
                sh += 1;
                v.send(&rec(sv)).unwrap();
                sv += 1;
                pump(&mut h, &mut hg);
                pump(&mut v, &mut vg);
                hg.len() >= 10 && vg.len() >= 10
            },
            5
        ));
        // Relay gone on both sides, then the viewer's address changes.
        h.primary_dead = true;
        h.engine.primary_lost(Instant::now());
        v.primary_dead = true;
        v.engine.primary_lost(Instant::now());
        v.sockets = DirectSockets::bind_loopback().unwrap();
        assert!(
            until(
                || {
                    h.send(&rec(sh)).unwrap();
                    sh += 1;
                    v.send(&rec(sv)).unwrap();
                    sv += 1;
                    let _ = h.poll_recv(&mut |d| hg.push(d.to_vec()));
                    let _ = v.poll_recv(&mut |d| vg.push(d.to_vec()));
                    h.direct_stats().rebinds >= 1
                        && hg.len() as u32 >= sv.saturating_sub(5)
                        && sv > 20
                },
                8
            ),
            "host {:?} viewer {:?} hg {} sv {} vg {} sh {} ev {:?} {:?}",
            h.direct_stats(),
            v.direct_stats(),
            hg.len(),
            sv,
            vg.len(),
            sh,
            h.take_events(),
            v.take_events()
        );
        assert!(
            until(
                || {
                    let _ = h.poll_recv(&mut |d| hg.push(d.to_vec()));
                    let _ = v.poll_recv(&mut |d| vg.push(d.to_vec()));
                    hg.len() as u32 == sv && vg.len() as u32 == sh
                },
                8
            ),
            "host got {}/{sv}, viewer got {}/{sh}",
            hg.len(),
            vg.len()
        );
        assert_eq!(h.state(), DirectState::Direct);
        assert_eq!(v.state(), DirectState::Direct);
        for (i, r) in vg.iter().enumerate() {
            assert_eq!(
                u32::from_be_bytes(r[..4].try_into().unwrap()),
                i as u32,
                "viewer rx order"
            );
        }
        assert!(h.take_events().iter().any(|e| e.contains("rebinding")));
    }

    /// Once the relay is gone, a direct path that goes quiet for a few
    /// seconds (Wi-Fi power save, roaming) is kept: there is nothing to fall
    /// back to. It is given up only after DEAD_AFTER_RELAYLESS.
    #[test]
    fn relayless_direct_path_survives_a_stall() {
        let t0 = Instant::now();
        let (mut h, _v) = media_pair(t0);
        let mut io = MemIo::default();
        h.poll(t0 + DEAD_AFTER / 2, &mut io, &mut |_| {});
        h.primary_lost(t0 + DEAD_AFTER / 2);
        h.poll(t0 + DEAD_AFTER * 2, &mut io, &mut |_| {});
        assert_eq!(
            h.state(),
            DirectState::Direct,
            "kept through a {:?} stall",
            DEAD_AFTER * 2
        );
        assert!(h.next_deadline(t0 + DEAD_AFTER * 2).is_some());
        h.poll(
            t0 + DEAD_AFTER_RELAYLESS + Duration::from_millis(10),
            &mut io,
            &mut |_| {},
        );
        assert_ne!(
            h.state(),
            DirectState::Direct,
            "given up once silent past the grace"
        );
        assert!(h.take_events().iter().any(|e| e.contains("relay gone")));
    }

    /// Wave 3: a replayed or forged media datagram is dropped.
    #[test]
    fn media_replay_and_forgery_are_rejected() {
        let t0 = Instant::now();
        let (mut h, mut v) = media_pair(t0);
        let (mut hio, mut vio) = (MemIo::default(), MemIo::default());
        assert!(h.send_media_audio(&[1, 0, 0, 0, 1, 224, 9], t0, &mut hio));
        let d = hio.direct.pop().unwrap();
        v.on_direct(0, &d, t0, &mut vio, &mut |_| {});
        v.on_direct(0, &d, t0, &mut vio, &mut |_| {});
        let mut forged = d.clone();
        let last = forged.len() - 1;
        forged[last] ^= 1;
        v.on_direct(0, &forged, t0, &mut vio, &mut |_| {});
        let s = v.media_stats().unwrap();
        assert_eq!(s.rx.audio, 1);
        assert_eq!(s.replayed_rx, 1);
        assert_eq!(s.bad_rx, 1);
        // The other direction's key cannot open it either (directional keys).
        let mut hio2 = MemIo::default();
        h.on_direct(0, &d, t0, &mut hio2, &mut |_| {});
        assert_eq!(h.media_stats().unwrap().bad_rx, 1);
    }

    /// Wave 3: over real sockets, enabling media before the probe makes the
    /// link v2 from the start; video crosses it and records still work.
    #[test]
    fn media_over_loopback_sockets() {
        let (mut h, mut v, addr) = pair([6; 32]);
        h.arm();
        v.arm();
        let (ck, sk) = ([8u8; 32], [9u8; 32]);
        h.enable_media(media::derive_media_keys(&ck, &sk, true));
        v.enable_media(media::derive_media_keys(&ck, &sk, false));
        v.start_probe(&[addr]);
        let (mut hg, mut vg) = (Vec::new(), Vec::new());
        assert!(until(
            || {
                pump(&mut h, &mut hg);
                pump(&mut v, &mut vg);
                h.send(&[1; 50]).unwrap();
                v.send(&[2; 50]).unwrap();
                h.media_ready() && v.media_ready()
            },
            5
        ));
        let unit: Vec<u8> = (0..50_000u32).map(|i| (i % 253) as u8).collect();
        assert!(h.send_media_video(&unit, true));
        let mut got = Vec::new();
        assert!(until(
            || {
                pump(&mut h, &mut hg);
                pump(&mut v, &mut vg);
                got.extend(v.take_media_events());
                !got.is_empty()
            },
            5
        ));
        assert_eq!(got[0], MediaEvent::Video(unit));
        assert!(h.cc_target().is_some());
        assert!(!hg.is_empty() && !vg.is_empty());
    }

    /// A probe signed with another session's key is ignored.
    #[test]
    fn foreign_probe_is_rejected() {
        let (mut h, _v, addr) = pair([4; 32]);
        h.arm();
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        s.send_to(&probe(&[5u8; 32], P_REQ, &[0; 16], 0), addr)
            .unwrap();
        s.set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let mut got = Vec::new();
        assert!(until(
            || {
                pump(&mut h, &mut got);
                h.direct_stats().bad_probes == 1
            },
            2
        ));
        let mut b = [0u8; 64];
        assert!(
            s.recv_from(&mut b).is_err(),
            "no response to a forged probe"
        );
    }

    /// Records sent on the direct path that the peer had not acknowledged
    /// are replayed on the relay at demotion (no gap, no duplicate).
    #[test]
    fn engine_replays_unacked_on_demotion() {
        struct Io {
            relay: Vec<Vec<u8>>,
            direct: Vec<(u32, Vec<u8>)>,
        }
        impl DirectIo for Io {
            fn primary_send(&mut self, d: &[u8]) -> Result<(), TransportError> {
                self.relay.push(d.to_vec());
                Ok(())
            }
            fn direct_send(&mut self, p: u32, d: &[u8]) -> io::Result<()> {
                self.direct.push((p, d.to_vec()));
                Ok(())
            }
        }
        let key = [1u8; 32];
        let now = Instant::now();
        let mut tx = DirectEngine::new(false, key);
        let mut rx = DirectEngine::new(true, key);
        tx.arm();
        rx.arm();
        let mut io_t = Io {
            relay: vec![],
            direct: vec![],
        };
        let mut io_r = Io {
            relay: vec![],
            direct: vec![],
        };
        tx.send(b"r0", now, &mut io_t).unwrap();
        tx.link = Some(new_link(0, now, true));
        tx.promote(now, &mut io_t);
        tx.send(b"d1", now, &mut io_t).unwrap();
        tx.send(b"d2", now, &mut io_t).unwrap();
        // The direct datagrams are lost; the path dies.
        tx.demote(now, &mut io_t, "test");
        let mut got: Vec<Vec<u8>> = Vec::new();
        for d in io_t.relay.drain(..) {
            rx.on_primary(&d, now, &mut io_r, &mut |r| got.push(r.to_vec()));
        }
        assert_eq!(got, vec![b"r0".to_vec(), b"d1".to_vec(), b"d2".to_vec()]);
        assert_eq!(tx.stats().records_replayed, 2);
    }
}
