//! Integration test: bidirectional file-id-keyed file transfer over a
//! live relay — the viewer sends a file to the host AND the host sends a file
//! to the viewer, concurrently, through the same established session. This is
//! the transport-level proof for `TerminalSession::send_file_xfer` /
//! `take_xfer_file` (the reassembly itself is the pure `ovapp::file_transfer`
//! module, unit-tested separately).

use std::time::{Duration, Instant};

use ovapp::terminal::TerminalSession;
use ovcore::handshake::{Auth, Handshake, HandshakeEvent, Role};
use ovcore::identity::Identity;
use ovrelay::Relay;
use ovtransport::{Carrier, RelayTunnelCarrier, TunnelRole};

fn flush(hs: &mut Handshake, carrier: &mut RelayTunnelCarrier) {
    for ev in hs.take_events() {
        let HandshakeEvent::Sent(dgram) = ev;
        carrier.send(&dgram).expect("relay send");
    }
}

fn pump_until_established(
    client: &mut Handshake,
    server: &mut Handshake,
    cc: &mut RelayTunnelCarrier,
    sc: &mut RelayTunnelCarrier,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !(client.is_established() && server.is_established()) && Instant::now() < deadline {
        sc.poll_recv(&mut |d| {
            let _ = server.feed(d);
        })
        .expect("relay poll");
        flush(server, sc);
        cc.poll_recv(&mut |d| {
            let _ = client.feed(d);
        })
        .expect("relay poll");
        flush(client, cc);
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn file_transfer_over_relay_both_directions() {
    let relay = Relay::bind("127.0.0.1:0".parse().unwrap()).expect("bind relay");
    let relay_addr = relay.local_addr();
    let _handle = std::thread::spawn(move || relay.serve(None).expect("serve"));

    let client_id = Identity::generate();
    let server_id = Identity::generate();
    let (dev_c, dev_s) = (0x00AA_0007u64, 0x00BB_0008u64);
    let code = server_id.pairing_code();
    let session_id = u32::from_be_bytes(code.as_bytes()[..4].try_into().expect("code >= 4"));

    let mut cc = RelayTunnelCarrier::connect(relay_addr, session_id, TunnelRole::Initiator)
        .expect("view connect");
    let mut sc = RelayTunnelCarrier::connect(relay_addr, session_id, TunnelRole::Responder)
        .expect("host connect");

    let mut client = Handshake::new(
        Role::Client,
        dev_c,
        &client_id,
        Auth::PairingCode(code.clone()),
    );
    client.expect_fingerprint(server_id.fingerprint());
    let mut server = Handshake::new(Role::Server, dev_s, &server_id, Auth::PairingCode(code));
    server.expect_device(dev_c);

    flush(&mut client, &mut cc);
    pump_until_established(&mut client, &mut server, &mut cc, &mut sc);
    assert!(client.is_established(), "client: {:?}", client.failed());
    assert!(server.is_established(), "server: {:?}", server.failed());

    let mut view = TerminalSession::new(client.take_session().expect("session"), cc);
    let mut host = TerminalSession::new(server.take_session().expect("session"), sc);

    // Two distinct payloads: viewer -> host (larger than one chunk) and
    // host -> viewer (a different size, so the streams are distinguishable).
    let v2h: Vec<u8> = (0..=255u8).cycle().take(50_000).collect();
    let h2v: Vec<u8> = b"openview host-to-viewer payload \x00\x01\x02\xff".to_vec();

    view.send_file_xfer("viewer-upload.bin", &v2h, 8192)
        .expect("viewer send");
    host.send_file_xfer("host-download.bin", &h2v, 0)
        .expect("host send");

    // Pump both ends until each has reassembled the peer's file.
    let deadline = Instant::now() + Duration::from_secs(10);
    let (mut got_at_host, mut got_at_view) = (None, None);
    while (got_at_host.is_none() || got_at_view.is_none()) && Instant::now() < deadline {
        host.pump().expect("host pump");
        view.pump().expect("view pump");
        if got_at_host.is_none() {
            got_at_host = host.take_xfer_file();
        }
        if got_at_view.is_none() {
            got_at_view = view.take_xfer_file();
        }
        std::thread::sleep(Duration::from_millis(2));
    }

    let at_host = got_at_host.expect("host received viewer's file");
    assert_eq!(at_host.name, "viewer-upload.bin");
    assert_eq!(at_host.data, v2h);

    let at_view = got_at_view.expect("viewer received host's file");
    assert_eq!(at_view.name, "host-download.bin");
    assert_eq!(at_view.data, h2v);

    // Nothing left in flight on either side.
    assert!(host.take_xfer_file().is_none());
    assert!(view.take_xfer_file().is_none());

    view.close().expect("close view");
    host.close().expect("close host");
}
