use super::*;

#[test]
fn keys_are_directional_and_domain_separated() {
    let (c, s) = ([1u8; 32], [2u8; 32]);
    let srv = derive_media_keys(&c, &s, true);
    let cli = derive_media_keys(&c, &s, false);
    assert_eq!(srv.tx, cli.rx);
    assert_eq!(srv.rx, cli.tx);
    assert_ne!(srv.tx, srv.rx);
    assert_ne!(srv.tx, c);
    assert_ne!(srv.tx, crate::direct::wire::derive_direct_key(&c, &s));
}

fn datagram(key: &[u8; 32], pn: u64, pt: &[u8]) -> Vec<u8> {
    let mut d = vec![V2_MAGIC, KIND_MEDIA, 0, 0, 0, 7, 0, 0, 0, 9];
    seal_into(key, pn, pt, &mut d);
    d
}

#[test]
fn seal_open_and_tamper() {
    let k = [5u8; 32];
    let d = datagram(&k, 42, b"hello media");
    assert_eq!(open(&k, &d), Some((42, b"hello media".to_vec())));
    // The transport header is authenticated too.
    let mut bad = d.clone();
    bad[3] ^= 1;
    assert!(open(&k, &bad).is_none());
    let mut bad = d.clone();
    bad[MEDIA_AAD] ^= 1;
    assert!(open(&k, &bad).is_none());
    assert!(open(&[6u8; 32], &d).is_none());
}

#[test]
fn replay_window_rejects_duplicates_and_stale() {
    let mut w = ReplayWindow::new();
    assert!(!w.check(0));
    for pn in [1u64, 3, 2, 10] {
        assert!(w.check(pn));
        w.commit(pn);
    }
    assert!(!w.check(3));
    assert!(w.check(9));
    w.commit(10 + REPLAY_WINDOW);
    assert!(!w.check(10), "older than the window");
    assert!(w.check(11), "inside the window, unseen");
    assert!(!w.check(10 + REPLAY_WINDOW), "the top itself was seen");
    assert!(w.check(12 + REPLAY_WINDOW));
}

fn unit(n: usize, seed: u8) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

fn feed(rx: &mut MediaRx, msgs: &[Vec<u8>], drop: &[usize], now: Instant) {
    for (i, m) in msgs.iter().enumerate() {
        if !drop.contains(&i) {
            rx.on_shard(m, now);
        }
    }
}

#[test]
fn fec_recovers_lost_shards_in_order() {
    let now = Instant::now();
    let mut rx = MediaRx::new();
    let u0 = unit(5000, 1);
    let u1 = unit(30_000, 2);
    let m0 = fec_encode(&u0, 0, true, 0.3).unwrap();
    let m1 = fec_encode(&u1, 1, false, 0.3).unwrap();
    assert!(m1.iter().all(|m| m.len() <= SHARD_HEADER + MAX_SHARD));
    // Frame 0's first shard, then frame 1 (reordered ahead of the rest of
    // frame 0) losing 3 shards, then frame 0 losing one.
    rx.on_shard(&m0[0], now);
    feed(&mut rx, &m1, &[0, 5, 9], now);
    feed(&mut rx, &m0[1..], &[1], now);
    rx.poll(now, None, &mut |_, _| panic!("no nack needed"));
    let ev = rx.take_events();
    assert_eq!(ev, vec![MediaEvent::Video(u0), MediaEvent::Video(u1)]);
    assert_eq!(rx.stats().fec_recovered, 2);
}

#[test]
fn unrecoverable_frame_is_nacked_on_a_short_rtt_and_lost_on_a_long_one() {
    let t0 = Instant::now();
    let u = unit(12_000, 3);
    let m = fec_encode(&u, 0, false, 0.1).unwrap(); // k = 11, m = 2
    let later = fec_encode(&unit(100, 4), 1, false, 0.1).unwrap();
    // Short RTT: NACK, then the retransmission completes it.
    let mut rx = MediaRx::new();
    feed(&mut rx, &m, &[0, 1, 2, 3], t0);
    feed(&mut rx, &later, &[], t0);
    let mut asked = Vec::new();
    rx.poll(
        t0 + Duration::from_millis(5),
        Some(Duration::from_millis(2)),
        &mut |f, idx| {
            asked.push((f, idx.to_vec()));
            true
        },
    );
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].0, 0);
    assert_eq!(asked[0].1.len(), 2, "only what is still needed");
    assert!(
        rx.take_events().is_empty(),
        "frame 1 waits behind the NACKed frame 0"
    );
    for &i in &asked[0].1 {
        let mut retx = m[usize::from(i)].clone();
        retx[15] |= SHARD_RETX;
        rx.on_shard(&retx, t0 + Duration::from_millis(7));
    }
    rx.poll(
        t0 + Duration::from_millis(7),
        Some(Duration::from_millis(2)),
        &mut |_, _| true,
    );
    let ev = rx.take_events();
    assert_eq!(ev.len(), 2);
    assert_eq!(ev[0], MediaEvent::Video(u.clone()));
    assert_eq!(rx.stats().nack_recovered, 1);
    // Long RTT: the frame is given up right away, the next one flows.
    let mut rx = MediaRx::new();
    feed(&mut rx, &m, &[0, 1, 2, 3], t0);
    feed(&mut rx, &later, &[], t0);
    rx.poll(
        t0 + Duration::from_millis(5),
        Some(Duration::from_millis(80)),
        &mut |_, _| panic!("no nack"),
    );
    let ev = rx.take_events();
    assert_eq!(ev[0], MediaEvent::VideoLost { frames: 1 });
    assert!(matches!(ev[1], MediaEvent::Video(_)));
}

#[test]
fn a_wholly_lost_frame_is_detected_from_the_next_one() {
    let t0 = Instant::now();
    let mut rx = MediaRx::new();
    feed(
        &mut rx,
        &fec_encode(&unit(800, 1), 7, true, 0.1).unwrap(),
        &[],
        t0,
    );
    rx.poll(t0, None, &mut |_, _| true);
    assert_eq!(rx.take_events().len(), 1);
    // Frame 8 never arrives; frame 9 does.
    feed(
        &mut rx,
        &fec_encode(&unit(800, 2), 9, false, 0.1).unwrap(),
        &[],
        t0,
    );
    rx.poll(t0 + Duration::from_millis(1), None, &mut |_, _| true);
    assert!(rx.take_events().is_empty(), "reorder grace");
    assert!(rx.next_deadline(t0 + Duration::from_millis(1)).is_some());
    rx.poll(t0 + Duration::from_millis(10), None, &mut |_, _| true);
    let ev = rx.take_events();
    assert_eq!(ev[0], MediaEvent::VideoLost { frames: 1 });
    assert!(matches!(ev[1], MediaEvent::Video(_)));
    // A paused stream is not a loss.
    rx.poll(t0 + Duration::from_secs(2), None, &mut |_, _| true);
    assert!(rx.take_events().is_empty());
}

#[test]
fn audio_gaps_are_counted_and_late_packets_dropped() {
    let mut rx = MediaRx::new();
    for s in [5u32, 6, 9, 8, 10] {
        rx.on_audio(&audio_msg(s, &[1, 0, 0, 0, 1, 224, 9]));
    }
    let lost: Vec<u32> = rx
        .take_events()
        .into_iter()
        .map(|e| match e {
            MediaEvent::Audio { lost, .. } => lost,
            _ => 99,
        })
        .collect();
    assert_eq!(lost, vec![0, 0, 2, 0]);
    assert_eq!(rx.stats().audio_late, 1);
}

#[test]
fn malformed_shards_are_rejected() {
    let m = fec_encode(&unit(3000, 1), 0, false, 0.2).unwrap();
    let mut bad = m[0].clone();
    bad[7..9].copy_from_slice(&0u16.to_be_bytes()); // k = 0
    assert!(parse_shard(&bad).is_none());
    let mut bad = m[0].clone();
    bad[11..15].copy_from_slice(&999_999u32.to_be_bytes()); // unit_len > k*S
    assert!(parse_shard(&bad).is_none());
    assert!(parse_shard(&m[0][..10]).is_none());
    // Oversize k / m: slots for them would be pre-allocated per frame.
    let mut bad = m[0].clone();
    bad[7..9].copy_from_slice(&u16::MAX.to_be_bytes());
    assert!(parse_shard(&bad).is_none(), "k inconsistent with unit_len");
    let mut bad = m[0].clone();
    bad[9..11].copy_from_slice(&u16::MAX.to_be_bytes());
    assert!(parse_shard(&bad).is_none(), "m > k");
    assert_eq!(parse_nack(&nack_msg(3, &[1, 2])), Some((3, vec![1, 2])));
    assert!(parse_nack(&[M_NACK, 0, 0, 0, 1, 0, 9]).is_none());
}

/// A peer that opens many large frames and never completes them cannot
/// make the receiver hold more than `MAX_BUFFERED` bytes.
#[test]
fn incomplete_frames_are_bounded_in_memory() {
    let mut rx = MediaRx::new();
    let t0 = Instant::now();
    let big = unit(MAX_UNIT, 3);
    for id in 0..40u32 {
        let msgs = fec_encode(&big, id, false, 0.1).unwrap();
        // All data shards but the last: never decodable.
        let k = MAX_DATA_SHARDS;
        for m in &msgs[..k - 1] {
            rx.on_shard(m, t0);
        }
        assert!(rx.buffered_bytes() <= MAX_BUFFERED, "frame {id}");
    }
    assert!(rx.stats().frames_lost > 0);
}
