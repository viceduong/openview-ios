//! OpenView C FFI — the iOS-facing surface over `ovcore` + `ovtransport`.
//!
//! Design notes (mirrors the wiring used by `ovview`/`ovhost`):
//! * `OvSession` is a `Mutex<Inner>`; all entry points lock it. Sinks (C
//!   function pointers) are invoked on whatever thread calls
//!   [`ov_session_pump`], never from a Rust-internal thread, so Swift can
//!   marshal them safely. Never re-enter the same session synchronously
//!   from a sink — buffer and call [`ov_session_pump`] again instead.
//! * Transport is pluggable: a datagram transport is owned by the host
//!   (Swift `NWConnection`); outbound datagrams are delivered through the
//!   datagram sink, inbound ones are pushed with [`ov_session_on_datagram`].
//!   A relay transport is owned by Rust (blocking TCP) via
//!   [`ov_session_relay_connect`].
//! * No entry point unwinds across the FFI boundary: pointer arguments are
//!   null-checked, fallible ops return negative error codes, and every
//!   `extern "C"` body runs inside [`ffi_guard!`], which turns a Rust panic
//!   into [`OV_ERR_PANIC`] (or a null / neutral return value).
//!
//! Error codes: 0 = success, -1 = null/invalid argument, -2 = wrong state,
//! -3 = authentication failed (wrong PIN / pairing code; the only code that
//! means "the credentials are wrong"), -4 = transport error, -5 = relay
//! closed (`ov_session_pump_relay` only; terminal), -6 = internal error (a
//! Rust panic was caught; the handle should be torn down), -7 = rate
//! limited (too many PIN attempts; see [`ov_session_retry_after_secs`]),
//! -8 = incompatible handshake version, -9 = any other handshake / protocol
//! failure (malformed message, bad signature, peer mismatch, ...).
//!
//! The C declarations live in `ios/OvFFI/include/shim.h`; the
//! `header_tests` module fails when the header and the exported symbols
//! drift apart.

use ovcore::handshake::{Auth, Handshake, HandshakeEvent, Role};
use ovcore::identity::Identity;
use ovcore::session::Session;
use ovtransport::{
    BridgeWriter, Carrier, CarrierStats, RelayTunnelCarrier, TransportError, TunnelRole,
};
use std::borrow::Cow;
use std::collections::VecDeque;
use std::ffi::{c_char, c_void, CStr};
use std::sync::{Mutex, MutexGuard};

/// Error code returned when an entry point caught a Rust panic. The handle
/// may be in an inconsistent state (a poisoned session mutex reports -2 from
/// then on); the shell should free it and start over.
pub const OV_ERR_PANIC: i32 = -6;

/// The peer rejected our credentials, or theirs failed key confirmation:
/// wrong PIN or pairing code. The only error after which a shell should
/// discard a stored PIN.
pub const OV_ERR_AUTH_FAILED: i32 = -3;
/// The host refuses PIN attempts for a while (online brute-force
/// protection); [`ov_session_retry_after_secs`] tells for how long.
pub const OV_ERR_RATE_LIMITED: i32 = -7;
/// The peer speaks an incompatible handshake version; both apps need an
/// update.
pub const OV_ERR_INCOMPATIBLE_VERSION: i32 = -8;
/// Any other handshake failure (malformed or unexpected message, bad
/// signature, peer or auth-mode mismatch). Not a credential problem.
pub const OV_ERR_HANDSHAKE: i32 = -9;

/// The FFI code for a failed handshake.
fn handshake_error_code(e: &ovcore::HandshakeError) -> i32 {
    if e.is_auth_failure() {
        OV_ERR_AUTH_FAILED
    } else if e.is_rate_limited() {
        OV_ERR_RATE_LIMITED
    } else if matches!(e, ovcore::HandshakeError::IncompatibleVersion(_)) {
        OV_ERR_INCOMPATIBLE_VERSION
    } else {
        OV_ERR_HANDSHAKE
    }
}

/// Whether `code` reports a handshake outcome (as opposed to transport or
/// usage errors).
fn is_handshake_code(code: i32) -> bool {
    matches!(
        code,
        OV_ERR_AUTH_FAILED | OV_ERR_RATE_LIMITED | OV_ERR_INCOMPATIBLE_VERSION | OV_ERR_HANDSHAKE
    )
}

/// Run the body of an `extern "C"` entry point, catching any panic so it
/// never unwinds into the C / Swift caller (undefined behaviour). On a panic
/// the entry point returns `$on_panic` instead: [`OV_ERR_PANIC`] for status
/// codes, null for constructors, a neutral value for queries.
///
/// The body runs inside a closure, so `return` inside it returns from the
/// entry point as expected. Unwind safety is asserted: after a panic the
/// only state that matters is the session mutex, which poisoning already
/// guards.
macro_rules! ffi_guard {
    ($on_panic:expr, $body:block) => {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| $body)) {
            Ok(value) => value,
            Err(_) => $on_panic,
        }
    };
}

// ---------------- callbacks ----------------

/// Outbound datagram sink (handshake + record datagrams). Called on the
/// `ov_session_pump` caller thread.
pub type DatagramSink = extern "C" fn(user: *mut c_void, data: *const u8, len: usize);
/// Decrypted inbound record sink (payload bytes; channel reported separately).
pub type RecordSink = extern "C" fn(user: *mut c_void, channel: u8, data: *const u8, len: usize);
/// State transition sink: `state = 1` means established; the peer's 16-byte
/// fingerprint follows (all-zero when unknown).
pub type StateSink = extern "C" fn(user: *mut c_void, state: i32, fp: *const u8, fp_len: usize);
/// Direct-path send sink: put `data` on the wire of path
/// `path_id` (the NWConnection the shell opened for that candidate).
pub type DirectSendSink =
    extern "C" fn(user: *mut c_void, path_id: u32, data: *const u8, len: usize);
/// Direct-path open sink: the host offered candidate `addr`
/// (NUL-terminated `"ip:port"` / `"[v6]:port"`); open a UDP NWConnection
/// to it, feed its datagrams with `ov_session_on_direct_datagram(path_id)`
/// and send what `DirectSendSink` hands out for `path_id`.
pub type DirectPathSink = extern "C" fn(user: *mut c_void, path_id: u32, addr: *const c_char);
/// Relay wake sink (event-driven receive): the Rust-owned relay socket has
/// inbound bytes — schedule an [`ov_session_pump_relay`] on the shell's
/// session thread. Fires on a Rust watcher thread (the one exception to
/// "sinks fire on the pump caller's thread"): it must only dispatch, never
/// call into the session synchronously or block. Wakes are coalesced: after
/// one wake the next fires only once a pump drained the socket again.
pub type WakeSink = extern "C" fn(user: *mut c_void);

#[derive(Clone, Copy)]
struct Sink<F> {
    f: Option<F>,
    user: *mut c_void,
}
impl<F> Sink<F> {
    fn none() -> Self {
        Self {
            f: None,
            user: core::ptr::null_mut(),
        }
    }
}
impl Sink<DatagramSink> {
    /// Hand one byte chunk to the host callback (a method, so closures
    /// capture the whole `Send` wrapper rather than its raw-pointer field).
    fn emit_bytes(&self, bytes: &[u8]) -> bool {
        if let Some(f) = self.f {
            f(self.user, bytes.as_ptr(), bytes.len());
        }
        true
    }
}
impl Sink<WakeSink> {
    /// Fire the wake callback (a method for the same capture reason).
    fn fire(&self) {
        if let Some(f) = self.f {
            f(self.user);
        }
    }
}
// The user pointer is only dereferenced by the host callback itself, on the
// pump-caller thread; moving the wrapper between threads is fine.
unsafe impl<F> Send for Sink<F> {}

// ---------------- transport ----------------

/// A [`Carrier`] whose send path calls a host callback and whose receive
/// path is a queue fed by `ov_session_on_datagram`.
///
/// Datagrams pass through an [`ovtransport::rudp::ReliableLink`].
/// It starts in raw mode (bare records) and switches to
/// the reliable framing (<= 1.2 KB fragments, ACK/retransmit, in-order) as
/// soon as the peer sends a framed datagram — a capable host does after our
/// CAPS advertised `CAP_RUDP`. The shell keeps moving opaque datagrams; ACKs
/// and retransmissions go out through the same sink from `ov_session_pump`
/// (call it at least every ~50 ms in direct mode so RTO timers run).
struct FfiDatagramCarrier {
    sink: Sink<DatagramSink>,
    /// Datagrams from the shell, not yet run through the link.
    inbound: VecDeque<Vec<u8>>,
    /// Records the link released, in order.
    delivered: VecDeque<Vec<u8>>,
    link: ovtransport::rudp::ReliableLink,
    closed: bool,
    stats: CarrierStats,
}

impl FfiDatagramCarrier {
    fn new(sink: Sink<DatagramSink>) -> Self {
        Self {
            sink,
            inbound: VecDeque::new(),
            delivered: VecDeque::new(),
            link: ovtransport::rudp::ReliableLink::new(),
            closed: false,
            stats: CarrierStats::default(),
        }
    }

    /// Run queued inbound datagrams + the link timers (ACKs, retransmits,
    /// queued sends), moving released records to `delivered`.
    fn service(&mut self) {
        let now = std::time::Instant::now();
        let sink = self.sink;
        let mut out = |d: &[u8]| -> std::io::Result<()> {
            match sink.f {
                Some(f) => {
                    f(sink.user, d.as_ptr(), d.len());
                    Ok(())
                }
                None => Err(std::io::Error::other("no datagram sink")),
            }
        };
        let delivered = &mut self.delivered;
        let stats = &mut self.stats;
        let mut deliver = |r: &[u8]| {
            stats.recv_datagrams += 1;
            stats.bytes_recv += r.len() as u64;
            delivered.push_back(r.to_vec());
        };
        let had = !self.inbound.is_empty();
        while let Some(d) = self.inbound.pop_front() {
            self.link.on_datagram(&d, now, &mut deliver, &mut out);
        }
        if had {
            self.link.end_batch(now, &mut deliver, &mut out);
        } else {
            self.link.poll(now, &mut deliver, &mut out);
        }
    }

    /// The next in-order record (servicing the link first when empty).
    fn take_inbound(&mut self) -> Option<Vec<u8>> {
        if self.delivered.is_empty() {
            self.service();
        }
        self.delivered.pop_front()
    }
}

impl Carrier for FfiDatagramCarrier {
    fn send(&mut self, datagram: &[u8]) -> Result<usize, TransportError> {
        if self.closed {
            return Err(TransportError::Closed);
        }
        let sink = self.sink;
        if sink.f.is_none() {
            return Err(TransportError::SendFailed(std::io::Error::other(
                "no datagram sink",
            )));
        }
        let mut out = |d: &[u8]| -> std::io::Result<()> {
            if let Some(f) = sink.f {
                f(sink.user, d.as_ptr(), d.len());
            }
            Ok(())
        };
        self.link
            .send(datagram, std::time::Instant::now(), &mut out)
            .map_err(TransportError::SendFailed)?;
        self.stats.sent_datagrams += 1;
        self.stats.bytes_sent += datagram.len() as u64;
        Ok(datagram.len())
    }

    fn poll_recv(&mut self, sink: &mut dyn FnMut(&[u8])) -> Result<usize, TransportError> {
        if self.closed {
            return Err(TransportError::Closed);
        }
        let mut n = 0;
        while let Some(d) = self.take_inbound() {
            sink(&d);
            n += 1;
        }
        Ok(n)
    }

    fn pending_send(&self) -> usize {
        self.link.pending_bytes()
    }

    fn reliable_capable(&self) -> bool {
        true
    }

    fn enable_reliable(&mut self) -> bool {
        self.link.enable();
        true
    }

    fn close(&mut self) -> Result<(), TransportError> {
        self.closed = true;
        Ok(())
    }

    fn is_reliable(&self) -> bool {
        false
    }

    fn stats(&self) -> &CarrierStats {
        &self.stats
    }
}

/// The direct-path engine over shell-owned per-candidate paths.
struct FfiDirect {
    engine: ovtransport::direct::DirectEngine,
    /// Datagrams from the shell, `(path_id, bytes)`.
    inbound: VecDeque<(u32, Vec<u8>)>,
    /// Candidate addresses by path id.
    paths: Vec<String>,
}

/// The engine's I/O: the relay transport + the shell's direct send sink.
struct FfiDirectIo<'a> {
    transport: &'a mut Transport,
    sink: Sink<DirectSendSink>,
}

impl ovtransport::direct::DirectIo for FfiDirectIo<'_> {
    fn primary_send(&mut self, d: &[u8]) -> Result<(), TransportError> {
        transport_send(self.transport, d).map(|_| ())
    }
    fn direct_send(&mut self, path: u32, d: &[u8]) -> std::io::Result<()> {
        match self.sink.f {
            Some(f) => {
                f(self.sink.user, path, d.as_ptr(), d.len());
                Ok(())
            }
            None => Err(std::io::Error::other("no direct sink")),
        }
    }
}

#[derive(Default)]
enum Transport {
    #[default]
    None,
    /// Host-owned datagram path (Swift NWConnection / UDP).
    Datagram(Box<FfiDatagramCarrier>),
    /// Rust-owned relay TCP tunnel.
    Relay(Box<RelayTunnelCarrier>),
}

// ---------------- session ----------------

struct Inner {
    identity: Identity,
    device_id: u64,
    is_server: bool,
    /// Own pairing code (server validates against it; client presents peer's).
    pairing_code: String,
    /// Stretched access PIN: server validates the peer's
    /// proof against it; client proves knowledge of it. `None` = PIN unused.
    pin_psk: Option<[u8; 32]>,
    handshake: Option<Handshake>,
    session: Option<Session>,
    /// Peer fingerprint, cached at establishment (the handshake is dropped
    /// after `take_session`).
    peer_fp: Option<[u8; 16]>,
    transport: Transport,
    /// Inbound datagrams from the Rust-owned relay carrier.
    relay_inbound: VecDeque<Vec<u8>>,
    datagram_sink: Sink<DatagramSink>,
    record_sink: Sink<RecordSink>,
    state_sink: Sink<StateSink>,
    /// Direct-path sinks (set = direct-capable) and the engine
    /// (created at establishment over a relay transport).
    direct_send_sink: Sink<DirectSendSink>,
    direct_path_sink: Sink<DirectPathSink>,
    direct: Option<FfiDirect>,
    /// The relay closed while the direct path carried the session.
    relay_dead: bool,
    /// The handshake failed with `RateLimited`: seconds until the host
    /// accepts another PIN attempt.
    retry_after_secs: Option<u32>,
    /// Event-driven relay receive: woken when the relay socket has bytes.
    wake_sink: Sink<WakeSink>,
    /// The CAPS this session advertised (peeked from the outgoing
    /// channel-0 CAPS record the shell sends) and the peer's (peeked from
    /// the incoming one): the media channel opens when both carry
    /// CAP_MEDIA.
    local_caps: Option<u32>,
    peer_caps: Option<u32>,
}

/// Opaque handle handed to C.
pub struct OvSession {
    inner: Mutex<Inner>,
}

// ---------------- helpers ----------------

fn err_from_transport(e: &TransportError) -> i32 {
    match e {
        TransportError::Closed => -2,
        _ => -4,
    }
}

unsafe fn cstr<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        return None;
    }
    CStr::from_ptr(p).to_str().ok()
}

fn lock(s: &OvSession) -> Result<MutexGuard<'_, Inner>, i32> {
    s.inner.lock().map_err(|_| -2)
}

/// device_id from the fingerprint's low bytes (same recipe as ovview/ovhost):
/// `[0, 0, 0, fp[0..5]]` as a u64.
fn device_id_from_fp(fp: [u8; 16]) -> u64 {
    u64::from_be_bytes([0, 0, 0, fp[0], fp[1], fp[2], fp[3], fp[4]])
}

/// u32 BE from the first 4 bytes of a pairing code (relay session id).
fn relay_session_id(code: &str) -> u32 {
    let bytes = code.as_bytes();
    let mut b = [0u8; 4];
    let n = bytes.len().min(4);
    b[..n].copy_from_slice(&bytes[..n]);
    u32::from_be_bytes(b)
}

/// Copy `s` into `out` NUL-terminated; returns len or -1 if `cap` is too small.
fn copy_str(s: &str, out: *mut c_char, cap: usize) -> i32 {
    let bytes = s.as_bytes();
    if bytes.len() + 1 > cap {
        return -1;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr() as *const c_char, out, bytes.len());
        *out.add(bytes.len()) = 0;
    }
    bytes.len() as i32
}

fn resolve(host: &str, port: u16) -> Option<std::net::SocketAddr> {
    use std::net::ToSocketAddrs;
    (host, port).to_socket_addrs().ok()?.next()
}

// ---------------- identity FFI ----------------

/// Create a fresh random identity. Returns null on failure.
#[no_mangle]
pub extern "C" fn ov_identity_create() -> *mut Identity {
    ffi_guard!(core::ptr::null_mut(), {
        Box::into_raw(Box::new(Identity::generate()))
    })
}

/// Free an identity previously returned by [`ov_identity_create`].
///
/// # Safety
/// `id` must be null or a pointer from [`ov_identity_create`].
#[no_mangle]
pub unsafe extern "C" fn ov_identity_free(id: *mut Identity) {
    ffi_guard!((), {
        if !id.is_null() {
            drop(Box::from_raw(id));
        }
    })
}

/// Write the 16-byte fingerprint into `out`. 0 on success.
///
/// # Safety
/// `id` must be valid; `out` must point to at least 16 writable bytes.
#[no_mangle]
pub unsafe extern "C" fn ov_identity_fingerprint(id: *const Identity, out: *mut u8) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        if id.is_null() || out.is_null() {
            return -1;
        }
        let fp = (*id).fingerprint();
        std::ptr::copy_nonoverlapping(fp.as_ptr(), out, 16);
        0
    })
}

/// Write the human-shareable fingerprint ("A1B2C3D4 E5F60718 ...", NUL
/// terminated) into `out`. `cap` includes the NUL. Returns the string length
/// (excluding NUL), or -1 if the buffer is too small.
///
/// # Safety
/// `out` must be writable for `cap` bytes.
#[no_mangle]
pub unsafe extern "C" fn ov_identity_fingerprint_pine(
    id: *const Identity,
    out: *mut c_char,
    cap: usize,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        if id.is_null() || out.is_null() || cap == 0 {
            return -1;
        }
        copy_str(&(*id).fingerprint_pine(), out, cap)
    })
}

/// Write the 6-digit pairing code (NUL terminated) into `out`. Same return
/// convention as [`ov_identity_fingerprint_pine`].
///
/// # Safety
/// `out` must be writable for `cap` bytes.
#[no_mangle]
pub unsafe extern "C" fn ov_identity_pairing_code(
    id: *const Identity,
    out: *mut c_char,
    cap: usize,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        if id.is_null() || out.is_null() || cap == 0 {
            return -1;
        }
        copy_str(&(*id).pairing_code(), out, cap)
    })
}

// ---------------- session FFI ----------------

/// Create a session.
///
/// * `is_server`: 0 = client (dials), 1 = server (listens).
/// * `pairing_code`: for a server, **our own** 6-digit code (clients must
///   present it); for a client, **the peer's** code we are dialing.
///
/// Returns null on invalid arguments.
///
/// # Safety
/// `pairing_code` must be a valid NUL-terminated C string or null.
#[no_mangle]
pub unsafe extern "C" fn ov_session_create(
    is_server: i32,
    pairing_code: *const c_char,
) -> *mut OvSession {
    ffi_guard!(core::ptr::null_mut(), {
        let Some(code) = cstr(pairing_code).filter(|c| !c.is_empty()) else {
            return core::ptr::null_mut();
        };
        let identity = Identity::generate();
        let device_id = device_id_from_fp(identity.fingerprint());
        let inner = Inner {
            identity,
            device_id,
            is_server: is_server != 0,
            pairing_code: code.to_string(),
            pin_psk: None,
            handshake: None,
            session: None,
            peer_fp: None,
            transport: Transport::None,
            relay_inbound: VecDeque::new(),
            datagram_sink: Sink::none(),
            record_sink: Sink::none(),
            state_sink: Sink::none(),
            direct_send_sink: Sink::none(),
            direct_path_sink: Sink::none(),
            direct: None,
            relay_dead: false,
            retry_after_secs: None,
            wake_sink: Sink::none(),
            local_caps: None,
            peer_caps: None,
        };
        Box::into_raw(Box::new(OvSession {
            inner: Mutex::new(inner),
        }))
    })
}

/// Free a session. Idempotent on null.
///
/// # Safety
/// `s` must be null or a pointer from [`ov_session_create`].
#[no_mangle]
pub unsafe extern "C" fn ov_session_free(s: *mut OvSession) {
    ffi_guard!((), {
        if !s.is_null() {
            drop(Box::from_raw(s));
        }
    })
}

/// Wire the datagram transport. `datagram_sink` receives every outbound
/// datagram (handshake + records) on the pump thread. Returns 0 or -1.
///
/// # Safety
/// `s` must be valid; `user`/`datagram_sink` must outlive the session.
#[no_mangle]
pub unsafe extern "C" fn ov_session_set_datagram_sink(
    s: *mut OvSession,
    user: *mut c_void,
    datagram_sink: DatagramSink,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        if s.is_null() {
            return -1;
        }
        match lock(&*s) {
            Ok(mut g) => {
                g.datagram_sink = Sink {
                    f: Some(datagram_sink),
                    user,
                };
                if matches!(g.transport, Transport::None) {
                    g.transport = Transport::Datagram(Box::new(FfiDatagramCarrier::new(Sink {
                        f: Some(datagram_sink),
                        user,
                    })));
                }
                0
            }
            Err(e) => e,
        }
    })
}

/// Push one inbound datagram (from NWConnection / UDP) into the session.
/// It is queued; call [`ov_session_pump`] to process it.
///
/// # Safety
/// `s` must be valid; `data` must be readable for `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn ov_session_on_datagram(
    s: *mut OvSession,
    data: *const u8,
    len: usize,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        if s.is_null() || (data.is_null() && len > 0) {
            return -1;
        }
        let bytes = if len > 0 {
            unsafe { std::slice::from_raw_parts(data, len) }
        } else {
            &[][..]
        };
        match lock(&*s) {
            Ok(mut g) => match &mut g.transport {
                Transport::Datagram(c) => {
                    c.inbound.push_back(bytes.to_vec());
                    0
                }
                _ => -2,
            },
            Err(e) => e,
        }
    })
}

/// Begin the handshake. For a client this emits the HELLO datagram on the
/// next [`ov_session_pump`]; a server waits for the client's HELLO.
/// `auth_mode`: 0 = pairing code, 1 = direct device (pinned peer).
/// Returns 0, -1 (bad args), -2 (already begun/established).
///
/// # Safety
/// `s` must be valid.
#[no_mangle]
pub unsafe extern "C" fn ov_session_begin(s: *mut OvSession, auth_mode: i32) -> i32 {
    ffi_guard!(OV_ERR_PANIC, { ov_session_begin_inner(s, auth_mode, None) })
}

/// Begin the handshake in PIN (unattended) mode. `pin` is the access
/// password as UTF-8 bytes (`pin_len` bytes): on a server, the PIN clients
/// must present; on a client, the host's PIN we are dialing. The PIN itself
/// never crosses the wire — it is stretched to a PSK ([`ovcore::pin::pin_psk`])
/// and used for the handshake proof + key schedule.
///
/// `auth_mode` must be 2 (PIN) here; `ov_session_begin` keeps 0/1 so existing
/// callers are unaffected.
/// Returns 0, -1 (bad args), -2 (already begun/established).
///
/// # Safety
/// `s` must be valid; `pin` readable for `pin_len` bytes (or null with len 0).
#[no_mangle]
pub unsafe extern "C" fn ov_session_begin_pin(
    s: *mut OvSession,
    pin: *const u8,
    pin_len: usize,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        if s.is_null() || (pin.is_null() && pin_len > 0) || pin_len == 0 {
            return -1;
        }
        let bytes = std::slice::from_raw_parts(pin, pin_len);
        let psk = ovcore::pin::pin_psk(bytes);
        ov_session_begin_inner(s, 2, Some(psk))
    })
}

/// Shared body of [`ov_session_begin`] / [`ov_session_begin_pin`].
///
/// # Safety
/// `s` must be null or a valid `OvSession` pointer.
unsafe fn ov_session_begin_inner(s: *mut OvSession, auth_mode: i32, psk: Option<[u8; 32]>) -> i32 {
    if s.is_null() {
        return -1;
    }
    lock(&*s)
        .and_then(|mut g| {
            if g.handshake.is_some() || g.session.is_some() {
                return Err(-2);
            }
            let auth = match (auth_mode, psk) {
                (0, _) => Auth::PairingCode(g.pairing_code.clone()),
                (1, _) => Auth::DirectDevice,
                (2, Some(psk)) => {
                    g.pin_psk = Some(psk);
                    Auth::Pin(psk)
                }
                _ => return Err(-1),
            };
            let role = if g.is_server {
                Role::Server
            } else {
                Role::Client
            };
            g.handshake = Some(Handshake::new(role, g.device_id, &g.identity, auth));
            Ok(0)
        })
        .unwrap_or(-1)
}

/// Pump: drains handshake events to the datagram sink, feeds queued inbound
/// datagrams into the handshake, and — once established — decrypts inbound
/// datagrams into records delivered to the record sink. Sinks fire on the
/// caller's thread. Returns 0, or a negative error. A failed handshake
/// first flushes what it queued for the peer (REJECT; in PIN mode the
/// client's CONFIRM goes out the same way), then reports -3 (wrong PIN /
/// code), -7 (rate limited, see [`ov_session_retry_after_secs`]), -8
/// (incompatible version) or -9 (other handshake failure); transport and
/// state errors keep -4 / -2.
///
/// # Safety
/// `s` must be valid.
#[no_mangle]
pub unsafe extern "C" fn ov_session_pump(s: *mut OvSession) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        if s.is_null() {
            return -1;
        }
        lock(&*s)
            .and_then(|mut g| pump_inner(&mut g).map(|_| 0i32))
            .unwrap_or_else(|code| code)
    })
}

/// Send one application record on `channel`. Returns 0 or a negative error;
/// -1 also covers a payload larger than 65 479 bytes (one record must fit
/// every carrier) — rejected without consuming a sequence number, so the
/// session stays usable.
///
/// # Safety
/// `s` must be valid; `data` readable for `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn ov_session_send_record(
    s: *mut OvSession,
    channel: u8,
    data: *const u8,
    len: usize,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        if s.is_null() || (data.is_null() && len > 0) {
            return -1;
        }
        let payload = if len > 0 {
            unsafe { std::slice::from_raw_parts(data, len) }
        } else {
            &[][..]
        };
        lock(&*s)
            .and_then(|mut g| {
                let Some(session) = g.session.as_mut() else {
                    return Err(-2);
                };
                // Reject before encrypting: an oversized record would consume a
                // send sequence number and then fail on the carrier, desyncing
                // the peer permanently.
                if payload.len()
                    > ovapp::terminal::MAX_RECORD_LEN - ovapp::terminal::RECORD_OVERHEAD
                {
                    return Err(-1);
                }
                let record = session.encrypt_record(channel, payload);
                if matches!(g.transport, Transport::None) {
                    return Err(-2);
                }
                if let Some(flags) = caps_of(channel, payload) {
                    g.local_caps = Some(flags);
                    maybe_enable_media(&mut g);
                }
                send_record_bytes(&mut g, &record)
                    .map(|_| 0)
                    .map_err(|e| err_from_transport(&e))
            })
            .unwrap_or(-2)
    })
}

/// Register the record sink (decrypted inbound payloads).
///
/// # Safety
/// `s` must be valid; callback/user must outlive the session.
#[no_mangle]
pub unsafe extern "C" fn ov_session_set_record_sink(
    s: *mut OvSession,
    user: *mut c_void,
    record_sink: RecordSink,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        if s.is_null() {
            return -1;
        }
        match lock(&*s) {
            Ok(mut g) => {
                g.record_sink = Sink {
                    f: Some(record_sink),
                    user,
                };
                0
            }
            Err(e) => e,
        }
    })
}

/// Register the state sink. `state = 1` = established.
///
/// # Safety
/// `s` must be valid; callback/user must outlive the session.
#[no_mangle]
pub unsafe extern "C" fn ov_session_set_state_sink(
    s: *mut OvSession,
    user: *mut c_void,
    state_sink: StateSink,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        if s.is_null() {
            return -1;
        }
        match lock(&*s) {
            Ok(mut g) => {
                g.state_sink = Sink {
                    f: Some(state_sink),
                    user,
                };
                0
            }
            Err(e) => e,
        }
    })
}

/// Write the peer's 16-byte fingerprint into `out` once established.
/// 0 on success, -2 if not established yet.
///
/// # Safety
/// `s` must be valid; `out` writable for 16 bytes.
#[no_mangle]
pub unsafe extern "C" fn ov_session_peer_fingerprint(s: *mut OvSession, out: *mut u8) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        if s.is_null() || out.is_null() {
            return -1;
        }
        match lock(&*s) {
            Ok(g) => match g
                .peer_fp
                .or_else(|| g.handshake.as_ref()?.peer().map(|p| p.fingerprint))
            {
                Some(fp) => {
                    std::ptr::copy_nonoverlapping(fp.as_ptr(), out, 16);
                    0
                }
                None => -2,
            },
            Err(e) => e,
        }
    })
}

/// Seconds until the host accepts another PIN attempt, after a pump
/// returned [`OV_ERR_RATE_LIMITED`]; -1 when the session was not rate
/// limited (or on a null session).
///
/// # Safety
/// `s` must be null or a valid session pointer.
#[no_mangle]
pub unsafe extern "C" fn ov_session_retry_after_secs(s: *mut OvSession) -> i64 {
    ffi_guard!(-1, {
        if s.is_null() {
            return -1;
        }
        lock(&*s)
            .ok()
            .and_then(|g| g.retry_after_secs)
            .map_or(-1, i64::from)
    })
}

/// Connect the Rust-owned relay transport (blocking TCP). `relay_host` may be
/// a hostname or IP; `session_id` is the u32 relay session id (the ovview
/// recipe: first 4 bytes of the server's pairing code, BE — see
/// `ov_relay_session_id`).
/// Returns 0, -1 (bad args), -2 (already connected), -4 (transport failure).
///
/// # Safety
/// `s` must be valid; `relay_host` a valid NUL-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn ov_session_relay_connect(
    s: *mut OvSession,
    relay_host: *const c_char,
    relay_port: u16,
    session_id: u32,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        if s.is_null() {
            return -1;
        }
        let Some(host) = cstr(relay_host) else {
            return -1;
        };
        lock(&*s)
            .and_then(|mut g| {
                if !matches!(g.transport, Transport::None) {
                    return Err(-2);
                }
                let addr = resolve(host, relay_port).ok_or(-1)?;
                let role = if g.is_server {
                    TunnelRole::Responder
                } else {
                    TunnelRole::Initiator
                };
                let carrier =
                    RelayTunnelCarrier::connect(addr, session_id, role).map_err(|_| -4)?;
                g.transport = Transport::Relay(Box::new(carrier));
                install_wake(&mut g);
                Ok(0)
            })
            .unwrap_or(-4)
    })
}

/// Start a relay session over a byte stream the host owns — the iOS app's
/// `URLSessionWebSocketTask` to `wss://host/relay` (ovrelay speaks the same
/// relay protocol inside WebSocket binary messages). `stream_sink` receives
/// every outbound chunk (CONNECT first, right here) to send as one binary
/// message; push inbound message bytes with [`ov_session_relay_feed`] and
/// drive the session with [`ov_session_pump_relay`] as for the TCP relay.
/// Returns 0, -1 (bad args), -2 (already connected), -4 (transport failure).
///
/// # Safety
/// `s` must be valid; `user`/`stream_sink` must outlive the session.
#[no_mangle]
pub unsafe extern "C" fn ov_session_relay_connect_stream(
    s: *mut OvSession,
    user: *mut c_void,
    stream_sink: DatagramSink,
    session_id: u32,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        if s.is_null() {
            return -1;
        }
        let sink = Sink {
            f: Some(stream_sink),
            user,
        };
        lock(&*s)
            .and_then(|mut g| {
                if !matches!(g.transport, Transport::None) {
                    return Err(-2);
                }
                let role = if g.is_server {
                    TunnelRole::Responder
                } else {
                    TunnelRole::Initiator
                };
                let writer: BridgeWriter = Box::new(move |bytes: &[u8]| sink.emit_bytes(bytes));
                let carrier =
                    RelayTunnelCarrier::connect_bridge(writer, session_id, role).map_err(|_| -4)?;
                g.transport = Transport::Relay(Box::new(carrier));
                Ok(0)
            })
            .unwrap_or(-4)
    })
}

/// Push inbound bytes (one WebSocket message) of a stream relay started with
/// [`ov_session_relay_connect_stream`]; processed by the next
/// [`ov_session_pump_relay`]. Returns 0, -1 (bad args), -2 (no stream relay).
///
/// # Safety
/// `s` must be valid; `data` must be readable for `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn ov_session_relay_feed(
    s: *mut OvSession,
    data: *const u8,
    len: usize,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        if s.is_null() || (data.is_null() && len > 0) {
            return -1;
        }
        let bytes = if len > 0 {
            unsafe { std::slice::from_raw_parts(data, len) }
        } else {
            &[][..]
        };
        match lock(&*s) {
            Ok(mut g) => match &mut g.transport {
                Transport::Relay(c) => {
                    c.bridge_feed(bytes);
                    0
                }
                _ => -2,
            },
            Err(e) => e,
        }
    })
}

/// The host's stream to the relay closed (WebSocket closed or failed): the
/// relay transport reports closed once buffered bytes are drained.
/// Returns 0, -1 (bad args), -2 (no relay).
///
/// # Safety
/// `s` must be valid.
#[no_mangle]
pub unsafe extern "C" fn ov_session_relay_eof(s: *mut OvSession) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        if s.is_null() {
            return -1;
        }
        match lock(&*s) {
            Ok(mut g) => match &mut g.transport {
                Transport::Relay(c) => {
                    c.bridge_eof();
                    0
                }
                _ => -2,
            },
            Err(e) => e,
        }
    })
}

/// Event-driven relay receive: register a wake sink. While the
/// session runs on the Rust-owned TCP relay ([`ov_session_relay_connect`],
/// before or after this call) a watcher thread blocks on the relay socket
/// and fires `sink` as soon as bytes arrive, so the shell pumps right away
/// instead of on its next timer tick. Coalesced: after a wake the next one
/// fires only once an [`ov_session_pump_relay`] drained the socket again.
/// Keep a slow timer pump as the keepalive / fallback path. The sink fires
/// on the watcher thread (see [`WakeSink`]); the last wake can race a
/// concurrent `ov_session_free` only up to the point the free returns.
/// Pass a null `sink` to stop watching.
///
/// Returns 1 when the socket is being watched, 0 when the sink is stored
/// but there is nothing to watch (no relay yet, or the WebSocket bridge,
/// which is event driven already), -1 on a null session, -4 when the
/// watcher thread could not start.
///
/// # Safety
/// `s` must be valid; `user`/`sink` must stay valid until the session is
/// freed or the sink is removed.
#[no_mangle]
pub unsafe extern "C" fn ov_session_set_wake_sink(
    s: *mut OvSession,
    user: *mut c_void,
    sink: Option<WakeSink>,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        if s.is_null() {
            return -1;
        }
        match lock(&*s) {
            Ok(mut g) => {
                g.wake_sink = match sink {
                    Some(f) => Sink { f: Some(f), user },
                    None => Sink::none(),
                };
                install_wake(&mut g)
            }
            Err(e) => e,
        }
    })
}

/// (Re)install the wake sink on a TCP relay carrier. Returns 1 watched, 0
/// nothing to watch, -4 watcher spawn failure.
fn install_wake(g: &mut Inner) -> i32 {
    let Transport::Relay(c) = &mut g.transport else {
        return 0;
    };
    let sink = g.wake_sink;
    let wake: Option<ovtransport::ReadWake> = if sink.f.is_some() {
        Some(Box::new(move || sink.fire()))
    } else {
        None
    };
    match c.set_read_waker(wake) {
        Ok(true) => 1,
        Ok(false) => 0,
        Err(_) => -4,
    }
}

/// Compute the relay session id (u32 BE) from a pairing code — the same
/// recipe ovview/ovhost use. Writes 4 bytes into `out`.
///
/// # Safety
/// `out` writable for 4 bytes; `code` a valid C string.
#[no_mangle]
pub unsafe extern "C" fn ov_relay_session_id(code: *const c_char, out: *mut u8) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        let Some(code) = cstr(code) else { return -1 };
        if out.is_null() {
            return -1;
        }
        let id = relay_session_id(code);
        std::ptr::copy_nonoverlapping(id.to_be_bytes().as_ptr(), out, 4);
        0
    })
}

/// Max relay polls per [`ov_session_pump_relay`] call (each poll drains up to
/// 1 MiB from the socket), bounding one call's work under a flood.
const PUMP_RELAY_MAX_POLLS: usize = 8;

/// Poll the relay transport: drains every available tunnel frame (incl.
/// relay PING/PONG and punch probes; bounded per call) and feeds record
/// datagrams through the normal decrypt path. Call this instead of
/// [`ov_session_pump`] when using [`ov_session_relay_connect`] /
/// [`ov_session_relay_connect_stream`].
///
/// Returns 0, or a negative error: a handshake verdict as for
/// [`ov_session_pump`] (-3 auth failed, -7 rate limited, -8 incompatible
/// version, -9 other; it takes precedence over a relay close), -4 = relay
/// transport failure (I/O or relay protocol error), and
/// **-5 = relay closed** (peer DISCONNECT, relay EOF / `XT` rejection or
/// timeout, or [`ov_session_relay_eof`] after the buffered bytes were
/// drained). Datagrams received before the close are still processed (their
/// sinks fire) before -5 is returned; -5 is terminal — tear the session down.
///
/// # Safety
/// `s` must be valid.
#[no_mangle]
pub unsafe extern "C" fn ov_session_pump_relay(s: *mut OvSession) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        if s.is_null() {
            return -1;
        }
        lock(&*s)
            .and_then(|mut g| {
                let mut poll_err: Option<i32> = None;
                let relay_dead = g.relay_dead;
                if let (Transport::Relay(c), false) = (&mut g.transport, relay_dead) {
                    let mut batch: VecDeque<Vec<u8>> = VecDeque::new();
                    for _ in 0..PUMP_RELAY_MAX_POLLS {
                        match c.poll_recv(&mut |d| batch.push_back(d.to_vec())) {
                            Ok(0) => break,
                            Ok(_) => {}
                            Err(TransportError::Closed) => {
                                poll_err = Some(-5);
                                break;
                            }
                            Err(_) => {
                                poll_err = Some(-4);
                                break;
                            }
                        }
                    }
                    g.relay_inbound.extend(batch);
                }
                // Deliver what arrived before any close, then report the close
                // (it takes precedence: a send failing on the dead tunnel is a
                // symptom), except that a handshake verdict (auth failed,
                // rate limited, ...) wins over the close it usually causes.
                // A relay that dies while the direct path carries the session
                // is not fatal (it becomes fatal once the direct path is gone
                // too). That covers any relay failure: a WebSocket torn
                // mid-frame reads as -4, not -5. The engine then gives the
                // path a longer silence budget.
                if matches!(poll_err, Some(-5) | Some(-4)) && direct_state(&g) == 2 {
                    g.relay_dead = true;
                    poll_err = None;
                    if let Some(d) = g.direct.as_mut() {
                        d.engine.primary_lost(std::time::Instant::now());
                    }
                }
                let pumped = pump_inner(&mut g);
                if let Err(code) = pumped {
                    if is_handshake_code(code) {
                        return Err(code);
                    }
                }
                if g.relay_dead && direct_state(&g) != 2 {
                    return Err(-5);
                }
                if let Some(code) = poll_err {
                    return Err(code);
                }
                pumped.map(|_| 0)
            })
            .unwrap_or_else(|code| code)
    })
}

// ---------------- pump internals ----------------

/// Send one datagram over whichever transport is wired (host datagram sink
/// or the Rust-owned / bridged relay tunnel).
fn transport_send(t: &mut Transport, d: &[u8]) -> Result<usize, TransportError> {
    match t {
        Transport::Datagram(c) => c.send(d),
        Transport::Relay(c) => c.send(d),
        Transport::None => Err(TransportError::Closed),
    }
}

/// Send every queued handshake event (HELLO / READY) over the transport.
fn flush_handshake(hs: &mut Handshake, t: &mut Transport) -> Result<(), i32> {
    for ev in hs.take_events() {
        let HandshakeEvent::Sent(d) = ev;
        transport_send(t, &d).map_err(|e| err_from_transport(&e))?;
    }
    Ok(())
}

/// Drain handshake events / inbound datagrams / decrypt established records.
fn pump_inner(g: &mut Inner) -> Result<(), i32> {
    // 1. Handshake phase: send queued HELLO/READY, feed inbound datagrams.
    if let Some(hs) = g.handshake.as_mut() {
        // No transport yet: leave the events queued (taking them now would
        // lose the HELLO) and report wrong state.
        if matches!(g.transport, Transport::None) {
            return Err(-2);
        }
        flush_handshake(hs, &mut g.transport)?;
        // Feed queued inbound datagrams into the handshake; feed may
        // generate new Sent events (e.g. the server's HELLO+READY), so
        // drain again after each feed. Stop feeding as soon as the
        // handshake is established: any datagrams behind it in the queue
        // are already records for the session (decrypted in step 2).
        while !hs.is_established() {
            let next = match &mut g.transport {
                Transport::Datagram(c) => c.take_inbound(),
                Transport::Relay(_) => g.relay_inbound.pop_front(),
                Transport::None => None,
            };
            let Some(d) = next else { break };
            let fed = hs.feed(&d);
            // Flush first, failure or not: a failed feed may have queued a
            // REJECT for the peer (wrong PIN, rate limited, version), and a
            // successful one a CONFIRM / READY.
            let flushed = flush_handshake(hs, &mut g.transport);
            if let Err(e) = fed {
                if let ovcore::HandshakeError::RateLimited { retry_after_secs } = &e {
                    g.retry_after_secs = Some(*retry_after_secs);
                }
                return Err(handshake_error_code(&e));
            }
            flushed?;
        }
        if hs.is_established() {
            let session = hs.take_session();
            g.peer_fp = hs.peer().map(|p| p.fingerprint);
            g.handshake = None;
            g.session = session;
            init_direct(g);
            if let Some(f) = g.state_sink.f {
                let fp = g.peer_fp.unwrap_or([0u8; 16]);
                f(g.state_sink.user, 1, fp.as_ptr(), fp.len());
            }
        } else if let Some(e) = hs.error() {
            return Err(handshake_error_code(e));
        }
        if g.session.is_none() {
            return Ok(()); // still handshaking
        }
    }

    // 2. Established: decrypt inbound datagrams into records.
    if g.session.is_none() {
        return Ok(());
    }
    let mut records: Vec<Vec<u8>> = Vec::new();
    loop {
        let next = match &mut g.transport {
            Transport::Datagram(c) => c.take_inbound(),
            Transport::Relay(_) => g.relay_inbound.pop_front(),
            Transport::None => None,
        };
        let Some(d) = next else { break };
        match g.direct.as_mut() {
            // Relay datagrams pass the engine (markers consumed,
            // records released in order across both paths).
            Some(dir) => {
                let now = std::time::Instant::now();
                let mut io = FfiDirectIo {
                    transport: &mut g.transport,
                    sink: g.direct_send_sink,
                };
                dir.engine
                    .on_primary(&d, now, &mut io, &mut |r| records.push(r.to_vec()));
            }
            None => records.push(d),
        }
    }
    if let Some(dir) = g.direct.as_mut() {
        let now = std::time::Instant::now();
        let mut io = FfiDirectIo {
            transport: &mut g.transport,
            sink: g.direct_send_sink,
        };
        while let Some((path, d)) = dir.inbound.pop_front() {
            dir.engine
                .on_direct(path, &d, now, &mut io, &mut |r| records.push(r.to_vec()));
        }
        dir.engine
            .poll(now, &mut io, &mut |r| records.push(r.to_vec()));
    }
    let Some(session) = g.session.as_mut() else {
        return Ok(());
    };
    let mut offers: Vec<Vec<(u8, std::net::SocketAddr)>> = Vec::new();
    let mut peer_caps: Option<u32> = None;
    for d in records {
        // The channel is inside the AEAD AAD, not the header — try 0..=15
        // and deliver the first that decrypts.
        for ch in 0u8..=15 {
            if let Ok(payload) = session.decrypt_record(ch, &d) {
                // The host's CAPS (media channel negotiation).
                if let Some(flags) = caps_of(ch, &payload) {
                    peer_caps = Some(flags);
                }
                // The host's DIRECT_CANDIDATES starts probing.
                if ch == 0
                    && payload.first() == Some(&(ovapp::terminal::OpCode::DirectCandidates as u8))
                {
                    if let Ok(c) = ovapp::terminal::decode_candidates(&payload[1..]) {
                        offers.push(c);
                    }
                }
                if let Some(f) = g.record_sink.f {
                    f(g.record_sink.user, ch, payload.as_ptr(), payload.len());
                }
                break;
            }
        }
    }
    if let Some(f) = peer_caps {
        g.peer_caps = Some(f);
        maybe_enable_media(g);
    }
    for c in offers {
        start_direct_probe(g, &c);
    }
    deliver_media(g);
    Ok(())
}

/// The CAPS flags of a channel-0 CAPS record payload.
fn caps_of(channel: u8, payload: &[u8]) -> Option<u32> {
    (channel == 0 && payload.len() >= 5 && payload[0] == ovapp::terminal::OpCode::Caps as u8)
        .then(|| u32::from_be_bytes([payload[1], payload[2], payload[3], payload[4]]))
}

/// Open the media channel once both CAPS carry CAP_MEDIA (and a
/// direct engine exists).
fn maybe_enable_media(g: &mut Inner) {
    let both = g
        .local_caps
        .is_some_and(|c| c & ovapp::terminal::CAP_MEDIA != 0)
        && g.peer_caps
            .is_some_and(|c| c & ovapp::terminal::CAP_MEDIA != 0);
    if !both {
        return;
    }
    let (Some(session), Some(dir)) = (g.session.as_ref(), g.direct.as_mut()) else {
        return;
    };
    if !dir.engine.media_enabled() {
        dir.engine
            .enable_media(ovtransport::media::session_media_keys(session));
    }
}

/// Hand what the media channel delivered to the shell's record sink
/// as channel-0 commands `ov_screen_feed` understands: a VIDEO_UNIT per
/// complete access unit, a MEDIA_GAP for frames / audio packets lost for
/// good, an AUDIO per Opus packet — so the Swift side needs no change.
fn deliver_media(g: &mut Inner) {
    let Some(dir) = g.direct.as_mut() else { return };
    let events = dir.engine.take_media_events();
    let Some(f) = g.record_sink.f else { return };
    let user = g.record_sink.user;
    let emit = |p: &[u8]| f(user, 0, p.as_ptr(), p.len());
    for ev in events {
        match ev {
            ovtransport::media::MediaEvent::Video(unit) => {
                let mut p = Vec::with_capacity(1 + unit.len());
                p.push(ovapp::terminal::OpCode::VideoUnit as u8);
                p.extend_from_slice(&unit);
                emit(&p);
            }
            ovtransport::media::MediaEvent::VideoLost { frames } => {
                emit(&ovapp::terminal::encode(
                    &ovapp::terminal::Command::MediaGap {
                        kind: ovapp::terminal::GAP_VIDEO,
                        packets: frames.min(u32::from(u16::MAX)) as u16,
                        samples: 0,
                    },
                ));
            }
            ovtransport::media::MediaEvent::Audio { lost, payload } => {
                if lost > 0 {
                    let per = match ovapp::audio::parse_audio_frame(&payload) {
                        Ok((_, ovapp::audio::AudioPayload::Opus { samples, .. })) => samples,
                        Ok((_, ovapp::audio::AudioPayload::Pcm(s))) => s.len() as u32,
                        Err(_) => ovapp::audio::OPUS10_SAMPLES,
                    };
                    let packets = lost.min(u32::from(u16::MAX));
                    emit(&ovapp::terminal::encode(
                        &ovapp::terminal::Command::MediaGap {
                            kind: ovapp::terminal::GAP_AUDIO,
                            packets: packets as u16,
                            samples: per.saturating_mul(packets),
                        },
                    ));
                }
                let mut p = Vec::with_capacity(1 + payload.len());
                p.push(ovapp::terminal::OpCode::Audio as u8);
                p.extend_from_slice(&payload);
                emit(&p);
            }
        }
    }
}

/// Send one encrypted record on the session's current path (the direct
/// engine when present, else the transport).
fn send_record_bytes(g: &mut Inner, record: &[u8]) -> Result<(), TransportError> {
    if let Some(dir) = g.direct.as_mut() {
        let mut io = FfiDirectIo {
            transport: &mut g.transport,
            sink: g.direct_send_sink,
        };
        return dir.engine.send(record, std::time::Instant::now(), &mut io);
    }
    transport_send(&mut g.transport, record).map(|_| ())
}

/// Create the direct engine once the session is established over a relay
/// with direct sinks installed.
fn init_direct(g: &mut Inner) {
    if g.direct.is_some()
        || g.direct_send_sink.f.is_none()
        || !matches!(g.transport, Transport::Relay(_))
    {
        return;
    }
    let Some(session) = g.session.as_ref() else {
        return;
    };
    let key = ovtransport::direct::session_direct_key(session);
    g.direct = Some(FfiDirect {
        engine: ovtransport::direct::DirectEngine::new(session.role_is_server(), key),
        inbound: VecDeque::new(),
        paths: Vec::new(),
    });
}

/// The host offered candidates: number them, ask the shell to open a path
/// per candidate, and start probing.
fn start_direct_probe(g: &mut Inner, cands: &[(u8, std::net::SocketAddr)]) {
    let path_sink = g.direct_path_sink;
    let Some(dir) = g.direct.as_mut() else { return };
    dir.engine.arm();
    let mut ids = Vec::new();
    for (_, a) in cands {
        let text = a.to_string();
        let id = match dir.paths.iter().position(|p| *p == text) {
            Some(i) => i as u32,
            None => {
                dir.paths.push(text.clone());
                let id = (dir.paths.len() - 1) as u32;
                if let (Some(f), Ok(c)) = (path_sink.f, std::ffi::CString::new(text)) {
                    f(path_sink.user, id, c.as_ptr());
                }
                id
            }
        };
        ids.push(id);
    }
    dir.engine.start_probe(ids, std::time::Instant::now());
    let mut io = FfiDirectIo {
        transport: &mut g.transport,
        sink: g.direct_send_sink,
    };
    dir.engine
        .poll(std::time::Instant::now(), &mut io, &mut |_| {});
}

/// 0 relay, 1 probing, 2 direct.
fn direct_state(g: &Inner) -> i32 {
    g.direct.as_ref().map_or(0, |d| d.engine.state() as i32)
}

// ---------------- direct path FFI ----------------

/// Make the session direct-capable: install the per-path send sink and the
/// path-open sink. Call before the handshake completes (or any time over
/// an established relay session). Pair it with
/// [`ov_screen_set_direct_capable`] so our CAPS advertise `CAP_DIRECT` — the
/// host then offers its candidates, `open_path` fires once per candidate,
/// and probing / promotion / fallback run inside the pump calls.
/// Returns 0 or -1.
///
/// # Safety
/// `s` must be valid; `user`/the sinks must outlive the session.
#[no_mangle]
pub unsafe extern "C" fn ov_session_set_direct_sinks(
    s: *mut OvSession,
    user: *mut c_void,
    send: DirectSendSink,
    open_path: DirectPathSink,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        if s.is_null() {
            return -1;
        }
        match lock(&*s) {
            Ok(mut g) => {
                g.direct_send_sink = Sink {
                    f: Some(send),
                    user,
                };
                g.direct_path_sink = Sink {
                    f: Some(open_path),
                    user,
                };
                init_direct(&mut g);
                0
            }
            Err(e) => e,
        }
    })
}

/// Feed one datagram received on direct path `path_id`. Queued; processed
/// by the next `ov_session_pump_relay` / `ov_session_pump`.
/// Returns 0, -1 (bad args), -2 (no direct engine).
///
/// # Safety
/// `s` must be valid; `data` readable for `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn ov_session_on_direct_datagram(
    s: *mut OvSession,
    path_id: u32,
    data: *const u8,
    len: usize,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        if s.is_null() || (data.is_null() && len > 0) {
            return -1;
        }
        let bytes = if len > 0 {
            std::slice::from_raw_parts(data, len)
        } else {
            &[][..]
        };
        match lock(&*s) {
            Ok(mut g) => match g.direct.as_mut() {
                Some(d) => {
                    if d.inbound.len() < 65_536 {
                        d.inbound.push_back((path_id, bytes.to_vec()));
                    }
                    0
                }
                None => -2,
            },
            Err(e) => e,
        }
    })
}

/// The direct-path state: 0 = relay, 1 = probing, 2 = direct. -1 on a
/// null session.
///
/// # Safety
/// `s` must be null or valid.
#[no_mangle]
pub unsafe extern "C" fn ov_session_direct_state(s: *mut OvSession) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        if s.is_null() {
            return -1;
        }
        lock(&*s).map(|g| direct_state(&g)).unwrap_or(-1)
    })
}

/// Milliseconds until the engine next needs a pump (probe resend,
/// keepalive, retransmission); -1 = no timer pending. Shells wake their
/// pump loop at this deadline (or simply pump every ~20 ms while
/// `ov_session_direct_state` != 0).
///
/// # Safety
/// `s` must be null or valid.
#[no_mangle]
pub unsafe extern "C" fn ov_session_direct_deadline_ms(s: *mut OvSession) -> i64 {
    ffi_guard!(-1, {
        if s.is_null() {
            return -1;
        }
        lock(&*s)
            .ok()
            .and_then(|g| {
                g.direct
                    .as_ref()?
                    .engine
                    .next_deadline(std::time::Instant::now())
            })
            .map_or(-1, |d| d.as_millis() as i64)
    })
}

/// The active direct path id (`>= 0`) while state is 2, else -1.
///
/// # Safety
/// `s` must be null or valid.
#[no_mangle]
pub unsafe extern "C" fn ov_session_direct_path(s: *mut OvSession) -> i64 {
    ffi_guard!(-1, {
        if s.is_null() {
            return -1;
        }
        lock(&*s)
            .ok()
            .and_then(|g| g.direct.as_ref()?.engine.active_path())
            .map_or(-1, i64::from)
    })
}

/// Send a file to the peer: begin + `FILE_XFER_DATA` chunks (16 KiB
/// default) + `FILE_XFER_COMPLETE`, all on channel 0 — the same file-id-keyed
/// path the desktop shells use, driven by `ovapp::file_transfer::FileTransfer::send`
/// (chunking/assembly logic shared, not duplicated). Safe to call from any
/// thread (the session mutex serializes it against the pump). Returns 0,
/// -1 (bad args), -2 (not established / no transport) or -4 (transport send
/// failure).
///
/// # Safety
/// `s` must be valid; `name` a valid NUL-terminated C string; `data`
/// readable for `data_len` bytes.
#[no_mangle]
pub unsafe extern "C" fn ov_file_send(
    s: *mut OvSession,
    name: *const c_char,
    data: *const u8,
    data_len: usize,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        if s.is_null() || (data.is_null() && data_len > 0) {
            return -1;
        }
        let Some(name) = cstr(name) else {
            return -1;
        };
        let bytes = if data_len > 0 {
            std::slice::from_raw_parts(data, data_len)
        } else {
            &[][..]
        };
        lock(&*s)
            .and_then(|mut g| {
                // Temporarily take the session out so the send closure can touch
                // both the encrypt path and the transport (disjoint borrows of
                // `g` are not expressible inside one closure).
                let Some(mut session) = g.session.take() else {
                    return Err(-2);
                };
                let mut xfer = ovapp::file_transfer::FileTransfer::new();
                let mut send_err: Option<i32> = None;
                let res = xfer.send(name, bytes, 0, |cmd| {
                    if send_err.is_some() {
                        return Ok(());
                    }
                    // One command = one AEAD record on channel 0, sent through
                    // the transport (same path as `ov_session_send_record`).
                    let record = session.encrypt_record(0, &ovapp::terminal::encode(cmd));
                    let sent = send_record_bytes(&mut g, &record);
                    match sent {
                        Ok(_) => Ok(()),
                        Err(e) => {
                            send_err = Some(err_from_transport(&e));
                            Err("transport".to_owned())
                        }
                    }
                });
                g.session = Some(session);
                match (res, send_err) {
                    (Err(_), Some(code)) => Err(code),
                    // Rejected by the chunker (e.g. an unusable name).
                    (Err(_), None) => Err(-1),
                    (Ok(()), _) => Ok(0),
                }
            })
            .unwrap_or(-2)
    })
}

// ---------------- screen frames ----------------

/// Reassembled-screen-frame sink. Fired on the `ov_screen_feed` caller thread
/// once a frame's `FRAME_END` has closed it. `pixels` points into the
/// assembler's buffer and is valid only for the duration of the callback —
/// copy it out.
pub type FrameSink = extern "C" fn(
    user: *mut c_void,
    seq: u64,
    width: u32,
    height: u32,
    stride: u32,
    format: u8,
    pixels: *const u8,
    len: usize,
);

/// Screen-frame reassembler. Fed **terminal-channel (channel 0)** command
/// records — each [`ov_screen_feed`] call carries one wire command
/// (`[1B opcode][payload]`). Non-screen opcodes (ECHO/TYPE/FILE_*/PING) are
/// ignored; `FRAME_BEGIN/DATA/END` are reassembled into a pixel buffer and
/// delivered via the frame sink on `FRAME_END`; `CURSOR`/`CURSOR_SHAPE`
/// commands fire the cursor sinks.
///
/// This keeps pixel reassembly on the Rust side (single source of truth with
/// the desktop `TerminalSession`) so the phone shell only renders.
///
/// The assembler keeps a persistent framebuffer
/// ([`ovapp::tiles::ScreenDecoder`], shared with the desktop viewer), applies
/// full frames and `TILES_BEGIN` dirty-tile deltas (DEFLATE or zstd, decoded
/// in pure Rust) and still delivers the **whole** framebuffer through the
/// same frame sink — the shell's contract is unchanged. Capabilities and
/// keyframe requests ride the existing 9-byte reply path
/// ([`ov_screen_take_pong_reply`]): once the first screen command arrives
/// the next take yields a `CAPS` record, and a desync (lost/corrupt update)
/// queues a `KEYFRAME_REQUEST` — both exactly 9 bytes, like a PONG, so the
/// shell sends them without knowing what they are.
pub struct OvScreen {
    /// The update being reassembled: its FRAME_END seq + decode header.
    rx: Option<(u64, ovapp::tiles::UpdateMeta)>,
    buf: Vec<u8>,
    /// Persistent framebuffer (full frames + tile deltas applied in order).
    dec: ovapp::tiles::ScreenDecoder,
    /// 9-byte control records owed to the peer (CAPS, KEYFRAME_REQUEST),
    /// drained after any PONG by [`ov_screen_take_pong_reply`].
    outbox: std::collections::VecDeque<[u8; ovapp::terminal::SMALL_CONTROL_LEN]>,
    /// CAPS queued once per screen (on the first screen command).
    caps_queued: bool,
    /// The host's CAPS bits (0 = old host: no keyframe requests).
    peer_caps: u32,
    /// Rate limit for KEYFRAME_REQUEST.
    last_kf_request: Option<std::time::Instant>,
    /// Advertise `CAP_DIRECT` (the shell wired the direct sinks).
    direct_capable: bool,
    /// The video stream's colour description (primaries,
    /// transfer, matrix, full_range) from the last SPS.
    video_colour: [u8; 4],
    sink: Sink<FrameSink>,
    cursor_sink: Sink<CursorSink>,
    cursor_shape_sink: Sink<CursorShapeSink>,
    /// Audio sink: fired on AUDIO commands with one 20 ms tick of
    /// s16le 48 kHz mono PCM.
    audio_sink: Sink<AudioSink>,
    /// Opus sink: fired on Opus AUDIO commands with the packet
    /// passed through (the shell decodes it natively). Installing it makes
    /// our CAPS advertise `CAP_OPUS`; without it the host keeps sending raw
    /// PCM to `audio_sink`.
    opus_sink: Sink<OpusSink>,
    /// H.264 video sink: access units passed through (SPS/PPS +
    /// AVCC) for VideoToolbox. Installing it advertises `CAP_H264`.
    video_sink: Sink<VideoSink>,
    /// The open VIDEO_BEGIN (its FRAME_DATA accumulates in `buf`).
    rx_video: Option<(ovapp::video::VideoHeader, u32)>,
    /// Video continuity (a gap drops P-frames until the next IDR).
    video_rx: ovapp::video::VideoRx,
    last_video_seq: u64,
    last_idr_request: Option<std::time::Instant>,
    /// Monitors sink: fired on MONITORS_INFO with the host's
    /// monitor list so the shell can offer a monitor picker.
    monitors_sink: Sink<MonitorsSink>,
    /// Clipboard sink: fired on CLIPBOARD with peer clipboard text.
    clipboard_sink: Sink<ClipboardSink>,
    /// File sink: fired when a peer's file transfer completes. The
    /// reassembler (`ovapp::file_transfer::FileTransfer`) mirrors the desktop
    /// `TerminalSession` pump exactly — single source of truth.
    file_sink: Sink<FileSink>,
    /// File-id-keyed transfer reassembly, drained into `file_sink`
    /// after each `FILE_XFER_*` command is fed.
    xfer: ovapp::file_transfer::FileTransfer,
    /// Liveness state: inbound tracking + keepalive pacing + RTT.
    live: ovapp::liveness::Liveness,
    keepalive: ovapp::liveness::Keepalive,
    /// When the in-flight PING was sent, in the shell's millisecond clock.
    ping_sent_ms: Option<i64>,
    /// First shell tick (shell ms, real instant): maps the shell's clock onto
    /// this screen's real clock (see `shell_now`).
    shell_clock_base: Option<(i64, std::time::Instant)>,
    /// Real-clock instant of the in-flight PING (RTT is measured in real
    /// time, since `ov_screen_feed` has no shell clock).
    ping_sent_real: Option<std::time::Instant>,
    /// Real-clock instant of the in-flight PING (RTT is measured in real
    /// time, since `ov_screen_feed` has no shell clock).
    /// Last measured RTT in milliseconds (-1 before the first PONG).
    last_rtt_ms: i32,
    /// Rolling inbound-frame statistics for the FPS sink value.
    stats: ovapp::liveness::LinkStats,
    /// Last computed FPS (kept between ticks so the sink can fire anytime).
    last_fps: f32,
    liveness_sink: Sink<LivenessSink>,
    /// The echoed nonce of an inbound PING awaiting the shell's PONG reply
    ///; `None` when nothing is pending.
    pong_reply_nonce: Option<u64>,
    /// The Instant `ov_screen_new` ran at — the epoch for `Instant` math.
    epoch: std::time::Instant,
}

/// Remote-cursor-position sink (`CURSOR` commands). Coordinates are in frame
/// pixel space (the captured region).
pub type CursorSink = extern "C" fn(user: *mut c_void, x: i32, y: i32);

/// Remote-cursor-shape sink (`CURSOR_SHAPE` commands). `bgra` is a
/// premultiplied-alpha 32-bit BGRA bitmap of `w * h * 4` bytes, valid only
/// for the duration of the callback — copy it out.
pub type CursorShapeSink = extern "C" fn(
    user: *mut c_void,
    hotspot_x: u16,
    hotspot_y: u16,
    w: u16,
    h: u16,
    bgra: *const u8,
    len: usize,
);

/// Liveness sink, fired by [`ov_screen_liveness_tick`]: `alive` is 1
/// while inbound traffic is fresh and 0 once the peer has been silent past
/// the dead window; `rtt_ms` is the last measured round trip (-1 before the
/// first PONG); `fps` is the inbound frame rate over the rolling window.
pub type LivenessSink = extern "C" fn(user: *mut c_void, alive: i32, rtt_ms: i32, fps: f32);

/// Monitors sink, fired on MONITORS_INFO: `mons` points to
/// `count` packed records of 17 bytes each (big-endian) — `i32 x, i32 y, u32 w, u32 h,
/// u8 is_primary` — valid only for the duration of the callback (copy it
/// out). `count` may be 0.
pub type MonitorsSink = extern "C" fn(user: *mut c_void, mons: *const u8, count: usize);

/// Clipboard sink, fired on CLIPBOARD commands: `kind` is the
/// payload kind (0 = text) and `text` points to `len` UTF-8 bytes, valid
/// only for the duration of the callback (copy it out).
pub type ClipboardSink = extern "C" fn(user: *mut c_void, kind: u8, text: *const u8, len: usize);

/// File sink, fired when a peer-to-phone file transfer completes:
/// `name` is a NUL-terminated UTF-8 file name (sanitized by the reassembler)
/// and `data` points to `len` reassembled bytes, valid only for the duration
/// of the callback (copy it out — the whole file is held in memory here; a
/// streaming-to-disk receive is a documented future refinement).
pub type FileSink =
    extern "C" fn(user: *mut c_void, name: *const c_char, data: *const u8, len: usize);

/// Audio sink, fired on AUDIO commands: `samples` points to `len`
/// s16le PCM samples (48 kHz mono — 960 per 20 ms tick), valid only for the
/// duration of the callback (copy it out).
pub type AudioSink = extern "C" fn(user: *mut c_void, samples: *const u8, len: usize);

/// Opus audio sink, fired on Opus AUDIO commands: `packet` points
/// to `len` bytes of ONE Opus packet (48 kHz mono, normally 20 ms) that
/// decodes to `samples` samples; valid only for the duration of the
/// callback (copy it out). Rust does not decode Opus (the staticlib stays
/// pure Rust) — the shell feeds the packet to its native decoder.
///
/// On the direct path's media channel, `packet` NULL with `len` 0 signals
/// that `samples` samples of audio were lost for good: the shell conceals
/// that span (or simply skips it; the current Swift sink ignores NULL).
pub type OpusSink = extern "C" fn(user: *mut c_void, packet: *const u8, len: usize, samples: u32);

/// [`OvVideoPacket::flags`]: an IDR access unit; `sps`/`pps` are set.
pub const OV_VIDEO_KEYFRAME: u32 = 1;
/// [`OvVideoPacket::flags`]: motion over — no picture; the next frame-sink
/// delivery carries the lossless refresh of `x, y, w, h`, after which the
/// shell removes its video overlay.
pub const OV_VIDEO_END: u32 = 2;

/// One H.264 access unit for the shell, laid out for C (`ov_video_packet`
/// in shim.h). All pointers are valid only for the duration of the callback.
///
/// The picture (`w x h`, even) goes at `(x, y)` of the `screen_w x
/// screen_h` framebuffer the frame sink delivers. `data` is **AVCC**: each
/// NAL unit prefixed by its 4-byte big-endian length, parameter sets and
/// access-unit delimiters removed (exactly what a `CMSampleBuffer` for
/// VideoToolbox wants, with `nalUnitHeaderLength` 4). On a keyframe `sps` /
/// `pps` are the raw parameter-set NAL units (no start code, no length) for
/// `CMVideoFormatDescriptionCreateFromH264ParameterSets`; otherwise null/0.
/// `pts_us` is the host's capture time (microseconds since the UNIX epoch).
#[repr(C)]
#[derive(Debug)]
pub struct OvVideoPacket {
    pub seq: u64,
    pub pts_us: u64,
    pub flags: u32,
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub screen_w: u32,
    pub screen_h: u32,
    pub reserved: u32,
    pub sps: *const u8,
    pub sps_len: usize,
    pub pps: *const u8,
    pub pps_len: usize,
    pub data: *const u8,
    pub data_len: usize,
    /// The stream's colour description as H.264 /
    /// ISO 23001-8 code points, from the SPS VUI (the host always signals
    /// BT.709 limited range: 1 / 1 / 1 / 0). Map `matrix` to
    /// `kCMFormatDescriptionExtension_YCbCrMatrix` (1 = ITU_R_709_2,
    /// 6 = ITU_R_601_4), `primaries` to `..._ColorPrimaries`, `transfer`
    /// to `..._TransferFunction`; `full_range` 0 = video range (420v).
    pub colour_primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
    pub full_range: u8,
}

/// H.264 video sink, fired on the `ov_screen_feed` caller thread.
pub type VideoSink = extern "C" fn(user: *mut c_void, packet: *const OvVideoPacket);

/// Advertise `CAP_DIRECT` in our CAPS (call before the first
/// screen command, alongside `ov_session_set_direct_sinks`). Returns 0 or -1.
///
/// # Safety
/// `s` must be null or valid.
#[no_mangle]
pub unsafe extern "C" fn ov_screen_set_direct_capable(s: *mut OvScreen, on: i32) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        match s.as_mut() {
            Some(s) => {
                s.direct_capable = on != 0;
                0
            }
            None => -1,
        }
    })
}

/// Build a SESSION_OPTION record payload (`key` 1 = hide the
/// host wallpaper, 2 = disable host animations; `value` 0/1) into `out`
/// (>= 3 bytes) for `ov_session_send_record(channel 0)`. Only meaningful
/// when `ov_screen_peer_caps` has `CAP_DESKTOP_FX` (1 << 8). Returns the
/// length (3) or -1.
///
/// # Safety
/// `out` writable for `cap` bytes.
#[no_mangle]
pub unsafe extern "C" fn ov_session_option_record(
    key: u8,
    value: u8,
    out: *mut u8,
    cap: usize,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        if out.is_null() || cap < 3 {
            return -1;
        }
        let b = ovapp::terminal::encode(&ovapp::terminal::Command::SessionOption { key, value });
        std::ptr::copy_nonoverlapping(b.as_ptr(), out, b.len());
        b.len() as i32
    })
}

/// Create an empty screen assembler.
#[no_mangle]
pub extern "C" fn ov_screen_new() -> *mut OvScreen {
    ffi_guard!(core::ptr::null_mut(), {
        let now = std::time::Instant::now();
        Box::into_raw(Box::new(OvScreen {
            rx: None,
            buf: Vec::new(),
            dec: ovapp::tiles::ScreenDecoder::new(),
            outbox: std::collections::VecDeque::new(),
            caps_queued: false,
            peer_caps: 0,
            last_kf_request: None,
            direct_capable: false,
            video_colour: [1, 1, 1, 0],
            sink: Sink::none(),
            cursor_sink: Sink::none(),
            cursor_shape_sink: Sink::none(),
            audio_sink: Sink::none(),
            opus_sink: Sink::none(),
            video_sink: Sink::none(),
            rx_video: None,
            video_rx: ovapp::video::VideoRx::new(),
            last_video_seq: 0,
            last_idr_request: None,
            monitors_sink: Sink::none(),
            clipboard_sink: Sink::none(),
            file_sink: Sink::none(),
            xfer: ovapp::file_transfer::FileTransfer::new(),
            live: ovapp::liveness::Liveness::new(now),
            keepalive: ovapp::liveness::Keepalive::new(ovapp::liveness::KEEPALIVE_INTERVAL, now),
            ping_sent_ms: None,
            shell_clock_base: None,
            ping_sent_real: None,
            last_rtt_ms: -1,
            stats: ovapp::liveness::LinkStats::new(std::time::Duration::from_secs(2)),
            last_fps: 0.0,
            liveness_sink: Sink::none(),
            pong_reply_nonce: None,
            epoch: now,
        }))
    })
}

/// Free an assembler returned by [`ov_screen_new`].
///
/// # Safety
/// `s` must be null or a pointer from [`ov_screen_new`] that was not freed
/// yet; it must not be used afterwards.
#[no_mangle]
pub unsafe extern "C" fn ov_screen_free(s: *mut OvScreen) {
    ffi_guard!((), {
        if !s.is_null() {
            drop(Box::from_raw(s));
        }
    })
}

/// Wire the frame sink. `sink`/`user` must outlive the assembler.
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`]. The screen is
/// not internally locked: no other call on the same handle may run
/// concurrently (the shell drives it from one serial queue). `sink` and
/// `user` must stay valid until the sink is replaced or the screen is freed.
#[no_mangle]
pub unsafe extern "C" fn ov_screen_set_frame_sink(
    s: *mut OvScreen,
    user: *mut c_void,
    sink: FrameSink,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        let screen = match s.as_mut() {
            Some(s) => s,
            None => return -1,
        };
        screen.sink = Sink {
            f: Some(sink),
            user,
        };
        0
    })
}

/// Wire the remote-cursor-position sink. `sink`/`user` must outlive the
/// assembler. Pass a null `sink` to remove it.
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`]. The screen is
/// not internally locked: no other call on the same handle may run
/// concurrently (the shell drives it from one serial queue). `sink` and
/// `user` must stay valid until the sink is replaced or the screen is freed.
#[no_mangle]
pub unsafe extern "C" fn ov_screen_set_cursor_sink(
    s: *mut OvScreen,
    user: *mut c_void,
    sink: Option<CursorSink>,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        let screen = match s.as_mut() {
            Some(s) => s,
            None => return -1,
        };
        screen.cursor_sink = match sink {
            Some(f) => Sink { f: Some(f), user },
            None => Sink::none(),
        };
        0
    })
}

/// Wire the remote-cursor-shape sink. `sink`/`user` must outlive the
/// assembler. Pass a null `sink` to remove it.
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`]. The screen is
/// not internally locked: no other call on the same handle may run
/// concurrently (the shell drives it from one serial queue). `sink` and
/// `user` must stay valid until the sink is replaced or the screen is freed.
#[no_mangle]
pub unsafe extern "C" fn ov_screen_set_cursor_shape_sink(
    s: *mut OvScreen,
    user: *mut c_void,
    sink: Option<CursorShapeSink>,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        let screen = match s.as_mut() {
            Some(s) => s,
            None => return -1,
        };
        screen.cursor_shape_sink = match sink {
            Some(f) => Sink { f: Some(f), user },
            None => Sink::none(),
        };
        0
    })
}

/// Wire the monitors sink. `sink`/`user` must outlive the assembler.
/// Pass a null `sink` to remove it.
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`]. The screen is
/// not internally locked: no other call on the same handle may run
/// concurrently (the shell drives it from one serial queue). `sink` and
/// `user` must stay valid until the sink is replaced or the screen is freed.
#[no_mangle]
pub unsafe extern "C" fn ov_screen_set_monitors_sink(
    s: *mut OvScreen,
    user: *mut c_void,
    sink: Option<MonitorsSink>,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        let screen = match s.as_mut() {
            Some(s) => s,
            None => return -1,
        };
        screen.monitors_sink = match sink {
            Some(f) => Sink { f: Some(f), user },
            None => Sink::none(),
        };
        0
    })
}

/// Wire the audio sink. `sink`/`user` must outlive the assembler.
/// Pass a null `sink` to remove it.
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`]. The screen is
/// not internally locked: no other call on the same handle may run
/// concurrently (the shell drives it from one serial queue). `sink` and
/// `user` must stay valid until the sink is replaced or the screen is freed.
#[no_mangle]
pub unsafe extern "C" fn ov_screen_set_audio_sink(
    s: *mut OvScreen,
    user: *mut c_void,
    sink: Option<AudioSink>,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        let screen = match s.as_mut() {
            Some(s) => s,
            None => return -1,
        };
        screen.audio_sink = match sink {
            Some(f) => Sink { f: Some(f), user },
            None => Sink::none(),
        };
        0
    })
}

/// Wire the Opus audio sink. Set it **before** feeding the first
/// screen command: our CAPS (sent once, with the first screen command) then
/// advertise `CAP_OPUS` and an Opus-capable host switches its AUDIO to Opus
/// packets, delivered here instead of to the PCM audio sink. `sink`/`user`
/// must outlive the assembler. Pass a null `sink` to remove it (raw PCM
/// keeps flowing to the audio sink for hosts without Opus).
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`]. The screen is
/// not internally locked: no other call on the same handle may run
/// concurrently (the shell drives it from one serial queue). `sink` and
/// `user` must stay valid until the sink is replaced or the screen is freed.
#[no_mangle]
pub unsafe extern "C" fn ov_screen_set_opus_sink(
    s: *mut OvScreen,
    user: *mut c_void,
    sink: Option<OpusSink>,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        let screen = match s.as_mut() {
            Some(s) => s,
            None => return -1,
        };
        screen.opus_sink = match sink {
            Some(f) => Sink { f: Some(f), user },
            None => Sink::none(),
        };
        0
    })
}

/// Wire the H.264 video sink. Set it **before** feeding the first
/// screen command, and only when the device can decode H.264 (VideoToolbox):
/// our CAPS then advertise `CAP_H264`, and a capable host streams
/// high-motion regions as [`OvVideoPacket`]s (composited by the shell over
/// the frame-sink framebuffer) instead of lossless tiles. `sink`/`user` must
/// outlive the assembler. Pass a null `sink` to remove it.
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`]. The screen is
/// not internally locked: no other call on the same handle may run
/// concurrently (the shell drives it from one serial queue). `sink` and
/// `user` must stay valid until the sink is replaced or the screen is freed.
#[no_mangle]
pub unsafe extern "C" fn ov_screen_set_video_sink(
    s: *mut OvScreen,
    user: *mut c_void,
    sink: Option<VideoSink>,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        let screen = match s.as_mut() {
            Some(s) => s,
            None => return -1,
        };
        screen.video_sink = match sink {
            Some(f) => Sink { f: Some(f), user },
            None => Sink::none(),
        };
        0
    })
}

/// The shell's decoder failed (or it dropped a unit): skip
/// P-frames until the next keyframe and queue a `VIDEO_IDR_REQUEST` for
/// the host (sent through [`ov_screen_take_pong_reply`]; rate limited).
/// Returns 0, or -1 on a null handle.
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`]. The screen is
/// not internally locked: no other call on the same handle may run
/// concurrently (the shell drives it from one serial queue). Never call it from
/// inside one of this screen's sinks.
#[no_mangle]
pub unsafe extern "C" fn ov_screen_request_video_idr(s: *mut OvScreen) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        let screen = match s.as_mut() {
            Some(s) => s,
            None => return -1,
        };
        screen.video_rx.mark_broken();
        screen.request_idr();
        0
    })
}

/// Wire the clipboard sink. `sink`/`user` must outlive the
/// assembler. Pass a null `sink` to remove it.
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`]. The screen is
/// not internally locked: no other call on the same handle may run
/// concurrently (the shell drives it from one serial queue). `sink` and
/// `user` must stay valid until the sink is replaced or the screen is freed.
#[no_mangle]
pub unsafe extern "C" fn ov_screen_set_clipboard_sink(
    s: *mut OvScreen,
    user: *mut c_void,
    sink: Option<ClipboardSink>,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        let screen = match s.as_mut() {
            Some(s) => s,
            None => return -1,
        };
        screen.clipboard_sink = match sink {
            Some(f) => Sink { f: Some(f), user },
            None => Sink::none(),
        };
        0
    })
}

/// Wire the file sink. Fired when a peer's file transfer completes
/// (name + reassembled bytes). `sink`/`user` must outlive the assembler.
/// Pass a null `sink` to remove it.
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`]. The screen is
/// not internally locked: no other call on the same handle may run
/// concurrently (the shell drives it from one serial queue). `sink` and
/// `user` must stay valid until the sink is replaced or the screen is freed.
#[no_mangle]
pub unsafe extern "C" fn ov_screen_set_file_sink(
    s: *mut OvScreen,
    user: *mut c_void,
    sink: Option<FileSink>,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        let screen = match s.as_mut() {
            Some(s) => s,
            None => return -1,
        };
        screen.file_sink = match sink {
            Some(f) => Sink { f: Some(f), user },
            None => Sink::none(),
        };
        0
    })
}

/// Wire the liveness sink. `sink`/`user` must outlive the assembler.
/// Pass a null `sink` to remove it.
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`]. The screen is
/// not internally locked: no other call on the same handle may run
/// concurrently (the shell drives it from one serial queue). `sink` and
/// `user` must stay valid until the sink is replaced or the screen is freed.
#[no_mangle]
pub unsafe extern "C" fn ov_screen_set_liveness_sink(
    s: *mut OvScreen,
    user: *mut c_void,
    sink: Option<LivenessSink>,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        let screen = match s.as_mut() {
            Some(s) => s,
            None => return -1,
        };
        screen.liveness_sink = match sink {
            Some(f) => Sink { f: Some(f), user },
            None => Sink::none(),
        };
        0
    })
}

/// Keepalive tick: call periodically (e.g. every pump cycle) with
/// the shell's monotonic clock in milliseconds. Returns the PING sequence to
/// put on the wire (> 0) when a keepalive PING is due — the shell sends a
/// `PING` command (8-byte BE sequence) on channel 0 — or 0 when not due.
/// The matching `PONG` echo (fed back through [`ov_screen_feed`]) measures
/// the RTT and fires the liveness sink.
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`]. The screen is
/// not internally locked: no other call on the same handle may run
/// concurrently (the shell drives it from one serial queue).
#[no_mangle]
pub unsafe extern "C" fn ov_screen_keepalive_tick(s: *mut OvScreen, now_ms: i64) -> i64 {
    ffi_guard!(0, {
        let screen = match s.as_mut() {
            Some(s) => s,
            None => return 0,
        };
        let now = screen.shell_now(now_ms);
        match screen.keepalive.tick(now) {
            Some(seq) => {
                screen.ping_sent_ms = Some(now_ms);
                screen.ping_sent_real = Some(std::time::Instant::now());
                seq as i64
            }
            None => 0,
        }
    })
}

/// Watchdog: milliseconds since the last inbound record from the host (any
/// command, media included), on the shell's clock (`now_ms` as for
/// [`ov_screen_liveness_tick`], mapped the same way so reconnects keep
/// working). -1 on a null handle. The host sends at least a keepalive PING
/// every 5 s, so a few seconds of silence means the session is dead even if
/// no transport reported it.
///
/// # Safety
/// `s` must be null or valid.
#[no_mangle]
pub unsafe extern "C" fn ov_screen_inbound_age_ms(s: *mut OvScreen, now_ms: i64) -> i64 {
    ffi_guard!(-1, {
        let Some(screen) = s.as_mut() else { return -1 };
        let now = screen.shell_now(now_ms);
        screen
            .live
            .since_inbound(now)
            .as_millis()
            .min(i64::MAX as u128) as i64
    })
}

/// Liveness tick: call periodically with the shell's monotonic clock
/// in milliseconds. Fires the liveness sink with the current alive/RTT/FPS
/// verdict and returns 0 (or -1 on a null handle).
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`]. The screen is
/// not internally locked: no other call on the same handle may run
/// concurrently (the shell drives it from one serial queue).
#[no_mangle]
pub unsafe extern "C" fn ov_screen_liveness_tick(s: *mut OvScreen, now_ms: i64) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        let screen = match s.as_mut() {
            Some(s) => s,
            None => return -1,
        };
        let now = screen.shell_now(now_ms);
        let (fps, _) = screen.stats.snapshot(now);
        screen.last_fps = fps as f32;
        let alive = i32::from(!screen.live.is_dead(now, ovapp::liveness::DEAD_AFTER));
        if let Some(f) = screen.liveness_sink.f {
            f(
                screen.liveness_sink.user,
                alive,
                screen.last_rtt_ms,
                screen.last_fps,
            );
        }
        0
    })
}

/// Take the next 9-byte control record owed to the peer. First the PONG
/// echo for the last inbound PING (`0x11` opcode + the 8-byte BE
/// nonce); otherwise a queued control record (`CAPS` after the first
/// screen command, `KEYFRAME_REQUEST` after a desync). Writes it into `out`
/// and returns 1; the shell sends it on channel 0. Returns 0 when nothing is
/// pending, -1 on a null handle or a too-small buffer (`cap` must be >= 9).
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`]. The screen is
/// not internally locked: no other call on the same handle may run
/// concurrently (the shell drives it from one serial queue). `out` must be
/// writable for `cap` bytes.
#[no_mangle]
pub unsafe extern "C" fn ov_screen_take_pong_reply(
    s: *mut OvScreen,
    out: *mut u8,
    cap: usize,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        let screen = match s.as_mut() {
            Some(s) => s,
            None => return -1,
        };
        let reply = if let Some(nonce) = screen.pong_reply_nonce {
            let mut reply = [0u8; 9];
            reply[0] = ovapp::terminal::OpCode::Pong as u8;
            reply[1..].copy_from_slice(&nonce.to_be_bytes());
            reply
        } else if let Some(rec) = screen.outbox.front() {
            *rec
        } else {
            return 0;
        };
        if out.is_null() || cap < 9 {
            return -1;
        }
        if screen.pong_reply_nonce.take().is_none() {
            screen.outbox.pop_front();
        }
        std::ptr::copy_nonoverlapping(reply.as_ptr(), out, 9);
        1
    })
}

/// The host's CAPS bits (`ovapp::terminal::CAP_*`), 0 until the host
/// answered our CAPS (an old host never does). The shell checks
/// `CAP_INPUT2` (0x40) before sending `POINTER_EX` / `WHEEL` / `KEY_EX` /
/// `TEXT` / `RELEASE_ALL` / `SAS`, and falls back to `POINTER` / `KEY`
/// otherwise. Returns 0 on a null handle.
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`] (not
/// internally locked: call it on the thread that feeds the screen).
#[no_mangle]
pub unsafe extern "C" fn ov_screen_peer_caps(s: *const OvScreen) -> u32 {
    ffi_guard!(0, { s.as_ref().map_or(0, |s| s.peer_caps) })
}

/// The rects the last delivered frame changed, as
/// `count` packed `u16` quadruples `x, y, w, h` (frame pixels) written to
/// `out` (room for `max_rects`). A full frame reports one rect covering it.
/// Returns the rect count (may exceed `max_rects`: only the first
/// `max_rects` are written), 0 before any frame, -1 on a null handle.
/// Shells that ignore this keep redrawing the whole frame, which is correct.
///
/// # Safety
/// `s` must be valid; `out` writable for `max_rects * 4` u16s (or null with
/// `max_rects == 0`).
#[no_mangle]
pub unsafe extern "C" fn ov_screen_last_dirty_rects(
    s: *mut OvScreen,
    out: *mut u16,
    max_rects: usize,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        let screen = match s.as_ref() {
            Some(s) => s,
            None => return -1,
        };
        let rects = screen.dec.last_dirty();
        if !out.is_null() {
            for (i, r) in rects.iter().take(max_rects).enumerate() {
                let q = [r.x, r.y, r.w, r.h];
                std::ptr::copy_nonoverlapping(q.as_ptr(), out.add(i * 4), 4);
            }
        }
        rects.len().min(i32::MAX as usize) as i32
    })
}

/// Zero-copy framebuffer visitor: `pixels` points at the
/// assembler's persistent framebuffer (`len` bytes, `height` rows of
/// `stride` bytes, `format` 0 = BGRA / 1 = BGR), `rects` at `rect_count`
/// packed `u16` quadruples `x, y, w, h` — the rects the last delivered frame
/// changed. `rect_count == 0` means "the whole frame" (a full refresh was
/// asked for, or the last update had no rect list). Everything is valid only
/// for the duration of the call.
pub type FramebufferVisitor = extern "C" fn(
    user: *mut c_void,
    pixels: *const u8,
    len: usize,
    width: u32,
    height: u32,
    stride: u32,
    format: u8,
    seq: u64,
    rects: *const u16,
    rect_count: usize,
);

/// Zero-copy frames: call `visit` once, synchronously, with a
/// pointer to the current framebuffer plus the rects the last delivered
/// frame changed (`full != 0`: no rects — the shell wants everything, e.g.
/// a fresh texture). The shell uploads straight from the pointer (only the
/// dirty rects) instead of copying the whole framebuffer per update. Call
/// it right after the `ov_screen_feed` whose frame sink fired (or any time
/// for a full refresh) on the thread that feeds the screen — never from
/// inside a sink. Returns 0 (visited), -2 (no framebuffer yet), -1 (null
/// handle / visitor).
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`]; `visit` must
/// not call back into this screen.
#[no_mangle]
pub unsafe extern "C" fn ov_screen_visit_framebuffer(
    s: *mut OvScreen,
    user: *mut c_void,
    visit: Option<FramebufferVisitor>,
    full: i32,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        let (Some(screen), Some(visit)) = (s.as_ref(), visit) else {
            return -1;
        };
        let Some(fb) = screen.dec.frame() else {
            return -2;
        };
        let rects: Vec<u16> = if full != 0 {
            Vec::new()
        } else {
            screen
                .dec
                .last_dirty()
                .iter()
                .flat_map(|r| [r.x, r.y, r.w, r.h])
                .collect()
        };
        visit(
            user,
            fb.pixels.as_ptr(),
            fb.pixels.len(),
            fb.width,
            fb.height,
            fb.stride,
            fb.format.tag(),
            fb.seq,
            if rects.is_empty() {
                std::ptr::null()
            } else {
                rects.as_ptr()
            },
            rects.len() / 4,
        );
        0
    })
}

impl OvScreen {
    /// Map the shell's clock (`now_ms`, from its own epoch) onto this
    /// screen's real monotonic clock, which `record_inbound` / RTT use. The
    /// shell's epoch is its session controller's, which outlives this screen
    /// across reconnects; using `self.epoch + now_ms` directly skewed every
    /// liveness/keepalive check by the length of the earlier connection
    /// (a permanent "Reconnecting…" after an auto-reconnect). The first tick
    /// pins the offset; later ticks advance by the shell's elapsed time.
    fn shell_now(&mut self, now_ms: i64) -> std::time::Instant {
        let now_ms = now_ms.max(0);
        let (base_ms, base) = *self
            .shell_clock_base
            .get_or_insert_with(|| (now_ms, std::time::Instant::now().max(self.epoch)));
        let elapsed = (now_ms - base_ms).max(0) as u64;
        base + std::time::Duration::from_millis(elapsed)
    }

    /// Queue our CAPS once the peer turned out to be streaming a screen.
    fn queue_caps(&mut self) {
        if !self.caps_queued {
            self.caps_queued = true;
            let flags = self.local_caps();
            self.queue_small(&ovapp::terminal::Command::Caps { flags });
        }
    }

    /// The CAPS we advertise: tiles/zstd/keyframes and LZ4 block decode
    /// always (all pure Rust), `CAP_OPUS` only when the shell
    /// installed an Opus sink (it decodes Opus natively).
    fn local_caps(&self) -> u32 {
        // Lossy JPEG atlases + tile cache decode in the shared
        // ScreenDecoder (pure Rust), dirty rects via last_dirty.
        let mut flags = ovapp::terminal::LOCAL_CAPS
            | ovapp::terminal::CAP_LZ4
            | ovapp::terminal::VIEWER_W2_CAPS;
        if self.opus_sink.f.is_some() {
            // The iOS player reads the packet duration from the Opus TOC,
            // so 10 ms packets are fine too.
            flags |= ovapp::terminal::CAP_OPUS | ovapp::terminal::CAP_AUDIO10;
        }
        if self.video_sink.f.is_some() {
            flags |= ovapp::terminal::CAP_H264;
        }
        if self.direct_capable {
            // The session's direct engine speaks the v2 framing and
            // the media channel (video FEC + Opus over unreliable datagrams).
            flags |= ovapp::terminal::CAP_DIRECT | ovapp::terminal::CAP_MEDIA;
        }
        flags
    }

    /// Queue a VIDEO_IDR_REQUEST (rate limited, at most one queued).
    fn request_idr(&mut self) {
        let now = std::time::Instant::now();
        if self
            .last_idr_request
            .is_some_and(|t| now.saturating_duration_since(t) < ovapp::terminal::KEYFRAME_RETRY)
        {
            return;
        }
        let op = ovapp::terminal::OpCode::VideoIdrRequest as u8;
        if self.outbox.iter().any(|r| r[0] == op) {
            return;
        }
        self.last_idr_request = Some(now);
        let have_seq = self.last_video_seq;
        self.queue_small(&ovapp::terminal::Command::VideoIdrRequest { have_seq });
    }

    /// A complete video unit: continuity check, then the sink.
    fn video_unit(&mut self, h: ovapp::video::VideoHeader, data: &[u8]) {
        use ovapp::video::VideoVerdict;
        let Some(f) = self.video_sink.f else {
            self.last_video_seq = h.seq;
            return; // never advertised CAP_H264
        };
        let verdict = self.video_rx.accept(&h);
        if verdict == VideoVerdict::Stale {
            return; // Lost the race between media and reliable
        }
        self.last_video_seq = h.seq;
        if verdict == VideoVerdict::Drop {
            self.request_idr();
            return;
        }
        let au = if verdict == VideoVerdict::End {
            ovapp::video::AccessUnit::default()
        } else {
            ovapp::video::AccessUnit::from_annexb(data)
        };
        let keyframe = verdict == VideoVerdict::Deliver && h.is_keyframe();
        if keyframe && (au.sps.is_none() || au.pps.is_none()) {
            // A keyframe the shell cannot build a format description from.
            self.video_rx.mark_broken();
            self.request_idr();
            return;
        }
        if let Some(c) = au.sps.as_deref().and_then(ovapp::video::sps::colour) {
            self.video_colour = if c.colour_description_present {
                [c.primaries, c.transfer, c.matrix, c.full_range as u8]
            } else {
                // Unspecified: what the host's converter produces.
                [1, 1, 1, c.full_range as u8]
            };
        }
        let (sps, sps_len) = au
            .sps
            .as_deref()
            .map_or((std::ptr::null(), 0), |p| (p.as_ptr(), p.len()));
        let (pps, pps_len) = au
            .pps
            .as_deref()
            .map_or((std::ptr::null(), 0), |p| (p.as_ptr(), p.len()));
        let pkt = OvVideoPacket {
            seq: h.seq,
            pts_us: h.pts_us,
            flags: if verdict == VideoVerdict::End {
                OV_VIDEO_END
            } else if keyframe {
                OV_VIDEO_KEYFRAME
            } else {
                0
            },
            x: u32::from(h.rect.x),
            y: u32::from(h.rect.y),
            w: u32::from(h.rect.w),
            h: u32::from(h.rect.h),
            screen_w: h.screen_w,
            screen_h: h.screen_h,
            reserved: 0,
            sps: if keyframe { sps } else { std::ptr::null() },
            sps_len: if keyframe { sps_len } else { 0 },
            pps: if keyframe { pps } else { std::ptr::null() },
            pps_len: if keyframe { pps_len } else { 0 },
            data: if au.avcc.is_empty() {
                std::ptr::null()
            } else {
                au.avcc.as_ptr()
            },
            data_len: au.avcc.len(),
            colour_primaries: self.video_colour[0],
            transfer: self.video_colour[1],
            matrix: self.video_colour[2],
            full_range: self.video_colour[3],
        };
        f(self.video_sink.user, &pkt);
    }

    fn queue_small(&mut self, cmd: &ovapp::terminal::Command) {
        let wire = ovapp::terminal::encode(cmd);
        if let Ok(rec) = <[u8; ovapp::terminal::SMALL_CONTROL_LEN]>::try_from(wire.as_slice()) {
            self.outbox.push_back(rec);
        }
    }

    /// The framebuffer is stale: ask a capable host for a keyframe (rate
    /// limited, at most one queued).
    fn desync(&mut self) {
        if self.peer_caps & ovapp::terminal::CAP_KEYFRAME == 0 {
            return;
        }
        let now = std::time::Instant::now();
        if self
            .last_kf_request
            .is_some_and(|t| now.saturating_duration_since(t) < ovapp::terminal::KEYFRAME_RETRY)
        {
            return;
        }
        let op = ovapp::terminal::OpCode::KeyframeRequest as u8;
        if self.outbox.iter().any(|r| r[0] == op) {
            return;
        }
        self.last_kf_request = Some(now);
        let have_seq = self.dec.seq().unwrap_or(0);
        self.queue_small(&ovapp::terminal::Command::KeyframeRequest { have_seq });
    }
}

/// Feed one terminal-channel command record. Returns 0 on success, -1 on a
/// null/undecodable command. A completed frame fires the frame sink inline.
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`]. The screen is
/// not internally locked: no other call on the same handle may run
/// concurrently (the shell drives it from one serial queue). `data` must be
/// readable for `len` bytes (or null with `len == 0`). Never call it from
/// inside one of this screen's sinks.
#[no_mangle]
pub unsafe extern "C" fn ov_screen_feed(s: *mut OvScreen, data: *const u8, len: usize) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        let screen = match s.as_mut() {
            Some(s) => s,
            None => return -1,
        };
        if data.is_null() && len > 0 {
            return -1;
        }
        let bytes: Cow<'_, [u8]> = if len == 0 {
            Cow::Borrowed(&[])
        } else {
            Cow::Borrowed(std::slice::from_raw_parts(data, len))
        };
        let now = std::time::Instant::now();
        let cmd = match ovapp::terminal::decode(&bytes) {
            Ok(c) => c,
            Err(_) => return -1,
        };
        match cmd {
            ovapp::terminal::Command::FrameBegin {
                seq,
                width,
                height,
                stride,
                format,
                total,
                codec,
                compressed_len,
            } => {
                screen.queue_caps();
                screen.rx = Some((
                    seq,
                    ovapp::tiles::UpdateMeta {
                        width,
                        height,
                        stride,
                        format,
                        total,
                        codec,
                        compressed_len,
                        delta: None,
                    },
                ));
                screen.rx_video = None;
                screen.buf.clear();
                screen.buf.reserve(compressed_len as usize);
            }
            ovapp::terminal::Command::TilesBegin {
                seq,
                base_seq,
                width,
                height,
                format,
                codec,
                total,
                compressed_len,
                rects,
            } => {
                screen.queue_caps();
                screen.rx = Some((
                    seq,
                    ovapp::tiles::UpdateMeta {
                        width,
                        height,
                        stride: width.saturating_mul(format.bytes_per_pixel()),
                        format,
                        total,
                        codec,
                        compressed_len,
                        delta: Some(ovapp::tiles::Delta { base_seq, rects }),
                    },
                ));
                screen.rx_video = None;
                screen.buf.clear();
                screen.buf.reserve(compressed_len as usize);
            }
            // An H.264 access unit (only after our CAP_H264).
            ovapp::terminal::Command::VideoBegin {
                header,
                compressed_len,
            } => {
                screen.queue_caps();
                screen.rx = None;
                screen.rx_video = Some((header, compressed_len));
                screen.buf.clear();
                screen.buf.reserve(compressed_len.min(8 << 20) as usize);
            }
            ovapp::terminal::Command::FrameData(chunk) => {
                if screen.rx.is_none() && screen.rx_video.is_none() {
                    return -1; // FRAME_DATA before FRAME_BEGIN
                }
                screen.buf.extend_from_slice(&chunk);
            }
            ovapp::terminal::Command::FrameEnd(seq) => {
                if let Some((h, len)) = screen.rx_video.take() {
                    screen.live.record_inbound(now);
                    let n = len as usize;
                    if seq != h.seq || screen.buf.len() < n {
                        screen.buf.clear();
                        screen.last_video_seq = h.seq;
                        screen.video_rx.mark_broken();
                        if screen.video_sink.f.is_some() {
                            screen.request_idr();
                        }
                        return -1;
                    }
                    screen.stats.frame_rx(now, n as u64);
                    let data = std::mem::take(&mut screen.buf);
                    screen.video_unit(h, &data[..n]);
                    screen.buf = data;
                    screen.buf.clear();
                    return 0;
                }
                let Some((open_seq, meta)) = screen.rx.take() else {
                    return -1;
                };
                screen.live.record_inbound(now);
                let n = meta.compressed_len as usize;
                if seq != open_seq || screen.buf.len() < n {
                    // Torn/short update: the delta chain is broken.
                    screen.buf.clear();
                    screen.desync();
                    return -1;
                }
                screen.buf.truncate(n);
                screen.stats.frame_rx(now, u64::from(meta.total));
                // Decode (DEFLATE / zstd / raw) and apply onto the persistent
                // framebuffer; a corrupt update or a delta on the wrong base
                // leaves it untouched (never a torn frame) and asks for a
                // keyframe.
                let res = screen.dec.apply(&meta, seq, &screen.buf);
                screen.buf.clear();
                if res.is_err() {
                    screen.desync();
                    return -1;
                }
                if let (Some(f), Some(fb)) = (screen.sink.f, screen.dec.frame()) {
                    f(
                        screen.sink.user,
                        fb.seq,
                        fb.width,
                        fb.height,
                        fb.stride,
                        fb.format.tag(),
                        fb.pixels.as_ptr(),
                        fb.pixels.len(),
                    );
                }
            }
            // CAPS: the host's capability reply.
            ovapp::terminal::Command::Caps { flags } => {
                screen.live.record_inbound(now);
                screen.peer_caps = flags;
            }
            // ECHO / TYPE / FILE_* / PING / PONG ride the same channel.
            // Liveness: any inbound record counts as fresh traffic; a
            // PONG closes the in-flight PING and measures the RTT; a PING is
            // answered by the shell via [`ov_screen_take_pong_reply`]. Frames
            // feed the FPS/bandwidth stats.
            ovapp::terminal::Command::Pong(seq) => {
                screen.live.record_inbound(now);
                // RTT is measured against the real send instant (feed has no
                // shell clock); the shell ms only paces the keepalive.
                if let Some(sent_at) = screen.ping_sent_real.take() {
                    let rtt = std::time::Instant::now().saturating_duration_since(sent_at);
                    if screen.keepalive.on_pong(seq, sent_at + rtt).is_some() {
                        screen.last_rtt_ms = rtt.as_millis().min(i32::MAX as u128) as i32;
                    }
                }
            }
            ovapp::terminal::Command::Ping(nonce) => {
                screen.live.record_inbound(now);
                screen.pong_reply_nonce = Some(nonce);
            }
            ovapp::terminal::Command::Echo(_) | ovapp::terminal::Command::Type { .. } => {
                screen.live.record_inbound(now);
            }
            ovapp::terminal::Command::MonitorsInfo { monitors } => {
                screen.live.record_inbound(now);
                screen.queue_caps();
                if let Some(f) = screen.monitors_sink.f {
                    // Packed 17-byte records: i32 x, i32 y, u32 w, u32 h, u8
                    // is_primary (big-endian, matching the wire layout). The iOS
                    // onMonitors unpack MUST use the same stride (17) — a mismatch
                    // misaligns every record after the first on multi-monitor hosts.
                    let mut packed = Vec::with_capacity(monitors.len() * 17);
                    for m in &monitors {
                        packed.extend_from_slice(&m.x.to_be_bytes());
                        packed.extend_from_slice(&m.y.to_be_bytes());
                        packed.extend_from_slice(&m.w.to_be_bytes());
                        packed.extend_from_slice(&m.h.to_be_bytes());
                        packed.push(u8::from(m.is_primary));
                    }
                    f(screen.monitors_sink.user, packed.as_ptr(), monitors.len());
                }
            }
            ovapp::terminal::Command::Cursor { x, y } => {
                if let Some(f) = screen.cursor_sink.f {
                    f(screen.cursor_sink.user, x, y);
                }
            }
            ovapp::terminal::Command::CursorShape {
                hotspot_x,
                hotspot_y,
                w,
                h,
                bgra,
            } => {
                if let Some(f) = screen.cursor_shape_sink.f {
                    f(
                        screen.cursor_shape_sink.user,
                        hotspot_x,
                        hotspot_y,
                        w,
                        h,
                        bgra.as_ptr(),
                        bgra.len(),
                    );
                }
            }
            ovapp::terminal::Command::Clipboard { kind, text } => {
                screen.live.record_inbound(now);
                if let Some(f) = screen.clipboard_sink.f {
                    f(screen.clipboard_sink.user, kind, text.as_ptr(), text.len());
                }
            }
            // File transfer: the reassembly mirrors the desktop
            // `TerminalSession` (chunks stay out of any command hook; the completed
            // file is surfaced through the file sink). Malformed inbound transfer
            // data is dropped and reported as -1, like a corrupt frame.
            ovapp::terminal::Command::FileXferData { .. }
            | ovapp::terminal::Command::FileXferComplete { .. } => {
                screen.live.record_inbound(now);
                let malformed = screen.xfer.poll(&cmd).is_err();
                while let Some(file) = screen.xfer.take() {
                    if let Some(f) = screen.file_sink.f {
                        let name = ovapp::file_transfer::FileTransfer::sanitize_name(&file.name);
                        if let Ok(cname) = std::ffi::CString::new(name) {
                            f(
                                screen.file_sink.user,
                                cname.as_ptr(),
                                file.data.as_ptr(),
                                file.data.len(),
                            );
                        } // unreachable Err: sanitize removes control characters
                    }
                }
                if malformed {
                    return -1;
                }
            }
            ovapp::terminal::Command::Audio(payload) => {
                screen.live.record_inbound(now);
                // Raw PCM (codec 0): the sink gets the s16le sample bytes (the
                // shell converts to Float32). Opus (codec 1, only sent after our
                // CAP_OPUS): the packet is passed through to the Opus sink.
                match ovapp::audio::parse_audio_frame(payload.as_slice()) {
                    Ok((_, ovapp::audio::AudioPayload::Pcm(samples))) => {
                        if let Some(f) = screen.audio_sink.f {
                            let ptr = samples.as_ptr().cast::<u8>();
                            let n = samples.len() * 2;
                            f(screen.audio_sink.user, ptr, n);
                        }
                    }
                    Ok((_, ovapp::audio::AudioPayload::Opus { samples, packet })) => {
                        if let Some(f) = screen.opus_sink.f {
                            f(
                                screen.opus_sink.user,
                                packet.as_ptr(),
                                packet.len(),
                                samples,
                            );
                        }
                    }
                    Err(_) => {}
                }
            }
            // A whole access unit from the direct path's media channel
            // (the session synthesizes it; never mixed into `buf`, so a tile
            // update in progress is untouched).
            ovapp::terminal::Command::VideoUnit { header, data } => {
                screen.live.record_inbound(now);
                screen.stats.frame_rx(now, data.len() as u64);
                screen.video_unit(header, &data);
            }
            // Media lost for good. Video: drop P-frames until the next
            // IDR and ask for one. Audio: the Opus sink gets a NULL packet with
            // the lost sample count (the shell conceals / skips that span).
            ovapp::terminal::Command::MediaGap { kind, samples, .. } => {
                if kind == ovapp::terminal::GAP_VIDEO {
                    screen.video_rx.mark_broken();
                    if screen.video_sink.f.is_some() {
                        screen.request_idr();
                    }
                } else if kind == ovapp::terminal::GAP_AUDIO {
                    if let Some(f) = screen.opus_sink.f {
                        f(screen.opus_sink.user, std::ptr::null(), 0, samples);
                    }
                }
            }
            _ => {}
        }
        0
    })
}

#[cfg(test)]
mod direct_tests;
#[cfg(test)]
mod header_tests;
#[cfg(test)]
mod pin_tests;
#[cfg(test)]
mod relay_tests;
#[cfg(test)]
mod tests;
