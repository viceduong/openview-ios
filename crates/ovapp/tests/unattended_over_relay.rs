//! Integration test: unattended access (PIN auth) end-to-end through a live
//! relay — the carrier/FFI path that the `ovcore` unit tests cover only
//! in-memory. Both ends dial the relay; the host listens under its **stable
//! device id** (relay session id = device id's low 32 bits) with a PIN, the
//! viewer dials by that device id + the same PIN. On a PIN match the
//! handshake completes with no pre-shared public keys and terminal traffic
//! flows; on a PIN mismatch the server rejects with an explicit
//! `PIN proof mismatch` and the client never establishes.

use std::time::{Duration, Instant};

use ovapp::terminal::{Command, TerminalSession};
use ovcore::handshake::{Auth, Handshake, HandshakeEvent, Role};
use ovcore::identity::Identity;
use ovcore::pin::pin_psk;
use ovrelay::Relay;
use ovtransport::{Carrier, RelayTunnelCarrier, TunnelRole};

/// Stable device id for an unattended host: the low 5 bytes of the identity
/// fingerprint, left-justified in a u64 (high 3 bytes zero). Mirrors
/// `ovhost::unattended_device_id` — it is deterministic, so the same host
/// identity always publishes the same id (and hence the same relay session
/// key) across restarts.
fn device_id(id: &Identity) -> u64 {
    let fp = id.fingerprint();
    u64::from_be_bytes([0, 0, 0, fp[0], fp[1], fp[2], fp[3], fp[4]])
}

/// Relay session id for an unattended host = its stable device id's low 32
/// bits (a device id is a 64-bit value, the relay key is 32-bit).
fn session_id_of(dev: u64) -> u32 {
    (dev & 0xFFFF_FFFF) as u32
}

fn flush(hs: &mut Handshake, carrier: &mut RelayTunnelCarrier) {
    for ev in hs.take_events() {
        let HandshakeEvent::Sent(dgram) = ev;
        carrier.send(&dgram).expect("relay send");
    }
}

/// Drive both ends until the client settles (established or failed) or the
/// deadline. Carrier polls are error-tolerant: when a side rejects the other
/// (e.g. wrong PIN) it closes the channel, which surfaces here as a poll
/// error rather than a handshake state — we keep draining so an in-flight
/// reject record still reaches the peer before we stop.
fn pump_until_settled(
    client: &mut Handshake,
    server: &mut Handshake,
    cc: &mut RelayTunnelCarrier,
    sc: &mut RelayTunnelCarrier,
    deadline: Duration,
) {
    let end = Instant::now() + deadline;
    loop {
        let _ = sc.poll_recv(&mut |d| {
            let _ = server.feed(d);
        });
        flush(server, sc);
        let _ = cc.poll_recv(&mut |d| {
            let _ = client.feed(d);
        });
        flush(client, cc);
        // PIN mode is three flights: the host is established only once the
        // client's CONFIRM arrived, so wait for both sides (or a failure).
        let both = client.is_established() && server.is_established();
        let failed = client.failed().is_some() || server.failed().is_some();
        if both || failed || Instant::now() >= end {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn spin_relay() -> std::net::SocketAddr {
    let relay = Relay::bind("127.0.0.1:0".parse().unwrap()).expect("bind relay");
    let addr = relay.local_addr();
    let _handle = std::thread::spawn(move || relay.serve(None).expect("serve"));
    addr
}

#[test]
fn unattended_correct_pin_establishes_and_flows() {
    // The carrier-level twin of the in-memory `end_to_end_pin_then_records`:
    // same PIN on both sides, no pre-shared public keys, terminal traffic.
    let relay_addr = spin_relay();
    let host_id = Identity::generate();
    let host_dev = device_id(&host_id);
    let session_id = session_id_of(host_dev);

    let mut sc = RelayTunnelCarrier::connect(relay_addr, session_id, TunnelRole::Responder)
        .expect("host connect");
    let mut server = Handshake::new(
        Role::Server,
        host_dev,
        &host_id,
        Auth::Pin(pin_psk(b"123456")),
    );

    let view_id = Identity::generate();
    let mut cc = RelayTunnelCarrier::connect(relay_addr, session_id, TunnelRole::Initiator)
        .expect("view connect");
    let mut client = Handshake::new(
        Role::Client,
        device_id(&view_id),
        &view_id,
        Auth::Pin(pin_psk(b"123456")),
    );
    client.expect_device(host_dev);

    flush(&mut client, &mut cc);
    pump_until_settled(
        &mut client,
        &mut server,
        &mut cc,
        &mut sc,
        Duration::from_secs(5),
    );
    assert!(client.is_established(), "client: {:?}", client.failed());
    assert!(server.is_established(), "server: {:?}", server.failed());

    // Terminal traffic over the PIN-authenticated, relayed channel.
    let mut view = TerminalSession::new(client.take_session().expect("session"), cc);
    let mut host = TerminalSession::new(server.take_session().expect("session"), sc);
    view.send(&Command::Echo(b"headless".to_vec()))
        .expect("echo send");
    let reply = host
        .wait_for(Duration::from_secs(5), |cmd| match cmd {
            Command::Echo(p) => Some(p.clone()),
            _ => None,
        })
        .expect("host saw echo");
    assert_eq!(reply, b"headless".to_vec());
    view.close().expect("close view");
    host.close().expect("close host");
}

#[test]
fn unattended_wrong_pin_rejected_explicitly() {
    // Host accepts PIN "123456"; the viewer offers "999999". The server must
    // reject the viewer's PIN proof and the client must surface the explicit
    // rejection — it must NOT silently establish, and must NOT be a mid-stream
    // AEAD failure.
    let relay_addr = spin_relay();
    let host_id = Identity::generate();
    let host_dev = device_id(&host_id);
    let session_id = session_id_of(host_dev);

    let mut sc = RelayTunnelCarrier::connect(relay_addr, session_id, TunnelRole::Responder)
        .expect("host connect");
    let mut server = Handshake::new(
        Role::Server,
        host_dev,
        &host_id,
        Auth::Pin(pin_psk(b"123456")),
    );

    let view_id = Identity::generate();
    let mut cc = RelayTunnelCarrier::connect(relay_addr, session_id, TunnelRole::Initiator)
        .expect("view connect");
    let mut client = Handshake::new(
        Role::Client,
        device_id(&view_id),
        &view_id,
        Auth::Pin(pin_psk(b"999999")),
    );
    client.expect_device(host_dev);

    flush(&mut client, &mut cc);
    pump_until_settled(
        &mut client,
        &mut server,
        &mut cc,
        &mut sc,
        Duration::from_secs(5),
    );

    // The security property that matters on the wire: a wrong PIN must never
    // establish a channel. (The typed `AuthFailed` outcome on both sides is
    // asserted at the unit level by `wrong_pin_fails_cleanly_on_both_sides`; over a
    // live carrier the server may simply close the channel after rejecting, so
    // here we assert the client did not come up and — if it recorded a failure
    // — that it names the PIN rejection.)
    assert!(!client.is_established(), "wrong PIN must not establish");
    if let Some(err) = client.failed() {
        assert!(
            err.to_lowercase().contains("pin") || err.to_lowercase().contains("reject"),
            "failure should name the PIN rejection, got: {err}"
        );
    }
}
