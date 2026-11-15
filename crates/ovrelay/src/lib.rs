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
//! pending client gets `b"XT" || sid` and is torn down. A parked client's
//! socket is read while it waits (it leaves the table the moment it goes
//! away; WebSocket PINGs are answered), a newer initiator replaces a parked
//! one, and a viewer dialing a host that is busy in another session parks
//! until the host parks again. std only — one OS
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
use std::sync::atomic::{AtomicBool, Ordering};
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

/// One relay client: its TCP stream, guarded so other threads can write
/// control lines and teardown frames to it while this client's own thread
/// blocks reading it. `addr` is the relay's own view of this client's socket
/// (`getpeername` at accept time) — learned passively, never routed on.
struct Slot {
    /// Serializes `try_write` (a torn interleaved write from two threads
    /// would corrupt the frame stream - observed as `unknown control byte`
    /// on the receiving carrier under concurrent DATA + auto-PONG traffic).
    /// Shared with a WebSocket reader, which writes PONGs. The guarded `Vec`
    /// is a reusable scratch buffer for WebSocket framing (no per-write
    /// allocation).
    write_lock: Arc<Mutex<Vec<u8>>>,
    stream: Mutex<Option<TcpStream>>,
    /// The client speaks WebSocket: writes are framed as binary messages.
    ws: bool,
    /// The client's address as the relay sees it. `None` for a WebSocket
    /// client: it arrives through a proxy, so the socket address is the
    /// proxy's and useless as a punch target.
    addr: Option<SocketAddr>,
    /// Which side this is (initiator = viewer, responder = host), for
    /// the session-end log. Set at CONNECT.
    role: Mutex<&'static str>,
    /// Where this client's inbound bytes go (see [`Route`]).
    route: Mutex<Route>,
}

/// Where a client's inbound bytes go. Transitions happen under the slot's
/// `route` lock, so a claiming peer and the client's own departure (or the
/// park timeout) can never both win.
enum Route {
    /// Not paired yet: the client's own thread already reads its socket (so
    /// a client that leaves is noticed at once and WebSocket PINGs are
    /// answered while it waits); bytes it sends meanwhile are held here
    /// (at most [`EARLY_CAP`]) and forwarded first when a peer claims it.
    Parked {
        /// Bytes read before a peer claimed the slot.
        early: Vec<u8>,
        /// When the park started (the park timeout runs from here).
        since: Instant,
    },
    /// Piped to `dst`; `link` is shared by both directions.
    Paired { dst: Arc<Slot>, link: Arc<Link> },
    /// Gone: left while parked, timed out, displaced, or torn down.
    Closed,
}

/// Most bytes a parked client may send before a peer claims it.
const EARLY_CAP: usize = 64 * 1024;

/// How often the park reaper looks for timed-out parks.
const REAP_EVERY: Duration = Duration::from_millis(20);

impl Slot {
    /// Best-effort write; a dead peer just loses the bytes (its own reader
    /// notices the dead socket and tears the session down).
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

    /// Start (or restart) a park: nothing read yet, the timeout runs from
    /// now. Only called by the client's own thread while the slot is not in
    /// the session table.
    fn begin_park(&self) {
        *lock(&self.route) = Route::Parked {
            early: Vec::new(),
            since: Instant::now(),
        };
    }

    /// When the current park started, `None` when not parked.
    fn parked_since(&self) -> Option<Instant> {
        match &*lock(&self.route) {
            Route::Parked { since, .. } => Some(*since),
            _ => None,
        }
    }

    /// Mark the slot closed; `true` when it was parked until now (a claim
    /// cannot race this: the caller took the slot out of the session table
    /// first).
    fn close_if_parked(&self) -> bool {
        let mut route = lock(&self.route);
        let parked = matches!(*route, Route::Parked { .. });
        *route = Route::Closed;
        parked
    }

    /// Close this side: WebSocket CLOSE (if applicable), then
    /// `shutdown(Both)` so every clone of the socket — including a reader
    /// blocked in `read` — wakes with EOF instead of leaking, then drop the
    /// stream.
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

/// One paired session, shared by its two directions: the session-end log
/// (written by whichever direction finishes last) and the WebSocket
/// keepalive's stop flag.
struct Link {
    session_id: u32,
    started: Instant,
    /// Both directions finished (stops the keepalive thread).
    done: AtomicBool,
    /// `(src label, dst label, report)` per finished direction.
    reports: Mutex<Vec<(String, String, PipeReport)>>,
}

impl Link {
    fn new(session_id: u32) -> Self {
        Self {
            session_id,
            started: Instant::now(),
            done: AtomicBool::new(false),
            reports: Mutex::new(Vec::with_capacity(2)),
        }
    }

    /// One direction finished; the second one logs the session's end.
    fn finish(&self, src: String, dst: String, report: PipeReport) {
        let mut reports = lock(&self.reports);
        reports.push((src, dst, report));
        if reports.len() < 2 {
            return;
        }
        self.done.store(true, Ordering::Relaxed);
        let sid = self.session_id;
        // Who ended it: the direction whose read finished first (the other
        // one is woken by the teardown).
        let first = reports
            .iter()
            .min_by_key(|(_, _, r)| r.ended)
            .map(|(src, _, _)| src.clone())
            .unwrap_or_default();
        tracing::info!(
            "session {sid:08x} closed after {:.1} s; first to end: {first}",
            self.started.elapsed().as_secs_f64(),
        );
        let fmt = |(src, dst, r): &(String, String, PipeReport)| {
            format!(
                "{src} -> {dst}: {} B in {} frame read(s), ended by {} after {:.1} s, idle {:.1} s before",
                r.bytes,
                r.reads,
                r.reason,
                r.ended.saturating_duration_since(self.started).as_secs_f64(),
                r.ended.saturating_duration_since(r.last_rx).as_secs_f64()
            )
        };
        tracing::debug!(
            "session {sid:08x}: [{}]; [{}]",
            fmt(&reports[0]),
            fmt(&reports[1])
        );
    }
}

/// Shared state between the accept loop and the client threads.
/// Per-session entry: each side is `None` until its client parks.
type Sessions = HashMap<u32, (Option<Arc<Slot>>, Option<Arc<Slot>>)>;

struct Shared {
    /// session_id -> (initiator slot, responder slot) for *pending* (parked,
    /// unpaired) sessions only: pairing removes the entry, so an active
    /// session leaves the id free for the next park — a viewer dialing a
    /// host that is busy with another session parks here until the host
    /// parks again.
    sessions: Mutex<Sessions>,
    /// Idle timeout for an unmatched CONNECT.
    timeout: Duration,
    /// Connection / pending-session caps.
    limits: RelayLimits,
    /// Open connections (total + per IP).
    conns: Mutex<ConnTable>,
    /// Request paths accepted for a WebSocket upgrade.
    ws_paths: Vec<String>,
}

/// Outcome of [`Shared::register`].
enum Registered {
    /// The other side was parked: it is removed from the table and the
    /// caller pairs with it (see [`claim`]).
    Paired(Arc<Slot>),
    /// The caller is parked. `displaced` is an older parked initiator of
    /// the same session that this one replaced (newest viewer wins: the
    /// older one is most likely an attempt its app already abandoned).
    Parked { displaced: Option<Arc<Slot>> },
}

impl Shared {
    /// Register a CONNECT with the caller's own slot (already in
    /// [`Route::Parked`]). A second responder for a parked session is a role
    /// conflict (`Err(Protocol)`); a second initiator replaces the parked
    /// one.
    fn register(&self, req: ConnectRequest, my: Arc<Slot>) -> Result<Registered, RelayError> {
        let mut sessions = lock(&self.sessions);
        let entry = sessions.entry(req.session_id).or_insert((None, None));
        let (mine, theirs) = if req.initiator {
            (&mut entry.0, &mut entry.1)
        } else {
            (&mut entry.1, &mut entry.0)
        };
        if mine.is_some() && !req.initiator {
            return Err(RelayError::Protocol(format!(
                "session {} already has a responder",
                req.session_id
            )));
        }
        if let Some(peer) = theirs.take() {
            // Pair complete: our side was empty, theirs is now taken.
            sessions.remove(&req.session_id);
            return Ok(Registered::Paired(peer));
        }
        let displaced = mine.replace(my);
        Ok(Registered::Parked { displaced })
    }

    /// Remove `my` from its session entry if — and only if — it is still
    /// parked there (`Arc::ptr_eq`, under the lock). Returns `true` when it
    /// was removed; `false` means it is no longer in the table (a peer
    /// claimed it, or it was displaced / reaped).
    fn unpark(&self, req: ConnectRequest, my: &Arc<Slot>) -> bool {
        let mut sessions = lock(&self.sessions);
        let Some(entry) = sessions.get_mut(&req.session_id) else {
            return false;
        };
        let side = if req.initiator {
            &mut entry.0
        } else {
            &mut entry.1
        };
        if !side.as_ref().is_some_and(|s| Arc::ptr_eq(s, my)) {
            return false;
        }
        *side = None;
        if entry.0.is_none() && entry.1.is_none() {
            sessions.remove(&req.session_id);
        }
        true
    }

    /// Time out every park older than `timeout`: `XT || sid`, then close.
    /// A park removed here can no longer be claimed (a claim takes it out
    /// of the table first), so a session that paired in the last instant is
    /// never touched.
    fn reap_expired(&self) {
        let now = Instant::now();
        let expired: Vec<(ConnectRequest, Arc<Slot>)> = {
            let sessions = lock(&self.sessions);
            let mut out = Vec::new();
            for (&session_id, (i, r)) in sessions.iter() {
                for (initiator, slot) in [(true, i), (false, r)] {
                    let Some(slot) = slot else { continue };
                    let due = slot
                        .parked_since()
                        .is_some_and(|t| now.saturating_duration_since(t) >= self.timeout);
                    if due {
                        let req = ConnectRequest {
                            session_id,
                            initiator,
                        };
                        out.push((req, slot.clone()));
                    }
                }
            }
            out
        };
        for (req, slot) in expired {
            if self.unpark(req, &slot) {
                if slot.close_if_parked() {
                    slot.try_write(&reject_line(req.session_id));
                }
                slot.close();
            }
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // A poisoned lock can only mean a client thread panicked while holding
    // it; unwrapping the poison and continuing keeps the relay alive.
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
    /// [`serve`](Self::serve).
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

    /// Accept loop: one thread per client, plus one park reaper (times
    /// unclaimed parks out). Runs forever: a failed `accept` (e.g. out of
    /// descriptors, a connection reset before accept) is logged and the
    /// loop continues after a short pause. `max_sessions`, when given, caps
    /// concurrent *pending* sessions (else [`RelayLimits::max_pending`]);
    /// excess CONNECTs get `XT` (rejected). Connections beyond the total /
    /// per-IP caps are closed at accept.
    pub fn serve(&self, max_sessions: Option<usize>) -> Result<(), RelayError> {
        let max = max_sessions.unwrap_or(self.shared.limits.max_pending);
        let reaper = Arc::downgrade(&self.shared);
        thread::Builder::new()
            .name("ovrelay-reaper".into())
            .spawn(move || {
                while let Some(shared) = reaper.upgrade() {
                    shared.reap_expired();
                    drop(shared);
                    thread::sleep(REAP_EVERY);
                }
            })
            .map_err(RelayError::Io)?;
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

/// Per-client state machine: CONNECT -> park or pair -> pump this client's
/// direction (its own thread reads its socket from the CONNECT on).
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
    let mut reader: Box<dyn Read + Send> = if ws {
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
        addr: my_addr,
        role: Mutex::new("?"),
        route: Mutex::new(Route::Closed),
    });

    let req = match read_connect(&mut DeadlineReader {
        inner: &mut *reader,
        deadline,
    }) {
        Ok(r) => r,
        Err(e) => {
            mine.close();
            return Err(e);
        }
    };
    *lock(&mine.role) = if req.initiator {
        "initiator/viewer"
    } else {
        "responder/host"
    };
    // From here reads block until the client talks or a teardown's
    // shutdown wakes them (the socket option is shared by every clone).
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
    // (`A || sid`) follows once both sides are present.
    mine.try_write(b"O");

    // Park, or pair with the parked other side. A parked side that left a
    // moment ago (its claim fails) is skipped: register again.
    loop {
        mine.begin_park();
        match shared.register(req, mine.clone()) {
            Ok(Registered::Parked { displaced }) => {
                if let Some(old) = displaced {
                    tracing::debug!(
                        "session {:08x}: a newer {} replaces the parked one",
                        req.session_id,
                        mine.label()
                    );
                    if old.close_if_parked() {
                        old.try_write(&reject_line(req.session_id));
                    }
                    old.close();
                }
                break;
            }
            Ok(Registered::Paired(peer)) => {
                let link = Arc::new(Link::new(req.session_id));
                if claim(&peer, &mine, &link, req.session_id) {
                    *lock(&mine.route) = Route::Paired {
                        dst: peer.clone(),
                        link: link.clone(),
                    };
                    tracing::info!(
                        "session {:08x} paired: {} <-> {}",
                        req.session_id,
                        mine.label(),
                        peer.label()
                    );
                    spawn_keepalive(&mine, &peer, &link);
                    break;
                }
            }
            Err(e) => {
                mine.close();
                return Err(e);
            }
        }
    }
    pump_side(&shared, req, &mine, reader);
    Ok(())
}

/// Claim the parked `peer` for `mine` (both already out of the session
/// table): send `A || sid || peer-external-addr` to both ends — each learns
/// the *other* end's relay-observed address — then forward what the peer
/// sent while parked and route its reader to `mine`. `false` when the peer
/// left in the meantime (nothing was sent).
fn claim(peer: &Arc<Slot>, mine: &Arc<Slot>, link: &Arc<Link>, session_id: u32) -> bool {
    let mut route = lock(&peer.route);
    let Route::Parked { early, .. } = &mut *route else {
        return false;
    };
    let early = std::mem::take(early);
    mine.try_write(&attach_line(session_id, peer.addr));
    peer.try_write(&attach_line(session_id, mine.addr));
    if !early.is_empty() {
        mine.try_write(&early);
    }
    *route = Route::Paired {
        dst: mine.clone(),
        link: link.clone(),
    };
    true
}

/// WebSocket keepalive: PING frames toward WebSocket clients while the
/// session lives (the tunnel payload may idle for long stretches once a
/// session moved to its direct path).
fn spawn_keepalive(a: &Arc<Slot>, b: &Arc<Slot>, link: &Arc<Link>) {
    if !(a.ws || b.ws) {
        return;
    }
    let (a, b, link) = (a.clone(), b.clone(), link.clone());
    let spawned = thread::Builder::new()
        .name(format!("ovrelay-wsping-{:08x}", link.session_id))
        .spawn(move || {
            let mut next = Instant::now() + WS_PING_EVERY;
            while !link.done.load(Ordering::Relaxed) {
                if Instant::now() >= next {
                    a.try_ping();
                    b.try_ping();
                    next += WS_PING_EVERY;
                }
                thread::sleep(Duration::from_millis(200));
            }
        });
    if let Err(e) = spawned {
        tracing::debug!("cannot spawn the WebSocket keepalive: {e}");
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

/// The paired peer of a direction and the session it belongs to.
type PeerLink = (Arc<Slot>, Arc<Link>);

/// Hand `bytes` read from `src` on: to the paired peer (cached in `dst`
/// once seen), or into the park buffer while unpaired. `false` when a
/// parked client overran [`EARLY_CAP`].
fn deliver(src: &Slot, dst: &mut Option<PeerLink>, bytes: &[u8]) -> bool {
    if dst.is_none() {
        let mut route = lock(&src.route);
        match &mut *route {
            Route::Paired { dst: d, link } => *dst = Some((d.clone(), link.clone())),
            Route::Parked { early, .. } => {
                if early.len() + bytes.len() > EARLY_CAP {
                    return false;
                }
                early.extend_from_slice(bytes);
                return true;
            }
            // Torn down: the socket is shut, the next read ends the loop.
            Route::Closed => return true,
        }
    }
    if let Some((d, _)) = dst {
        d.try_write(bytes);
    }
    true
}

/// This client's direction of the session, from the CONNECT until the
/// client leaves or the session is torn down.
///
/// Each `read` (up to [`PIPE_BUF`]) is forwarded with a single write — one
/// WebSocket binary message for a WebSocket destination — instead of one
/// write per tunnel frame. Partial frames are forwarded as they arrive (the
/// far-end carrier reassembles); only an incomplete *header* is held back
/// (at most 2 bytes) so a DISCONNECT split across reads is never half-sent.
///
/// While parked the same loop runs: a client that goes away (EOF, error,
/// DISCONNECT, WebSocket CLOSE) is removed from the session table at once,
/// so a later peer never pairs with a dead socket, and a WebSocket client's
/// PINGs are answered by the reader while it waits.
///
/// Once paired, a DISCONNECT (or EOF / hard error) writes a DISCONNECT
/// frame to the peer and closes both sides; closing shuts the sockets down,
/// which wakes the peer's blocked read so its direction ends too.
fn pump_side(
    shared: &Shared,
    req: ConnectRequest,
    src: &Arc<Slot>,
    mut reader: Box<dyn Read + Send>,
) {
    let session_id = req.session_id;
    let (mut bytes, mut reads) = (0u64, 0u64);
    let mut last_rx = Instant::now();
    let mut reason: &'static str = "EOF";
    let mut cursor = FrameCursor::default();
    let mut buf = vec![0u8; PIPE_BUF];
    // Unforwarded incomplete-header bytes kept at buf[..held].
    let mut held = 0usize;
    let mut dst: Option<PeerLink> = None;
    let end = loop {
        let n = match reader.read(&mut buf[held..]) {
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
        let (forward, stop) = match cursor.scan(&buf[..total]) {
            Scan::Forward { forward } => (forward, None),
            Scan::Stop { forward, end, why } => {
                reason = if why.is_some() {
                    "framing error"
                } else {
                    "DISCONNECT frame"
                };
                if let Some(why) = why {
                    tracing::debug!("session {session_id:08x} framing error: {why}");
                }
                (forward, Some(end))
            }
        };
        if forward > 0 && !deliver(src, &mut dst, &buf[..forward]) {
            tracing::debug!("session {session_id:08x}: parked client sent too much");
            reason = "parked overflow";
            break PipeEnd::Error;
        }
        if let Some(end) = stop {
            break end;
        }
        buf.copy_within(forward..total, 0);
        held = total - forward;
    };
    // Paired, or still parked? Decided under the route lock, so a peer
    // claiming us right now either got us (Paired) or sees Closed and
    // moves on.
    let paired = dst.or_else(|| {
        let mut route = lock(&src.route);
        match &*route {
            Route::Paired { dst, link } => Some((dst.clone(), link.clone())),
            _ => {
                *route = Route::Closed;
                None
            }
        }
    });
    let Some((dst, link)) = paired else {
        // Left (or was timed out / displaced) before any peer came. A
        // no-op when the reaper or a newer initiator removed us already.
        if shared.unpark(req, src) {
            tracing::debug!(
                "session {session_id:08x}: parked {} left ({reason})",
                src.label()
            );
        }
        src.close();
        return;
    };
    if let PipeEnd::Disconnect = end {
        // Peer asked for teardown (or went away cleanly): relay it.
        dst.try_write(&encode_frame(DISCONNECT, &[]).expect("fixed kind fits"));
    }
    let ended = Instant::now();
    // Our own side first: closing the peer wakes its reader with EOF, which
    // would otherwise race a DISCONNECT back onto this (still open) side.
    src.close();
    dst.close();
    link.finish(
        src.label(),
        dst.label(),
        PipeReport {
            bytes,
            reads,
            reason,
            ended,
            last_rx,
        },
    );
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

    fn test_shared(timeout: Duration) -> Shared {
        Shared {
            sessions: Mutex::new(HashMap::new()),
            timeout,
            limits: RelayLimits::DEFAULT,
            conns: Mutex::new(ConnTable::default()),
            ws_paths: Vec::new(),
        }
    }

    /// A slot without a socket (writes are dropped), parked as of now.
    fn parked_slot() -> Arc<Slot> {
        let s = Arc::new(Slot {
            write_lock: Arc::new(Mutex::new(Vec::new())),
            stream: Mutex::new(None),
            ws: false,
            addr: None,
            role: Mutex::new("test"),
            route: Mutex::new(Route::Closed),
        });
        s.begin_park();
        s
    }

    const VIEWER: ConnectRequest = ConnectRequest {
        session_id: 7,
        initiator: true,
    };
    const HOST: ConnectRequest = ConnectRequest {
        session_id: 7,
        initiator: false,
    };

    #[test]
    fn a_viewer_parks_while_the_host_is_busy_and_pairs_on_the_next_park() {
        let shared = test_shared(Duration::from_secs(30));
        // Session 1 pairs: the entry is gone while it runs (host busy).
        let h1 = parked_slot();
        assert!(matches!(
            shared.register(HOST, h1),
            Ok(Registered::Parked { displaced: None })
        ));
        assert!(matches!(
            shared.register(VIEWER, parked_slot()),
            Ok(Registered::Paired(_))
        ));
        assert_eq!(shared.pending(), 0);
        // A second viewer parks instead of being refused...
        let v2 = parked_slot();
        assert!(matches!(
            shared.register(VIEWER, v2.clone()),
            Ok(Registered::Parked { displaced: None })
        ));
        assert_eq!(shared.pending(), 1);
        // ...and the host's next park pairs with exactly that viewer.
        match shared.register(HOST, parked_slot()) {
            Ok(Registered::Paired(peer)) => assert!(Arc::ptr_eq(&peer, &v2)),
            _ => panic!("the host's re-park must pair with the waiting viewer"),
        }
        assert_eq!(shared.pending(), 0);
    }

    #[test]
    fn a_newer_viewer_displaces_the_parked_one_but_a_second_host_is_refused() {
        let shared = test_shared(Duration::from_secs(30));
        let v1 = parked_slot();
        assert!(shared.register(VIEWER, v1.clone()).is_ok());
        let v2 = parked_slot();
        match shared.register(VIEWER, v2.clone()) {
            Ok(Registered::Parked {
                displaced: Some(old),
            }) => assert!(Arc::ptr_eq(&old, &v1)),
            _ => panic!("the older viewer must be displaced"),
        }
        // The displaced slot is no longer in the table.
        assert!(!shared.unpark(VIEWER, &v1));
        let h1 = parked_slot();
        let h2 = parked_slot();
        shared.unpark(VIEWER, &v2);
        assert!(shared.register(HOST, h1).is_ok());
        assert!(matches!(
            shared.register(HOST, h2),
            Err(RelayError::Protocol(_))
        ));
    }

    #[test]
    fn the_reaper_times_out_old_parks_only() {
        let shared = test_shared(Duration::from_millis(50));
        let old = parked_slot();
        let fresh = parked_slot();
        *lock(&old.route) = Route::Parked {
            early: Vec::new(),
            since: Instant::now() - Duration::from_secs(1),
        };
        assert!(shared.register(VIEWER, old.clone()).is_ok());
        let other = ConnectRequest {
            session_id: 8,
            initiator: false,
        };
        assert!(shared.register(other, fresh.clone()).is_ok());
        shared.reap_expired();
        assert!(matches!(*lock(&old.route), Route::Closed));
        assert!(!shared.unpark(VIEWER, &old), "reaped from the table");
        assert!(fresh.parked_since().is_some(), "a fresh park stays");
        assert_eq!(shared.pending(), 1);
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
