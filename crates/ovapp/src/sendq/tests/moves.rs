//! Scroll / OS-move MOVE records, the lossless refresh backlog and LZ4 band choice.

use super::*;

/// A "document": unique text-like rows.
fn doc(w: u32, h: u32, seed: u32) -> Vec<u8> {
    let mut px = vec![255u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let o = ((y * w + x) * 4) as usize;
            if (x / 6 + y / 3) % 5 != 0 {
                let v = ((x.wrapping_mul(7) ^ (y + seed).wrapping_mul(131)) % 251) as u8;
                px[o] = v;
                px[o + 1] = v.wrapping_mul(3);
                px[o + 2] = (y % 256) as u8;
            }
        }
    }
    px
}

/// Scroll `win` of `px` up by `dy`, new content in the exposed strip.
fn scroll_up(px: &[u8], w: u32, win: TileRect, dy: usize, t: u32) -> Vec<u8> {
    let stride = w as usize * 4;
    let mut cur = px.to_vec();
    let (x0, x1) = (usize::from(win.x) * 4, usize::from(win.x + win.w) * 4);
    let (y0, y1) = (usize::from(win.y), usize::from(win.y + win.h));
    for y in y0..y1 - dy {
        let src = px[(y + dy) * stride + x0..(y + dy) * stride + x1].to_vec();
        cur[y * stride + x0..y * stride + x1].copy_from_slice(&src);
    }
    for y in y1 - dy..y1 {
        for x in (x0..x1).step_by(4) {
            cur[y * stride + x] = (x as u32 * 13 + y as u32 * 7 + t) as u8;
        }
    }
    cur
}

/// Copy-rect end to end: scroll steps become MOVE + strip, the
/// decoder reproduces every frame exactly, the bytes collapse, and the
/// scroll does not register as motion (`fresh` = residual only).
#[test]
fn scrolls_are_sent_as_moves_and_decode_exactly() {
    let (w, h) = (1280u32, 720u32);
    let win = TileRect {
        x: 180,
        y: 100,
        w: 900,
        h: 560,
    };
    let mut px = doc(w, h, 0);
    let mb = FrameMailbox::default();
    let mut dec = ScreenDecoder::new();
    // Same steps for a viewer without CAP_MOVE (the baseline bytes).
    let mut old = UpdateEncoder::new();
    old.set_peer_caps(LOCAL_CAPS & !crate::terminal::CAP_MOVE & !CAP_BANDS);
    let mbo = FrameMailbox::default();
    let mut enc = UpdateEncoder::new();
    enc.set_peer_caps(LOCAL_CAPS);
    enc.submit(&px, w, h, None, &mb);
    apply(&mut dec, &mb.take().unwrap());
    old.submit(&px, w, h, None, &mbo);
    mbo.take();
    let (mut with, mut without) = (0usize, 0usize);
    for step in 0..6 {
        px = scroll_up(&px, w, win, 24, step);
        match enc.submit(&px, w, h, None, &mb) {
            SubmitOutcome::Delta { bytes, .. } => with += bytes,
            other => panic!("step {step}: {other:?}"),
        }
        assert!(
            enc.fresh().count() <= 2 * 15,
            "residual only: {}",
            enc.fresh().count()
        );
        let f = mb.take().unwrap();
        assert_eq!(f.codec, FrameCodec::Packed);
        apply(&mut dec, &f);
        assert_eq!(dec.frame().unwrap().pixels, px, "step {step}");
        assert!(
            dec.last_dirty().iter().any(|r| r.w == 900),
            "moved rect reported dirty"
        );
        if let SubmitOutcome::Delta { bytes, .. } | SubmitOutcome::Full { bytes } =
            old.submit(&px, w, h, None, &mbo)
        {
            without += bytes;
        }
        mbo.take();
    }
    assert_eq!(enc.stats().moves, 6);
    eprintln!("6 scroll steps: {with} bytes with MOVE vs {without} without");
    assert!(with * 5 < without, "{with} vs {without}");
}

/// A move update replaced in the mailbox before it was sent: the next
/// update (against the older base) still reconstructs exactly.
#[test]
fn a_replaced_move_update_is_recovered_by_the_next_delta() {
    let (w, h) = (1024u32, 640u32);
    let win = TileRect {
        x: 64,
        y: 64,
        w: 800,
        h: 512,
    };
    let mut px = doc(w, h, 3);
    let mb = FrameMailbox::default();
    let mut dec = ScreenDecoder::new();
    let mut enc = UpdateEncoder::new();
    enc.set_peer_caps(LOCAL_CAPS);
    enc.submit(&px, w, h, None, &mb);
    apply(&mut dec, &mb.take().unwrap());
    px = scroll_up(&px, w, win, 30, 1);
    enc.submit(&px, w, h, None, &mb); // a MOVE update, never taken
    assert_eq!(enc.stats().moves, 1);
    px = scroll_up(&px, w, win, 30, 2);
    enc.submit(&px, w, h, None, &mb); // replaces it: no move (not chained)
    apply(&mut dec, &mb.take().unwrap());
    assert_eq!(dec.frame().unwrap().pixels, px);
    // And chaining resumes with moves.
    px = scroll_up(&px, w, win, 30, 3);
    enc.submit(&px, w, h, None, &mb);
    apply(&mut dec, &mb.take().unwrap());
    assert_eq!(dec.frame().unwrap().pixels, px);
    assert_eq!(enc.stats().moves, 2);
}

/// OS move rects (DXGI) are verified against the mirror: a correct one
/// is used, a wrong one is ignored (the detector still finds it).
#[test]
fn os_move_rects_are_verified_before_use() {
    let (w, h) = (800u32, 600u32);
    let win = TileRect {
        x: 0,
        y: 0,
        w: 800,
        h: 600,
    };
    let mut px = doc(w, h, 9);
    let mb = FrameMailbox::default();
    let mut dec = ScreenDecoder::new();
    let mut enc = UpdateEncoder::new();
    enc.set_peer_caps(LOCAL_CAPS);
    enc.submit(&px, w, h, None, &mb);
    apply(&mut dec, &mb.take().unwrap());
    px = scroll_up(&px, w, win, 20, 1);
    let good = MoveRect {
        x: 0,
        y: 0,
        w: 800,
        h: 580,
        dx: 0,
        dy: -20,
    };
    enc.submit_ex(&px, w, h, None, Some(&[good]), None, None, &mb);
    apply(&mut dec, &mb.take().unwrap());
    assert_eq!(dec.frame().unwrap().pixels, px);
    assert_eq!((enc.stats().moves, enc.stats().detected_moves), (1, 0));
    px = scroll_up(&px, w, win, 20, 2);
    let wrong = MoveRect { dy: -7, ..good };
    enc.submit_ex(&px, w, h, None, Some(&[wrong]), None, None, &mb);
    apply(&mut dec, &mb.take().unwrap());
    assert_eq!(dec.frame().unwrap().pixels, px);
    assert_eq!((enc.stats().moves, enc.stats().detected_moves), (2, 1));
}

/// The post-motion refresh drains in bounded chunks, each
/// update stays small, and the result is exact.
#[test]
fn refresh_backlog_drains_in_small_chunks() {
    let (w, h) = (1024u32, 768u32);
    let px = noise(w, h, 31);
    let mb = FrameMailbox::default();
    let mut dec = ScreenDecoder::new();
    let mut enc = UpdateEncoder::new();
    enc.set_peer_caps(LOCAL_CAPS);
    enc.submit(&px, w, h, None, &mb);
    apply(&mut dec, &mb.take().unwrap());
    // Pretend video painted the region lossy on the viewer.
    let region = TileRect {
        x: 128,
        y: 128,
        w: 640,
        h: 448,
    };
    let stride = w as usize * 4;
    let mut lossy = dec.frame().unwrap().pixels.clone();
    for y in 128..576usize {
        for b in &mut lossy[y * stride + 128 * 4..y * stride + 768 * 4] {
            *b ^= 1;
        }
    }
    dec.blit(
        TileRect {
            x: 0,
            y: 0,
            w: w as u16,
            h: h as u16,
        },
        w,
        h,
        &lossy,
    );
    enc.queue_refresh(region);
    assert_eq!(enc.refresh_pending(), 70);
    let mut updates = 0;
    while enc.refresh_pending() > 0 {
        match enc.submit(&px, w, h, Some(&[]), &mb) {
            SubmitOutcome::Delta { tiles, .. } => assert!(tiles <= 64, "{tiles}"),
            other => panic!("{other:?}"),
        }
        let f = mb.take().unwrap();
        assert!(
            f.data.len() <= 2 * REFRESH_CHUNK_BYTES + 4096 || updates == 0,
            "{}",
            f.data.len()
        );
        apply(&mut dec, &f);
        updates += 1;
        assert!(updates < 100);
    }
    assert!(updates >= 2, "chunked: {updates}");
    assert_eq!(dec.frame().unwrap().pixels, px);
    assert_eq!(
        enc.submit(&px, w, h, Some(&[]), &mb),
        SubmitOutcome::Unchanged
    );
}

/// Photo-like (incompressible) content goes out as LZ4/raw
/// bands only on a fast link; a slow or unknown link gets zstd.
#[cfg(feature = "zstd-encode")]
#[test]
fn lz4_bands_only_on_a_fast_link() {
    let (w, h) = (512u32, 512u32);
    // Photo-like: a gradient plus fine noise (zstd ~1.2x, LZ4 ~1x).
    let px: Vec<u8> = noise(w, h, 77)
        .iter()
        .enumerate()
        .map(|(i, n)| {
            if i % 4 == 3 {
                255
            } else {
                ((i / 4) % 512 / 4) as u8 / 2 + (n & 15)
            }
        })
        .collect();
    for (rate, want_lz4) in [(0u64, false), (1_000_000, false), (50_000_000, true)] {
        let mb = FrameMailbox::default();
        let mut enc = UpdateEncoder::new();
        enc.set_peer_caps(LOCAL_CAPS);
        enc.set_link_rate(rate);
        enc.submit(&px, w, h, None, &mb);
        let f = mb.take().unwrap();
        let mut dec = ScreenDecoder::new();
        apply(&mut dec, &f);
        assert_eq!(dec.frame().unwrap().pixels, px);
        let st = enc.stats();
        assert_eq!(st.zstd_bands == 0, want_lz4, "rate {rate}: {st:?}");
    }
}
