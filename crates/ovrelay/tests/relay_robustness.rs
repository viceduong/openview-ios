//! Relay robustness: teardown delivers EOF to the peer, a re-park during an
//! active session pairs with the next dialer, a parked side's timeout never
//! kills an active session, and oversized WebSocket frames are rejected.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use ovrelay::Relay;
use ovtransport::{
    encode_frame, Carrier, RelayTunnelCarrier, TransportError, TunnelRole, DATA, DISCONNECT,
};

fn start_relay(timeout: Duration) -> SocketAddr {
    let mut relay = Relay::bind("127.0.0.1:0".parse().unwrap()).expect("bind relay");
    relay.set_timeout(timeout);
    let addr = relay.local_addr();
    thread::spawn(move || {
        let _ = relay.serve(None);
    });
    addr
}

/// A raw relay client: CONNECT written, `O` consumed.
fn raw_connect(addr: SocketAddr, sid: u32, initiator: bool) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_nodelay(true).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut line = vec![b'C'];
    line.extend_from_slice(&sid.to_be_bytes());
    line.push(if initiator { 0 } else { 1 });
    s.write_all(&line).unwrap();
    let mut o = [0u8; 1];
    s.read_exact(&mut o).unwrap();
    assert_eq!(o[0], b'O');
    s
}

/// Read the 11-byte `A` line.
fn read_attach(s: &mut TcpStream, sid: u32) {
    let mut a = [0u8; 11];
    s.read_exact(&mut a).unwrap();
    assert_eq!(a[0], b'A');
    assert_eq!(u32::from_be_bytes([a[1], a[2], a[3], a[4]]), sid);
}

/// Read everything until EOF (or the read timeout, which fails the test).
fn read_to_eof(s: &mut TcpStream) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match s.read(&mut buf) {
            Ok(0) => return out,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => return out,
            Err(e) => panic!("expected EOF, got {e} after {} bytes", out.len()),
        }
    }
}

fn recv_data(c: &mut RelayTunnelCarrier) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut got = Vec::new();
        c.poll_recv(&mut |d| got.push(d.to_vec())).expect("poll");
        if let Some(d) = got.into_iter().next() {
            return d;
        }
        assert!(Instant::now() < deadline, "no datagram arrived");
        thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn disconnect_relays_frame_then_eof_to_the_peer() {
    let addr = start_relay(Duration::from_secs(5));
    let sid = 0x0EEF_0001;
    let mut host = raw_connect(addr, sid, false);
    let mut view = raw_connect(addr, sid, true);
    read_attach(&mut host, sid);
    read_attach(&mut view, sid);
    // Data first (it must arrive intact), then a DISCONNECT.
    let mut out = encode_frame(DATA, b"last words").unwrap();
    out.extend(encode_frame(DISCONNECT, &[]).unwrap());
    view.write_all(&out).unwrap();
    let rest = read_to_eof(&mut host);
    let mut want = encode_frame(DATA, b"last words").unwrap();
    want.extend(encode_frame(DISCONNECT, &[]).unwrap());
    assert_eq!(rest, want, "DATA, then the relayed DISCONNECT, then EOF");
    // The sender's own socket is shut down too (no leaked pipe thread).
    assert!(read_to_eof(&mut view).is_empty());
}

#[test]
fn abrupt_close_delivers_disconnect_and_eof_to_the_peer() {
    let addr = start_relay(Duration::from_secs(5));
    let sid = 0x0EEF_0002;
    let mut host = raw_connect(addr, sid, false);
    let mut view = raw_connect(addr, sid, true);
    read_attach(&mut host, sid);
    read_attach(&mut view, sid);
    drop(view); // no DISCONNECT frame: plain FIN
    assert_eq!(
        read_to_eof(&mut host),
        encode_frame(DISCONNECT, &[]).unwrap()
    );
}

#[test]
fn repark_during_active_session_pairs_with_the_next_dialer() {
    let addr = start_relay(Duration::from_secs(5));
    let sid = 0x0EEF_0003;
    let mut h1 = RelayTunnelCarrier::connect(addr, sid, TunnelRole::Responder).unwrap();
    let mut v1 = RelayTunnelCarrier::connect(addr, sid, TunnelRole::Initiator).unwrap();
    v1.send(b"s1").unwrap();
    assert_eq!(recv_data(&mut h1), b"s1");

    // While session 1 is live, the same id parks again and pairs anew.
    let mut h2 = RelayTunnelCarrier::connect(addr, sid, TunnelRole::Responder).unwrap();
    let mut v2 = RelayTunnelCarrier::connect(addr, sid, TunnelRole::Initiator).unwrap();
    v2.send(b"s2").unwrap();
    assert_eq!(recv_data(&mut h2), b"s2");
    h2.send(b"s2-back").unwrap();
    assert_eq!(recv_data(&mut v2), b"s2-back");

    // Session 1 is untouched by session 2's pairing.
    h1.send(b"s1-back").unwrap();
    assert_eq!(recv_data(&mut v1), b"s1-back");

    // Ending session 1 does not disturb session 2 (no stale entry removal).
    v1.close().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match h1.poll_recv(&mut |_| {}) {
            Err(TransportError::Closed) => break,
            Ok(_) => {}
            Err(e) => panic!("unexpected {e}"),
        }
        assert!(Instant::now() < deadline, "h1 never saw the teardown");
        thread::sleep(Duration::from_millis(2));
    }
    v2.send(b"still-up").unwrap();
    assert_eq!(recv_data(&mut h2), b"still-up");
    // And the id is free to park once more.
    let _h3 = RelayTunnelCarrier::connect(addr, sid, TunnelRole::Responder).unwrap();
}

#[test]
fn parked_side_timeout_never_kills_an_active_session() {
    // Park timeout far shorter than the session's life.
    let addr = start_relay(Duration::from_millis(200));
    let sid = 0x0EEF_0004;
    let mut host = RelayTunnelCarrier::connect(addr, sid, TunnelRole::Responder).unwrap();
    let mut view = RelayTunnelCarrier::connect(addr, sid, TunnelRole::Initiator).unwrap();
    thread::sleep(Duration::from_millis(600));
    view.send(b"after-timeout").unwrap();
    assert_eq!(recv_data(&mut host), b"after-timeout");
    host.send(b"reply").unwrap();
    assert_eq!(recv_data(&mut view), b"reply");
}

#[test]
fn unmatched_park_times_out_with_xt() {
    let addr = start_relay(Duration::from_millis(150));
    let sid = 0x0EEF_0005;
    let mut s = raw_connect(addr, sid, false);
    let rest = read_to_eof(&mut s);
    let mut want = b"XT".to_vec();
    want.extend_from_slice(&sid.to_be_bytes());
    assert!(rest.starts_with(&want), "got {rest:?}");
    // The id is free again afterwards.
    let _again = raw_connect(addr, sid, false);
}

#[test]
fn oversized_websocket_frame_is_rejected() {
    let addr = start_relay(Duration::from_secs(5));
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(
        b"GET /relay HTTP/1.1\r\nHost: t\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
    )
    .unwrap();
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        s.read_exact(&mut b).unwrap();
        head.push(b[0]);
    }
    assert!(head.starts_with(b"HTTP/1.1 101"));
    // A masked binary frame header claiming 1 MiB (> the 128 KiB cap).
    let mut hdr = vec![0x82, 0x80 | 127];
    hdr.extend_from_slice(&(1u64 << 20).to_be_bytes());
    hdr.extend_from_slice(&[1, 2, 3, 4]);
    s.write_all(&hdr).unwrap();
    // The relay drops the connection without waiting for the payload.
    let t = Instant::now();
    let mut buf = [0u8; 64];
    let ended = loop {
        match s.read(&mut buf) {
            Ok(0) => break true,
            Ok(_) => continue, // e.g. a CLOSE frame
            Err(e)
                if e.kind() == std::io::ErrorKind::ConnectionReset
                    || e.kind() == std::io::ErrorKind::ConnectionAborted =>
            {
                break true
            }
            Err(_) => break false,
        }
    };
    assert!(ended, "connection must be closed");
    assert!(t.elapsed() < Duration::from_secs(4));
}
