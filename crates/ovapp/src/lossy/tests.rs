use super::*;

pub(crate) fn xorshift(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

/// Synthetic photo: smooth gradients + sine + per-pixel noise (the
/// benchmarks' "photo region").
pub(crate) fn photo(w: u32, h: u32, seed: u64) -> Vec<u8> {
    let mut s = seed | 1;
    let mut v = vec![255u8; (w * h * 4) as usize];
    for y in 0..h as usize {
        for x in 0..w as usize {
            let n = (xorshift(&mut s) >> 58) as i32 - 32;
            let o = (y * w as usize + x) * 4;
            let base = [
                ((x * 7 + y * 3) / 9 % 256) as i32,
                ((x * 2 + y * 5) / 8 % 256) as i32,
                ((x as f32 / 37.0).sin() * 60.0 + 128.0) as i32 + (y / 9) as i32 % 64,
            ];
            for c in 0..3 {
                v[o + c] = (base[c] + n / 3).clamp(0, 255) as u8;
            }
        }
    }
    v
}

/// Synthetic UI text: a light background with dark anti-aliased glyph
/// blobs (a few grey levels), lines of "words".
pub(crate) fn text(w: u32, h: u32, seed: u64) -> Vec<u8> {
    let mut s = seed | 1;
    let mut v = vec![0u8; (w * h * 4) as usize];
    let (px, _) = v.as_chunks_mut::<4>();
    for p in px {
        p.copy_from_slice(&[0xF3, 0xF3, 0xF3, 0xFF]);
    }
    let stride = w as usize * 4;
    let mut y = 4usize;
    while y + 12 < h as usize {
        let mut x = 4usize;
        while x + 8 < w as usize {
            if !xorshift(&mut s).is_multiple_of(5) {
                // a 6x10 glyph: random strokes in 3 grey levels
                for gy in 0..10 {
                    for gx in 0..6 {
                        let r = xorshift(&mut s) % 4;
                        if r > 1 {
                            let g = [0x20u8, 0x60, 0xA0][(r % 3) as usize];
                            let o = (y + gy) * stride + (x + gx) * 4;
                            v[o..o + 3].copy_from_slice(&[g, g, g]);
                        }
                    }
                }
            }
            x += 7;
        }
        y += 16;
    }
    v
}

#[test]
fn classifier_tells_text_from_photo() {
    let (w, h) = (512u32, 256u32);
    let stride = w as usize * 4;
    let mut all = TileMap::new(w, h);
    all.fill();
    let mut out = TileMap::default();
    let p = photo(w, h, 3);
    assert_eq!(
        classify_tiles(&p, w, h, stride, &all, &mut out),
        all.len(),
        "photo: every tile image"
    );
    let t = text(w, h, 5);
    assert_eq!(
        classify_tiles(&t, w, h, stride, &all, &mut out),
        0,
        "text: no image tile"
    );
    // Flat fill and a 2-colour dither are not images.
    let flat = vec![0x40u8; (w * h * 4) as usize];
    assert_eq!(classify_tiles(&flat, w, h, stride, &all, &mut out), 0);
    let dither: Vec<u8> = (0..w * h)
        .flat_map(|i| {
            if (i + i / w) % 2 == 0 {
                [0, 0, 0, 255]
            } else {
                [255; 4]
            }
        })
        .collect();
    assert_eq!(classify_tiles(&dither, w, h, stride, &all, &mut out), 0);
    // Mixed: photo left half, text right half -> only left tiles.
    let mut mixed = t.clone();
    for y in 0..h as usize {
        mixed[y * stride..y * stride + stride / 2]
            .copy_from_slice(&p[y * stride..y * stride + stride / 2]);
    }
    classify_tiles(&mixed, w, h, stride, &all, &mut out);
    for r in 0..out.rows() {
        for c in 0..out.cols() {
            assert_eq!(out.get(c, r), c < 4, "tile ({c},{r})");
        }
    }
    // A photo window's edge: 40 % of a tile photo, the rest text ->
    // image (sending its photo part lossless would cost more).
    let mut edge = t.clone();
    for y in 64..128usize {
        edge[y * stride + 64 * 4..y * stride + 90 * 4]
            .copy_from_slice(&p[y * stride + 64 * 4..y * stride + 90 * 4]);
    }
    let mut one = TileMap::new(w, h);
    one.set(1, 1);
    assert_eq!(
        classify_tiles(&edge, w, h, stride, &one, &mut out),
        1,
        "photo edge tile"
    );
    // Only candidates are classified.
    let mut cand = TileMap::new(w, h);
    cand.set(1, 1);
    assert_eq!(classify_tiles(&p, w, h, stride, &cand, &mut out), 1);
    assert!(out.get(1, 1));
}

#[test]
fn hash_distinguishes_tiles_and_ignores_outside_pixels() {
    let (w, h) = (256u32, 128u32);
    let stride = w as usize * 4;
    let mut p = photo(w, h, 9);
    let a = tile_hash(&p, stride, 0, 0);
    let b = tile_hash(&p, stride, 64, 0);
    assert_ne!(a, b);
    // A change outside the tile: same hash; one bit inside: different.
    p[100 * 4] ^= 1;
    assert_eq!(tile_hash(&p, stride, 0, 0), a);
    p[stride * 63 + 63 * 4 + 2] ^= 0x80;
    assert_ne!(tile_hash(&p, stride, 0, 0), a);
    // Same content at another position: same hash.
    let mut q = p.clone();
    for y in 0..64 {
        let (s, d) = (y * stride, y * stride + 128 * 4);
        q.copy_within(s..s + 256, d);
    }
    assert_eq!(tile_hash(&q, stride, 128, 0), tile_hash(&p, stride, 0, 0));
}

#[test]
fn cache_lru_evicts_least_recent_and_reuses_slots() {
    let mut c = TileCache::new(3);
    assert_eq!(c.insert(1), 0);
    assert_eq!(c.insert(2), 1);
    assert_eq!(c.insert(3), 2);
    assert_eq!(c.lookup(1), Some(0)); // 1 now MRU, 2 is LRU
    assert_eq!(c.insert(4), 1); // evicts 2
    assert!(!c.contains(2) && c.contains(4));
    assert_eq!(c.insert(3), 2); // already cached: same slot
    assert_eq!(c.insert(5), 0); // LRU now 1 (slot 0)
    assert_eq!(c.len(), 3);
    c.clear();
    assert!(c.is_empty() && c.lookup(3).is_none());
    assert_eq!(c.insert(9), 0);
}

#[test]
fn atlas_round_trips_close_to_the_source_including_edge_tiles() {
    let (w, h) = (300u32, 150u32); // partial tiles on both edges
    let stride = w as usize * 4;
    let p = photo(w, h, 11);
    let tiles: Vec<(u16, u16)> = vec![(0, 0), (4, 0), (2, 1), (4, 2), (1, 2)];
    let imgs = encode_images(&p, w, h, stride, &tiles, LossyParams::default()).unwrap();
    assert_eq!(imgs.len(), 1);
    let mut fb = vec![0u8; p.len()];
    let mut buf = Vec::new();
    for i in &imgs {
        let li = LossyImage {
            kind: i.kind,
            cols: i.cols,
            tiles: i.tiles.clone(),
            data: &i.data,
        };
        decode_atlas(&li, &mut buf).unwrap();
        paint_atlas(&li, &buf, &mut fb, w, h, stride);
    }
    for &(c, r) in &tiles {
        let tr = tile_rect(u32::from(c), u32::from(r), w, h);
        let mut err = 0u64;
        let mut n = 0u64;
        for y in tr.y as usize..(tr.y + tr.h) as usize {
            for x in tr.x as usize..(tr.x + tr.w) as usize {
                for k in 0..3 {
                    let o = y * stride + x * 4 + k;
                    err += u64::from(p[o].abs_diff(fb[o]));
                    n += 1;
                }
                assert_eq!(fb[y * stride + x * 4 + 3], 255);
            }
        }
        assert!(err / n < 12, "tile ({c},{r}) mean abs error {}", err / n);
    }
    // Untouched outside the tiles.
    assert!(fb[(140 * stride) + 200 * 4..(140 * stride) + 250 * 4]
        .iter()
        .all(|&b| b == 0));
    // Many tiles: several atlases (parallel), still all decodable.
    let all: Vec<(u16, u16)> = (0..3).flat_map(|r| (0..5).map(move |c| (c, r))).collect();
    let big = photo(1024, 1024, 1);
    let many: Vec<(u16, u16)> = (0..16).flat_map(|r| (0..16).map(move |c| (c, r))).collect();
    let imgs = encode_images(&big, 1024, 1024, 4096, &many, LossyParams::default()).unwrap();
    assert!(imgs.len() >= 4, "{}", imgs.len());
    assert_eq!(imgs.iter().map(|i| i.tiles.len()).sum::<usize>(), 256);
    let _ = all;
}

#[test]
fn corrupt_atlases_are_rejected() {
    let p = photo(128, 64, 2);
    let imgs = encode_images(&p, 128, 64, 512, &[(0, 0), (1, 0)], LossyParams::default()).unwrap();
    let i = &imgs[0];
    let mut buf = Vec::new();
    // Tile list that does not match the picture size.
    let li = LossyImage {
        kind: 0,
        cols: 1,
        tiles: i.tiles.clone(),
        data: &i.data,
    };
    assert!(decode_atlas(&li, &mut buf).is_err());
    let li = LossyImage {
        kind: 7,
        cols: i.cols,
        tiles: i.tiles.clone(),
        data: &i.data,
    };
    assert!(decode_atlas(&li, &mut buf).is_err());
    let li = LossyImage {
        kind: 0,
        cols: i.cols,
        tiles: i.tiles.clone(),
        data: &i.data[..40],
    };
    assert!(decode_atlas(&li, &mut buf).is_err());
    let mut junk = i.data.clone();
    for b in junk.iter_mut().skip(700) {
        *b = 0xFF;
    }
    let li = LossyImage {
        kind: 0,
        cols: i.cols,
        tiles: i.tiles.clone(),
        data: &junk,
    };
    let _ = decode_atlas(&li, &mut buf); // must not panic
}

#[test]
fn slots_store_and_load_tiles() {
    let (w, h) = (192u32, 128u32);
    let stride = w as usize * 4;
    let p = photo(w, h, 4);
    let mut s = SlotStore::new();
    assert!(!s.is_filled(5));
    s.store(5, &p, stride, 64, 64, 64, 64);
    assert!(s.is_filled(5) && s.filled() == 1);
    assert_eq!(s.slot_hash(5), Some(tile_hash(&p, stride, 64, 64)));
    let mut fb = vec![0u8; p.len()];
    assert!(s.load(5, &mut fb, stride, 128, 0, 64, 64));
    assert!(!s.load(4, &mut fb, stride, 0, 0, 64, 64));
    assert_eq!(
        tile_hash(&fb, stride, 128, 0),
        tile_hash(&p, stride, 64, 64)
    );
    // A clipped edge tile: its own size, hash includes it.
    s.store(6, &p, stride, 128, 100, 64, 28);
    assert_eq!(s.dims(6), Some((64, 28)));
    assert!(
        !s.load(6, &mut fb, stride, 0, 0, 64, 64),
        "size mismatch refused"
    );
    assert!(s.load(6, &mut fb, stride, 0, 100, 64, 28));
    assert_eq!(
        tile_hash_wh(&fb, stride, 0, 100, 64, 28),
        tile_hash_wh(&p, stride, 128, 100, 64, 28)
    );
    assert_ne!(
        tile_hash_wh(&p, stride, 0, 0, 64, 28),
        tile_hash_wh(&p, stride, 0, 0, 64, 27)
    );
    s.clear();
    assert!(!s.is_filled(5) && s.filled() == 0);
}
