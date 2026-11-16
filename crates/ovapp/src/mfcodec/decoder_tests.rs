//! Decoder creation, D3D11 path selection, zero-copy output and fallback.

use super::*;
use crate::tiles::TileRect;
use crate::video::{psnr_bgra, synth_video, DecodedTo, VideoCodec, VideoDecode, VideoHeader};

/// Software-encoded H.264 of `n` synthetic `w x h` pictures (the sources
/// alongside). `None` without an encoder.
fn stream(w: u32, h: u32, n: u32) -> Option<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut enc = H264Encoder::new(w, h, 30, 8_000_000, EncoderPref::Software)
        .map_err(|e| eprintln!("skipping: no software encoder ({e})"))
        .ok()?;
    let mut out = Vec::new();
    for t in 0..n {
        let mut px = vec![0u8; (w * h * 4) as usize];
        let rect = TileRect {
            x: 0,
            y: 0,
            w: w as u16,
            h: h as u16,
        };
        synth_video(&mut px, w, rect, t);
        for au in enc
            .encode(&px, w as usize * 4, t == 0, 1_000 + u64::from(t) * 33_333)
            .ok()?
        {
            out.push((au.data, px.clone()));
        }
    }
    Some(out)
}

fn header(w: u32, h: u32, seq: u64) -> VideoHeader {
    VideoHeader {
        seq,
        flags: 0,
        codec: VideoCodec::H264.wire(),
        rect: TileRect {
            x: 16,
            y: 8,
            w: w as u16,
            h: h as u16,
        },
        screen_w: w + 32,
        screen_h: h + 16,
        pts_us: 0,
    }
}

/// A presenter on the hardware device, `None` (skip) without one.
fn target() -> Option<GpuTarget> {
    match gpu_device() {
        Ok(device) => Some(GpuTarget {
            device,
            slot: GpuPictureSlot::new(),
        }),
        Err(e) => {
            eprintln!("skipping: no hardware D3D11 device ({e})");
            None
        }
    }
}

#[test]
fn software_decoder_opens_without_gpu() {
    let d = MfVideoDecoder::open(VideoCodec::H264, None).unwrap();
    assert!(!d.is_gpu());
    assert_eq!(d.codec(), VideoCodec::H264);
    assert!(d.name().contains("software"));
    let (d, why) = MfVideoDecoder::open_best(VideoCodec::H264, None).unwrap();
    assert!(!d.is_gpu() && why.is_none());
}

/// `open_best` takes the D3D11 path on a video-capable hardware device
/// (Microsoft's H.264 MFT is D3D11-aware), else reports why it fell back.
#[test]
fn d3d11_path_selected_on_a_video_device() {
    let Some(t) = target() else { return };
    let (d, why) = MfVideoDecoder::open_best(VideoCodec::H264, Some(&t)).unwrap();
    eprintln!("decoder: {} (fallback reason {why:?})", d.name());
    assert_eq!(d.is_gpu(), why.is_none());
    assert!(d.is_gpu(), "hardware device but no DXVA: {why:?}");
}

/// Zero-copy: pictures land in the slot as BGRA textures at the unit's
/// rect; `end_gpu` reads the last one back (matching the source), and a
/// disabled slot makes the same decoder produce CPU pixels.
#[test]
fn gpu_output_reaches_the_slot_and_reads_back() {
    let Some(t) = target() else { return };
    let (w, h) = (640u32, 360u32);
    let Some(units) = stream(w, h, 20) else {
        return;
    };
    let Ok(mut d) = MfVideoDecoder::open(VideoCodec::H264, Some(&t)) else {
        eprintln!("skipping: no DXVA decoder");
        return;
    };
    let mut out = Vec::new();
    let mut gpu = 0;
    let mut last_src = None;
    for (i, (au, src)) in units.iter().enumerate() {
        let hd = header(w, h, i as u64);
        match d.decode_unit(&hd, au, &mut out).unwrap() {
            DecodedTo::Gpu => {
                gpu += 1;
                last_src = Some(src);
                let p = t.slot.current().expect("a picture is shown");
                assert_eq!((p.rect, p.screen_w, p.screen_h), (hd.rect, w + 32, h + 16));
            }
            DecodedTo::Cpu => panic!("CPU picture on the D3D11 path"),
            DecodedTo::Nothing => {}
        }
    }
    assert!(gpu >= 15, "only {gpu} GPU pictures of {}", units.len());
    assert!(d.end_gpu(&mut out), "a shown picture reads back");
    assert!(t.slot.current().is_none(), "end_gpu hides it");
    assert_eq!(out.len(), (w * h * 4) as usize);
    let psnr = psnr_bgra(last_src.unwrap(), &out);
    eprintln!("GPU picture readback PSNR {psnr:.1} dB");
    assert!(psnr > 30.0, "GPU conversion off: {psnr:.1} dB");
    assert!(!d.end_gpu(&mut out), "nothing shown any more");

    // Disabled slot (recording / no Direct2D): CPU pixels, nothing shown.
    t.slot.set_enabled(false);
    let src = &units.last().unwrap().1;
    let mut d = MfVideoDecoder::open(VideoCodec::H264, Some(&t)).unwrap();
    let mut got = DecodedTo::Nothing;
    for (i, (au, _)) in units.iter().enumerate() {
        got = d
            .decode_unit(&header(w, h, i as u64), au, &mut out)
            .unwrap();
        assert_ne!(got, DecodedTo::Gpu);
    }
    assert_eq!(got, DecodedTo::Cpu);
    assert!(t.slot.current().is_none());
    let psnr = psnr_bgra(src, &out);
    assert!(psnr > 30.0, "CPU readback off: {psnr:.1} dB");
}

/// The software and D3D11 paths agree on colour (both BT.709 limited ->
/// full-range sRGB).
#[test]
fn gpu_and_cpu_pictures_match() {
    let Some(t) = target() else { return };
    let (w, h) = (320u32, 240u32);
    let Some(units) = stream(w, h, 6) else { return };
    let Ok(mut g) = MfVideoDecoder::open(VideoCodec::H264, Some(&t)) else {
        eprintln!("skipping: no DXVA decoder");
        return;
    };
    let mut c = MfVideoDecoder::new().unwrap();
    let (mut a, mut b) = (Vec::new(), Vec::new());
    let mut compared = 0;
    for (au, _) in &units {
        // `decode` is the CPU contract on either decoder.
        let x = g.decode(au, w, h, &mut a).unwrap();
        let y = c.decode(au, w, h, &mut b).unwrap();
        if x && y {
            let psnr = psnr_bgra(&a, &b);
            // Mean signed difference per channel: a colour-matrix or range
            // mismatch shows as an offset; chroma filtering does not.
            let mut bias = [0i64; 3];
            for (p, q) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0) {
                for c in 0..3 {
                    bias[c] += i64::from(p[c]) - i64::from(q[c]);
                }
            }
            let n = (w * h) as f64;
            let bias = bias.map(|v| v as f64 / n);
            eprintln!("GPU vs CPU: {psnr:.1} dB, mean BGR offset {bias:.2?}");
            assert!(bias.iter().all(|b| b.abs() < 1.5), "colour offset {bias:?}");
            assert!(psnr > 32.0, "paths disagree: {psnr:.1} dB");
            compared += 1;
        }
    }
    assert!(compared > 0);
}

/// HEVC: advertised only when its decoder MFT really opens.
#[test]
fn hevc_availability_matches_creation() {
    let avail = MfVideoDecoder::hevc_available();
    eprintln!("HEVC decoder available: {avail}");
    assert_eq!(avail, MfVideoDecoder::open(VideoCodec::Hevc, None).is_ok());
    assert_eq!(MfVideoDecoder::new().unwrap().hevc(), avail);
}

/// The slot's hand-off: show / current / hide, and the enable flag.
#[test]
fn picture_slot_hand_off() {
    let s = GpuPictureSlot::new();
    assert!(s.enabled() && s.current().is_none() && s.hide().is_none());
    s.set_enabled(false);
    assert!(!s.clone().enabled(), "clones share state");
}
