//! Latency telemetry: TIMING round trip, clock probes, stage joins and
//! percentiles.

use super::*;
use ovcore::clocksync::{instant_us, us_instant};

#[test]
fn timing_record_round_trips() {
    let stamps = HostStamps {
        capture_us: 1_000_000,
        grabbed_us: 1_003_000,
        encoded_us: 1_007_500,
    };
    let t = FrameTiming::from_stamps(KIND_VIDEO, 42, &stamps, 1_008_000);
    assert_eq!(
        (t.grab_us, t.encode_us, t.queue_us),
        (3_000, 4_500, 500),
        "stage deltas"
    );
    assert_eq!(t.sent_us(), 1_008_000);
    let mut wire = Vec::new();
    t.encode(&mut wire);
    assert_eq!(wire.len(), FrameTiming::LEN);
    assert_eq!(FrameTiming::decode(&wire), Ok(t));
    assert!(FrameTiming::decode(&wire[..28]).is_err());
    wire.push(0);
    assert!(FrameTiming::decode(&wire).is_err());
}

#[test]
fn timing_stages_saturate_on_out_of_order_stamps() {
    // Encode stamped before the grab (clock read races): stages never wrap.
    let stamps = HostStamps {
        capture_us: 500,
        grabbed_us: 400,
        encoded_us: 0,
    };
    let t = FrameTiming::from_stamps(KIND_SCREEN, 1, &stamps, 100);
    assert_eq!((t.grab_us, t.encode_us, t.queue_us), (0, 0, 0));
    assert_eq!(t.sent_us(), 500);
}

#[test]
fn clock_probes_are_tagged_and_paced() {
    let mut p = ClockProbe::new();
    let t0 = Instant::now();
    let n1 = p.due(t0).expect("first probe at once");
    assert!(is_clock_probe(n1));
    assert!(!is_clock_probe(7), "keepalive sequences are not probes");
    assert!(!is_clock_probe(crate::pacing::PROBE_TAG | 7));
    assert_eq!(p.due(t0 + Duration::from_millis(50)), None);
    let n2 = p.due(t0 + Duration::from_millis(100)).expect("fast phase");
    assert_ne!(n1, n2);
    // After the fast phase, one per second.
    let mut t = t0 + Duration::from_millis(100);
    for _ in 0..PROBE_FAST {
        t += Duration::from_millis(100);
        let _ = p.due(t);
    }
    assert_eq!(p.due(t + Duration::from_millis(500)), None);
    assert!(p.due(t + Duration::from_secs(1)).is_some());
}

#[test]
fn timed_pong_yields_an_offset_sample() {
    let mut p = ClockProbe::new();
    let t0 = Instant::now();
    let nonce = p.due(t0).unwrap();
    let t1 = instant_us(t0);
    p.note_ping(nonce, t1);
    // Host 5 s ahead, 2 ms each way, 100 µs turnaround.
    let t2 = t1 + 5_000_000 + 2_000;
    let s = p
        .on_pong(nonce, t2, t2 + 100, t1 + 4_100)
        .expect("matching nonce");
    assert_eq!(s.offset_us, 5_000_000);
    assert_eq!(s.rtt_us, 4_000);
    assert_eq!(p.sync().offset_us(), Some(5_000_000));
    // The same echo again (or an unknown nonce) is ignored.
    assert!(p.on_pong(nonce, t2, t2, t1 + 5_000).is_none());
    assert!(p.on_pong(99, t2, t2, t1 + 5_000).is_none());
}

/// A host clock 3 s behind the viewer's, and one update through every stage.
#[test]
fn tracker_joins_host_and_viewer_stamps() {
    let mut clock = ClockSync::new();
    let offset = -3_000_000i64; // host - viewer
    clock.add(ovcore::clocksync::ClockSample {
        offset_us: offset,
        rtt_us: 1_000,
    });
    let base = instant_us(Instant::now()) + 10_000_000;
    let host = |viewer_us: u64| (viewer_us as i64 + offset) as u64;
    // Viewer-clock timeline (µs): capture 0, grab 3 ms, encode 7 ms, send
    // 8 ms, reassembled 20 ms, decoded 22 ms, presented 30 ms.
    let v = |ms: u64| base + ms * 1000;
    let stamps = HostStamps {
        capture_us: host(v(0)),
        grabbed_us: host(v(3)),
        encoded_us: host(v(7)),
    };
    let mut tr = LatencyTracker::new();
    tr.on_timing(FrameTiming::from_stamps(
        KIND_SCREEN,
        9,
        &stamps,
        host(v(8)),
    ));
    tr.on_received(KIND_SCREEN, 9, us_instant(v(20)));
    tr.on_decoded(KIND_SCREEN, 9, us_instant(v(22)));
    assert_eq!(
        tr.on_presented(us_instant(v(25)), us_instant(v(30)), Some(&clock)),
        1
    );
    let snap = tr.snapshot(Some(&clock));
    let p50 = |s: Stage| snap.stage(s).p50_us;
    assert_eq!(p50(Stage::Capture), 3_000);
    assert_eq!(p50(Stage::Encode), 4_000);
    assert_eq!(p50(Stage::Send), 1_000);
    assert_eq!(p50(Stage::Network), 12_000);
    assert_eq!(p50(Stage::Decode), 2_000);
    assert_eq!(p50(Stage::Present), 8_000);
    assert_eq!(p50(Stage::Total), 30_000);
    assert_eq!(snap.frames, 1);
    assert_eq!(snap.offset_us, Some(offset));
    assert_eq!(
        snap.hud_line().unwrap(),
        "cap 3.0 · enc 4.0 · send 1.0 · net 12 · dec 2.0 · pres 8.0 = 30 ms"
    );
    assert!(snap
        .metrics_fragment()
        .starts_with("lat_ms p50/p95 cap 3.0/3.0 enc 4.0/4.0"));
}

#[test]
fn present_only_takes_updates_decoded_before_the_draw() {
    let t = Instant::now();
    let ms = |n: u64| t + Duration::from_millis(n);
    let mut tr = LatencyTracker::new();
    tr.on_received(KIND_SCREEN, 1, ms(1));
    tr.on_received(KIND_VIDEO, 1, ms(2));
    tr.on_decoded_all(ms(3));
    tr.on_received(KIND_SCREEN, 2, ms(4));
    tr.on_decoded(KIND_SCREEN, 2, ms(6));
    // Drawn at 5 ms: update 2 (decoded at 6) waits for the next present.
    assert_eq!(tr.on_presented(ms(5), ms(9), None), 2);
    assert_eq!(tr.on_presented(ms(10), ms(12), None), 1);
    let snap = tr.snapshot(None);
    assert_eq!(snap.frames, 3);
    // Without stamps or offset, only the viewer stages have data.
    assert_eq!(snap.stage(Stage::Total).n, 0);
    assert_eq!(snap.stage(Stage::Capture).n, 0);
    assert_eq!(snap.stage(Stage::Decode).n, 3);
    let line = snap.hud_line().unwrap();
    assert!(line.starts_with("cap ? · enc ? · send ? · net ?"), "{line}");
    assert!(line.ends_with("= ? ms"), "{line}");
}

#[test]
fn decoding_an_unreceived_update_is_ignored() {
    let t = Instant::now();
    let mut tr = LatencyTracker::new();
    tr.on_decoded(KIND_SCREEN, 5, t);
    assert_eq!(tr.on_presented(t, t, None), 0);
    assert_eq!(tr.snapshot(None).hud_line(), None, "nothing measured yet");
}

#[test]
fn pending_updates_are_bounded() {
    let t = Instant::now();
    let mut tr = LatencyTracker::new();
    for seq in 0..(MAX_PENDING as u64 * 3) {
        tr.on_received(KIND_SCREEN, seq, t);
    }
    assert_eq!(tr.pending.len(), MAX_PENDING);
    tr.on_decoded_all(t);
    assert_eq!(tr.on_presented(t, t, None), MAX_PENDING);
}

#[test]
fn rolling_percentiles_follow_the_window() {
    let t = Instant::now();
    let mut tr = LatencyTracker::new();
    // 100 updates with present delays 1..=100 ms.
    for i in 1..=100u64 {
        tr.on_received(KIND_SCREEN, i, t);
        tr.on_decoded(KIND_SCREEN, i, t);
        tr.on_presented(t, t + Duration::from_millis(i), None);
    }
    let p = tr.snapshot(None).stage(Stage::Present);
    assert_eq!(p.n, 100);
    assert_eq!(p.p50_us, 51_000, "nearest rank of 100 samples");
    assert_eq!(p.p95_us, 95_000);
    assert_eq!(tr.session_quantile(Stage::Present, 0.99), Some(99_000));
    // The window slides: 200 more fast updates push the slow ones out.
    for i in 101..=300u64 {
        tr.on_received(KIND_SCREEN, i, t);
        tr.on_decoded(KIND_SCREEN, i, t);
        tr.on_presented(t, t + Duration::from_millis(1), None);
    }
    let p = tr.snapshot(None).stage(Stage::Present);
    assert_eq!(p.n as usize, ROLLING_WINDOW);
    assert_eq!(p.p95_us, 1_000);
    assert_eq!(tr.session_len(Stage::Present), 300);
}

#[test]
fn quantile_nearest_rank() {
    assert_eq!(quantile(&[], 0.5), None);
    assert_eq!(quantile(&[7], 0.99), Some(7));
    assert_eq!(quantile(&[1, 2, 3, 4], 0.5), Some(3));
    assert_eq!(quantile(&[1, 2, 3, 4], 0.0), Some(1));
    assert_eq!(quantile(&[1, 2, 3, 4], 1.0), Some(4));
}
