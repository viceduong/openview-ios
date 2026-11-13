use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use super::link::new_link;
use super::wire::{derive_direct_key, marker, open_probe, probe};
use super::*;
use crate::media::{self, MediaEvent};
use crate::Carrier;
use crate::InMemoryCarrier;

fn pair(key: [u8; 32]) -> (DirectCarrier, DirectCarrier, SocketAddr) {
    let (a, b) = InMemoryCarrier::pair();
    let host = DirectCarrier::new(
        Box::new(a),
        DirectSockets::bind_loopback().unwrap(),
        true,
        key,
    );
    let view = DirectCarrier::new(
        Box::new(b),
        DirectSockets::bind_loopback().unwrap(),
        false,
        key,
    );
    let addr: SocketAddr = format!("127.0.0.1:{}", host.local_port()).parse().unwrap();
    (host, view, addr)
}

fn pump(c: &mut DirectCarrier, got: &mut Vec<Vec<u8>>) {
    c.poll_recv(&mut |d| got.push(d.to_vec())).unwrap();
}

fn until(mut f: impl FnMut() -> bool, secs: u64) -> bool {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    false
}

#[test]
fn probe_roundtrip_and_tamper() {
    let k = [7u8; 32];
    let p = probe(&k, P_REQ, &[1; 16], 42);
    assert_eq!(open_probe(&k, &p), Some((P_REQ, [1; 16], 42)));
    let mut bad = p;
    bad[10] ^= 1;
    assert_eq!(open_probe(&k, &bad), None);
    assert_eq!(open_probe(&[8u8; 32], &p), None);
    assert!(is_marker(&marker(M_SYNC_UP, 5)));
    // A record header (8-byte LE device id, top bytes zero) never is.
    let mut rec = vec![0u8; MARK_LEN];
    rec[..8].copy_from_slice(&0x0000_00AB_CDEF_0102u64.to_le_bytes());
    assert!(!is_marker(&rec));
}

#[test]
fn key_is_symmetric() {
    let a = [1u8; 32];
    let b = [2u8; 32];
    assert_eq!(derive_direct_key(&a, &b), derive_direct_key(&a, &b));
    assert_ne!(derive_direct_key(&a, &b), derive_direct_key(&b, &a));
}

/// Disarmed engines are a transparent wrapper.
#[test]
fn unarmed_is_passthrough() {
    let (mut h, mut v, _) = pair([3; 32]);
    for i in 0..10u8 {
        v.send(&[i; 40]).unwrap();
    }
    let mut got = Vec::new();
    assert!(until(
        || {
            pump(&mut h, &mut got);
            got.len() == 10
        },
        2
    ));
    assert_eq!(got[3], vec![3u8; 40]);
    assert_eq!(h.state(), DirectState::Relay);
}

/// Full upgrade: records before, during and after the switch arrive
/// once each and in order, both directions; then the direct path dies
/// (sockets closed) and the session falls back to the relay without a
/// gap.
#[test]
fn promote_then_demote_keeps_order() {
    let (mut h, mut v, addr) = pair([9; 32]);
    h.arm();
    v.arm();
    let mut hgot = Vec::new();
    let mut vgot = Vec::new();
    let mut sent_v = 0u32;
    let mut sent_h = 0u32;
    let rec = |i: u32, from: u8| -> Vec<u8> {
        let mut r = vec![from; 300 + (i as usize % 3000)];
        r[..4].copy_from_slice(&i.to_be_bytes());
        r
    };
    for _ in 0..50 {
        v.send(&rec(sent_v, 1)).unwrap();
        sent_v += 1;
        h.send(&rec(sent_h, 2)).unwrap();
        sent_h += 1;
    }
    // Unreachable candidate + the real one.
    v.start_probe(&["127.0.0.1:9".parse().unwrap(), addr]);
    assert!(until(
        || {
            pump(&mut h, &mut hgot);
            pump(&mut v, &mut vgot);
            v.send(&rec(sent_v, 1)).unwrap();
            sent_v += 1;
            h.send(&rec(sent_h, 2)).unwrap();
            sent_h += 1;
            v.state() == DirectState::Direct && h.state() == DirectState::Direct
        },
        5
    ));
    for _ in 0..200 {
        v.send(&rec(sent_v, 1)).unwrap();
        sent_v += 1;
        h.send(&rec(sent_h, 2)).unwrap();
        sent_h += 1;
        pump(&mut h, &mut hgot);
        pump(&mut v, &mut vgot);
    }
    assert!(until(
        || {
            pump(&mut h, &mut hgot);
            pump(&mut v, &mut vgot);
            hgot.len() == sent_v as usize && vgot.len() == sent_h as usize
        },
        5
    ));
    assert!(h.direct_stats().records_direct_rx > 0 && v.direct_stats().records_direct_rx > 0);
    assert!(v.direct_peer().is_some());
    // Kill the direct path on the host side: both demote, replay, and
    // later records still arrive in order on the relay.
    h.sockets.v4 = None;
    for _ in 0..20 {
        v.send(&rec(sent_v, 1)).unwrap();
        sent_v += 1;
        h.send(&rec(sent_h, 2)).unwrap();
        sent_h += 1;
    }
    assert!(until(
        || {
            pump(&mut h, &mut hgot);
            pump(&mut v, &mut vgot);
            v.state() != DirectState::Direct
                && h.state() != DirectState::Direct
                && hgot.len() == sent_v as usize
                && vgot.len() == sent_h as usize
        },
        10
    ));
    for (i, r) in hgot.iter().enumerate() {
        assert_eq!(
            u32::from_be_bytes(r[..4].try_into().unwrap()),
            i as u32,
            "host rx order"
        );
    }
    for (i, r) in vgot.iter().enumerate() {
        assert_eq!(
            u32::from_be_bytes(r[..4].try_into().unwrap()),
            i as u32,
            "viewer rx order"
        );
    }
    assert!(h.direct_stats().demotions >= 1 || v.direct_stats().demotions >= 1);
}

#[derive(Default)]
struct MemIo {
    relay: Vec<Vec<u8>>,
    direct: Vec<Vec<u8>>,
}

impl DirectIo for MemIo {
    fn primary_send(&mut self, d: &[u8]) -> Result<(), TransportError> {
        self.relay.push(d.to_vec());
        Ok(())
    }
    fn direct_send(&mut self, _p: u32, d: &[u8]) -> io::Result<()> {
        self.direct.push(d.to_vec());
        Ok(())
    }
}

/// Two engines with media enabled and a v2 link on path 0 (no probing).
fn media_pair(now: Instant) -> (DirectEngine, DirectEngine) {
    let (ck, sk) = ([3u8; 32], [4u8; 32]);
    let mut h = DirectEngine::new(true, [1; 32]);
    let mut v = DirectEngine::new(false, [1; 32]);
    for (e, srv) in [(&mut h, true), (&mut v, false)] {
        e.arm();
        e.enable_media(media::derive_media_keys(&ck, &sk, srv));
        e.link = Some(e.make_link(0, now, true));
        e.on_direct = true;
    }
    (h, v)
}

/// Video units cross a lossy path in order — FEC repairs most
/// losses, NACKs (short RTT) the rest; audio gaps are reported; the
/// controller gets feedback; records still flow reliably next to it.
#[test]
fn media_frames_survive_random_loss() {
    let t0 = Instant::now();
    let (mut h, mut v) = media_pair(t0);
    assert!(h.media_ready() && v.media_ready());
    let (mut hio, mut vio) = (MemIo::default(), MemIo::default());
    // What promotion would have sent on the relay: records 0.. go direct.
    v.on_primary(&marker(M_SYNC_UP, 0), t0, &mut vio, &mut |_| {});
    let mut rng = 0x2545_F491_4F6C_DD1Du64;
    let mut drop = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng % 100 < 3
    };
    let mut now = t0;
    let (mut units, mut lost, mut audio, mut audio_lost, mut recs) = (Vec::new(), 0u32, 0, 0u32, 0);
    for f in 0..150u32 {
        let mut unit = vec![(f % 251) as u8; 2000 + (f as usize * 997) % 30_000];
        unit[..4].copy_from_slice(&f.to_be_bytes());
        assert!(h.send_media_video(&unit, f % 50 == 0, now, &mut hio));
        for _ in 0..3 {
            assert!(h.send_media_audio(&[1, 0, 0, 0, 1, 224, 9], now, &mut hio));
        }
        h.send(&[0xAB; 300], now, &mut hio).unwrap();
        for _ in 0..33 {
            now += Duration::from_millis(1);
            h.poll(now, &mut hio, &mut |_| {});
            for d in std::mem::take(&mut hio.direct) {
                if !drop() {
                    v.on_direct(0, &d, now, &mut vio, &mut |_| recs += 1);
                }
            }
            v.poll(now, &mut vio, &mut |_| recs += 1);
            for d in std::mem::take(&mut vio.direct) {
                if !drop() {
                    h.on_direct(0, &d, now, &mut hio, &mut |_| {});
                }
            }
            for e in v.take_media_events() {
                match e {
                    MediaEvent::Video(u) => {
                        units.push(u32::from_be_bytes(u[..4].try_into().unwrap()))
                    }
                    MediaEvent::VideoLost { frames } => lost += frames,
                    MediaEvent::Audio { lost, .. } => {
                        audio += 1;
                        audio_lost += lost;
                    }
                }
            }
        }
    }
    let hs = h.media_stats().unwrap();
    let vs = v.media_stats().unwrap();
    eprintln!(
        "units {} lost {lost} audio {audio}/{audio_lost} recs {recs}; rx {:?}; tx {:?}",
        units.len(),
        vs.rx,
        hs
    );
    assert!(
        units.windows(2).all(|w| w[0] < w[1]),
        "in order, no duplicates"
    );
    assert!(
        units.len() as u32 + lost >= 149,
        "every frame delivered or reported lost"
    );
    assert!(lost <= 2, "{lost} frames lost at 3% loss");
    assert!(vs.rx.fec_recovered > 10);
    assert!(audio_lost > 0 && audio + audio_lost >= 449);
    assert_eq!(recs, 150, "reliable records next to the media");
    assert!(hs.cc.feedbacks > 50, "feedback reached the sender");
    assert!(hs.parity_tx > 0);
}

/// The viewer's NAT / VPN mapping changes mid-session (live: an iPhone
/// behind a VPN app, relay WebSocket and UDP flow re-established at
/// once): its datagrams now come from a new source port and the host's
/// replies to the old one vanish. The viewer's REBIND moves the host's
/// link; records keep flowing both ways, in order, without the relay.
#[test]
fn nat_rebinding_moves_the_link() {
    let (mut h, mut v, addr) = pair([12; 32]);
    h.arm();
    v.arm();
    v.start_probe(&[addr]);
    let (mut hg, mut vg) = (Vec::new(), Vec::new());
    let (mut sh, mut sv) = (0u32, 0u32);
    let rec = |i: u32| -> Vec<u8> {
        let mut r = vec![7u8; 200];
        r[..4].copy_from_slice(&i.to_be_bytes());
        r
    };
    assert!(until(
        || {
            pump(&mut h, &mut hg);
            pump(&mut v, &mut vg);
            v.state() == DirectState::Direct && h.state() == DirectState::Direct
        },
        5
    ));
    // Warm up: the SYNC markers cross the relay, records flow direct.
    assert!(until(
        || {
            h.send(&rec(sh)).unwrap();
            sh += 1;
            v.send(&rec(sv)).unwrap();
            sv += 1;
            pump(&mut h, &mut hg);
            pump(&mut v, &mut vg);
            hg.len() >= 10 && vg.len() >= 10
        },
        5
    ));
    // Relay gone on both sides, then the viewer's address changes.
    h.primary_dead = true;
    h.engine.primary_lost(Instant::now());
    v.primary_dead = true;
    v.engine.primary_lost(Instant::now());
    v.sockets = DirectSockets::bind_loopback().unwrap();
    assert!(
        until(
            || {
                h.send(&rec(sh)).unwrap();
                sh += 1;
                v.send(&rec(sv)).unwrap();
                sv += 1;
                let _ = h.poll_recv(&mut |d| hg.push(d.to_vec()));
                let _ = v.poll_recv(&mut |d| vg.push(d.to_vec()));
                h.direct_stats().rebinds >= 1 && hg.len() as u32 >= sv.saturating_sub(5) && sv > 20
            },
            8
        ),
        "host {:?} viewer {:?} hg {} sv {} vg {} sh {} ev {:?} {:?}",
        h.direct_stats(),
        v.direct_stats(),
        hg.len(),
        sv,
        vg.len(),
        sh,
        h.take_events(),
        v.take_events()
    );
    assert!(
        until(
            || {
                let _ = h.poll_recv(&mut |d| hg.push(d.to_vec()));
                let _ = v.poll_recv(&mut |d| vg.push(d.to_vec()));
                hg.len() as u32 == sv && vg.len() as u32 == sh
            },
            8
        ),
        "host got {}/{sv}, viewer got {}/{sh}",
        hg.len(),
        vg.len()
    );
    assert_eq!(h.state(), DirectState::Direct);
    assert_eq!(v.state(), DirectState::Direct);
    for (i, r) in vg.iter().enumerate() {
        assert_eq!(
            u32::from_be_bytes(r[..4].try_into().unwrap()),
            i as u32,
            "viewer rx order"
        );
    }
    assert!(h.take_events().iter().any(|e| e.contains("rebinding")));
}

/// Once the relay is gone, a direct path that goes quiet for a few
/// seconds (Wi-Fi power save, roaming) is kept: there is nothing to fall
/// back to. It is given up only after DEAD_AFTER_RELAYLESS.
#[test]
fn relayless_direct_path_survives_a_stall() {
    let t0 = Instant::now();
    let (mut h, _v) = media_pair(t0);
    let mut io = MemIo::default();
    h.poll(t0 + DEAD_AFTER / 2, &mut io, &mut |_| {});
    h.primary_lost(t0 + DEAD_AFTER / 2);
    h.poll(t0 + DEAD_AFTER * 2, &mut io, &mut |_| {});
    assert_eq!(
        h.state(),
        DirectState::Direct,
        "kept through a {:?} stall",
        DEAD_AFTER * 2
    );
    assert!(h.next_deadline(t0 + DEAD_AFTER * 2).is_some());
    h.poll(
        t0 + DEAD_AFTER_RELAYLESS + Duration::from_millis(10),
        &mut io,
        &mut |_| {},
    );
    assert_ne!(
        h.state(),
        DirectState::Direct,
        "given up once silent past the grace"
    );
    assert!(h.take_events().iter().any(|e| e.contains("relay gone")));
}

/// A replayed or forged media datagram is dropped.
#[test]
fn media_replay_and_forgery_are_rejected() {
    let t0 = Instant::now();
    let (mut h, mut v) = media_pair(t0);
    let (mut hio, mut vio) = (MemIo::default(), MemIo::default());
    assert!(h.send_media_audio(&[1, 0, 0, 0, 1, 224, 9], t0, &mut hio));
    let d = hio.direct.pop().unwrap();
    v.on_direct(0, &d, t0, &mut vio, &mut |_| {});
    v.on_direct(0, &d, t0, &mut vio, &mut |_| {});
    let mut forged = d.clone();
    let last = forged.len() - 1;
    forged[last] ^= 1;
    v.on_direct(0, &forged, t0, &mut vio, &mut |_| {});
    let s = v.media_stats().unwrap();
    assert_eq!(s.rx.audio, 1);
    assert_eq!(s.replayed_rx, 1);
    assert_eq!(s.bad_rx, 1);
    // The other direction's key cannot open it either (directional keys).
    let mut hio2 = MemIo::default();
    h.on_direct(0, &d, t0, &mut hio2, &mut |_| {});
    assert_eq!(h.media_stats().unwrap().bad_rx, 1);
}

/// Over real sockets, enabling media before the probe makes the
/// link v2 from the start; video crosses it and records still work.
#[test]
fn media_over_loopback_sockets() {
    let (mut h, mut v, addr) = pair([6; 32]);
    h.arm();
    v.arm();
    let (ck, sk) = ([8u8; 32], [9u8; 32]);
    h.enable_media(media::derive_media_keys(&ck, &sk, true));
    v.enable_media(media::derive_media_keys(&ck, &sk, false));
    v.start_probe(&[addr]);
    let (mut hg, mut vg) = (Vec::new(), Vec::new());
    assert!(until(
        || {
            pump(&mut h, &mut hg);
            pump(&mut v, &mut vg);
            h.send(&[1; 50]).unwrap();
            v.send(&[2; 50]).unwrap();
            h.media_ready() && v.media_ready()
        },
        5
    ));
    let unit: Vec<u8> = (0..50_000u32).map(|i| (i % 253) as u8).collect();
    assert!(h.send_media_video(&unit, true));
    let mut got = Vec::new();
    assert!(until(
        || {
            pump(&mut h, &mut hg);
            pump(&mut v, &mut vg);
            got.extend(v.take_media_events());
            !got.is_empty()
        },
        5
    ));
    assert_eq!(got[0], MediaEvent::Video(unit));
    assert!(h.cc_target().is_some());
    assert!(!hg.is_empty() && !vg.is_empty());
}

/// A probe signed with another session's key is ignored.
#[test]
fn foreign_probe_is_rejected() {
    let (mut h, _v, addr) = pair([4; 32]);
    h.arm();
    let s = UdpSocket::bind("127.0.0.1:0").unwrap();
    s.send_to(&probe(&[5u8; 32], P_REQ, &[0; 16], 0), addr)
        .unwrap();
    s.set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let mut got = Vec::new();
    assert!(until(
        || {
            pump(&mut h, &mut got);
            h.direct_stats().bad_probes == 1
        },
        2
    ));
    let mut b = [0u8; 64];
    assert!(
        s.recv_from(&mut b).is_err(),
        "no response to a forged probe"
    );
}

/// Records sent on the direct path that the peer had not acknowledged
/// are replayed on the relay at demotion (no gap, no duplicate).
#[test]
fn engine_replays_unacked_on_demotion() {
    struct Io {
        relay: Vec<Vec<u8>>,
        direct: Vec<(u32, Vec<u8>)>,
    }
    impl DirectIo for Io {
        fn primary_send(&mut self, d: &[u8]) -> Result<(), TransportError> {
            self.relay.push(d.to_vec());
            Ok(())
        }
        fn direct_send(&mut self, p: u32, d: &[u8]) -> io::Result<()> {
            self.direct.push((p, d.to_vec()));
            Ok(())
        }
    }
    let key = [1u8; 32];
    let now = Instant::now();
    let mut tx = DirectEngine::new(false, key);
    let mut rx = DirectEngine::new(true, key);
    tx.arm();
    rx.arm();
    let mut io_t = Io {
        relay: vec![],
        direct: vec![],
    };
    let mut io_r = Io {
        relay: vec![],
        direct: vec![],
    };
    tx.send(b"r0", now, &mut io_t).unwrap();
    tx.link = Some(new_link(0, now, true));
    tx.promote(now, &mut io_t);
    tx.send(b"d1", now, &mut io_t).unwrap();
    tx.send(b"d2", now, &mut io_t).unwrap();
    // The direct datagrams are lost; the path dies.
    tx.demote(now, &mut io_t, "test");
    let mut got: Vec<Vec<u8>> = Vec::new();
    for d in io_t.relay.drain(..) {
        rx.on_primary(&d, now, &mut io_r, &mut |r| got.push(r.to_vec()));
    }
    assert_eq!(got, vec![b"r0".to_vec(), b"d1".to_vec(), b"d2".to_vec()]);
    assert_eq!(tx.stats().records_replayed, 2);
}
