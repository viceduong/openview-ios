//! Synthetic content for the host `--test-pattern`, tests and benches.

use crate::tiles::TileRect;

/// A desktop-like static background: light window chrome with dark "text"
/// runs (deterministic).
pub fn synth_desktop(px: &mut [u8], w: u32, h: u32) {
    let stride = w as usize * 4;
    for y in 0..h as usize {
        for x in 0..w as usize {
            let o = y * stride + x * 4;
            let band = (y / 18) % 7;
            let glyph = ((x / 7) * 31 + y / 18 * 17) % 5;
            let ink = band != 0
                && (y % 18) > 4
                && (y % 18) < 14
                && glyph < 3
                && (x % 7) < 5
                && (x / 300) % 2 == 0;
            let v = if ink {
                40
            } else if (x / 300) % 2 == 1 {
                230
            } else {
                250
            };
            px[o] = v;
            px[o + 1] = v;
            px[o + 2] = if ink { 40 } else { v.saturating_sub(5) };
            px[o + 3] = 255;
        }
    }
}

/// Paint frame `t` of a moving, video-like picture into `rect`: drifting
/// gradients, a moving fine texture and a bouncing disc (compressible like
/// real video, different every frame).
pub fn synth_video(px: &mut [u8], w: u32, rect: TileRect, t: u32) {
    let stride = w as usize * 4;
    let (rw, rh) = (u32::from(rect.w), u32::from(rect.h));
    let cx = ((t * 7) % (rw.max(2) * 2)) as i32;
    let cx = if cx >= rw as i32 {
        2 * rw as i32 - cx
    } else {
        cx
    };
    let cy = (f64::from(t) * 0.11)
        .sin()
        .mul_add(f64::from(rh) * 0.35, f64::from(rh) * 0.5) as i32;
    let rad = (rh / 5).max(4) as i32;
    for y in 0..rh {
        let row = (usize::from(rect.y) + y as usize) * stride;
        for x in 0..rw {
            let o = row + (usize::from(rect.x) + x as usize) * 4;
            let dx = x as i32 - cx;
            let dy = y as i32 - cy;
            let disc = dx * dx + dy * dy < rad * rad;
            // A texture panning 2 px per frame (detail the encoder must track).
            let tx = x + 2 * t;
            let texture = ((((tx / 4) ^ (y / 4)) & 3) * 12) as i32;
            let r = ((x + t * 3) % 256) as i32;
            let g = ((y * 2 + t * 2) % 256) as i32;
            let b = (((x + y) / 2 + t * 5) % 256) as i32;
            let (r, g, b) = if disc { (240, 200, 40) } else { (r, g, b) };
            px[o] = (b / 2 + texture).clamp(0, 255) as u8;
            px[o + 1] = (g / 2 + texture).clamp(0, 255) as u8;
            px[o + 2] = (r / 2 + texture).clamp(0, 255) as u8;
            px[o + 3] = 255;
        }
    }
}

/// Frame `t` of a page scrolling up by `speed` pixels per frame inside
/// `rect` (text-like rows on white).
pub fn synth_scroll(px: &mut [u8], w: u32, rect: TileRect, t: u32, speed: u32) {
    let stride = w as usize * 4;
    let off = t.wrapping_mul(speed);
    for y in 0..u32::from(rect.h) {
        let py = y.wrapping_add(off); // page coordinate
        let row = (usize::from(rect.y) + y as usize) * stride;
        let line = py / 22;
        let in_line = py % 22;
        for x in 0..u32::from(rect.w) {
            let o = row + (usize::from(rect.x) + x as usize) * 4;
            let word = (x / 9 + line * 13) % 11;
            let ink = in_line > 5
                && in_line < 17
                && word < 8
                && (x % 9) < 6
                && ((x / 9 * 7 + line * 3 + (x % 9)) % 4) != 0
                && x > 20
                && x + 20 < u32::from(rect.w)
                && (line % 9) != 0;
            let heading = line % 9 == 1;
            let v: u8 = if ink {
                if heading {
                    20
                } else {
                    60
                }
            } else {
                255
            };
            px[o] = if ink && heading { 160 } else { v };
            px[o + 1] = v;
            px[o + 2] = v;
            px[o + 3] = 255;
        }
    }
}
