//! `ov_session_*`: session lifecycle, sinks, the handshake, and records.
//!
//! Every entry point here takes a session pointer `s`. Unless stated
//! otherwise its `# Safety` contract is the same: `s` must be null or a live
//! handle from [`ov_session_create`] that is not freed while the call runs.

use std::collections::VecDeque;
use std::ffi::{c_char, c_void};
use std::sync::{Mutex, MutexGuard};

use ovcore::handshake::{Auth, Handshake, Role};
use ovcore::identity::Identity;
use ovcore::session::Session;

use crate::cabi::{bytes, cstr, write};
use crate::callbacks::{
    DatagramSink, DirectPathSink, DirectSendSink, RecordSink, Sink, StateSink, WakeSink,
};
use crate::direct::FfiDirect;
use crate::error::{err_from_transport, OV_ERR_PANIC};
use crate::media::{caps_of, maybe_enable_media};
use crate::pump::{pump_inner, send_record_bytes};
use crate::transport::{FfiDatagramCarrier, Transport};

/// Mutable session state behind [`OvSession`]'s mutex.
pub(crate) struct Inner {
    pub(crate) identity: Identity,
    pub(crate) device_id: u64,
    pub(crate) is_server: bool,
    /// Own pairing code (server validates against it; client presents peer's).
    pub(crate) pairing_code: String,
    /// Stretched access PIN: server validates the peer's
    /// proof against it; client proves knowledge of it. `None` = PIN unused.
    pub(crate) pin_psk: Option<[u8; 32]>,
    pub(crate) handshake: Option<Handshake>,
    pub(crate) session: Option<Session>,
    /// Peer fingerprint, cached at establishment (the handshake is dropped
    /// after `take_session`).
    pub(crate) peer_fp: Option<[u8; 16]>,
    pub(crate) transport: Transport,
    /// Inbound datagrams from the Rust-owned relay carrier.
    pub(crate) relay_inbound: VecDeque<Vec<u8>>,
    pub(crate) datagram_sink: Sink<DatagramSink>,
    pub(crate) record_sink: Sink<RecordSink>,
    pub(crate) state_sink: Sink<StateSink>,
    /// Direct-path sinks (set = direct-capable) and the engine
    /// (created at establishment over a relay transport).
    pub(crate) direct_send_sink: Sink<DirectSendSink>,
    pub(crate) direct_path_sink: Sink<DirectPathSink>,
    pub(crate) direct: Option<FfiDirect>,
    /// The relay closed while the direct path carried the session.
    pub(crate) relay_dead: bool,
    /// The handshake failed with `RateLimited`: seconds until the host
    /// accepts another PIN attempt.
    pub(crate) retry_after_secs: Option<u32>,
    /// Event-driven relay receive: woken when the relay socket has bytes.
    pub(crate) wake_sink: Sink<WakeSink>,
    /// The CAPS this session advertised (peeked from the outgoing
    /// channel-0 CAPS record the shell sends) and the peer's (peeked from
    /// the incoming one): the media channel opens when both carry
    /// CAP_MEDIA.
    pub(crate) local_caps: Option<u32>,
    pub(crate) peer_caps: Option<u32>,
}

/// Opaque session handle handed to C.
pub struct OvSession {
    pub(crate) inner: Mutex<Inner>,
}

/// Lock the session; a poisoned mutex (a caught panic) reports -2.
pub(crate) fn lock(s: &OvSession) -> Result<MutexGuard<'_, Inner>, i32> {
    s.inner.lock().map_err(|_| -2)
}

/// device_id from the fingerprint's low bytes (same recipe as ovview/ovhost):
/// `[0, 0, 0, fp[0..5]]` as a u64.
fn device_id_from_fp(fp: [u8; 16]) -> u64 {
    u64::from_be_bytes([0, 0, 0, fp[0], fp[1], fp[2], fp[3], fp[4]])
}

/// Create a session.
///
/// * `is_server`: 0 = client (dials), 1 = server (listens).
/// * `pairing_code`: the attended host's one-time 10-digit code
///   (`ovcore::pairing`; grouping spaces / dashes are ignored): for a
///   server, **our own** code; for a client, **the peer's** code we are
///   dialing. It is the SPAKE2 password and never crosses the wire; its
///   first four digits name the relay slot (`ov_relay_session_id`).
///   Unattended sessions pass any placeholder and use `ov_session_begin_pin`.
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
        // SAFETY: `pairing_code` is null or a NUL-terminated string (caller
        // contract); it is copied before returning.
        let code = unsafe { cstr(pairing_code) };
        let Some(code) = code.filter(|c| !c.is_empty()) else {
            return core::ptr::null_mut();
        };
        let identity = Identity::generate();
        let device_id = device_id_from_fp(identity.fingerprint());
        let inner = Inner {
            identity,
            device_id,
            is_server: is_server != 0,
            pairing_code: ovcore::pairing::normalize(code).unwrap_or_else(|| code.to_string()),
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
/// `s` must be null or a pointer from [`ov_session_create`] that was not
/// freed yet; it must not be used afterwards.
#[no_mangle]
pub unsafe extern "C" fn ov_session_free(s: *mut OvSession) {
    ffi_guard!((), {
        if !s.is_null() {
            // SAFETY: `s` came from `Box::into_raw` in `ov_session_create`
            // and is freed only once (caller contract).
            drop(unsafe { Box::from_raw(s) });
        }
    })
}

/// Wire the datagram transport. `datagram_sink` receives every outbound
/// datagram (handshake + records) on the pump thread. Returns 0 or -1.
///
/// # Safety
/// `s` must be null or a live session; `user`/`datagram_sink` must outlive
/// the session.
#[no_mangle]
pub unsafe extern "C" fn ov_session_set_datagram_sink(
    s: *mut OvSession,
    user: *mut c_void,
    datagram_sink: DatagramSink,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        // SAFETY: `s` is null or a live session (caller contract).
        let Some(s) = (unsafe { s.as_ref() }) else {
            return -1;
        };
        match lock(s) {
            Ok(mut g) => {
                g.datagram_sink = Sink::new(Some(datagram_sink), user);
                if matches!(g.transport, Transport::None) {
                    g.transport = Transport::Datagram(Box::new(FfiDatagramCarrier::new(
                        Sink::new(Some(datagram_sink), user),
                    )));
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
/// `s` must be null or a live session; `data` must be readable for `len`
/// bytes (or null with `len == 0`).
#[no_mangle]
pub unsafe extern "C" fn ov_session_on_datagram(
    s: *mut OvSession,
    data: *const u8,
    len: usize,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        // SAFETY: `s` is null or a live session; `data` is readable for
        // `len` bytes for the whole call (caller contract).
        let args = unsafe { (s.as_ref(), bytes(data, len)) };
        let (Some(s), Some(bytes)) = args else {
            return -1;
        };
        match lock(s) {
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
/// `s` must be null or a live session.
#[no_mangle]
pub unsafe extern "C" fn ov_session_begin(s: *mut OvSession, auth_mode: i32) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        // SAFETY: `s` is null or a live session (caller contract).
        let Some(s) = (unsafe { s.as_ref() }) else {
            return -1;
        };
        begin(s, auth_mode, None)
    })
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
/// `s` must be null or a live session; `pin` readable for `pin_len` bytes
/// (or null with `pin_len == 0`).
#[no_mangle]
pub unsafe extern "C" fn ov_session_begin_pin(
    s: *mut OvSession,
    pin: *const u8,
    pin_len: usize,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        if pin_len == 0 {
            return -1;
        }
        // SAFETY: `s` is null or a live session; `pin` is readable for
        // `pin_len` bytes for the whole call (caller contract).
        let args = unsafe { (s.as_ref(), bytes(pin, pin_len)) };
        let (Some(s), Some(pin)) = args else {
            return -1;
        };
        let psk = ovcore::pin::pin_psk(pin);
        begin(s, 2, Some(psk))
    })
}

/// Shared body of [`ov_session_begin`] / [`ov_session_begin_pin`].
fn begin(s: &OvSession, auth_mode: i32, psk: Option<[u8; 32]>) -> i32 {
    lock(s)
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
/// `s` must be null or a live session.
#[no_mangle]
pub unsafe extern "C" fn ov_session_pump(s: *mut OvSession) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        // SAFETY: `s` is null or a live session (caller contract).
        let Some(s) = (unsafe { s.as_ref() }) else {
            return -1;
        };
        lock(s)
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
/// `s` must be null or a live session; `data` readable for `len` bytes (or
/// null with `len == 0`).
#[no_mangle]
pub unsafe extern "C" fn ov_session_send_record(
    s: *mut OvSession,
    channel: u8,
    data: *const u8,
    len: usize,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        // SAFETY: `s` is null or a live session; `data` is readable for
        // `len` bytes for the whole call (caller contract).
        let args = unsafe { (s.as_ref(), bytes(data, len)) };
        let (Some(s), Some(payload)) = args else {
            return -1;
        };
        lock(s)
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
/// `s` must be null or a live session; callback/user must outlive the
/// session.
#[no_mangle]
pub unsafe extern "C" fn ov_session_set_record_sink(
    s: *mut OvSession,
    user: *mut c_void,
    record_sink: RecordSink,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        // SAFETY: `s` is null or a live session (caller contract).
        let Some(s) = (unsafe { s.as_ref() }) else {
            return -1;
        };
        match lock(s) {
            Ok(mut g) => {
                g.record_sink = Sink::new(Some(record_sink), user);
                0
            }
            Err(e) => e,
        }
    })
}

/// Register the state sink. `state = 1` = established.
///
/// # Safety
/// `s` must be null or a live session; callback/user must outlive the
/// session.
#[no_mangle]
pub unsafe extern "C" fn ov_session_set_state_sink(
    s: *mut OvSession,
    user: *mut c_void,
    state_sink: StateSink,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        // SAFETY: `s` is null or a live session (caller contract).
        let Some(s) = (unsafe { s.as_ref() }) else {
            return -1;
        };
        match lock(s) {
            Ok(mut g) => {
                g.state_sink = Sink::new(Some(state_sink), user);
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
/// `s` must be null or a live session; `out` null or writable for 16 bytes.
#[no_mangle]
pub unsafe extern "C" fn ov_session_peer_fingerprint(s: *mut OvSession, out: *mut u8) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        if out.is_null() {
            return -1;
        }
        // SAFETY: `s` is null or a live session (caller contract).
        let Some(s) = (unsafe { s.as_ref() }) else {
            return -1;
        };
        match lock(s) {
            Ok(g) => match g
                .peer_fp
                .or_else(|| g.handshake.as_ref()?.peer().map(|p| p.fingerprint))
            {
                Some(fp) => {
                    // SAFETY: `out` is non-null and writable for 16 bytes
                    // (caller contract).
                    unsafe { write(&fp, out) };
                    0
                }
                None => -2,
            },
            Err(e) => e,
        }
    })
}

/// Seconds until the host accepts another PIN attempt, after a pump
/// returned [`OV_ERR_RATE_LIMITED`](crate::OV_ERR_RATE_LIMITED); -1 when the
/// session was not rate limited (or on a null session).
///
/// # Safety
/// `s` must be null or a live session.
#[no_mangle]
pub unsafe extern "C" fn ov_session_retry_after_secs(s: *mut OvSession) -> i64 {
    ffi_guard!(-1, {
        // SAFETY: `s` is null or a live session (caller contract).
        let Some(s) = (unsafe { s.as_ref() }) else {
            return -1;
        };
        lock(s)
            .ok()
            .and_then(|g| g.retry_after_secs)
            .map_or(-1, i64::from)
    })
}

/// Build a SESSION_OPTION record payload (`key` 1 = hide the
/// host wallpaper, 2 = disable host animations; `value` 0/1) into `out`
/// (>= 3 bytes) for `ov_session_send_record(channel 0)`. Only meaningful
/// when `ov_screen_peer_caps` has `CAP_DESKTOP_FX` (1 << 8). Returns the
/// length (3) or -1.
///
/// # Safety
/// `out` must be null or writable for `cap` bytes.
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
        // SAFETY: `out` is non-null and writable for `cap >= 3` bytes (caller
        // contract), and a SESSION_OPTION record is 3 bytes.
        unsafe { write(&b, out) };
        b.len() as i32
    })
}
