use super::*;

/// A zstd frame of `t` (pure-Rust encoder: works without `zstd-encode`).
fn zframe(t: &[u8]) -> Vec<u8> {
    ruzstd::encoding::compress_to_vec(t, ruzstd::encoding::CompressionLevel::Fastest)
}

/// A codec-4 body around the transformed stream `t`.
fn body(chunk_log2: u8, t: &[u8]) -> Vec<u8> {
    let mut b = vec![chunk_log2];
    b.extend_from_slice(&(t.len() as u32).to_be_bytes());
    b.extend_from_slice(&zframe(t));
    b
}

/// `n` pixels cycling through `colors` distinct colours, in runs of `run`.
fn cycling(n: usize, colors: u32, run: usize) -> Vec<u8> {
    (0..n)
        .flat_map(|i| {
            let c = (i / run) as u32 % colors;
            [(c * 13) as u8, (c * 7) as u8, (c * 3) as u8, 255]
        })
        .collect()
}

/// Transform, wrap and decode `band`; asserts the exact round trip and
/// returns the transformed length.
fn roundtrip(band: &[u8], chunk_log2: u8) -> usize {
    let tr = transform(band, chunk_log2, usize::MAX).expect("transform applies");
    let mut out = vec![0u8; band.len()];
    decode(&body(chunk_log2, &tr.t), &mut out).expect("decodes");
    assert_eq!(out, band);
    tr.t.len()
}

#[test]
fn index_width_follows_the_palette_size() {
    assert_eq!(bits_for(1), 1);
    assert_eq!(bits_for(2), 1);
    assert_eq!(bits_for(3), 2);
    assert_eq!(bits_for(4), 2);
    assert_eq!(bits_for(5), 4);
    assert_eq!(bits_for(16), 4);
    // 64 px: 2 colours = 8 B of indices, 4 = 16 B, 16 = 32 B.
    for (colors, idx) in [(2u32, 8usize), (4, 16), (16, 32)] {
        let band = cycling(64, colors, 1);
        let tr = transform(&band, 6, usize::MAX).unwrap();
        assert_eq!(
            tr.t.len(),
            1 + colors as usize * 4 + idx,
            "{colors} colours"
        );
        assert_eq!(tr.palette_px, 64);
    }
}

#[test]
fn round_trips_every_palette_size_and_raw_chunks() {
    for colors in [1, 2, 3, 4, 5, 9, 16, 17, 40] {
        for run in [1, 3, 64] {
            let band = cycling(1000, colors, run);
            for log2 in [MIN_CHUNK_LOG2, CHUNK_LOG2, 8, MAX_CHUNK_LOG2] {
                roundtrip(&band, log2);
            }
        }
    }
    // More than 16 colours per chunk: raw chunks (tag + pixels).
    let band = cycling(128, 17, 1);
    let tr = transform(&band, 6, usize::MAX).unwrap();
    assert_eq!(tr.palette_px, 0);
    assert_eq!(tr.t.len(), 2 + band.len());
}

#[test]
fn a_covering_palette_is_reused() {
    // Chunk 1 holds 4 colours, chunk 2 two of them: REUSE, no palette bytes.
    let mut band = cycling(64, 4, 1);
    band.extend(cycling(64, 2, 5));
    let tr = transform(&band, 6, usize::MAX).unwrap();
    assert_eq!(tr.t.len(), (1 + 16 + 16) + (1 + 16));
    assert_eq!(tr.t[1 + 16 + 16], 0x80);
    roundtrip(&band, 6);
}

#[test]
fn round_trips_text_and_odd_lengths() {
    let text = crate::lossy::tests::text(203, 61, 7);
    let n = roundtrip(&text, CHUNK_LOG2);
    assert!(n * 3 < text.len(), "text palettizes: {n} of {}", text.len());
    // A last chunk shorter than the rest; a single pixel.
    roundtrip(&text[..(64 * 3 + 5) * 4], CHUNK_LOG2);
    roundtrip(&[1, 2, 3, 4], CHUNK_LOG2);
    roundtrip(&[], CHUNK_LOG2);
}

#[test]
fn transform_refuses_bad_input_and_gives_up_past_its_limit() {
    assert!(transform(&[0; 6], CHUNK_LOG2, usize::MAX).is_none());
    assert!(transform(&[0; 8], 3, usize::MAX).is_none());
    assert!(transform(&[0; 8], 13, usize::MAX).is_none());
    // Noise never fits a palette: past half the band it gives up.
    let noise: Vec<u8> = (0..4096u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    assert!(transform(&noise, CHUNK_LOG2, noise.len() / 2).is_none());
    assert!(encode(&noise, 0.5).is_none());
}

#[test]
fn decode_rejects_malformed_bodies() {
    let px = 64usize;
    let mut out = vec![0u8; px * 4];
    let good = transform(&cycling(px, 3, 1), 6, usize::MAX).unwrap().t;
    decode(&body(6, &good), &mut out).unwrap();
    let bad = |b: &[u8], out: &mut [u8]| decode(b, out).is_err();
    // Header: truncated, chunk size out of range, t_len past its bound.
    assert!(bad(&[6, 0, 0], &mut out));
    assert!(bad(&body(3, &good), &mut out));
    assert!(bad(&body(13, &good), &mut out));
    let mut huge = body(6, &good);
    huge[1..5].copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(bad(&huge, &mut out));
    // t_len disagreeing with the zstd frame.
    let mut short = body(6, &good);
    short[1..5].copy_from_slice(&((good.len() - 1) as u32).to_be_bytes());
    assert!(bad(&short, &mut out));
    // Output not whole pixels.
    assert!(bad(&body(6, &good), &mut vec![0u8; px * 4 - 2]));
    // Unknown tag, reuse before any palette, index outside the palette.
    let mut t = good.clone();
    t[0] = 0x41;
    assert!(bad(&body(6, &t), &mut out));
    let mut t = vec![0x80];
    t.extend_from_slice(&[0; 16]);
    assert!(bad(&body(6, &t), &mut out));
    let mut t = vec![3u8];
    t.extend_from_slice(&[0; 12]);
    t.extend_from_slice(&[0xFF; 16]); // index 3 of a 3-colour palette
    assert!(bad(&body(6, &t), &mut out));
    // Truncated and trailing streams.
    assert!(bad(&body(6, &good[..good.len() - 1]), &mut out));
    let mut t = good.clone();
    t.push(0);
    assert!(bad(&body(6, &t), &mut out));
}

/// The host's codec choice on text: a palette band, and it decodes through
/// the container like any other band.
#[test]
#[cfg(feature = "zstd-encode")]
fn container_carries_palette_bands_for_text() {
    let text = crate::lossy::tests::text(512, 256, 3);
    let policy = crate::bands::BandPolicy {
        zstd: true,
        lz4: false,
        palette: true,
        ..crate::bands::BandPolicy::default()
    };
    let mut wire = Vec::new();
    let st = crate::bands::encode_packed(&text, &[], &policy, &mut wire);
    assert_eq!(st.palette_bands, st.bands);
    let p = crate::bands::parse(&wire).unwrap();
    let mut out = Vec::new();
    crate::bands::decode_bands(&p.bands, &mut out).unwrap();
    assert_eq!(out, text);
    // Without CAP_PALETTE: plain zstd.
    let plain = crate::bands::BandPolicy {
        palette: false,
        ..policy
    };
    let st = crate::bands::encode_packed(&text, &[], &plain, &mut wire);
    assert_eq!(st.palette_bands, 0);
}

/// Byte benchmark (`cargo test -p ovapp --release --features zstd-encode
/// palette_bytes -- --ignored --nocapture`): palette + zstd vs plain zstd
/// on synthetic text / UI screens.
#[test]
#[ignore = "benchmark"]
#[cfg(feature = "zstd-encode")]
fn palette_bytes() {
    use crate::tiles::TileRect;
    let (w, h) = (1920u32, 1080u32);
    let mut desktop = vec![0u8; (w * h * 4) as usize];
    crate::video::synth_desktop(&mut desktop, w, h);
    let mut page = desktop.clone();
    let full = TileRect {
        x: 0,
        y: 0,
        w: w as u16,
        h: h as u16,
    };
    crate::video::synth_scroll(&mut page, w, full, 3, 12);
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("synth_desktop 1920x1080", desktop),
        ("synth_scroll page 1920x1080", page),
        ("AA text 1920x1080", crate::lossy::tests::text(w, h, 11)),
        ("AA text 64x64 tile", crate::lossy::tests::text(64, 64, 5)),
        ("code editor 1920x1080", code_editor(w, h)),
        ("photo 640x360", crate::lossy::tests::photo(640, 360, 2)),
    ];
    for (name, px) in cases {
        let t0 = std::time::Instant::now();
        let z = crate::bands::zstd_band(&px).unwrap();
        let tz = t0.elapsed();
        let t0 = std::time::Instant::now();
        let p = encode(&px, 1.0);
        let tp = t0.elapsed();
        let pl = p.as_ref().map_or(0, Vec::len);
        let mut out = vec![0u8; px.len()];
        let t0 = std::time::Instant::now();
        if let Some(p) = &p {
            decode(p, &mut out).unwrap();
            assert_eq!(out, px);
        }
        let td = t0.elapsed();
        println!(
            "{name:30} raw {:>9}  zstd {:>8} ({:.1} ms)  palette {:>8} ({:.1} ms enc, {:.1} ms dec)  {}",
            px.len(),
            z.len(),
            tz.as_secs_f64() * 1e3,
            pl,
            tp.as_secs_f64() * 1e3,
            td.as_secs_f64() * 1e3,
            if p.is_some() {
                format!("{:.2}x smaller", z.len() as f64 / pl.max(1) as f64)
            } else {
                "not palettized (plain zstd)".to_owned()
            }
        );
    }
}

/// A dark-theme code editor: gutter, line numbers, syntax-coloured
/// "tokens" with 2 anti-aliasing levels per colour, a current-line band.
pub(crate) fn code_editor(w: u32, h: u32) -> Vec<u8> {
    let bg = [0x1E, 0x1E, 0x1E, 0xFF];
    let gutter = [0x25, 0x25, 0x26, 0xFF];
    let cur = [0x2A, 0x2D, 0x2E, 0xFF];
    let inks: [[u8; 4]; 6] = [
        [0xD4, 0xD4, 0xD4, 0xFF],
        [0xD6, 0x9C, 0x56, 0xFF],
        [0x9C, 0xDC, 0xFE, 0xFF],
        [0x78, 0x9C, 0xCE, 0xFF],
        [0x6A, 0x99, 0x55, 0xFF],
        [0xB5, 0xCE, 0xA8, 0xFF],
    ];
    let mix = |a: [u8; 4], b: [u8; 4], k: u16| -> [u8; 4] {
        let m = |x: u8, y: u8| ((u16::from(x) * (4 - k) + u16::from(y) * k) / 4) as u8;
        [m(a[0], b[0]), m(a[1], b[1]), m(a[2], b[2]), 0xFF]
    };
    let mut v = vec![0u8; (w * h * 4) as usize];
    let (px, _) = v.as_chunks_mut::<4>();
    let mut s = 0x9E37_79B9_u64;
    let mut rnd = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    let line_h = 19u32;
    // Per line: its tokens as (first column, length, colour).
    let lines: Vec<Vec<(u32, u32, usize)>> = (0..h / line_h + 1)
        .map(|_| {
            let indent = (rnd() % 6) as u32 * 4;
            let mut toks = Vec::new();
            let mut col = indent;
            let n = rnd() % 9;
            for _ in 0..n {
                let len = 2 + (rnd() % 10) as u32;
                toks.push((col, len, (rnd() % 6) as usize));
                col += len + 1;
            }
            toks
        })
        .collect();
    for y in 0..h {
        let line = (y / line_h) as usize;
        let in_line = y % line_h;
        let base = if line == 7 { cur } else { bg };
        for x in 0..w {
            let p = &mut px[(y * w + x) as usize];
            if x < 60 {
                *p = gutter;
                continue;
            }
            *p = base;
            if !(4..15).contains(&in_line) {
                continue;
            }
            let cx = (x - 60) / 8;
            let gx = (x - 60) % 8;
            if gx > 6 {
                continue;
            }
            if let Some(&(_, _, ink)) = lines[line].iter().find(|&&(c, l, _)| cx >= c && cx < c + l)
            {
                // Glyph strokes with two anti-aliasing levels.
                let hsh = (x * 31 + y * 17 + cx * 7) % 7;
                *p = match hsh {
                    0 | 1 => inks[ink],
                    2 => mix(base, inks[ink], 2),
                    3 => mix(base, inks[ink], 1),
                    _ => base,
                };
            }
        }
    }
    v
}

#[test]
fn random_few_colour_bands_round_trip() {
    let mut s = 0x1234_5678_u64;
    let mut rnd = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    for _ in 0..300 {
        let px = 1 + (rnd() % 3000) as usize;
        let colors = 1 + (rnd() % 24) as u32;
        let mut band = Vec::with_capacity(px * 4);
        let mut c = 0u32;
        for _ in 0..px {
            if rnd() % 4 == 0 {
                c = (rnd() % u64::from(colors)) as u32;
            }
            band.extend_from_slice(&[c as u8, (c * 3) as u8, 0, 255]);
        }
        // Sprinkle byte-level noise (a pixel differing in one channel).
        if rnd() % 2 == 0 {
            let i = (rnd() as usize) % band.len();
            band[i] ^= 0x5A;
        }
        roundtrip(
            &band,
            (MIN_CHUNK_LOG2 + (rnd() % 9) as u8).min(MAX_CHUNK_LOG2),
        );
    }
}
