//! Reaching a host: the three dial paths (pairing code through the relay,
//! unattended device id + PIN through the relay, device id + PIN on the LAN
//! over direct UDP without the relay) and the client side of the handshake.
//! Each path ends in [`session::viewer_loop`] once the handshake is
//! established.

use std::net::SocketAddr;
use std::time::Duration;

use ovcommon::relay_addr::RelayAddr;
use ovcommon::status::{self, StatusEvent};
use ovcore::handshake::{Auth, Handshake, HandshakeEvent, Role};
use ovcore::identity::Identity;
use ovtransport::{Carrier, RelayTunnelCarrier, TunnelRole, UdpP2pCarrier, WsEndpoint};
use tracing::{debug, info, warn};

use crate::cli::Target;
use crate::session::{self, SessionOptions};

#[cfg(test)]
mod tests;

/// How long `--id` looks for the host on the LAN (mDNS).
const LAN_RESOLVE: Duration = Duration::from_secs(3);
/// Handshake budget per LAN address.
const LAN_HANDSHAKE: Duration = Duration::from_secs(4);

/// How long a dialing viewer waits for the relay to attach the host: the
/// relay's own park window (30 s) plus slack. A host that is between parks
/// (re-parking after an idle timeout) attaches within this window.
const ATTACH_WAIT: Duration = Duration::from_secs(35);

/// Connect to `target` and run the viewer session until it ends.
pub(crate) fn run(
    target: Target,
    relay: &RelayAddr,
    options: SessionOptions,
) -> Result<(), String> {
    // 1 ms waits (media-channel loss decisions, pacing of our sends).
    ovapp::netwait::high_res_timers();
    crate::window::set_renderer(options.renderer);
    info!("OpenView viewer {} starting", env!("CARGO_PKG_VERSION"));

    let me = Identity::generate();
    debug!("our fingerprint {}", me.fingerprint_pine());

    // Handshake as client. Our device ID is the first 5 bytes of our
    // fingerprint — stable per identity, unique enough for a demo.
    let fp = me.fingerprint();
    let my_dev = u64::from_be_bytes([0, 0, 0, fp[0], fp[1], fp[2], fp[3], fp[4]]);

    match target {
        Target::Relay(code) => run_relay(&me, my_dev, code, relay, options),
        Target::Lan {
            device_id,
            pin,
            addrs,
            relay_fallback,
        } => match lan_connect(&me, my_dev, device_id, &pin, addrs) {
            Ok((carrier, hs, backlog)) => session::viewer_loop(carrier, hs, backlog, options),
            Err(LanError::Unreachable(e)) if relay_fallback => {
                warn!("{e}; dialing through the relay instead");
                run_unattended(&me, my_dev, device_id, &pin, relay, options)
            }
            Err(LanError::Unreachable(e) | LanError::Refused(e)) => Err(e),
        },
        Target::Unattended { device_id, pin } => {
            run_unattended(&me, my_dev, device_id, &pin, relay, options)
        }
    }
}

/// Pairing-code path: dial the relay, tunnel to the host, handshake as client.
fn run_relay(
    me: &Identity,
    my_dev: u64,
    code: String,
    addr: &RelayAddr,
    options: SessionOptions,
) -> Result<(), String> {
    info!("dialing relay {addr} with pairing code {code}");
    status::emit(&StatusEvent::Connecting {
        relay: addr.to_string(),
        target: code.clone(),
    });
    // The relay session id is the code's first 4 bytes (matches ovhost).
    let session_id = u32::from_be_bytes(
        code.as_bytes()[..4]
            .try_into()
            .map_err(|_| "pairing code shorter than 4 bytes".to_owned())?,
    );
    let mut carrier = dial_relay(addr, session_id, ATTACH_WAIT)?;

    let mut hs = Handshake::new(Role::Client, my_dev, me, Auth::PairingCode(code));
    let backlog = handshake_over(&mut hs, &mut carrier)?;
    session::viewer_loop(carrier, hs, backlog, options)
}

/// Why the LAN path did not produce a session.
enum LanError {
    /// Nobody answered (not found by mDNS, or no address replied): the
    /// relay may still reach the host.
    Unreachable(String),
    /// The host answered and refused (wrong PIN, rate limit, version).
    Refused(String),
}

/// LAN path: find the host by device id on the local network (mDNS, unless
/// `addrs` were given), then run the PIN handshake over direct UDP to its
/// LAN port — no relay involved. Addresses are tried in order; a rejected
/// PIN ends the attempt at once. The session then runs over the returned
/// carrier.
fn lan_connect(
    me: &Identity,
    my_dev: u64,
    device_id: u64,
    pin: &str,
    addrs: Vec<SocketAddr>,
) -> Result<LanSession, LanError> {
    let addrs = if addrs.is_empty() {
        info!("looking for device {device_id} on the local network (mDNS)");
        let host = ovdiscovery::mdns::resolve(device_id, LAN_RESOLVE)
            .map_err(|e| LanError::Unreachable(format!("LAN discovery failed: {e}")))?
            .ok_or_else(|| {
                LanError::Unreachable(format!(
                    "device {device_id} was not found on this network; \
                     dial it through the relay instead (--connect {device_id})"
                ))
            })?;
        if !host.is_compatible() {
            warn!(
                "{} advertises protocol version {:?}; the handshake may be refused",
                host.name, host.version
            );
        }
        info!("found {} at {}", host.name, join(&host.addrs));
        host.addrs
    } else {
        addrs
    };
    let mut last = String::from("no address to dial");
    for addr in addrs {
        status::emit(&StatusEvent::Connecting {
            relay: format!("lan {addr}"),
            target: device_id.to_string(),
        });
        match lan_handshake(me, my_dev, device_id, pin, addr) {
            Ok(session) => {
                info!("LAN session with {addr}");
                return Ok(session);
            }
            Err((e, fatal)) => {
                warn!("LAN {addr}: {e}");
                if fatal {
                    return Err(LanError::Refused(e));
                }
                last = format!("LAN {addr}: {e}");
            }
        }
    }
    Err(LanError::Unreachable(last))
}

/// An established LAN handshake: carrier, handshake, post-READY backlog.
type LanSession = (UdpP2pCarrier, Handshake, Vec<Vec<u8>>);

/// One LAN attempt: a UDP carrier to `addr` and the client PIN handshake.
/// The error says whether trying another address is pointless (the host
/// answered and refused: wrong PIN, rate limit, version, wrong device).
fn lan_handshake(
    me: &Identity,
    my_dev: u64,
    device_id: u64,
    pin: &str,
    addr: SocketAddr,
) -> Result<LanSession, (String, bool)> {
    let local = if addr.is_ipv4() {
        SocketAddr::from(([0, 0, 0, 0], 0))
    } else {
        SocketAddr::from(([0u16; 8], 0))
    };
    let mut carrier =
        UdpP2pCarrier::new(local, addr).map_err(|e| (format!("udp socket: {e}"), false))?;
    // Reliable-UDP framing from the first packet: a lost handshake datagram
    // is retransmitted instead of stalling the handshake (the host's LAN
    // port accepts a framed HELLO and frames its side too).
    carrier.enable_reliable();
    let psk = ovcore::pin::pin_psk(pin.as_bytes());
    let mut hs = Handshake::new(Role::Client, my_dev, me, Auth::Pin(psk));
    // Pin the device id: mDNS answers are unauthenticated, so a different
    // host answering for this id must fail the handshake.
    hs.expect_device(device_id);
    match handshake_over_for(&mut hs, &mut carrier, LAN_HANDSHAKE) {
        Ok(backlog) => Ok((carrier, hs, backlog)),
        Err(e) => {
            let fatal = hs.error().is_some_and(|err| {
                matches!(
                    err,
                    ovcore::error::HandshakeError::AuthFailed(_)
                        | ovcore::error::HandshakeError::RateLimited { .. }
                        | ovcore::error::HandshakeError::IncompatibleVersion(_)
                )
            });
            Err((e, fatal))
        }
    }
}

fn join(addrs: &[SocketAddr]) -> String {
    addrs
        .iter()
        .map(SocketAddr::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Unattended path: dial the relay by the host's device id (the relay
/// session id is the low 32 bits, matching ovhost --unattended) and complete
/// the handshake with PIN auth — no pairing, no pre-shared identity.
fn run_unattended(
    me: &Identity,
    my_dev: u64,
    device_id: u64,
    pin: &str,
    addr: &RelayAddr,
    options: SessionOptions,
) -> Result<(), String> {
    let session_id = (device_id & 0xFFFF_FFFF) as u32;
    info!("dialing relay {addr} for device {device_id} (session {session_id:08x}, PIN auth)");
    status::emit(&StatusEvent::Connecting {
        relay: addr.to_string(),
        target: device_id.to_string(),
    });
    let mut carrier = dial_relay(addr, session_id, ATTACH_WAIT)?;

    let psk = ovcore::pin::pin_psk(pin.as_bytes());
    let mut hs = Handshake::new(Role::Client, my_dev, me, Auth::Pin(psk));
    // Dialing by device id: pin the peer's device id so we cannot be steered
    // to a different host that happens to share the relay session id.
    hs.expect_device(device_id);
    let backlog = handshake_over(&mut hs, &mut carrier)?;
    session::viewer_loop(carrier, hs, backlog, options)
}

/// Dial the relay as the INITIATOR and wait (up to `attach_wait`) for the
/// relay's attach confirmation before returning — the handshake timer only
/// starts once the host is actually on the other end.
fn dial_relay(
    addr: &RelayAddr,
    session_id: u32,
    attach_wait: Duration,
) -> Result<RelayTunnelCarrier, String> {
    let mut carrier = dial(addr, session_id, TunnelRole::Initiator)
        .map_err(|e| format!("relay {addr}: connect failed: {e}"))?;
    if !carrier.is_attached() {
        info!("waiting for the host to attach (up to {attach_wait:?})");
    }
    match carrier.wait_attached(attach_wait) {
        Ok(true) => Ok(carrier),
        Ok(false) => Err(format!("host did not attach within {attach_wait:?}")),
        Err(ovtransport::TransportError::Closed) => {
            Err("host not reachable (relay park timed out with no host)".into())
        }
        Err(e) => Err(format!("relay attach failed: {e}")),
    }
}

/// Dial the relay at `addr` — raw TCP (hostnames resolved, every address
/// tried) or WebSocket (`ws://` / `wss://`) — and send CONNECT.
fn dial(
    addr: &RelayAddr,
    session_id: u32,
    role: TunnelRole,
) -> Result<RelayTunnelCarrier, ovtransport::TransportError> {
    match addr {
        RelayAddr::Tcp { host, port } => {
            RelayTunnelCarrier::connect_host(host, *port, session_id, role)
        }
        RelayAddr::WebSocket {
            tls,
            host,
            port,
            path,
            ..
        } => RelayTunnelCarrier::connect_ws(
            &WsEndpoint {
                host: host.clone(),
                port: *port,
                path: path.clone(),
                tls: *tls,
            },
            session_id,
            role,
        ),
    }
}

/// Pump the handshake over a carrier until established (datagram re-send on
/// the client side covers loss; loopback converges on the first pass).
///
/// Returns the datagrams that arrived *after* the handshake completed within
/// the same poll batch (e.g. a record the host sent right behind its READY):
/// they belong to the session and must be handed to it, not fed to the
/// (now established) handshake, which would silently discard them.
fn handshake_over(hs: &mut Handshake, carrier: &mut dyn Carrier) -> Result<Vec<Vec<u8>>, String> {
    handshake_over_for(hs, carrier, Duration::from_secs(3))
}

/// [`handshake_over`] with an explicit time budget.
fn handshake_over_for(
    hs: &mut Handshake,
    carrier: &mut dyn Carrier,
    budget: Duration,
) -> Result<Vec<Vec<u8>>, String> {
    for ev in hs.take_events() {
        let HandshakeEvent::Sent(d) = ev;
        carrier
            .send(&d)
            .map_err(|e| format!("handshake send: {e}"))?;
    }
    let mut backlog = Vec::new();
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
        if hs.is_established() {
            return Ok(backlog);
        }
        if let Some(e) = hs.failed() {
            return Err(format!("handshake failed: {e}"));
        }
        carrier
            .poll_recv(&mut |dgram| {
                if hs.is_established() {
                    backlog.push(dgram.to_vec());
                } else {
                    let _ = hs.feed(dgram);
                }
            })
            .map_err(|e| format!("handshake recv: {e}"))?;
        for ev in hs.take_events() {
            let HandshakeEvent::Sent(d) = ev;
            let _ = carrier.send(&d);
        }
        if hs.is_established() {
            return Ok(backlog);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Err("handshake timed out".into())
}
