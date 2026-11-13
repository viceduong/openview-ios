//! Handshake state machine, protocol version 3 (see `docs/WIRE_PROTOCOL.md` §3).
//!
//! ```text
//! Client                                   Server
//! --- HELLO ------------------------------->   identity + eph keys, auth blob
//!                                              (PIN / pairing: SPAKE2 message A)
//! <-- HELLO + READY ------------------------   server identity + eph keys
//!                                              (PIN / pairing: SPAKE2 message B),
//!                                              READY = Ed25519 sig over the
//!                                              transcript [+ server finished MAC]
//! --- CONFIRM ----------------------------->   PAKE modes: client finished MAC
//! ```
//!
//! Direct-device sessions are established after two flights (the client
//! after READY, the server right after the client's HELLO). The two
//! credential modes — **PIN** (unattended) and **pairing code** (attended)
//! — are both SPAKE2 and need the third flight: the server only accepts once
//! the client proved, with a MAC keyed by the PAKE-derived secret, that it
//! knows the credential.
//!
//! # Credentials (SPAKE2)
//!
//! | Mode | SPAKE2 password | identities (client / server) |
//! |---|---|---|
//! | PIN | [`crate::pin::pin_psk`] of the PIN | `openview/v3/pin-client` / `openview/v3/pin-server` |
//! | pairing code | the 10-digit code ([`crate::pairing`]) | `openview/v3/pair-client` / `openview/v3/pair-server` |
//!
//! The distinct identities make a PIN and a pairing code never
//! interchangeable, even for equal strings. Neither credential crosses the
//! wire in any form. The PAKE key is mixed into the session key schedule
//! together with both X25519 shared secrets and the transcript hash, which
//! covers both HELLOs in full (device IDs, static + ephemeral keys, Ed25519
//! keys and signatures, auth mode, PAKE messages, clock field). Explicit key
//! confirmation runs in both directions:
//!
//! - **server → client** in READY: `HMAC(server_finished, "ov3 server finished" || TH)`
//! - **client → server** in CONFIRM: `HMAC(client_finished, "ov3 client finished" || TH)`
//!
//! A wrong credential therefore fails cleanly on both sides — the client
//! sends REJECT(auth failed) — and the only thing either a passive observer
//! or an active attacker learns is "this one guess was wrong". Nothing on the
//! wire is a function of the credential alone, so there is no offline
//! dictionary attack.
//!
//! Online guessing is bounded by the host-side [`AuthThrottle`] (both modes)
//! and, for pairing codes, by the host burning the code after one attempt.
//! A locked host answers a HELLO with REJECT(rate limited, retry-after)
//! without running the PAKE, surfaced as [`HandshakeError::RateLimited`].
//!
//! # Versioning
//!
//! HELLO carries a version byte ([`PROTOCOL_VERSION`] = 3). A server answers
//! any other version — and a v1 HELLO (tag `0x01`) — with REJECT(incompatible
//! version) and fails with [`HandshakeError::IncompatibleVersion`]; an older
//! peer fails immediately on that REJECT (v2) or on the unknown tag (v1)
//! instead of hanging.
//!
//! # Key schedule
//!
//! See [`KeySchedule`]; per-direction nonce bases are
//! `HMAC-SHA256(dir_key, sender_dev_BE8 || receiver_dev_BE8)[..12]`.

use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Instant;

use ed25519_dalek::{Signature, Signer, VerifyingKey};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use spake2::{Ed25519Group, Identity as PakeIdentity, Password, Spake2};
use x25519_dalek::{EphemeralSecret, PublicKey as X25519PublicKey};
use zeroize::{Zeroize, Zeroizing};

use crate::crypto;
use crate::error::{HandshakeError, RejectCode};
use crate::identity::Identity;
use crate::session::{KeySchedule, Session, SessionKeys};
use crate::throttle::{Attempt, AuthThrottle};

/// Handshake protocol version carried in every HELLO.
pub const PROTOCOL_VERSION: u8 = 3;
/// HELLO: both roles send (the server together with READY).
pub const TAG_HELLO: u8 = 0x11;
/// READY: server only; transcript signature [+ server finished MAC].
pub const TAG_READY: u8 = 0x12;
/// CONFIRM: client only, PAKE modes; client finished MAC.
pub const TAG_CONFIRM: u8 = 0x13;
/// REJECT: either role; `code u8 || retry_after_secs u32 BE`.
pub const TAG_REJECT: u8 = 0x1F;
/// The v1 HELLO tag (pre-PAKE peers). Answered with REJECT(incompatible).
pub const LEGACY_TAG_HELLO: u8 = 0x01;
/// The v1 READY tag.
pub const LEGACY_TAG_READY: u8 = 0x02;

/// Largest accepted handshake message body (a HELLO is ~300 bytes).
const MAX_MSG_LEN: usize = 4 * 1024;
/// SPAKE2 role identities, per credential (both sides use the same pair).
const PIN_ID_CLIENT: &[u8] = b"openview/v3/pin-client";
const PIN_ID_SERVER: &[u8] = b"openview/v3/pin-server";
const PAIR_ID_CLIENT: &[u8] = b"openview/v3/pair-client";
const PAIR_ID_SERVER: &[u8] = b"openview/v3/pair-server";
/// Domain tags.
const TRANSCRIPT_TAG: &[u8] = b"openview handshake v3";
const READY_SIG_TAG: &[u8] = b"ov3 ready";
const SERVER_FINISHED_TAG: &[u8] = b"ov3 server finished";
const CLIENT_FINISHED_TAG: &[u8] = b"ov3 client finished";
/// SPAKE2 (Ed25519 group) message: side byte + 32-byte element.
const PAKE_MSG_LEN: usize = 33;
const MAC_LEN: usize = 32;

type HmacSha256 = Hmac<Sha256>;

// ---------------- wire primitives (u16 BE lengths, per spec §3) ----------------

fn malformed(what: &str) -> HandshakeError {
    HandshakeError::Malformed(what.to_owned())
}

fn wbytes(o: &mut Vec<u8>, b: &[u8]) {
    debug_assert!(b.len() <= usize::from(u16::MAX));
    o.extend_from_slice(&(b.len() as u16).to_be_bytes());
    o.extend_from_slice(b);
}

/// Wrap a message body as a full wire datagram: `[1B tag][2B u16-BE len][body]`.
fn datagram(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(body.len() + 3);
    b.push(tag);
    wbytes(&mut b, body);
    b
}

/// Bounds-checked big-endian reader over a message body.
struct Rd<'a> {
    b: &'a [u8],
    off: usize,
}

impl<'a> Rd<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, off: 0 }
    }

    fn take(&mut self, n: usize, what: &str) -> Result<&'a [u8], HandshakeError> {
        let end = self.off.checked_add(n).ok_or_else(|| malformed(what))?;
        let s = self.b.get(self.off..end).ok_or_else(|| malformed(what))?;
        self.off = end;
        Ok(s)
    }

    fn u8(&mut self, what: &str) -> Result<u8, HandshakeError> {
        Ok(self.take(1, what)?[0])
    }

    fn array<const N: usize>(&mut self, what: &str) -> Result<[u8; N], HandshakeError> {
        let mut a = [0u8; N];
        a.copy_from_slice(self.take(N, what)?);
        Ok(a)
    }

    /// `lp(x)`: u16 BE length + bytes.
    fn lp(&mut self, what: &str) -> Result<&'a [u8], HandshakeError> {
        let n = u16::from_be_bytes(self.array::<2>(what)?);
        self.take(usize::from(n), what)
    }

    /// `lp(x)` that must be exactly `N` bytes.
    fn lp_array<const N: usize>(&mut self, what: &str) -> Result<[u8; N], HandshakeError> {
        let s = self.lp(what)?;
        s.try_into().map_err(|_| malformed(what))
    }

    fn finish(&self, what: &str) -> Result<(), HandshakeError> {
        if self.off == self.b.len() {
            Ok(())
        } else {
            Err(HandshakeError::Malformed(format!(
                "trailing bytes in {what}"
            )))
        }
    }
}

fn sha256_fp(ed: &VerifyingKey) -> [u8; 16] {
    let h = crypto::sha256(ed.as_bytes());
    let mut f = [0u8; 16];
    f.copy_from_slice(&h[..16]);
    f
}

/// `SHA-256(tag || u32(len c) || c || u32(len s) || s)` over the two HELLO bodies.
fn transcript_hash(client_hello: &[u8], server_hello: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(TRANSCRIPT_TAG);
    for part in [client_hello, server_hello] {
        h.update((part.len() as u32).to_be_bytes());
        h.update(part);
    }
    h.finalize().into()
}

fn finished_mac(key: &[u8; 32], tag: &[u8], th: &[u8; 32]) -> HmacSha256 {
    let mut m = <HmacSha256 as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
    m.update(tag);
    m.update(th);
    m
}

// ---------------- public API ----------------

/// Which side of the handshake this endpoint plays.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    /// The dialing side (sends the first HELLO).
    Client,
    /// The answering side (validates the client's HELLO, verifies PINs).
    Server,
}

/// Authentication mode advertised in HELLO (wire byte).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum AuthMode {
    /// Attended access: a one-time pairing code ([`crate::pairing`]),
    /// authenticated with SPAKE2.
    PairingCode = 0,
    /// Peer is pinned by device ID / fingerprint (LAN / direct).
    DirectDevice = 1,
    /// Certificate-based (not implemented).
    MutualTls = 2,
    /// Unattended access: the access PIN, authenticated with SPAKE2.
    Pin = 3,
}

impl AuthMode {
    fn from_id(v: u8) -> Result<Self, HandshakeError> {
        match v {
            0 => Ok(Self::PairingCode),
            1 => Ok(Self::DirectDevice),
            2 => Err(HandshakeError::AuthModeMismatch(
                "MutualTls is not implemented".into(),
            )),
            3 => Ok(Self::Pin),
            _ => Err(HandshakeError::Malformed(format!("unknown auth mode {v}"))),
        }
    }

    /// Whether this mode authenticates a credential with SPAKE2 (three
    /// flights, key confirmation, throttled).
    #[must_use]
    pub fn is_pake(self) -> bool {
        matches!(self, Self::Pin | Self::PairingCode)
    }

    /// The credential's name in error messages.
    fn credential(self) -> &'static str {
        match self {
            Self::PairingCode => "pairing code",
            _ => "PIN",
        }
    }
}

/// The credential this side authenticates with.
///
/// `Debug` is redacting and credentials are zeroized on drop.
#[derive(Clone, PartialEq, Eq)]
pub enum Auth {
    /// The one-time pairing code of the **server** (both sides hold the same
    /// code: the host generated it, the user typed it on the viewer). The
    /// SPAKE2 password — never sent on the wire.
    PairingCode(String),
    /// Nothing to present — the peer is pinned out-of-band.
    DirectDevice,
    /// Unattended access: the stretched access PIN (see
    /// [`crate::pin::pin_psk`]), used as the SPAKE2 password. Never sent on
    /// the wire in any form an observer could test guesses against.
    Pin([u8; 32]),
    /// Certificate bytes (not implemented).
    MutualTls,
}

impl Drop for Auth {
    fn drop(&mut self) {
        match self {
            Auth::Pin(k) => k.zeroize(),
            Auth::PairingCode(c) => c.zeroize(),
            Auth::DirectDevice | Auth::MutualTls => {}
        }
    }
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Auth::PairingCode(_) => f.write_str("PairingCode(<redacted>)"),
            Auth::DirectDevice => f.write_str("DirectDevice"),
            Auth::Pin(_) => f.write_str("Pin(<redacted>)"),
            Auth::MutualTls => f.write_str("MutualTls"),
        }
    }
}

impl Auth {
    fn mode(&self) -> AuthMode {
        match self {
            Auth::PairingCode(_) => AuthMode::PairingCode,
            Auth::DirectDevice => AuthMode::DirectDevice,
            Auth::Pin(_) => AuthMode::Pin,
            Auth::MutualTls => AuthMode::MutualTls,
        }
    }

    /// The SPAKE2 password and role identities of a credential mode.
    fn pake_params(&self) -> Option<(&[u8], &'static [u8], &'static [u8])> {
        match self {
            Auth::Pin(psk) => Some((psk.as_slice(), PIN_ID_CLIENT, PIN_ID_SERVER)),
            Auth::PairingCode(code) => Some((code.as_bytes(), PAIR_ID_CLIENT, PAIR_ID_SERVER)),
            Auth::DirectDevice | Auth::MutualTls => None,
        }
    }

    /// Decode a wire auth blob back into [`Auth`]. In the credential modes
    /// the blob is a PAKE message, not the credential, so the result carries
    /// an empty / all-zero credential (the caller supplies its own).
    #[must_use]
    pub fn from_wire(mode: AuthMode, _blob: &[u8]) -> Auth {
        match mode {
            AuthMode::PairingCode => Auth::PairingCode(String::new()),
            AuthMode::DirectDevice => Auth::DirectDevice,
            AuthMode::Pin => Auth::Pin([0u8; 32]),
            AuthMode::MutualTls => Auth::MutualTls,
        }
    }
}

/// What a peer told us in its HELLO (validated before this is stored).
#[derive(Clone, Debug)]
pub struct PeerInfo {
    /// The peer's device id.
    pub device_id: u64,
    /// 16-byte fingerprint of the peer's Ed25519 key.
    pub fingerprint: [u8; 16],
    /// The authentication mode the peer advertised.
    pub auth_mode: AuthMode,
    /// Peer's wall-clock offset estimate in ms (0 in practice).
    pub clock_offset_ms: i64,
    /// The peer's static X25519 public key.
    pub x25519_static_pk: X25519PublicKey,
    /// The peer's ephemeral X25519 public key for this handshake.
    pub x25519_eph_pk: X25519PublicKey,
}

/// Output of the handshake state machine.
#[derive(Clone, Debug)]
pub enum HandshakeEvent {
    /// Bytes to send over the wire (one datagram).
    Sent(Vec<u8>),
}

/// A parsed, signature-checked HELLO.
struct Hello<'a> {
    device_id: u64,
    static_pk: X25519PublicKey,
    eph_pk: X25519PublicKey,
    ed: VerifyingKey,
    mode: AuthMode,
    auth_blob: &'a [u8],
    clock_offset_ms: i64,
}

/// Server state between READY and the client's CONFIRM (PIN mode).
struct PendingConfirm {
    session: Session,
    client_finished: Zeroizing<[u8; 32]>,
    transcript: [u8; 32],
    /// Our HELLO + READY datagrams (re-sent on a duplicate client HELLO).
    response: Vec<Vec<u8>>,
}

enum State {
    /// Client: hello emitted, awaiting the server's HELLO, then READY.
    AwaitReady,
    /// Server: awaiting the client's HELLO.
    AwaitHello,
    /// Server, PIN mode: READY sent, awaiting the client's CONFIRM.
    AwaitConfirm(Box<PendingConfirm>),
    Established,
    Failed {
        err: HandshakeError,
        msg: String,
    },
}

/// One side of a handshake. Feed it the peer's datagrams with
/// [`feed`](Self::feed), ship what [`take_events`](Self::take_events)
/// returns, and take the [`Session`] once [`is_established`](Self::is_established).
pub struct Handshake {
    role: Role,
    device_id: u64,
    identity: Identity,
    auth: Auth,
    clock_offset_ms: i64,
    expected_device: Option<u64>,
    expected_fingerprint: Option<[u8; 16]>,

    state: State,
    rx: Vec<u8>,
    out: Vec<HandshakeEvent>,

    eph: Option<EphemeralSecret>,
    eph_pk: X25519PublicKey,
    /// Client, PAKE modes: the SPAKE2 state between HELLO and the server's HELLO.
    pake: Option<Spake2<Ed25519Group>>,
    /// The PAKE output once both messages are in.
    pake_key: Option<Zeroizing<Vec<u8>>>,
    /// Our own HELLO body (client: built in `new`; server: on the client HELLO).
    our_hello: Vec<u8>,
    /// The peer's HELLO body, verbatim (transcript input).
    peer_hello: Vec<u8>,
    peer: Option<PeerInfo>,
    /// The validated peer's Ed25519 pubkey (READY verification).
    peer_ed: Option<VerifyingKey>,
    session: Option<Session>,

    /// Server, PAKE modes: attempt limiter (default: [`AuthThrottle::global`]).
    throttle: Option<Arc<AuthThrottle>>,
    /// Server, PAKE modes: the client's address, when the carrier knows it.
    auth_source: Option<IpAddr>,
    /// Server, PAKE modes: the attempt in flight (a failure unless the
    /// client's CONFIRM verifies — also when this handshake is dropped).
    attempt: Option<Attempt>,
}

impl Handshake {
    /// Create a handshake. A client immediately emits its HELLO as the first
    /// `Sent` event (drain with [`take_events`](Self::take_events)); a server
    /// emits nothing until it receives the client's HELLO.
    pub fn new(role: Role, device_id: u64, identity: &Identity, auth: Auth) -> Self {
        let eph = EphemeralSecret::random_from_rng(rand::rngs::OsRng);
        let eph_pk = X25519PublicKey::from(&eph);
        let mut hs = Self {
            role,
            device_id,
            identity: identity.clone(),
            auth,
            clock_offset_ms: 0,
            expected_device: None,
            expected_fingerprint: None,
            state: State::AwaitHello,
            rx: Vec::new(),
            out: Vec::new(),
            eph: Some(eph),
            eph_pk,
            pake: None,
            pake_key: None,
            our_hello: Vec::new(),
            peer_hello: Vec::new(),
            peer: None,
            peer_ed: None,
            session: None,
            throttle: None,
            auth_source: None,
            attempt: None,
        };
        if role == Role::Client {
            let blob = match hs.auth.pake_params() {
                Some((password, id_client, id_server)) => {
                    let (state, msg) = Spake2::<Ed25519Group>::start_a(
                        &Password::new(password),
                        &PakeIdentity::new(id_client),
                        &PakeIdentity::new(id_server),
                    );
                    hs.pake = Some(state);
                    msg
                }
                None => Vec::new(),
            };
            hs.our_hello = hs.encode_hello_body(&blob);
            hs.state = State::AwaitReady;
            hs.out
                .push(HandshakeEvent::Sent(datagram(TAG_HELLO, &hs.our_hello)));
        }
        hs
    }

    /// Pin the peer's device ID (DirectDevice dialing by ID).
    pub fn expect_device(&mut self, device_id: u64) {
        self.expected_device = Some(device_id);
    }

    /// Pin the peer's 16-byte fingerprint (out-of-band "pine" on first pair).
    pub fn expect_fingerprint(&mut self, fp: [u8; 16]) {
        self.expected_fingerprint = Some(fp);
    }

    /// Our wall-clock offset estimate in ms (sent in HELLO). Only affects a
    /// server's HELLO: a client's HELLO is built in [`new`](Self::new).
    pub fn set_clock_offset_ms(&mut self, ms: i64) {
        self.clock_offset_ms = ms;
    }

    /// Server, PAKE modes: the client's network address, when known (direct
    /// / LAN carriers). Enables per-source rate limiting on top of the global
    /// limit. Relay-carried sessions leave it unset.
    pub fn set_auth_source(&mut self, source: IpAddr) {
        self.auth_source = Some(source);
    }

    /// Server, PAKE modes: use `throttle` instead of the process-wide
    /// [`AuthThrottle::global`].
    pub fn set_throttle(&mut self, throttle: Arc<AuthThrottle>) {
        self.throttle = Some(throttle);
    }

    /// Whether the handshake completed and a session is available.
    pub fn is_established(&self) -> bool {
        matches!(self.state, State::Established)
    }

    /// The failure reason as text, once failed.
    pub fn failed(&self) -> Option<&str> {
        match &self.state {
            State::Failed { msg, .. } => Some(msg.as_str()),
            _ => None,
        }
    }

    /// The typed failure, once failed (e.g. to tell "wrong PIN" from "too
    /// many attempts" from "incompatible version").
    pub fn error(&self) -> Option<&HandshakeError> {
        match &self.state {
            State::Failed { err, .. } => Some(err),
            _ => None,
        }
    }

    /// The AEAD record session, once established.
    pub fn session(&self) -> Option<&Session> {
        self.session.as_ref()
    }

    /// Mutable session access (record encrypt/decrypt advance nonces).
    pub fn session_mut(&mut self) -> Option<&mut Session> {
        self.session.as_mut()
    }

    /// Take the established session out of the handshake (post-handshake the
    /// state machine is done; the record channel is all that remains). The
    /// handshake is left session-less (`session()` then returns `None`).
    pub fn take_session(&mut self) -> Option<Session> {
        self.session.take()
    }

    /// What the peer announced in its HELLO, once validated.
    pub fn peer(&self) -> Option<&PeerInfo> {
        self.peer.as_ref()
    }

    /// Our own identity (diagnostics; the peer's is in `peer()`).
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// Drain pending outgoing events (datagrams to send).
    pub fn take_events(&mut self) -> Vec<HandshakeEvent> {
        std::mem::take(&mut self.out)
    }

    /// Feed received bytes (one or more datagrams, chunked arbitrarily).
    /// On success the handshake may have produced new `Sent` events and may
    /// have become established. Never panics on hostile input.
    pub fn feed(&mut self, data: &[u8]) -> Result<(), HandshakeError> {
        if let State::Failed { err, .. } = &self.state {
            return Err(err.clone());
        }
        if matches!(self.state, State::Established) {
            // Records flow through the Session from here on. Bytes fed now
            // are NOT processed: callers must stop feeding once
            // `is_established()` and hand any further datagrams of the same
            // poll batch to the session (a server that sends a record right
            // behind its READY would otherwise lose it).
            return Ok(());
        }
        self.rx.extend_from_slice(data);
        while !matches!(self.state, State::Established) {
            if self.rx.len() < 3 {
                return Ok(());
            }
            let tag = self.rx[0];
            let len = usize::from(u16::from_be_bytes([self.rx[1], self.rx[2]]));
            if len > MAX_MSG_LEN {
                return self.fail(HandshakeError::Malformed(format!(
                    "handshake message too large ({len} > {MAX_MSG_LEN})"
                )));
            }
            if self.rx.len() < 3 + len {
                return Ok(()); // wait for more
            }
            let body: Vec<u8> = self.rx[3..3 + len].to_vec();
            self.rx.drain(..3 + len);
            if let Err(e) = self.dispatch(tag, &body) {
                return self.fail(e);
            }
        }
        Ok(())
    }

    fn dispatch(&mut self, tag: u8, body: &[u8]) -> Result<(), HandshakeError> {
        match (tag, self.role) {
            (TAG_REJECT, _) => Err(self.on_reject(body)),
            (TAG_HELLO, Role::Server) => self.server_on_hello(body),
            (TAG_CONFIRM, Role::Server) => self.server_on_confirm(body),
            (TAG_HELLO, Role::Client) => self.client_on_hello(body),
            (TAG_READY, Role::Client) => self.client_on_ready(body),
            (LEGACY_TAG_HELLO, Role::Server) => {
                self.push_reject(RejectCode::IncompatibleVersion, 0);
                Err(HandshakeError::IncompatibleVersion(
                    "peer sent a v1 HELLO; v3 required — update both apps".into(),
                ))
            }
            (LEGACY_TAG_HELLO | LEGACY_TAG_READY, Role::Client) => {
                Err(HandshakeError::IncompatibleVersion(
                    "peer speaks handshake v1; v3 required — update both apps".into(),
                ))
            }
            (TAG_READY, Role::Server) => Err(HandshakeError::Unexpected("READY as server".into())),
            (TAG_CONFIRM, Role::Client) => {
                Err(HandshakeError::Unexpected("CONFIRM as client".into()))
            }
            (other, _) => Err(HandshakeError::Malformed(format!(
                "unknown handshake tag {other:#04x}"
            ))),
        }
    }

    fn fail(&mut self, err: HandshakeError) -> Result<(), HandshakeError> {
        tracing::debug!(role = ?self.role, error = %err, "handshake failed");
        self.state = State::Failed {
            msg: err.to_string(),
            err: err.clone(),
        };
        self.pake = None;
        self.pake_key = None;
        self.attempt = None; // records the failure
        Err(err)
    }

    fn push_reject(&mut self, code: RejectCode, retry_after_secs: u32) {
        let mut b = Vec::with_capacity(5);
        b.push(code as u8);
        b.extend_from_slice(&retry_after_secs.to_be_bytes());
        self.out
            .push(HandshakeEvent::Sent(datagram(TAG_REJECT, &b)));
    }

    /// The error a REJECT from the peer turns into.
    fn on_reject(&self, body: &[u8]) -> HandshakeError {
        let mut r = Rd::new(body);
        let (Ok(code), Ok(retry)) = (r.u8("REJECT"), r.array::<4>("REJECT")) else {
            return malformed("REJECT body");
        };
        let retry_after_secs = u32::from_be_bytes(retry);
        match (RejectCode::from_u8(code), self.role) {
            (Some(RejectCode::RateLimited), _) => HandshakeError::RateLimited { retry_after_secs },
            (Some(RejectCode::IncompatibleVersion), _) => HandshakeError::IncompatibleVersion(
                "the peer rejected our handshake version — update both apps".into(),
            ),
            (Some(RejectCode::AuthFailed), Role::Client) => HandshakeError::AuthFailed(
                "rejected by the host (wrong PIN or pairing code)".into(),
            ),
            (Some(RejectCode::AuthFailed), Role::Server) => HandshakeError::AuthFailed(format!(
                "client rejected our key confirmation (wrong {})",
                self.auth.mode().credential()
            )),
            (Some(RejectCode::AuthModeMismatch), _) => {
                HandshakeError::AuthModeMismatch("rejected by the peer".into())
            }
            (Some(RejectCode::Malformed), _) => {
                HandshakeError::Malformed("the peer could not parse our message".into())
            }
            (None, _) => HandshakeError::Rejected(code),
        }
    }

    // ---------------- encoding ----------------

    fn encode_hello_body(&self, auth_blob: &[u8]) -> Vec<u8> {
        let mut b = Vec::with_capacity(320);
        b.push(PROTOCOL_VERSION);
        b.extend_from_slice(&self.device_id.to_be_bytes());
        wbytes(&mut b, self.identity.x().as_bytes());
        wbytes(&mut b, self.eph_pk.as_bytes());
        wbytes(&mut b, self.identity.ed.verifying_key().as_bytes());
        wbytes(&mut b, &self.identity.x25519_sig());
        wbytes(&mut b, &self.kex_sig().to_bytes());
        b.push(self.auth.mode() as u8);
        wbytes(&mut b, auth_blob);
        b.extend_from_slice(&self.clock_offset_ms.to_be_bytes());
        b
    }

    /// Ed25519 signature binding our eph KEX key to our device identity.
    fn kex_sig(&self) -> Signature {
        let mut m = Vec::with_capacity(40);
        m.extend_from_slice(self.eph_pk.as_bytes());
        m.extend_from_slice(&self.device_id.to_be_bytes());
        self.identity.ed.sign(&m)
    }

    fn ready_sig_msg(th: &[u8; 32]) -> Vec<u8> {
        let mut m = Vec::with_capacity(READY_SIG_TAG.len() + 32);
        m.extend_from_slice(READY_SIG_TAG);
        m.extend_from_slice(th);
        m
    }

    // ---------------- validation ----------------

    /// Parse a HELLO body and check its binding signatures and the caller's pins.
    fn parse_hello<'b>(&self, body: &'b [u8]) -> Result<Hello<'b>, HandshakeError> {
        let mut r = Rd::new(body);
        let version = r.u8("HELLO version")?;
        if version != PROTOCOL_VERSION {
            return Err(HandshakeError::IncompatibleVersion(format!(
                "peer speaks handshake v{version}, we speak v{PROTOCOL_VERSION} — update both apps"
            )));
        }
        let device_id = u64::from_be_bytes(r.array::<8>("HELLO device id")?);
        let static_pk = r.lp_array::<32>("bad key sizes in HELLO (static)")?;
        let eph_pk = r.lp_array::<32>("bad key sizes in HELLO (eph)")?;
        let ed_pub = r.lp_array::<32>("bad key sizes in HELLO (ed25519)")?;
        let static_sig = r.lp_array::<64>("bad static sig")?;
        let kex_sig = r.lp_array::<64>("bad kex sig")?;
        let mode = AuthMode::from_id(r.u8("HELLO auth mode")?)?;
        let auth_blob = r.lp("HELLO auth blob")?;
        let clock = i64::from_be_bytes(r.array::<8>("truncated HELLO clock field")?);
        r.finish("HELLO")?;

        let ed = VerifyingKey::from_bytes(&ed_pub)
            .map_err(|_| HandshakeError::BadSignature("bad Ed25519 pubkey".into()))?;
        // static_sig: Ed(ed_sk, x25519_static_pk) — binds the KEX key to the identity.
        ed.verify_strict(&static_pk, &Signature::from_bytes(&static_sig))
            .map_err(|_| HandshakeError::BadSignature("static key signature invalid".into()))?;
        // kex_sig: Ed(ed_sk, eph_pk || device_id) — authenticates this run's eph key.
        let mut km = Vec::with_capacity(40);
        km.extend_from_slice(&eph_pk);
        km.extend_from_slice(&device_id.to_be_bytes());
        ed.verify_strict(&km, &Signature::from_bytes(&kex_sig))
            .map_err(|_| HandshakeError::BadSignature("eph key signature invalid".into()))?;

        if let Some(fp) = self.expected_fingerprint {
            if sha256_fp(&ed) != fp {
                return Err(HandshakeError::PeerMismatch(
                    "fingerprint pin mismatch".into(),
                ));
            }
        }
        if let Some(d) = self.expected_device {
            if device_id != d {
                return Err(HandshakeError::PeerMismatch(format!(
                    "device ID mismatch (want {d})"
                )));
            }
        }
        Ok(Hello {
            device_id,
            static_pk: X25519PublicKey::from(static_pk),
            eph_pk: X25519PublicKey::from(eph_pk),
            ed,
            mode,
            auth_blob,
            clock_offset_ms: clock,
        })
    }

    fn store_peer(&mut self, h: &Hello<'_>, body: &[u8]) {
        self.peer_ed = Some(h.ed);
        self.peer_hello = body.to_vec();
        self.peer = Some(PeerInfo {
            device_id: h.device_id,
            fingerprint: sha256_fp(&h.ed),
            auth_mode: h.mode,
            clock_offset_ms: h.clock_offset_ms,
            x25519_static_pk: h.static_pk,
            x25519_eph_pk: h.eph_pk,
        });
    }

    fn check_mode(&self, peer_mode: AuthMode) -> Result<(), HandshakeError> {
        let mine = self.auth.mode();
        if peer_mode == mine {
            Ok(())
        } else {
            Err(HandshakeError::AuthModeMismatch(format!(
                "auth mode mismatch (peer {peer_mode:?}, ours {mine:?})"
            )))
        }
    }

    // ---------------- client side ----------------

    fn client_on_hello(&mut self, body: &[u8]) -> Result<(), HandshakeError> {
        if self.peer.is_some() {
            return Err(HandshakeError::Unexpected("second server HELLO".into()));
        }
        let h = self.parse_hello(body)?;
        self.check_mode(h.mode)?;
        if h.mode.is_pake() {
            let pake = self
                .pake
                .take()
                .ok_or_else(|| HandshakeError::Unexpected("PAKE already finished".into()))?;
            if h.auth_blob.len() != PAKE_MSG_LEN {
                return Err(malformed("bad PAKE message length"));
            }
            let key = pake
                .finish(h.auth_blob)
                .map_err(|e| HandshakeError::Malformed(format!("bad PAKE message: {e}")))?;
            self.pake_key = Some(Zeroizing::new(key));
        }
        self.store_peer(&h, body);
        Ok(()) // wait for READY
    }

    fn client_on_ready(&mut self, body: &[u8]) -> Result<(), HandshakeError> {
        let peer_dev = self
            .peer
            .as_ref()
            .ok_or_else(|| HandshakeError::Unexpected("READY before HELLO".into()))?
            .device_id;
        let mut r = Rd::new(body);
        let dev = u64::from_be_bytes(r.array::<8>("READY device id")?);
        let sig = r.lp_array::<64>("bad READY sig")?;
        let confirm = r.lp("READY confirm")?;
        r.finish("READY")?;
        if dev != peer_dev {
            return Err(HandshakeError::PeerMismatch(
                "READY device ID differs from HELLO".into(),
            ));
        }
        let th = transcript_hash(&self.our_hello, &self.peer_hello);
        // ready_sig = Ed(ed_server, "ov3 ready" || TH): the server signed the
        // whole transcript, including our HELLO.
        let ed = self
            .peer_ed
            .ok_or_else(|| HandshakeError::Unexpected("missing peer ed pubkey".into()))?;
        ed.verify_strict(&Self::ready_sig_msg(&th), &Signature::from_bytes(&sig))
            .map_err(|_| HandshakeError::BadSignature("READY signature invalid".into()))?;

        let hk = self.derive(&th)?;
        let mode = self.auth.mode();
        if mode.is_pake() {
            let ok = confirm.len() == MAC_LEN
                && finished_mac(&hk.server_finished, SERVER_FINISHED_TAG, &th)
                    .verify_slice(confirm)
                    .is_ok();
            if !ok {
                // Tell the host so it fails now instead of timing out.
                self.push_reject(RejectCode::AuthFailed, 0);
                return Err(HandshakeError::AuthFailed(format!(
                    "wrong {} (server key confirmation did not verify)",
                    mode.credential()
                )));
            }
            let mac = finished_mac(&hk.client_finished, CLIENT_FINISHED_TAG, &th)
                .finalize()
                .into_bytes();
            let mut b = Vec::with_capacity(2 + MAC_LEN);
            wbytes(&mut b, &mac);
            self.out
                .push(HandshakeEvent::Sent(datagram(TAG_CONFIRM, &b)));
        } else if !confirm.is_empty() {
            return Err(malformed("confirm MAC outside a PAKE mode"));
        }
        self.session = Some(self.build_session(&hk.keys));
        self.state = State::Established;
        Ok(())
    }

    // ---------------- server side ----------------

    fn server_on_hello(&mut self, body: &[u8]) -> Result<(), HandshakeError> {
        if let State::AwaitConfirm(p) = &self.state {
            // A retransmitted client HELLO: repeat our answer, nothing else.
            if body == self.peer_hello.as_slice() {
                let resend = p.response.clone();
                self.out
                    .extend(resend.into_iter().map(HandshakeEvent::Sent));
                return Ok(());
            }
            return Err(HandshakeError::Unexpected(
                "second, different client HELLO".into(),
            ));
        }
        match self.server_on_hello_inner(body) {
            Ok(()) => Ok(()),
            Err(e) => {
                if let Some(code) = e.reject_code() {
                    let retry = match &e {
                        HandshakeError::RateLimited { retry_after_secs } => *retry_after_secs,
                        _ => 0,
                    };
                    self.push_reject(code, retry);
                }
                Err(e)
            }
        }
    }

    fn server_on_hello_inner(&mut self, body: &[u8]) -> Result<(), HandshakeError> {
        let h = self.parse_hello(body)?;
        self.check_mode(h.mode)?;
        let blob = match self.auth.pake_params() {
            Some((password, id_client, id_server)) => {
                let throttle = self.throttle.clone().unwrap_or_else(AuthThrottle::global);
                let now = Instant::now();
                if let Err(wait) = throttle.check(self.auth_source, now) {
                    let retry_after_secs =
                        u32::try_from(wait.as_secs() + u64::from(wait.subsec_nanos() > 0))
                            .unwrap_or(u32::MAX);
                    tracing::warn!(
                        source = ?self.auth_source,
                        retry_after_secs,
                        "{} attempt refused: too many failures",
                        h.mode.credential()
                    );
                    return Err(HandshakeError::RateLimited { retry_after_secs });
                }
                if h.auth_blob.len() != PAKE_MSG_LEN {
                    return Err(malformed("bad PAKE message length"));
                }
                let (state, msg) = Spake2::<Ed25519Group>::start_b(
                    &Password::new(password),
                    &PakeIdentity::new(id_client),
                    &PakeIdentity::new(id_server),
                );
                let key = state
                    .finish(h.auth_blob)
                    .map_err(|e| HandshakeError::Malformed(format!("bad PAKE message: {e}")))?;
                self.pake_key = Some(Zeroizing::new(key));
                // A failure unless the client's CONFIRM verifies.
                self.attempt = Some(throttle.begin(self.auth_source));
                msg
            }
            None => Vec::new(),
        };
        self.store_peer(&h, body);
        self.our_hello = self.encode_hello_body(&blob);
        let th = transcript_hash(&self.peer_hello, &self.our_hello);
        let hk = self.derive(&th)?;

        let confirm = if self.auth.mode().is_pake() {
            finished_mac(&hk.server_finished, SERVER_FINISHED_TAG, &th)
                .finalize()
                .into_bytes()
                .to_vec()
        } else {
            Vec::new()
        };
        let sig = self.identity.ed.sign(&Self::ready_sig_msg(&th));
        let mut ready = Vec::with_capacity(8 + 2 + 64 + 2 + MAC_LEN);
        ready.extend_from_slice(&self.device_id.to_be_bytes());
        wbytes(&mut ready, &sig.to_bytes());
        wbytes(&mut ready, &confirm);

        // Our HELLO and READY as two datagrams (spec §3).
        let response = vec![
            datagram(TAG_HELLO, &self.our_hello),
            datagram(TAG_READY, &ready),
        ];
        self.out
            .extend(response.iter().cloned().map(HandshakeEvent::Sent));
        let session = self.build_session(&hk.keys.swapped());
        if self.auth.mode().is_pake() {
            self.state = State::AwaitConfirm(Box::new(PendingConfirm {
                session,
                client_finished: hk.client_finished,
                transcript: th,
                response,
            }));
        } else {
            self.session = Some(session);
            self.state = State::Established;
        }
        Ok(())
    }

    fn server_on_confirm(&mut self, body: &[u8]) -> Result<(), HandshakeError> {
        let State::AwaitConfirm(p) = std::mem::replace(&mut self.state, State::AwaitHello) else {
            return Err(HandshakeError::Unexpected("CONFIRM before HELLO".into()));
        };
        let mut r = Rd::new(body);
        let mac = r.lp("CONFIRM mac")?;
        r.finish("CONFIRM")?;
        let ok = mac.len() == MAC_LEN
            && finished_mac(&p.client_finished, CLIENT_FINISHED_TAG, &p.transcript)
                .verify_slice(mac)
                .is_ok();
        if !ok {
            return Err(HandshakeError::AuthFailed(format!(
                "wrong {} (client key confirmation did not verify)",
                self.auth.mode().credential()
            )));
        }
        if let Some(attempt) = self.attempt.take() {
            attempt.succeed();
        }
        self.pake_key = None;
        self.session = Some(p.session);
        self.state = State::Established;
        Ok(())
    }

    // ---------------- key derivation ----------------

    /// Run both X25519 exchanges and the v3 key schedule over `th`.
    fn derive(&mut self, th: &[u8; 32]) -> Result<crate::session::HandshakeKeys, HandshakeError> {
        let peer = self
            .peer
            .as_ref()
            .ok_or_else(|| HandshakeError::Unexpected("no peer state".into()))?;
        let eph = self
            .eph
            .take()
            .ok_or_else(|| HandshakeError::Unexpected("ephemeral key already consumed".into()))?;
        let eph_dh = eph.diffie_hellman(&peer.x25519_eph_pk);
        let static_dh = self
            .identity
            .x25519_sk
            .diffie_hellman(&peer.x25519_static_pk);
        if !eph_dh.was_contributory() || !static_dh.was_contributory() {
            return Err(HandshakeError::BadSignature(
                "non-contributory X25519 key".into(),
            ));
        }
        let mut ikm = Zeroizing::new(Vec::with_capacity(96));
        ikm.extend_from_slice(eph_dh.as_bytes());
        ikm.extend_from_slice(static_dh.as_bytes());
        if self.auth.mode().is_pake() {
            let k = self
                .pake_key
                .as_ref()
                .ok_or_else(|| HandshakeError::Unexpected("PAKE mode without a PAKE key".into()))?;
            ikm.extend_from_slice(k);
        }
        Ok(KeySchedule::derive_v3(&ikm, th))
    }

    /// Session from client-view keys swapped into OUR view (`encrypt` = what we send).
    fn build_session(&self, ours: &SessionKeys) -> Session {
        let peer_dev = self.peer.as_ref().map_or(0, |p| p.device_id);
        let send_base = nonce_base(ours.encrypt_key(), self.device_id, peer_dev);
        let recv_base = nonce_base(ours.decrypt_key(), peer_dev, self.device_id);
        Session::established(
            self.role == Role::Server,
            ours.clone(),
            send_base,
            recv_base,
            self.device_id,
            peer_dev,
        )
    }
}

/// Per-direction nonce base: HMAC-SHA256(dir_key, sender_dev_BE8 || receiver_dev_BE8)[..12].
fn nonce_base(key: &[u8; 32], sender_dev: u64, receiver_dev: u64) -> [u8; 12] {
    let mut m = [0u8; 16];
    m[..8].copy_from_slice(&sender_dev.to_be_bytes());
    m[8..].copy_from_slice(&receiver_dev.to_be_bytes());
    let h = crypto::hmac_sha256(key, &m);
    let mut b = [0u8; 12];
    b.copy_from_slice(&h[..12]);
    b
}

// ---------------- tests ----------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::throttle::ThrottlePolicy;
    use std::time::Duration;

    /// Unidirectional pipe: datagrams are appended, then delivered in 13-byte
    /// `feed()` calls so message boundaries are routinely split mid-flight.
    struct Pipe(Vec<u8>);
    impl Pipe {
        fn new() -> Self {
            Self(Vec::new())
        }
        fn push(&mut self, data: &[u8]) {
            self.0.extend_from_slice(data);
        }
        fn deliver(&mut self, hs: &mut Handshake) -> Result<(), HandshakeError> {
            let data = std::mem::take(&mut self.0);
            let mut off = 0;
            while off < data.len() {
                let end = (off + 13).min(data.len());
                hs.feed(&data[off..end])?;
                off = end;
            }
            Ok(())
        }
    }

    fn sent(hs: &mut Handshake, what: &str) -> Vec<u8> {
        let evs = hs.take_events();
        evs.into_iter()
            .map(|e| match e {
                HandshakeEvent::Sent(d) => d,
            })
            .next()
            .unwrap_or_else(|| panic!("{what} emitted no datagram"))
    }

    fn all_sent(hs: &mut Handshake) -> Vec<u8> {
        hs.take_events()
            .into_iter()
            .flat_map(|e| match e {
                HandshakeEvent::Sent(d) => d,
            })
            .collect()
    }

    fn datagrams(hs: &mut Handshake) -> Vec<Vec<u8>> {
        hs.take_events()
            .into_iter()
            .map(|e| match e {
                HandshakeEvent::Sent(d) => d,
            })
            .collect()
    }

    fn private_throttle() -> Arc<AuthThrottle> {
        Arc::new(AuthThrottle::default())
    }

    fn run_pairing() -> (Handshake, Handshake) {
        let client_id = Identity::generate();
        let server_id = Identity::generate();
        let (dev_c, dev_s) = (42424242u64, 987654321u64);

        let server_code = crate::pairing::generate();
        let mut client = Handshake::new(
            Role::Client,
            dev_c,
            &client_id,
            Auth::PairingCode(server_code.clone()),
        );
        client.expect_fingerprint(server_id.fingerprint());
        let mut server = Handshake::new(
            Role::Server,
            dev_s,
            &server_id,
            Auth::PairingCode(server_code),
        );
        server.expect_device(dev_c);
        server.set_throttle(private_throttle());

        run_pin(&mut client, &mut server).expect("pairing handshake");
        assert!(server.is_established());
        assert!(client.is_established());
        (client, server)
    }

    /// Every datagram of a full PAKE handshake, both directions.
    fn pake_transcript(client: &mut Handshake, server: &mut Handshake) -> Vec<u8> {
        let mut wire = Vec::new();
        let hello = all_sent(client);
        wire.extend_from_slice(&hello);
        let _ = server.feed(&hello);
        let resp = all_sent(server);
        wire.extend_from_slice(&resp);
        let _ = client.feed(&resp);
        let last = all_sent(client);
        wire.extend_from_slice(&last);
        let _ = server.feed(&last);
        wire.extend_from_slice(&all_sent(server));
        wire
    }

    fn pairing_pair(client_code: &str, server_code: &str) -> (Handshake, Handshake) {
        let client = Handshake::new(
            Role::Client,
            11,
            &Identity::generate(),
            Auth::PairingCode(client_code.into()),
        );
        let mut server = Handshake::new(
            Role::Server,
            22,
            &Identity::generate(),
            Auth::PairingCode(server_code.into()),
        );
        server.set_throttle(private_throttle());
        (client, server)
    }

    #[test]
    fn pairing_code_never_appears_on_the_wire() {
        let code = crate::pairing::generate();
        let (mut c, mut s) = pairing_pair(&code, &code);
        let wire = pake_transcript(&mut c, &mut s);
        assert!(c.is_established() && s.is_established());
        assert!(
            !wire.windows(code.len()).any(|w| w == code.as_bytes()),
            "the code must not travel in cleartext"
        );
        // Not even its secret part.
        let secret = &code.as_bytes()[crate::pairing::RENDEZVOUS_DIGITS..];
        assert!(!wire.windows(secret.len()).any(|w| w == secret));
        assert_records_flow(&mut c, &mut s);
    }

    #[test]
    fn pairing_server_waits_for_confirm() {
        let code = crate::pairing::generate();
        let (mut c, mut s) = pairing_pair(&code, &code);
        s.feed(&all_sent(&mut c)).unwrap();
        assert!(!s.is_established(), "pairing is a PAKE mode now");
        c.feed(&all_sent(&mut s)).unwrap();
        assert!(c.is_established());
        let confirm = sent(&mut c, "CONFIRM");
        assert_eq!(confirm[0], TAG_CONFIRM);
        s.feed(&confirm).unwrap();
        assert!(s.is_established());
    }

    #[test]
    fn wrong_pairing_code_fails_on_both_sides_and_burns_the_attempt() {
        let (mut c, mut s) = pairing_pair("1234000000", "1234999999");
        let _ = pake_transcript(&mut c, &mut s);
        assert!(c.error().unwrap().is_auth_failure(), "{:?}", c.failed());
        assert!(c.failed().unwrap().contains("pairing code"));
        assert!(s.error().unwrap().is_auth_failure(), "{:?}", s.failed());
        // The host's handshake for this code is dead: even the right code
        // cannot complete on it (the host then generates a new code).
        let (mut retry, _) = pairing_pair("1234999999", "1234999999");
        let err = s.feed(&all_sent(&mut retry)).unwrap_err();
        assert!(err.is_auth_failure(), "{err}");
        assert!(!s.is_established());
    }

    #[test]
    fn pin_and_pairing_code_are_not_interchangeable() {
        // Even with the same 32 password bytes, the SPAKE2 identities differ.
        let code = "1".repeat(32);
        let mut psk = [0u8; 32];
        psk.copy_from_slice(code.as_bytes());
        let mut c = Handshake::new(
            Role::Client,
            1,
            &Identity::generate(),
            Auth::PairingCode(code.clone()),
        );
        let mut s = Handshake::new(Role::Server, 2, &Identity::generate(), Auth::Pin(psk));
        s.set_throttle(private_throttle());
        let err = s.feed(&all_sent(&mut c)).unwrap_err();
        assert!(matches!(err, HandshakeError::AuthModeMismatch(_)), "{err}");
        // And below the mode byte: SPAKE2 itself keys on the identities.
        let pw = Password::new(code.as_bytes());
        let (a, ma) = Spake2::<Ed25519Group>::start_a(
            &pw,
            &PakeIdentity::new(PAIR_ID_CLIENT),
            &PakeIdentity::new(PAIR_ID_SERVER),
        );
        let (b, mb) = Spake2::<Ed25519Group>::start_b(
            &pw,
            &PakeIdentity::new(PIN_ID_CLIENT),
            &PakeIdentity::new(PIN_ID_SERVER),
        );
        assert_ne!(a.finish(&mb).unwrap(), b.finish(&ma).unwrap());
    }

    #[test]
    fn pairing_mode_is_throttled_too() {
        let throttle = Arc::new(AuthThrottle::new(
            ThrottlePolicy::PER_SOURCE,
            ThrottlePolicy {
                threshold: 1,
                ..ThrottlePolicy::GLOBAL
            },
        ));
        let (mut c, mut s) = pairing_pair("1111111111", "2222222222");
        s.set_throttle(throttle.clone());
        let _ = pake_transcript(&mut c, &mut s);
        let (mut c, mut s) = pairing_pair("2222222222", "2222222222");
        s.set_throttle(throttle);
        let _ = pake_transcript(&mut c, &mut s);
        assert!(c.error().unwrap().is_rate_limited(), "{:?}", c.failed());
    }

    fn assert_records_flow(client: &mut Handshake, server: &mut Handshake) {
        let rec = client
            .session_mut()
            .unwrap()
            .encrypt_record(0, b"hello server");
        assert_eq!(
            server
                .session_mut()
                .unwrap()
                .decrypt_record(0, &rec)
                .expect("server decrypts"),
            b"hello server"
        );
        let rec = server
            .session_mut()
            .unwrap()
            .encrypt_record(1, b"hello client");
        assert_eq!(
            client
                .session_mut()
                .unwrap()
                .decrypt_record(1, &rec)
                .expect("client decrypts"),
            b"hello client"
        );
    }

    #[test]
    fn end_to_end_pairing_then_records() {
        let (mut client, mut server) = run_pairing();
        assert_eq!(client.session().unwrap().sender_id(), 42424242);
        assert_eq!(client.session().unwrap().target_id(), 987654321);
        assert_eq!(server.session().unwrap().sender_id(), 987654321);
        assert_eq!(server.session().unwrap().target_id(), 42424242);
        assert!(server.session().unwrap().role_is_server());
        assert_records_flow(&mut client, &mut server);

        let peer = client.peer().unwrap();
        assert_eq!(peer.device_id, 987654321);
        assert_eq!(peer.fingerprint, server.identity().fingerprint());
    }

    /// `pin_psk` is deliberately slow (PBKDF2); memoize it for the tests.
    fn psk(pin: &str) -> [u8; 32] {
        use std::cell::RefCell;
        use std::collections::HashMap;
        thread_local! {
            static CACHE: RefCell<HashMap<String, [u8; 32]>> = RefCell::new(HashMap::new());
        }
        CACHE.with(|c| {
            *c.borrow_mut()
                .entry(pin.to_owned())
                .or_insert_with(|| crate::pin::pin_psk(pin.as_bytes()))
        })
    }

    /// Client + server in PIN mode, the server on a private throttle.
    fn pin_pair(client_pin: &str, server_pin: &str) -> (Handshake, Handshake) {
        let client_id = Identity::generate();
        let server_id = Identity::generate();
        let client = Handshake::new(Role::Client, 111, &client_id, Auth::Pin(psk(client_pin)));
        let mut server = Handshake::new(Role::Server, 222, &server_id, Auth::Pin(psk(server_pin)));
        server.set_throttle(private_throttle());
        (client, server)
    }

    /// Run all three PIN flights through split pipes.
    fn run_pin(client: &mut Handshake, server: &mut Handshake) -> Result<(), HandshakeError> {
        let mut p_cs = Pipe::new();
        let mut p_sc = Pipe::new();
        p_cs.push(&all_sent(client));
        p_cs.deliver(server)?;
        p_sc.push(&all_sent(server));
        let r = p_sc.deliver(client);
        // Deliver the client's CONFIRM (or REJECT) either way.
        p_cs.push(&all_sent(client));
        let r2 = p_cs.deliver(server);
        r.and(r2)
    }

    #[test]
    fn end_to_end_pin_then_records() {
        let (mut client, mut server) = pin_pair("7319", "7319");
        client.expect_device(222);
        server.expect_device(111);
        // After the client's HELLO the server waits for CONFIRM.
        p_feed(&mut client, &mut server);
        assert!(!server.is_established(), "PIN server waits for CONFIRM");
        let mut back = Pipe::new();
        back.push(&all_sent(&mut server));
        back.deliver(&mut client).expect("client verifies server");
        assert!(client.is_established());
        let mut fwd = Pipe::new();
        fwd.push(&all_sent(&mut client)); // CONFIRM
        fwd.deliver(&mut server).expect("server verifies client");
        assert!(server.is_established());
        assert_records_flow(&mut client, &mut server);
    }

    fn p_feed(client: &mut Handshake, server: &mut Handshake) {
        let mut p = Pipe::new();
        p.push(&all_sent(client));
        p.deliver(server).expect("server processes client hello");
    }

    #[test]
    fn wrong_pin_fails_cleanly_on_both_sides() {
        let (mut client, mut server) = pin_pair("0000", "9999");
        let err = run_pin(&mut client, &mut server).unwrap_err();
        assert!(err.is_auth_failure(), "got: {err}");
        assert!(client.error().unwrap().is_auth_failure());
        assert!(client.failed().unwrap().contains("PIN"));
        assert!(
            server.error().is_some_and(HandshakeError::is_auth_failure),
            "server learns of the failure via REJECT: {:?}",
            server.failed()
        );
        assert!(!client.is_established() && !server.is_established());
        assert!(client.session().is_none() && server.session().is_none());
    }

    #[test]
    fn pake_messages_are_fresh_per_attempt() {
        // Same PIN, two attempts: the PIN-bearing blob differs (no fixed
        // PIN-derived value an observer could replay or test offline).
        let blob = || {
            let (mut c, _) = pin_pair("1234", "1234");
            sent(&mut c, "client")
        };
        let (a, b) = (blob(), blob());
        assert_eq!(a.len(), b.len());
        assert_ne!(
            a[a.len() - 8 - PAKE_MSG_LEN..],
            b[b.len() - 8 - PAKE_MSG_LEN..]
        );
    }

    #[test]
    fn rate_limiting_locks_after_failures_and_reports_retry_after() {
        let throttle = Arc::new(AuthThrottle::new(
            ThrottlePolicy::PER_SOURCE,
            ThrottlePolicy {
                threshold: 2,
                base_lock: Duration::from_secs(30),
                max_lock: Duration::from_secs(900),
                forget_after: Duration::from_secs(3600),
            },
        ));
        for _ in 0..2 {
            let (mut c, mut s) = pin_pair("1111", "2222");
            s.set_throttle(throttle.clone());
            assert!(run_pin(&mut c, &mut s).unwrap_err().is_auth_failure());
        }
        // Locked now — even the right PIN is refused without running the PAKE.
        let (mut c, mut s) = pin_pair("2222", "2222");
        s.set_throttle(throttle.clone());
        let mut p = Pipe::new();
        p.push(&all_sent(&mut c));
        let err = p.deliver(&mut s).unwrap_err();
        assert!(err.is_rate_limited(), "{err}");
        let mut back = Pipe::new();
        back.push(&all_sent(&mut s));
        let err = back.deliver(&mut c).unwrap_err();
        match err {
            HandshakeError::RateLimited { retry_after_secs } => {
                assert!((1..=30).contains(&retry_after_secs), "{retry_after_secs}");
            }
            other => panic!("expected RateLimited, got {other}"),
        }
        assert!(c.failed().unwrap().contains("too many"));
    }

    #[test]
    fn per_source_limit_applies_when_the_address_is_known() {
        let throttle = Arc::new(AuthThrottle::new(
            ThrottlePolicy {
                threshold: 1,
                ..ThrottlePolicy::PER_SOURCE
            },
            ThrottlePolicy::GLOBAL,
        ));
        let bad: IpAddr = [192, 0, 2, 1].into();
        let good: IpAddr = [192, 0, 2, 2].into();
        let (mut c, mut s) = pin_pair("1", "2");
        s.set_throttle(throttle.clone());
        s.set_auth_source(bad);
        assert!(run_pin(&mut c, &mut s).is_err());
        let (mut c, mut s) = pin_pair("2", "2");
        s.set_throttle(throttle.clone());
        s.set_auth_source(bad);
        assert!(run_pin(&mut c, &mut s).unwrap_err().is_rate_limited());
        let (mut c, mut s) = pin_pair("2", "2");
        s.set_throttle(throttle);
        s.set_auth_source(good);
        run_pin(&mut c, &mut s).expect("other source unaffected");
        assert!(c.is_established() && s.is_established());
    }

    #[test]
    fn successful_logins_do_not_accumulate_lockout() {
        let throttle = Arc::new(AuthThrottle::new(
            ThrottlePolicy::PER_SOURCE,
            ThrottlePolicy {
                threshold: 2,
                ..ThrottlePolicy::GLOBAL
            },
        ));
        for _ in 0..5 {
            let (mut c, mut s) = pin_pair("42", "42");
            s.set_throttle(throttle.clone());
            run_pin(&mut c, &mut s).expect("correct PIN");
        }
    }

    #[test]
    fn transcript_tampering_is_detected_in_pin_mode() {
        // The clock field is not covered by any Ed25519 signature; only the
        // transcript binds it. Flip it in flight: key confirmation must fail.
        let (mut c, mut s) = pin_pair("5555", "5555");
        let mut hello = sent(&mut c, "client");
        let last = hello.len() - 1;
        hello[last] ^= 0x01;
        s.feed(&hello).expect("server cannot tell yet");
        let mut back = Pipe::new();
        back.push(&all_sent(&mut s));
        let err = back.deliver(&mut c).unwrap_err();
        assert!(matches!(err, HandshakeError::BadSignature(_)), "{err}");
        assert!(!c.is_established());
    }

    #[test]
    fn transcript_tampering_is_detected_without_pin() {
        let (cid, sid) = (Identity::generate(), Identity::generate());
        let mut c = Handshake::new(Role::Client, 1, &cid, Auth::DirectDevice);
        let mut s = Handshake::new(Role::Server, 2, &sid, Auth::DirectDevice);
        let mut hello = sent(&mut c, "client");
        let last = hello.len() - 1;
        hello[last] ^= 0x80;
        s.feed(&hello).unwrap();
        let err = c.feed(&all_sent(&mut s)).unwrap_err();
        assert!(err.to_string().contains("READY signature"), "{err}");
    }

    #[test]
    fn tampered_server_confirmation_fails_with_auth_error() {
        let (mut c, mut s) = pin_pair("8080", "8080");
        s.feed(&sent(&mut c, "client")).unwrap();
        let mut dgs = datagrams(&mut s);
        let ready = dgs.last_mut().unwrap();
        let last = ready.len() - 1;
        ready[last] ^= 0x01; // last byte of the server finished MAC
        c.feed(&dgs.concat()).unwrap_err();
        assert!(c.error().unwrap().is_auth_failure());
        // The REJECT reaches the server, which fails promptly.
        let _ = s.feed(&all_sent(&mut c));
        assert!(s.error().unwrap().is_auth_failure());
    }

    #[test]
    fn tampered_client_confirmation_fails_with_auth_error() {
        let (mut c, mut s) = pin_pair("8080", "8080");
        s.feed(&sent(&mut c, "client")).unwrap();
        c.feed(&all_sent(&mut s)).unwrap();
        assert!(c.is_established());
        let mut confirm = sent(&mut c, "client confirm");
        assert_eq!(confirm[0], TAG_CONFIRM);
        let last = confirm.len() - 1;
        confirm[last] ^= 0x01;
        let err = s.feed(&confirm).unwrap_err();
        assert!(err.is_auth_failure(), "{err}");
        assert!(!s.is_established() && s.session().is_none());
    }

    #[test]
    fn duplicate_client_hello_resends_the_response() {
        let (mut c, mut s) = pin_pair("1", "1");
        let hello = sent(&mut c, "client");
        s.feed(&hello).unwrap();
        let first = all_sent(&mut s);
        s.feed(&hello).unwrap();
        let again = all_sent(&mut s);
        assert_eq!(first, again);
        c.feed(&again).unwrap();
        s.feed(&all_sent(&mut c)).unwrap();
        assert!(c.is_established() && s.is_established());
    }

    #[test]
    fn legacy_v1_hello_gets_incompatible_version() {
        let sid = Identity::generate();
        let mut s = Handshake::new(Role::Server, 2, &sid, Auth::DirectDevice);
        // A v1 HELLO: tag 0x01, body starting with the device id.
        let mut v1 = vec![LEGACY_TAG_HELLO, 0, 8];
        v1.extend_from_slice(&7u64.to_be_bytes());
        let err = s.feed(&v1).unwrap_err();
        assert!(
            matches!(err, HandshakeError::IncompatibleVersion(_)),
            "{err}"
        );
        let reject = sent(&mut s, "server reject");
        assert_eq!(reject[0], TAG_REJECT);
        assert_eq!(reject[3], RejectCode::IncompatibleVersion as u8);
        // A v3 client receiving that REJECT reports the version problem.
        let cid = Identity::generate();
        let mut c = Handshake::new(Role::Client, 1, &cid, Auth::DirectDevice);
        let _ = c.take_events();
        let err = c.feed(&reject).unwrap_err();
        assert!(
            matches!(err, HandshakeError::IncompatibleVersion(_)),
            "{err}"
        );
        assert!(err.to_string().contains("update both apps"), "{err}");
    }

    #[test]
    fn v2_hello_gets_incompatible_version() {
        let (cid, sid) = (Identity::generate(), Identity::generate());
        let mut c = Handshake::new(Role::Client, 1, &cid, Auth::DirectDevice);
        let mut s = Handshake::new(Role::Server, 2, &sid, Auth::DirectDevice);
        let mut hello = sent(&mut c, "client");
        hello[3] = 2; // a v2 peer: same tags, older version byte
        let err = s.feed(&hello).unwrap_err();
        assert!(
            matches!(err, HandshakeError::IncompatibleVersion(_)),
            "{err}"
        );
        let err = c.feed(&sent(&mut s, "reject")).unwrap_err();
        assert!(
            matches!(err, HandshakeError::IncompatibleVersion(_)),
            "{err}"
        );
    }

    #[test]
    fn future_version_byte_is_incompatible() {
        let (cid, sid) = (Identity::generate(), Identity::generate());
        let mut c = Handshake::new(Role::Client, 1, &cid, Auth::DirectDevice);
        let mut s = Handshake::new(Role::Server, 2, &sid, Auth::DirectDevice);
        let mut hello = sent(&mut c, "client");
        hello[3] = PROTOCOL_VERSION + 1;
        let err = s.feed(&hello).unwrap_err();
        assert!(
            matches!(err, HandshakeError::IncompatibleVersion(_)),
            "{err}"
        );
        assert_eq!(sent(&mut s, "reject")[0], TAG_REJECT);
    }

    #[test]
    fn v1_server_traffic_is_incompatible_for_a_v2_client() {
        let cid = Identity::generate();
        let mut c = Handshake::new(Role::Client, 1, &cid, Auth::DirectDevice);
        let err = c.feed(&[LEGACY_TAG_HELLO, 0, 0]).unwrap_err();
        assert!(matches!(err, HandshakeError::IncompatibleVersion(_)));
    }

    #[test]
    fn pin_mode_rejects_non_pin_peer_mode() {
        // A client dialing with the pairing-code mode must not be accepted
        // by a PIN server (mode mismatch).
        let client_id = Identity::generate();
        let server_id = Identity::generate();
        let mut client = Handshake::new(
            Role::Client,
            1,
            &client_id,
            Auth::PairingCode(server_id.short_code()),
        );
        let mut server = Handshake::new(
            Role::Server,
            2,
            &server_id,
            Auth::Pin(crate::pin::pin_psk(b"1234")),
        );
        server.set_throttle(private_throttle());
        let err = server.feed(&sent(&mut client, "client")).unwrap_err();
        assert!(err.to_string().contains("auth mode"), "got: {err}");
        let err = client.feed(&all_sent(&mut server)).unwrap_err();
        assert!(matches!(err, HandshakeError::AuthModeMismatch(_)), "{err}");
    }

    #[test]
    fn end_to_end_direct_device() {
        let client_id = Identity::generate();
        let server_id = Identity::generate();
        let mut client = Handshake::new(Role::Client, 7, &client_id, Auth::DirectDevice);
        client.expect_fingerprint(server_id.fingerprint());
        let mut server = Handshake::new(Role::Server, 8, &server_id, Auth::DirectDevice);
        server.expect_device(7);

        let mut p_cs = Pipe::new();
        let mut p_sc = Pipe::new();
        p_cs.push(&sent(&mut client, "client"));
        p_cs.deliver(&mut server).unwrap();
        p_sc.push(&all_sent(&mut server));
        p_sc.deliver(&mut client).unwrap();
        assert!(client.is_established() && server.is_established());
        assert_records_flow(&mut client, &mut server);
    }

    #[test]
    fn pairing_code_mismatch_is_rejected() {
        let client_id = Identity::generate();
        let server_id = Identity::generate();
        let mut client = Handshake::new(
            Role::Client,
            1,
            &client_id,
            Auth::PairingCode("0000000000".into()),
        );
        let mut server = Handshake::new(
            Role::Server,
            2,
            &server_id,
            Auth::PairingCode(crate::pairing::generate()),
        );
        server.set_throttle(private_throttle());
        // The server cannot tell yet (no code on the wire); the client fails
        // on the server's key confirmation and tells the server.
        server.feed(&sent(&mut client, "client")).unwrap();
        let err = client.feed(&all_sent(&mut server)).unwrap_err();
        assert!(err.to_string().contains("pairing code"), "got: {err}");
        assert!(err.is_auth_failure(), "client is told: {err}");
        let err = server.feed(&all_sent(&mut client)).unwrap_err();
        assert!(err.is_auth_failure(), "{err}");
    }

    #[test]
    fn fingerprint_pin_mismatch_is_rejected() {
        let client_id = Identity::generate();
        let server_id = Identity::generate();
        let impostor = Identity::generate();
        // Client pins a fingerprint the real server will not present.
        let mut client = Handshake::new(Role::Client, 1, &client_id, Auth::DirectDevice);
        client.expect_fingerprint(impostor.fingerprint());
        let mut server = Handshake::new(Role::Server, 2, &server_id, Auth::DirectDevice);

        let mut p_cs = Pipe::new();
        let mut p_sc = Pipe::new();
        p_cs.push(&sent(&mut client, "client"));
        p_cs.deliver(&mut server).unwrap();
        p_sc.push(&all_sent(&mut server));
        let err = p_sc.deliver(&mut client).unwrap_err();
        assert!(err.to_string().contains("fingerprint pin"), "got: {err}");
        assert!(!client.is_established());
    }

    #[test]
    fn device_pin_mismatch_is_rejected() {
        let client_id = Identity::generate();
        let server_id = Identity::generate();
        let mut client = Handshake::new(Role::Client, 1, &client_id, Auth::DirectDevice);
        let mut server = Handshake::new(Role::Server, 2, &server_id, Auth::DirectDevice);
        server.expect_device(999); // wrong pin
        let hello = sent(&mut client, "client");
        let err = server.feed(&hello).unwrap_err();
        assert!(err.to_string().contains("device ID mismatch"), "got: {err}");
    }

    #[test]
    fn tampered_ephemeral_key_is_rejected() {
        let (client_id, server_id) = (Identity::generate(), Identity::generate());
        let mut client = Handshake::new(Role::Client, 1, &client_id, Auth::DirectDevice);
        let mut server = Handshake::new(Role::Server, 2, &server_id, Auth::DirectDevice);

        let mut p_cs = Pipe::new();
        p_cs.push(&sent(&mut client, "client"));
        p_cs.deliver(&mut server).unwrap();
        let hello_plus_ready = all_sent(&mut server);
        // Corrupt the first byte of the server's ephemeral pubkey inside its
        // HELLO: [tag 1][len 2][ver 1][dev 8][len 2][static 32][len 2] -> eph @ 48.
        let mut corrupted = hello_plus_ready.clone();
        corrupted[48] ^= 0x01;
        let err = client.feed(&corrupted).unwrap_err();
        assert!(err.to_string().contains("eph key signature"), "got: {err}");
        assert!(!client.is_established());
    }

    #[test]
    fn ready_before_hello_is_rejected() {
        let client_id = Identity::generate();
        let mut client = Handshake::new(Role::Client, 1, &client_id, Auth::DirectDevice);
        let _ = sent(&mut client, "client");
        let fake_ready = [
            TAG_READY, 0x00, 0x08, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0,
        ];
        let err = client.feed(&fake_ready).unwrap_err();
        assert!(err.to_string().contains("READY before HELLO"), "got: {err}");
    }

    #[test]
    fn auth_debug_is_redacted() {
        let a = Auth::Pin([0xAB; 32]);
        let s = format!("{a:?}");
        assert_eq!(s, "Pin(<redacted>)");
        assert!(!format!("{:?}", Auth::PairingCode("123456".into())).contains("123456"));
    }

    /// Deterministic xorshift for the smoke-fuzz below.
    fn xorshift(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    #[test]
    fn hostile_input_never_panics() {
        let mut rng = 0x9E37_79B9_7F4A_7C15u64;
        let (mut c0, _) = pin_pair("1", "1");
        let valid_hello = sent(&mut c0, "client");
        for i in 0..400 {
            let mut buf = if i % 2 == 0 {
                let n = (xorshift(&mut rng) % 400) as usize;
                (0..n).map(|_| xorshift(&mut rng) as u8).collect::<Vec<_>>()
            } else {
                let mut b = valid_hello.clone();
                for _ in 0..(1 + xorshift(&mut rng) % 4) {
                    let at = (xorshift(&mut rng) as usize) % b.len();
                    b[at] = xorshift(&mut rng) as u8;
                }
                b
            };
            if i % 3 == 0 && !buf.is_empty() {
                buf[0] = [TAG_HELLO, TAG_READY, TAG_CONFIRM, TAG_REJECT][i % 4];
            }
            let (mut c, mut s) = pin_pair("1", "1");
            let _ = s.feed(&buf);
            let _ = c.feed(&buf);
        }
    }
}
