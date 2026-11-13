//! `WM_PAINT`: hand the frame to the Direct2D presenter, or — when there is
//! none (or it just failed) — draw it with GDI `StretchDIBits`, CPU-scaled
//! for a real downscale, with the cursor composited in and the HUD on top.

use std::sync::atomic::Ordering;

use windows::Win32::Graphics::Gdi::{
    SetBrushOrgEx, SetStretchBltMode, StretchDIBits, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    COLORONCOLOR, DIB_RGB_COLORS, HALFTONE, HDC, ROP_CODE, SRCCOPY,
};

use super::{cursor, overlay, WindowState};
use ovapp::scale::Scaler;

/// Below this fit scale a frame is pre-scaled by the CPU area/sharpen
/// scaler and then blitted 1:1.
const CPU_SCALE_BELOW: f32 = 0.97;

/// Below this remaining blit scale GDI uses `HALFTONE` filtering.
const HALFTONE_BELOW: f32 = 0.75;

/// The raster operation passed to `StretchDIBits`: `SRCCOPY` (copy the
/// source). A ROP of 0 is the ternary code `BLACKNESS` (its index byte is
/// 0x00) and paints the destination black — see the test below.
const BLIT_ROP: ROP_CODE = SRCCOPY;

/// Repaint the stored frame with the cursor overlay composited on top,
/// scaled to the stored content rect (letterboxed by aspect).
///
/// Direct2D first; any failure drops to GDI for good. On the GDI path,
/// without a visible cursor overlay the frame is blitted straight from its
/// own buffer (no copy). Otherwise the composite is cached in
/// `state.display` keyed by `(frame seq, overlay)` and its buffer is reused:
/// a new frame costs one copy into the existing allocation, and a cursor
/// move between frames only restores the old cursor rectangle from the frame
/// and blends the new one — O(cursor area), not O(frame).
///
/// # Safety
///
/// `hdc` must be the live DC from `BeginPaint` for the window owning
/// `state`, on that window's thread.
pub(super) unsafe fn paint(hdc: HDC, state: &WindowState) {
    let Ok(guard) = state.last_frame.lock() else {
        return;
    };
    let hud = state.hud.lock().map(|h| h.clone()).unwrap_or_default();
    if let Ok(mut slot) = state.d2d.lock() {
        if let Some(r) = slot.as_mut() {
            let overlay = state.cursor.lock().map(|c| c.clone()).unwrap_or_default();
            let rect = state.content_rect();
            match r.render(guard.as_ref(), &overlay, &hud, rect, state.client_size()) {
                Ok(true) => return,
                Ok(false) => {
                    state.repaint_pending.store(true, Ordering::Relaxed);
                    return;
                }
                Err(e) => {
                    tracing::warn!("Direct2D presenter failed ({e}); falling back to GDI");
                    *slot = None;
                }
            }
        }
    }
    let Some(frame) = guard.as_ref() else {
        // No frame yet: still surface the dead-peer banner.
        if hud.dead {
            // SAFETY: `hdc` is live (this function's contract).
            unsafe { overlay::draw_hud(hdc, &hud, 8, 8, 600) };
        }
        return;
    };
    if !frame.is_complete() || frame.width == 0 || frame.height == 0 {
        return;
    }
    let Ok(rect) = state.content_rect.lock() else {
        return;
    };
    let overlay = state.cursor.lock().map(|c| c.clone()).unwrap_or_default();
    // Build (or reuse) the composited display buffer.
    let Ok(mut disp) = state.display.lock() else {
        return;
    };
    let key_overlay = overlay.clone();
    let mut pixels: &[u8] = if overlay.pos.is_none() || overlay.shape.is_none() {
        // Nothing to composite: blit the frame itself.
        &frame.pixels
    } else {
        cursor::compose(&mut disp, frame, overlay)
    };
    // A real downscale goes through the CPU area/sharpen scaler (area +
    // sharpen 0.5) and is then blitted 1:1.
    let mut src_w = frame.width as i32;
    let mut src_h = frame.height as i32;
    let shrink = (rect.w as f32 / frame.width as f32).min(rect.h as f32 / frame.height as f32);
    let Ok(mut scaled) = state.scaled.lock() else {
        return;
    };
    if shrink < CPU_SCALE_BELOW && rect.w > 0 && rect.h > 0 {
        let dims = (frame.width, frame.height, rect.w as u32, rect.h as u32);
        let key = (frame.seq, key_overlay, rect.w, rect.h);
        let stale = match scaled.as_ref() {
            Some((sc, k, _)) => *k != key || sc.dims() != dims,
            None => true,
        };
        if stale {
            let (mut sc, mut buf) = match scaled.take() {
                Some((sc, _, buf)) if sc.dims() == dims => (sc, buf),
                other => (
                    Scaler::area_sharpen(dims.0, dims.1, dims.2, dims.3),
                    other.map(|(_, _, buf)| buf).unwrap_or_default(),
                ),
            };
            sc.scale(pixels, frame.stride as usize, &mut buf);
            *scaled = Some((sc, key, buf));
        }
        if let Some((_, _, buf)) = scaled.as_ref() {
            pixels = buf;
            src_w = rect.w;
            src_h = rect.h;
        }
    }
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: src_w,
            biHeight: -src_h, // negative = top-down, like our BGRA buffers
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            biSizeImage: pixels.len() as u32,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        },
        // DIB_RGB_COLORS reads no palette entries for a 32-bpp BI_RGB DIB.
        bmiColors: Default::default(),
    };
    // COLORONCOLOR: fast, artifact-free for the usual ~1:1 / upscale case
    // (the default BLACKONWHITE ANDs dropped rows into dark smears);
    // HALFTONE (needs a brush-origin reset) only for a strong downscale
    // where its filtering visibly helps text. A real downscale was already
    // done by the CPU scaler above; HALFTONE remains for a mild one it
    // skipped.
    let downscale = (rect.w as f32 / src_w as f32).min(rect.h as f32 / src_h as f32);
    // SAFETY: `hdc` is live (contract); `pixels` holds `src_w * src_h` BGRA
    // pixels (a complete frame or its scaled copy) and outlives the blit,
    // which reads exactly that many through `bmi`.
    unsafe {
        if downscale < HALFTONE_BELOW {
            SetStretchBltMode(hdc, HALFTONE);
            let _ = SetBrushOrgEx(hdc, 0, 0, None);
        } else {
            SetStretchBltMode(hdc, COLORONCOLOR);
        }
        StretchDIBits(
            hdc,
            rect.x,
            rect.y,
            rect.w,
            rect.h, // destination (letterboxed content rect)
            0,
            0,
            src_w,
            src_h, // source (whole frame, or its pre-scaled copy)
            Some(pixels.as_ptr().cast()),
            &bmi,
            DIB_RGB_COLORS,
            BLIT_ROP,
        );
    }
    // The HUD: the health panel top-left, and the dead-peer banner centered
    // above it when the peer is gone. Drawn on top of the blitted frame
    // every paint.
    let (cw, _) = state.client_size();
    // SAFETY: `hdc` is live (contract).
    unsafe { overlay::draw_hud(hdc, &hud, rect.x + 8, rect.y + 8, cw) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject,
    };

    /// Blit a 4x2 BGRA frame 1:1 into a 32-bpp DIB section with `rop` and
    /// return what landed there.
    fn blit_with(rop: ROP_CODE) -> Vec<u8> {
        let (w, h) = (4i32, 2i32);
        let src: Vec<u8> = (0..(w * h * 4) as u8)
            .map(|i| i.wrapping_mul(29) | 1)
            .collect();
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            bmiColors: Default::default(),
        };
        // SAFETY: plain GDI object lifecycle on a private memory DC; `bits`
        // points at the DIB section's `w * h * 4` bytes while it is alive,
        // and every object is released before returning.
        unsafe {
            let dc = CreateCompatibleDC(None);
            assert!(!dc.is_invalid());
            let mut bits = std::ptr::null_mut();
            let dib = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
                .expect("CreateDIBSection");
            let old = SelectObject(dc, dib.into());
            // Pre-fill the destination so a no-op ROP is visible too.
            std::ptr::write_bytes(bits.cast::<u8>(), 0x55, (w * h * 4) as usize);
            SetStretchBltMode(dc, COLORONCOLOR);
            let lines = StretchDIBits(
                dc,
                0,
                0,
                w,
                h,
                0,
                0,
                w,
                h,
                Some(src.as_ptr().cast()),
                &bmi,
                DIB_RGB_COLORS,
                rop,
            );
            assert!(lines > 0, "StretchDIBits failed");
            let out = std::slice::from_raw_parts(bits.cast::<u8>(), (w * h * 4) as usize).to_vec();
            SelectObject(dc, old);
            let _ = DeleteObject(dib.into());
            let _ = DeleteDC(dc);
            // RGB channels must match (GDI may leave the X byte alone).
            out.iter()
                .zip(&src)
                .enumerate()
                .map(|(i, (o, s))| if i % 4 == 3 { *s } else { *o })
                .collect()
        }
    }

    #[test]
    fn gdi_blit_copies_pixels() {
        let (w, h) = (4usize, 2usize);
        let src: Vec<u8> = (0..(w * h * 4) as u8)
            .map(|i| i.wrapping_mul(29) | 1)
            .collect();
        assert_eq!(blit_with(BLIT_ROP), src);
        // ROP 0 (`BLACKNESS`) would blank the frame.
        assert!(blit_with(ROP_CODE(0))
            .iter()
            .enumerate()
            .all(|(i, &b)| i % 4 == 3 || b == 0));
    }
}
