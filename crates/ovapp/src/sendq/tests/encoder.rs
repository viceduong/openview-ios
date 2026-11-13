//! [`UpdateEncoder`] tile deltas, rebasing, CAPS negotiation and resync.

use super::*;

/// The core tile-delta invariant: whatever subset of updates the sender
/// gets to take (newer ones replace unsent older ones in the mailbox),
/// the viewer applying exactly the taken ones in order always ends up
/// with the pixels of the capture behind the last one it applied.
#[test]
fn dropped_updates_merge_into_the_next_delta() {
    let (w, h) = (300, 200);
    let mb = FrameMailbox::default();
    let mut enc = UpdateEncoder::new();
    enc.set_peer_caps(LOCAL_CAPS);
    let mut dec = ScreenDecoder::new();
    let mut rng = Rng(0xC0FFEE);
    let mut screen = noise(w, h, 1);
    // seq -> the capture each queued update encodes.
    let mut captured = std::collections::HashMap::new();
    let (mut deltas, mut fulls, mut replaced) = (0, 0, 0);
    for step in 0..600 {
        mutate(&mut screen, w, h, &mut rng);
        let seq = enc.next_seq();
        let was_pending = mb.is_pending();
        match enc.submit(&screen, w, h, None, &mb) {
            SubmitOutcome::Unchanged => {}
            SubmitOutcome::Full { .. } => {
                fulls += 1;
                replaced += usize::from(was_pending);
                captured.insert(seq, screen.clone());
            }
            SubmitOutcome::Delta { .. } => {
                deltas += 1;
                replaced += usize::from(was_pending);
                captured.insert(seq, screen.clone());
            }
        }
        // The link drains at a random pace: often several captures per
        // send, so unsent updates get replaced.
        if rng.below(3) == 0 || step % 50 == 49 {
            if let Some(f) = mb.take() {
                apply(&mut dec, &f);
                assert_eq!(
                    dec.frame().unwrap().pixels,
                    captured[&f.seq],
                    "after update {}",
                    f.seq
                );
            }
        }
    }
    assert!(
        deltas > 100 && replaced > 50,
        "deltas {deltas} fulls {fulls} replaced {replaced}"
    );
}

/// The sender taking the previous update between the encoder reading
/// the base and queueing its delta must not let a delta with a stale
/// base through.
#[test]
fn a_take_between_diff_and_put_forces_a_rebase() {
    let mb = FrameMailbox::default();
    mb.put(frame(1, 4));
    let base = mb.last_taken();
    assert_eq!(mb.take().map(|f| f.seq), Some(1));
    assert!(
        mb.put_if_taken(frame(2, 4), base, true).is_err(),
        "stale base refused"
    );
    assert_eq!(mb.put_if_taken(frame(2, 4), Some(1), true), Ok(false));
    assert_eq!(mb.last_taken(), Some(1));
}

#[test]
fn many_dirty_tiles_send_a_full_frame_instead() {
    let (w, h) = (640, 640); // 100 tiles
    let mb = FrameMailbox::default();
    let mut enc = UpdateEncoder::new();
    enc.set_peer_caps(LOCAL_CAPS);
    let mut px = noise(w, h, 3);
    assert!(
        matches!(enc.submit(&px, w, h, None, &mb), SubmitOutcome::Full { .. }),
        "first frame"
    );
    mb.take();
    let touch = |px: &mut Vec<u8>, tiles: u32| {
        for t in 0..tiles {
            let (c, r) = (t % 10, t / 10);
            let i = (((r * 64) * w + c * 64) * 4) as usize;
            px[i] = px[i].wrapping_add(1);
        }
    };
    touch(&mut px, 60); // exactly 60%: still a delta
    assert!(matches!(
        enc.submit(&px, w, h, None, &mb),
        SubmitOutcome::Delta { tiles: 60, .. }
    ));
    mb.take();
    touch(&mut px, 61); // > 60%: full
    assert!(matches!(
        enc.submit(&px, w, h, None, &mb),
        SubmitOutcome::Full { .. }
    ));
    mb.take();
    // Unchanged after that, until a keyframe is requested.
    assert_eq!(enc.submit(&px, w, h, None, &mb), SubmitOutcome::Unchanged);
    enc.request_keyframe();
    assert!(matches!(
        enc.submit(&px, w, h, None, &mb),
        SubmitOutcome::Full { .. }
    ));
    assert!(mb.take().unwrap().delta.is_none());
    assert_eq!(enc.submit(&px, w, h, None, &mb), SubmitOutcome::Unchanged);
    // Monitor switch / resolution change: full.
    enc.reset();
    assert!(matches!(
        enc.submit(&px, w, h, None, &mb),
        SubmitOutcome::Full { .. }
    ));
    let small = noise(320, 200, 4);
    assert!(matches!(
        enc.submit(&small, 320, 200, None, &mb),
        SubmitOutcome::Full { .. }
    ));
}

#[test]
fn an_old_viewer_only_ever_gets_deflate_full_frames() {
    let (w, h) = (256, 256);
    let mb = FrameMailbox::default();
    let mut enc = UpdateEncoder::new(); // no CAPS from the viewer
    let mut px = noise(w, h, 5);
    let mut dec = ScreenDecoder::new();
    for i in 0..5 {
        px[i * 4] ^= 1;
        assert!(matches!(
            enc.submit(&px, w, h, None, &mb),
            SubmitOutcome::Full { .. }
        ));
        let f = mb.take().unwrap();
        assert!(f.delta.is_none());
        assert_eq!(f.codec, FrameCodec::Deflate);
        assert!(matches!(f.begin(), Command::FrameBegin { .. }));
        apply(&mut dec, &f);
        assert_eq!(dec.frame().unwrap().pixels, px);
    }
    assert_eq!(enc.submit(&px, w, h, None, &mb), SubmitOutcome::Unchanged);
}

#[test]
fn zstd_only_for_a_viewer_that_advertised_it() {
    let (w, h) = (128, 128);
    let px = noise(w, h, 6);
    let mb = FrameMailbox::default();
    let mut enc = UpdateEncoder::new();
    enc.set_peer_caps(CAP_TILES);
    enc.submit(&px, w, h, None, &mb);
    assert_eq!(mb.take().unwrap().codec, FrameCodec::Deflate);
    enc.set_peer_caps(CAP_TILES | crate::terminal::CAP_ZSTD);
    enc.request_keyframe();
    enc.submit(&px, w, h, None, &mb);
    let want = if crate::codec::zstd_encode_available() {
        FrameCodec::Zstd
    } else {
        FrameCodec::Deflate
    };
    assert_eq!(mb.take().unwrap().codec, want);
    // A CAP_BANDS viewer gets band containers.
    enc.set_peer_caps(LOCAL_CAPS);
    enc.request_keyframe();
    enc.submit(&px, w, h, None, &mb);
    assert_eq!(mb.take().unwrap().codec, FrameCodec::Packed);
}

/// OS dirty rects (DXGI) as hints: changes inside them are found,
/// claimed-dirty-but-identical areas cost nothing.
#[test]
fn hints_drive_the_diff() {
    let (w, h) = (256, 256);
    let mb = FrameMailbox::default();
    let mut enc = UpdateEncoder::new();
    enc.set_peer_caps(LOCAL_CAPS);
    let mut px = noise(w, h, 7);
    enc.submit(&px, w, h, Some(&[]), &mb); // first frame: full regardless
    mb.take();
    assert_eq!(
        enc.submit(&px, w, h, Some(&[]), &mb),
        SubmitOutcome::Unchanged
    );
    let whole = [TileRect {
        x: 0,
        y: 0,
        w: 256,
        h: 256,
    }];
    assert_eq!(
        enc.submit(&px, w, h, Some(&whole), &mb),
        SubmitOutcome::Unchanged
    );
    px[(70 * 256 + 70) * 4] ^= 0xFF;
    let hint = [TileRect {
        x: 70,
        y: 70,
        w: 1,
        h: 1,
    }];
    assert!(matches!(
        enc.submit(&px, w, h, Some(&hint), &mb),
        SubmitOutcome::Delta { tiles: 1, .. }
    ));
}

/// A change captured while the link is busy (its update still unsent)
/// and followed only by idle captures must still reach the viewer:
/// DXGI hints name only what changed since the *previous capture*, so a
/// later delta must re-carry the replaced update's tiles, and idle
/// captures (empty hints) must not drop them.
#[test]
fn change_queued_while_busy_survives_replacement_and_idle() {
    let (w, h) = (320, 256);
    let mb = FrameMailbox::default();
    let mut enc = UpdateEncoder::new();
    enc.set_peer_caps(LOCAL_CAPS);
    let mut dec = ScreenDecoder::new();
    let mut px = noise(w, h, 41);
    enc.submit(&px, w, h, None, &mb);
    apply(&mut dec, &mb.take().unwrap());
    let fill = |px: &mut [u8], r: TileRect, v: u8| {
        for y in r.y..r.y + r.h {
            let o = (usize::from(y) * w as usize + usize::from(r.x)) * 4;
            px[o..o + usize::from(r.w) * 4]
                .iter_mut()
                .for_each(|b| *b = v);
        }
    };
    // A: a big block appears (the Start menu); the link is busy, so
    // its update waits in the mailbox.
    let a = TileRect {
        x: 0,
        y: 64,
        w: 192,
        h: 192,
    };
    fill(&mut px, a, 0x11);
    assert!(matches!(
        enc.submit(&px, w, h, Some(&[a]), &mb),
        SubmitOutcome::Delta { .. }
    ));
    // B: a small change elsewhere replaces the unsent update; its hint
    // names only B.
    let b = TileRect {
        x: 300,
        y: 0,
        w: 4,
        h: 4,
    };
    fill(&mut px, b, 0x22);
    assert!(matches!(
        enc.submit(&px, w, h, Some(&[b]), &mb),
        SubmitOutcome::Delta { .. }
    ));
    assert_eq!(mb.replaced_count(), 1);
    // Idle: nothing new, nothing queued, nothing forgotten.
    for _ in 0..5 {
        assert_eq!(
            enc.submit(&px, w, h, Some(&[]), &mb),
            SubmitOutcome::Unchanged
        );
    }
    apply(
        &mut dec,
        &mb.take().expect("the merged update is still waiting"),
    );
    assert_eq!(
        dec.frame().unwrap().pixels,
        px,
        "A and B both on the viewer"
    );
    assert!(!mb.is_pending());
}

/// Caps negotiation over a real session: an old viewer (no CAPS) leaves
/// the host without peer caps; a new viewer's CAPS gets the host's reply.
#[test]
fn caps_negotiation_old_and_new_viewer() {
    let (mut host, mut view) = session_pair();
    for _ in 0..20 {
        host.pump().unwrap();
        view.pump().unwrap();
    }
    assert_eq!(host.peer_caps(), None, "old viewer: never advertised");
    assert_eq!(view.peer_caps(), None);
    view.announce_caps().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while (host.peer_caps().is_none() || view.peer_caps().is_none()) && Instant::now() < deadline {
        host.pump().unwrap();
        view.pump().unwrap();
    }
    let ours = LOCAL_CAPS | crate::terminal::VIEWER_W2_CAPS | crate::audio::audio_caps();
    assert_eq!(host.peer_caps(), Some(ours));
    assert_eq!(view.peer_caps(), Some(ours));
}

/// End to end: tile deltas stream through PrioritySender into the
/// viewer's framebuffer; a lost update makes the viewer ask for a
/// keyframe, and the host's answer resyncs it.
#[test]
fn lost_update_triggers_a_keyframe_request_and_resync() {
    let (mut host, mut view) = session_pair();
    let hook_kf = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let caps = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    {
        let kf = std::sync::Arc::clone(&hook_kf);
        let caps = std::sync::Arc::clone(&caps);
        host.on_cmd(Some(Box::new(move |cmd| match cmd {
            Command::KeyframeRequest { .. } => kf.store(true, Ordering::Relaxed),
            Command::Caps { flags } => caps.store(*flags, Ordering::Relaxed),
            _ => {}
        })));
    }
    view.announce_caps().unwrap();
    let (w, h) = (320, 192);
    let mb = FrameMailbox::default();
    let mut tx = PrioritySender::new(1024);
    let mut enc = UpdateEncoder::new();
    let pump_all =
        |host: &mut TerminalSession, view: &mut TerminalSession, tx: &mut PrioritySender| {
            for _ in 0..50 {
                tx.flush(host, &mb, 64).unwrap();
                host.pump().unwrap();
                view.pump().unwrap();
            }
        };
    pump_all(&mut host, &mut view, &mut tx);
    assert!(view.peer_caps().is_some_and(|c| c & CAP_KEYFRAME != 0));
    enc.set_peer_caps(caps.load(Ordering::Relaxed));
    // Exact pixels per update are the point here (lossy tiles
    // are covered by their own tests).
    enc.set_lossy(false);
    let mut px = noise(w, h, 8);
    let mut rng = Rng(99);
    for _ in 0..10 {
        mutate(&mut px, w, h, &mut rng);
        if enc.submit(&px, w, h, None, &mb) == SubmitOutcome::Unchanged {
            continue;
        }
        pump_all(&mut host, &mut view, &mut tx);
        let got = view.take_latest_frame().expect("frame");
        assert_eq!(got.pixels, px);
    }
    // Lose one update on the way (taken from the mailbox, never sent).
    px[0] ^= 0xFF;
    enc.submit(&px, w, h, None, &mb);
    let lost = mb.take().unwrap();
    assert!(lost.delta.is_some());
    px[(100 * w as usize + 300) * 4] ^= 0xFF;
    enc.submit(&px, w, h, None, &mb);
    pump_all(&mut host, &mut view, &mut tx);
    assert!(
        view.take_latest_frame().is_none(),
        "the orphan delta is not shown"
    );
    pump_all(&mut host, &mut view, &mut tx);
    assert!(
        hook_kf.load(Ordering::Relaxed),
        "viewer asked for a keyframe"
    );
    assert_eq!(view.stats().keyframe_requests, 1);
    enc.request_keyframe();
    assert!(matches!(
        enc.submit(&px, w, h, None, &mb),
        SubmitOutcome::Full { .. }
    ));
    pump_all(&mut host, &mut view, &mut tx);
    assert_eq!(view.take_latest_frame().expect("keyframe").pixels, px);
}

/// A viewer that falls behind: more updates than MAX_QUEUED_FRAMES pile
/// up between takes. Deltas can never be skipped, so overflow applies
/// them; older full frames are dropped undecoded; the newest state wins.
#[test]
fn slow_viewer_queue_keeps_the_delta_chain() {
    let (mut host, mut view) = session_pair();
    let (w, h) = (256, 192);
    let mb = FrameMailbox::default();
    let mut tx = PrioritySender::new(4096);
    let mut enc = UpdateEncoder::new();
    enc.set_peer_caps(LOCAL_CAPS);
    let mut px = noise(w, h, 12);
    let mut push = |px: &[u8],
                    enc: &mut UpdateEncoder,
                    host: &mut TerminalSession,
                    view: &mut TerminalSession| {
        enc.submit(px, w, h, None, &mb);
        while tx.has_work(&mb) {
            tx.flush(host, &mb, 64).unwrap();
        }
        for _ in 0..5 {
            view.pump().unwrap();
        }
    };
    push(&px, &mut enc, &mut host, &mut view); // keyframe
    for i in 0..10 {
        px[i * 4 * 70] ^= 0x55;
        push(&px, &mut enc, &mut host, &mut view);
    }
    let got = view.take_latest_frame().expect("frame");
    assert_eq!(got.pixels, px);
    assert!(view.take_latest_frame().is_none(), "nothing newer");
    assert_eq!(view.stats().screen_desyncs, 0);

    // Oldest-first consumer: every update yields its own snapshot.
    let mut want = Vec::new();
    for i in 0..3 {
        px[(i + 1) * 4 * 300] ^= 0x33;
        want.push(px.clone());
        push(&px, &mut enc, &mut host, &mut view);
    }
    for w in want {
        assert_eq!(view.take_frame().expect("queued").pixels, w);
    }
    assert!(view.take_frame().is_none());
}
