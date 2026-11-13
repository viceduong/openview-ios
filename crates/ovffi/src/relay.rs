//! Relay transports: the Rust-owned TCP relay tunnel, the host-owned byte
//! stream bridge (WebSocket), the event-driven wake sink, and the relay pump.
//!
//! Every entry point here that takes a session pointer `s` requires it to be
//! null or a live handle from [`ov_session_create`](crate::ov_session_create)
//! that is not freed while the call runs.

use std::collections::VecDeque;
use std::ffi::{c_char, c_void};

use ovtransport::{BridgeWriter, Carrier, RelayTunnelCarrier, TransportError, TunnelRole};

use crate::cabi::{bytes, cstr, write};
use crate::callbacks::{DatagramSink, Sink, WakeSink};
use crate::direct::direct_state;
use crate::error::{is_handshake_code, OV_ERR_PANIC};
use crate::pump::pump_inner;
use crate::session::{lock, Inner, OvSession};
use crate::transport::Transport;

/// Max relay polls per [`ov_session_pump_relay`] call (each poll drains up to
/// 1 MiB from the socket), bounding one call's work under a flood.
const PUMP_RELAY_MAX_POLLS: usize = 8;

/// u32 BE from the first 4 bytes of a pairing code (relay session id).
pub(crate) fn relay_session_id(code: &str) -> u32 {
    let bytes = code.as_bytes();
    let mut b = [0u8; 4];
    let n = bytes.len().min(4);
    b[..n].copy_from_slice(&bytes[..n]);
    u32::from_be_bytes(b)
}

/// Resolve `host:port` to its first socket address.
fn resolve(host: &str, port: u16) -> Option<std::net::SocketAddr> {
    use std::net::ToSocketAddrs;
    (host, port).to_socket_addrs().ok()?.next()
}

/// The tunnel role for this session's side.
fn tunnel_role(g: &Inner) -> TunnelRole {
    if g.is_server {
        TunnelRole::Responder
    } else {
        TunnelRole::Initiator
    }
}

/// Connect the Rust-owned relay transport (blocking TCP). `relay_host` may be
/// a hostname or IP; `session_id` is the u32 relay session id (the ovview
/// recipe: first 4 bytes of the server's pairing code, BE — see
/// `ov_relay_session_id`).
/// Returns 0, -1 (bad args), -2 (already connected), -4 (transport failure).
///
/// # Safety
/// `s` must be null or a live session; `relay_host` null or a valid
/// NUL-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn ov_session_relay_connect(
    s: *mut OvSession,
    relay_host: *const c_char,
    relay_port: u16,
    session_id: u32,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        // SAFETY: `s` is null or a live session; `relay_host` is null or a
        // NUL-terminated string live for the whole call (caller contract).
        let args = unsafe { (s.as_ref(), cstr(relay_host)) };
        let (Some(s), Some(host)) = args else {
            return -1;
        };
        lock(s)
            .and_then(|mut g| {
                if !matches!(g.transport, Transport::None) {
                    return Err(-2);
                }
                let addr = resolve(host, relay_port).ok_or(-1)?;
                let carrier = RelayTunnelCarrier::connect(addr, session_id, tunnel_role(&g))
                    .map_err(|_| -4)?;
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
/// `s` must be null or a live session; `user`/`stream_sink` must outlive the
/// session.
#[no_mangle]
pub unsafe extern "C" fn ov_session_relay_connect_stream(
    s: *mut OvSession,
    user: *mut c_void,
    stream_sink: DatagramSink,
    session_id: u32,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        // SAFETY: `s` is null or a live session (caller contract).
        let Some(s) = (unsafe { s.as_ref() }) else {
            return -1;
        };
        let sink = Sink::new(Some(stream_sink), user);
        lock(s)
            .and_then(|mut g| {
                if !matches!(g.transport, Transport::None) {
                    return Err(-2);
                }
                let writer: BridgeWriter = Box::new(move |bytes: &[u8]| sink.emit_bytes(bytes));
                let carrier =
                    RelayTunnelCarrier::connect_bridge(writer, session_id, tunnel_role(&g))
                        .map_err(|_| -4)?;
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
/// `s` must be null or a live session; `data` must be readable for `len`
/// bytes (or null with `len == 0`).
#[no_mangle]
pub unsafe extern "C" fn ov_session_relay_feed(
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
/// `s` must be null or a live session.
#[no_mangle]
pub unsafe extern "C" fn ov_session_relay_eof(s: *mut OvSession) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        // SAFETY: `s` is null or a live session (caller contract).
        let Some(s) = (unsafe { s.as_ref() }) else {
            return -1;
        };
        match lock(s) {
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
/// `s` must be null or a live session; `user`/`sink` must stay valid until
/// the session is freed or the sink is removed.
#[no_mangle]
pub unsafe extern "C" fn ov_session_set_wake_sink(
    s: *mut OvSession,
    user: *mut c_void,
    sink: Option<WakeSink>,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        // SAFETY: `s` is null or a live session (caller contract).
        let Some(s) = (unsafe { s.as_ref() }) else {
            return -1;
        };
        match lock(s) {
            Ok(mut g) => {
                g.wake_sink = Sink::new(sink, user);
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
/// `code` must be null or a valid NUL-terminated C string; `out` null or
/// writable for 4 bytes.
#[no_mangle]
pub unsafe extern "C" fn ov_relay_session_id(code: *const c_char, out: *mut u8) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        // SAFETY: `code` is null or a NUL-terminated string live for the
        // whole call (caller contract).
        let Some(code) = (unsafe { cstr(code) }) else {
            return -1;
        };
        if out.is_null() {
            return -1;
        }
        let id = relay_session_id(code);
        // SAFETY: `out` is non-null and writable for 4 bytes (caller
        // contract).
        unsafe { write(&id.to_be_bytes(), out) };
        0
    })
}

/// Poll the relay transport: drains every available tunnel frame (incl.
/// relay PING/PONG and punch probes; bounded per call) and feeds record
/// datagrams through the normal decrypt path. Call this instead of
/// [`ov_session_pump`](crate::ov_session_pump) when using
/// [`ov_session_relay_connect`] / [`ov_session_relay_connect_stream`].
///
/// Returns 0, or a negative error: a handshake verdict as for
/// `ov_session_pump` (-3 auth failed, -7 rate limited, -8 incompatible
/// version, -9 other; it takes precedence over a relay close), -4 = relay
/// transport failure (I/O or relay protocol error), and
/// **-5 = relay closed** (peer DISCONNECT, relay EOF / `XT` rejection or
/// timeout, or [`ov_session_relay_eof`] after the buffered bytes were
/// drained). Datagrams received before the close are still processed (their
/// sinks fire) before -5 is returned; -5 is terminal — tear the session down.
///
/// # Safety
/// `s` must be null or a live session.
#[no_mangle]
pub unsafe extern "C" fn ov_session_pump_relay(s: *mut OvSession) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        // SAFETY: `s` is null or a live session (caller contract).
        let Some(s) = (unsafe { s.as_ref() }) else {
            return -1;
        };
        lock(s)
            .and_then(|mut g| pump_relay(&mut g))
            .unwrap_or_else(|code| code)
    })
}

/// Body of [`ov_session_pump_relay`] on the locked session.
fn pump_relay(g: &mut Inner) -> Result<i32, i32> {
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
    if matches!(poll_err, Some(-5) | Some(-4)) && direct_state(g) == 2 {
        g.relay_dead = true;
        poll_err = None;
        if let Some(d) = g.direct.as_mut() {
            d.engine.primary_lost(std::time::Instant::now());
        }
    }
    let pumped = pump_inner(g);
    if let Err(code) = pumped {
        if is_handshake_code(code) {
            return Err(code);
        }
    }
    if g.relay_dead && direct_state(g) != 2 {
        return Err(-5);
    }
    if let Some(code) = poll_err {
        return Err(code);
    }
    pumped.map(|_| 0)
}
