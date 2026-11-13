//! The **unreliable media channel** of the direct UDP path.
//!
//! The record layer needs in-order, lossless delivery, so on a lossy path
//! one lost datagram stalls everything behind it (head-of-line blocking):
//! video, cursor and audio alike. With `CAP_MEDIA` negotiated,
//! the direct path carries two channels:
//!
//! - the **reliable** channel — unchanged records over [`crate::rudp`]
//!   (control, input, clipboard, tiles, `VIDEO_END`, IDR requests);
//! - the **media** channel — H.264 access units (Reed–Solomon FEC per
//!   frame) and Opus packets as independent, individually authenticated
//!   datagrams. A lost video frame that FEC cannot repair is NACKed (short
//!   RTT) or dropped (the viewer then asks for an IDR); a lost audio packet
//!   is concealed by the decoder (Opus in-band FEC / PLC).
//!
//! # Wire format (direct path only; all integers big-endian)
//!
//! Every datagram on a v2 link (both channels) starts with the transport
//! header used for congestion control ([`crate::cc`]):
//!
//! ```text
//! v2 datagram:  0xB8 | kind u8 | tseq u32 | send_ts u32 | body
//!   kind 1 RUDP:  body = one rudp datagram ("\xB7RU1" ...), as before
//!   kind 2 MEDIA: body = pn u64 | ciphertext | tag[16]
//!                 ChaCha20-Poly1305, key = media key of the sending
//!                 direction, nonce = 00 00 00 00 | pn (8 B BE),
//!                 AAD = the 18 bytes 0xB8 .. pn (transport header + pn)
//! ```
//!
//! `tseq` counts every datagram the sender puts on the link; `send_ts` is
//! the sender's clock in microseconds (wrapping). Media plaintexts:
//!
//! ```text
//! 1 VIDEO shard:  01 | frame_id u32 | idx u16 | k u16 | m u16 | unit_len u32
//!                    | flags u8 (1 keyframe, 2 retransmission) | shard[S]
//!                 unit = VideoHeader (38 B, see ovapp::video) || Annex-B AU;
//!                 shards 0..k = the unit cut in S-byte pieces (S even,
//!                 <= MAX_SHARD, the last zero-padded); k..k+m = Reed-Solomon
//!                 recovery shards (reed-solomon-simd, GF(2^16), index i-k)
//! 2 AUDIO:        02 | aseq u32 | AUDIO payload (ovapp::audio wire form)
//! 3 FEEDBACK:     03 | base u32 | count u16 | ref_us u32 | count x u16
//!                    (arrival - ref_us) / 50 us, 0xFFFF = not received
//! 4 NACK:         04 | frame_id u32 | n u16 | n x idx u16  (n = 0: resend
//!                    every data shard)
//! ```
//!
//! # Keys, nonces, replay
//!
//! Media keys are derived from the session's record keys (never used
//! directly): `HKDF-SHA256(salt = "ov-media-v1", IKM = client_key ||
//! server_key, info = "c2s" | "s2c", 32 B)` — one key per direction, domain
//! separated from the record keys and the probe key (`"ov-direct-v1"`). The
//! packet number `pn` is a per-direction u64 counter starting at 1 that only
//! ever increments (it is consumed even when a send fails), so a (key,
//! nonce) pair is never reused: each session derives fresh keys (ephemeral
//! DH) and each session owns exactly one media sender per direction.
//! Receivers keep a `REPLAY_WINDOW`-packet sliding window: a `pn` already
//! seen or older than the window is dropped, and the window only moves after
//! the AEAD tag verified. Feedback and NACKs are authenticated the same way,
//! so an off-path attacker cannot steer the congestion controller.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

/// First byte of a v2 datagram.
pub(crate) const V2_MAGIC: u8 = 0xB8;
pub(crate) const KIND_RUDP: u8 = 1;
pub(crate) const KIND_MEDIA: u8 = 2;
/// Transport header bytes (magic, kind, tseq, send_ts).
pub(crate) const V2_HEADER: usize = 10;
/// Transport header + packet number (the media AAD).
pub(crate) const MEDIA_AAD: usize = V2_HEADER + 8;
pub(crate) const TAG_LEN: usize = 16;
/// Media datagram overhead around the plaintext.
pub(crate) const MEDIA_OVERHEAD: usize = MEDIA_AAD + TAG_LEN;

pub(crate) const M_VIDEO: u8 = 1;
pub(crate) const M_AUDIO: u8 = 2;
pub(crate) const M_FEEDBACK: u8 = 3;
pub(crate) const M_NACK: u8 = 4;

/// VIDEO shard header bytes (type .. flags).
pub(crate) const SHARD_HEADER: usize = 1 + 4 + 2 + 2 + 2 + 4 + 1;
/// Largest shard payload: the datagram stays <= 1190 bytes.
pub(crate) const MAX_SHARD: usize = 1136;
/// Largest video unit sent on the media channel.
pub(crate) const MAX_UNIT: usize = 4 << 20;
/// Most data shards one unit can have (a [`MAX_UNIT`] unit in
/// [`MAX_SHARD`]-byte shards).
pub(crate) const MAX_DATA_SHARDS: usize = MAX_UNIT.div_ceil(MAX_SHARD);
/// Replay window, packets.
pub(crate) const REPLAY_WINDOW: u64 = 4096;

pub(crate) const SHARD_KEYFRAME: u8 = 1;
pub(crate) const SHARD_RETX: u8 = 2;

/// Extra parity always added (fraction of k), and the ceiling.
pub(crate) const FEC_MIN: f64 = 0.10;
pub(crate) const FEC_MAX: f64 = 0.30;

/// The FEC ratio for a measured loss rate (0..1): 10 % at no loss, rising
/// with loss to 30 %.
pub(crate) fn fec_ratio(loss: f64) -> f64 {
    (FEC_MIN + 4.0 * loss.max(0.0)).clamp(FEC_MIN, FEC_MAX)
}

// ---------------------------------------------------------------------------
// Keys + AEAD
// ---------------------------------------------------------------------------

/// The media keys of one endpoint.
#[derive(Clone)]
pub struct MediaKeys {
    /// Key this endpoint seals with.
    pub tx: [u8; 32],
    /// Key this endpoint opens the peer's datagrams with.
    pub rx: [u8; 32],
}

impl std::fmt::Debug for MediaKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MediaKeys(..)")
    }
}

fn hkdf32(ikm: &[u8], info: &[u8]) -> [u8; 32] {
    let k = ovcore::crypto::hkdf_sha256(b"ov-media-v1", ikm, info, 32);
    let mut out = [0u8; 32];
    out.copy_from_slice(&k);
    out
}

/// Derive both directions' media keys from the record keys (client's
/// encrypt key first); `is_server` picks which one we send with.
pub(crate) fn derive_media_keys(
    client_key: &[u8; 32],
    server_key: &[u8; 32],
    is_server: bool,
) -> MediaKeys {
    let mut ikm = [0u8; 64];
    ikm[..32].copy_from_slice(client_key);
    ikm[32..].copy_from_slice(server_key);
    let c2s = hkdf32(&ikm, b"c2s");
    let s2c = hkdf32(&ikm, b"s2c");
    if is_server {
        MediaKeys { tx: s2c, rx: c2s }
    } else {
        MediaKeys { tx: c2s, rx: s2c }
    }
}

/// The media keys of a session.
pub fn session_media_keys(session: &ovcore::session::Session) -> MediaKeys {
    let k = session.keys();
    if session.role_is_server() {
        derive_media_keys(k.decrypt_key(), k.encrypt_key(), true)
    } else {
        derive_media_keys(k.encrypt_key(), k.decrypt_key(), false)
    }
}

fn nonce(pn: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&pn.to_be_bytes());
    n
}

/// Append `pn | ciphertext | tag` of `plaintext` to `out`, which must
/// already hold the 10-byte transport header (it becomes part of the AAD).
pub(crate) fn seal_into(key: &[u8; 32], pn: u64, plaintext: &[u8], out: &mut Vec<u8>) {
    debug_assert_eq!(out.len(), V2_HEADER);
    out.extend_from_slice(&pn.to_be_bytes());
    let n = chacha20poly1305::Nonce::clone_from_slice(&nonce(pn));
    let (ct, tag) = ovcore::crypto::aead_seal(key, &n, &out[..MEDIA_AAD], plaintext);
    out.extend_from_slice(&ct);
    out.extend_from_slice(&tag);
}

/// Open a media datagram (whole v2 datagram): `(pn, plaintext)` when the
/// tag verifies. Replay is the caller's ([`ReplayWindow`]).
pub(crate) fn open(key: &[u8; 32], d: &[u8]) -> Option<(u64, Vec<u8>)> {
    if d.len() < MEDIA_OVERHEAD || d[0] != V2_MAGIC || d[1] != KIND_MEDIA {
        return None;
    }
    let pn = u64::from_be_bytes(d[V2_HEADER..MEDIA_AAD].try_into().ok()?);
    let n = chacha20poly1305::Nonce::clone_from_slice(&nonce(pn));
    let tag: [u8; 16] = d[d.len() - TAG_LEN..].try_into().ok()?;
    let pt = ovcore::crypto::aead_open(
        key,
        &n,
        &d[..MEDIA_AAD],
        &d[MEDIA_AAD..d.len() - TAG_LEN],
        tag,
    )?;
    Some((pn, pt))
}

/// Sliding anti-replay window over packet numbers: the shared
/// [`ovcore::replay::ReplayWindow`] sized to [`REPLAY_WINDOW`].
pub(crate) type ReplayWindow = ovcore::replay::ReplayWindow<{ (REPLAY_WINDOW / 64) as usize }>;

// ---------------------------------------------------------------------------
// Video FEC
// ---------------------------------------------------------------------------

/// Parsed VIDEO shard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Shard<'a> {
    pub frame_id: u32,
    pub idx: u16,
    pub k: u16,
    pub m: u16,
    pub unit_len: u32,
    pub flags: u8,
    pub data: &'a [u8],
}

/// Parse a VIDEO message (type byte included).
pub(crate) fn parse_shard(p: &[u8]) -> Option<Shard<'_>> {
    if p.len() <= SHARD_HEADER || p[0] != M_VIDEO {
        return None;
    }
    let s = Shard {
        frame_id: u32::from_be_bytes(p[1..5].try_into().ok()?),
        idx: u16::from_be_bytes([p[5], p[6]]),
        k: u16::from_be_bytes([p[7], p[8]]),
        m: u16::from_be_bytes([p[9], p[10]]),
        unit_len: u32::from_be_bytes(p[11..15].try_into().ok()?),
        flags: p[15],
        data: &p[SHARD_HEADER..],
    };
    let sz = s.data.len();
    // `k` is exactly what `fec_encode` derives from the unit length and `m`
    // never exceeds `k` (the FEC ratio is at most FEC_MAX < 1): a peer
    // cannot make the receiver pre-allocate slots for 65 535 + 65 535
    // shards per frame.
    let ok = s.k >= 1
        && usize::from(s.k) == (s.unit_len as usize).div_ceil(MAX_SHARD)
        && usize::from(s.k) <= MAX_DATA_SHARDS
        && s.m >= 1
        && s.m <= s.k
        && u32::from(s.idx) < u32::from(s.k) + u32::from(s.m)
        && sz.is_multiple_of(2)
        && sz <= MAX_SHARD
        && (s.unit_len as usize) <= usize::from(s.k) * sz
        && (s.unit_len as usize) > (usize::from(s.k) - 1) * sz
        && (s.unit_len as usize) <= MAX_UNIT;
    ok.then_some(s)
}

/// Cut `unit` into the VIDEO messages of frame `frame_id`: `k` data shards
/// plus `ceil(k x ratio)` (at least one) Reed-Solomon recovery shards.
pub(crate) fn fec_encode(
    unit: &[u8],
    frame_id: u32,
    keyframe: bool,
    ratio: f64,
) -> Result<Vec<Vec<u8>>, String> {
    if unit.is_empty() || unit.len() > MAX_UNIT {
        return Err(format!("video unit of {} bytes", unit.len()));
    }
    let k = unit.len().div_ceil(MAX_SHARD);
    let s = unit.len().div_ceil(k).next_multiple_of(2);
    let m = ((k as f64 * ratio).ceil() as usize).max(1);
    let shards: Vec<Vec<u8>> = (0..k)
        .map(|i| {
            let mut v = unit[(i * s).min(unit.len())..((i + 1) * s).min(unit.len())].to_vec();
            v.resize(s, 0);
            v
        })
        .collect();
    let recovery =
        reed_solomon_simd::encode(k, m, &shards).map_err(|e| format!("FEC encode: {e}"))?;
    let flags = if keyframe { SHARD_KEYFRAME } else { 0 };
    let msg = |idx: usize, data: &[u8]| {
        let mut p = Vec::with_capacity(SHARD_HEADER + data.len());
        p.push(M_VIDEO);
        p.extend_from_slice(&frame_id.to_be_bytes());
        p.extend_from_slice(&(idx as u16).to_be_bytes());
        p.extend_from_slice(&(k as u16).to_be_bytes());
        p.extend_from_slice(&(m as u16).to_be_bytes());
        p.extend_from_slice(&(unit.len() as u32).to_be_bytes());
        p.push(flags);
        p.extend_from_slice(data);
        p
    };
    Ok(shards
        .iter()
        .enumerate()
        .map(|(i, d)| msg(i, d))
        .chain(recovery.iter().enumerate().map(|(i, d)| msg(k + i, d)))
        .collect())
}

/// A NACK message.
pub(crate) fn nack_msg(frame_id: u32, idx: &[u16]) -> Vec<u8> {
    let n = idx.len().min(512);
    let mut p = Vec::with_capacity(7 + 2 * n);
    p.push(M_NACK);
    p.extend_from_slice(&frame_id.to_be_bytes());
    p.extend_from_slice(&(n as u16).to_be_bytes());
    for i in &idx[..n] {
        p.extend_from_slice(&i.to_be_bytes());
    }
    p
}

/// Parse a NACK message (type byte included).
pub(crate) fn parse_nack(p: &[u8]) -> Option<(u32, Vec<u16>)> {
    if p.len() < 7 || p[0] != M_NACK {
        return None;
    }
    let f = u32::from_be_bytes(p[1..5].try_into().ok()?);
    let n = usize::from(u16::from_be_bytes([p[5], p[6]]));
    if p.len() < 7 + 2 * n {
        return None;
    }
    Some((
        f,
        (0..n)
            .map(|i| u16::from_be_bytes([p[7 + 2 * i], p[8 + 2 * i]]))
            .collect(),
    ))
}

/// An AUDIO message.
pub(crate) fn audio_msg(aseq: u32, payload: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(5 + payload.len());
    p.push(M_AUDIO);
    p.extend_from_slice(&aseq.to_be_bytes());
    p.extend_from_slice(payload);
    p
}

// ---------------------------------------------------------------------------
// Receiver: frame reassembly, loss decisions, audio sequencing
// ---------------------------------------------------------------------------

/// What the media receiver hands the application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaEvent {
    /// One complete video unit (VideoHeader || Annex-B), in frame order.
    Video(Vec<u8>),
    /// `frames` video frames are gone for good (FEC and NACK could not
    /// repair them): stop decoding P-frames and ask for an IDR.
    VideoLost {
        /// How many frames were lost.
        frames: u32,
    },
    /// One audio payload; `lost` packets before it never arrived (conceal
    /// them before decoding this one).
    Audio {
        /// Packets missing before this one.
        lost: u32,
        /// The audio payload (one Opus packet).
        payload: Vec<u8>,
    },
}

/// Receiver counters.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MediaRxStats {
    /// Video shards received.
    pub shards: u64,
    /// Video frames delivered.
    pub frames: u64,
    /// Frames that needed Reed-Solomon recovery.
    pub fec_recovered: u64,
    /// Frames completed by a NACK retransmission.
    pub nack_recovered: u64,
    /// NACKs sent.
    pub nacks: u64,
    /// Video frames given up as lost.
    pub frames_lost: u64,
    /// Shards that arrived after their frame was delivered or dropped.
    pub late_shards: u64,
    /// Audio packets delivered.
    pub audio: u64,
    /// Audio packets that never arrived.
    pub audio_lost: u64,
    /// Audio packets that arrived too late (dropped).
    pub audio_late: u64,
    /// Malformed media messages.
    pub bad: u64,
}

#[derive(Debug)]
struct Asm {
    k: u16,
    m: u16,
    unit_len: u32,
    s: usize,
    keyframe: bool,
    shards: Vec<Option<Vec<u8>>>,
    have: usize,
    unit: Option<Vec<u8>>,
    nacked: Option<Instant>,
    nack_deadline: Option<Instant>,
    retx: bool,
}

/// How long after evidence that a frame finished sending (a later frame's
/// shard arrived) the receiver waits for reordered shards.
pub(crate) const REORDER_GRACE: Duration = Duration::from_millis(4);
/// A frame with no later evidence is given up after this.
pub(crate) const FRAME_TIMEOUT: Duration = Duration::from_millis(150);
/// Frames held at most (beyond: the oldest is given up).
const MAX_FRAMES: usize = 96;
/// Shard / unit bytes held across all incomplete frames at most (beyond:
/// the oldest frames are given up). Four maximum-size units: honest
/// traffic never gets close, a peer that streams shards it never completes
/// cannot grow the receiver without bound.
pub(crate) const MAX_BUFFERED: usize = 4 * MAX_UNIT;

/// The media receiver (sans-IO).
#[derive(Debug)]
pub struct MediaRx {
    frames: BTreeMap<u32, Asm>,
    /// Next frame id to release.
    next: Option<u32>,
    /// When a frame newer than `next` was first seen (evidence `next` is
    /// done sending), and the first arrival of `next` itself.
    later_seen: Option<Instant>,
    first_seen: Option<Instant>,
    /// When `next` became the head (for frames never seen at all).
    head_since: Option<Instant>,
    /// Inter-frame arrival interval (EWMA, seconds).
    frame_interval: f64,
    last_frame_start: Option<Instant>,
    /// A head frame never seen at all was NACKed (whole-frame resend).
    head_nacked: Option<u32>,
    head_nack_deadline: Option<Instant>,
    audio_next: Option<u32>,
    events: VecDeque<MediaEvent>,
    stats: MediaRxStats,
}

impl Default for MediaRx {
    fn default() -> Self {
        Self::new()
    }
}

fn seq_after(a: u32, b: u32) -> bool {
    // a strictly after b (wrapping)
    a != b && a.wrapping_sub(b) < 1 << 31
}

impl MediaRx {
    /// An empty receiver.
    pub fn new() -> Self {
        Self {
            frames: BTreeMap::new(),
            next: None,
            later_seen: None,
            first_seen: None,
            head_since: None,
            frame_interval: 1.0 / 30.0,
            last_frame_start: None,
            head_nacked: None,
            head_nack_deadline: None,
            audio_next: None,
            events: VecDeque::new(),
            stats: MediaRxStats::default(),
        }
    }

    /// Counters.
    pub fn stats(&self) -> MediaRxStats {
        self.stats
    }

    /// Drain the events for the application.
    pub fn take_events(&mut self) -> Vec<MediaEvent> {
        self.events.drain(..).collect()
    }

    /// Whether events are waiting.
    pub fn has_events(&self) -> bool {
        !self.events.is_empty()
    }

    /// The smoothed interval between video frames.
    pub fn frame_interval(&self) -> Duration {
        Duration::from_secs_f64(self.frame_interval)
    }

    /// One AUDIO message (type byte included).
    pub fn on_audio(&mut self, p: &[u8]) {
        if p.len() < 5 {
            self.stats.bad += 1;
            return;
        }
        let aseq = u32::from_be_bytes(p[1..5].try_into().unwrap_or([0; 4]));
        let lost = match self.audio_next {
            None => 0,
            Some(n) if aseq == n => 0,
            Some(n) if seq_after(aseq, n) && aseq.wrapping_sub(n) < 1000 => aseq.wrapping_sub(n),
            Some(n) if seq_after(aseq, n) => 0, // a huge jump: resync silently
            Some(_) => {
                self.stats.audio_late += 1;
                return; // late or duplicate: its slot was concealed already
            }
        };
        self.audio_next = Some(aseq.wrapping_add(1));
        self.stats.audio += 1;
        self.stats.audio_lost += u64::from(lost);
        self.events.push_back(MediaEvent::Audio {
            lost,
            payload: p[5..].to_vec(),
        });
    }

    /// One VIDEO message (type byte included).
    pub fn on_shard(&mut self, p: &[u8], now: Instant) {
        let Some(s) = parse_shard(p) else {
            self.stats.bad += 1;
            return;
        };
        self.stats.shards += 1;
        let next = *self.next.get_or_insert(s.frame_id);
        if self.head_since.is_none() {
            self.head_since = Some(now);
        }
        if s.frame_id != next && !seq_after(s.frame_id, next) {
            self.stats.late_shards += 1;
            return; // already released / given up
        }
        if s.frame_id.wrapping_sub(next) > 512 {
            // Far ahead (long outage): give up everything before it.
            let gone = s.frame_id.wrapping_sub(next);
            self.frames.clear();
            self.lost(gone);
            self.set_head(s.frame_id, now);
        }
        if seq_after(s.frame_id, next) && self.later_seen.is_none() {
            self.later_seen = Some(now);
        }
        let is_new = !self.frames.contains_key(&s.frame_id);
        let asm = self.frames.entry(s.frame_id).or_insert_with(|| Asm {
            k: s.k,
            m: s.m,
            unit_len: s.unit_len,
            s: s.data.len(),
            keyframe: s.flags & SHARD_KEYFRAME != 0,
            shards: vec![None; usize::from(s.k) + usize::from(s.m)],
            have: 0,
            unit: None,
            nacked: None,
            nack_deadline: None,
            retx: false,
        });
        if is_new {
            if let Some(t) = self.last_frame_start {
                let dt = now
                    .saturating_duration_since(t)
                    .as_secs_f64()
                    .clamp(0.004, 0.25);
                self.frame_interval = 0.9 * self.frame_interval + 0.1 * dt;
            }
            self.last_frame_start = Some(now);
            if s.frame_id == next && self.first_seen.is_none() {
                self.first_seen = Some(now);
            }
        }
        if (asm.k, asm.m, asm.unit_len, asm.s) != (s.k, s.m, s.unit_len, s.data.len()) {
            self.stats.bad += 1;
            return;
        }
        if asm.unit.is_some() || asm.shards[usize::from(s.idx)].is_some() {
            return; // done already / duplicate
        }
        asm.shards[usize::from(s.idx)] = Some(s.data.to_vec());
        asm.have += 1;
        if s.flags & SHARD_RETX != 0 {
            asm.retx = true;
        }
        if asm.have >= usize::from(asm.k) {
            match reassemble(asm) {
                Some((unit, used_fec)) => {
                    if used_fec {
                        self.stats.fec_recovered += 1;
                    }
                    if asm.retx {
                        self.stats.nack_recovered += 1;
                    }
                    asm.unit = Some(unit);
                    asm.shards = Vec::new();
                }
                None => self.stats.bad += 1,
            }
        }
        while self.frames.len() > MAX_FRAMES
            || (self.frames.len() > 1 && self.buffered_bytes() > MAX_BUFFERED)
        {
            self.give_up_head(now);
        }
    }

    /// Payload bytes held by frames not yet released.
    fn buffered_bytes(&self) -> usize {
        self.frames
            .values()
            .map(|a| a.unit.as_ref().map_or(a.have * a.s, Vec::len))
            .sum()
    }

    fn set_head(&mut self, id: u32, now: Instant) {
        self.next = Some(id);
        self.head_since = Some(now);
        self.first_seen = self.frames.contains_key(&id).then_some(now);
        self.later_seen = self.frames.keys().any(|&f| seq_after(f, id)).then_some(now);
    }

    fn lost(&mut self, frames: u32) {
        if frames == 0 {
            return;
        }
        self.stats.frames_lost += u64::from(frames);
        match self.events.back_mut() {
            Some(MediaEvent::VideoLost { frames: f }) => *f += frames,
            _ => self.events.push_back(MediaEvent::VideoLost { frames }),
        }
    }

    fn give_up_head(&mut self, now: Instant) {
        let Some(next) = self.next else { return };
        self.frames.remove(&next);
        self.lost(1);
        self.set_head(next.wrapping_add(1), now);
    }

    /// Release complete frames in order and decide about incomplete ones.
    /// `rtt` = the link's smoothed RTT; `nack` sends a NACK for a frame
    /// (returns false when it could not be sent). Call after every receive
    /// batch and at [`next_deadline`](Self::next_deadline).
    pub fn poll(
        &mut self,
        now: Instant,
        rtt: Option<Duration>,
        nack: &mut dyn FnMut(u32, &[u16]) -> bool,
    ) {
        loop {
            let Some(next) = self.next else { return };
            let complete = self.frames.get(&next).is_some_and(|a| a.unit.is_some());
            if complete {
                if let Some(mut a) = self.frames.remove(&next) {
                    if let Some(u) = a.unit.take() {
                        self.stats.frames += 1;
                        self.events.push_back(MediaEvent::Video(u));
                    }
                }
                self.set_head(next.wrapping_add(1), now);
                continue;
            }
            // Head incomplete (or never seen): overdue?
            let evidence = self.later_seen.is_some_and(|t| now >= t + REORDER_GRACE);
            let started = self.first_seen.or(self.head_since).unwrap_or(now);
            let timed_out = now >= started + FRAME_TIMEOUT;
            if !(evidence || timed_out) {
                return;
            }
            if !evidence
                && !self.frames.keys().any(|&f| seq_after(f, next))
                && !self.frames.contains_key(&next)
            {
                // Nothing at all is pending (the stream paused): no loss.
                return;
            }
            let interval = self.frame_interval();
            let asm = self.frames.get_mut(&next);
            let nack_ok = rtt.is_some_and(|r| r < interval);
            match asm {
                Some(a) if a.nack_deadline.is_some_and(|d| now < d) => return, // waiting
                Some(a) if nack_ok && a.nacked.is_none() => {
                    let missing: Vec<u16> = (0..a.k as usize + a.m as usize)
                        .filter(|&i| a.shards.get(i).is_some_and(Option::is_none))
                        .take(usize::from(a.k).saturating_sub(a.have))
                        .map(|i| i as u16)
                        .collect();
                    if nack(next, &missing) {
                        self.stats.nacks += 1;
                        a.nacked = Some(now);
                        a.nack_deadline = Some(
                            now + rtt.unwrap_or_default().mul_f64(1.5) + Duration::from_millis(8),
                        );
                        return;
                    }
                    self.give_up_head(now);
                }
                None if nack_ok && self.head_nacked != Some(next) => {
                    if nack(next, &[]) {
                        self.stats.nacks += 1;
                        self.head_nacked = Some(next);
                        self.head_nack_deadline = Some(
                            now + rtt.unwrap_or_default().mul_f64(1.5) + Duration::from_millis(8),
                        );
                        return;
                    }
                    self.give_up_head(now);
                }
                None if self.head_nacked == Some(next)
                    && self.head_nack_deadline.is_some_and(|d| now < d) =>
                {
                    return;
                }
                _ => self.give_up_head(now),
            }
        }
    }

    /// When [`poll`](Self::poll) next has a decision to make.
    pub fn next_deadline(&self, now: Instant) -> Option<Duration> {
        let next = self.next?;
        if self.frames.get(&next).is_some_and(|a| a.unit.is_some()) {
            return Some(Duration::ZERO);
        }
        let mut d: Option<Instant> = None;
        let mut take = |t: Instant| d = Some(d.map_or(t, |x: Instant| x.min(t)));
        if let Some(t) = self.later_seen {
            take(t + REORDER_GRACE);
        }
        if !self.frames.is_empty() {
            take(self.first_seen.or(self.head_since).unwrap_or(now) + FRAME_TIMEOUT);
        }
        if let Some(a) = self.frames.get(&next) {
            if let Some(t) = a.nack_deadline {
                take(t);
            }
        }
        if let Some(t) = self
            .head_nack_deadline
            .filter(|_| self.head_nacked == Some(next))
        {
            take(t);
        }
        d.map(|t| t.saturating_duration_since(now))
    }
}

/// Rebuild a frame's unit from >= k shards. `(unit, needed recovery)`.
fn reassemble(a: &Asm) -> Option<(Vec<u8>, bool)> {
    let k = usize::from(a.k);
    let mut originals: Vec<Option<Vec<u8>>> = a.shards[..k].to_vec();
    let missing = originals.iter().filter(|o| o.is_none()).count();
    if missing > 0 {
        let orig = originals
            .iter()
            .enumerate()
            .filter_map(|(i, o)| o.as_ref().map(|v| (i, v.as_slice())));
        let rec = a.shards[k..]
            .iter()
            .enumerate()
            .filter_map(|(i, o)| o.as_ref().map(|v| (i, v.as_slice())));
        let restored = reed_solomon_simd::decode(k, usize::from(a.m), orig, rec).ok()?;
        for (i, v) in restored {
            if i < k {
                originals[i] = Some(v);
            }
        }
    }
    let mut unit = Vec::with_capacity(k * a.s);
    for o in originals {
        unit.extend_from_slice(&o?);
    }
    unit.truncate(a.unit_len as usize);
    let _ = a.keyframe;
    Some((unit, missing > 0))
}

#[cfg(test)]
mod tests;
