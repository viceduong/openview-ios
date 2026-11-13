//! [`FrameMailbox`] and [`PrioritySender`] scheduling.

use super::*;

#[test]
fn newer_frame_replaces_an_unsent_one() {
    let mb = FrameMailbox::default();
    assert!(!mb.put(frame(1, 10)));
    assert!(mb.put(frame(2, 10)), "frame 1 was never taken");
    assert_eq!(mb.replaced_count(), 1);
    assert_eq!(mb.take().map(|f| f.seq), Some(2));
    assert!(mb.take().is_none());
}

#[test]
fn mailbox_put_fires_the_notify_hook() {
    let hits = std::sync::Arc::new(AtomicU64::new(0));
    let h = std::sync::Arc::clone(&hits);
    let mb = FrameMailbox::new(Some(Box::new(move || {
        h.fetch_add(1, Ordering::Relaxed);
    })));
    mb.put(frame(1, 1));
    mb.put(frame(2, 1));
    assert_eq!(hits.load(Ordering::Relaxed), 2);
}

#[test]
fn frame_streams_begin_chunks_end() {
    let mb = FrameMailbox::default();
    let mut tx = PrioritySender::new(4);
    mb.put(frame(7, 10));
    let now = Instant::now();
    let recs: Vec<_> = std::iter::from_fn(|| tx.next(&mb, now)).collect();
    assert_eq!(recs.len(), 1 + 3 + 1, "BEGIN + ceil(10/4) chunks + END");
    assert!(matches!(
        recs[0],
        Outgoing::Frame(Command::FrameBegin {
            seq: 7,
            compressed_len: 10,
            ..
        })
    ));
    let data: Vec<u8> = recs[1..4]
        .iter()
        .flat_map(|r| match r {
            Outgoing::Frame(Command::FrameData(c)) => c.clone(),
            other => panic!("expected FRAME_DATA, got {other:?}"),
        })
        .collect();
    assert_eq!(data, vec![7u8; 10]);
    assert!(is_frame_end(&recs[4], 7));
    assert_eq!(tx.frames_sent(), 1);
    assert_eq!(tx.meter().frames(), 1);
}

#[test]
fn at_most_one_frame_in_flight_and_newest_wins() {
    let mb = FrameMailbox::default();
    let mut tx = PrioritySender::new(4);
    let now = Instant::now();
    mb.put(frame(1, 8));
    // Start frame 1 (BEGIN + first chunk).
    assert!(matches!(
        tx.next(&mb, now),
        Some(Outgoing::Frame(Command::FrameBegin { seq: 1, .. }))
    ));
    assert!(matches!(
        tx.next(&mb, now),
        Some(Outgoing::Frame(Command::FrameData(_)))
    ));
    // Two newer frames arrive mid-flight: frame 2 is replaced by 3.
    mb.put(frame(2, 8));
    mb.put(frame(3, 8));
    // Frame 1 still finishes first (never torn).
    assert!(matches!(
        tx.next(&mb, now),
        Some(Outgoing::Frame(Command::FrameData(_)))
    ));
    assert!(is_frame_end(&tx.next(&mb, now).unwrap(), 1));
    // Then the newest frame, 3 — frame 2 was never sent.
    assert!(matches!(
        tx.next(&mb, now),
        Some(Outgoing::Frame(Command::FrameBegin { seq: 3, .. }))
    ));
    assert_eq!(mb.replaced_count(), 1);
}

#[test]
fn inflight_eta_tracks_remaining_bytes_at_the_measured_rate() {
    let mb = FrameMailbox::default();
    let mut tx = PrioritySender::new(100);
    let t = Instant::now();
    // No measurement yet: no estimate.
    mb.put(frame(1, 1000));
    tx.next(&mb, t);
    assert_eq!(tx.inflight_eta(), Duration::ZERO);
    // Drain frame 1 over 100 ms -> 10 000 B/s measured.
    while tx.frame_in_flight() {
        tx.next(&mb, t + Duration::from_millis(100));
    }
    assert_eq!(tx.meter().per_frame(), Some(Duration::from_millis(100)));
    // Frame 2: BEGIN + 3 chunks out, 700 B left -> ~70 ms.
    mb.put(frame(2, 1000));
    for _ in 0..4 {
        tx.next(&mb, t + Duration::from_millis(200));
    }
    let eta = tx.inflight_eta();
    assert!(
        eta > Duration::from_millis(65) && eta < Duration::from_millis(75),
        "{eta:?}"
    );
    // Idle link: zero.
    while tx.frame_in_flight() {
        tx.next(&mb, t + Duration::from_millis(300));
    }
    assert_eq!(tx.inflight_eta(), Duration::ZERO);
}

#[test]
fn control_never_waits_behind_more_than_one_chunk() {
    let mb = FrameMailbox::default();
    let mut tx = PrioritySender::new(4);
    let now = Instant::now();
    mb.put(frame(1, 4 * 100));
    let mut frame_records_since_control = 0usize;
    let mut worst = 0usize;
    let mut pong_pending = false;
    for i in 0..400 {
        // A PONG becomes due at arbitrary points while the frame streams.
        if i % 7 == 3 {
            tx.push_control(Command::Pong(i));
            pong_pending = true;
            frame_records_since_control = 0;
        }
        match tx.next(&mb, now) {
            Some(Outgoing::Control(Command::Pong(_))) => {
                worst = worst.max(frame_records_since_control);
                pong_pending = false;
            }
            Some(Outgoing::Frame(_)) => {
                if pong_pending {
                    frame_records_since_control += 1;
                }
            }
            Some(other) => panic!("unexpected {other:?}"),
            None => break,
        }
    }
    assert_eq!(worst, 0, "queued control goes before the next frame record");
    assert!(!tx.frame_in_flight());
}

#[test]
fn flush_sends_control_then_one_frame_record_over_a_session() {
    let (mut host, mut view) = session_pair();
    let mb = FrameMailbox::default();
    let mut tx = PrioritySender::new(1024);
    let mut f = crate::screen::synthetic_frame(5, 64, 32);
    f.seq = 5;
    let px = f.pixels.clone();
    mb.put(EncodedFrame::compress(&f));
    tx.push_control(Command::Cursor { x: 1, y: 2 });
    // One call: the cursor, the pacer's probe PING (frames
    // are about to flow) + exactly one frame record (FRAME_BEGIN).
    assert_eq!(tx.flush(&mut host, &mb, 1).unwrap(), 3);
    assert!(tx.frame_in_flight());
    while tx.has_work(&mb) {
        tx.flush(&mut host, &mb, 1).unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut got = None;
    while got.is_none() && Instant::now() < deadline {
        view.pump().unwrap();
        got = view.take_frame();
    }
    let got = got.expect("frame reassembled");
    assert_eq!(got.seq, 5);
    assert_eq!(got.pixels, px);
    // The viewer auto-echoed the probe; its PONG belongs to the pacer.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut pong = None;
    while pong.is_none() && Instant::now() < deadline {
        host.pump().unwrap();
        pong = host.take_pong();
    }
    let pong = pong.expect("probe echoed");
    assert!(crate::pacing::is_probe(pong));
    assert!(
        tx.on_pong(pong, Instant::now()),
        "probe PONG consumed by the pacer"
    );
    assert!(!tx.on_pong(3, Instant::now()), "a keepalive PONG is not");
    assert_eq!(tx.pacer().stats().probes_acked, 1);
}

#[test]
fn disabled_pacer_sends_no_probes() {
    let (mut host, _view) = session_pair();
    let mb = FrameMailbox::default();
    let mut tx = PrioritySender::new(1024);
    tx.pacer_mut().set_enabled(false);
    let f = crate::screen::synthetic_frame(1, 16, 16);
    mb.put(EncodedFrame::compress(&f));
    assert_eq!(tx.flush(&mut host, &mb, 1).unwrap(), 1, "FRAME_BEGIN only");
    assert_eq!(tx.pacing_deadline(&host, &mb), None);
}
