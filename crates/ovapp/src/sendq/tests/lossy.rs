//! Lossy-first image tiles, the refinement lane and the tile cache.

use super::*;

use crate::bands::{FLAG_CACHE, FLAG_LOSSY};
use crate::terminal::{CAP_CACHE, CAP_LOSSY, VIEWER_W2_CAPS};

const LOSSY_CACHE_CAPS: u32 = LOCAL_CAPS | VIEWER_W2_CAPS;

/// A desktop-ish screen: text everywhere, a photo window at `photo_at`.
fn desk(w: u32, h: u32, photo_at: Option<TileRect>, seed: u64) -> Vec<u8> {
    let mut px = crate::lossy::tests::text(w, h, seed);
    if let Some(r) = photo_at {
        paste_photo(&mut px, w, r, seed ^ 0x55);
    }
    px
}

fn paste_photo(px: &mut [u8], w: u32, r: TileRect, seed: u64) {
    let p = crate::lossy::tests::photo(u32::from(r.w), u32::from(r.h), seed);
    let stride = w as usize * 4;
    let rw = usize::from(r.w) * 4;
    for y in 0..usize::from(r.h) {
        let d = (usize::from(r.y) + y) * stride + usize::from(r.x) * 4;
        px[d..d + rw].copy_from_slice(&p[y * rw..(y + 1) * rw]);
    }
}

/// Container flags of an update (0 for a non-container codec).
fn flags(f: &EncodedFrame) -> u8 {
    if f.codec == FrameCodec::Packed {
        f.data[1]
    } else {
        0
    }
}

/// Run the refine lane dry (each refine update taken and applied):
/// (updates, largest update bytes).
fn drain_refine(
    enc: &mut UpdateEncoder,
    mb: &FrameMailbox,
    dec: &mut ScreenDecoder,
) -> (usize, usize) {
    let (mut n, mut max) = (0, 0);
    while enc.submit_refine(mb) != SubmitOutcome::Unchanged {
        let f = mb.take().expect("refine update queued");
        max = max.max(f.data.len());
        apply(dec, &f);
        n += 1;
        assert!(n < 2000, "refine lane never drains");
    }
    (n, max)
}

fn tile_eq(a: &[u8], b: &[u8], w: u32, h: u32, c: u32, r: u32) -> bool {
    let tr = crate::lossy::tile_rect(c, r, w, h);
    let stride = w as usize * 4;
    (usize::from(tr.y)..usize::from(tr.y + tr.h)).all(|y| {
        let o = y * stride + usize::from(tr.x) * 4;
        a[o..o + usize::from(tr.w) * 4] == b[o..o + usize::from(tr.w) * 4]
    })
}

fn dirty_covers(dirty: &[TileRect], w: u32, h: u32, c: u32, r: u32) -> bool {
    let tr = crate::lossy::tile_rect(c, r, w, h);
    dirty
        .iter()
        .any(|d| d.x <= tr.x && d.y <= tr.y && d.x + d.w >= tr.x + tr.w && d.y + d.h >= tr.y + tr.h)
}

/// Photo tiles go out lossy first (a fraction of the
/// lossless bytes), text stays exact, and once the photo settled the
/// refine lane converges to the exact pixels in small chunks.
#[test]
fn lossy_first_then_refined_to_exact_pixels() {
    let (w, h) = (640u32, 384u32);
    let photo = TileRect {
        x: 128,
        y: 64,
        w: 320,
        h: 256,
    }; // 5x4 tiles
    let px = desk(w, h, Some(photo), 1);
    let t0 = Instant::now();
    // Lossless reference (a viewer without the lossy / cache CAPS).
    let lossless_bytes = {
        let mb = FrameMailbox::default();
        let mut enc = UpdateEncoder::new();
        enc.set_peer_caps(LOCAL_CAPS);
        match enc.submit(&px, w, h, None, &mb) {
            SubmitOutcome::Full { bytes } => bytes,
            o => panic!("{o:?}"),
        }
    };
    let mb = FrameMailbox::default();
    let mut enc = UpdateEncoder::new();
    let mut dec = ScreenDecoder::new();
    enc.set_peer_caps(LOSSY_CACHE_CAPS);
    enc.set_clock(Some(t0));
    let first = match enc.submit(&px, w, h, None, &mb) {
        SubmitOutcome::Full { bytes } => bytes,
        o => panic!("{o:?}"),
    };
    let f = mb.take().unwrap();
    assert_ne!(flags(&f) & FLAG_LOSSY, 0);
    assert_ne!(flags(&f) & FLAG_CACHE, 0);
    apply(&mut dec, &f);
    assert!(
        first * 3 < lossless_bytes,
        "lossy first {first} vs lossless {lossless_bytes}"
    );
    assert_eq!(dec.lossy_tiles().count(), 20);
    assert_eq!(enc.refresh_pending(), 20);
    assert_eq!(enc.stats().lossy_tiles, 20);
    let fb = dec.frame().unwrap().pixels.clone();
    for r in 0..h / 64 {
        for c in 0..w / 64 {
            let in_photo = (2..7).contains(&c) && (1..5).contains(&r);
            assert_eq!(tile_eq(&fb, &px, w, h, c, r), !in_photo, "tile ({c},{r})");
        }
    }
    // Text tiles went into the cache, lossy ones did not.
    assert_eq!(dec.cache_slots_filled(), (w / 64 * (h / 64)) as usize - 20);
    // Not settled yet: nothing to refine.
    enc.set_clock(Some(t0 + Duration::from_millis(200)));
    assert!(!enc.refine_due());
    assert_eq!(enc.submit_refine(&mb), SubmitOutcome::Unchanged);
    assert_eq!(enc.submit(&px, w, h, None, &mb), SubmitOutcome::Unchanged);
    // Settled: refined in chunks, to the exact pixels.
    enc.set_clock(Some(t0 + Duration::from_millis(350)));
    enc.set_link_rate(1_250_000); // 10 Mbit/s: ~19 KB chunks
    assert!(enc.refine_due());
    let (n, max) = drain_refine(&mut enc, &mb, &mut dec);
    assert!(n >= 3, "chunked: {n}");
    assert!(max <= 2 * REFRESH_CHUNK_BYTES, "chunk {max}");
    assert_eq!(dec.frame().unwrap().pixels, px);
    assert_eq!(dec.lossy_tiles().count(), 0);
    assert_eq!(enc.refresh_pending(), 0);
    assert_eq!(enc.stats().refined_tiles, 20);
    // Refined tiles are exact now: cached.
    assert_eq!(dec.cache_slots_filled(), (w / 64 * (h / 64)) as usize);
    assert_eq!(enc.submit(&px, w, h, None, &mb), SubmitOutcome::Unchanged);
}

/// A tile that changes again while its refinement waits unsent is sent
/// lossy again (the refinement is superseded), and its own clock
/// restarts.
#[test]
fn refinement_is_cancelled_when_the_tile_changes_again() {
    let (w, h) = (640u32, 384u32);
    let photo = TileRect {
        x: 128,
        y: 64,
        w: 320,
        h: 256,
    };
    let mut px = desk(w, h, Some(photo), 1);
    let t0 = Instant::now();
    let mb = FrameMailbox::default();
    let mut enc = UpdateEncoder::new();
    let mut dec = ScreenDecoder::new();
    enc.set_peer_caps(LOSSY_CACHE_CAPS);
    enc.set_clock(Some(t0));
    enc.submit(&px, w, h, None, &mb);
    apply(&mut dec, &mb.take().unwrap());
    // Settled: a refine chunk is queued but the link has not taken it.
    enc.set_clock(Some(t0 + Duration::from_millis(350)));
    assert!(matches!(
        enc.submit_refine(&mb),
        SubmitOutcome::Delta { .. }
    ));
    assert!(mb.is_pending());
    // The photo changes (a new picture) before the chunk went out.
    paste_photo(&mut px, w, photo, 99);
    enc.set_clock(Some(t0 + Duration::from_millis(360)));
    let replaced_before = mb.replaced_count();
    assert!(matches!(
        enc.submit(&px, w, h, None, &mb),
        SubmitOutcome::Delta { .. }
    ));
    assert_eq!(
        mb.replaced_count(),
        replaced_before + 1,
        "the refine chunk was superseded"
    );
    let f = mb.take().unwrap();
    assert_ne!(flags(&f) & FLAG_LOSSY, 0, "changed tiles go lossy again");
    // No photo tile travels losslessly in it.
    for r in &f.delta.as_ref().unwrap().rects {
        let inter = r.x < photo.x + photo.w
            && photo.x < r.x + r.w
            && r.y < photo.y + photo.h
            && photo.y < r.y + r.h;
        assert!(!inter, "lossless rect {r:?} inside the changed photo");
    }
    apply(&mut dec, &f);
    assert_eq!(dec.lossy_tiles().count(), 20);
    for c in 2..7 {
        for r in 1..5 {
            assert!(
                dirty_covers(dec.last_dirty(), w, h, c, r),
                "lossy tile ({c},{r}) reported dirty"
            );
        }
    }
    // Its clock restarted at 360 ms: not due at 600 ms, due at 700 ms.
    enc.set_clock(Some(t0 + Duration::from_millis(600)));
    assert!(!enc.refine_due());
    enc.set_clock(Some(t0 + Duration::from_millis(700)));
    drain_refine(&mut enc, &mb, &mut dec);
    assert_eq!(dec.frame().unwrap().pixels, px);
    assert_eq!(dec.lossy_tiles().count(), 0);
}

/// The host's cache model and the viewer's slots stay identical over
/// many random updates — screens switching back and forth, edits,
/// photos, refinement, a tiny cache (constant eviction), and the
/// mailbox dropping unsent updates — and every tile the host believes
/// exact is exact on the viewer.
#[test]
fn cache_and_lossy_state_stay_in_sync_over_random_updates() {
    let (w, h) = (384u32, 256u32); // 6x4 = 24 tiles
    let screens: Vec<Vec<u8>> = (0..4)
        .map(|i| {
            desk(
                w,
                h,
                (i % 2 == 1).then_some(TileRect {
                    x: 64,
                    y: 64,
                    w: 192,
                    h: 128,
                }),
                10 + i,
            )
        })
        .collect();
    let mb = FrameMailbox::default();
    let mut enc = UpdateEncoder::new();
    let mut dec = ScreenDecoder::new();
    enc.set_peer_caps(LOSSY_CACHE_CAPS);
    enc.set_cache_slots(16);
    let t0 = Instant::now();
    let mut t = Duration::ZERO;
    enc.set_clock(Some(t0));
    let mut rng = Rng(0xBADC0DE);
    let mut px = screens[0].clone();
    let mut captured = std::collections::HashMap::new();
    let mut applied = 0;
    for step in 0..1500 {
        match rng.below(10) {
            0..=2 => px.copy_from_slice(&screens[rng.below(4) as usize]),
            3..=5 => mutate(&mut px, w, h, &mut rng),
            6 => {
                let r = TileRect {
                    x: 64 * rng.below(4) as u16,
                    y: 64 * rng.below(2) as u16,
                    w: 128,
                    h: 128,
                };
                paste_photo(&mut px, w, r, u64::from(rng.below(5)));
            }
            _ => {}
        }
        t += Duration::from_millis(u64::from(rng.below(120)));
        enc.set_clock(Some(t0 + t));
        let seq = enc.next_seq();
        if enc.submit(&px, w, h, None, &mb) != SubmitOutcome::Unchanged {
            captured.insert(seq, px.clone());
        }
        if rng.below(4) == 0 {
            let seq = enc.next_seq();
            if enc.submit_refine(&mb) != SubmitOutcome::Unchanged {
                captured.insert(seq, enc.last_px.clone());
            }
        }
        if rng.below(3) == 0 || step % 40 == 39 {
            if let Some(f) = mb.take() {
                apply(&mut dec, &f);
                applied += 1;
                // Cache sync: the host's model of this update vs the
                // viewer's slots.
                let model = enc.cache_tentative.as_ref().unwrap_or(&enc.cache_committed);
                for s in 0..model.capacity() as u16 {
                    if let Some(k) = model.slot_key(s) {
                        assert_eq!(
                            dec.cache_slot_hash(s),
                            Some(k),
                            "slot {s} after update {}",
                            f.seq
                        );
                    }
                }
                // Exactness: tiles outside the host's backlog are exact.
                let want = &captured[&f.seq];
                let fb = &dec.frame().unwrap().pixels;
                for r in 0..enc.backlog.rows() {
                    for c in 0..enc.backlog.cols() {
                        if !enc.backlog.get(c, r) {
                            assert!(
                                tile_eq(fb, want, w, h, c, r),
                                "tile ({c},{r}) after update {}",
                                f.seq
                            );
                        }
                    }
                }
            }
        }
    }
    let st = enc.stats();
    assert!(applied > 300, "{applied}");
    assert!(st.cache_refs > 100 && st.cache_stores > 100, "{st:?}");
    assert!(st.lossy_tiles > 50 && st.refined_tiles > 20, "{st:?}");
    // Settle: everything converges to exact.
    if let Some(f) = mb.take() {
        apply(&mut dec, &f);
    }
    enc.set_clock(Some(t0 + t + Duration::from_secs(1)));
    if enc.submit(&px, w, h, None, &mb) != SubmitOutcome::Unchanged {
        apply(&mut dec, &mb.take().unwrap());
    }
    enc.set_clock(Some(t0 + t + Duration::from_secs(2)));
    drain_refine(&mut enc, &mb, &mut dec);
    assert_eq!(dec.frame().unwrap().pixels, px);
    assert_eq!(enc.refresh_pending(), 0);
}

/// Window switching: going back to a screen the viewer has seen costs
/// cache REFs (a few bytes per tile) instead of pixels.
#[test]
fn switching_back_to_a_seen_window_is_sent_as_cache_refs() {
    let (w, h) = (1280u32, 768u32);
    let a = desk(w, h, None, 1);
    let b = desk(w, h, None, 2);
    let run = |caps: u32| -> (usize, ScreenDecoder) {
        let mb = FrameMailbox::default();
        let mut enc = UpdateEncoder::new();
        let mut dec = ScreenDecoder::new();
        enc.set_peer_caps(caps);
        let mut last = 0;
        for px in [&a, &b, &a] {
            last = match enc.submit(px, w, h, None, &mb) {
                SubmitOutcome::Full { bytes } | SubmitOutcome::Delta { bytes, .. } => bytes,
                o => panic!("{o:?}"),
            };
            apply(&mut dec, &mb.take().unwrap());
        }
        assert_eq!(dec.frame().unwrap().pixels, a);
        (last, dec)
    };
    let (with_cache, dec) = run(LOSSY_CACHE_CAPS);
    let (without, _) = run(LOCAL_CAPS);
    assert!(
        with_cache * 20 < without,
        "back to A: {with_cache} B with the cache vs {without} without"
    );
    // Every tile of the switch is reported dirty (the Metal path uploads it).
    for r in 0..h / 64 {
        for c in 0..w / 64 {
            assert!(dirty_covers(dec.last_dirty(), w, h, c, r));
        }
    }
}

/// CAPS interop: lossy / cache sections only reach a viewer that advertised
/// them (and CAP_BANDS); every combination still ends exact.
#[test]
fn lossy_and_cache_sections_only_for_viewers_that_advertised_them() {
    let (w, h) = (512u32, 320u32);
    let photo = TileRect {
        x: 64,
        y: 64,
        w: 256,
        h: 192,
    };
    let a = desk(w, h, Some(photo), 3);
    let b = desk(w, h, None, 4);
    for caps in [
        LOCAL_CAPS,
        LOCAL_CAPS | CAP_LOSSY,
        LOCAL_CAPS | CAP_CACHE,
        LOSSY_CACHE_CAPS,
        LOSSY_CACHE_CAPS & !CAP_BANDS,
        CAP_TILES | CAP_KEYFRAME | VIEWER_W2_CAPS,
        0,
    ] {
        let mb = FrameMailbox::default();
        let mut enc = UpdateEncoder::new();
        let mut dec = ScreenDecoder::new();
        enc.set_peer_caps(caps);
        let t0 = Instant::now();
        enc.set_clock(Some(t0));
        let mut seen = 0u8;
        for (i, px) in [&a, &b, &a].into_iter().enumerate() {
            enc.set_clock(Some(t0 + Duration::from_millis(10 * i as u64)));
            if enc.submit(px, w, h, None, &mb) != SubmitOutcome::Unchanged {
                let f = mb.take().unwrap();
                seen |= flags(&f);
                apply(&mut dec, &f);
            }
        }
        enc.set_clock(Some(t0 + Duration::from_secs(1)));
        while enc.submit_refine(&mb) != SubmitOutcome::Unchanged {
            let f = mb.take().unwrap();
            seen |= flags(&f);
            apply(&mut dec, &f);
        }
        let bands = caps & CAP_BANDS != 0;
        assert_eq!(
            seen & FLAG_LOSSY != 0,
            bands && caps & CAP_LOSSY != 0,
            "caps {caps:#x}"
        );
        assert_eq!(
            seen & FLAG_CACHE != 0,
            bands && caps & CAP_CACHE != 0,
            "caps {caps:#x}"
        );
        assert_eq!(dec.frame().unwrap().pixels, a, "caps {caps:#x}");
    }
}

/// The post-H.264 refresh rides the refine lane for any tile viewer:
/// the region drains in chunks after VIDEO_END.
#[test]
fn video_region_is_refined_through_the_refine_lane() {
    let (w, h) = (640u32, 384u32);
    let px = desk(w, h, None, 5);
    let mb = FrameMailbox::default();
    let mut enc = UpdateEncoder::new();
    let mut dec = ScreenDecoder::new();
    enc.set_peer_caps(LOSSY_CACHE_CAPS);
    enc.submit(&px, w, h, None, &mb);
    apply(&mut dec, &mb.take().unwrap());
    // The viewer painted video over a region (lossy pixels).
    let region = TileRect {
        x: 64,
        y: 64,
        w: 448,
        h: 256,
    };
    let video: Vec<u8> = vec![77; region.area() * 4];
    assert!(dec.blit(region, w, h, &video));
    assert_eq!(dec.lossy_tiles().count(), 28);
    enc.queue_refresh(region);
    assert!(enc.refine_due(), "a video region is due at once");
    let (n, _) = drain_refine(&mut enc, &mb, &mut dec);
    assert!(n >= 1);
    assert_eq!(dec.frame().unwrap().pixels, px);
    assert_eq!(dec.lossy_tiles().count(), 0);
}
