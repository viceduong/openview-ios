//! Integration test: the full ovcore handshake + AEAD record exchange over
//! two real UDP carriers on 127.0.0.1. Proves the Carrier trait is
//! path-agnostic: the same pump that drove ovcore's in-memory pipe tests
//! drives a real kernel UDP socket pair here.

use std::net::UdpSocket;

use ovcore::handshake::{Auth, Handshake, HandshakeEvent, Role};
use ovcore::identity::Identity;
use ovtransport::{Carrier, UdpP2pCarrier};

/// Drain a handshake's outgoing datagrams into the carrier.
fn flush(hs: &mut Handshake, carrier: &mut UdpP2pCarrier) {
    for ev in hs.take_events() {
        let HandshakeEvent::Sent(dgram) = ev;
        carrier.send(&dgram).expect("udp send");
    }
}

/// Move at most one pump cycle of datagrams from `src` to `dst_hs`.
/// Returns the number of datagrams delivered into the handshake.
fn pump(src: &mut UdpP2pCarrier, dst_hs: &mut Handshake) -> usize {
    let mut n = 0;
    src.poll_recv(&mut |dgram| {
        // feed() accepts arbitrarily chunked input; one datagram per call is
        // the normal case and exercises the same code path.
        if dst_hs.feed(dgram).is_ok() {
            n += 1;
        }
    })
    .expect("udp poll_recv");
    n
}

#[test]
fn handshake_over_udp_then_records_both_ways() {
    // Two real sockets on 127.0.0.1:0; each carrier connects to the other's
    // resolved port.
    let c_sock = UdpSocket::bind("127.0.0.1:0").expect("bind client socket");
    let s_sock = UdpSocket::bind("127.0.0.1:0").expect("bind server socket");
    let c_addr = c_sock.local_addr().unwrap();
    let s_addr = s_sock.local_addr().unwrap();
    let mut client_carrier = UdpP2pCarrier::from_socket(c_sock, s_addr);
    let mut server_carrier = UdpP2pCarrier::from_socket(s_sock, c_addr);
    assert_eq!(client_carrier.local_addr(), c_addr);
    assert_eq!(server_carrier.peer_addr(), c_addr);

    let client_id = Identity::generate();
    let server_id = Identity::generate();
    let (dev_c, dev_s) = (0x00AA_0001u64, 0x00BB_0002u64);

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

    // The client's HELLO is emitted at construction; keep a copy. The raw
    // UDP carrier has no ARQ, so if a datagram is lost the test re-sends the
    // HELLO until the server reacts (a test-level stand-in for reliability).
    let initial_hello = match client.take_events().into_iter().next() {
        Some(HandshakeEvent::Sent(d)) => d,
        _ => unreachable!("client emits HELLO at construction"),
    };
    client_carrier.send(&initial_hello).expect("udp send");

    // Pump until both sides are established (or the bound is hit). Each
    // iteration: deliver client->server, drain server events, then reverse.
    for _ in 0..100 {
        if client.is_established() && server.is_established() {
            break;
        }
        let delivered = pump(&mut server_carrier, &mut server);
        if !server.is_established() && delivered == 0 {
            client_carrier
                .send(&initial_hello)
                .expect("udp re-send hello");
        }
        flush(&mut server, &mut server_carrier);
        pump(&mut client_carrier, &mut client);
        flush(&mut client, &mut client_carrier);
    }
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

    // Identity binding survived the wire: each side sees the pinned peer.
    assert_eq!(client.peer().unwrap().device_id, dev_s);
    assert_eq!(client.peer().unwrap().fingerprint, server_id.fingerprint());
    assert_eq!(server.peer().unwrap().device_id, dev_c);

    // AEAD records both ways over the same carriers.
    let rec = client
        .session_mut()
        .expect("client session")
        .encrypt_record(0, b"over the wire");
    client_carrier.send(&rec).expect("send record");
    let mut received = Vec::new();
    let _ = server_carrier.poll_recv(&mut |d| received.push(d.to_vec()));
    assert_eq!(received.len(), 1);
    let pt = server
        .session_mut()
        .expect("server session")
        .decrypt_record(0, &received[0])
        .expect("server decrypts client record");
    assert_eq!(pt, b"over the wire");

    let rec = server
        .session_mut()
        .expect("server session")
        .encrypt_record(1, b"and back");
    server_carrier.send(&rec).expect("send record");
    let mut received = Vec::new();
    let _ = client_carrier.poll_recv(&mut |d| received.push(d.to_vec()));
    assert_eq!(received.len(), 1);
    let pt = client
        .session_mut()
        .expect("client session")
        .decrypt_record(1, &received[0])
        .expect("client decrypts server record");
    assert_eq!(pt, b"and back");

    // Traffic accounting: handshake = 1 datagram client->server, 2
    // server->client (HELLO + READY); plus one record each way. The HELLO
    // re-send only fires on loss, so on loopback the counts are exact.
    let cs = client_carrier.stats();
    assert_eq!(cs.sent_datagrams, 2, "client: hello + record");
    assert_eq!(
        cs.recv_datagrams, 3,
        "client: server hello + ready + record"
    );
    let ss = server_carrier.stats();
    assert_eq!(ss.sent_datagrams, 3, "server: hello + ready + record");
    assert_eq!(ss.recv_datagrams, 2, "server: client hello + record");

    client_carrier.close().expect("close client carrier");
    server_carrier.close().expect("close server carrier");
    assert!(matches!(
        client_carrier.send(b"x"),
        Err(ovtransport::TransportError::Closed)
    ));
}
