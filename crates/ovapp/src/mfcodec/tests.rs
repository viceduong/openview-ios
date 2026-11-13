use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;

use super::*;
use crate::tiles::TileRect;
use crate::video::{
    bgra_to_nv12, nv12_to_bgra, psnr_bgra, synth_scroll, synth_video, AccessUnit, VideoDecode,
};

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

/// Encode N synthetic frames: non-empty Annex-B, the first an IDR with
/// SPS + PPS, a forced IDR honoured. Skips when no encoder exists.
fn smoke(pref: EncoderPref) {
    let (w, h) = (640, 360);
    let mut enc = match H264Encoder::new(w, h, 30, 4_000_000, pref) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("skipping {pref:?} encoder smoke test: {e}");
            return;
        }
    };
    eprintln!("encoder: {}", enc.name());
    let mut aus = Vec::new();
    for t in 0..30 {
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
    // Async hardware may still hold the last picture; one more flushes it.
    assert!(aus.len() >= 28, "only {} units for 30 pictures", aus.len());
    assert!(aus.iter().all(|a| !a.data.is_empty()));
    let first = AccessUnit::from_annexb(&aus[0].data);
    assert!(aus[0].keyframe && first.idr, "first unit is an IDR");
    assert!(
        first.sps.is_some() && first.pps.is_some(),
        "IDR carries SPS/PPS"
    );
    // The SPS on the wire signals BT.709 limited range.
    let sps_nal = first.sps.as_deref().unwrap();
    let summary = crate::video::sps::summary(sps_nal).expect("SPS parses");
    eprintln!("SPS: {summary:?}");
    assert!(summary.colour.is_bt709_limited(), "{:?}", summary.colour);
    assert!(
        aus[1..15].iter().all(|a| !a.keyframe),
        "no spontaneous IDRs"
    );
    let forced: Vec<_> = aus[15..].iter().filter(|a| a.keyframe).collect();
    assert!(!forced.is_empty(), "forced IDR honoured");
    for k in forced {
        let au = AccessUnit::from_annexb(&k.data);
        assert!(
            au.sps.is_some() && au.pps.is_some(),
            "forced IDR self-contained"
        );
    }
}

#[test]
fn hardware_encoder_smoke() {
    smoke(EncoderPref::Hardware);
}

#[test]
fn software_encoder_smoke() {
    smoke(EncoderPref::Software);
}

/// GPU input: a BGRA texture through the video processor
/// into the hardware MFT decodes back close to the source (colours
/// included: BT.709 limited on both ends). Skips without hardware.
#[test]
fn gpu_input_round_trip_psnr() {
    let (w, h) = (640u32, 384u32);
    let Ok(device) = gpu_device() else {
        eprintln!("skipping: no D3D11 device");
        return;
    };
    let mut enc = match H264Encoder::new_gpu(&device, w, h, 30, 8_000_000) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("skipping: no GPU-input encoder ({e})");
            return;
        }
    };
    let mut dec = H264Decoder::new().unwrap();
    let tex = test_texture(&device, w, h);
    // SAFETY: a getter on the live device created above.
    let ctx = unsafe { device.GetImmediateContext().unwrap() };
    let mut out = Vec::new();
    let mut psnrs = Vec::new();
    let mut sent = std::collections::HashMap::new();
    for t in 0..40u32 {
        let px = picture(w, h, t);
        // SAFETY: `px` holds `h` rows of `w * 4` bytes, exactly the
        // `w x h` BGRA texture being updated (row pitch `w * 4`).
        unsafe { ctx.UpdateSubresource(&tex, 0, None, px.as_ptr().cast(), w * 4, 0) };
        let pts = 5_000_000 + u64::from(t) * 33_333;
        sent.insert(pts, px);
        for au in enc.encode_texture(&tex, (0, 0, w, h), false, pts).unwrap() {
            if dec.decode(&au.data, w, h, &mut out).unwrap() {
                if let Some(src) = sent.get(&au.pts_us) {
                    psnrs.push(psnr_bgra(src, &out));
                }
            }
        }
    }
    let tail = &psnrs[psnrs.len().saturating_sub(20)..];
    let avg = tail.iter().sum::<f64>() / tail.len().max(1) as f64;
    eprintln!(
        "GPU-input round trip via {}: {} pictures, PSNR avg {avg:.1} dB",
        enc.name(),
        psnrs.len()
    );
    assert!(
        psnrs.len() >= 35 && avg > 33.0,
        "PSNR {avg:.1} dB over {}",
        psnrs.len()
    );
}

/// Wait until the GPU finished everything queued so far.
fn gpu_sync(device: &ID3D11Device, ctx: &ID3D11DeviceContext) {
    // SAFETY: D3D11 calls on live device/context references; `GetData`
    // writes at most 4 bytes into the local `done` (an i32).
    unsafe {
        let qd = D3D11_QUERY_DESC {
            Query: D3D11_QUERY_EVENT,
            MiscFlags: 0,
        };
        let mut q: Option<ID3D11Query> = None;
        device.CreateQuery(&qd, Some(&mut q)).unwrap();
        let q = q.unwrap();
        ctx.End(&q);
        ctx.Flush();
        let mut done: i32 = 0;
        while ctx
            .GetData(&q, Some((&mut done as *mut i32).cast()), 4, 0)
            .is_err()
            || done == 0
        {
            std::thread::yield_now();
        }
    }
}

fn test_texture(device: &ID3D11Device, w: u32, h: u32) -> ID3D11Texture2D {
    let td = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex = None;
    // SAFETY: valid texture description, no initial data, a local out-pointer.
    unsafe { device.CreateTexture2D(&td, None, Some(&mut tex)).unwrap() };
    tex.unwrap()
}

/// Benchmark, not a correctness test (it only prints timings): per-frame
/// encode latency of the hardware MFT with system-memory NV12 (CPU
/// BGRA->NV12 + upload) vs D3D11 input (GPU video-processor conversion),
/// 1080p and a 640x360 region. Run with
/// `cargo test --release -p ovapp --features mf -- --ignored --nocapture bench_gpu_vs_cpu_input`.
#[test]
#[ignore]
fn bench_gpu_vs_cpu_input() {
    let Ok(device) = gpu_device() else {
        println!("no D3D11 device");
        return;
    };
    // SAFETY: a getter on the live device created above.
    let ctx = unsafe { device.GetImmediateContext().unwrap() };
    for (w, h) in [(1920u32, 1080u32), (640, 360), (1920, 1080), (640, 360)] {
        let frames = 120u32;
        let bitrate = crate::video::target_bitrate(w, h, 30, None);
        let pics: Vec<Vec<u8>> = (0..frames).map(|t| picture(w, h, t)).collect();
        let tex = test_texture(&device, w, h);
        let ms = |d: Duration| d.as_secs_f64() * 1000.0 / f64::from(frames);
        if let Ok(mut cpu) = H264Encoder::new(w, h, 30, bitrate, EncoderPref::Hardware) {
            let mut total = Duration::ZERO;
            let mut n = 0;
            for (t, px) in pics.iter().enumerate() {
                let t0 = Instant::now();
                n += cpu
                    .encode(px, w as usize * 4, false, 1_000_000 + t as u64 * 33_333)
                    .unwrap()
                    .len();
                total += t0.elapsed();
            }
            println!(
                "{w}x{h} {}: CPU BGRA->NV12 + system-memory input: {:.2} ms/frame ({n} units)",
                cpu.name(),
                ms(total)
            );
        }
        match H264Encoder::new_gpu(&device, w, h, 30, bitrate) {
            Ok(mut gpu) => {
                let mut total = Duration::ZERO;
                let mut n = 0;
                for (t, px) in pics.iter().enumerate() {
                    // The upload stands in for the duplication copy that
                    // already happened on the GPU in the host.
                    // SAFETY: `px` holds `h` rows of `w * 4` bytes, exactly
                    // the `w x h` BGRA texture being updated.
                    unsafe { ctx.UpdateSubresource(&tex, 0, None, px.as_ptr().cast(), w * 4, 0) };
                    gpu_sync(&device, &ctx);
                    let t0 = Instant::now();
                    n += gpu
                        .encode_texture(&tex, (0, 0, w, h), false, 1_000_000 + t as u64 * 33_333)
                        .unwrap()
                        .len();
                    total += t0.elapsed();
                }
                println!(
                    "{w}x{h} {}: D3D11 texture input: {:.2} ms/frame ({n} units)",
                    gpu.name(),
                    ms(total)
                );
            }
            Err(e) => println!("{w}x{h}: GPU input unavailable: {e}"),
        }
    }
}

/// encode -> decode round trip on the desktop: PSNR well above 35 dB at
/// a generous bitrate.
#[test]
fn encode_decode_round_trip_psnr() {
    let (w, h) = (640, 384);
    let mut enc = match H264Encoder::new(w, h, 30, 8_000_000, EncoderPref::Auto) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("skipping round trip: no encoder ({e})");
            return;
        }
    };
    let mut dec = match H264Decoder::new() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("skipping round trip: no decoder ({e})");
            return;
        }
    };
    let mut out = Vec::new();
    let mut psnrs = Vec::new();
    let mut decoded = 0;
    let mut sent = std::collections::HashMap::new();
    for t in 0..40u32 {
        let px = picture(w, h, t);
        let pts = 5_000_000 + u64::from(t) * 33_333;
        sent.insert(pts, px.clone());
        for au in enc.encode(&px, w as usize * 4, false, pts).unwrap() {
            if dec.decode(&au.data, w, h, &mut out).unwrap() {
                decoded += 1;
                if let Some(src) = sent.get(&au.pts_us) {
                    psnrs.push(psnr_bgra(src, &out));
                }
            }
        }
    }
    assert!(decoded >= 35, "decoded {decoded} of 40");
    let tail = &psnrs[psnrs.len().saturating_sub(20)..];
    let avg = tail.iter().sum::<f64>() / tail.len().max(1) as f64;
    eprintln!(
        "round trip via {}: {decoded} pictures, PSNR avg {avg:.1} dB",
        enc.name()
    );
    assert!(avg > 35.0, "PSNR {avg:.1} dB");
}

/// Benchmark, not a correctness test (it only prints measurements): encode
/// latency hw vs sw, decode latency, bitrate at 30 fps for a 640x360 video
/// region (inside a 1080p desktop) and a full-screen scrolling page, against
/// lossless tiles on the very same content. Run with
/// `cargo test --release -p ovapp --features mf,zstd-encode -- --ignored --nocapture bench_h264`.
#[test]
#[ignore]
fn bench_h264() {
    use crate::sendq::{FrameMailbox, UpdateEncoder};
    use crate::video::synth_desktop;
    let (sw, sh) = (1920u32, 1080u32);
    let frames = 150u32;
    let secs = f64::from(frames) / 30.0;
    let content = |px: &mut Vec<u8>, scroll: bool, r: TileRect, t: u32| {
        if scroll {
            synth_scroll(px, sw, r, t, 8);
        } else {
            synth_video(px, sw, r, t);
        }
    };
    for (label, scroll, r) in [
        (
            "video region 640x360",
            false,
            TileRect {
                x: 640,
                y: 320,
                w: 640,
                h: 360,
            },
        ),
        (
            "scrolling page 1920x1080",
            true,
            TileRect {
                x: 0,
                y: 0,
                w: 1920,
                h: 1080,
            },
        ),
        (
            "full-screen video 1920x1080",
            false,
            TileRect {
                x: 0,
                y: 0,
                w: 1920,
                h: 1080,
            },
        ),
    ] {
        // Lossless tiles (zstd when built with zstd-encode) baseline.
        let mut px = vec![0u8; (sw * sh * 4) as usize];
        synth_desktop(&mut px, sw, sh);
        let mut enc = UpdateEncoder::new();
        enc.set_peer_caps(crate::terminal::LOCAL_CAPS);
        let mb = FrameMailbox::default();
        enc.submit(&px, sw, sh, None, &mb);
        mb.take();
        let mut lossless = 0usize;
        let t0 = Instant::now();
        for t in 0..frames {
            content(&mut px, scroll, r, t);
            enc.submit(&px, sw, sh, None, &mb);
            if let Some(f) = mb.take() {
                lossless += f.data.len();
            }
        }
        println!(
            "{label}: lossless tiles {:.0} KB/s ({:.2} ms/frame encode)",
            lossless as f64 / 1024.0 / secs,
            t0.elapsed().as_secs_f64() * 1000.0 / f64::from(frames)
        );
        for pref in [EncoderPref::Hardware, EncoderPref::Software] {
            let (w, h) = (u32::from(r.w), u32::from(r.h));
            let bitrate = crate::video::target_bitrate(w, h, 30, None);
            let mut enc = match H264Encoder::new(w, h, 30, bitrate, pref) {
                Ok(e) => e,
                Err(e) => {
                    println!("  {pref:?}: unavailable: {e}");
                    continue;
                }
            };
            let mut dec = H264Decoder::new().unwrap();
            let (mut bytes, mut n, mut psnr, mut pn) = (0usize, 0u32, 0f64, 0u32);
            let (mut conv, mut enc_t, mut dec_t, mut back) = (
                Duration::ZERO,
                Duration::ZERO,
                Duration::ZERO,
                Duration::ZERO,
            );
            let mut nv = Vec::new();
            let mut out = Vec::new();
            let off = usize::from(r.y) * sw as usize * 4 + usize::from(r.x) * 4;
            for t in 0..frames {
                content(&mut px, scroll, r, t);
                let c0 = Instant::now();
                bgra_to_nv12(&px[off..], sw as usize * 4, w, h, &mut nv);
                conv += c0.elapsed();
                let e0 = Instant::now();
                let aus = enc
                    .encode_nv12(&nv, false, 1_000_000 + u64::from(t) * 33_333)
                    .unwrap();
                enc_t += e0.elapsed();
                for au in aus {
                    bytes += au.data.len();
                    let d0 = Instant::now();
                    if dec.decode_nv12(&au.data).unwrap() {
                        dec_t += d0.elapsed();
                        let b0 = Instant::now();
                        let (nv12, pitch, _, ch) = dec.picture();
                        nv12_to_bgra(
                            nv12,
                            pitch as usize,
                            pitch as usize * ch as usize,
                            w,
                            h,
                            &mut out,
                        )
                        .unwrap();
                        back += b0.elapsed();
                        n += 1;
                        if t + 10 >= frames && au.pts_us == 1_000_000 + u64::from(t) * 33_333 {
                            let mut src = Vec::with_capacity(out.len());
                            for y in 0..h as usize {
                                let o = off + y * sw as usize * 4;
                                src.extend_from_slice(&px[o..o + w as usize * 4]);
                            }
                            psnr += psnr_bgra(&src, &out);
                            pn += 1;
                        }
                    }
                }
            }
            let ms = |d: Duration, k: u32| d.as_secs_f64() * 1000.0 / f64::from(k.max(1));
            println!(
                "  {}: encode {:.2} ms/frame + BGRA->NV12 {:.2} ms; decode {:.2} ms + NV12->BGRA {:.2} ms; {:.0} KB/s ({:.2} Mbps, target {:.1}) = {:.1}x less than lossless; PSNR {:.1} dB",
                enc.name(),
                ms(enc_t, frames),
                ms(conv, frames),
                ms(dec_t, n),
                ms(back, n),
                bytes as f64 / 1024.0 / secs,
                bytes as f64 * 8.0 / 1e6 / secs,
                f64::from(bitrate) / 1e6,
                lossless as f64 / bytes.max(1) as f64,
                psnr / f64::from(pn.max(1))
            );
        }
    }
}
