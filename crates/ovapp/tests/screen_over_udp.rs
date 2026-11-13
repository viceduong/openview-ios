//! Integration test: CAPS negotiation of the reliable UDP framing between two real
//! `TerminalSession`s on loopback UDP. Two capable peers switch to the framing
//! (both directions) and stream a large frame intact; a capable host facing an
//! older viewer (no `CAP_RUDP`) keeps sending raw records, as before.

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use ovapp::screen::{synthetic_frame, Frame};
use ovapp::terminal::{TerminalSession, CAP_RUDP, LOCAL_CAPS};
use ovcore::handshake::{Auth, Handshake, HandshakeEvent, Role};
use ovcore::identity::Identity;
use ovtransport::{Carrier, UdpP2pCarrier};

fn flush(hs: &mut Handshake, c: &mut UdpP2pCarrier) {
    for ev in hs.take_events() {
        let HandshakeEvent::Sent(d) = ev;
        c.send(&d).unwrap();
    }
}

/// Handshake over a loopback UDP pair; returns (viewer, host) sessions.
fn sessions() -> (TerminalSession, TerminalSession) {
    let a = UdpSocket::bind("127.0.0.1:0").unwrap();
    let b = UdpSocket::bind("127.0.0.1:0").unwrap();
    let (aa, ba) = (a.local_addr().unwrap(), b.local_addr().unwrap());
    let mut vc = UdpP2pCarrier::from_socket(a, ba);
    let mut hc = UdpP2pCarrier::from_socket(b, aa);
    let (vid, hid) = (Identity::generate(), Identity::generate());
    let code = hid.pairing_code();
    let mut v = Handshake::new(Role::Client, 0x11, &vid, Auth::PairingCode(code.clone()));
    v.expect_fingerprint(hid.fingerprint());
    let mut h = Handshake::new(Role::Server, 0x22, &hid, Auth::PairingCode(code));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !(v.is_established() && h.is_established()) {
        assert!(Instant::now() < deadline, "handshake timed out");
        flush(&mut v, &mut vc);
        flush(&mut h, &mut hc);
        hc.poll_recv(&mut |d| {
            let _ = h.feed(d);
        })
        .unwrap();
        vc.poll_recv(&mut |d| {
            let _ = v.feed(d);
        })
        .unwrap();
        std::thread::sleep(Duration::from_millis(1));
    }
    flush(&mut h, &mut hc);
    (
        TerminalSession::new(v.take_session().unwrap(), vc),
        TerminalSession::new(h.take_session().unwrap(), hc),
    )
}

fn big_frame() -> Frame {
    // Incompressible-ish content so the frame stays large on the wire.
    let mut f = synthetic_frame(3, 640, 400);
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    for p in f.pixels.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *p = x as u8;
    }
    f.seq = 3;
    f
}

/// CAPS both ways, then a 1 MB frame host -> viewer; returns the frame the
/// viewer reassembled.
fn exchange(view: &mut TerminalSession, host: &mut TerminalSession) -> Frame {
    view.announce_caps().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while view.peer_caps().is_none() {
        assert!(Instant::now() < deadline, "CAPS exchange timed out");
        host.pump().unwrap();
        view.pump().unwrap();
        std::thread::sleep(Duration::from_millis(1));
    }
    let f = big_frame();
    host.send_frame(&f, 4096).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        assert!(Instant::now() < deadline, "frame never arrived");
        view.pump().unwrap();
        host.pump().unwrap();
        if let Some(got) = view.take_frame() {
            return got;
        }
        std::thread::sleep(Duration::from_micros(300));
    }
}

#[test]
fn new_peers_negotiate_reliable_udp_and_stream_a_frame() {
    let (mut view, mut host) = sessions();
    assert_ne!(LOCAL_CAPS & CAP_RUDP, 0);
    assert!(
        !host.carrier_reliable() && !view.carrier_reliable(),
        "raw until CAPS"
    );
    let got = exchange(&mut view, &mut host);
    assert!(
        host.carrier_reliable(),
        "host switched on the viewer's CAPS"
    );
    assert!(
        view.carrier_reliable(),
        "viewer switched (host CAPS / first framed datagram)"
    );
    assert_eq!(got.pixels, big_frame().pixels);
}

#[test]
fn new_host_keeps_raw_udp_for_an_old_viewer() {
    let (mut view, mut host) = sessions();
    // An old viewer: never advertises CAP_RUDP (and so never receives the
    // framing).
    view.set_local_caps(view.local_caps() & !CAP_RUDP);
    let got = exchange(&mut view, &mut host);
    assert!(!host.carrier_reliable(), "host stays raw for an old viewer");
    assert!(!view.carrier_reliable());
    assert_eq!(got.pixels, big_frame().pixels);
}
