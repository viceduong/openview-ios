//! Offscreen Direct2D scaling test.

use super::*;
use windows::Win32::Graphics::Direct2D::{D2D1_BITMAP_OPTIONS_CPU_READ, D2D1_MAP_OPTIONS_READ};

/// Offscreen: the high-quality cubic draw of a 1-px checkerboard at 1:2
/// comes out flat grey (pre-filtered, no aliasing) on this machine's
/// D3D11 device. Skips when no device exists.
#[test]
fn d2d_hq_cubic_downscale_offscreen() {
    let Ok((device, driver)) = create_device() else {
        eprintln!("skipping: no D3D11 device");
        return;
    };
    let dxgi: IDXGIDevice1 = device.cast().unwrap();
    let dc = create_dc(&dxgi).unwrap();
    // SAFETY: single-threaded use of live COM objects; `px` outlives the
    // bitmap creation that copies it, and the mapped rows are read only
    // within `pitch * dh` bytes before `Unmap`.
    unsafe {
        let (w, h) = (256u32, 256u32);
        let mut px = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let v = if (x + y) % 2 == 0 { 255 } else { 0 };
                let i = ((y * w + x) * 4) as usize;
                px[i..i + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        let props = |opts| D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: opts,
            colorContext: std::mem::ManuallyDrop::new(None),
        };
        let src = dc
            .CreateBitmap(
                D2D_SIZE_U {
                    width: w,
                    height: h,
                },
                Some(px.as_ptr().cast()),
                w * 4,
                &props(D2D1_BITMAP_OPTIONS_NONE),
            )
            .unwrap();
        let (dw, dh) = (w / 2, h / 2);
        let target = dc
            .CreateBitmap(
                D2D_SIZE_U {
                    width: dw,
                    height: dh,
                },
                None,
                0,
                &props(D2D1_BITMAP_OPTIONS_TARGET),
            )
            .unwrap();
        dc.SetTarget(&target);
        dc.BeginDraw();
        let r = D2D_RECT_F {
            left: 0.0,
            top: 0.0,
            right: dw as f32,
            bottom: dh as f32,
        };
        let t0 = std::time::Instant::now();
        dc.DrawBitmap(
            &src,
            Some(&r),
            1.0,
            D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC,
            None,
            None,
        );
        dc.EndDraw(None, None).unwrap();
        let read = dc
            .CreateBitmap(
                D2D_SIZE_U {
                    width: dw,
                    height: dh,
                },
                None,
                0,
                &props(D2D1_BITMAP_OPTIONS_CPU_READ | D2D1_BITMAP_OPTIONS_CANNOT_DRAW),
            )
            .unwrap();
        read.CopyFromBitmap(None, &target, None).unwrap();
        let m = read.Map(D2D1_MAP_OPTIONS_READ).unwrap();
        eprintln!(
            "D2D ({driver}) HQ cubic {w}x{h} -> {dw}x{dh}: {:.2} ms incl. readback setup",
            t0.elapsed().as_secs_f64() * 1000.0
        );
        let mut lo = 255u8;
        let mut hi = 0u8;
        // Interior only (edges see the transparent outside).
        for y in 4..dh - 4 {
            let row =
                std::slice::from_raw_parts(m.bits.add((y * m.pitch) as usize), (dw * 4) as usize);
            for x in 4..dw - 4 {
                let v = row[(x * 4) as usize];
                lo = lo.min(v);
                hi = hi.max(v);
            }
        }
        read.Unmap().unwrap();
        eprintln!("checkerboard -> grey range {lo}..={hi}");
        assert!(lo >= 100 && hi <= 155, "aliasing: {lo}..={hi}");
    }
}
