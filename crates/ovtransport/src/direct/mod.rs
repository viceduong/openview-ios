//! Direct-path upgrade of a relayed session (`docs/WIRE_PROTOCOL.md`,
//! "Direct-path upgrade").
//!
//! A session always starts on the relay (the handshake runs there). Once both
//! peers advertised `CAP_DIRECT`, the host sends its candidate addresses
//! (LAN IPv4s, the UPnP-mapped public address, the STUN reflexive address,
//! global IPv6) in a `DIRECT_CANDIDATES` command and the viewer probes all of
//! them in parallel. The first candidate that answers an authenticated probe
//! wins: the viewer COMMITs to it and both sides move their *sending* to a
//! reliable-UDP link ([`crate::rudp::ReliableLink`]) on that path. The relay
//! stays connected as the fallback (with a keepalive); a direct path that
//! goes silent for `DEAD_AFTER` is dropped, unacknowledged records are
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
//! probe (raw UDP):  "\xB7DP1" | type u8 (1 REQ, 2 RESP, 3 COMMIT, 4 REBIND) | nonce [16]
//!                   | t_us u64 BE | mac [16]   (HMAC-SHA256(key, all before)[..16])
//! relay marker:     FF FF FF FF FF FF FF FF "OVDX" | kind u8 | n u64 BE
//!                   (kind 1 SYNC_UP, 3 SYNC_DOWN, 2 NOP keepalive)
//! direct record:    rudp-framed; inside: 0x01 | idx u64 BE | record
//!                                     or 0x02 | delivered u64 BE  (DACK / keepalive)
//! ```
//!
//! The probe key is derived from both session AEAD keys
//! (`derive_direct_key`), so only the two session peers can produce or
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
//! # Media channel + delay-based congestion control
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

use std::io;
use std::time::Duration;

use crate::carrier::TransportError;

mod carrier;
mod engine;
mod link;
mod media_state;
pub(crate) mod wire;

pub use carrier::{DirectCarrier, DirectSockets};
pub use engine::DirectEngine;
pub use media_state::MediaStats;
pub use wire::session_direct_key;
pub(crate) use wire::{is_marker, is_probe};

/// Probe datagram magic.
pub(crate) const PROBE_MAGIC: [u8; 4] = [0xB7, b'D', b'P', b'1'];
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
pub(crate) const REBIND_AFTER: Duration = Duration::from_millis(1000);
/// ... and repeat it this often while it stays silent.
pub(crate) const REBIND_INTERVAL: Duration = Duration::from_millis(500);
/// Probe datagram length.
pub(crate) const PROBE_LEN: usize = 4 + 1 + 16 + 8 + 16;

/// Relay marker prefix (an impossible record header, see module docs).
pub(crate) const MARK_PREFIX: [u8; 12] = [
    0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, b'O', b'V', b'D', b'X',
];
const M_SYNC_UP: u8 = 1;
const M_NOP: u8 = 2;
const M_SYNC_DOWN: u8 = 3;
/// Relay marker length.
pub(crate) const MARK_LEN: usize = 12 + 1 + 8;

const K_DATA: u8 = 1;
const K_DACK: u8 = 2;

/// Probe retransmit interval within a round.
pub(crate) const PROBE_INTERVAL: Duration = Duration::from_millis(100);
/// How long one probe round lasts before giving up (until the re-probe).
pub(crate) const PROBE_ROUND: Duration = Duration::from_secs(4);
/// First re-probe delay after a failed round or a demotion (doubles to
/// [`REPROBE_MAX`]).
pub(crate) const REPROBE_AFTER: Duration = Duration::from_secs(10);
/// Longest re-probe delay.
pub(crate) const REPROBE_MAX: Duration = Duration::from_secs(120);
/// Direct-link keepalive (DACK) interval when idle.
pub(crate) const KEEPALIVE: Duration = Duration::from_millis(250);
/// A direct path silent this long is dead: demote to the relay.
pub(crate) const DEAD_AFTER: Duration = Duration::from_secs(3);
/// Relay keepalive (NOP marker) interval while sending on the direct path,
/// so the idle relay tunnel stays open as the fallback.
pub(crate) const PRIMARY_KEEPALIVE: Duration = Duration::from_secs(5);
/// Once the relay is gone there is no fallback to demote to (and no relay
/// to re-promote over): a silent direct path then gets this long to recover
/// (Wi-Fi power save, a roaming hiccup) before the session is given up.
pub(crate) const DEAD_AFTER_RELAYLESS: Duration = Duration::from_secs(15);
/// Send a DACK at least this often while records arrive.
const DACK_INTERVAL: Duration = Duration::from_millis(50);
const DACK_EVERY: u64 = 64;
/// Replay buffer cap: beyond it the oldest records are forgotten (a
/// demotion after that would desync the session; the link would have to
/// be stalled for many seconds of full-rate video first).
pub(crate) const REPLAY_LIMIT: usize = 64 << 20;
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
    /// Switches from the relay to the direct path.
    pub promotions: u64,
    /// Switches from the direct path back to the relay.
    pub demotions: u64,
    /// Probe datagrams sent.
    pub probes_sent: u64,
    /// Probe rounds started.
    pub probe_rounds: u64,
    /// Valid probe responses received.
    pub probe_responses: u64,
    /// Probes that failed authentication.
    pub bad_probes: u64,
    /// Records sent on the direct path.
    pub records_direct_tx: u64,
    /// Records received on the direct path.
    pub records_direct_rx: u64,
    /// Unacknowledged records replayed on the relay after a demotion.
    pub records_replayed: u64,
    /// Duplicate records dropped by the receiver.
    pub duplicates_dropped: u64,
    /// RTT of the winning probe (request -> response), microseconds.
    pub probe_rtt_us: u64,
    /// REBINDs sent (viewer) / link moves after one (host).
    pub rebinds: u64,
}

#[cfg(test)]
mod tests;
