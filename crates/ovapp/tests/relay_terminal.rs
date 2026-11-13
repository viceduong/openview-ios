//! Integration test: the basic demo path end-to-end through a live relay —
//! exactly what `ovview --code` / `ovhost --listen` do: both ends dial the
//! relay with the pairing code as session id, handshake (client/server),
//! then chat + terminal commands over the relayed carrier.

use std::time::{Duration, Instant};

use ovapp::terminal::{Command, TerminalSession};
use ovcore::handshake::{Auth, Handshake, HandshakeEvent, Role};
use ovcore::identity::Identity;
use ovrelay::Relay;
use ovtransport::{Carrier, RelayTunnelCarrier, TunnelRole};

/// Receive datagrams until one arrives (deadline-bounded).
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
fn terminal_over_relay_end_to_end() {
    let relay = Relay::bind("127.0.0.1:0".parse().unwrap()).expect("bind relay");
    let relay_addr = relay.local_addr();
    // serve() runs until process exit; detach the thread deliberately.
    let _handle = std::thread::spawn(move || relay.serve(None).expect("serve"));

    let client_id = Identity::generate();
    let server_id = Identity::generate();
    let (dev_c, dev_s) = (0x00AA_0005u64, 0x00BB_0006u64);
    let code = server_id.short_code();
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

    // Terminal commands over the relay.
    let mut view = TerminalSession::new(client.take_session().expect("session"), cc);
    let mut host = TerminalSession::new(server.take_session().expect("session"), sc);

    view.send(&Command::Echo(b"via relay".to_vec()))
        .expect("echo send");
    let reply = host
        .wait_for(Duration::from_secs(5), |cmd| match cmd {
            Command::Echo(p) => Some(p.clone()),
            _ => None,
        })
        .expect("host saw echo");
    assert_eq!(reply, b"via relay".to_vec());

    // File transfer through the relay.
    let payload: Vec<u8> = (0..=255u8).cycle().take(50_000).collect();
    view.send_file("relay.bin", &payload, 8192)
        .expect("file send");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut got = None;
    while got.is_none() && Instant::now() < deadline {
        host.pump().expect("host pump");
        got = host.take_file();
        if got.is_none() {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    eprintln!("host stats: {:?}", host.stats());
    eprintln!("view stats: {:?}", view.stats());
    let (name, data) = got.expect("file reassembled over relay");
    assert_eq!(name, "relay.bin");
    assert_eq!(data, payload);

    view.close().expect("close view");
    host.close().expect("close host");
}
