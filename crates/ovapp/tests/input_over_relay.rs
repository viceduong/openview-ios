//! Integration test: the remote-input path end-to-end through a live relay —
//! the controller sends POINTER (down → move → up) and KEY (down, up) commands
//! on the command channel; the host's `on_cmd` hook collects them and we
//! assert all five arrive **in order with exact fields**. This is the same
//! wire path `ovview`/the iOS viewer use to drive `ovhost --screen`.

use std::time::{Duration, Instant};

use ovapp::terminal::{Command, PointerKind, TerminalSession};
use ovcore::handshake::{Auth, Handshake, HandshakeEvent, Role};
use ovcore::identity::Identity;
use ovrelay::Relay;
use ovtransport::{Carrier, RelayTunnelCarrier, TunnelRole};

/// Receive datagrams until one arrives (deadline-bounded), feeding `hs`.
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
fn pointer_and_key_over_relay_in_order() {
    let relay = Relay::bind("127.0.0.1:0".parse().unwrap()).expect("bind relay");
    let relay_addr = relay.local_addr();
    let _handle = std::thread::spawn(move || relay.serve(None).expect("serve"));

    let client_id = Identity::generate();
    let server_id = Identity::generate();
    let (dev_c, dev_s) = (0x00EE_0001u64, 0x00FF_0002u64);
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

    // Host (server) receives input; view (client) sends it — the real roles.
    let mut host = TerminalSession::new(server.take_session().expect("session"), sc);
    let mut view = TerminalSession::new(client.take_session().expect("session"), cc);

    // Collect every Pointer/Key the host's on_cmd hook observes.
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Command>::new()));
    let seen_hook = seen.clone();
    host.on_cmd(Some(Box::new(move |cmd: &Command| {
        if matches!(cmd, Command::Pointer { .. } | Command::Key { .. }) {
            seen_hook.lock().unwrap().push(cmd.clone());
        }
    })));

    // The exact event sequence a tap-drag-tap + keypress produces:
    // pointer down, move, up, then backspace down + up.
    let sent = [
        Command::Pointer {
            kind: PointerKind::Down,
            x: 32_768,
            y: 16_384,
        },
        Command::Pointer {
            kind: PointerKind::Move,
            x: 40_000,
            y: 20_000,
        },
        Command::Pointer {
            kind: PointerKind::Up,
            x: 40_000,
            y: 20_000,
        },
        Command::Key {
            vk: 0x08,
            down: true,
        },
        Command::Key {
            vk: 0x08,
            down: false,
        },
    ];
    for cmd in &sent {
        view.send(cmd).expect("send input command");
    }

    // Drain the host until all five arrive (order preserved by the carrier).
    let deadline = Instant::now() + Duration::from_secs(6);
    while seen.lock().unwrap().len() < sent.len() && Instant::now() < deadline {
        host.pump().expect("host pump");
        std::thread::sleep(Duration::from_millis(2));
    }

    let got = seen.lock().unwrap().clone();
    assert_eq!(got.len(), sent.len(), "all five input events must arrive");
    assert_eq!(got, sent, "events must arrive in order with exact fields");

    let h = host.stats();
    let v = view.stats();
    assert_eq!(h.decrypt_errors, 0, "host must not drop a record");
    assert_eq!(v.decrypt_errors, 0, "view must not drop a record");
    assert_eq!(h.decode_errors, 0, "host must decode every command");

    view.close().expect("close view");
    host.close().expect("close host");
}
