//! Relay dialing and handshake tests (in-process relay).

use std::sync::{Arc, Mutex};
use std::time::Instant;

use ovapp::terminal::{Command, TerminalSession};
use ovtransport::InMemoryCarrier;

use super::*;

/// The viewer dials first and the host only parks 2.5 s later — longer
/// than the ~2 s handshake window (the viewer comes in over WebSocket). `dial_relay` must hold on until the
/// relay attaches the host, and only then hand back the carrier.
#[test]
fn viewer_waits_for_a_late_host_to_attach() {
    let relay = ovrelay::Relay::bind("127.0.0.1:0".parse().unwrap()).expect("bind");
    let addr = relay.local_addr();
    std::thread::spawn(move || relay.serve(None));
    let sid = 0x7E57_0002;
    let host = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(2500));
        let mut c =
            RelayTunnelCarrier::connect(addr, sid, TunnelRole::Responder).expect("host connect");
        assert!(c
            .wait_attached(Duration::from_secs(10))
            .expect("host attach"));
        c.send(b"ready").expect("host send");
        // Keep the tunnel up until the viewer read it.
        std::thread::sleep(Duration::from_millis(500));
    });
    let t0 = Instant::now();
    // The viewer dials over WebSocket; the host is on raw TCP.
    let ws = RelayAddr::parse(&format!("ws://{addr}/relay")).unwrap();
    let mut c = dial_relay(&ws, sid, Duration::from_secs(10)).expect("viewer attached");
    assert!(t0.elapsed() >= Duration::from_secs(2));
    let mut got = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while got.is_empty() && Instant::now() < deadline {
        c.poll_recv(&mut |d| got.push(d.to_vec())).unwrap();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(got, vec![b"ready".to_vec()]);
    host.join().unwrap();
}

#[test]
fn viewer_reports_a_missing_host() {
    let mut relay = ovrelay::Relay::bind("127.0.0.1:0".parse().unwrap()).expect("bind");
    relay.set_timeout(Duration::from_millis(200));
    let addr = relay.local_addr();
    std::thread::spawn(move || relay.serve(None));
    let tcp = RelayAddr::parse(&addr.to_string()).unwrap();
    let err = dial_relay(&tcp, 0x7E57_0003, Duration::from_secs(10)).unwrap_err();
    assert!(err.contains("not reachable"), "{err}");
}

/// The host completes its side, then immediately sends a record (e.g.
/// MONITORS_INFO) — so the viewer's single poll delivers HELLO, READY
/// *and* that record. The record must reach the TerminalSession instead
/// of being swallowed by the established handshake (which would also
/// desync every later record's sequence number).
#[test]
fn record_behind_ready_survives_the_handshake() {
    let (ida, idb) = (Identity::generate(), Identity::generate());
    // Pairing-code mode: the server completes on the client's HELLO, so
    // it can send a record right behind READY. (PIN mode adds a CONFIRM
    // flight, after which the client is already established.)
    let code = idb.pairing_code();
    let (mut cc, mut sc) = InMemoryCarrier::pair();
    let mut client = Handshake::new(Role::Client, 1, &ida, Auth::PairingCode(code.clone()));
    let mut server = Handshake::new(Role::Server, 2, &idb, Auth::PairingCode(code));
    for ev in client.take_events() {
        let HandshakeEvent::Sent(d) = ev;
        cc.send(&d).unwrap();
    }
    let _ = sc.poll_recv(&mut |d| {
        let _ = server.feed(d);
    });
    for ev in server.take_events() {
        let HandshakeEvent::Sent(d) = ev;
        sc.send(&d).unwrap();
    }
    assert!(server.is_established());
    let mut host = TerminalSession::new(server.take_session().unwrap(), sc);
    host.send(&Command::Echo(b"first-record".to_vec())).unwrap();

    let backlog = handshake_over(&mut client, &mut cc).expect("handshake");
    assert_eq!(
        backlog.len(),
        1,
        "the trailing record is kept, not fed to the handshake"
    );
    let mut view = TerminalSession::new(client.take_session().unwrap(), cc);
    view.queue_inbound(backlog);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    view.on_cmd(Some(Box::new(move |c: &Command| {
        if let Command::Echo(p) = c {
            sink.lock().unwrap().push(p.clone());
        }
    })));
    host.send(&Command::Echo(b"second".to_vec())).unwrap();
    for _ in 0..5 {
        let _ = view.pump();
    }
    assert_eq!(view.stats().decrypt_errors, 0, "no sequence desync");
    assert_eq!(
        *seen.lock().unwrap(),
        vec![b"first-record".to_vec(), b"second".to_vec()]
    );
}
