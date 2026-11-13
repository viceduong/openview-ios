//! LAN sessions without a relay.
//!
//! A viewer on the same network finds an unattended host by mDNS
//! (`<device-id>._openview._udp.local`, see `ovdiscovery::mdns`) and sends
//! its handshake HELLO straight to the **LAN port** (UDP, default
//! [`DEFAULT_LAN_PORT`]; the SRV record carries the bound port) — or dials
//! an attended host's address with its pairing code. The session
//! then runs over a point-to-point UDP carrier
//! ([`ovtransport::UdpP2pCarrier`]: raw datagrams until both sides speak
//! the reliable-UDP framing) with the same PIN handshake, session code and
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

/// A viewer's first datagram (its HELLO) on the LAN port.
pub(crate) struct LanPending {
    /// The listener socket, handed over to the session (connected to
    /// `peer` by the carrier).
    pub(crate) socket: UdpSocket,
    /// The viewer's address.
    pub(crate) peer: SocketAddr,
    /// The HELLO, to feed to the server handshake.
    pub(crate) first: Vec<u8>,
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

    /// A viewer's HELLO, if one is waiting (non-blocking). Anything that is
    /// not a handshake HELLO is dropped.
    pub(crate) fn poll(&mut self) -> Option<LanPending> {
        let sock = self.socket()?;
        let mut buf = vec![0u8; 4096];
        loop {
            match sock.recv_from(&mut buf) {
                Ok((n, peer)) => {
                    if !is_hello(&buf[..n]) {
                        debug!("lan: ignoring a {n}-byte non-HELLO datagram from {peer}");
                        continue;
                    }
                    debug!("lan: HELLO from {peer}");
                    let socket = self.sock.take()?;
                    return Some(LanPending {
                        socket,
                        peer,
                        first: buf[..n].to_vec(),
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return None,
                // Windows reports an earlier ICMP port-unreachable on the
                // next receive (WSAECONNRESET): harmless, keep reading.
                Err(e) if e.kind() == io::ErrorKind::ConnectionReset => continue,
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
/// REJECT(incompatible version)).
fn is_hello(d: &[u8]) -> bool {
    matches!(d.first(), Some(&TAG_HELLO) | Some(&LEGACY_TAG_HELLO))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(p.first, vec![TAG_HELLO, 1, 2, 3]);
        assert_eq!(p.peer, tx.local_addr().unwrap());
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
