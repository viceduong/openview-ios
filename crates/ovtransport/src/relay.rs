//! Relay tunnel carrier — a [`Carrier`] over one TCP connection to an
//! `ovrelay` server (spec v0.4 "relay tunnel"). Both ends of a relayed
//! session dial the relay, announce `session_id` + role, and from then on
//! speak framed tunnel traffic; the relay pipes frames between the two ends
//! of the same session.
//!
//! ```text
//! carrier -> relay:  b"C" || session_id(4B BE) || role(1B)   (CONNECT)
//! relay  -> carrier: b"O"                                    (accepted)
//! relay  -> carrier: b"A" || session_id(4B BE)               (peer attached)
//! relay  -> carrier: b"X" b'T' || session_id(4B BE)          (timed out / rejected)
//! then:  tunnel::Frame traffic both ways (relayed opaquely)
//! ```
//!
//! The carrier answers incoming PING frames with PONG inside `poll_recv`
//! (TCP-keepalive style, transparent to the upper layers) and records PONG
//! round trips in `last_pong()` for liveness checks.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Instant;

use crate::carrier::{Carrier, CarrierStats, TransportError};
use crate::tunnel::{self, FrameReader, DISCONNECT, PONG};

/// Which end of a relayed session we are. The relay pairs exactly one of
/// each per `session_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TunnelRole {
    /// The dialing side (ovcore handshake Client).
    Initiator,
    /// The listening side (ovcore handshake Server).
    Responder,
}

impl TunnelRole {
    fn wire_byte(self) -> u8 {
        match self {
            TunnelRole::Initiator => 0x00,
            TunnelRole::Responder => 0x01,
        }
    }
}

/// Outbound writer for a host-owned relay connection (e.g. an iOS
/// `URLSessionWebSocketTask` to `wss://host/relay`): receives each chunk of
/// relay-protocol bytes; returns `false` when the connection is gone.
pub type BridgeWriter = Box<dyn FnMut(&[u8]) -> bool + Send>;

/// The byte stream to the relay: a TCP socket we own, or a connection the
/// host app owns (it feeds inbound bytes via
/// [`RelayTunnelCarrier::bridge_feed`]).
enum Io {
    Tcp(TcpStream),
    Bridge(BridgeWriter),
}

impl std::fmt::Debug for Io {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Io::Tcp(s) => f.debug_tuple("Tcp").field(s).finish(),
            Io::Bridge(_) => f.write_str("Bridge"),
        }
    }
}

/// A carrier whose datagrams flow through an `ovrelay` server.
///
/// Framing state in `poll_recv`: TCP is a byte stream, so inbound bytes
/// accumulate in `rx` and are cut into frames as they complete. Control
/// lines (`O`/`A`/`XT`) can arrive interleaved with frames, so the reader
/// also recognizes them by their non-frame first byte.
#[derive(Debug)]
pub struct RelayTunnelCarrier {
    io: Option<Io>,
    session_id: u32,
    /// Peer attached and confirmed by the relay (`A` line seen)?
    attached: bool,
    /// Inbound byte accumulator (control lines + partial frames).
    rx: Vec<u8>,
    /// Persistent socket read buffer (allocated on first TCP read; a
    /// per-poll stack buffer would be zeroed on every idle poll).
    scratch: Vec<u8>,
    /// The peer's relay-observed address from the `A` line, if the relay
    /// reported one (None for an IPv6 peer or a pre-v0.5 relay).
    peer_external: Option<SocketAddr>,
    /// Local UDP port announced by the peer's most recent PROBE frame.
    last_probe: Option<u16>,
    /// Local port echoed by the peer's most recent PUNCH_ACK frame.
    last_punch_ack: Option<u16>,
    /// The relay closed the TCP stream (clean EOF).
    eof: bool,
    /// Outbound bytes the socket did not accept yet (send buffer full):
    /// `tx[tx_off..]` is pending, in order. Flushed opportunistically by
    /// every send and `poll_recv`, so a writer never blocks on a slow link
    /// until the backlog exceeds [`TX_SOFT_CAP`].
    tx: Vec<u8>,
    tx_off: usize,
    frames: FrameReader,
    last_pong: Option<Instant>,
    stats: CarrierStats,
    /// Event-driven receive (see [`set_read_waker`](Self::set_read_waker)):
    /// a thread blocked on the socket's readability that wakes the shell.
    watcher: Option<ReadWatcher>,
}

/// Wake callback for [`RelayTunnelCarrier::set_read_waker`]. Runs on the
/// watcher thread: it must only *schedule* a pump (e.g. dispatch onto the
/// shell's session queue) — never call back into the carrier or its owner
/// synchronously, and never block.
pub type ReadWake = Box<dyn FnMut() + Send>;

/// Watcher state shared with the thread: `armed` = the owner polled since
/// the last wake (so a still-readable socket means *new* bytes, not the
/// ones the owner has not drained yet); `stop` = the carrier is gone.
#[derive(Default)]
struct WatchState {
    armed: bool,
    stop: bool,
}

struct WatchShared {
    state: std::sync::Mutex<WatchState>,
    cv: std::sync::Condvar,
}

/// Handle to the readability watcher thread. Dropping it stops the thread:
/// once `drop` returns the wake callback is never called again (the
/// callback runs under the same lock `drop` takes).
struct ReadWatcher {
    shared: std::sync::Arc<WatchShared>,
}

impl std::fmt::Debug for ReadWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReadWatcher")
    }
}

/// How long one readiness wait blocks before the watcher re-checks `stop`.
const WATCH_POLL: std::time::Duration = std::time::Duration::from_millis(200);

impl ReadWatcher {
    fn spawn(stream: TcpStream, mut wake: ReadWake) -> io::Result<Self> {
        let shared = std::sync::Arc::new(WatchShared {
            // Armed from the start: bytes already queued wake at once.
            state: std::sync::Mutex::new(WatchState {
                armed: true,
                stop: false,
            }),
            cv: std::sync::Condvar::new(),
        });
        let t = std::sync::Arc::clone(&shared);
        std::thread::Builder::new()
            .name("ov-relay-watch".into())
            .spawn(move || {
                // `stream` (a clone of the carrier's socket) keeps the
                // descriptor valid for as long as this thread polls it.
                let sock = raw_of(&stream);
                loop {
                    // 1. Wait until the owner drained since the last wake.
                    {
                        let Ok(mut st) = t.state.lock() else { return };
                        while !st.armed && !st.stop {
                            st = match t.cv.wait(st) {
                                Ok(g) => g,
                                Err(_) => return,
                            };
                        }
                        if st.stop {
                            return;
                        }
                    }
                    // 2. Block until the socket is readable (data, EOF or
                    // error all count: the owner's next poll sorts it out).
                    loop {
                        match wait_readable(sock, WATCH_POLL) {
                            Ok(true) => break,
                            Ok(false) => {}
                            // EINTR and friends: retry after a breather.
                            Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
                        }
                        if t.state.lock().map_or(true, |s| s.stop) {
                            return;
                        }
                    }
                    // 3. Wake the owner (under the lock: never after drop).
                    let Ok(mut st) = t.state.lock() else { return };
                    if st.stop {
                        return;
                    }
                    st.armed = false;
                    wake();
                }
            })?;
        Ok(Self { shared })
    }

    /// The owner polled the socket: the next readability is news.
    fn rearm(&self) {
        if let Ok(mut st) = self.shared.state.lock() {
            if !st.armed {
                st.armed = true;
                self.shared.cv.notify_all();
            }
        }
    }
}

impl Drop for ReadWatcher {
    fn drop(&mut self) {
        if let Ok(mut st) = self.shared.state.lock() {
            st.stop = true;
        }
        self.shared.cv.notify_all();
    }
}

#[cfg(unix)]
type WatchSock = std::os::fd::RawFd;
#[cfg(windows)]
type WatchSock = usize;

#[cfg(unix)]
fn raw_of(s: &TcpStream) -> WatchSock {
    std::os::fd::AsRawFd::as_raw_fd(s)
}
#[cfg(windows)]
fn raw_of(s: &TcpStream) -> WatchSock {
    std::os::windows::io::AsRawSocket::as_raw_socket(s) as usize
}

/// Block up to `timeout` until `sock` is readable (or hung up / errored).
/// `Ok(true)` = readable, `Ok(false)` = timed out. Declared by hand like the
/// rest of this workspace's OS calls (no extra crates; the iOS staticlib
/// stays pure Rust).
#[cfg(unix)]
fn wait_readable(sock: WatchSock, timeout: std::time::Duration) -> io::Result<bool> {
    #[repr(C)]
    struct PollFd {
        fd: i32,
        events: i16,
        revents: i16,
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    type NFds = std::ffi::c_ulong;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    type NFds = std::ffi::c_uint;
    extern "C" {
        fn poll(fds: *mut PollFd, nfds: NFds, timeout: i32) -> i32;
    }
    const POLLIN: i16 = 0x0001;
    let mut p = PollFd {
        fd: sock,
        events: POLLIN,
        revents: 0,
    };
    let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    // SAFETY: one valid pollfd; the descriptor is kept open by the caller.
    let rc = unsafe { poll(&mut p, 1, ms) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(rc > 0 && p.revents != 0)
}

#[cfg(windows)]
fn wait_readable(sock: WatchSock, timeout: std::time::Duration) -> io::Result<bool> {
    #[repr(C)]
    struct WsaPollFd {
        fd: usize,
        events: i16,
        revents: i16,
    }
    #[link(name = "ws2_32")]
    extern "system" {
        fn WSAPoll(fds: *mut WsaPollFd, nfds: u32, timeout: i32) -> i32;
    }
    const POLLRDNORM: i16 = 0x0100;
    let mut p = WsaPollFd {
        fd: sock,
        events: POLLRDNORM,
        revents: 0,
    };
    let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    // SAFETY: one valid WSAPOLLFD; the socket is kept open by the caller.
    let rc = unsafe { WSAPoll(&mut p, 1, ms) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(rc > 0 && p.revents != 0)
}

#[cfg(not(any(unix, windows)))]
fn wait_readable(_sock: WatchSock, timeout: std::time::Duration) -> io::Result<bool> {
    std::thread::sleep(timeout);
    Ok(true)
}

#[cfg(not(any(unix, windows)))]
type WatchSock = ();
#[cfg(not(any(unix, windows)))]
fn raw_of(_s: &TcpStream) -> WatchSock {}

impl RelayTunnelCarrier {
    /// Dial the relay, send the CONNECT line and wait (blocking, at most
    /// [`CONNECT_TIMEOUT`]) for the `O` (accepted) reply. A relay that never
    /// answers surfaces as [`TransportError::Io`] with `TimedOut`.
    ///
    /// On success the socket is left in non-blocking mode for the rest of
    /// its life: `poll_recv` reads until `WouldBlock` without toggling the
    /// mode per poll, and writes fall back to a blocking completion only when
    /// the send buffer is full (see `write_all`).
    pub fn connect(
        relay: SocketAddr,
        session_id: u32,
        role: TunnelRole,
    ) -> Result<Self, TransportError> {
        let stream = TcpStream::connect(relay).map_err(TransportError::Io)?;
        // Best-effort nodelay: handshake/record-sized frames, Nagle would
        // add up to 200 ms per round trip on the request/response pump.
        let _ = stream.set_nodelay(true);
        stream
            .set_read_timeout(Some(CONNECT_TIMEOUT))
            .map_err(TransportError::Io)?;
        let mut me = Self::with_io(Io::Tcp(stream), session_id);
        me.write_all(&connect_line(session_id, role))?;
        // Wait for the relay's verdict before returning a usable carrier:
        // a blocking read (bounded by the read timeout) instead of spinning
        // on non-blocking polls.
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        while me.rx.is_empty() {
            if Instant::now() >= deadline {
                return Err(TransportError::Io(io::ErrorKind::TimedOut.into()));
            }
            let Some(Io::Tcp(stream)) = me.io.as_mut() else {
                return Err(TransportError::Closed);
            };
            let mut chunk = [0u8; 1024];
            match stream.read(&mut chunk) {
                Ok(0) => return Err(TransportError::Closed),
                Ok(n) => me.rx.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    return Err(TransportError::Io(io::ErrorKind::TimedOut.into()));
                }
                Err(e) => return Err(TransportError::Io(e)),
            }
        }
        if me.rx[0] != b'O' {
            // Rejected (`XT`) or garbage before acceptance: tear down.
            return Err(TransportError::Closed);
        }
        me.rx.drain(..1);
        if let Some(Io::Tcp(stream)) = me.io.as_mut() {
            stream.set_read_timeout(None).map_err(TransportError::Io)?;
            stream.set_nonblocking(true).map_err(TransportError::Io)?;
        }
        // An `A` may have arrived in the same read; consume it so the
        // carrier comes back attached when possible.
        while let Some(true) = me.take_control()? {}
        Ok(me)
    }

    /// Start a relay session over a connection the host app owns (an iOS
    /// WebSocket to `wss://host/relay`, which carries the identical byte
    /// stream). Sends CONNECT through `writer` and returns immediately: the
    /// host feeds every inbound chunk with [`bridge_feed`](Self::bridge_feed)
    /// and the relay's `O`/`A`/`XT` replies are handled in `poll_recv`
    /// (`XT` surfaces as [`TransportError::Closed`]).
    pub fn connect_bridge(
        writer: BridgeWriter,
        session_id: u32,
        role: TunnelRole,
    ) -> Result<Self, TransportError> {
        let mut me = Self::with_io(Io::Bridge(writer), session_id);
        me.write_all(&connect_line(session_id, role))?;
        Ok(me)
    }

    /// Inbound bytes from a host-owned connection (see
    /// [`connect_bridge`](Self::connect_bridge)); processed on the next poll.
    pub fn bridge_feed(&mut self, bytes: &[u8]) {
        if self.io.is_some() {
            self.rx.extend_from_slice(bytes);
        }
    }

    /// The host-owned connection closed: the tunnel ends once the buffered
    /// bytes are drained.
    pub fn bridge_eof(&mut self) {
        self.eof = true;
    }

    fn with_io(io: Io, session_id: u32) -> Self {
        Self {
            io: Some(io),
            session_id,
            attached: false,
            rx: Vec::new(),
            scratch: Vec::new(),
            peer_external: None,
            last_probe: None,
            last_punch_ack: None,
            eof: false,
            tx: Vec::new(),
            tx_off: 0,
            frames: FrameReader::default(),
            last_pong: None,
            stats: CarrierStats::default(),
            watcher: None,
        }
    }

    /// Event-driven receive for shells that cannot block on the socket
    /// themselves (the iOS app drives everything from one serial queue):
    /// spawn a thread that waits for the relay socket to turn readable and
    /// calls `wake`, which should schedule a `poll_recv` on the owner's
    /// thread. Wakes are coalesced: after one wake the thread waits until
    /// the owner polled again before watching the socket anew, so a burst
    /// costs one wake per drain, never a busy loop. `None` stops watching.
    ///
    /// Returns `Ok(false)` (and installs nothing) for a host-owned bridge
    /// connection — its bytes arrive through `bridge_feed`, already event
    /// driven — or a closed carrier.
    pub fn set_read_waker(&mut self, wake: Option<ReadWake>) -> io::Result<bool> {
        self.watcher = None;
        let Some(wake) = wake else { return Ok(false) };
        let stream = match self.io.as_ref() {
            Some(Io::Tcp(s)) => s.try_clone()?,
            _ => return Ok(false),
        };
        self.watcher = Some(ReadWatcher::spawn(stream, wake)?);
        Ok(true)
    }

    /// Whether a readability watcher is installed.
    pub fn has_read_waker(&self) -> bool {
        self.watcher.is_some()
    }

    /// Liveness probe: send a PING with a caller-chosen nonce. The peer
    /// carrier answers PONG inside its own `poll_recv`; when the PONG comes
    /// back through the relay, `last_pong()` updates.
    pub fn ping(&mut self, nonce: u64) -> Result<(), TransportError> {
        self.send_frame(tunnel::PING, &nonce.to_be_bytes())
    }

    /// When the most recent PONG round trip completed, if any.
    pub fn last_pong(&self) -> Option<Instant> {
        self.last_pong
    }

    /// `true` once the relay confirmed the peer attached (`A` line).
    pub fn is_attached(&self) -> bool {
        self.attached
    }

    /// The session this carrier is bound to.
    pub fn session_id(&self) -> u32 {
        self.session_id
    }

    /// The peer's address as the relay observed it (`A` line), if reported.
    /// This is the candidate punch target; `None` means the relay could not
    /// provide an IPv4 address (IPv6 peer) and the caller should stay on the
    /// relay path.
    pub fn peer_external_addr(&self) -> Option<SocketAddr> {
        self.peer_external
    }

    /// The local UDP port the peer announced in its most recent PROBE frame
    /// (i.e. the port to punch datagrams to). Cleared by [`take_probe`].
    pub fn last_probe(&self) -> Option<u16> {
        self.last_probe
    }

    /// Take the most recent PROBE (clearing it): the peer's local UDP port.
    pub fn take_probe(&mut self) -> Option<u16> {
        self.last_probe.take()
    }

    /// The local port echoed by the peer's most recent PUNCH_ACK frame.
    /// Cleared by [`take_punch_ack`].
    pub fn last_punch_ack(&self) -> Option<u16> {
        self.last_punch_ack
    }

    /// Take the most recent PUNCH_ACK (clearing it): the peer's confirmation
    /// that a punch datagram arrived on our announced local port.
    pub fn take_punch_ack(&mut self) -> Option<u16> {
        self.last_punch_ack.take()
    }

    /// Send one framed control/data message over the tunnel. Crate-visible:
    /// the `punch` helper uses it for PROBE frames.
    pub(crate) fn send_frame(&mut self, kind: u8, payload: &[u8]) -> Result<(), TransportError> {
        let wire = tunnel::encode_frame(kind, payload).map_err(|e| match e {
            tunnel::FrameError::TooLarge(n, _) => TransportError::FrameTooLarge(n),
            other => TransportError::SendFailed(io::Error::new(
                io::ErrorKind::InvalidData,
                other.to_string(),
            )),
        })?;
        self.write_all(&wire)
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
        match self.io.as_mut().ok_or(TransportError::Closed)? {
            Io::Tcp(stream) => {
                if self.tx_off == self.tx.len() {
                    // Nothing queued: write straight through, queue the rest.
                    let n = write_some(stream, bytes).map_err(TransportError::SendFailed)?;
                    if n < bytes.len() {
                        self.tx.clear();
                        self.tx_off = 0;
                        self.tx.extend_from_slice(&bytes[n..]);
                    }
                } else {
                    self.tx.extend_from_slice(bytes);
                    let n = write_some(stream, &self.tx[self.tx_off..])
                        .map_err(TransportError::SendFailed)?;
                    self.tx_off += n;
                }
                // Bound the backlog: past the cap, complete the excess the
                // old blocking way (sleep-retry until the kernel takes it).
                let pending = self.tx.len() - self.tx_off;
                if pending > TX_SOFT_CAP {
                    let end = self.tx.len() - TX_SOFT_CAP;
                    write_all_nonblocking(stream, &self.tx[self.tx_off..end])
                        .map_err(TransportError::SendFailed)?;
                    self.tx_off = end;
                }
                self.compact_tx();
                Ok(())
            }
            Io::Bridge(writer) => {
                if writer(bytes) {
                    Ok(())
                } else {
                    Err(TransportError::SendFailed(io::ErrorKind::BrokenPipe.into()))
                }
            }
        }
    }

    /// Push queued outbound bytes to the socket without blocking.
    fn flush_tx(&mut self) -> Result<(), TransportError> {
        if self.tx_off == self.tx.len() {
            return Ok(());
        }
        if let Some(Io::Tcp(stream)) = self.io.as_mut() {
            let n =
                write_some(stream, &self.tx[self.tx_off..]).map_err(TransportError::SendFailed)?;
            self.tx_off += n;
            self.compact_tx();
        }
        Ok(())
    }

    /// Drop the already-sent prefix of `tx` once it dominates the buffer.
    fn compact_tx(&mut self) {
        if self.tx_off == self.tx.len() {
            self.tx.clear();
            self.tx_off = 0;
        } else if self.tx_off > 64 * 1024 && self.tx_off * 2 > self.tx.len() {
            self.tx.drain(..self.tx_off);
            self.tx_off = 0;
        }
    }

    /// Read everything currently available into `rx` (the socket is already
    /// non-blocking): loops until `WouldBlock`, EOF, or [`READ_BUDGET`] bytes
    /// so one poll drains a burst instead of 16 KiB per call. EOF sets
    /// `self.eof`; hard errors propagate. A bridge has nothing to read here:
    /// the host already pushed its bytes via `bridge_feed`.
    fn read_some(&mut self) -> Result<usize, TransportError> {
        let stream = match self.io.as_mut().ok_or(TransportError::Closed)? {
            Io::Tcp(stream) => stream,
            Io::Bridge(_) => return Ok(0),
        };
        if self.scratch.is_empty() {
            self.scratch = vec![0u8; 64 * 1024];
        }
        let chunk = &mut self.scratch;
        let mut total = 0usize;
        while total < READ_BUDGET {
            match stream.read(chunk) {
                Ok(0) => {
                    self.eof = true;
                    break;
                }
                Ok(n) => {
                    self.rx.extend_from_slice(&chunk[..n]);
                    total += n;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(TransportError::RecvFailed(e)),
            }
        }
        Ok(total)
    }

    /// Cut one control line or frame off the front of `rx`.
    ///
    /// Returns:
    /// - `Ok(Some(true))`  — a frame was decoded into `self.frames`-independent
    ///   form and handled here (control line) — caller continues draining.
    /// - `Ok(Some(false))` — a tunnel frame is ready via `self.frames.next()`.
    /// - `Ok(None)`        — need more bytes.
    fn take_control(&mut self) -> Result<Option<bool>, TransportError> {
        if self.rx.is_empty() {
            return Ok(None);
        }
        // A partially-received frame is staged in the reader: every byte
        // still belongs to that frame, so hand them over wholesale instead
        // of misreading a payload byte as a control line.
        if self.frames.pending() > 0 {
            let bytes = std::mem::take(&mut self.rx);
            self.frames.feed(&bytes);
            return Ok(Some(false));
        }
        match self.rx[0] {
            // Control lines can only be at stream start (before any frame);
            // anything else would be a relay protocol violation.
            b'O' if self.frames.pending() == 0 => {
                self.rx.drain(..1);
                Ok(Some(true))
            }
            b'A' if self.frames.pending() == 0 => {
                // Line layout (v0.5): 'A' || sid(4B BE) || peer_ipv4(4B) ||
                // peer_port(2B BE) — always exactly 11 bytes (ovrelay reports
                // an IPv6/no-v4 peer as 4 zero IP bytes + a zero port). The
                // line must be consumed WHOLE before we commit the attach:
                // if connect() returned after only the 5-byte 'A'+sid prefix
                // arrived (a TCP split), the 6 trailing address bytes would
                // sit in the buffer and poll_recv would mis-read them as two
                // empty DATA frames. Waiting for the full 11 bytes makes the
                // line self-delimiting and the buffer clean on return.
                if self.rx.len() < 11 {
                    return Ok(None); // partial line; wait for the remaining bytes
                }
                let sid = u32::from_be_bytes([self.rx[1], self.rx[2], self.rx[3], self.rx[4]]);
                if sid != self.session_id {
                    return Err(TransportError::RecvFailed(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "relay attached wrong session {sid} (want {})",
                            self.session_id
                        ),
                    )));
                }
                let ip = std::net::Ipv4Addr::new(self.rx[5], self.rx[6], self.rx[7], self.rx[8]);
                let port = u16::from_be_bytes([self.rx[9], self.rx[10]]);
                // A zeroed IP means "no v4 address — stay on relay".
                self.peer_external = if ip.is_unspecified() {
                    None
                } else {
                    Some(SocketAddr::new(ip.into(), port))
                };
                self.attached = true;
                self.rx.drain(..11);
                Ok(Some(true))
            }
            b'X' if self.frames.pending() == 0 => {
                if self.rx.len() < 6 {
                    return Ok(None); // partial control line; wait for more
                }
                // Rejected / timed out: b"XT" || session_id. Surface as Closed.
                self.rx.drain(..6);
                self.io = None;
                Err(TransportError::Closed)
            }
            k if k <= tunnel::PUNCH_ACK => {
                // Tunnel frame bytes: hand the whole buffer to the reader,
                // which owns the framing state from here on.
                let bytes = std::mem::take(&mut self.rx);
                self.frames.feed(&bytes);
                Ok(Some(false))
            }
            k => Err(TransportError::RecvFailed(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("relay sent unknown control byte 0x{k:02x}"),
            ))),
        }
    }
}

/// How long [`RelayTunnelCarrier::connect`] waits for the relay's `O`.
pub const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Max bytes one `poll_recv` pulls off the socket before decoding (bounds
/// the work per poll while still draining a whole burst).
const READ_BUDGET: usize = 1024 * 1024;

/// Most unsent bytes a relay carrier queues before a send completes the
/// excess synchronously (see [`Carrier::pending_send`]).
pub const TX_SOFT_CAP: usize = 256 * 1024;

/// Write as much of `bytes` as the non-blocking socket accepts right now;
/// returns the count (0 when the send buffer is full).
fn write_some(stream: &mut TcpStream, bytes: &[u8]) -> io::Result<usize> {
    let mut done = 0usize;
    while done < bytes.len() {
        match stream.write(&bytes[done..]) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => done += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) => return Err(e),
        }
    }
    Ok(done)
}

/// How long a full send buffer may make no progress before the write fails
/// (a peer that stopped reading entirely, or a wedged relay path).
pub const WRITE_STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// `write_all` for a socket kept in non-blocking mode for life: the fast
/// path is a plain non-blocking write; if the send buffer fills
/// (`WouldBlock`), the remainder is retried after short sleeps until it is
/// accepted, so a frame is never torn (the stream is ordered and frames must
/// stay whole). The socket is never switched back to blocking mode (an OS
/// readiness registration such as `WSAEventSelect` forbids that), and a
/// buffer that accepts nothing for [`WRITE_STALL_TIMEOUT`] fails the write.
fn write_all_nonblocking(stream: &mut TcpStream, mut bytes: &[u8]) -> io::Result<()> {
    let mut last_progress = Instant::now();
    while !bytes.is_empty() {
        match stream.write(bytes) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => {
                bytes = &bytes[n..];
                last_progress = Instant::now();
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                if last_progress.elapsed() >= WRITE_STALL_TIMEOUT {
                    return Err(io::ErrorKind::TimedOut.into());
                }
                // The kernel drains the buffer at link rate; a short sleep
                // keeps this from spinning while costing at most ~1 ms of
                // send latency once space frees up.
                std::thread::sleep(std::time::Duration::from_micros(500));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// CONNECT: `b"C" || session_id(4B BE) || role(1B)`.
fn connect_line(session_id: u32, role: TunnelRole) -> [u8; 6] {
    let sid = session_id.to_be_bytes();
    [b'C', sid[0], sid[1], sid[2], sid[3], role.wire_byte()]
}

impl Carrier for RelayTunnelCarrier {
    fn send(&mut self, datagram: &[u8]) -> Result<usize, TransportError> {
        self.send_frame(tunnel::DATA, datagram)?;
        self.stats.record_sent(datagram.len());
        Ok(datagram.len())
    }

    fn poll_recv(&mut self, sink: &mut dyn FnMut(&[u8])) -> Result<usize, TransportError> {
        // Phase 0: push out anything a full send buffer left queued.
        self.flush_tx()?;
        // Phase 1: read everything available (bounded by READ_BUDGET); clean
        // EOF is a closed tunnel (after buffered bytes are delivered below).
        let read = self.read_some();
        // The socket was just drained: a readable socket from here on is
        // news worth a wake (event-driven shells, see set_read_waker).
        if let Some(w) = &self.watcher {
            w.rearm();
        }
        read?;
        // Phase 2: split control lines and frames apart, then deliver every
        // decoded frame (not one per poll — a burst drains in one call).
        loop {
            match self.take_control()? {
                Some(true) => continue, // control line consumed
                Some(false) => break,   // frame bytes staged in the reader
                None => break,
            }
        }
        let delivered = self.drain_decoded(sink)?;
        if delivered == 0 && self.eof {
            // Everything buffered is drained; the stream is gone.
            self.io = None;
            return Err(TransportError::Closed);
        }
        Ok(delivered)
    }

    fn close(&mut self) -> Result<(), TransportError> {
        // Best-effort graceful teardown: tell the peer (behind anything still
        // queued, given a bounded moment to drain), then drop the stream.
        if self.io.is_some() {
            let _ = self.send_frame(DISCONNECT, &[]);
            let deadline = Instant::now() + std::time::Duration::from_secs(1);
            while self.tx_off < self.tx.len() && Instant::now() < deadline {
                if self.flush_tx().is_err() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        self.io = None;
        // The watcher holds a clone of the socket: release it too so the
        // connection really closes now.
        self.watcher = None;
        self.tx.clear();
        self.tx_off = 0;
        self.rx.clear();
        Ok(())
    }

    fn is_reliable(&self) -> bool {
        true
    }

    fn stats(&self) -> &CarrierStats {
        &self.stats
    }

    fn pending_send(&self) -> usize {
        self.tx.len() - self.tx_off
    }

    fn raw_socket(&self) -> Option<crate::carrier::RawSock> {
        match self.io.as_ref()? {
            #[cfg(windows)]
            Io::Tcp(s) => Some(std::os::windows::io::AsRawSocket::as_raw_socket(s)),
            #[cfg(unix)]
            Io::Tcp(s) => Some(std::os::fd::AsRawFd::as_raw_fd(s)),
            #[cfg(not(any(windows, unix)))]
            Io::Tcp(_) => None,
            Io::Bridge(_) => None,
        }
    }
}

impl RelayTunnelCarrier {
    /// Wait (up to `timeout`) for the relay's attach confirmation (`A`
    /// line): the peer joined the session and the tunnel is live. Returns
    /// `Ok(true)` once attached, `Ok(false)` when `timeout` elapsed first,
    /// and [`TransportError::Closed`] when the relay timed the park out
    /// (`XT`) or closed the stream — for a parked side that is the normal
    /// "nobody came" outcome, not a failure.
    ///
    /// Only control lines are consumed: tunnel frames that arrive right
    /// behind the `A` stay buffered for the next `poll_recv`, so nothing the
    /// peer sends first is lost. Callers should give this the relay's full
    /// park window (~30 s) and start their handshake timer only afterwards.
    pub fn wait_attached(&mut self, timeout: std::time::Duration) -> Result<bool, TransportError> {
        let deadline = Instant::now() + timeout;
        loop {
            while let Some(true) = self.take_control()? {}
            if self.attached {
                return Ok(true);
            }
            if self.eof {
                self.io = None;
                return Err(TransportError::Closed);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            if self.read_some()? == 0 && !self.eof {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }

    /// Deliver every decoded frame currently in the reader, handling PING
    /// (auto-PONG) and PONG (liveness timestamp) inline. DISCONNECT closes
    /// the tunnel; DATA is the only frame that reaches the sink.
    fn drain_decoded(&mut self, sink: &mut dyn FnMut(&[u8])) -> Result<usize, TransportError> {
        let mut delivered = 0usize;
        loop {
            // Borrow DATA payloads straight out of the reader's buffer (no
            // per-frame allocation); PING payloads are copied out first
            // because answering needs `&mut self`.
            let (kind, payload) = match self.frames.next_ref() {
                Ok(Some(f)) => f,
                Ok(None) => break,
                Err(e) => {
                    return Err(TransportError::RecvFailed(io::Error::new(
                        io::ErrorKind::InvalidData,
                        e.to_string(),
                    )));
                }
            };
            if kind == tunnel::DATA {
                sink(payload);
                self.stats.record_recv(payload.len());
                delivered += 1;
                continue;
            }
            let frame = tunnel::DecodedFrame {
                kind,
                payload: payload.to_vec(),
            };
            match frame.kind {
                tunnel::PING => {
                    // Answer transparently, like a TCP keepalive.
                    self.send_frame(PONG, &frame.payload)?;
                }
                tunnel::PONG => {
                    self.last_pong = Some(Instant::now());
                }
                DISCONNECT => {
                    self.io = None;
                    self.rx.clear();
                    return Err(TransportError::Closed);
                }
                tunnel::PROBE => {
                    if frame.payload.len() == 2 {
                        self.last_probe =
                            Some(u16::from_be_bytes([frame.payload[0], frame.payload[1]]));
                    }
                }
                tunnel::PUNCH_ACK => {
                    if frame.payload.len() == 2 {
                        self.last_punch_ack =
                            Some(u16::from_be_bytes([frame.payload[0], frame.payload[1]]));
                    }
                }
                _ => unreachable!("FrameReader rejects kinds > PUNCH_ACK"),
            }
        }
        Ok(delivered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    /// A minimal fake relay: accepts one CONNECT, replies `O`, then echoes
    /// frames back (so PING->PONG and DATA->DATA round trips work). Answers
    /// `XT` if `reject` is set.
    fn fake_relay(reject: bool) -> (SocketAddr, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut connect = [0u8; 6];
            stream.read_exact(&mut connect).unwrap();
            if reject {
                let _ = stream.write_all(b"XT");
                let _ = stream.write_all(&connect[1..5]);
                let _ = stream.shutdown(std::net::Shutdown::Both);
                return;
            }
            stream.write_all(b"O").unwrap();
            // v0.5 'A' line: sid + relay-observed peer address (fake 0.0.0.0:0).
            stream.write_all(b"A").unwrap();
            stream.write_all(&connect[1..5]).unwrap();
            stream.write_all(&[0u8; 6]).unwrap();
            // Echo frames until EOF.
            let mut rx = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => rx.extend_from_slice(&buf[..n]),
                }
                // Echo complete frames back.
                while rx.len() >= 3 {
                    let len = u16::from_be_bytes([rx[1], rx[2]]) as usize;
                    if rx.len() < 3 + len {
                        break;
                    }
                    let frame: Vec<u8> = rx.drain(..3 + len).collect();
                    let _ = stream.write_all(&frame);
                }
            }
        });
        (addr, handle)
    }

    #[test]
    fn connect_and_echo_ping_pong() {
        let (addr, server) = fake_relay(false);
        let mut c = RelayTunnelCarrier::connect(addr, 7, TunnelRole::Initiator).unwrap();
        assert_eq!(c.session_id(), 7);

        // PING round trip: the fake relay echoes it back, but the echo is a
        // PING — the carrier auto-answers PONG and the relay echoes that
        // back as a PONG, updating last_pong. The pump loop also consumes
        // the relay's `A` line, so is_attached is asserted afterwards.
        c.ping(0xDEAD_BEEF).unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        while (c.last_pong().is_none() || !c.is_attached()) && Instant::now() < deadline {
            let _ = c.poll_recv(&mut |_| {});
            thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(c.is_attached(), "relay never confirmed attach");
        assert!(c.last_pong().is_some(), "no PONG came back");
        // Close before joining: the relay thread blocks on read until EOF.
        c.close().unwrap();
        drop(c);
        server.join().unwrap();
    }

    #[test]
    fn data_roundtrip_through_fake_relay() {
        let (addr, server) = fake_relay(false);
        let mut c = RelayTunnelCarrier::connect(addr, 9, TunnelRole::Responder).unwrap();
        c.send(b"payload").unwrap();
        let mut got = Vec::new();
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        while got.is_empty() && Instant::now() < deadline {
            c.poll_recv(&mut |d| got.push(d.to_vec())).unwrap();
            if got.is_empty() {
                thread::sleep(std::time::Duration::from_millis(2));
            }
        }
        assert_eq!(got, vec![&b"payload"[..]]);
        assert_eq!(c.stats().sent_datagrams, 1);
        assert_eq!(c.stats().recv_datagrams, 1);
        c.close().unwrap();
        drop(c);
        server.join().unwrap();
    }

    /// A fake relay that accepts (`O`), then after `delay` either attaches
    /// (`A` + one DATA frame right behind it) or times the park out (`XT`).
    fn delayed_relay(
        delay: std::time::Duration,
        attach: bool,
    ) -> (SocketAddr, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut connect = [0u8; 6];
            stream.read_exact(&mut connect).unwrap();
            stream.write_all(b"O").unwrap();
            thread::sleep(delay);
            if attach {
                let mut out = vec![b'A'];
                out.extend_from_slice(&connect[1..5]);
                out.extend_from_slice(&[0u8; 6]);
                out.extend_from_slice(&tunnel::encode_frame(tunnel::DATA, b"first").unwrap());
                stream.write_all(&out).unwrap();
                // Hold the stream open until the client hangs up.
                let mut sink = [0u8; 64];
                while matches!(stream.read(&mut sink), Ok(n) if n > 0) {}
            } else {
                let _ = stream.write_all(b"XT");
                let _ = stream.write_all(&connect[1..5]);
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        });
        (addr, handle)
    }

    #[test]
    fn wait_attached_waits_past_a_short_handshake_window() {
        // The peer attaches 600 ms after we park — longer than a 2 s
        // handshake pump would tolerate only by luck; wait_attached holds on.
        let (addr, server) = delayed_relay(std::time::Duration::from_millis(600), true);
        let mut c = RelayTunnelCarrier::connect(addr, 0x0A0B, TunnelRole::Initiator).unwrap();
        assert!(!c.is_attached());
        let t0 = Instant::now();
        assert!(c.wait_attached(std::time::Duration::from_secs(10)).unwrap());
        assert!(c.is_attached());
        assert!(t0.elapsed() >= std::time::Duration::from_millis(500));
        // The DATA frame that rode in behind the `A` line is not lost.
        let mut got = Vec::new();
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        while got.is_empty() && Instant::now() < deadline {
            c.poll_recv(&mut |d| got.push(d.to_vec())).unwrap();
        }
        assert_eq!(got, vec![b"first".to_vec()]);
        c.close().unwrap();
        drop(c);
        server.join().unwrap();
    }

    #[test]
    fn wait_attached_reports_park_timeout_as_closed() {
        let (addr, server) = delayed_relay(std::time::Duration::from_millis(100), false);
        let mut c = RelayTunnelCarrier::connect(addr, 3, TunnelRole::Responder).unwrap();
        let err = c
            .wait_attached(std::time::Duration::from_secs(10))
            .unwrap_err();
        assert!(matches!(err, TransportError::Closed));
        server.join().unwrap();
    }

    #[test]
    fn wait_attached_times_out_without_a_verdict() {
        let (addr, server) = delayed_relay(std::time::Duration::from_millis(800), true);
        let mut c = RelayTunnelCarrier::connect(addr, 4, TunnelRole::Initiator).unwrap();
        assert!(!c
            .wait_attached(std::time::Duration::from_millis(100))
            .unwrap());
        assert!(c.wait_attached(std::time::Duration::from_secs(10)).unwrap());
        c.close().unwrap();
        drop(c);
        server.join().unwrap();
    }

    #[test]
    fn full_send_buffer_completes_without_blocking_mode() {
        // A reader that drains slowly: the writer hits WouldBlock and must
        // finish via the retry path (the socket stays non-blocking).
        let (addr, server) = fake_relay(false);
        let mut c = RelayTunnelCarrier::connect(addr, 8, TunnelRole::Initiator).unwrap();
        let big = vec![0x5Au8; 60 * 1024];
        for _ in 0..64 {
            c.send(&big).unwrap();
            // Drain echoes so the fake relay's writes never back up.
            let _ = c.poll_recv(&mut |_| {});
        }
        assert_eq!(c.stats().sent_datagrams, 64);
        c.close().unwrap();
        drop(c);
        server.join().unwrap();
    }

    /// A full send buffer queues instead of blocking: sends return at once
    /// (backlog visible via `pending_send`), polls drain it, and the peer
    /// gets every byte in order.
    #[test]
    fn full_send_buffer_queues_and_drains_in_order() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let reader = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut connect = [0u8; 6];
            stream.read_exact(&mut connect).unwrap();
            let mut out = vec![b'O', b'A'];
            out.extend_from_slice(&connect[1..5]);
            out.extend_from_slice(&[0u8; 6]);
            stream.write_all(&out).unwrap();
            // Not reading yet: the client's send buffer fills up.
            thread::sleep(std::time::Duration::from_millis(400));
            let mut all = Vec::new();
            let _ = stream.read_to_end(&mut all);
            all
        });
        let mut c = RelayTunnelCarrier::connect(addr, 6, TunnelRole::Initiator).unwrap();
        let chunk = 16 * 1024;
        let mut n = 0usize;
        let mut slowest = std::time::Duration::ZERO;
        // Keep sending into the stalled peer until the kernel buffers are
        // full and the carrier starts queueing (well before its cap).
        while c.pending_send() == 0 && n < 4096 {
            let t = Instant::now();
            c.send(&vec![(n % 251) as u8; chunk]).unwrap();
            slowest = slowest.max(t.elapsed());
            n += 1;
        }
        assert!(
            c.pending_send() > 0,
            "a stalled peer must leave bytes queued"
        );
        assert!(c.pending_send() <= TX_SOFT_CAP);
        // No send waited for the stalled reader (it sleeps 400 ms).
        assert!(
            slowest < std::time::Duration::from_millis(200),
            "{slowest:?}"
        );
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while c.pending_send() > 0 && Instant::now() < deadline {
            let _ = c.poll_recv(&mut |_| {});
            thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(c.pending_send(), 0);
        c.close().unwrap();
        drop(c);
        let all = reader.join().unwrap();
        let mut fr = FrameReader::default();
        fr.feed(&all);
        let mut got = Vec::new();
        while let Ok(Some((kind, payload))) = fr.next_ref() {
            if kind == tunnel::DATA {
                got.push(payload.to_vec());
            }
        }
        assert_eq!(got.len(), n);
        for (i, p) in got.iter().enumerate() {
            assert_eq!(
                p,
                &vec![(i % 251) as u8; chunk],
                "datagram {i} intact and in order"
            );
        }
    }

    /// The read waker fires once per drain: data wakes it, no further wake
    /// comes until the owner polled, and dropping it stops the thread.
    #[test]
    fn read_waker_fires_on_data_and_coalesces_until_polled() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let (addr, server) = fake_relay(false);
        let mut c = RelayTunnelCarrier::connect(addr, 11, TunnelRole::Initiator).unwrap();
        let wakes = Arc::new(AtomicUsize::new(0));
        let w = Arc::clone(&wakes);
        assert!(c
            .set_read_waker(Some(Box::new(move || {
                w.fetch_add(1, Ordering::SeqCst);
            })))
            .unwrap());
        let wait_for = |n: usize| {
            let deadline = Instant::now() + std::time::Duration::from_secs(2);
            while wakes.load(Ordering::SeqCst) < n && Instant::now() < deadline {
                thread::sleep(std::time::Duration::from_millis(1));
            }
            wakes.load(Ordering::SeqCst)
        };
        // The `A` line may already be buffered: drain whatever woke us.
        let _ = c.poll_recv(&mut |_| {});
        let base = wakes.load(Ordering::SeqCst);
        // Echoed DATA arrives: one wake.
        c.send(b"x").unwrap();
        assert!(wait_for(base + 1) > base, "data must wake the owner");
        // Not polled yet: more data does not wake again.
        c.send(b"y").unwrap();
        thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(
            wakes.load(Ordering::SeqCst),
            base + 1,
            "wakes coalesce until polled"
        );
        let mut got = Vec::new();
        c.poll_recv(&mut |d| got.push(d.to_vec())).unwrap();
        // Polled: the next arrival wakes again.
        c.send(b"z").unwrap();
        assert!(wait_for(base + 2) >= base + 2);
        c.close().unwrap();
        assert!(!c.has_read_waker());
        drop(c);
        server.join().unwrap();
    }

    #[test]
    fn relay_rejection_surfaces_closed() {
        let (addr, server) = fake_relay(true);
        let err = RelayTunnelCarrier::connect(addr, 5, TunnelRole::Initiator).unwrap_err();
        assert!(matches!(err, TransportError::Closed));
        server.join().unwrap();
    }
}
