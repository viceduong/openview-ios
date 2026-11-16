//! Long-term references and HEVC on the real Media Foundation MFTs: LTR
//! marking is reported / honoured, a recovery P-frame from a confirmed LTR
//! decodes with the stock decoder after frames were lost, and HEVC encodes
//! decodable Annex-B with VPS/SPS/PPS on keyframes. Skips (passes) when an
//! encoder / decoder is not installed.

use super::encoder::EncodeParams;
use super::*;
use crate::tiles::TileRect;
use crate::video::{psnr_bgra, synth_video, AccessUnit, FrameRefs, VideoCodec, VideoDecode};

fn picture(w: u32, h: u32, t: u32) -> Vec<u8> {
    let mut px = vec![0u8; (w * h * 4) as usize];
    synth_video(
        &mut px,
        w,
        TileRect {
            x: 0,
            y: 0,
            w: w as u16,
            h: h as u16,
        },
        t,
    );
    px
}

fn ltr_encoder(codec: VideoCodec, pref: EncoderPref, w: u32, h: u32) -> Option<H264Encoder> {
    let p = EncodeParams {
        codec,
        ltr_slots: 2,
        ..EncodeParams::new(w, h, 30, 3_000_000)
    };
    match H264Encoder::with_params(&p, pref) {
        Ok(e) => Some(e),
        Err(e) => {
            eprintln!("skipping {codec} {pref:?}: no encoder ({e})");
            None
        }
    }
}

/// What one loss + recovery run measured.
#[derive(Debug, Default)]
struct Recovery {
    /// Bytes of the picture that brought the viewer back.
    resync_bytes: usize,
    /// Mean bytes of the P-frames before the loss.
    p_bytes: usize,
    /// PSNR of the pictures decoded after the resync (dB, mean).
    psnr_after: f64,
    /// Pictures decoded after the resync / errors.
    decoded_after: u32,
    errors: u32,
    /// A decoder was available (else only the encode side ran).
    decoder: bool,
    /// Whether the resync picture was a keyframe.
    keyframe: bool,
}

/// Encode 60 pictures; the "viewer" loses pictures 40..45 and resyncs at
/// 45 with an IDR (`ltr == false`) or a recovery P-frame from the LTR
/// marked at picture 30 (`ltr == true`).
fn loss_run(codec: VideoCodec, pref: EncoderPref, ltr: bool) -> Option<Recovery> {
    let (w, h) = (640u32, 384u32);
    let mut enc = ltr_encoder(codec, pref, w, h)?;
    if ltr && enc.ltr_slots() == 0 {
        eprintln!("skipping {}: no LTR control ({:?})", enc.name(), enc.caps());
        return None;
    }
    let mut dec = match MfVideoDecoder::open(codec, None) {
        Ok(d) => Some(d),
        Err(e) => {
            eprintln!("{codec}: no decoder ({e}); encode side only");
            None
        }
    };
    let mut r = Recovery {
        decoder: dec.is_some(),
        ..Recovery::default()
    };
    let mut out = Vec::new();
    let mut sent = std::collections::HashMap::new();
    let (mut psnr, mut p_sum, mut p_n) = (0f64, 0usize, 0usize);
    let mut ltr30 = None;
    for t in 0..60u32 {
        let px = picture(w, h, t);
        let pts = 1_000_000 + u64::from(t) * 33_333;
        sent.insert(pts, px.clone());
        let mut force = false;
        if t == 30 {
            // Slot 1 (the IDR keeps slot 0).
            enc.set_frame_refs(FrameRefs {
                mark_ltr: Some(1),
                use_ltr: 0,
            });
        }
        if t == 45 {
            if ltr {
                enc.set_frame_refs(FrameRefs {
                    mark_ltr: None,
                    use_ltr: 1 << ltr30.unwrap_or(1),
                });
            } else {
                force = true;
            }
        }
        for au in enc.encode(&px, w as usize * 4, force, pts).unwrap() {
            let at = ((au.pts_us - 1_000_000) / 33_333) as u32;
            if at < 3 || au.ltr.marked.is_some() || (45..47).contains(&at) {
                eprintln!(
                    "  {codec} {} pic {at}: {} B key {} ltr {:?}",
                    enc.name(),
                    au.data.len(),
                    au.keyframe,
                    au.ltr
                );
            }
            if at == 30 {
                ltr30 = au.ltr.marked;
            }
            if (10..40).contains(&at) && !au.keyframe {
                p_sum += au.data.len();
                p_n += 1;
            }
            if (40..45).contains(&at) {
                continue; // lost
            }
            if at == 45 {
                r.resync_bytes = au.data.len();
                r.keyframe = au.keyframe;
            }
            let Some(dec) = dec.as_mut() else {
                continue;
            };
            match dec.decode_codec(codec, &au.data, w, h, &mut out) {
                Ok(true) if at >= 45 => {
                    if let Some(src) = sent.get(&au.pts_us) {
                        psnr += psnr_bgra(src, &out);
                        r.decoded_after += 1;
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("  decode error at {at}: {e}");
                    r.errors += u32::from(at >= 45);
                }
            }
        }
    }
    r.p_bytes = p_sum / p_n.max(1);
    r.psnr_after = psnr / f64::from(r.decoded_after.max(1));
    Some(r)
}

fn check_ltr_recovery(codec: VideoCodec, pref: EncoderPref) {
    let Some(idr) = loss_run(codec, pref, false) else {
        return;
    };
    let Some(ltr) = loss_run(codec, pref, true) else {
        return;
    };
    eprintln!("{codec} {pref:?} IDR resync: {idr:?}");
    eprintln!("{codec} {pref:?} LTR resync: {ltr:?}");
    assert!(idr.keyframe, "forced resync is a keyframe");
    assert!(!ltr.keyframe, "LTR resync is a P-frame");
    assert!(
        ltr.resync_bytes < idr.resync_bytes,
        "recovery P-frame {} B vs IDR {} B",
        ltr.resync_bytes,
        idr.resync_bytes
    );
    if !ltr.decoder {
        return;
    }
    assert_eq!(ltr.errors, 0, "recovery frame decodes");
    assert!(ltr.decoded_after >= 13, "{ltr:?}");
    // Moving content, 15 pictures after a 15-picture-old reference:
    // indistinguishable from the IDR path.
    assert!(ltr.psnr_after > 30.0, "{ltr:?}");
    assert!(ltr.psnr_after > idr.psnr_after - 3.0, "{ltr:?} vs {idr:?}");
}

#[test]
fn ltr_recovery_after_loss_hardware_h264() {
    check_ltr_recovery(VideoCodec::H264, EncoderPref::Hardware);
}

#[test]
fn ltr_recovery_after_loss_software_h264() {
    check_ltr_recovery(VideoCodec::H264, EncoderPref::Software);
}

#[test]
fn ltr_recovery_after_loss_hardware_hevc() {
    check_ltr_recovery(VideoCodec::Hevc, EncoderPref::Hardware);
}

/// HEVC: Annex-B with VPS + SPS + PPS on the first and on forced
/// keyframes, decodable by the MF HEVC decoder.
#[test]
fn hevc_encodes_self_contained_keyframes() {
    let (w, h) = (640u32, 384u32);
    let p = EncodeParams {
        codec: VideoCodec::Hevc,
        ..EncodeParams::new(w, h, 30, 4_000_000)
    };
    let mut enc = match H264Encoder::with_params(&p, EncoderPref::Auto) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("skipping: no HEVC encoder ({e})");
            return;
        }
    };
    assert_eq!(enc.codec(), VideoCodec::Hevc);
    let mut aus = Vec::new();
    for t in 0..30u32 {
        let px = picture(w, h, t);
        aus.extend(
            enc.encode(
                &px,
                w as usize * 4,
                t == 20,
                1_000_000 + u64::from(t) * 33_333,
            )
            .unwrap(),
        );
    }
    let keys: Vec<usize> = (0..aus.len()).filter(|&i| aus[i].keyframe).collect();
    assert!(keys.len() >= 2 && keys[0] == 0, "keyframes at {keys:?}");
    for &k in &keys {
        let au = AccessUnit::from_annexb_codec(VideoCodec::Hevc, &aus[k].data);
        assert!(
            au.idr && au.has_param_sets(VideoCodec::Hevc),
            "keyframe {k}"
        );
    }
    let Ok(mut dec) = MfVideoDecoder::open(VideoCodec::Hevc, None) else {
        eprintln!("no HEVC decoder: encode checked only");
        return;
    };
    let mut out = Vec::new();
    let mut n = 0;
    for au in &aus {
        if dec
            .decode_codec(VideoCodec::Hevc, &au.data, w, h, &mut out)
            .unwrap()
        {
            n += 1;
        }
    }
    let src = picture(w, h, 29);
    let db = psnr_bgra(&src, &out);
    eprintln!(
        "HEVC via {}: {n} pictures, last PSNR {db:.1} dB",
        enc.name()
    );
    assert!(n >= 25 && db > 30.0);
}

/// Y-plane PSNR of `dec` (a sequence of NV12 pictures) against `src`.
fn y_psnr(src: &[Vec<u8>], dec: &[u8], w: u32, h: u32) -> f64 {
    let n = crate::video::nv12_len(w, h);
    let ylen = (w * h) as usize;
    let pics = (dec.len() / n).min(src.len());
    let mut sum = 0f64;
    for i in 0..pics {
        let a = &src[i][..ylen];
        let b = &dec[i * n..i * n + ylen];
        let se: f64 = a
            .iter()
            .zip(b)
            .map(|(&x, &y)| {
                let d = f64::from(x) - f64::from(y);
                d * d
            })
            .sum();
        let mse = (se / ylen as f64).max(1e-3);
        sum += 10.0 * (255.0f64 * 255.0 / mse).log10();
    }
    sum / pics.max(1) as f64
}

/// Decode an Annex-B file with ffmpeg to raw NV12 (`None` = no ffmpeg).
fn ffmpeg_decode(codec: VideoCodec, path: &std::path::Path) -> Option<Vec<u8>> {
    let fmt = match codec {
        VideoCodec::H264 => "h264",
        VideoCodec::Hevc => "hevc",
    };
    let out = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-f", fmt, "-i"])
        .arg(path)
        .args(["-f", "rawvideo", "-pix_fmt", "nv12", "-"])
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout)
}

/// Benchmark (prints only): H.264 High vs HEVC Main on the NVENC MFTs,
/// equal CBR targets and equal QP, 1080p bench scenes — bytes and Y-PSNR
/// (decoded with ffmpeg, against the encoder's NV12 input). Then the LTR
/// numbers: encoder open time and resync size, IDR vs recovery P-frame.
/// `cargo test --release -p ovapp --features mf -- --ignored --nocapture bench_hevc_vs_h264`
#[test]
#[ignore]
fn bench_hevc_vs_h264() {
    use crate::video::{bgra_to_nv12, synth_desktop, synth_scroll};
    let (w, h) = (1920u32, 1080u32);
    let frames = 60u32;
    let secs = f64::from(frames) / 30.0;
    let full = TileRect {
        x: 0,
        y: 0,
        w: w as u16,
        h: h as u16,
    };
    let region = TileRect {
        x: 640,
        y: 320,
        w: 640,
        h: 360,
    };
    type Scene = (&'static str, fn(&mut Vec<u8>, u32, TileRect, TileRect));
    let scenes: [Scene; 3] = [
        ("scrolling page", |px, t, full, _| {
            synth_scroll(px, 1920, full, t, 8)
        }),
        ("desktop + video region", |px, t, _, region| {
            synth_video(px, 1920, region, t)
        }),
        ("full-screen video", |px, t, full, _| {
            synth_video(px, 1920, full, t)
        }),
    ];
    let dir = std::env::temp_dir().join(format!("ov_hevc_bench_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (label, scene) in scenes {
        // The source pictures, as the encoder sees them (NV12).
        let mut px = vec![0u8; (w * h * 4) as usize];
        synth_desktop(&mut px, w, h);
        let mut bgra = Vec::new();
        let mut nv12 = Vec::new();
        for t in 0..frames {
            scene(&mut px, t, full, region);
            bgra.push(px.clone());
            let mut n = Vec::new();
            bgra_to_nv12(&px, w as usize * 4, w, h, &mut n);
            nv12.push(n);
        }
        for (mode, qp, bitrate) in [
            ("CBR 4 Mbps", None, 4_000_000u32),
            ("CBR 8 Mbps", None, 8_000_000),
            ("QP 30", Some(30u32), 8_000_000),
        ] {
            let mut row = Vec::new();
            for codec in [VideoCodec::H264, VideoCodec::Hevc] {
                let p = EncodeParams {
                    codec,
                    qp,
                    ..EncodeParams::new(w, h, 30, bitrate)
                };
                let mut enc = match H264Encoder::with_params(&p, EncoderPref::Hardware) {
                    Ok(e) => e,
                    Err(e) => {
                        println!("{codec}: unavailable: {e}");
                        return;
                    }
                };
                let mut stream = Vec::new();
                for (t, n) in nv12.iter().enumerate() {
                    let pts = 1_000_000 + t as u64 * 33_333;
                    for au in enc.encode_nv12(n, false, pts).unwrap() {
                        stream.extend_from_slice(&au.data);
                    }
                }
                let path = dir.join(format!("s.{codec}"));
                std::fs::write(&path, &stream).unwrap();
                let db = ffmpeg_decode(codec, &path).map_or(f64::NAN, |d| y_psnr(&nv12, &d, w, h));
                let kbps = stream.len() as f64 * 8.0 / 1000.0 / secs;
                row.push((codec, kbps, db));
            }
            let (a, b) = (row[0], row[1]);
            println!(
                "{label} | {mode}: H.264 High {:.0} kbps {:.2} dB | HEVC {:.0} kbps {:.2} dB | \
                 HEVC/H.264 bytes {:.0}%, dPSNR {:+.2} dB",
                a.1,
                a.2,
                b.1,
                b.2,
                b.1 / a.1 * 100.0,
                b.2 - a.2
            );
        }
    }
    // LTR: open time, IDR vs recovery P-frame after a 5-picture loss, on
    // the desktop + video region scene (1080p, CBR 8 Mbps).
    for codec in [VideoCodec::H264, VideoCodec::Hevc] {
        let p = EncodeParams {
            codec,
            ltr_slots: 2,
            ..EncodeParams::new(w, h, 30, 8_000_000)
        };
        let mut sizes = Vec::new();
        for ltr in [false, true] {
            let t0 = std::time::Instant::now();
            let Ok(mut enc) = H264Encoder::with_params(&p, EncoderPref::Hardware) else {
                return;
            };
            let open_ms = t0.elapsed().as_secs_f64() * 1000.0;
            let mut px = vec![0u8; (w * h * 4) as usize];
            synth_desktop(&mut px, w, h);
            let (mut p_sum, mut p_n, mut resync) = (0usize, 0usize, 0usize);
            for t in 0..60u32 {
                synth_video(&mut px, w, region, t);
                if t == 30 {
                    enc.set_frame_refs(FrameRefs {
                        mark_ltr: Some(1),
                        use_ltr: 0,
                    });
                }
                if t == 45 && ltr {
                    enc.set_frame_refs(FrameRefs {
                        mark_ltr: None,
                        use_ltr: 1 << 1,
                    });
                }
                let pts = 1_000_000 + u64::from(t) * 33_333;
                for au in enc
                    .encode(&px, w as usize * 4, t == 45 && !ltr, pts)
                    .unwrap()
                {
                    if t == 45 {
                        resync = au.data.len();
                    } else if t > 30 && !au.keyframe {
                        p_sum += au.data.len();
                        p_n += 1;
                    }
                }
            }
            sizes.push((open_ms, resync, p_sum / p_n.max(1)));
        }
        let ((open_ms, idr, p), (_, rec, _)) = (sizes[0], sizes[1]);
        // At the 8 Mbps link the extra bytes of the resync picture queue
        // in front of the next picture.
        let extra_ms = |b: usize| (b.saturating_sub(p)) as f64 * 8.0 / 8_000.0;
        println!(
            "{codec} LTR 1080p desktop+video, CBR 8 Mbps: open {open_ms:.0} ms; P {p} B; \
             IDR resync {idr} B (+{:.1} ms at 8 Mbps); LTR recovery {rec} B (+{:.1} ms); \
             recovery = {:.0}% of the IDR",
            extra_ms(idr),
            extra_ms(rec),
            rec as f64 / idr as f64 * 100.0
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
