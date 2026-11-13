//! The direct UDP path: the [`ovtransport::direct::DirectEngine`] running
//! over shell-owned per-candidate paths, with the relay as primary.
//!
//! Every entry point here takes a session pointer `s`; `s` must be null or a
//! live handle from [`ov_session_create`](crate::ov_session_create) that is
//! not freed while the call runs.

use std::collections::VecDeque;
use std::ffi::c_void;

use ovtransport::TransportError;

use crate::cabi::bytes;
use crate::callbacks::{DirectPathSink, DirectSendSink, Sink};
use crate::error::OV_ERR_PANIC;
use crate::session::{lock, Inner, OvSession};
use crate::transport::Transport;

/// Max datagrams queued from the shell's direct paths between pumps.
const DIRECT_INBOUND_CAP: usize = 65_536;

/// The direct-path engine over shell-owned per-candidate paths.
pub(crate) struct FfiDirect {
    pub(crate) engine: ovtransport::direct::DirectEngine,
    /// Datagrams from the shell, `(path_id, bytes)`.
    pub(crate) inbound: VecDeque<(u32, Vec<u8>)>,
    /// Candidate addresses by path id.
    paths: Vec<String>,
}

/// The engine's I/O: the relay transport + the shell's direct send sink.
pub(crate) struct FfiDirectIo<'a> {
    pub(crate) transport: &'a mut Transport,
    pub(crate) sink: Sink<DirectSendSink>,
}

impl ovtransport::direct::DirectIo for FfiDirectIo<'_> {
    fn primary_send(&mut self, d: &[u8]) -> Result<(), TransportError> {
        self.transport.send(d).map(|_| ())
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

/// Create the direct engine once the session is established over a relay
/// with direct sinks installed.
pub(crate) fn init_direct(g: &mut Inner) {
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
pub(crate) fn start_direct_probe(g: &mut Inner, cands: &[(u8, std::net::SocketAddr)]) {
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
pub(crate) fn direct_state(g: &Inner) -> i32 {
    g.direct.as_ref().map_or(0, |d| d.engine.state() as i32)
}

/// Make the session direct-capable: install the per-path send sink and the
/// path-open sink. Call before the handshake completes (or any time over
/// an established relay session). Pair it with
/// [`ov_screen_set_direct_capable`](crate::ov_screen_set_direct_capable) so
/// our CAPS advertise `CAP_DIRECT` — the host then offers its candidates,
/// `open_path` fires once per candidate, and probing / promotion / fallback
/// run inside the pump calls.
/// Returns 0 or -1.
///
/// # Safety
/// `s` must be null or a live session; `user`/the sinks must outlive the
/// session.
#[no_mangle]
pub unsafe extern "C" fn ov_session_set_direct_sinks(
    s: *mut OvSession,
    user: *mut c_void,
    send: DirectSendSink,
    open_path: DirectPathSink,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        // SAFETY: `s` is null or a live session (caller contract).
        let Some(s) = (unsafe { s.as_ref() }) else {
            return -1;
        };
        match lock(s) {
            Ok(mut g) => {
                g.direct_send_sink = Sink::new(Some(send), user);
                g.direct_path_sink = Sink::new(Some(open_path), user);
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
/// `s` must be null or a live session; `data` readable for `len` bytes (or
/// null with `len == 0`).
#[no_mangle]
pub unsafe extern "C" fn ov_session_on_direct_datagram(
    s: *mut OvSession,
    path_id: u32,
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
            Ok(mut g) => match g.direct.as_mut() {
                Some(d) => {
                    if d.inbound.len() < DIRECT_INBOUND_CAP {
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
/// `s` must be null or a live session.
#[no_mangle]
pub unsafe extern "C" fn ov_session_direct_state(s: *mut OvSession) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        // SAFETY: `s` is null or a live session (caller contract).
        let Some(s) = (unsafe { s.as_ref() }) else {
            return -1;
        };
        lock(s).map(|g| direct_state(&g)).unwrap_or(-1)
    })
}

/// Milliseconds until the engine next needs a pump (probe resend,
/// keepalive, retransmission); -1 = no timer pending. Shells wake their
/// pump loop at this deadline (or simply pump every ~20 ms while
/// `ov_session_direct_state` != 0).
///
/// # Safety
/// `s` must be null or a live session.
#[no_mangle]
pub unsafe extern "C" fn ov_session_direct_deadline_ms(s: *mut OvSession) -> i64 {
    ffi_guard!(-1, {
        // SAFETY: `s` is null or a live session (caller contract).
        let Some(s) = (unsafe { s.as_ref() }) else {
            return -1;
        };
        lock(s)
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
/// `s` must be null or a live session.
#[no_mangle]
pub unsafe extern "C" fn ov_session_direct_path(s: *mut OvSession) -> i64 {
    ffi_guard!(-1, {
        // SAFETY: `s` is null or a live session (caller contract).
        let Some(s) = (unsafe { s.as_ref() }) else {
            return -1;
        };
        lock(s)
            .ok()
            .and_then(|g| g.direct.as_ref()?.engine.active_path())
            .map_or(-1, i64::from)
    })
}
