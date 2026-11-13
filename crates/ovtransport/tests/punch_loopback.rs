//! Integration test: the full v0.5 punch sequence on loopback (spec "UDP
//! hole punching"). Two UDP sockets on 127.0.0.1 play the two NATs, a live
//! `Relay` provides the default path, both sides exchange PROBE frames
//! through the tunnel, punch each other with the 0xFE schedule, confirm
//! with PUNCH_ACK over the tunnel, and finally run a complete ovcore
//! handshake + one AEAD record over the *direct* UDP path.
//!
//! Every poll uses an explicit deadline and non-blocking polls — no
//! blocking `recv_from` on a live socket (that is how the previous
//! milestone deadlocked).

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use ovcore::handshake::{Auth, Handshake, HandshakeEvent, Role};
use ovcore::identity::Identity;
use ovdiscovery::detect_symmetric;
use ovrelay::Relay;
use ovtransport::{Carrier, RelayTunnelCarrier, TunnelRole, UdpP2pCarrier};
use ovtransport::{HolePunch, PUNCH_BYTE};

const PUNCH_N: u32 = 10;
const PUNCH_INTERVAL: Duration = Duration::from_millis(20); // test schedule
const DEADLINE_SECS: u64 = 10;

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(DEADLINE_SECS)
}

/// Receive datagrams from a socket with a deadline, without blocking past
/// it: non-blocking drain + short sleeps until the deadline expires.
fn recv_until(socket: &UdpSocket, deadline: Instant, want: usize) -> Vec<(Vec<u8>, SocketAddr)> {
    socket.set_nonblocking(true).expect("set nonblocking");
    let mut got = Vec::new();
    while got.len() < want && Instant::now() < deadline {
        let mut buf = [0u8; 1500];
        match socket.recv_from(&mut buf) {
            Ok((n, src)) => got.push((buf[..n].to_vec(), src)),
            Err(_) => std::thread::sleep(Duration::from_millis(2)),
        }
    }
    got
}

/// Drain a handshake's outgoing datagrams into the carrier.
#[allow(dead_code)]
fn flush(hs: &mut Handshake, carrier: &mut RelayTunnelCarrier) {
    for ev in hs.take_events() {
        let HandshakeEvent::Sent(dgram) = ev;
        carrier.send(&dgram).expect("tunnel send");
    }
}

#[allow(dead_code)]
fn pump_until_established(
    client: &mut Handshake,
    server: &mut Handshake,
    cc: &mut RelayTunnelCarrier,
    sc: &mut RelayTunnelCarrier,
) {
    let dl = deadline();
    while !(client.is_established() && server.is_established()) && Instant::now() < dl {
        sc.poll_recv(&mut |d| {
            let _ = server.feed(d);
        })
        .expect("tunnel poll");
        flush(server, sc);
        cc.poll_recv(&mut |d| {
            let _ = client.feed(d);
        })
        .expect("tunnel poll");
        flush(client, cc);
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn probe_punch_ack_then_handshake_over_direct_path() {
    // --- The relay (default path) ---
    let mut relay = Relay::bind("127.0.0.1:0".parse().unwrap()).expect("bind relay");
    relay.set_timeout(Duration::from_secs(30));
    let relay_addr = relay.local_addr();
    std::thread::spawn(move || {
        let _ = relay.serve(None);
    });

    // --- The two "NATs": plain UDP sockets on loopback ---
    let sock_a = UdpSocket::bind("127.0.0.1:0").expect("bind A");
    let sock_b = UdpSocket::bind("127.0.0.1:0").expect("bind B");
    let local_a = sock_a.local_addr().unwrap();
    let local_b = sock_b.local_addr().unwrap();

    // --- Both sides dial the relay ---
    let mut cc = RelayTunnelCarrier::connect(relay_addr, 0x0505_0001, TunnelRole::Initiator)
        .expect("initiator connect");
    let mut sc = RelayTunnelCarrier::connect(relay_addr, 0x0505_0001, TunnelRole::Responder)
        .expect("responder connect");
    let dl = deadline();
    while (!cc.is_attached() || !sc.is_attached()) && Instant::now() < dl {
        let _ = cc.poll_recv(&mut |_| {});
        let _ = sc.poll_recv(&mut |_| {});
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        cc.is_attached() && sc.is_attached(),
        "relay never attached the pair"
    );

    // The relay reports each peer's own observed address.
    let ext_a = cc.peer_external_addr().expect("relay reported A's address");
    let ext_b = sc.peer_external_addr().expect("relay reported B's address");
    assert_eq!(
        ext_a.ip(),
        local_a.ip(),
        "relay-observed address is loopback"
    );

    // --- PROBE exchange: each side announces its local UDP port ---
    HolePunch::plan(&mut cc, local_a.port()).expect("A probe");
    HolePunch::plan(&mut sc, local_b.port()).expect("B probe");
    let dl = deadline();
    let (mut probe_b, mut probe_a) = (None, None);
    while (probe_b.is_none() || probe_a.is_none()) && Instant::now() < dl {
        sc.poll_recv(&mut |_| {}).expect("tunnel poll");
        probe_b = sc.take_probe().or(probe_b);
        cc.poll_recv(&mut |_| {}).expect("tunnel poll");
        probe_a = cc.take_probe().or(probe_a);
        std::thread::sleep(Duration::from_millis(2));
    }
    let probe_b = probe_b.expect("A never saw B's PROBE");
    let probe_a = probe_a.expect("B never saw A's PROBE");
    assert_eq!(probe_b, local_a.port(), "B received A's PROBE");
    assert_eq!(probe_a, local_b.port(), "A received B's PROBE");

    // --- Punch targets: peer's external address + peer's announced port ---
    let target_b = HolePunch::punch_target(ext_b, probe_b); // A punches here
    let target_a = HolePunch::punch_target(ext_a, probe_a); // B punches here

    // Both sides punch simultaneously (10 datagrams each, 20 ms apart).
    let a_thread = std::thread::spawn({
        let sock = sock_a.try_clone().expect("clone A");
        move || HolePunch::punch(&sock, target_b, PUNCH_N, PUNCH_INTERVAL)
    });
    let b_thread = std::thread::spawn({
        let sock = sock_b.try_clone().expect("clone B");
        move || HolePunch::punch(&sock, target_a, PUNCH_N, PUNCH_INTERVAL)
    });
    a_thread.join().unwrap().expect("A punch");
    b_thread.join().unwrap().expect("B punch");

    // --- The 0xFE datagrams arrived on each peer's socket ---
    let dl = deadline();
    let at_b = recv_until(&sock_b, dl, PUNCH_N as usize);
    assert_eq!(at_b.len(), PUNCH_N as usize, "B missed punch datagrams");
    assert!(
        at_b.iter().all(|(d, _)| d == &[PUNCH_BYTE]),
        "wrong punch payload"
    );
    let punch_src_on_b = at_b[0].1;

    let dl = deadline();
    let at_a = recv_until(&sock_a, dl, PUNCH_N as usize);
    assert_eq!(at_a.len(), PUNCH_N as usize, "A missed punch datagrams");
    let punch_src_on_a = at_a[0].1;

    // --- PUNCH_ACK over the tunnel, echoing the peer's local port ---
    HolePunch::send_punch_ack(&mut cc, local_b.port()).expect("A ack");
    HolePunch::send_punch_ack(&mut sc, local_a.port()).expect("B ack");
    let dl = deadline();
    let (mut ack_a, mut ack_b) = (None, None);
    while (ack_a.is_none() || ack_b.is_none()) && Instant::now() < dl {
        sc.poll_recv(&mut |_| {}).expect("tunnel poll");
        ack_b = sc.take_punch_ack().or(ack_b);
        cc.poll_recv(&mut |_| {}).expect("tunnel poll");
        ack_a = cc.take_punch_ack().or(ack_a);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(
        ack_a,
        Some(local_a.port()),
        "A never got PUNCH_ACK echoing its port"
    );
    assert_eq!(
        ack_b,
        Some(local_b.port()),
        "B never got PUNCH_ACK echoing its port"
    );

    // --- Migration: wrap the punched sockets in UdpP2pCarriers ---
    let mut direct_a = UdpP2pCarrier::from_socket(sock_a.try_clone().unwrap(), punch_src_on_b);
    let mut direct_b = UdpP2pCarrier::from_socket(sock_b.try_clone().unwrap(), punch_src_on_a);

    // --- Full ovcore handshake + one AEAD record over the DIRECT path ---
    let client_id = Identity::generate();
    let server_id = Identity::generate();
    let (dev_c, dev_s) = (0x00AA_0501u64, 0x00BB_0502u64);
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

    // The handshake runs purely over the direct UDP carriers now — the
    // session keys are path-agnostic, so the tunnel is not needed.
    let initial_hello = match client.take_events().into_iter().next() {
        Some(HandshakeEvent::Sent(d)) => d,
        _ => unreachable!("client emits HELLO at construction"),
    };
    direct_a.send(&initial_hello).expect("direct send hello");
    for _ in 0..200 {
        if client.is_established() && server.is_established() {
            break;
        }
        let delivered = direct_b
            .poll_recv(&mut |d| {
                let _ = server.feed(d);
            })
            .expect("direct poll");
        if !server.is_established() && delivered == 0 {
            direct_a.send(&initial_hello).expect("udp re-send hello");
        }
        flush_direct(&mut server, &mut direct_b);
        direct_a
            .poll_recv(&mut |d| {
                let _ = client.feed(d);
            })
            .expect("direct poll");
        flush_direct(&mut client, &mut direct_a);
        std::thread::sleep(Duration::from_millis(2));
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
    assert_eq!(client.peer().unwrap().fingerprint, server_id.fingerprint());

    // One AEAD record over the punched path.
    let rec = client
        .session_mut()
        .expect("client session")
        .encrypt_record(0, b"punched!");
    direct_a.send(&rec).expect("direct record send");
    let mut received = Vec::new();
    let dl = deadline();
    while received.is_empty() && Instant::now() < dl {
        direct_b
            .poll_recv(&mut |d| received.push(d.to_vec()))
            .expect("direct poll");
        if received.is_empty() {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    let pt = server
        .session_mut()
        .expect("server session")
        .decrypt_record(0, &received[0])
        .expect("decrypt over punched path");
    assert_eq!(pt, b"punched!");

    // --- Liveness guard: fresh traffic keeps the direct path ---
    assert!(!HolePunch::should_demote(Instant::now()));
    direct_a.close().unwrap();
    direct_b.close().unwrap();
    cc.close().unwrap();
    sc.close().unwrap();
}

fn flush_direct(hs: &mut Handshake, carrier: &mut UdpP2pCarrier) {
    for ev in hs.take_events() {
        let HandshakeEvent::Sent(dgram) = ev;
        carrier.send(&dgram).expect("direct send");
    }
}

#[test]
fn symmetric_nat_stub_with_fake_addresses() {
    let a: SocketAddr = "198.51.100.1:40000".parse().unwrap();
    let b_same: SocketAddr = "198.51.100.1:40000".parse().unwrap();
    let b_diff: SocketAddr = "198.51.100.1:41000".parse().unwrap();
    assert!(!detect_symmetric(a, b_same), "same mapping: not symmetric");
    assert!(detect_symmetric(a, b_diff), "different mapping: symmetric");
}
