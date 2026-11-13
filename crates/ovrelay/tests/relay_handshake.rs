//! Integration test: the full ovcore handshake + AEAD record exchange over
//! the `ovrelay` server, with both endpoints using
//! `ovtransport::RelayTunnelCarrier`. Proves the relay is a transparent byte
//! pipe for the framed datagrams the handshake pump produces, and that the
//! AEAD record layer rides on top of it.

use std::time::{Duration, Instant};

use ovcore::handshake::{Auth, Handshake, HandshakeEvent, Role};
use ovcore::identity::Identity;
use ovrelay::Relay;
use ovtransport::{Carrier, RelayTunnelCarrier, TunnelRole};

/// Drain a handshake's outgoing datagrams into the carrier.
/// Receive exactly one DATA datagram, polling until it arrives.
fn recv_one(carrier: &mut RelayTunnelCarrier) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut got = Vec::new();
        let _ = carrier.poll_recv(&mut |d| got.push(d.to_vec()));
        if let Some(data) = got.into_iter().next() {
            return data;
        }
        assert!(
            Instant::now() < deadline,
            "datagram never traversed the relay"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn flush(hs: &mut Handshake, carrier: &mut RelayTunnelCarrier) {
    for ev in hs.take_events() {
        let HandshakeEvent::Sent(dgram) = ev;
        carrier.send(&dgram).expect("relay send");
    }
}

/// Deliver every datagram currently queued on `src` into `dst_hs`.
fn pump(src: &mut RelayTunnelCarrier, dst_hs: &mut Handshake) -> usize {
    let mut n = 0;
    src.poll_recv(&mut |dgram| {
        if dst_hs.feed(dgram).is_ok() {
            n += 1;
        }
    })
    .expect("relay poll_recv");
    n
}

/// Lock-step the two handshakes through the relay until both are established
/// (or the iteration bound is hit — loopback is lossless, so this converges).
fn pump_until_established(
    client: &mut Handshake,
    server: &mut Handshake,
    cc: &mut RelayTunnelCarrier,
    sc: &mut RelayTunnelCarrier,
) {
    // Time-bounded, not iteration-bounded: the relay's pipe threads may
    // start a moment after the attach lines (a busy loop could give up first).
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if client.is_established() && server.is_established() {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
        pump(sc, server);
        flush(server, sc);
        pump(cc, client);
        flush(client, cc);
    }
}

/// Bind a relay on an ephemeral loopback port and run `serve` on a thread.
fn spawn_relay(timeout: Duration) -> SocketAddrGuard {
    let mut relay = Relay::bind("127.0.0.1:0".parse().unwrap()).expect("bind relay");
    relay.set_timeout(timeout);
    let addr = relay.local_addr();
    let handle = std::thread::spawn(move || relay.serve(None).expect("serve"));
    SocketAddrGuard {
        addr,
        handle: Some(handle),
    }
}

/// Owns the relay thread; joining it requires tearing the sessions down, so
/// the guard only joins on drop after the test closed its carriers.
struct SocketAddrGuard {
    addr: std::net::SocketAddr,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Drop for SocketAddrGuard {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            // serve() only returns on listener error; on drop we just detach.
            // The test process is exiting anyway; nothing to leak but a thread.
            std::mem::forget(h);
        }
    }
}

#[test]
fn handshake_over_relay_then_records_both_ways() {
    let relay = spawn_relay(Duration::from_secs(5));

    let client_id = Identity::generate();
    let server_id = Identity::generate();
    let (dev_c, dev_s) = (0x00AA_0001u64, 0x00BB_0002u64);
    let session_id: u32 = u32::from_be_bytes(
        server_id.pairing_code().as_bytes()[..4]
            .try_into()
            .expect("code >= 4 bytes"),
    );

    // Both ends dial the relay: initiator (client) and responder (server).
    let mut cc = RelayTunnelCarrier::connect(relay.addr, session_id, TunnelRole::Initiator)
        .expect("initiator connect");
    let mut sc = RelayTunnelCarrier::connect(relay.addr, session_id, TunnelRole::Responder)
        .expect("responder connect");

    // The relay matches them and sends `A || sid` to each; pump until both
    // carriers have seen it.
    let deadline = Instant::now() + Duration::from_secs(2);
    while (!cc.is_attached() || !sc.is_attached()) && Instant::now() < deadline {
        let _ = cc.poll_recv(&mut |_| {});
        let _ = sc.poll_recv(&mut |_| {});
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(cc.is_attached(), "initiator never attached");
    assert!(sc.is_attached(), "responder never attached");

    let server_code = server_id.pairing_code();
    let mut client = Handshake::new(
        Role::Client,
        dev_c,
        &client_id,
        Auth::PairingCode(server_code.clone()),
    );
    client.expect_fingerprint(server_id.fingerprint());
    let mut server = Handshake::new(
        Role::Server,
        dev_s,
        &server_id,
        Auth::PairingCode(server_code),
    );
    server.expect_device(dev_c);

    // The client's HELLO is emitted at construction.
    flush(&mut client, &mut cc);
    pump_until_established(&mut client, &mut server, &mut cc, &mut sc);

    assert!(
        client.is_established(),
        "client not established; err={:?}",
        client.failed()
    );
    assert!(
        server.is_established(),
        "server not established; err={:?}",
        server.failed()
    );

    // Identity binding survived the relay.
    assert_eq!(client.peer().unwrap().device_id, dev_s);
    assert_eq!(client.peer().unwrap().fingerprint, server_id.fingerprint());
    assert_eq!(server.peer().unwrap().device_id, dev_c);

    // AEAD records both ways over the relayed tunnel. A record is just bytes
    // to the carrier: send the serialized record as DATA, decrypt on the
    // other side with the peer's session.
    let rec = client
        .session_mut()
        .expect("client session")
        .encrypt_record(0, b"over the relay");
    cc.send(&rec).expect("send record");
    let received = recv_one(&mut sc);
    let pt = server
        .session_mut()
        .expect("server session")
        .decrypt_record(0, &received)
        .expect("server decrypts client record");
    assert_eq!(pt, b"over the relay");

    let rec = server
        .session_mut()
        .expect("server session")
        .encrypt_record(1, b"and back");
    sc.send(&rec).expect("send record");
    let received = recv_one(&mut cc);
    let pt = client
        .session_mut()
        .expect("client session")
        .decrypt_record(1, &received)
        .expect("client decrypts server record");
    assert_eq!(pt, b"and back");

    // Graceful teardown both ways; the relay threads then finish.
    cc.close().unwrap();
    sc.close().unwrap();
}

#[test]
fn relay_timeout_rejects_unmatched_connect() {
    let relay = spawn_relay(Duration::from_millis(150));
    let mut c = RelayTunnelCarrier::connect(relay.addr, 0xCAFE_0001, TunnelRole::Initiator)
        .expect("connect accepted (O comes before the timeout)");
    // No peer ever arrives: after ~150 ms the relay sends `XT || sid` and
    // closes. The carrier surfaces that as Closed from poll_recv.
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut saw_closed = false;
    while Instant::now() < deadline {
        match c.poll_recv(&mut |_| {}) {
            Ok(_) => std::thread::sleep(Duration::from_millis(10)),
            Err(ovtransport::TransportError::Closed) => {
                saw_closed = true;
                break;
            }
            Err(e) => panic!("unexpected error: {e}"),
        }
    }
    assert!(saw_closed, "relay never timed the session out");
}

#[test]
fn ping_pong_roundtrip_through_relay() {
    let relay = spawn_relay(Duration::from_secs(5));
    let mut cc = RelayTunnelCarrier::connect(relay.addr, 0x1234_5678, TunnelRole::Initiator)
        .expect("initiator connect");
    let mut sc = RelayTunnelCarrier::connect(relay.addr, 0x1234_5678, TunnelRole::Responder)
        .expect("responder connect");

    // Initiator pings; the responder's poll auto-answers PONG; the relay
    // forwards it back; the initiator's last_pong() updates.
    cc.ping(0x0DD5_EED5).expect("ping");
    let deadline = Instant::now() + Duration::from_secs(2);
    while cc.last_pong().is_none() && Instant::now() < deadline {
        let _ = cc.poll_recv(&mut |_| {});
        let _ = sc.poll_recv(&mut |_| {});
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        cc.last_pong().is_some(),
        "PONG never made it back through the relay"
    );
    assert!(
        sc.last_pong().is_none(),
        "responder never pinged, must have no pong"
    );

    cc.close().unwrap();
    sc.close().unwrap();
}
