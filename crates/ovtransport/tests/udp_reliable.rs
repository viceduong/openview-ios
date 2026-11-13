//! The UDP carrier's reliable framing over real loopback sockets —
//! a whole-frame burst arrives complete and in order, loss/reordering
//! injected by a proxy is recovered, and an old (raw) peer still works.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ovtransport::rudp::{DATA_HEADER, MAX_PAYLOAD};
use ovtransport::{Carrier, UdpP2pCarrier};

fn pair_via(
    a_sock: UdpSocket,
    b_sock: UdpSocket,
    a_peer: SocketAddr,
    b_peer: SocketAddr,
) -> (UdpP2pCarrier, UdpP2pCarrier) {
    (
        UdpP2pCarrier::from_socket(a_sock, a_peer),
        UdpP2pCarrier::from_socket(b_sock, b_peer),
    )
}

fn direct_pair() -> (UdpP2pCarrier, UdpP2pCarrier) {
    let a = UdpSocket::bind("127.0.0.1:0").unwrap();
    let b = UdpSocket::bind("127.0.0.1:0").unwrap();
    let (aa, ba) = (a.local_addr().unwrap(), b.local_addr().unwrap());
    pair_via(a, b, ba, aa)
}

/// A frame's worth of records (what `send_frame` emits: 4 KiB chunks).
fn frame_records(n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| (0..4096 + 29).map(|j| (i * 7 + j) as u8).collect())
        .collect()
}

/// Send `recs` from `a` to `b` back to back, pumping both until `b` holds
/// all of them (or the deadline). Returns what `b` delivered.
fn blast(
    a: &mut UdpP2pCarrier,
    b: &mut UdpP2pCarrier,
    recs: &[Vec<u8>],
    secs: u64,
) -> Vec<Vec<u8>> {
    let mut got = Vec::new();
    for r in recs {
        a.send(r).unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(secs);
    while got.len() < recs.len() && Instant::now() < deadline {
        let _ = b.poll_recv(&mut |d| got.push(d.to_vec()));
        let _ = a.poll_recv(&mut |_| {});
        std::thread::sleep(Duration::from_micros(200));
    }
    got
}

#[test]
fn framed_burst_arrives_complete_and_in_order() {
    let (mut a, mut b) = direct_pair();
    assert!(a.enable_reliable() && b.enable_reliable());
    assert!(a.raw_socket().is_some(), "UDP socket is watchable");
    let (rcv, snd) = b.buffer_sizes();
    eprintln!("socket buffers: rcv {rcv} snd {snd}");
    // 357 x 4.1 KB = one ~1.5 MB full frame — the audit's worst case.
    let recs = frame_records(357);
    let t0 = Instant::now();
    let got = blast(&mut a, &mut b, &recs, 20);
    eprintln!(
        "357 x 4.1 KB burst: {}/{} records in {:?}; sender {:?}",
        got.len(),
        recs.len(),
        t0.elapsed(),
        a.link_stats()
    );
    assert_eq!(got.len(), recs.len());
    assert!(got == recs, "in order, byte-exact");
}

/// A UDP proxy between two ports that drops `loss_pct`% and delays every
/// tenth datagram (reordering), in both directions.
fn lossy_proxy(loss_pct: u64) -> (SocketAddr, SocketAddr, Arc<AtomicBool>, Arc<AtomicU64>) {
    let pa = UdpSocket::bind("127.0.0.1:0").unwrap(); // A talks to this
    let pb = UdpSocket::bind("127.0.0.1:0").unwrap(); // B talks to this
    let (aa, ba) = (pa.local_addr().unwrap(), pb.local_addr().unwrap());
    pa.set_read_timeout(Some(Duration::from_millis(1))).unwrap();
    pb.set_read_timeout(Some(Duration::from_millis(1))).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicU64::new(0));
    let (st, dr) = (Arc::clone(&stop), Arc::clone(&dropped));
    std::thread::spawn(move || {
        let mut rng = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let mut a_addr: Option<SocketAddr> = None;
        let mut b_addr: Option<SocketAddr> = None;
        let mut held: Vec<(Instant, bool, Vec<u8>)> = Vec::new();
        let mut buf = vec![0u8; 65_535];
        let mut n = 0u64;
        while !st.load(Ordering::Relaxed) {
            for (sock, from_a) in [(&pa, true), (&pb, false)] {
                while let Ok((len, from)) = sock.recv_from(&mut buf) {
                    if from_a {
                        a_addr = Some(from);
                    } else {
                        b_addr = Some(from);
                    }
                    n += 1;
                    if next() % 100 < loss_pct {
                        dr.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    let delay = if n.is_multiple_of(10) {
                        Duration::from_millis(3)
                    } else {
                        Duration::ZERO
                    };
                    held.push((Instant::now() + delay, from_a, buf[..len].to_vec()));
                }
            }
            let now = Instant::now();
            held.retain(|(at, from_a, d)| {
                if *at > now {
                    return true;
                }
                if *from_a {
                    if let Some(b) = b_addr {
                        let _ = pb.send_to(d, b);
                    }
                } else if let Some(a) = a_addr {
                    let _ = pa.send_to(d, a);
                }
                false
            });
        }
    });
    (aa, ba, stop, dropped)
}

#[test]
fn loss_and_reordering_are_recovered() {
    let loss = std::env::var("UDP_LOSS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    let (to_proxy_a, to_proxy_b, stop, dropped) = lossy_proxy(loss);
    let t0 = Instant::now();
    let a_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let b_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let (mut a, mut b) = pair_via(a_sock, b_sock, to_proxy_a, to_proxy_b);
    // B must speak first so the proxy learns its address.
    b.send(b"hello").unwrap();
    std::thread::sleep(Duration::from_millis(20));
    a.enable_reliable();
    b.enable_reliable();
    let recs = frame_records(300);
    let got = blast(&mut a, &mut b, &recs, 30);
    stop.store(true, Ordering::Relaxed);
    let st = a.link_stats();
    eprintln!(
        "{loss}% loss + reordering: {}/{} records in {:?}, proxy dropped {}, sender {:?}",
        got.len(),
        recs.len(),
        t0.elapsed(),
        dropped.load(Ordering::Relaxed),
        st
    );
    assert_eq!(got.len(), recs.len());
    assert!(got == recs, "in order, byte-exact despite loss");
    if loss > 0 {
        assert!(st.retransmits > 0);
    }
}

#[test]
fn raw_mode_interoperates_with_an_old_peer_and_switches_on_framed_rx() {
    // "Old peer": a plain socket exchanging bare datagrams.
    let old = UdpSocket::bind("127.0.0.1:0").unwrap();
    let a_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let a_addr = a_sock.local_addr().unwrap();
    old.connect(a_addr).unwrap();
    old.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut a = UdpP2pCarrier::from_socket(a_sock, old.local_addr().unwrap());
    a.send(b"record-1").unwrap();
    let mut buf = [0u8; 64];
    let n = old.recv(&mut buf).unwrap();
    assert_eq!(
        &buf[..n],
        b"record-1",
        "raw mode: the bare record on the wire"
    );
    old.send(b"legacy").unwrap();
    let mut got = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(2);
    while got.is_empty() && Instant::now() < deadline {
        a.poll_recv(&mut |d| got.push(d.to_vec())).unwrap();
    }
    assert_eq!(got, vec![b"legacy".to_vec()]);
    assert!(!a.is_reliable());

    // A new peer that switched (CAPS) sends framed: we switch too.
    let (mut x, mut y) = direct_pair();
    x.enable_reliable();
    x.send(b"framed").unwrap();
    let got = blast(&mut x, &mut y, &[], 0);
    assert!(got.is_empty());
    let mut got = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(2);
    while got.is_empty() && Instant::now() < deadline {
        y.poll_recv(&mut |d| got.push(d.to_vec())).unwrap();
    }
    assert_eq!(got, vec![b"framed".to_vec()]);
    assert!(
        y.is_reliable(),
        "auto-switched on the first framed datagram"
    );
    // And every datagram stays within the MTU budget.
    const { assert!(DATA_HEADER + MAX_PAYLOAD <= 1280 - 48) };
}
