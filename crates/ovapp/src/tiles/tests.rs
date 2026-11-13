use super::*;

/// Tiny deterministic PRNG (xorshift) — no dev-dependency needed.
pub(crate) struct Rng(u64);
impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }
    pub(crate) fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    pub(crate) fn below(&mut self, n: u32) -> u32 {
        (self.next() % u64::from(n.max(1))) as u32
    }
}

fn frame(w: u32, h: u32, seed: u64) -> Vec<u8> {
    let mut r = Rng::new(seed);
    (0..w * h * 4).map(|_| r.next() as u8).collect()
}

/// Brute-force reference: tile is dirty iff any pixel in it differs.
fn reference(prev: &[u8], cur: &[u8], w: u32, h: u32) -> TileMap {
    let mut m = TileMap::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            if prev[i..i + 4] != cur[i..i + 4] {
                m.set(x / TILE, y / TILE);
            }
        }
    }
    m
}

#[test]
fn diff_matches_brute_force_on_random_edits() {
    let (w, h) = (300, 200); // partial edge tiles on both axes
    let mut rng = Rng::new(42);
    let prev = frame(w, h, 7);
    for _ in 0..50 {
        let mut cur = prev.clone();
        for _ in 0..rng.below(12) {
            let x = rng.below(w);
            let y = rng.below(h);
            let i = ((y * w + x) * 4 + rng.below(4)) as usize;
            cur[i] = cur[i].wrapping_add(1 + rng.below(250) as u8);
        }
        let mut m = TileMap::default();
        let n = diff_tiles(&prev, &cur, w, h, (w * 4) as usize, 4, None, &mut m);
        let want = reference(&prev, &cur, w, h);
        assert_eq!(m, want);
        assert_eq!(n, want.count());
    }
}

#[test]
fn identical_frames_have_no_dirty_tiles() {
    let px = frame(130, 70, 3);
    let mut m = TileMap::default();
    assert_eq!(
        diff_tiles(&px, &px.clone(), 130, 70, 520, 4, None, &mut m),
        0
    );
}

#[test]
fn hints_restrict_the_compare() {
    let (w, h) = (256, 128);
    let prev = frame(w, h, 1);
    let mut cur = prev.clone();
    cur[0] ^= 0xFF; // tile (0,0)
    let far = ((100 * w + 200) * 4) as usize;
    cur[far] ^= 0xFF; // tile (3,1)
    let mut m = TileMap::default();
    // Hint only covers the top-left tile: the other change is (by
    // contract) assumed not to exist.
    let hint = [TileRect {
        x: 0,
        y: 0,
        w: 10,
        h: 10,
    }];
    assert_eq!(
        diff_tiles(&prev, &cur, w, h, (w * 4) as usize, 4, Some(&hint), &mut m),
        1
    );
    assert!(m.get(0, 0));
    // A hint over an unchanged area finds nothing (OS rects are verified).
    let hint = [TileRect {
        x: 64,
        y: 0,
        w: 64,
        h: 64,
    }];
    assert_eq!(
        diff_tiles(&prev, &cur, w, h, (w * 4) as usize, 4, Some(&hint), &mut m),
        0
    );
    // Empty hint list = nothing changed.
    assert_eq!(
        diff_tiles(&prev, &cur, w, h, (w * 4) as usize, 4, Some(&[]), &mut m),
        0
    );
}

#[test]
fn rects_cover_exactly_the_dirty_tiles_and_merge_runs() {
    let (w, h) = (300, 200);
    let mut m = TileMap::new(w, h);
    // A 3-wide run on rows 0 and 1 (one merged rect), plus a lone edge
    // tile on the partial last column/row.
    for c in 0..3 {
        m.set(c, 0);
        m.set(c, 1);
    }
    m.set(4, 3);
    let mut rects = Vec::new();
    rects_from_map(&m, w, h, &mut rects);
    assert_eq!(
        rects,
        vec![
            TileRect {
                x: 0,
                y: 0,
                w: 192,
                h: 128
            },
            TileRect {
                x: 256,
                y: 192,
                w: 44,
                h: 8
            },
        ]
    );
    // Re-marking the rects reproduces the map.
    let mut back = TileMap::new(w, h);
    rects.iter().for_each(|r| back.mark_rect(r));
    assert_eq!(back, m);
}

#[test]
fn random_maps_round_trip_through_rects() {
    let mut rng = Rng::new(9);
    for _ in 0..100 {
        let (w, h) = (1 + rng.below(700), 1 + rng.below(500));
        let mut m = TileMap::new(w, h);
        for _ in 0..rng.below(40) {
            m.set(rng.below(m.cols()), rng.below(m.rows()));
        }
        let mut rects = Vec::new();
        rects_from_map(&m, w, h, &mut rects);
        let mut back = TileMap::new(w, h);
        rects.iter().for_each(|r| {
            assert!(r.fits(w, h), "{r:?} in {w}x{h}");
            back.mark_rect(r)
        });
        assert_eq!(back, m);
        // Rects never overlap: their areas sum to the dirty pixel area.
        let area: usize = rects.iter().map(TileRect::area).sum();
        let mut px = 0usize;
        for row in 0..m.rows() {
            for col in 0..m.cols() {
                if m.get(col, row) {
                    let tw = (w - col * TILE).min(TILE) as usize;
                    let th = (h - row * TILE).min(TILE) as usize;
                    px += tw * th;
                }
            }
        }
        assert_eq!(area, px);
    }
}

#[test]
fn gather_scatter_patches_the_framebuffer() {
    let (w, h) = (200, 150);
    let prev = frame(w, h, 5);
    let mut cur = prev.clone();
    let mut rng = Rng::new(77);
    for _ in 0..30 {
        let i = rng.below(w * h * 4) as usize;
        cur[i] ^= 0x5A;
    }
    let mut m = TileMap::default();
    diff_tiles(&prev, &cur, w, h, (w * 4) as usize, 4, None, &mut m);
    let mut rects = Vec::new();
    rects_from_map(&m, w, h, &mut rects);
    let mut payload = Vec::new();
    gather(&cur, (w * 4) as usize, 4, &rects, &mut payload);
    let mut fb = prev.clone();
    scatter(&mut fb, w, h, (w * 4) as usize, 4, &rects, &payload).unwrap();
    assert_eq!(fb, cur);
    // Bad payload length / out-of-bounds rect: rejected, fb untouched.
    let before = fb.clone();
    assert!(scatter(&mut fb, w, h, (w * 4) as usize, 4, &rects, &payload[1..]).is_err());
    let oob = [TileRect {
        x: 190,
        y: 0,
        w: 20,
        h: 1,
    }];
    assert!(scatter(&mut fb, w, h, (w * 4) as usize, 4, &oob, &[0; 80]).is_err());
    assert_eq!(fb, before);
}

#[test]
fn scroll_dirties_most_tiles() {
    let (w, h) = (640, 480);
    let prev = frame(w, h, 11);
    // Scroll up by 40 px: every row now shows content from 40 rows down.
    let stride = (w * 4) as usize;
    let mut cur = vec![0u8; prev.len()];
    cur[..(h as usize - 40) * stride].copy_from_slice(&prev[40 * stride..]);
    let mut m = TileMap::default();
    diff_tiles(&prev, &cur, w, h, stride, 4, None, &mut m);
    assert!(m.mostly_dirty(), "{} of {}", m.count(), m.len());
    let mut rects = Vec::new();
    rects_from_map(&m, w, h, &mut rects);
    assert_eq!(rects.len(), 1, "whole-screen dirt merges into one rect");
}

fn meta_full(w: u32, h: u32, len: usize) -> UpdateMeta {
    UpdateMeta {
        width: w,
        height: h,
        stride: w * 4,
        format: PixelFormat::Bgra,
        total: w * h * 4,
        codec: FrameCodec::Raw,
        compressed_len: len as u32,
        delta: None,
    }
}

#[test]
fn decoder_applies_full_then_deltas_and_rejects_a_broken_chain() {
    let (w, h) = (130, 90);
    let f0 = frame(w, h, 1);
    let mut d = ScreenDecoder::new();
    // A delta before any keyframe is a desync.
    let early = UpdateMeta {
        delta: Some(Delta {
            base_seq: 0,
            rects: vec![],
        }),
        total: 0,
        ..meta_full(w, h, 0)
    };
    assert_eq!(d.apply(&early, 1, &[]), Err(ApplyError::Desync));
    d.apply(&meta_full(w, h, f0.len()), 10, &f0).unwrap();
    assert_eq!(d.frame().unwrap().pixels, f0);
    // Delta 10 -> 11.
    let mut f1 = f0.clone();
    f1[4 * 70] = 1;
    f1[(w * 4 * 80) as usize] = 2;
    let mut m = TileMap::default();
    diff_tiles(&f0, &f1, w, h, (w * 4) as usize, 4, None, &mut m);
    let mut rects = Vec::new();
    rects_from_map(&m, w, h, &mut rects);
    let mut payload = Vec::new();
    gather(&f1, (w * 4) as usize, 4, &rects, &mut payload);
    let dm = UpdateMeta {
        total: payload.len() as u32,
        compressed_len: payload.len() as u32,
        delta: Some(Delta {
            base_seq: 10,
            rects: rects.clone(),
        }),
        ..meta_full(w, h, 0)
    };
    d.apply(&dm, 11, &payload).unwrap();
    assert_eq!(d.frame().unwrap().pixels, f1);
    assert_eq!(d.seq(), Some(11));
    assert_eq!(d.last_dirty(), &rects[..]);
    // The same delta again: its base (10) is no longer what we hold.
    assert_eq!(d.apply(&dm, 12, &payload), Err(ApplyError::Desync));
    // Corrupt length: rejected, framebuffer untouched.
    let bad = UpdateMeta {
        delta: Some(Delta {
            base_seq: 11,
            rects,
        }),
        ..dm.clone()
    };
    assert!(matches!(
        d.apply(&bad, 12, &payload[1..]),
        Err(ApplyError::Corrupt(_))
    ));
    assert_eq!(d.frame().unwrap().pixels, f1);
    assert_eq!(d.seq(), Some(11));
}
