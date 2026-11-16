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

/// What [`run_media`] observed.
struct MediaRun {
    units: Vec<u32>,
    lost: u32,
    audio: u32,
    audio_lost: u32,
    recs: u32,
    hs: MediaStats,
    vs: MediaStats,
    /// The FEC ratio each frame was sent with.
    ratios: Vec<f64>,
}

/// `frames` video frames (30 fps, 2-32 KB, an IDR every 50) plus audio and
/// a reliable record per frame across a path that drops `loss_pct(frame)`
/// percent of datagrams both ways (1 ms ticks).
fn run_media(frames: u32, loss_pct: &dyn Fn(u32) -> u64) -> MediaRun {
    let t0 = Instant::now();
    let (mut h, mut v) = media_pair(t0);
    assert!(h.media_ready() && v.media_ready());
    let (mut hio, mut vio) = (MemIo::default(), MemIo::default());
    // What promotion would have sent on the relay: records 0.. go direct.
    v.on_primary(&marker(M_SYNC_UP, 0), t0, &mut vio, &mut |_| {});
    let mut rng = 0x2545_F491_4F6C_DD1Du64;
    let mut drop = move |pct: u64| {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng % 100 < pct
    };
    let mut now = t0;
    let (mut units, mut lost, mut audio, mut audio_lost, mut recs) = (Vec::new(), 0u32, 0, 0u32, 0);
    let mut ratios = Vec::new();
    for f in 0..frames {
        let pct = loss_pct(f);
        let mut unit = vec![(f % 251) as u8; 2000 + (f as usize * 997) % 30_000];
        unit[..4].copy_from_slice(&f.to_be_bytes());
        assert!(h.send_media_video(&unit, f % 50 == 0, now, &mut hio));
        ratios.push(h.fec_ratio());
        for _ in 0..3 {
            assert!(h.send_media_audio(&[1, 0, 0, 0, 1, 224, 9], now, &mut hio));
        }
        h.send(&[0xAB; 300], now, &mut hio).unwrap();
        for _ in 0..33 {
            now += Duration::from_millis(1);
            h.poll(now, &mut hio, &mut |_| {});
            for d in std::mem::take(&mut hio.direct) {
                if !drop(pct) {
                    v.on_direct(0, &d, now, &mut vio, &mut |_| recs += 1);
                }
            }
            v.poll(now, &mut vio, &mut |_| recs += 1);
            for d in std::mem::take(&mut vio.direct) {
                if !drop(pct) {
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
                    MediaEvent::PictureLoss(_) | MediaEvent::PictureUnanswered(_) => {}
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
        units.len() as u32 + lost >= frames - 1,
        "every frame delivered or reported lost"
    );
    assert_eq!(recs, frames, "reliable records next to the media");
    MediaRun {
        units,
        lost,
        audio,
        audio_lost,
        recs,
        hs,
        vs,
        ratios,
    }
}

/// Video units cross a lossy path in order — FEC repairs most
/// losses, NACKs (short RTT) the rest; audio gaps are reported; the
/// controller gets feedback; records still flow reliably next to it.
#[test]
fn media_frames_survive_random_loss() {
    let r = run_media(150, &|_| 3);
    assert!(r.lost <= 2, "{} frames lost at 3% loss", r.lost);
    assert!(r.vs.rx.fec_recovered > 10);
    assert!(r.audio_lost > 0 && r.audio + r.audio_lost >= 449);
    assert!(r.hs.cc.feedbacks > 50, "feedback reached the sender");
    assert!(r.hs.parity_tx > 0);
    // Lossy link: parity never drops below the 10 % floor.
    assert!(r.ratios.iter().all(|&x| x >= media::FEC_MIN));
    assert!(r.recs == 150);
}

/// 1 % loss: still (almost) every frame, parity at 10 %+.
#[test]
fn media_frames_survive_one_percent_loss() {
    let r = run_media(150, &|_| 1);
    assert!(r.lost <= 1, "{} frames lost at 1% loss", r.lost);
    assert!(r.ratios[100..].iter().all(|&x| x >= media::FEC_MIN));
}

/// Adaptive FEC: a clean, short-RTT link drops parity to the floor after
/// the hold time (keyframes keep 10 %); loss starting later ramps it back
/// up within a feedback round and NACK + FEC still recover the frames.
#[test]
fn media_fec_adapts_to_loss() {
    let r = run_media(300, &|f| if f < 150 { 0 } else { 3 });
    // Clean phase: 10 % until FEC_CLEAN_HOLD (3 s = 90 frames), then 2 %
    // for delta frames.
    assert!(r.ratios[..80].iter().all(|&x| x == media::FEC_MIN));
    for (f, &x) in r.ratios.iter().enumerate().take(150).skip(100) {
        let want = if f % 50 == 0 {
            media::FEC_MIN
        } else {
            media::FEC_FLOOR
        };
        assert_eq!(x, want, "frame {f}");
    }
    // Loss phase: back to >= 10 % within a few frames of the first loss.
    assert!(r.ratios[160..].iter().all(|&x| x >= media::FEC_MIN));
    assert!(r.lost <= 3, "{} frames lost after the switch", r.lost);
    assert!(r.units.len() >= 297);
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

/// A viewer's PLI reaches the host once per request (repeats ignored),
/// drops the video still queued whole in the pacer and rewinds the frame
/// ids, so the next (recovery) frame follows without a gap.
#[test]
fn pli_purges_queued_video_and_is_acted_on_once() {
    let t0 = Instant::now();
    let (mut h, mut v) = media_pair(t0);
    let (mut hio, mut vio) = (MemIo::default(), MemIo::default());
    let unit = |tag: u8| vec![tag; 20_000];
    for tag in 0..3 {
        assert!(h.send_media_video(&unit(tag), tag == 0, t0, &mut hio));
    }
    let req = media::PictureRequest::Recover { ltr_seq: 7 };
    assert!(v.request_picture(req, t0, &mut vio));
    for d in std::mem::take(&mut vio.direct) {
        h.on_direct(0, &d, t0, &mut hio, &mut |_| {});
    }
    assert_eq!(h.take_media_events(), vec![MediaEvent::PictureLoss(req)]);
    assert!(h.send_media_video(&unit(9), false, t0, &mut hio));
    // A repeat about an RTT later (100 ms without an RTT sample) is ignored.
    let t1 = t0 + media::PLI_EVERY_MAX;
    v.poll(t1, &mut vio, &mut |_| {});
    let mut got = Vec::new();
    let mut now = t1;
    while now < t1 + Duration::from_secs(1) {
        for d in std::mem::take(&mut vio.direct) {
            h.on_direct(0, &d, now, &mut hio, &mut |_| {});
        }
        h.poll(now, &mut hio, &mut |_| {});
        for d in std::mem::take(&mut hio.direct) {
            v.on_direct(0, &d, now, &mut vio, &mut |_| {});
        }
        v.poll(now, &mut vio, &mut |_| {});
        got.extend(v.take_media_events());
        assert!(h.take_media_events().is_empty(), "acted on once");
        v.picture_arrived();
        now += Duration::from_millis(1);
    }
    assert_eq!(
        got,
        vec![MediaEvent::Video(unit(0)), MediaEvent::Video(unit(9))]
    );
    assert_eq!(
        (
            v.media_stats().unwrap().pli_tx,
            h.media_stats().unwrap().pli_rx
        ),
        (2, 2)
    );
}

/// A PLI whose repeats run out with no way back in arriving is handed
/// back once, for the reliable request.
#[test]
fn unanswered_pli_is_handed_back_once() {
    let t0 = Instant::now();
    let (_h, mut v) = media_pair(t0);
    let mut vio = MemIo::default();
    let req = media::PictureRequest::Keyframe { have_seq: 3 };
    assert!(v.request_picture(req, t0, &mut vio));
    assert_eq!(v.picture_pending(), Some(req));
    v.poll(t0 + media::PLI_REPEAT_FOR / 2, &mut vio, &mut |_| {});
    assert!(v.take_media_events().is_empty());
    v.poll(t0 + media::PLI_REPEAT_FOR, &mut vio, &mut |_| {});
    assert_eq!(v.picture_pending(), None);
    assert_eq!(
        v.take_media_events(),
        vec![MediaEvent::PictureUnanswered(req)]
    );
    v.poll(t0 + media::PLI_REPEAT_FOR * 2, &mut vio, &mut |_| {});
    assert!(v.take_media_events().is_empty());
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

/// A direct path that is blocked at first (a VPN / carrier-NAT hiccup) is
/// picked up by the viewer's continuing re-probes once it opens: the live
/// session migrates from the relay within the backoff cap, and no record
/// is lost, duplicated or reordered on the way.
#[test]
fn blocked_path_migrates_once_it_opens() {
    let t0 = Instant::now();
    let mut h = DirectEngine::new(true, [2; 32]);
    let mut v = DirectEngine::new(false, [2; 32]);
    h.arm();
    v.arm();
    v.start_probe(vec![0], t0);
    let (mut hio, mut vio) = (MemIo::default(), MemIo::default());
    let (mut hg, mut vg): (Vec<Vec<u8>>, Vec<Vec<u8>>) = (Vec::new(), Vec::new());
    let opens = Duration::from_secs(20);
    let mut migrated = None;
    let mut sent = 0u32;
    let mut t = t0;
    while t < t0 + Duration::from_secs(40) {
        t += Duration::from_millis(10);
        if t < t0 + Duration::from_secs(38) {
            let r = sent.to_be_bytes();
            v.send(&r, t, &mut vio).unwrap();
            h.send(&r, t, &mut hio).unwrap();
            sent += 1;
        }
        v.poll(t, &mut vio, &mut |r| vg.push(r.to_vec()));
        h.poll(t, &mut hio, &mut |r| hg.push(r.to_vec()));
        let open = t >= t0 + opens;
        for d in std::mem::take(&mut vio.relay) {
            h.on_primary(&d, t, &mut hio, &mut |r| hg.push(r.to_vec()));
        }
        for d in std::mem::take(&mut hio.relay) {
            v.on_primary(&d, t, &mut vio, &mut |r| vg.push(r.to_vec()));
        }
        for d in std::mem::take(&mut vio.direct).into_iter().filter(|_| open) {
            h.on_direct(0, &d, t, &mut hio, &mut |r| hg.push(r.to_vec()));
        }
        for d in std::mem::take(&mut hio.direct).into_iter().filter(|_| open) {
            v.on_direct(0, &d, t, &mut vio, &mut |r| vg.push(r.to_vec()));
        }
        let direct = v.state() == DirectState::Direct && h.state() == DirectState::Direct;
        assert!(open || !direct, "direct while blocked");
        if direct && migrated.is_none() {
            migrated = Some(t - t0);
            assert!(v.stats().probe_rounds > 3, "kept probing while blocked");
        }
    }
    let migrated = migrated.expect("never migrated");
    assert!(
        migrated <= opens + REPROBE_MAX + PROBE_INTERVAL * 2,
        "migrated after {migrated:?}"
    );
    assert!(h.stats().records_direct_rx > 0 && v.stats().records_direct_rx > 0);
    let want: Vec<Vec<u8>> = (0..sent).map(|i| i.to_be_bytes().to_vec()).collect();
    assert!(hg == want, "host got {} of {sent} in order", hg.len());
    assert!(vg == want, "viewer got {} of {sent} in order", vg.len());
}

/// A network change cuts the backoff short: the viewer probes again at once.
#[test]
fn network_change_reprobes_at_once() {
    let t0 = Instant::now();
    let mut v = DirectEngine::new(false, [2; 32]);
    v.arm();
    v.start_probe(vec![0], t0);
    let mut io = MemIo::default();
    v.poll(t0 + PROBE_ROUND, &mut io, &mut |_| {});
    assert_eq!(v.state(), DirectState::Relay, "round failed, backing off");
    // Grow the backoff past its first step.
    let mut t = t0 + PROBE_ROUND;
    for _ in 0..2 {
        t += v.next_deadline(t).unwrap();
        v.poll(t, &mut io, &mut |_| {});
        t += PROBE_ROUND;
        v.poll(t, &mut io, &mut |_| {});
    }
    assert!(v.next_deadline(t).unwrap() > REPROBE_AFTER);
    let t = t + Duration::from_millis(1);
    v.network_changed(t, &mut io);
    assert_eq!(v.state(), DirectState::Probing);
    assert_eq!(v.stats().probe_rounds, 4);
    // The backoff restarts from its first step.
    let t = t + PROBE_ROUND;
    v.poll(t, &mut io, &mut |_| {});
    assert_eq!(v.state(), DirectState::Relay);
    assert_eq!(v.next_deadline(t), Some(REPROBE_AFTER));
}

/// On the direct path a network change sends a REBIND at once (no new
/// probe round).
#[test]
fn network_change_rebinds_an_active_link() {
    let t0 = Instant::now();
    let key = [2; 32];
    let mut v = DirectEngine::new(false, key);
    v.arm();
    v.start_probe(vec![0], t0);
    let mut io = MemIo::default();
    v.link = Some(v.make_link(0, t0, true));
    v.on_direct = true;
    v.network_changed(t0 + Duration::from_millis(1), &mut io);
    assert_eq!(v.state(), DirectState::Direct);
    assert_eq!(v.stats().probe_rounds, 1);
    assert_eq!(v.stats().rebinds, 1);
    let sent: Vec<u8> = io
        .direct
        .iter()
        .filter_map(|d| open_probe(&key, d).map(|(ty, ..)| ty))
        .collect();
    assert_eq!(sent, vec![P_REBIND]);
}

/// A lost COMMIT is repeated while the viewer sends on the unconfirmed
/// link: the host still promotes within two keepalive periods.
#[test]
fn lost_commit_is_repeated() {
    let t0 = Instant::now();
    let key = [5; 32];
    let mut h = DirectEngine::new(true, key);
    let mut v = DirectEngine::new(false, key);
    h.arm();
    v.arm();
    v.start_probe(vec![0], t0);
    let (mut hio, mut vio) = (MemIo::default(), MemIo::default());
    let mut dropped = None;
    let mut t = t0;
    while t < t0 + Duration::from_secs(2) && h.state() != DirectState::Direct {
        t += Duration::from_millis(10);
        v.send(b"rec", t, &mut vio).unwrap();
        v.poll(t, &mut vio, &mut |_| {});
        h.poll(t, &mut hio, &mut |_| {});
        for d in std::mem::take(&mut vio.relay) {
            h.on_primary(&d, t, &mut hio, &mut |_| {});
        }
        for d in std::mem::take(&mut hio.relay) {
            v.on_primary(&d, t, &mut vio, &mut |_| {});
        }
        for d in std::mem::take(&mut vio.direct) {
            let commit = open_probe(&key, &d).is_some_and(|(ty, ..)| ty == P_COMMIT);
            if commit && dropped.is_none() {
                dropped = Some(t);
                continue;
            }
            h.on_direct(0, &d, t, &mut hio, &mut |_| {});
        }
        for d in std::mem::take(&mut hio.direct) {
            v.on_direct(0, &d, t, &mut vio, &mut |_| {});
        }
    }
    let dropped = dropped.expect("no COMMIT sent");
    assert_eq!(h.state(), DirectState::Direct, "host never promoted");
    assert!(
        t - dropped <= KEEPALIVE * 2,
        "promoted {:?} after the loss",
        t - dropped
    );
    assert!(h.take_events().iter().any(|e| e.contains("committed")));
}
