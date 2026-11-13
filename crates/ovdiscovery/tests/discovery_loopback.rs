//! Integration test: mDNS advertise + browse entirely on 127.0.0.1. Each
//! test uses distinct service *instances* (unique fingerprints / pairing
//! codes) and polls the browser with a deadline — no unbounded sleeps.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use ovdiscovery::{browse_loopback, Advertiser};

fn deadline(secs: u64) -> Instant {
    Instant::now() + Duration::from_secs(secs)
}

#[test]
fn advertise_then_browse_finds_peer_with_both_ports() {
    let fp = [0xA5u8; 16]; // unique per test
    let adv = Advertiser::start(
        "127.0.0.1:0".parse().unwrap(),
        fp,
        5101,
        Some(5102),
        "246810",
    )
    .expect("advertiser starts");
    let responder: SocketAddr = adv.local_addr();

    let mut found = Vec::new();
    let dl = deadline(5);
    while found.is_empty() && Instant::now() < dl {
        found = browse_loopback("127.0.0.1:0".parse().unwrap(), &[responder], dl).expect("browse");
    }
    assert_eq!(found.len(), 1, "exactly one peer expected, got {found:?}");
    assert_eq!(found[0].fingerprint, fp);
    assert_eq!(found[0].udp_port, 5101);
    assert_eq!(found[0].tcp_port, Some(5102));

    drop(adv);
}

#[test]
fn browse_times_out_when_nothing_advertises() {
    // A browse against a responder that never answers (nothing bound there —
    // use a fresh advertiser that we immediately stop, or just an unbound
    // port: loopback send to a closed port yields ICMP unreachable, recv
    // just times out) must return an empty list, not hang.
    let dead: SocketAddr = "127.0.0.1:1".parse().unwrap(); // nothing listens
    let started = Instant::now();
    let found =
        browse_loopback("127.0.0.1:0".parse().unwrap(), &[dead], deadline(1)).expect("browse");
    assert!(found.is_empty());
    assert!(started.elapsed() < Duration::from_secs(9));
}

#[test]
fn two_advertisers_dedupe_and_distinguish() {
    let fp_a = [0x11u8; 16];
    let fp_b = [0x22u8; 16];
    let adv_a = Advertiser::start("127.0.0.1:0".parse().unwrap(), fp_a, 5201, None, "111111")
        .expect("advertiser A");
    let adv_b = Advertiser::start(
        "127.0.0.1:0".parse().unwrap(),
        fp_b,
        5301,
        Some(5302),
        "222222",
    )
    .expect("advertiser B");
    let responders = [adv_a.local_addr(), adv_b.local_addr()];

    let mut found = Vec::new();
    let dl = deadline(5);
    while found.len() < 2 && Instant::now() < dl {
        found = browse_loopback("127.0.0.1:0".parse().unwrap(), &responders, dl).expect("browse");
    }
    let mut fps: Vec<[u8; 16]> = found.iter().map(|p| p.fingerprint).collect();
    fps.sort();
    let mut expect = vec![fp_a, fp_b];
    expect.sort();
    assert_eq!(fps, expect, "both peers discovered exactly once");
    assert_eq!(
        found
            .iter()
            .find(|p| p.fingerprint == fp_a)
            .unwrap()
            .tcp_port,
        None
    );
    assert_eq!(
        found
            .iter()
            .find(|p| p.fingerprint == fp_b)
            .unwrap()
            .tcp_port,
        Some(5302)
    );

    drop(adv_a);
    drop(adv_b);
}
