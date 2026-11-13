//! Cursor compositing for the GDI path: the remote cursor bitmap blended
//! into a cached copy of the frame. (The Direct2D path draws the cursor as
//! its own layer instead.)
//!
//! Pure and target-independent: unit-tested on all platforms.

// Only the Windows paint path composites; the tests run everywhere.
#![cfg_attr(not(windows), allow(dead_code))]

use super::CursorBmp;
use ovapp::screen::Frame;

#[cfg(test)]
mod tests;

/// The remote cursor overlay state, updated from `CURSOR`/`CURSOR_SHAPE`
/// commands. Positions are in *frame pixel space* (the captured region), so
/// they composite straight onto the frame buffer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct CursorOverlay {
    pub(super) pos: Option<(i32, i32)>,
    pub(super) shape: Option<CursorBmp>,
}

/// Blend a premultiplied-BGRA cursor bitmap over a BGRA frame buffer at the
/// hotspot-anchored position `x, y` (top-left of the cursor bitmap is
/// `(x - hotspot_x, y - hotspot_y)`). Pixels outside the frame are clipped.
fn blend_cursor(dst: &mut [u8], stride: usize, fw: u32, fh: u32, cur: &CursorBmp, x: i32, y: i32) {
    let (cw, ch) = (i32::from(cur.w), i32::from(cur.h));
    let (fw, fh) = (fw as i32, fh as i32);
    if cw == 0 || ch == 0 {
        return;
    }
    // Top-left corner of the bitmap in frame coordinates.
    let (ox, oy) = (x - i32::from(cur.hotspot_x), y - i32::from(cur.hotspot_y));
    for cy in 0..ch {
        let fy = oy + cy;
        if fy < 0 || fy >= fh {
            continue;
        }
        for cx in 0..cw {
            let fx = ox + cx;
            if fx < 0 || fx >= fw {
                continue;
            }
            let s = ((cy * cw + cx) * 4) as usize;
            if s + 4 > cur.bgra.len() {
                return; // malformed bitmap: stop rather than read out of bounds
            }
            let (sb, sg, sr, sa) = (
                cur.bgra[s],
                cur.bgra[s + 1],
                cur.bgra[s + 2],
                cur.bgra[s + 3],
            );
            let inv = 255 - i32::from(sa);
            let d = (fy as usize * stride) + fx as usize * 4;
            // Premultiplied alpha OVER: out = src + dst * (1 - a) / 255.
            dst[d] = sb + ((i32::from(dst[d]) * inv) / 255) as u8;
            dst[d + 1] = sg + ((i32::from(dst[d + 1]) * inv) / 255) as u8;
            dst[d + 2] = sr + ((i32::from(dst[d + 2]) * inv) / 255) as u8;
            dst[d + 3] = 255;
        }
    }
}

/// Copy the frame pixels under `cur` placed at `(x, y)` (hotspot-anchored,
/// clipped like [`blend_cursor`]) from `src` back into `dst` — undoes a
/// previous blend without touching the rest of the buffer.
#[allow(clippy::too_many_arguments)]
fn restore_cursor_rect(
    dst: &mut [u8],
    src: &[u8],
    stride: usize,
    fw: u32,
    fh: u32,
    cur: &CursorBmp,
    x: i32,
    y: i32,
) {
    let (ox, oy) = (x - i32::from(cur.hotspot_x), y - i32::from(cur.hotspot_y));
    let x0 = ox.clamp(0, fw as i32) as usize;
    let x1 = (ox + i32::from(cur.w)).clamp(0, fw as i32) as usize;
    let y0 = oy.clamp(0, fh as i32) as usize;
    let y1 = (oy + i32::from(cur.h)).clamp(0, fh as i32) as usize;
    if x0 >= x1 {
        return;
    }
    for row in y0..y1 {
        let a = row * stride + x0 * 4;
        let b = row * stride + x1 * 4;
        if b > dst.len() || b > src.len() {
            return;
        }
        dst[a..b].copy_from_slice(&src[a..b]);
    }
}

/// Composite cache for the paint path: `(frame seq, overlay, buffer)`.
pub(super) type Display = Option<(u64, CursorOverlay, Vec<u8>)>;

/// Return the frame with the cursor overlay blended in, reusing the cached
/// buffer: unchanged -> reuse as is; same frame, moved/changed cursor ->
/// restore the old cursor rect and blend the new one; new frame -> copy into
/// the existing allocation and blend.
pub(super) fn compose<'a>(
    disp: &'a mut Display,
    frame: &Frame,
    overlay: CursorOverlay,
) -> &'a [u8] {
    let stride = frame.stride as usize;
    let prev = disp.take();
    let mut buf = match prev {
        Some((seq, old, buf)) if seq == frame.seq && old == overlay => {
            return &disp.insert((seq, old, buf)).2;
        }
        Some((seq, old, mut buf)) if seq == frame.seq && buf.len() == frame.pixels.len() => {
            if let (Some((ox, oy)), Some(oc)) = (old.pos, old.shape.as_ref()) {
                restore_cursor_rect(
                    &mut buf,
                    &frame.pixels,
                    stride,
                    frame.width,
                    frame.height,
                    oc,
                    ox,
                    oy,
                );
            }
            buf
        }
        other => {
            let mut buf = other.map(|(_, _, b)| b).unwrap_or_default();
            buf.clear();
            buf.extend_from_slice(&frame.pixels);
            buf
        }
    };
    if let (Some((x, y)), Some(cur)) = (overlay.pos, overlay.shape.as_ref()) {
        blend_cursor(&mut buf, stride, frame.width, frame.height, cur, x, y);
    }
    &disp.insert((frame.seq, overlay, buf)).2
}
