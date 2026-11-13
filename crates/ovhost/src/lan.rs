//! LAN sessions without a relay.
//!
//! A viewer on the same network finds an unattended host by mDNS
//! (`<device-id>._openview._udp.local`, see `ovdiscovery::mdns`) and sends
//! its handshake HELLO straight to the **LAN port** (UDP, default
//! [`DEFAULT_LAN_PORT`]; the SRV record carries the bound port) — or dials
//! an attended host's address with its pairing code. The session
//! then runs over a point-to-point UDP carrier
//! ([`ovtransport::UdpP2pCarrier`]) in the reliable-UDP framing from the
//! first packet (fragments <= 1.2 KB, retransmission, in-order delivery —
//! the handshake datagrams included; a viewer that starts with bare
//! datagrams is switched over by the first framed packet the host sends)
//! with the same PIN handshake, session code and
//! direct-path upgrade as a relayed session — the LAN carrier simply plays
//! the relay's part.
//!
//! The LAN port is separate from the direct-path port (4570): that socket
//! belongs to the direct engine of relayed sessions, and the two protocols
//! cannot share one socket. One session at a time, like the relay path:
//! while a session runs the listener is not polled, and datagrams queued
//! meanwhile are discarded before the next wait (a viewer that gave up
//! must not start a stale handshake).

use std::io;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use ovcore::handshake::{LEGACY_TAG_HELLO, TAG_HELLO};
use tracing::{debug, warn};

/// Default UDP port of the LAN session listener (the direct path uses
/// 4570, the relay 4569).
pub(crate) const DEFAULT_LAN_PORT: u16 = ovdiscovery::mdns::DEFAULT_LAN_PORT;
/// How often a lost listener socket is re-bound.
const REBIND_EVERY: Duration = Duration::from_secs(1);

/// A viewer whose HELLO is waiting on the LAN port.
pub(crate) struct LanPending {
    /// The listener socket, handed over to the session (connected to
    /// `peer` by the carrier). The HELLO is still queued in it (only
    /// peeked), so the session carrier reads it through its own
    /// reliable-UDP link like every later datagram.
    pub(crate) socket: UdpSocket,
    /// The viewer's address.
    pub(crate) peer: SocketAddr,
}

/// The LAN session listener: a UDP socket on the LAN port, polled between
/// sessions.
pub(crate) struct LanListener {
    port: u16,
    sock: Option<UdpSocket>,
    last_bind: Instant,
}

impl LanListener {
    /// Bind `0.0.0.0:port` (`0` = any free port; see [`port`](Self::port)).
    pub(crate) fn bind(port: u16) -> io::Result<Self> {
        let sock = bind_socket(port)?;
        let port = sock.local_addr()?.port();
        Ok(Self {
            port,
            sock: Some(sock),
            last_bind: Instant::now(),
        })
    }

    /// The bound UDP port (advertised in the mDNS SRV record).
    pub(crate) fn port(&self) -> u16 {
        self.port
    }

    /// Drop everything queued while the listener was not polled.
    pub(crate) fn discard_stale(&mut self) {
        let Some(sock) = self.socket() else { return };
        let mut buf = [0u8; 2048];
        let mut n = 0usize;
        while sock.recv_from(&mut buf).is_ok() {
            n += 1;
            if n > 10_000 {
                break;
            }
        }
        if n > 0 {
            debug!("lan: discarded {n} stale datagram(s)");
        }
    }

    /// A viewer's HELLO, if one is waiting (non-blocking). It is left in
    /// the socket (peeked); anything ahead of it that is not a handshake
    /// HELLO is dropped.
    pub(crate) fn poll(&mut self) -> Option<LanPending> {
        let sock = self.socket()?;
        let mut buf = vec![0u8; 65_536];
        loop {
            match sock.peek_from(&mut buf) {
                Ok((n, peer)) => {
                    if !is_hello(&buf[..n]) {
                        debug!("lan: ignoring a {n}-byte non-HELLO datagram from {peer}");
                        let _ = sock.recv_from(&mut buf);
                        continue;
                    }
                    debug!("lan: HELLO from {peer}");
                    let socket = self.sock.take()?;
                    return Some(LanPending { socket, peer });
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return None,
                // Windows reports an earlier ICMP port-unreachable on the
                // next receive (WSAECONNRESET): harmless, keep reading.
                Err(e) if e.kind() == io::ErrorKind::ConnectionReset => continue,
                // Bigger than the buffer (Windows WSAEMSGSIZE): not a HELLO.
                Err(_) if sock.recv_from(&mut buf).is_ok() => continue,
                Err(e) => {
                    debug!("lan: receive failed: {e}");
                    return None;
                }
            }
        }
    }

    /// Poll for up to `timeout`.
    pub(crate) fn wait(&mut self, timeout: Duration) -> Option<LanPending> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(p) = self.poll() {
                return Some(p);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The listener socket, re-bound after a session took the previous one.
    fn socket(&mut self) -> Option<&UdpSocket> {
        if self.sock.is_none() && self.last_bind.elapsed() >= REBIND_EVERY {
            self.last_bind = Instant::now();
            match bind_socket(self.port) {
                Ok(s) => self.sock = Some(s),
                Err(e) => warn!("lan: cannot re-bind UDP {}: {e}", self.port),
            }
        }
        self.sock.as_ref()
    }
}

fn bind_socket(port: u16) -> io::Result<UdpSocket> {
    let s = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port))?;
    s.set_nonblocking(true)?;
    Ok(s)
}

/// A v2 handshake HELLO (or a v1 one, which the handshake answers with
/// REJECT(incompatible version)): a bare datagram, or the first packet of a
/// reliable-UDP link (viewers frame their LAN handshake so a lost datagram
/// is retransmitted: DATA, sequence 0, first fragment, HELLO payload).
fn is_hello(d: &[u8]) -> bool {
    if ovtransport::rudp::is_framed(d) {
        const T_DATA: u8 = 1;
        const F_FIRST: u8 = 1;
        let hdr = ovtransport::rudp::DATA_HEADER;
        return d.len() > hdr
            && d[4] == T_DATA
            && d[5] & F_FIRST != 0
            && d[6..10] == [0, 0, 0, 0]
            && d[hdr] == TAG_HELLO;
    }
    matches!(d.first(), Some(&TAG_HELLO) | Some(&LEGACY_TAG_HELLO))
}

#[cfg(test)]
mod tests {

    /// The LAN advert's TXT `v=` default and the handshake must agree, or
    /// every viewer labels this host as "different version".
    #[test]
    fn advertised_txt_version_matches_the_handshake() {
        assert_eq!(
            ovdiscovery::mdns::TXT_PROTOCOL_VERSION,
            ovcore::handshake::PROTOCOL_VERSION
        );
    }
    use super::*;

    #[test]
    fn recognises_raw_and_framed_hellos() {
        assert!(is_hello(&[TAG_HELLO, 0]));
        assert!(is_hello(&[LEGACY_TAG_HELLO]));
        assert!(!is_hello(&[0x12]));
        let mut framed = vec![0xB7, b'R', b'U', b'1', 1, 3, 0, 0, 0, 0, 0, 0, 0, 0];
        framed.push(TAG_HELLO);
        assert!(is_hello(&framed));
        framed[9] = 1; // sequence 1: not the start of a link
        assert!(!is_hello(&framed));
        let ack = [
            0xB7, b'R', b'U', b'1', 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        assert!(!is_hello(&ack));
    }

    #[test]
    fn hands_over_the_socket_on_hello_and_rebinds() {
        let mut l = LanListener::bind(0).expect("bind");
        let port = l.port();
        assert_ne!(port, 0);
        let to = SocketAddr::from(([127, 0, 0, 1], port));
        let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        tx.send_to(b"stale", to).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        l.discard_stale();
        assert!(l.poll().is_none());

        tx.send_to(b"\x99junk", to).unwrap();
        tx.send_to(&[TAG_HELLO, 1, 2, 3], to).unwrap();
        let p = l.wait(Duration::from_secs(2)).expect("hello");
        assert_eq!(p.peer, tx.local_addr().unwrap());
        // The HELLO is still queued for the session carrier.
        let mut b = [0u8; 16];
        let (n, _) = p
            .socket
            .recv_from(&mut b)
            .expect("HELLO left in the socket");
        assert_eq!(&b[..n], &[TAG_HELLO, 1, 2, 3]);
        // The session owns the port now; after it ends the listener
        // re-binds the same port.
        drop(p);
        let deadline = Instant::now() + Duration::from_secs(3);
        while l.sock.is_none() && Instant::now() < deadline {
            let _ = l.poll();
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(l.sock.is_some(), "re-bound");
        assert_eq!(l.sock.as_ref().unwrap().local_addr().unwrap().port(), port);
    }
}
