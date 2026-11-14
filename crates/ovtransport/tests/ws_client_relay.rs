//! The WebSocket relay client (feature `ws-client`): a host and a viewer
//! both reach a local `ovrelay` through `ws://127.0.0.1:<port>/relay`
//! (`RelayTunnelCarrier::connect_ws`), complete the ovcore handshake, and
//! exchange AEAD records, a large burst, PING/PONG and a close. A hostname
//! TCP relay (`connect_host("localhost", ..)`) is checked too.

#![cfg(feature = "ws-client")]

use std::net::SocketAddr;
use std::thread;
use std::time::{Duration, Instant};

use ovcore::handshake::{Auth, Handshake, HandshakeEvent, Role};
use ovcore::identity::Identity;
use ovrelay::Relay;
use ovtransport::{Carrier, RelayTunnelCarrier, TransportError, TunnelRole, WsEndpoint};

fn start_relay() -> SocketAddr {
    let relay = Relay::bind("127.0.0.1:0".parse().unwrap()).expect("bind relay");
    let addr = relay.local_addr();
    thread::spawn(move || {
        let _ = relay.serve(None);
    });
    addr
}

fn ws_endpoint(relay: SocketAddr) -> WsEndpoint {
    WsEndpoint {
        host: "127.0.0.1".into(),
        port: relay.port(),
        path: "/relay".into(),
        tls: false,
    }
}

fn flush(hs: &mut Handshake, carrier: &mut RelayTunnelCarrier) {
    for ev in hs.take_events() {
        let HandshakeEvent::Sent(d) = ev;
        carrier.send(&d).expect("send handshake datagram");
    }
}

/// Poll until at least one datagram arrives (5 s bound).
fn recv(carrier: &mut RelayTunnelCarrier) -> Result<Vec<Vec<u8>>, TransportError> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut got = Vec::new();
        carrier.poll_recv(&mut |d| got.push(d.to_vec()))?;
        if !got.is_empty() {
            return Ok(got);
        }
        assert!(Instant::now() < deadline, "carrier timed out");
        thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn host_and_viewer_handshake_over_websocket_relay() {
    let relay = start_relay();
    let ep = ws_endpoint(relay);
    const SID: u32 = 0x5EB5_0C01;

    // Host parks first (as ovhost does), then the viewer dials.
    let ep2 = ep.clone();
    let host_thread = thread::spawn(move || {
        let mut c = RelayTunnelCarrier::connect_ws(&ep2, SID, TunnelRole::Responder).unwrap();
        assert!(c.wait_attached(Duration::from_secs(10)).unwrap());
        c
    });
    thread::sleep(Duration::from_millis(50));
    let mut viewer = RelayTunnelCarrier::connect_ws(&ep, SID, TunnelRole::Initiator).unwrap();
    assert!(viewer.wait_attached(Duration::from_secs(10)).unwrap());
    let mut host = host_thread.join().unwrap();
    // Both sides arrived through WebSocket: no punch address.
    assert_eq!(viewer.peer_external_addr(), None);

    // The event-driven wake works on the bridged socket.
    let (wtx, wrx) = std::sync::mpsc::channel();
    assert!(host
        .set_read_waker(Some(Box::new(move || {
            let _ = wtx.send(());
        })))
        .unwrap());

    let host_id = Identity::generate();
    let viewer_id = Identity::generate();
    let code = host_id.short_code();
    let mut hs_v = Handshake::new(
        Role::Client,
        0xAA,
        &viewer_id,
        Auth::PairingCode(code.clone()),
    );
    hs_v.expect_fingerprint(host_id.fingerprint());
    let mut hs_h = Handshake::new(Role::Server, 0xBB, &host_id, Auth::PairingCode(code));
    flush(&mut hs_v, &mut viewer);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !(hs_v.is_established() && hs_h.is_established()) {
        assert!(Instant::now() < deadline, "handshake did not complete");
        host.poll_recv(&mut |d| {
            let _ = hs_h.feed(d);
        })
        .unwrap();
        flush(&mut hs_h, &mut host);
        viewer
            .poll_recv(&mut |d| {
                let _ = hs_v.feed(d);
            })
            .unwrap();
        flush(&mut hs_v, &mut viewer);
        thread::sleep(Duration::from_millis(2));
    }
    assert!(
        wrx.recv_timeout(Duration::from_secs(5)).is_ok(),
        "read waker fired"
    );

    // AEAD records both ways.
    let rec = hs_v
        .session_mut()
        .unwrap()
        .encrypt_record(0, b"viewer over websocket");
    viewer.send(&rec).unwrap();
    let got = recv(&mut host).unwrap();
    let pt = hs_h
        .session_mut()
        .unwrap()
        .decrypt_record(0, &got[0])
        .unwrap();
    assert_eq!(pt, b"viewer over websocket");

    // A burst larger than one WebSocket message (64 KiB chunks) from the
    // host, every datagram intact and in order. Once the path backs up the
    // host carrier queues the tail (up to `TX_SOFT_CAP`) and only pushes it
    // out on its next send/poll, so keep polling the host while the viewer
    // drains, exactly as a real host's loop does. (Polling only the viewer
    // stranded the queued tail on slow CI runners.)
    let mut sent = Vec::new();
    let mut got = Vec::new();
    for i in 0..64u32 {
        let d = vec![(i % 251) as u8; 30_000 + i as usize];
        host.send(&d).unwrap();
        sent.push(d);
        // Drain the viewer between sends: once the path is full, `send`
        // waits for room, and only a reading peer makes room.
        viewer.poll_recv(&mut |d| got.push(d.to_vec())).unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while got.len() < sent.len() {
        assert_eq!(host.poll_recv(&mut |_| {}).unwrap(), 0, "host got data");
        viewer.poll_recv(&mut |d| got.push(d.to_vec())).unwrap();
        assert!(
            Instant::now() < deadline,
            "burst stalled: {}/{} datagrams, {} bytes still queued on the host",
            got.len(),
            sent.len(),
            host.pending_send()
        );
        thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(host.pending_send(), 0);
    assert_eq!(got, sent);

    // PING through both pumps, auto-answered by the far carrier.
    viewer.ping(9).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while viewer.last_pong().is_none() {
        let _ = host.poll_recv(&mut |_| {});
        let _ = viewer.poll_recv(&mut |_| {});
        assert!(Instant::now() < deadline, "no PONG");
        thread::sleep(Duration::from_millis(2));
    }

    // The viewer hangs up: the host sees the tunnel close.
    viewer.close().unwrap();
    drop(viewer);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match host.poll_recv(&mut |_| {}) {
            Err(TransportError::Closed) => break,
            _ => assert!(Instant::now() < deadline, "host never saw the close"),
        }
        thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn hostname_tcp_relay_and_ws_viewer_pair() {
    let relay = start_relay();
    const SID: u32 = 0x5EB5_0C02;
    let host_thread = thread::spawn(move || {
        let mut c =
            RelayTunnelCarrier::connect_host("localhost", relay.port(), SID, TunnelRole::Responder)
                .unwrap();
        assert!(c.wait_attached(Duration::from_secs(10)).unwrap());
        c
    });
    thread::sleep(Duration::from_millis(50));
    let mut viewer =
        RelayTunnelCarrier::connect_ws(&ws_endpoint(relay), SID, TunnelRole::Initiator).unwrap();
    assert!(viewer.wait_attached(Duration::from_secs(10)).unwrap());
    let mut host = host_thread.join().unwrap();
    host.send(b"tcp host").unwrap();
    assert_eq!(recv(&mut viewer).unwrap(), [b"tcp host".to_vec()]);
    viewer.send(b"ws viewer").unwrap();
    assert_eq!(recv(&mut host).unwrap(), [b"ws viewer".to_vec()]);
}

#[test]
fn websocket_errors_are_reported() {
    // Nothing listening on the port: a connect error, not a hang.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let ep = ws_endpoint(closed.local_addr().unwrap());
    drop(closed);
    assert!(RelayTunnelCarrier::connect_ws(&ep, 1, TunnelRole::Initiator).is_err());
    assert!(RelayTunnelCarrier::connect_host(
        "no-such-host.invalid",
        4569,
        1,
        TunnelRole::Initiator
    )
    .is_err());
}
