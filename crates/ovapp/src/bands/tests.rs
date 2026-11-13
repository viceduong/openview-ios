use super::*;

fn noise(n: usize, seed: u64) -> Vec<u8> {
    let mut s = seed.max(1);
    (0..n)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s as u8
        })
        .collect()
}

/// Text-like (very compressible) half + noise half.
fn mixed(n: usize) -> Vec<u8> {
    let mut v: Vec<u8> = (0..n / 2).map(|i| ((i / 64) % 7) as u8 * 30).collect();
    v.extend(noise(n - n / 2, 3));
    v
}

/// A container header with band entries (codec, raw_len, wire_len)
/// and `body` appended.
fn container(entries: &[(u8, u32, u32)], body: &[u8]) -> Vec<u8> {
    let mut v = vec![VERSION, 0];
    v.extend_from_slice(&(entries.len() as u16).to_be_bytes());
    for &(c, raw, wire) in entries {
        v.push(c);
        v.extend_from_slice(&raw.to_be_bytes());
        v.extend_from_slice(&wire.to_be_bytes());
    }
    v.extend_from_slice(body);
    v
}

#[test]
fn raw_len_is_bounded_before_any_allocation() {
    // One LZ4 band claiming 4 GiB from 1 byte of input.
    let c = container(&[(3, u32::MAX, 1)], &[0]);
    let err = parse(&c).unwrap_err().to_string();
    assert!(err.contains("more than"), "{err}");
    // 64 bands of 16 MiB each = 1 GiB > MAX_RAW_TOTAL.
    let entries = vec![(3u8, 16u32 << 20, 0u32); MAX_BANDS];
    assert!(parse(&container(&entries, &[])).is_err());
    // A geometry bound is tighter: 10x10 BGRA = 400 bytes.
    let c = container(&[(0, 401, 401)], &[0; 401]);
    assert!(parse(&c).is_ok());
    let max = max_raw_len(10, 10, 4).unwrap();
    assert!(parse_bounded(&c, max)
        .unwrap_err()
        .to_string()
        .contains("400"));
    assert!(parse_bounded(&container(&[(0, 400, 400)], &[7; 400]), max).is_ok());
    assert_eq!(max_raw_len(u32::MAX, u32::MAX, 4), None);
    assert_eq!(max_raw_len(8192, 8192, 4), Some(MAX_RAW_TOTAL));
}

#[test]
fn decode_bands_rejects_hand_built_lies() {
    let mut out = Vec::new();
    let lie = BandRef {
        codec: FrameCodec::Raw,
        raw_len: 8,
        data: &[1, 2, 3],
    };
    assert!(
        decode_bands(&[lie], &mut out).is_err(),
        "no copy_from_slice panic"
    );
    let huge = BandRef {
        codec: FrameCodec::Lz4,
        raw_len: usize::MAX,
        data: &[],
    };
    assert!(
        decode_bands(&[huge, huge], &mut out).is_err(),
        "no overflow"
    );
    assert!(out.capacity() < 1 << 20, "nothing big was allocated");
}

#[test]
fn hostile_containers_never_panic() {
    let mut s = 0x2545_F491_4F6C_DD1Du64;
    let mut next = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    let seed = {
        let mut out = Vec::new();
        let moves = [MoveRect {
            x: 1,
            y: 1,
            w: 2,
            h: 2,
            dx: 1,
            dy: 0,
        }];
        encode_packed(&noise(4096, 9), &moves, &BandPolicy::default(), &mut out);
        out
    };
    let mut buf = Vec::new();
    for i in 0..2000 {
        let mut c = if i % 2 == 0 {
            seed.clone()
        } else {
            (0..(next() % 64) as usize).map(|_| next() as u8).collect()
        };
        if !c.is_empty() {
            for _ in 0..(1 + next() % 3) {
                let at = (next() as usize) % c.len();
                c[at] = next() as u8;
            }
        }
        if let Ok(p) = parse_bounded(&c, 64 * 1024) {
            let _ = decode_bands(&p.bands, &mut buf);
        }
    }
}

#[test]
fn band_lengths_cover_and_align() {
    assert!(band_lengths(0, 8).is_empty());
    assert_eq!(band_lengths(1000, 8), vec![1000]);
    let l = band_lengths(8_294_400, 8);
    assert_eq!(l.len(), 8);
    assert_eq!(l.iter().sum::<usize>(), 8_294_400);
    assert!(l[..7].iter().all(|b| b % 4 == 0));
    let l = band_lengths(3 * MIN_BAND + 5, 8);
    assert_eq!(l.len(), 3);
    assert_eq!(l.iter().sum::<usize>(), 3 * MIN_BAND + 5);
}

#[test]
fn packed_round_trips_with_mixed_codecs_and_moves() {
    let data = mixed(2_000_000);
    let moves = [
        MoveRect {
            x: 0,
            y: 0,
            w: 10,
            h: 5,
            dx: 0,
            dy: -3,
        },
        MoveRect {
            x: 5,
            y: 9,
            w: 3,
            h: 2,
            dx: 2,
            dy: 1,
        },
    ];
    for policy in [
        BandPolicy::default(),
        BandPolicy {
            zstd: false,
            ..BandPolicy::default()
        },
        BandPolicy {
            lz4: false,
            ..BandPolicy::default()
        },
        BandPolicy {
            lz4: false,
            zstd: false,
            ..BandPolicy::default()
        },
    ] {
        let mut wire = Vec::new();
        let st = encode_packed(&data, &moves, &policy, &mut wire);
        assert!(st.bands >= 2, "{st:?}");
        if policy.lz4 {
            assert!(
                st.lz4_bands + st.raw_bands >= 1,
                "noise half goes LZ4/raw: {st:?}"
            );
        }
        if policy.zstd && policy.lz4 {
            assert!(st.zstd_bands >= 1, "text half goes zstd: {st:?}");
        }
        let p = parse(&wire).unwrap();
        assert_eq!(p.moves, moves);
        assert_eq!(p.raw_len(), data.len());
        let mut out = Vec::new();
        decode_bands(&p.bands, &mut out).unwrap();
        assert_eq!(out, data);
    }
    // No moves, empty payload.
    let mut wire = Vec::new();
    encode_packed(&[], &[], &BandPolicy::default(), &mut wire);
    let p = parse(&wire).unwrap();
    assert!(p.moves.is_empty() && p.bands.is_empty());
}

#[test]
fn parse_rejects_malformed_containers() {
    let data = mixed(300_000);
    let mut wire = Vec::new();
    encode_packed(
        &data,
        &[MoveRect {
            x: 1,
            y: 1,
            w: 1,
            h: 1,
            dx: 1,
            dy: 1,
        }],
        &BandPolicy::default(),
        &mut wire,
    );
    assert!(parse(&wire).is_ok());
    for cut in [0, 1, 3, 10, wire.len() - 1] {
        assert!(parse(&wire[..cut]).is_err(), "cut {cut}");
    }
    let mut extra = wire.clone();
    extra.push(0);
    assert!(parse(&extra).is_err(), "trailing byte");
    let mut bad = wire.clone();
    bad[0] = 9;
    assert!(parse(&bad).is_err(), "version");
    let mut bad = wire.clone();
    bad[1] = 0x82;
    assert!(parse(&bad).is_err(), "flags");
    // Corrupt band body: decode fails cleanly (never panics).
    let mut bad = wire.clone();
    let n = bad.len();
    for b in &mut bad[n - 200..] {
        *b ^= 0x5A;
    }
    if let Ok(p) = parse(&bad) {
        let mut out = Vec::new();
        let _ = decode_bands(&p.bands, &mut out);
    }
    // Raw band whose lengths disagree.
    let mut raw = vec![VERSION, 0, 0, 1, 0];
    raw.extend_from_slice(&4u32.to_be_bytes());
    raw.extend_from_slice(&3u32.to_be_bytes());
    raw.extend_from_slice(&[1, 2, 3]);
    assert!(parse(&raw).is_err());
}

#[test]
fn moves_apply_with_overlap_in_both_directions() {
    let (w, h) = (40u32, 30u32);
    let stride = w as usize * 4;
    let fb0: Vec<u8> = noise(stride * h as usize, 7);
    let px =
        |fb: &[u8], x: usize, y: usize| fb[y * stride + x * 4..y * stride + x * 4 + 4].to_vec();
    for (dx, dy) in [(0i16, -5i16), (0, 5), (3, 0), (-3, 0), (2, -2), (-4, 6)] {
        let m = MoveRect {
            x: 8,
            y: 8,
            w: 20,
            h: 12,
            dx,
            dy,
        };
        let mut fb = fb0.clone();
        apply_move(&mut fb, w, h, stride, 4, &m).unwrap();
        for y in 0..h as usize {
            for x in 0..w as usize {
                let inside = (8..28).contains(&x) && (8..20).contains(&y);
                let want = if inside {
                    px(
                        &fb0,
                        (x as i64 - i64::from(dx)) as usize,
                        (y as i64 - i64::from(dy)) as usize,
                    )
                } else {
                    px(&fb0, x, y)
                };
                assert_eq!(px(&fb, x, y), want, "move ({dx},{dy}) at ({x},{y})");
            }
        }
    }
    // Out of bounds: refused, untouched.
    let mut fb = fb0.clone();
    let bad = MoveRect {
        x: 0,
        y: 0,
        w: 10,
        h: 10,
        dx: 1,
        dy: 0,
    };
    assert!(apply_move(&mut fb, w, h, stride, 4, &bad).is_err());
    assert_eq!(fb, fb0);
}

#[cfg(feature = "zstd-encode")]
#[test]
fn zstd_bands_decode_sequentially_and_in_parallel() {
    let data = mixed(1_500_000);
    let wire = compress_zstd_bands(&data, 8).unwrap();
    let spans = crate::zstd_par::split_frames(&wire).unwrap();
    assert!(spans.len() > 1);
    let mut out = Vec::new();
    crate::codec::Decompressor::new()
        .decompress_into(FrameCodec::Zstd, &wire, data.len(), &mut out)
        .unwrap();
    assert_eq!(out, data);
    let mut seq = vec![0u8; data.len()];
    ruzstd::FrameDecoder::new()
        .decode_all(&wire, &mut seq)
        .unwrap();
    assert_eq!(seq, data);
}
