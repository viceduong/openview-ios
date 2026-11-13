//! UDP hole punching over an established relay tunnel. The relay is the **default path**: the ovcore handshake
//! completes over the relay tunnel, and punching is a pure optimization that
//! upgrades the session to a direct UDP path when the NATs allow it.
//!
//! Flow (each side runs the same steps):
//! 1. [`HolePunch::plan`] — announce our local UDP port to the peer with a
//!    [`tunnel::PROBE`] frame through the relay tunnel.
//! 2. Learn the peer's punch target: the relay-observed external address
//!    ([`RelayTunnelCarrier::peer_external_addr`]) plus the peer's local UDP
//!    port from its PROBE (the port is what we punch to; the address is what
//!    the NAT will map our packets onto).
//! 3. [`punch`] — send N one-byte `0xFE` datagrams to the peer's punch
//!    address at fixed intervals, opening pinholes in both NATs.
//! 4. [`wait_ack`] — poll the relay tunnel for a [`tunnel::PUNCH_ACK`] frame
//!    echoing our local port, proving a punch datagram landed.
//! 5. [`migrate`] — once a punch datagram arrives on our local socket, the
//!    caller wraps it in a [`UdpP2pCarrier`] and switches subsequent records
//!    to it. Session keys are path-agnostic (record layer), so no re-handshake is
//!    needed; "migration" is just using the new carrier.
//! 6. [`should_demote`] — liveness guard: if the direct path has been silent
//!    for [`DEMOTE_AFTER`] while the session is active, the caller falls back
//!    to the relay carrier.
//!
//! Everything here is synchronous and non-blocking except the explicit
//! `interval` pacing in [`punch`] — the caller owns the loop, matching the
//! Carrier layer's design.

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use crate::carrier::{Carrier, TransportError};
use crate::relay::RelayTunnelCarrier;
use crate::tunnel::{PROBE, PUNCH_ACK};

/// The payload byte of every punch datagram (spec: "empty 1-byte `0xFE`").
pub const PUNCH_BYTE: u8 = 0xFE;

/// Default punch schedule: this many datagrams, [`DEFAULT_PUNCH_INTERVAL`]
/// apart.
pub const DEFAULT_PUNCH_COUNT: u32 = 10;
/// Default interval between punch datagrams.
pub const DEFAULT_PUNCH_INTERVAL: Duration = Duration::from_millis(100);

/// Fall back to the relay path when the direct path has been silent this
/// long while the session is active (spec step 4: "within 3 s").
pub const DEMOTE_AFTER: Duration = Duration::from_secs(3);

/// Punch helper bound to one side's relay tunnel carrier.
#[derive(Debug, Default)]
pub struct HolePunch;

impl HolePunch {
    /// Step 1: announce our local UDP punch port to the peer through the
    /// relay tunnel (a [`PROBE`] frame carrying it as 2B u16-BE).
    pub fn plan(relay: &mut RelayTunnelCarrier, local_udp_port: u16) -> Result<(), TransportError> {
        relay.send_frame(PROBE, &local_udp_port.to_be_bytes())
    }

    /// The address to punch datagrams to: the peer's relay-observed external
    /// address with the peer's announced local UDP port.
    pub fn punch_target(peer_external: SocketAddr, peer_local_port: u16) -> SocketAddr {
        SocketAddr::new(peer_external.ip(), peer_local_port)
    }

    /// Step 3b: acknowledge a received punch datagram — send a
    /// [`PUNCH_ACK`] frame echoing `peer_local_port` (the port the punch
    /// arrived on, i.e. the peer's announced local port) through the tunnel.
    pub fn send_punch_ack(
        relay: &mut RelayTunnelCarrier,
        peer_local_port: u16,
    ) -> Result<(), TransportError> {
        relay.send_frame(PUNCH_ACK, &peer_local_port.to_be_bytes())
    }

    /// Step 3: send `n` one-byte `0xFE` datagrams to `peer` at `interval`
    /// spacing (default schedule: [`DEFAULT_PUNCH_COUNT`] /
    /// [`DEFAULT_PUNCH_INTERVAL`]). Returns after the last send — replies
    /// are observed on the socket by the caller, not here.
    pub fn punch(
        socket: &UdpSocket,
        peer: SocketAddr,
        n: u32,
        interval: Duration,
    ) -> Result<(), TransportError> {
        for i in 0..n {
            socket
                .send_to(&[PUNCH_BYTE], peer)
                .map_err(TransportError::SendFailed)?;
            if i + 1 < n {
                std::thread::sleep(interval);
            }
        }
        Ok(())
    }

    /// Step 4: poll the relay tunnel for a [`PUNCH_ACK`] frame until
    /// `deadline`. Returns the echoed local port (the peer's confirmation
    /// that a punch datagram arrived on our announced port), or `None` when
    /// the deadline passed first. Non-blocking polls with a small sleep
    /// between them — no blocking socket calls.
    pub fn wait_ack(relay: &mut RelayTunnelCarrier, deadline: Instant) -> Option<u16> {
        while Instant::now() < deadline {
            // PUNCH_ACK frames are recorded by the carrier's frame pump;
            // poll_recv drains them out of the TCP stream.
            let _ = relay.poll_recv(&mut |_| {});
            if let Some(port) = relay.take_punch_ack() {
                return Some(port);
            }
            if relay.last_punch_ack().is_some() {
                return relay.last_punch_ack();
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        None
    }

    /// Step 6 (liveness guard): `true` when the direct path has been silent
    /// for longer than [`DEMOTE_AFTER`] since `last_rx` — the caller should
    /// fall back to the relay carrier. Deliberately a pure function (no
    /// background thread) so tests stay deterministic.
    pub fn should_demote(last_rx: Instant) -> bool {
        last_rx.elapsed() > DEMOTE_AFTER
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::UdpSocket;

    #[test]
    fn punch_datagrams_arrive_with_fe_byte() {
        let a = UdpSocket::bind("127.0.0.1:0").unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").unwrap();
        let b_addr = b.local_addr().unwrap();
        a.set_nonblocking(true).unwrap();
        HolePunch::punch(&a, b_addr, 3, Duration::from_millis(1)).unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut got = 0;
        while got < 3 && Instant::now() < deadline {
            let mut buf = [0u8; 1];
            if b.recv_from(&mut buf).is_ok() {
                assert_eq!(buf[0], PUNCH_BYTE);
                got += 1;
            } else {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        assert_eq!(got, 3);
    }

    #[test]
    fn punch_target_overrides_port() {
        let ext: SocketAddr = "198.51.100.9:40000".parse().unwrap();
        assert_eq!(
            HolePunch::punch_target(ext, 5555),
            "198.51.100.9:5555".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn should_demote_respects_three_second_grace() {
        // Fresh traffic: keep the direct path.
        assert!(!HolePunch::should_demote(Instant::now()));
        // Simulate a stale last_rx by constructing an Instant in the past
        // (checked_sub within DEMOTE_AFTER is valid).
        let stale = Instant::now()
            .checked_sub(DEMOTE_AFTER + Duration::from_millis(1))
            .expect("3s offset is within Instant range");
        assert!(HolePunch::should_demote(stale));
    }
}
