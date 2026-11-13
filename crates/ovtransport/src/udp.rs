//! UDP point-to-point carrier — the primary OpenView transport path
//! (direct P2P after hole punching, or LAN via a discovered address).
//!
//! Three measures keep raw UDP from losing most of every frame:
//!
//! - **Socket buffers**: `SO_RCVBUF`/`SO_SNDBUF` are raised to
//!   [`SOCKET_BUFFER`] (the default ~64 KB could not hold one 400 KB burst).
//! - **Datagram size + loss recovery**: records go through
//!   [`crate::rudp::ReliableLink`] once the peer advertised `CAP_RUDP` —
//!   <= 1.2 KB fragments (no IP fragmentation), selective ACKs, fast
//!   retransmit + RTO, a congestion window and a pacing token bucket, and
//!   in-order delivery (the record layer requires it). Before that the link
//!   is in raw mode: byte-identical to plain UDP, for peers without `CAP_RUDP`.
//! - **Event-driven waits**: the socket is non-blocking for life, so it is
//!   exposed via [`Carrier::raw_socket`] and loops block on it instead of
//!   polling every 2 ms.

use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use crate::carrier::{Carrier, CarrierStats, TransportError};
use crate::rudp::ReliableLink;

/// Requested socket send/receive buffer size (the OS may clamp it).
pub const SOCKET_BUFFER: usize = 4 * 1024 * 1024;

/// A carrier over one connected UDP socket.
///
/// The socket is `connect()`ed to the peer so `send`/`recv` (not
/// `_from`/`_to`) suffice and the OS filters foreign packets for us —
/// appropriate for the point-to-point topology (one carrier == one peer).
/// The socket is non-blocking for life; `poll_recv` drains it until
/// `WouldBlock`, so one pump iteration picks up a whole burst.
pub struct UdpP2pCarrier {
    socket: Option<UdpSocket>,
    peer: SocketAddr,
    local: SocketAddr,
    stats: CarrierStats,
    link: ReliableLink,
    buf: Vec<u8>,
}

impl UdpP2pCarrier {
    /// Bind a fresh socket on `local` and connect it to `peer`.
    pub fn new(local: SocketAddr, peer: SocketAddr) -> Result<Self, TransportError> {
        let socket = UdpSocket::bind(local).map_err(TransportError::Io)?;
        socket.connect(peer).map_err(TransportError::Io)?;
        Ok(Self::from_socket(socket, peer))
    }

    /// Bind on an OS-chosen port (`127.0.0.1:0`-style `local`) and connect to `peer`.
    pub fn listen(local: SocketAddr, peer: SocketAddr) -> Result<Self, TransportError> {
        Self::new(local, peer)
    }

    /// Wrap an already-bound socket, connecting it to `peer`. Useful for tests
    /// that need to know both endpoints' ports up front. The socket (and any
    /// clone of it) becomes non-blocking with enlarged buffers.
    pub fn from_socket(socket: UdpSocket, peer: SocketAddr) -> Self {
        // connect() on an already-bound socket just pins the destination;
        // a failure here would leave an unconnected socket behind, so ignore
        // the error and let send() surface it instead.
        let _ = socket.connect(peer);
        let _ = socket.set_nonblocking(true);
        tune_buffers(&socket);
        let local = socket
            .local_addr()
            .unwrap_or_else(|_| "0.0.0.0:0".parse().expect("static fallback addr parses"));
        Self {
            socket: Some(socket),
            peer,
            local,
            stats: CarrierStats::default(),
            link: ReliableLink::new(),
            buf: vec![0u8; 65_535],
        }
    }

    /// The bound local address (resolved even for `:0` binds).
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// The connected peer address.
    pub fn peer_addr(&self) -> SocketAddr {
        self.peer
    }

    /// The reliable link's counters (retransmits, acks, ...).
    pub fn link_stats(&self) -> crate::rudp::LinkStats {
        self.link.stats()
    }

    /// The effective (OS-granted) receive / send buffer sizes.
    pub fn buffer_sizes(&self) -> (usize, usize) {
        match &self.socket {
            Some(s) => {
                let r = socket2::SockRef::from(s);
                (
                    r.recv_buffer_size().unwrap_or(0),
                    r.send_buffer_size().unwrap_or(0),
                )
            }
            None => (0, 0),
        }
    }
}

/// Raise SO_RCVBUF / SO_SNDBUF to [`SOCKET_BUFFER`] (best effort).
fn tune_buffers(socket: &UdpSocket) {
    let r = socket2::SockRef::from(socket);
    let _ = r.set_recv_buffer_size(SOCKET_BUFFER);
    let _ = r.set_send_buffer_size(SOCKET_BUFFER);
}

/// Send one datagram; a refused send (`WouldBlock`) is reported as such so
/// the link keeps the packet queued.
fn send_on(socket: &UdpSocket, d: &[u8]) -> io::Result<()> {
    socket.send(d).map(|_| ())
}

impl Carrier for UdpP2pCarrier {
    fn send(&mut self, datagram: &[u8]) -> Result<usize, TransportError> {
        let socket = self.socket.as_ref().ok_or(TransportError::Closed)?;
        let now = Instant::now();
        self.link
            .send(datagram, now, &mut |d| send_on(socket, d))
            .map_err(TransportError::SendFailed)?;
        self.stats.record_sent(datagram.len());
        Ok(datagram.len())
    }

    fn poll_recv(&mut self, sink: &mut dyn FnMut(&[u8])) -> Result<usize, TransportError> {
        let socket = self.socket.as_ref().ok_or(TransportError::Closed)?;
        let link = &mut self.link;
        let stats = &mut self.stats;
        let buf = &mut self.buf;
        let now = Instant::now();
        let mut delivered = 0usize;
        let mut deliver = |r: &[u8]| {
            stats.record_recv(r.len());
            delivered += 1;
            sink(r);
        };
        // Drain until the OS queue is empty (WouldBlock) or a hard error.
        let result = loop {
            match socket.recv(buf) {
                Ok(n) => {
                    link.on_datagram(&buf[..n], now, &mut deliver, &mut |d| send_on(socket, d))
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break Ok(()),
                Err(e) => break Err(e),
            }
        };
        // ACK, release in-order records, retransmit, send queued data.
        link.end_batch(now, &mut deliver, &mut |d| send_on(socket, d));
        result
            .map(|_| delivered)
            .map_err(TransportError::RecvFailed)
    }

    fn close(&mut self) -> Result<(), TransportError> {
        // Dropping the socket closes it; keep stats readable afterwards.
        self.socket = None;
        Ok(())
    }

    fn is_reliable(&self) -> bool {
        // Lossless + ordered once our framing is on (the peer's too: it
        // switched first or switches on our first framed datagram).
        self.link.is_enabled()
    }

    fn stats(&self) -> &CarrierStats {
        &self.stats
    }

    fn pending_send(&self) -> usize {
        self.link.pending_bytes()
    }

    fn raw_socket(&self) -> Option<crate::carrier::RawSock> {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawSocket;
            self.socket.as_ref().map(|s| s.as_raw_socket())
        }
        #[cfg(not(windows))]
        {
            use std::os::fd::AsRawFd;
            self.socket.as_ref().map(|s| s.as_raw_fd())
        }
    }

    fn reliable_capable(&self) -> bool {
        true
    }

    fn enable_reliable(&mut self) -> bool {
        self.link.enable();
        true
    }

    fn next_deadline(&self) -> Option<Duration> {
        self.link.next_deadline(Instant::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::carrier::Carrier;

    fn udp_pair() -> (UdpP2pCarrier, UdpP2pCarrier) {
        let a_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let b_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let a_addr = a_sock.local_addr().unwrap();
        let b_addr = b_sock.local_addr().unwrap();
        (
            UdpP2pCarrier::from_socket(a_sock, b_addr),
            UdpP2pCarrier::from_socket(b_sock, a_addr),
        )
    }

    #[test]
    fn udp_roundtrip_and_stats() {
        let (mut a, mut b) = udp_pair();
        assert!(!a.is_reliable());
        a.send(b"ping").unwrap();
        let mut got = Vec::new();
        let n = b.poll_recv(&mut |d| got.push(d.to_vec())).unwrap();
        assert_eq!(n, 1);
        assert_eq!(got[0], b"ping");
        b.send(b"pong").unwrap();
        let mut got2 = Vec::new();
        assert_eq!(a.poll_recv(&mut |d| got2.push(d.to_vec())).unwrap(), 1);
        assert_eq!(got2[0], b"pong");

        let sa = a.stats();
        assert_eq!(
            (
                sa.sent_datagrams,
                sa.recv_datagrams,
                sa.bytes_sent,
                sa.bytes_recv
            ),
            (1, 1, 4, 4)
        );
    }

    #[test]
    fn udp_poll_recv_empty_returns_zero() {
        let (mut _a, mut b) = udp_pair();
        let mut calls = 0;
        let n = b.poll_recv(&mut |_| calls += 1).unwrap();
        assert_eq!(n, 0);
        assert_eq!(calls, 0);
    }

    #[test]
    fn udp_close_then_closed_error() {
        let (mut a, mut _b) = udp_pair();
        a.close().unwrap();
        a.close().unwrap(); // idempotent
        assert!(matches!(a.send(b"x"), Err(TransportError::Closed)));
        let mut sink = |_: &[u8]| {};
        assert!(matches!(
            a.poll_recv(&mut sink),
            Err(TransportError::Closed)
        ));
        assert_eq!(a.stats().sent_datagrams, 0);
    }
}
