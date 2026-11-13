//! WebSocket relay mode: a viewer that reaches `ovrelay` over WebSocket (as
//! the iOS app does through a Cloudflare tunnel) pairs with a host on the raw
//! TCP relay protocol, and tunnel traffic flows both ways.
//!
//! The viewer side is a `RelayTunnelCarrier::connect_bridge` carrier whose
//! bytes travel through a minimal hand-rolled WebSocket client, standing in
//! for `URLSessionWebSocketTask`.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use ovrelay::Relay;
use ovtransport::{Carrier, RelayTunnelCarrier, TransportError, TunnelRole};

fn start_relay() -> SocketAddr {
    let relay = Relay::bind("127.0.0.1:0".parse().unwrap()).expect("bind relay");
    let addr = relay.local_addr();
    thread::spawn(move || {
        let _ = relay.serve(None);
    });
    addr
}

/// Open a WebSocket to the relay; returns the stream after the 101.
fn ws_open(addr: SocketAddr) -> TcpStream {
    let mut s = TcpStream::connect(addr).unwrap();
    s.write_all(
        b"GET /relay HTTP/1.1\r\nHost: openview.test\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
    )
    .unwrap();
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        s.read_exact(&mut b).unwrap();
        head.push(b[0]);
    }
    let head = String::from_utf8(head).unwrap();
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    assert!(
        head.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo="),
        "{head}"
    );
    s
}

/// A masked client binary frame.
fn client_frame(payload: &[u8]) -> Vec<u8> {
    let mask = [0x12u8, 0x34, 0x56, 0x78];
    let mut out = vec![0x82];
    let n = payload.len();
    if n < 126 {
        out.push(0x80 | n as u8);
    } else {
        out.push(0x80 | 126);
        out.extend_from_slice(&(n as u16).to_be_bytes());
    }
    out.extend_from_slice(&mask);
    out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    out
}

/// Read server frames forever, forwarding data payloads; `None` on close.
fn pump_server_frames(mut s: TcpStream, tx: mpsc::Sender<Option<Vec<u8>>>) {
    loop {
        let mut h = [0u8; 2];
        if s.read_exact(&mut h).is_err() {
            let _ = tx.send(None);
            return;
        }
        assert_eq!(h[1] & 0x80, 0, "server frames are unmasked");
        let mut len = u64::from(h[1] & 0x7F);
        if len == 126 {
            let mut b = [0u8; 2];
            s.read_exact(&mut b).unwrap();
            len = u64::from(u16::from_be_bytes(b));
        } else if len == 127 {
            let mut b = [0u8; 8];
            s.read_exact(&mut b).unwrap();
            len = u64::from_be_bytes(b);
        }
        let mut payload = vec![0u8; len as usize];
        s.read_exact(&mut payload).unwrap();
        match h[0] & 0x0F {
            0x2 => {
                let _ = tx.send(Some(payload));
            }
            0x8 => {
                let _ = tx.send(None);
                return;
            }
            _ => {}
        }
    }
}

/// Poll `carrier` (feeding WebSocket bytes first) until a datagram arrives.
fn recv_bridge(
    carrier: &mut RelayTunnelCarrier,
    rx: &mpsc::Receiver<Option<Vec<u8>>>,
) -> Result<Vec<u8>, TransportError> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        while let Ok(msg) = rx.try_recv() {
            match msg {
                Some(bytes) => carrier.bridge_feed(&bytes),
                None => carrier.bridge_eof(),
            }
        }
        let mut got = None;
        carrier.poll_recv(&mut |d| got = Some(d.to_vec()))?;
        if let Some(d) = got {
            return Ok(d);
        }
        assert!(Instant::now() < deadline, "bridge carrier timed out");
        thread::sleep(Duration::from_millis(5));
    }
}

fn recv_tcp(carrier: &mut RelayTunnelCarrier) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut got = None;
        carrier.poll_recv(&mut |d| got = Some(d.to_vec())).unwrap();
        if let Some(d) = got {
            return d;
        }
        assert!(Instant::now() < deadline, "tcp carrier timed out");
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn websocket_viewer_pairs_with_tcp_host() {
    let relay = start_relay();
    let ws = ws_open(relay);
    let (tx, rx) = mpsc::channel();
    let reader = ws.try_clone().unwrap();
    thread::spawn(move || pump_server_frames(reader, tx));
    let mut writer = ws.try_clone().unwrap();
    let mut viewer = RelayTunnelCarrier::connect_bridge(
        Box::new(move |bytes: &[u8]| writer.write_all(&client_frame(bytes)).is_ok()),
        0xC0DE,
        TunnelRole::Initiator,
    )
    .unwrap();

    let mut host = RelayTunnelCarrier::connect(relay, 0xC0DE, TunnelRole::Responder).unwrap();

    // viewer -> host, then a reply big enough for a 16-bit WS length.
    viewer.send(b"hello over websocket").unwrap();
    assert_eq!(recv_tcp(&mut host), b"hello over websocket");
    let big = vec![0xABu8; 4000];
    host.send(&big).unwrap();
    assert_eq!(recv_bridge(&mut viewer, &rx).unwrap(), big);
    assert!(viewer.is_attached());
    // A WebSocket peer arrives through a proxy: no punch address for it.
    assert_eq!(host.peer_external_addr(), None);

    // Ping from the TCP host is auto-answered by the bridge carrier.
    host.ping(7).unwrap();
    viewer.send(b"after ping").unwrap();
    assert_eq!(recv_tcp(&mut host), b"after ping");
    let deadline = Instant::now() + Duration::from_secs(5);
    while host.last_pong().is_none() {
        let _ = recv_bridge_once(&mut viewer, &rx);
        let _ = host.poll_recv(&mut |_| {});
        assert!(
            Instant::now() < deadline,
            "no PONG through the WebSocket side"
        );
        thread::sleep(Duration::from_millis(5));
    }

    // Host closes: the viewer sees the tunnel end.
    host.close().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match recv_bridge_once(&mut viewer, &rx) {
            Err(TransportError::Closed) => break,
            _ => assert!(Instant::now() < deadline, "viewer never saw the close"),
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn recv_bridge_once(
    carrier: &mut RelayTunnelCarrier,
    rx: &mpsc::Receiver<Option<Vec<u8>>>,
) -> Result<usize, TransportError> {
    while let Ok(msg) = rx.try_recv() {
        match msg {
            Some(bytes) => carrier.bridge_feed(&bytes),
            None => carrier.bridge_eof(),
        }
    }
    carrier.poll_recv(&mut |_| {})
}

/// Live sessions over the Cloudflare WebSocket died 18-30 s in: show that
/// the relay itself keeps a paired WebSocket session up well past that, with
/// its keepalive PINGs well formed (answered like URLSession does) and data
/// flowing both ways throughout. Slow (40 s): `cargo test -- --ignored`.
#[test]
#[ignore]
fn websocket_session_outlives_30_seconds_with_relay_pings() {
    let relay = start_relay();
    let ws = ws_open(relay);
    let (tx, rx) = mpsc::channel();
    let pings = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let (reader, pong_writer, p2) = (
        ws.try_clone().unwrap(),
        ws.try_clone().unwrap(),
        pings.clone(),
    );
    thread::spawn(move || {
        // pump_server_frames, answering PINGs with masked PONGs.
        let mut s = reader;
        let mut w = pong_writer;
        loop {
            let mut h = [0u8; 2];
            if s.read_exact(&mut h).is_err() {
                let _ = tx.send(None);
                return;
            }
            assert_eq!(h[0] & 0x80, 0x80, "FIN");
            assert_eq!(h[1] & 0x80, 0, "server frames are unmasked");
            let mut len = u64::from(h[1] & 0x7F);
            if len == 126 {
                let mut b = [0u8; 2];
                s.read_exact(&mut b).unwrap();
                len = u64::from(u16::from_be_bytes(b));
            } else if len == 127 {
                let mut b = [0u8; 8];
                s.read_exact(&mut b).unwrap();
                len = u64::from_be_bytes(b);
            }
            let mut payload = vec![0u8; len as usize];
            s.read_exact(&mut payload).unwrap();
            match h[0] & 0x0F {
                0x2 => {
                    let _ = tx.send(Some(payload));
                }
                0x9 => {
                    assert!(payload.len() <= 125);
                    p2.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let mut f = client_frame(&payload);
                    f[0] = 0x8A; // PONG
                    w.write_all(&f).unwrap();
                }
                0x8 => {
                    let _ = tx.send(None);
                    return;
                }
                op => panic!("unexpected opcode {op:#x}"),
            }
        }
    });
    let mut writer = ws.try_clone().unwrap();
    let mut viewer = RelayTunnelCarrier::connect_bridge(
        Box::new(move |bytes: &[u8]| writer.write_all(&client_frame(bytes)).is_ok()),
        0xC0DF,
        TunnelRole::Initiator,
    )
    .unwrap();
    let mut host = RelayTunnelCarrier::connect(relay, 0xC0DF, TunnelRole::Responder).unwrap();
    let start = Instant::now();
    let mut n = 0u32;
    while start.elapsed() < Duration::from_secs(40) {
        n += 1;
        host.send(&vec![(n % 251) as u8; 20_000]).unwrap();
        assert_eq!(
            recv_bridge(&mut viewer, &rx).expect("ws side closed").len(),
            20_000
        );
        viewer.send(&n.to_be_bytes()).unwrap();
        assert_eq!(recv_tcp(&mut host), n.to_be_bytes());
        thread::sleep(Duration::from_millis(if n % 50 < 10 { 400 } else { 20 }));
    }
    assert!(
        pings.load(std::sync::atomic::Ordering::Relaxed) >= 2,
        "relay keepalive PINGs"
    );
}

#[test]
fn plain_http_get_is_a_health_check() {
    let relay = start_relay();
    let mut s = TcpStream::connect(relay).unwrap();
    s.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    assert!(resp.starts_with("HTTP/1.1 200 OK"), "{resp}");
    assert!(resp.ends_with("ovrelay\n"));
}

#[test]
fn raw_tcp_clients_still_pair() {
    let relay = start_relay();
    let h = thread::spawn(move || {
        RelayTunnelCarrier::connect(relay, 77, TunnelRole::Responder).unwrap()
    });
    let mut a = RelayTunnelCarrier::connect(relay, 77, TunnelRole::Initiator).unwrap();
    let mut b = h.join().unwrap();
    a.send(b"tcp").unwrap();
    assert_eq!(recv_tcp(&mut b), b"tcp");
    // Raw TCP peers still learn each other's address for hole punching.
    let deadline = Instant::now() + Duration::from_secs(5);
    while a.peer_external_addr().is_none() {
        let _ = a.poll_recv(&mut |_| {});
        assert!(Instant::now() < deadline, "no peer address for a TCP peer");
        thread::sleep(Duration::from_millis(5));
    }
}
