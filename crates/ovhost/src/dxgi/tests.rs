use super::*;

#[test]
fn copy_opaque_sets_every_alpha_at_any_alignment() {
    let src: Vec<u8> = (0..400u32).map(|i| (i * 7) as u8).collect();
    let mut buf = vec![0u8; 404];
    for off in 0..4 {
        let n = 4 * 90;
        copy_opaque(&mut buf[off..off + n], &src[..n]);
        for (i, (d, s)) in buf[off..off + n].iter().zip(&src[..n]).enumerate() {
            assert_eq!(*d, if i % 4 == 3 { 255 } else { *s }, "off {off} byte {i}");
        }
    }
}

/// CI-safe smoke test: duplicating the primary monitor either works
/// (then frames are well-formed and polling never errors spuriously)
/// or fails cleanly with an error message (RDP / headless / secure
/// desktop) — it must never panic.
/// GPU encode input: the GPU mirror holds the same picture as
/// the CPU mirror — encoding it via D3D11 input and decoding gives the
/// CPU pixels back (high PSNR). Skips without DXGI / a GPU encoder.
#[test]
fn gpu_mirror_matches_cpu_mirror_through_the_encoder() {
    use ovapp::video::VideoEncode;
    ovapp::gdisrc::enable_dpi_awareness();
    GPU_ENCODE.store(true, std::sync::atomic::Ordering::Relaxed);
    let mons = ovapp::monitors::enumerate_monitors();
    let Some(m) = mons.iter().find(|m| m.is_primary).or(mons.first()) else {
        return;
    };
    let (w, h) = (m.w.min(640) & !15, m.h.min(352) & !15);
    let Ok(mut d) = DxgiCapture::new(m.x, m.y, w, h) else {
        eprintln!("skipping: no DXGI");
        return;
    };
    let Some(gf) = d.gpu_frame() else {
        eprintln!("skipping: no GPU mirror");
        return;
    };
    let _ = d.update();
    let (px, w, h, _) = d.frame();
    let px = px.to_vec();
    let mut enc = match ovapp::mfcodec::MfVideoEncoder::new(
        w,
        h,
        30,
        20_000_000,
        ovapp::mfcodec::EncoderPref::Hardware,
    ) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("skipping: {e}");
            return;
        }
    };
    let mut dec = ovapp::mfcodec::H264Decoder::new().unwrap();
    let mut out = Vec::new();
    let mut best = 0.0f64;
    for t in 0..10u64 {
        match enc.encode_gpu(&gf, (0, 0, w, h), false, 1_000_000 + t * 33_333) {
            Some(Ok(aus)) => {
                for au in aus {
                    if ovapp::video::VideoDecode::decode(&mut dec, &au.data, w, h, &mut out)
                        .unwrap()
                    {
                        best = best.max(ovapp::video::psnr_bgra(&px, &out));
                    }
                }
            }
            other => {
                eprintln!(
                    "skipping: GPU input unavailable ({:?})",
                    other.and_then(|r| r.err()).or(enc.gpu_error.clone())
                );
                return;
            }
        }
    }
    eprintln!(
        "GPU mirror via {}: PSNR {best:.1} dB vs the CPU mirror",
        enc.name()
    );
    assert!(
        best > 30.0,
        "GPU mirror differs from the CPU mirror: {best:.1} dB"
    );
}

/// Regression: a duplication opened without waiting returns at once,
/// with or without an initial image (a monitor-less output never
/// presents one: that wait used to stall the capture thread ~1.1 s on
/// every retry). Without an image the mirror is unknown (no hints).
#[test]
fn open_region_without_wait_never_blocks() {
    ovapp::gdisrc::enable_dpi_awareness();
    let mons = ovapp::monitors::enumerate_monitors();
    let Some(m) = mons.iter().find(|m| m.is_primary).or(mons.first()) else {
        eprintln!("no monitors: skipping");
        return;
    };
    let t0 = std::time::Instant::now();
    match DxgiCapture::open_region(m.x, m.y, m.w, m.h, 0) {
        Err(e) => eprintln!("DXGI unavailable here (fine): {e}"),
        Ok(mut d) => {
            for _ in 0..3 {
                if let Err(e) = d.update() {
                    eprintln!("lost mid-test (fine): {e}");
                    break;
                }
                let (px, w, h, hints) = d.frame();
                assert_eq!(px.len(), (w * h * 4) as usize);
                if !d.has_image() {
                    assert!(hints.is_none(), "no image yet: nothing is known");
                }
            }
            eprintln!("has image: {}", d.has_image());
        }
    }
    let took = t0.elapsed();
    assert!(
        took < std::time::Duration::from_millis(700),
        "open + polls took {took:?}"
    );
}

#[test]
fn dxgi_smoke_degrades_gracefully() {
    ovapp::gdisrc::enable_dpi_awareness();
    let mons = ovapp::monitors::enumerate_monitors();
    let Some(m) = mons.iter().find(|m| m.is_primary).or(mons.first()) else {
        eprintln!("no monitors: skipping");
        return;
    };
    match DxgiCapture::new(m.x, m.y, m.w, m.h) {
        Err(e) => eprintln!("DXGI unavailable here (fine): {e}"),
        Ok(mut d) => {
            let (px, w, h, hints) = d.frame();
            assert_eq!((w, h), (m.w, m.h));
            assert_eq!(px.len(), (w * h * 4) as usize);
            assert!(hints.is_none(), "first image: full compare");
            assert!(px.as_chunks::<4>().0.iter().all(|p| p[3] == 255));
            for _ in 0..5 {
                match d.update() {
                    Ok(()) => {
                        let (_, _, _, hints) = d.frame();
                        for r in hints.unwrap_or(&[]) {
                            assert!(r.fits(w, h), "{r:?}");
                        }
                    }
                    Err(e) => eprintln!("lost mid-test (fine): {e}"),
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }
    // Event-driven wait: returns within the timeout, and an
    // update afterwards keeps well-formed hints.
    if let Ok(mut d) = DxgiCapture::new(m.x, m.y, m.w, m.h) {
        let t0 = std::time::Instant::now();
        match d.wait(60) {
            Ok(changed) => {
                assert!(t0.elapsed() < std::time::Duration::from_secs(2));
                d.update().ok();
                let (_, w, h, hints) = d.frame();
                if changed {
                    assert!(
                        hints.is_some_and(|h| !h.is_empty()),
                        "a waited change keeps its hints"
                    );
                }
                for r in hints.unwrap_or(&[]) {
                    assert!(r.fits(w, h), "{r:?}");
                }
            }
            Err(e) => eprintln!("lost mid-test (fine): {e}"),
        }
    }
    // A region no output contains fails cleanly.
    assert!(DxgiCapture::new(-100_000, -100_000, 10, 10).is_err());
}
