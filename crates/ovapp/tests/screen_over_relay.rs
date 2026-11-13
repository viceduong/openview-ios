//! Integration test: the remote-display path end-to-end through a live relay —
//! the host streams synthetic screen frames to the view over the relayed AEAD
//! channel; the view reassembles them and we assert **byte-exact** pixels.
//! This is the same live path `ovhost --screen` / `ovview` use.

use std::time::{Duration, Instant};

use ovapp::screen::{synthetic_frame, Frame, PixelFormat};
use ovapp::terminal::TerminalSession;
use ovcore::handshake::{Auth, Handshake, HandshakeEvent, Role};
use ovcore::identity::Identity;
use ovrelay::Relay;
use ovtransport::{Carrier, RelayTunnelCarrier, TunnelRole};

const W: u32 = 256;
const H: u32 = 128; // 256*128*4 = 128 KiB frame -> 16 chunks of 8 KiB

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
fn screen_stream_over_relay_byte_exact() {
    let relay = Relay::bind("127.0.0.1:0".parse().unwrap()).expect("bind relay");
    let relay_addr = relay.local_addr();
    let _handle = std::thread::spawn(move || relay.serve(None).expect("serve"));

    let client_id = Identity::generate();
    let server_id = Identity::generate();
    let (dev_c, dev_s) = (0x00CC_0001u64, 0x00DD_0002u64);
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

    // Host (server) is the screen source; view (client) is the display sink.
    let mut host = TerminalSession::new(server.take_session().expect("session"), sc);
    let mut view = TerminalSession::new(client.take_session().expect("session"), cc);

    // Record which frame seqs the view's on_frame hook observes.
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u64>::new()));
    let seen_hook = seen.clone();
    view.on_frame(Some(Box::new(move |f: &Frame| {
        assert_eq!(f.format, PixelFormat::Bgra);
        assert_eq!(f.width, W);
        assert_eq!(f.height, H);
        assert!(f.is_complete());
        seen_hook.lock().unwrap().push(f.seq);
    })));

    const N: u64 = 3;
    for seq in 0..N {
        let frame = synthetic_frame(seq, W, H);
        host.send_frame(&frame, 8 * 1024).expect("send frame");
    }

    // Drain the view until all frames reassembled (byte-exact check via take_frame).
    let deadline = Instant::now() + Duration::from_secs(6);
    let mut received = Vec::new();
    while received.len() < N as usize && Instant::now() < deadline {
        view.pump().expect("view pump");
        if let Some(f) = view.take_frame() {
            received.push(f);
        } else {
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    assert_eq!(
        received.len(),
        N as usize,
        "all {} frames must arrive over the relay",
        N
    );
    for (idx, got) in received.iter().enumerate() {
        let seq = idx as u64;
        let expect = synthetic_frame(seq, W, H);
        assert_eq!(got.seq, seq, "frames must arrive in order");
        assert_eq!(got.format, PixelFormat::Bgra);
        assert_eq!(got.width, W);
        assert_eq!(got.height, H);
        assert!(
            got.is_complete(),
            "frame {seq} must be geometrically complete"
        );
        assert_eq!(
            got.pixels, expect.pixels,
            "frame {seq} pixels must be byte-exact through the relay"
        );
    }

    // The on_frame hook must have observed the same seqs.
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen, (0..N).collect::<Vec<u64>>(), "on_frame hook seqs");

    let h = host.stats();
    let v = view.stats();
    eprintln!("host stats: {h:?}");
    eprintln!("view stats: {v:?}");
    assert_eq!(h.decrypt_errors, 0, "host must not drop a record");
    assert_eq!(v.decrypt_errors, 0, "view must not drop a record");

    view.close().expect("close view");
    host.close().expect("close host");
}
