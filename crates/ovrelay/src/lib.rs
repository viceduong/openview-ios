//! `ovrelay` — the OpenView relay server.
//!
//! A thread-per-client TCP server that pipes two [`RelayTunnelCarrier`]s
//! (see `ovtransport::relay`) together when they announce the same u32
//! `session_id`, one as [`TunnelRole::Initiator`] and one as
//! [`TunnelRole::Responder`]. The relay is a *pure byte pipe* for tunnel
//! frames: DATA, PING and PONG are forwarded opaquely (the far-end carrier
//! auto-answers PING), and DISCONNECT is translated into a clean teardown.
//!
//! ```text
//! initiator --CONNECT(sid, 0x00)--> relay <--CONNECT(sid, 0x01)-- responder
//!      <--O--                       |                       -->O--
//!      <--A sid peer--              |               --A sid peer-->
//!      <======== opaque frames =====>        (thread-per-side pipe)
//! ```
//!
//! `A` also carries the *other* side's relay-observed IPv4 address and port
//! (6 bytes, zero for IPv6 / WebSocket peers; see `attach_line`).
//!
//! Sessions wait at most `timeout` seconds for their peer; afterwards the
//! pending client gets `b"XT" || sid` and is torn down. std only — one OS
//! thread per connected client; no async runtime.
//!
//! # Abuse limits ([`RelayLimits`])
//!
//! A public relay must not be exhaustible by one client: total and per-IP
//! connection caps are enforced at accept (excess connections are closed
//! before any thread is spawned), parked sessions are capped (excess
//! CONNECTs get `XT` *before* the `O` acknowledgement), and the pre-session
//! phase (HTTP upgrade + CONNECT line) has an overall deadline, not just a
//! per-read timeout, so a client dribbling bytes cannot pin a thread.
//! Loopback peers are exempt from the per-IP cap: behind a local reverse
//! proxy or tunnel (cloudflared) every client appears as 127.0.0.1.
//!
//! The relay only ever sees ciphertext: sessions are end-to-end encrypted
//! and authenticated by the `ovcore` handshake. The session id is a
//! rendezvous name, not a secret (see `docs/WIRE_PROTOCOL.md` §2.1).
//!
//! The same port also accepts WebSocket clients (`GET /relay` upgrade; see
//! `ws`): the identical byte stream travels inside binary messages, so a
//! client behind an HTTP-only front (a Cloudflare tunnel) can use the relay.
//! WebSocket and raw-TCP clients pair with each other transparently.
//!
//! [`RelayTunnelCarrier`]: ovtransport::RelayTunnelCarrier
//! [`TunnelRole::Initiator`]: ovtransport::TunnelRole::Initiator
//! [`TunnelRole::Responder`]: ovtransport::TunnelRole::Responder

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::HashMap;
use std::io::{self, Read};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ovtransport::{encode_frame, DISCONNECT, MAX_FRAME, PUNCH_ACK};

mod ws;

pub use ws::DEFAULT_WS_PATH;

/// Pipe read buffer: one read is forwarded as one write (one WebSocket
/// message for a WebSocket destination), so this also caps a message.
const PIPE_BUF: usize = 64 * 1024;

/// Read timeout for the pre-session phase (HTTP upgrade head + CONNECT
/// line): a client that connects and says nothing must not pin a thread.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// Resource caps of a relay (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelayLimits {
    /// Most simultaneously open client connections.
    pub max_connections: usize,
    /// Most simultaneously open connections per source IP (loopback exempt).
    pub max_per_ip: usize,
    /// Most parked (unpaired) sessions; `serve(Some(n))` overrides it.
    pub max_pending: usize,
    /// Overall deadline for the pre-session phase (upgrade + CONNECT).
    pub handshake_deadline: Duration,
}

impl RelayLimits {
    /// 1024 connections, 32 per IP, 512 pending, 20 s pre-session deadline.
    pub const DEFAULT: Self = Self {
        max_connections: 1024,
        max_per_ip: 32,
        max_pending: 512,
        handshake_deadline: Duration::from_secs(20),
    };
}

impl Default for RelayLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Open-connection accounting (total + per IP).
#[derive(Debug, Default)]
struct ConnTable {
    total: usize,
    per_ip: HashMap<IpAddr, usize>,
}

/// Releases one connection slot on drop (moved into the client thread).
struct ConnGuard {
    shared: Arc<Shared>,
    ip: IpAddr,
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        let mut t = lock(&self.shared.conns);
        t.total = t.total.saturating_sub(1);
        if let Some(n) = t.per_ip.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                t.per_ip.remove(&self.ip);
            }
        }
    }
}

impl Shared {
    /// Take a connection slot for `ip`, or `None` when a cap is reached.
    fn admit(self: &Arc<Self>, ip: IpAddr) -> Option<ConnGuard> {
        let mut t = lock(&self.conns);
        if t.total >= self.limits.max_connections {
            return None;
        }
        let n = t.per_ip.get(&ip).copied().unwrap_or(0);
        if !ip.is_loopback() && n >= self.limits.max_per_ip {
            return None;
        }
        t.total += 1;
        *t.per_ip.entry(ip).or_insert(0) += 1;
        Some(ConnGuard {
            shared: self.clone(),
            ip,
        })
    }

    /// Parked (unpaired) sessions right now.
    fn pending(&self) -> usize {
        lock(&self.sessions)
            .values()
            .filter(|(i, r)| i.is_none() || r.is_none())
            .count()
    }
}

/// A reader that refuses to read past an overall deadline (the socket's
/// per-read timeout alone lets a client dribble one byte per timeout).
struct DeadlineReader<'a, R: Read + ?Sized> {
    inner: &'a mut R,
    deadline: Instant,
}

impl<R: Read + ?Sized> Read for DeadlineReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if Instant::now() >= self.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "pre-session deadline exceeded",
            ));
        }
        self.inner.read(buf)
    }
}

/// What a client told the relay in its CONNECT line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ConnectRequest {
    session_id: u32,
    initiator: bool,
}

/// Errors the relay can surface. `thiserror`, matching the workspace style.
#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    /// Binding the listen socket failed.
    #[error("bind failed: {0}")]
    Bind(#[source] io::Error),
    /// A client connection failed I/O.
    #[error("client io error: {0}")]
    Io(#[source] io::Error),
    /// A client spoke garbage (bad CONNECT line, unknown control byte).
    #[error("protocol violation: {0}")]
    Protocol(String),
}

/// One half of a matched session: its TCP stream, guarded so the pipe thread
/// can write teardown frames to it while the peer's reader thread holds the
/// other end's state. `addr` is the relay's own view of this client's socket
/// (`getpeername` at accept time) — learned passively, never routed on.
struct Slot {
    /// Serializes `try_write` (a torn interleaved write from two pipe threads
    /// would corrupt the frame stream - observed as `unknown control byte`
    /// on the receiving carrier under concurrent DATA + auto-PONG traffic).
    /// Shared with a WebSocket reader, which writes PONGs. The guarded `Vec`
    /// is a reusable scratch buffer for WebSocket framing (no per-write
    /// allocation).
    write_lock: Arc<Mutex<Vec<u8>>>,
    stream: Mutex<Option<TcpStream>>,
    /// The client speaks WebSocket: writes are framed as binary messages.
    ws: bool,
    /// The byte-stream reader (a socket clone, or the WebSocket deframer).
    /// Taken by the CONNECT read and then by this side's pipe thread.
    reader: Mutex<Option<Box<dyn Read + Send>>>,
    /// The client's address as the relay sees it. `None` for a WebSocket
    /// client: it arrives through a proxy, so the socket address is the
    /// proxy's and useless as a punch target.
    addr: Option<SocketAddr>,
    /// Which side this is (initiator = viewer, responder = host), for
    /// the session-end log. Set at CONNECT.
    role: Mutex<&'static str>,
}

impl Slot {
    /// Best-effort write; a dead peer just loses the bytes (its own pipe
    /// direction notices the dead socket and tears the session down).
    fn try_write(&self, bytes: &[u8]) {
        let mut scratch = lock(&self.write_lock);
        if let Ok(mut guard) = self.stream.lock() {
            if let Some(s) = guard.as_mut() {
                use std::io::Write;
                let _ = if self.ws {
                    ws::encode_into(&mut scratch, ws::OP_BINARY, bytes);
                    s.write_all(&scratch)
                } else {
                    s.write_all(bytes)
                };
            }
        }
    }

    /// Send a WebSocket PING (keeps proxies / tunnels on the
    /// path - Cloudflare, VPN apps - from timing an idle-looking socket
    /// out; the client's stack answers it). No-op for raw TCP clients.
    fn try_ping(&self) {
        if !self.ws {
            return;
        }
        let _serial = lock(&self.write_lock);
        if let Ok(mut guard) = self.stream.lock() {
            if let Some(s) = guard.as_mut() {
                use std::io::Write;
                let _ = s.write_all(&ws::encode(ws::OP_PING, b"ov"));
            }
        }
    }

    fn label(&self) -> String {
        format!(
            "{} ({})",
            *lock(&self.role),
            if self.ws { "ws" } else { "tcp" }
        )
    }

    /// Close this side: WebSocket CLOSE (if applicable), then
    /// `shutdown(Both)` so every clone of the socket — including a pipe
    /// thread blocked in `read` — wakes with EOF instead of leaking, then
    /// drop the stream.
    fn close(&self) {
        let _serial = lock(&self.write_lock);
        if let Ok(mut guard) = self.stream.lock() {
            if let Some(s) = guard.as_mut() {
                if self.ws {
                    use std::io::Write;
                    let _ = s.write_all(&ws::close_frame());
                }
                let _ = s.shutdown(Shutdown::Both);
            }
            *guard = None;
        }
    }
}

/// Shared state between the accept loop and the pipe threads.
/// Per-session entry: each side is `None` until its client connects.
type Sessions = HashMap<u32, (Option<Arc<Slot>>, Option<Arc<Slot>>)>;

struct Shared {
    /// session_id -> (initiator slot, responder slot) for *pending* (parked,
    /// unpaired) sessions only: pairing removes the entry, so an active
    /// session leaves the id free for the next park.
    sessions: Mutex<Sessions>,
    /// Idle timeout for an unmatched CONNECT, and the peer-matching handshake.
    timeout: Duration,
    /// Connection / pending-session caps.
    limits: RelayLimits,
    /// Open connections (total + per IP).
    conns: Mutex<ConnTable>,
    /// Request paths accepted for a WebSocket upgrade.
    ws_paths: Vec<String>,
}

impl Shared {
    /// Register a CONNECT with the caller's own slot. Returns:
    /// - `Ok(Some(peer))` — both sides present, `peer` is the other side's
    ///   real slot; the caller pipes against it. The session entry is removed
    ///   right here, under the lock: the parked side sees it is no longer
    ///   parked (see [`Shared::unpark`]) and leaves the session alone.
    /// - `Ok(None)` — parked; the *later* connecting side gets the pair.
    /// - `Err(Protocol)` — role conflict.
    fn register(
        &self,
        req: ConnectRequest,
        my: Arc<Slot>,
    ) -> Result<Option<Arc<Slot>>, RelayError> {
        let mut sessions = lock(&self.sessions);
        let entry = sessions.entry(req.session_id).or_insert((None, None));
        let (mine, theirs) = if req.initiator {
            (&mut entry.0, &mut entry.1)
        } else {
            (&mut entry.1, &mut entry.0)
        };
        if mine.is_some() {
            return Err(RelayError::Protocol(format!(
                "session {} already has {}",
                req.session_id,
                if req.initiator {
                    "an initiator"
                } else {
                    "a responder"
                }
            )));
        }
        if let Some(peer) = theirs.take() {
            // Pair complete: our side was empty, theirs is now taken.
            sessions.remove(&req.session_id);
            return Ok(Some(peer));
        }
        *mine = Some(my);
        Ok(None)
    }

    /// `true` while `my` is still parked in its session entry (not yet
    /// claimed by a pairing peer).
    fn is_parked(&self, req: ConnectRequest, my: &Arc<Slot>) -> bool {
        let sessions = lock(&self.sessions);
        Self::side_holds(&sessions, req, my)
    }

    /// Remove `my` from its session entry if — and only if — it is still
    /// parked there (`Arc::ptr_eq`, under the lock). Returns `true` when it
    /// was removed (a genuine timeout); `false` means a peer claimed it in
    /// the meantime and the session is live — the caller must not tear down.
    fn unpark(&self, req: ConnectRequest, my: &Arc<Slot>) -> bool {
        let mut sessions = lock(&self.sessions);
        if !Self::side_holds(&sessions, req, my) {
            return false;
        }
        if let Some(entry) = sessions.get_mut(&req.session_id) {
            let side = if req.initiator {
                &mut entry.0
            } else {
                &mut entry.1
            };
            *side = None;
            if entry.0.is_none() && entry.1.is_none() {
                sessions.remove(&req.session_id);
            }
        }
        true
    }

    fn side_holds(sessions: &Sessions, req: ConnectRequest, my: &Arc<Slot>) -> bool {
        sessions.get(&req.session_id).is_some_and(|(i, r)| {
            let side = if req.initiator { i } else { r };
            side.as_ref().is_some_and(|s| Arc::ptr_eq(s, my))
        })
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // A poisoned lock can only mean a pipe thread panicked while holding it;
    // unwrapping the poison and continuing keeps the relay alive.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The relay server. Bind with [`Relay::bind`], run with [`Relay::serve`].
pub struct Relay {
    listener: TcpListener,
    shared: Arc<Shared>,
}

impl Relay {
    /// Bind the listen socket. The resolved (possibly ephemeral) address is
    /// available via [`local_addr`](Self::local_addr) — tests bind on
    /// `127.0.0.1:0` and read back the port.
    pub fn bind(addr: SocketAddr) -> Result<Self, RelayError> {
        let listener = TcpListener::bind(addr).map_err(RelayError::Bind)?;
        let _ = listener.set_nonblocking(false);
        Ok(Self {
            listener,
            shared: Arc::new(Shared {
                sessions: Mutex::new(HashMap::new()),
                timeout: Duration::from_secs(30),
                limits: RelayLimits::DEFAULT,
                conns: Mutex::new(ConnTable::default()),
                ws_paths: vec![DEFAULT_WS_PATH.to_owned()],
            }),
        })
    }

    /// The bound (resolved) listen address.
    pub fn local_addr(&self) -> SocketAddr {
        self.listener
            .local_addr()
            .unwrap_or_else(|_| "0.0.0.0:0".parse().expect("static fallback addr parses"))
    }

    /// Set the unmatched-CONNECT timeout (default 30 s). Call before
    /// [`serve`](Self::serve); the value is read once by each client thread.
    pub fn set_timeout(&mut self, timeout: Duration) {
        Arc::get_mut(&mut self.shared)
            .expect("set_timeout before serve: no client threads hold Shared yet")
            .timeout = timeout;
    }

    /// Set the connection / pending-session caps (default
    /// [`RelayLimits::DEFAULT`]). Call before [`serve`](Self::serve).
    pub fn set_limits(&mut self, limits: RelayLimits) {
        Arc::get_mut(&mut self.shared)
            .expect("set_limits before serve: no client threads hold Shared yet")
            .limits = limits;
    }

    /// Set the request paths accepted for a WebSocket upgrade (default
    /// [`DEFAULT_WS_PATH`], `/relay`). Each must start with `/`; an empty
    /// list keeps the default. Call before [`serve`](Self::serve).
    pub fn set_ws_paths(&mut self, paths: Vec<String>) -> Result<(), RelayError> {
        if let Some(bad) = paths
            .iter()
            .find(|p| !p.starts_with('/') || p.contains('?'))
        {
            return Err(RelayError::Protocol(format!(
                "WebSocket path `{bad}` must start with `/` and have no query"
            )));
        }
        if !paths.is_empty() {
            Arc::get_mut(&mut self.shared)
                .expect("set_ws_paths before serve: no client threads hold Shared yet")
                .ws_paths = paths;
        }
        Ok(())
    }

    /// Accept loop: one thread per client. Runs forever: a failed `accept`
    /// (e.g. out of descriptors, a connection reset before accept) is logged
    /// and the loop continues after a short pause. `max_sessions`, when
    /// given, caps concurrent *pending* sessions (else
    /// [`RelayLimits::max_pending`]); excess CONNECTs get `XT` (rejected).
    /// Connections beyond the total / per-IP caps are closed at accept.
    pub fn serve(&self, max_sessions: Option<usize>) -> Result<(), RelayError> {
        let max = max_sessions.unwrap_or(self.shared.limits.max_pending);
        for stream in self.listener.incoming() {
            let stream = match stream {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("accept failed: {e}");
                    thread::sleep(Duration::from_millis(50));
                    continue;
                }
            };
            let Some(guard) = stream
                .peer_addr()
                .ok()
                .and_then(|a| self.shared.admit(a.ip()))
            else {
                tracing::debug!(
                    peer = ?stream.peer_addr().ok(),
                    "connection refused: connection cap reached"
                );
                let _ = stream.shutdown(Shutdown::Both);
                continue;
            };
            let _ = stream.set_nodelay(true);
            let shared = self.shared.clone();
            // Thread-per-client: simple, and session counts are small. A
            // failed spawn (thread limit) drops the connection, not the relay.
            let spawned = thread::Builder::new()
                .name("ovrelay-client".into())
                .spawn(move || {
                    let _guard = guard;
                    if let Err(e) = handle_client(stream, shared, max) {
                        // Client-side failures are logged, not fatal to the relay.
                        tracing::debug!("client error: {e}");
                    }
                });
            if let Err(e) = spawned {
                tracing::warn!("cannot spawn client thread: {e}");
            }
        }
        Ok(())
    }
}

/// Per-client state machine: CONNECT -> park or pair -> pipe.
fn handle_client(
    mut stream: TcpStream,
    shared: Arc<Shared>,
    max_pending: usize,
) -> Result<(), RelayError> {
    // Bound the pre-session phase: a silent client times out instead of
    // holding this thread forever (per read), and the whole phase has an
    // overall deadline. Cleared once the CONNECT line is in.
    let deadline = Instant::now() + shared.limits.handshake_deadline;
    stream
        .set_read_timeout(Some(HANDSHAKE_TIMEOUT))
        .map_err(RelayError::Io)?;
    // A raw client opens with CONNECT (`C`); an HTTP client with `GET`.
    let mut first = [0u8; 1];
    let ws = matches!(stream.peek(&mut first), Ok(1)) && first[0] == b'G';
    if ws {
        match ws::handshake(&mut stream, deadline, &shared.ws_paths).map_err(RelayError::Io)? {
            ws::Upgrade::Accepted => {}
            ws::Upgrade::Answered => return Ok(()), // plain HTTP (health check)
        }
    }
    let socket_addr = stream.peer_addr().map_err(RelayError::Io)?;
    let write_lock = Arc::new(Mutex::new(Vec::new()));
    let reader: Box<dyn Read + Send> = if ws {
        Box::new(ws::WsReader::new(
            stream.try_clone().map_err(RelayError::Io)?,
            stream.try_clone().map_err(RelayError::Io)?,
            write_lock.clone(),
        ))
    } else {
        Box::new(stream.try_clone().map_err(RelayError::Io)?)
    };
    let my_addr = if ws { None } else { Some(socket_addr) };
    let mine = Arc::new(Slot {
        write_lock,
        stream: Mutex::new(Some(stream)),
        ws,
        reader: Mutex::new(Some(reader)),
        addr: my_addr,
        role: Mutex::new("?"),
    });

    let req = {
        let mut reader = lock(&mine.reader)
            .take()
            .expect("reader set at construction");
        let req = read_connect(&mut DeadlineReader {
            inner: &mut *reader,
            deadline,
        });
        *lock(&mine.reader) = Some(reader);
        match req {
            Ok(r) => r,
            Err(e) => {
                mine.close();
                return Err(e);
            }
        }
    };
    *lock(&mine.role) = if req.initiator {
        "initiator/viewer"
    } else {
        "responder/host"
    };
    // From here reads block until the peer talks or a teardown's shutdown
    // wakes them (the socket option is shared by every clone).
    if let Some(s) = lock(&mine.stream).as_ref() {
        let _ = s.set_read_timeout(None);
    }
    // Capacity guard first: reject (`XT`, no `O`) when too many sessions
    // are parked.
    if shared.pending() >= max_pending {
        tracing::debug!(
            "session {:08x}: refused, {max_pending} sessions already parked",
            req.session_id
        );
        mine.try_write(&reject_line(req.session_id));
        mine.close();
        return Ok(());
    }
    // Acknowledge the CONNECT (`O`); the peer-attach confirmation
    // (`A || sid`) follows once both sides are parked.
    mine.try_write(b"O");

    // Register our slot and see whether we complete a pair.
    let peer = match shared.register(req, mine.clone()) {
        Ok(Some(peer)) => {
            // We are the *second* side: the first side's slot was parked in
            // the table (and is now removed from it) — pipe against it.
            peer
        }
        Ok(None) => {
            // We are the first side: our slot is parked in the table. The
            // *second* side's thread owns the session from the moment it
            // pairs — it writes the `A || sid` lines, runs the pipe and tears
            // it down. We only time the park out.
            if !wait_until_claimed(&shared, req, &mine) {
                // Timed out with no peer (and removed from the table under
                // the lock, so no late pairing can race us): XT + tear down.
                mine.try_write(&reject_line(req.session_id));
                mine.close();
            }
            return Ok(());
        }
        Err(e) => {
            mine.close();
            return Err(e);
        }
    };

    // We are the second side: we completed the pair, so we own the session
    // handshake-back (`A || sid || peer-external-addr` to both ends) and the
    // pipe. Each end learns the *other* end's relay-observed address.
    mine.try_write(&attach_line(req.session_id, peer.addr));
    peer.try_write(&attach_line(req.session_id, my_addr));
    pipe_pair(&mine, &peer, req.session_id);
    Ok(())
}

/// Wait while the parked slot is unclaimed. Returns `true` as soon as a
/// peer claims it (the session is live and owned by the peer's thread — an
/// active session is never torn down from here, however long it runs), or
/// `false` after the park timeout, in which case the slot has already been
/// removed from the session table.
fn wait_until_claimed(shared: &Shared, req: ConnectRequest, mine: &Arc<Slot>) -> bool {
    let deadline = Instant::now() + shared.timeout;
    loop {
        if !shared.is_parked(req, mine) {
            return true;
        }
        if Instant::now() >= deadline {
            // Atomic check-and-remove: a peer pairing in the last instant
            // wins (unpark returns false) and the session stays up.
            return !shared.unpark(req, mine);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// `A || session_id(4B BE) || peer_ipv4(4B) || peer_port(2B BE)` —
/// "peer attached, tunnel live, here is the peer's relay-observed address".
/// For an IPv6 peer connection the IP is 4 zero bytes ("no v4 address — stay
/// on relay") and the port is 0. Purely informational: the relay never routes
/// on this address, it only reports what `getpeername` told it.
fn attach_line(session_id: u32, peer: Option<SocketAddr>) -> Vec<u8> {
    let mut line = Vec::with_capacity(11);
    line.push(b'A');
    line.extend_from_slice(&session_id.to_be_bytes());
    match peer {
        Some(SocketAddr::V4(v4)) => {
            line.extend_from_slice(&v4.ip().octets());
            line.extend_from_slice(&v4.port().to_be_bytes());
        }
        // IPv6, or a WebSocket client whose socket address is the proxy's.
        Some(SocketAddr::V6(_)) | None => {
            line.extend_from_slice(&[0u8; 4]);
            line.extend_from_slice(&0u16.to_be_bytes());
        }
    }
    line
}

/// `XT || session_id(4B BE)` — "rejected / timed out".
fn reject_line(session_id: u32) -> [u8; 6] {
    let b = session_id.to_be_bytes();
    [b'X', b'T', b[0], b[1], b[2], b[3]]
}

/// Read the 6-byte CONNECT line: `C || sid(4B BE) || role`.
fn read_connect<R: Read + ?Sized>(stream: &mut R) -> Result<ConnectRequest, RelayError> {
    let mut buf = [0u8; 6];
    stream.read_exact(&mut buf).map_err(RelayError::Io)?;
    if buf[0] != b'C' {
        return Err(RelayError::Protocol(format!(
            "expected CONNECT, got 0x{:02x}",
            buf[0]
        )));
    }
    Ok(ConnectRequest {
        session_id: u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]),
        initiator: buf[5] == 0x00,
    })
}

/// Pipe two attached slots together until either side finishes.
///
/// Two threads, one per direction (see [`spawn_pipe`]). A DISCONNECT (or
/// EOF / hard error) on one side writes a DISCONNECT frame to the other and
/// closes both; closing shuts the sockets down, which wakes the opposite
/// direction's blocked read so both threads exit.
fn pipe_pair(a: &Arc<Slot>, b: &Arc<Slot>, session_id: u32) {
    let started = Instant::now();
    tracing::info!(
        "session {session_id:08x} paired: {} <-> {}",
        a.label(),
        b.label()
    );
    // WebSocket keepalive: PING frames toward WebSocket clients while the
    // session lives (the tunnel payload may idle for long stretches once a
    // session moved to its direct path).
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let pinger = (a.ws || b.ws).then(|| {
        let (a, b, done) = (a.clone(), b.clone(), done.clone());
        thread::Builder::new()
            .name(format!("ovrelay-wsping-{session_id:08x}"))
            .spawn(move || {
                let mut next = Instant::now() + WS_PING_EVERY;
                while !done.load(std::sync::atomic::Ordering::Relaxed) {
                    if Instant::now() >= next {
                        a.try_ping();
                        b.try_ping();
                        next += WS_PING_EVERY;
                    }
                    thread::sleep(Duration::from_millis(200));
                }
            })
            .ok()
    });
    let (ra, rb) = pipe_pair_inner(a, b, session_id);
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    if let Some(Some(p)) = pinger {
        let _ = p.join();
    }
    // Who ended it: the direction whose read finished first (the other one
    // is woken by the teardown).
    let fmt = |src: &Arc<Slot>, dst: &Arc<Slot>, r: &Option<PipeReport>| match r {
        Some(r) => format!(
            "{} -> {}: {} B in {} frame read(s), ended by {} after {:.1} s, idle {:.1} s before",
            src.label(),
            dst.label(),
            r.bytes,
            r.reads,
            r.reason,
            r.ended.saturating_duration_since(started).as_secs_f64(),
            r.ended.saturating_duration_since(r.last_rx).as_secs_f64()
        ),
        None => format!("{} -> {}: no pipe", src.label(), dst.label()),
    };
    let first = match (&ra, &rb) {
        (Some(x), Some(y)) if y.ended < x.ended => b.label(),
        (Some(_), _) => a.label(),
        (None, _) => b.label(),
    };
    tracing::info!(
        "session {session_id:08x} closed after {:.1} s; first to end: {first}",
        started.elapsed().as_secs_f64(),
    );
    tracing::debug!(
        "session {session_id:08x}: [{}]; [{}]",
        fmt(a, b, &ra),
        fmt(b, a, &rb)
    );
}

/// Interval of the relay's WebSocket PING frames.
const WS_PING_EVERY: Duration = Duration::from_secs(15);

/// How one pipe direction went (session-end log).
struct PipeReport {
    bytes: u64,
    reads: u64,
    reason: &'static str,
    ended: Instant,
    last_rx: Instant,
}

fn pipe_pair_inner(
    a: &Arc<Slot>,
    b: &Arc<Slot>,
    session_id: u32,
) -> (Option<PipeReport>, Option<PipeReport>) {
    let ha = match spawn_pipe(a, b, session_id) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!("session {session_id:08x}: cannot spawn pipe: {e}");
            a.close();
            b.close();
            return (None, None);
        }
    };
    let hb = spawn_pipe(b, a, session_id);
    if let Err(e) = &hb {
        tracing::warn!("session {session_id:08x}: cannot spawn pipe: {e}");
        // Wake and end the direction that did start.
        a.close();
        b.close();
    }
    let ra = ha.join().ok().flatten();
    let rb = match hb {
        Ok(hb) => hb.join().ok().flatten(),
        Err(_) => None,
    };
    a.close();
    b.close();
    (ra, rb)
}

/// How one pipe direction ended.
enum PipeEnd {
    /// Clean EOF or a DISCONNECT frame: tell the peer, then tear down.
    Disconnect,
    /// Framing violation or read error: tear down without a DISCONNECT.
    Error,
}

/// Incremental frame-boundary tracker for the opaque pipe. The bytes are
/// forwarded as read; only the 3-byte headers are inspected (kind/length
/// validated in place) so DISCONNECT and framing errors are caught without
/// decoding or re-encoding any frame.
#[derive(Default)]
struct FrameCursor {
    /// Body bytes of the current frame still to pass through.
    body_left: usize,
}

/// What [`FrameCursor::scan`] found in a buffer.
enum Scan {
    /// All of `buf[..forward]` is safe to forward; `buf[forward..]` is an
    /// incomplete header (< 3 bytes) to carry into the next read.
    Forward { forward: usize },
    /// Forward `buf[..forward]`, then the stream ends (DISCONNECT seen or
    /// a framing violation).
    Stop {
        forward: usize,
        end: PipeEnd,
        why: Option<String>,
    },
}

impl FrameCursor {
    fn scan(&mut self, buf: &[u8]) -> Scan {
        let mut pos = 0usize;
        while pos < buf.len() {
            if self.body_left > 0 {
                let take = self.body_left.min(buf.len() - pos);
                pos += take;
                self.body_left -= take;
                continue;
            }
            if buf.len() - pos < 3 {
                return Scan::Forward { forward: pos };
            }
            let kind = buf[pos];
            let len = usize::from(u16::from_be_bytes([buf[pos + 1], buf[pos + 2]]));
            if kind > PUNCH_ACK {
                return Scan::Stop {
                    forward: pos,
                    end: PipeEnd::Error,
                    why: Some(format!("unknown frame kind 0x{kind:02x}")),
                };
            }
            if len > MAX_FRAME {
                return Scan::Stop {
                    forward: pos,
                    end: PipeEnd::Error,
                    why: Some(format!("frame payload too large ({len} > {MAX_FRAME})")),
                };
            }
            if kind == DISCONNECT {
                return Scan::Stop {
                    forward: pos,
                    end: PipeEnd::Disconnect,
                    why: None,
                };
            }
            self.body_left = len;
            pos += 3;
        }
        Scan::Forward { forward: pos }
    }
}

/// One direction of the pipe. Returns when the direction finishes.
///
/// Each `read` (up to [`PIPE_BUF`]) is forwarded with a single write — one
/// WebSocket binary message for a WebSocket destination — instead of one
/// write per tunnel frame. Partial frames are forwarded as they arrive (the
/// far-end carrier reassembles); only an incomplete *header* is held back
/// (at most 2 bytes) so a DISCONNECT split across reads is never half-sent.
fn spawn_pipe(
    src: &Arc<Slot>,
    dst: &Arc<Slot>,
    session_id: u32,
) -> io::Result<thread::JoinHandle<Option<PipeReport>>> {
    let src = src.clone();
    let dst = dst.clone();
    thread::Builder::new()
        .name(format!("ovrelay-pipe-{session_id:08x}"))
        .spawn(move || {
            // Read from the slot's reader (a socket clone, or the WebSocket
            // deframer): the original stream stays in the slot so
            // try_write/close (used by this and the peer's pipe thread for
            // teardown frames) keep working while this thread blocks on read.
            let Some(mut stream) = lock(&src.reader).take() else {
                return None; // reader gone: slot already torn down
            };
            let (mut bytes, mut reads) = (0u64, 0u64);
            let mut last_rx = Instant::now();
            let mut reason: &'static str = "EOF";
            let mut cursor = FrameCursor::default();
            let mut buf = vec![0u8; PIPE_BUF];
            // Unforwarded incomplete-header bytes kept at buf[..held].
            let mut held = 0usize;
            let end = loop {
                let n = match stream.read(&mut buf[held..]) {
                    Ok(0) => break PipeEnd::Disconnect, // clean EOF (or WS CLOSE)
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        tracing::debug!("session {session_id:08x} read error: {e}");
                        reason = "read error";
                        break PipeEnd::Error;
                    }
                };
                bytes += n as u64;
                reads += 1;
                last_rx = Instant::now();
                let total = held + n;
                match cursor.scan(&buf[..total]) {
                    Scan::Forward { forward } => {
                        if forward > 0 {
                            dst.try_write(&buf[..forward]);
                        }
                        buf.copy_within(forward..total, 0);
                        held = total - forward;
                    }
                    Scan::Stop { forward, end, why } => {
                        if forward > 0 {
                            dst.try_write(&buf[..forward]);
                        }
                        reason = if why.is_some() {
                            "framing error"
                        } else {
                            "DISCONNECT frame"
                        };
                        if let Some(why) = why {
                            tracing::debug!("session {session_id:08x} framing error: {why}");
                        }
                        break end;
                    }
                }
            };
            if let PipeEnd::Disconnect = end {
                // Peer asked for teardown (or went away cleanly): relay it.
                dst.try_write(&encode_frame(DISCONNECT, &[]).expect("fixed kind fits"));
            }
            let ended = Instant::now();
            // Our own side first: closing the peer wakes its pipe thread with
            // EOF, which would otherwise race a DISCONNECT back onto this
            // (still open) side.
            src.close();
            dst.close();
            Some(PipeReport {
                bytes,
                reads,
                reason,
                ended,
                last_rx,
            })
        })
}

/// Parser entry points for fuzzing (`fuzz/`, feature `fuzzing`). Each takes
/// arbitrary bytes and must never panic.
#[cfg(any(test, feature = "fuzzing"))]
#[doc(hidden)]
pub mod fuzzing {
    /// Feed `data` (client -> relay bytes after the upgrade) through the
    /// WebSocket deframer until EOF or a protocol error.
    pub fn ws_frames(data: &[u8]) {
        crate::ws::fuzz_deframe(data);
    }

    /// Feed `data` through the tunnel-frame boundary tracker, split at
    /// every position the first byte selects.
    pub fn frame_cursor(data: &[u8]) {
        let split = data.first().map_or(0, |&b| usize::from(b)).min(data.len());
        let mut c = super::FrameCursor::default();
        for part in [&data[..split], &data[split..]] {
            if let super::Scan::Stop { .. } = c.scan(part) {
                return;
            }
        }
    }

    /// Parse `data` as a CONNECT line.
    pub fn connect_line(data: &[u8]) {
        let _ = super::read_connect(&mut &data[..]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn connection_caps_total_and_per_ip_with_loopback_exempt() {
        let mut relay = Relay::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        relay.set_limits(RelayLimits {
            max_connections: 3,
            max_per_ip: 1,
            ..RelayLimits::DEFAULT
        });
        let s = &relay.shared;
        let a: IpAddr = [203, 0, 113, 1].into();
        let b: IpAddr = [203, 0, 113, 2].into();
        let lo: IpAddr = [127, 0, 0, 1].into();
        let g1 = s.admit(a).expect("first from a");
        assert!(s.admit(a).is_none(), "per-IP cap");
        let g2 = s.admit(b).expect("other IP");
        let g3 = s.admit(lo).expect("loopback");
        assert!(s.admit(lo).is_none(), "total cap applies to loopback too");
        drop(g3);
        let _g4 = s.admit(lo).expect("loopback exempt from per-IP cap");
        drop(g1);
        let _g5 = s.admit(a).expect("slot released on drop");
        drop(g2);
        assert_eq!(lock(&s.conns).per_ip.get(&b), None, "entry cleaned up");
    }

    #[test]
    fn deadline_reader_stops_dribbling_clients() {
        let data = [1u8; 8];
        let mut src = &data[..];
        let mut r = DeadlineReader {
            inner: &mut src,
            deadline: Instant::now() - Duration::from_millis(1),
        };
        let err = r.read(&mut [0u8; 4]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn parser_entry_points_survive_noise() {
        let mut s = 0x1234_5678_9ABC_DEF1u64;
        for _ in 0..3000 {
            let n = (s % 300) as usize;
            let buf: Vec<u8> = (0..n)
                .map(|_| {
                    s ^= s << 13;
                    s ^= s >> 7;
                    s ^= s << 17;
                    s as u8
                })
                .collect();
            fuzzing::ws_frames(&buf);
            fuzzing::frame_cursor(&buf);
            fuzzing::connect_line(&buf);
        }
    }

    #[test]
    fn connect_line_roundtrip() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let h = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            read_connect(&mut s).unwrap()
        });
        let mut c = TcpStream::connect(addr).unwrap();
        c.write_all(b"C").unwrap();
        c.write_all(&42u32.to_be_bytes()).unwrap();
        c.write_all(&[0x01]).unwrap();
        let req = h.join().unwrap();
        assert_eq!(req.session_id, 42);
        assert!(!req.initiator);
    }

    #[test]
    fn reject_line_layout() {
        let v4: SocketAddr = "203.0.113.7:5555".parse().unwrap();
        let v6: SocketAddr = "[2001:db8::1]:5555".parse().unwrap();
        assert_eq!(
            attach_line(0x0102_0304, Some(v4)),
            [b'A', 1, 2, 3, 4, 203, 0, 113, 7, 0x15, 0xB3]
        );
        // IPv6 peer: zeroed IP + zero port ("stay on relay").
        assert_eq!(
            attach_line(0x0102_0304, Some(v6)),
            [b'A', 1, 2, 3, 4, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(reject_line(0x0102_0304), *b"XT\x01\x02\x03\x04");
    }

    fn frames(parts: &[(u8, &[u8])]) -> Vec<u8> {
        parts
            .iter()
            .flat_map(|(k, p)| encode_frame(*k, p).unwrap())
            .collect()
    }

    #[test]
    fn frame_cursor_forwards_whole_and_split_frames() {
        let wire = frames(&[(0, b"hello"), (1, &7u64.to_be_bytes()), (0, &[9; 300])]);
        // Every split point: forwarded prefix + held header bytes always
        // reassemble the stream exactly, and only a header is ever held.
        for cut in 0..=wire.len() {
            let mut c = FrameCursor::default();
            let mut out = Vec::new();
            let Scan::Forward { forward } = c.scan(&wire[..cut]) else {
                panic!("stop")
            };
            assert!(cut - forward < 3, "only an incomplete header is held");
            out.extend_from_slice(&wire[..forward]);
            let mut rest = wire[forward..cut].to_vec();
            rest.extend_from_slice(&wire[cut..]);
            let Scan::Forward { forward: f2 } = c.scan(&rest) else {
                panic!("stop")
            };
            assert_eq!(f2, rest.len());
            out.extend_from_slice(&rest);
            assert_eq!(out, wire, "cut at {cut}");
        }
    }

    #[test]
    fn frame_cursor_stops_at_disconnect_and_bad_frames() {
        let mut wire = frames(&[(0, b"abc")]);
        let before = wire.len();
        wire.extend(frames(&[(DISCONNECT, b""), (0, b"after")]));
        match FrameCursor::default().scan(&wire) {
            Scan::Stop {
                forward,
                end: PipeEnd::Disconnect,
                ..
            } => assert_eq!(forward, before),
            _ => panic!("DISCONNECT must stop the pipe"),
        }
        let mut bad = frames(&[(0, b"ok")]);
        let ok_len = bad.len();
        bad.extend_from_slice(&[0x7F, 0, 0]);
        match FrameCursor::default().scan(&bad) {
            Scan::Stop {
                forward,
                end: PipeEnd::Error,
                ..
            } => assert_eq!(forward, ok_len),
            _ => panic!("unknown kind must stop the pipe"),
        }
        let huge = [0u8, 0xFF, 0xFF];
        assert!(matches!(
            FrameCursor::default().scan(&huge),
            Scan::Stop {
                forward: 0,
                end: PipeEnd::Error,
                ..
            }
        ));
    }
}
