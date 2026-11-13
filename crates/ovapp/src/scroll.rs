//! Host-side scroll detection: find a vertical copy-rect (MOVE)
//! between the frame the viewer holds and the new capture, so a scroll is
//! sent as one 12-byte MOVE plus the exposed strip instead of every tile
//! of the scrolled area.
//!
//! - `detect_vertical`: row hashing over the changed band (previous vs
//!   current rows, restricted to the dirty column span), a vote over the
//!   vertical shifts that map a current row onto a *unique* previous row
//!   (|dy| <= H/2), then exact verification row by row. The result covers
//!   the longest run of verified rows (short mismatching gaps — a hover
//!   highlight, a scrollbar thumb edge — are tolerated and later sent as
//!   residual tiles).
//! - `verify_move`: checks an OS-reported move (DXGI move rect) against
//!   the host mirror before it is used: a move is only trusted when the
//!   destination rows really equal the shifted source rows.
//!
//! Pure; unit-tested with synthetic scrolls.

use std::collections::HashMap;

use crate::bands::MoveRect;
use crate::tiles::{TileMap, TileRect, TILE};

/// Smallest dirty band (tile rows) worth a scroll search.
pub(crate) const MIN_ROWS: u32 = 96;
/// Smallest dirty band width (pixels).
pub(crate) const MIN_WIDTH: u32 = 128;
/// Longest run of non-matching rows still bridged inside one MOVE.
pub(crate) const MAX_GAP: usize = 8;
/// A detected move must verify at least this many rows.
pub(crate) const MIN_MATCH_ROWS: usize = 16;

/// Fast 64-bit hash of a pixel row (multiply-xor over u64 words).
pub(crate) fn row_hash(row: &[u8]) -> u64 {
    let mut h: u64 = 0x9E37_79B9_7F4A_7C15 ^ row.len() as u64;
    let (words, rest) = row.as_chunks::<8>();
    for c in words {
        let v = u64::from_le_bytes(*c);
        h = (h ^ v).wrapping_mul(0x0000_0100_0000_01B3).rotate_left(29);
    }
    for &b in rest {
        h = (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01B3);
    }
    h ^ (h >> 31)
}

/// Bounding box (pixels, clipped to `w x h`) of the dirty tiles of `map`.
pub(crate) fn dirty_bbox(map: &TileMap, w: u32, h: u32) -> Option<TileRect> {
    let (mut c0, mut r0, mut c1, mut r1) = (u32::MAX, u32::MAX, 0, 0);
    for r in 0..map.rows() {
        for c in 0..map.cols() {
            if map.get(c, r) {
                c0 = c0.min(c);
                r0 = r0.min(r);
                c1 = c1.max(c + 1);
                r1 = r1.max(r + 1);
            }
        }
    }
    if c0 == u32::MAX {
        return None;
    }
    let x0 = c0 * TILE;
    let y0 = r0 * TILE;
    let x1 = (c1 * TILE).min(w);
    let y1 = (r1 * TILE).min(h);
    Some(TileRect {
        x: x0 as u16,
        y: y0 as u16,
        w: (x1 - x0) as u16,
        h: (y1 - y0) as u16,
    })
}

/// Narrow `area` to the columns that actually changed (scanning every
/// other row from both ends). `None` when nothing changed.
pub(crate) fn tighten_columns(
    prev: &[u8],
    cur: &[u8],
    stride: usize,
    area: TileRect,
) -> Option<TileRect> {
    let (ax0, ax1) = (
        usize::from(area.x),
        usize::from(area.x) + usize::from(area.w),
    );
    let (mut lo, mut hi) = (ax1, ax0);
    for y in (usize::from(area.y)..usize::from(area.y) + usize::from(area.h)).step_by(2) {
        let a = row(prev, stride, y, ax0, ax1);
        let b = row(cur, stride, y, ax0, ax1);
        let pa = a.as_chunks::<4>().0.iter();
        let pb = b.as_chunks::<4>().0.iter();
        // Left edge: only scan up to the best found so far.
        if let Some(i) = pa
            .clone()
            .zip(pb.clone())
            .take(lo - ax0)
            .position(|(p, q)| p != q)
        {
            lo = ax0 + i;
        }
        let n = ax1 - ax0;
        if let Some(i) = pa
            .rev()
            .zip(pb.rev())
            .take(ax1 - hi.max(ax0))
            .position(|(p, q)| p != q)
        {
            hi = hi.max(ax0 + n - i);
        }
    }
    (lo < hi).then(|| TileRect {
        x: lo as u16,
        y: area.y,
        w: (hi - lo) as u16,
        h: area.h,
    })
}

fn row(px: &[u8], stride: usize, y: usize, x0: usize, x1: usize) -> &[u8] {
    &px[y * stride + x0 * 4..y * stride + x1 * 4]
}

/// Search `area` (pixels; BGRA frames `prev`/`cur` with `stride`) for a
/// vertical scroll. Returns the verified MOVE (destination rect inside
/// `area`, `dx == 0`) or `None`.
pub(crate) fn detect_vertical(
    prev: &[u8],
    cur: &[u8],
    stride: usize,
    area: TileRect,
) -> Option<MoveRect> {
    if u32::from(area.h) < MIN_ROWS || u32::from(area.w) < MIN_WIDTH {
        return None;
    }
    // The dirty tiles overhang the scrolled content (tile grid vs window
    // edges); unchanged columns would break whole-row matches.
    let area = tighten_columns(prev, cur, stride, area)?;
    if u32::from(area.w) < MIN_WIDTH {
        return None;
    }
    let (x0, x1) = (
        usize::from(area.x),
        usize::from(area.x) + usize::from(area.w),
    );
    let (y0, y1) = (
        usize::from(area.y),
        usize::from(area.y) + usize::from(area.h),
    );
    if prev.len() < y1 * stride || cur.len() < y1 * stride {
        return None;
    }
    let hp: Vec<u64> = (y0..y1)
        .map(|y| row_hash(row(prev, stride, y, x0, x1)))
        .collect();
    let hc: Vec<u64> = (y0..y1)
        .map(|y| row_hash(row(cur, stride, y, x0, x1)))
        .collect();
    // Keys: each row's hash, plus — for rows that differ from the row above
    // — the (above, row) pair: text renders many identical rows (a glyph
    // band, blank leading), but the *edges* between them are distinctive.
    // Keys that repeat in the previous frame are ambiguous and skipped.
    let n = hp.len() as i64;
    let keys = |h: &[u64]| -> Vec<(usize, u64)> {
        let mut v = Vec::with_capacity(h.len() * 2);
        #[allow(clippy::needless_range_loop)] // i and i - 1
        for i in 0..h.len() {
            v.push((i, h[i]));
            if i > 0 && h[i] != h[i - 1] {
                v.push((
                    i,
                    h[i - 1].rotate_left(17)
                        ^ h[i].wrapping_mul(0x9E37_79B9_7F4A_7C15)
                        ^ 0x5bd1_e995,
                ));
            }
        }
        v
    };
    let mut at: HashMap<u64, i64> = HashMap::with_capacity(hp.len() * 2);
    for (i, k) in keys(&hp) {
        at.entry(k).and_modify(|v| *v = -1).or_insert(i as i64);
    }
    let mut votes: HashMap<i64, u32> = HashMap::new();
    for (i, k) in keys(&hc) {
        if hc[i] == hp[i] {
            continue; // unchanged row says nothing about the shift
        }
        if let Some(&j) = at.get(&k) {
            let dy = i as i64 - j;
            if j >= 0 && dy != 0 && dy.abs() <= n / 2 {
                *votes.entry(dy).or_default() += 1;
            }
        }
    }
    let (&dy, &best) = votes.iter().max_by_key(|(dy, v)| (**v, -dy.abs()))?;
    if (best as usize) < 4 {
        return None;
    }
    // Verify every destination row whose source lies inside the area, then
    // keep the longest run of matches (bridging short gaps).
    let lo = dy.max(0) as usize;
    let hi = (n + dy.min(0)) as usize;
    let mut best_run: Option<(usize, usize, usize)> = None; // (start, end, matched)
    let mut cur_run: Option<(usize, usize, usize)> = None;
    let mut gap = 0usize;
    #[allow(clippy::needless_range_loop)] // i indexes hc, hp (shifted) and the frames
    for i in lo..hi {
        let j = (i as i64 - dy) as usize;
        let ok =
            hc[i] == hp[j] && row(cur, stride, y0 + i, x0, x1) == row(prev, stride, y0 + j, x0, x1);
        if ok {
            gap = 0;
            cur_run = Some(match cur_run {
                Some((s, _, m)) => (s, i + 1, m + 1),
                None => (i, i + 1, 1),
            });
        } else if cur_run.is_some() {
            gap += 1;
            if gap > MAX_GAP {
                if cur_run.map(|r| r.2) > best_run.map(|r| r.2) {
                    best_run = cur_run;
                }
                cur_run = None;
                gap = 0;
            }
        }
    }
    if cur_run.map(|r| r.2) > best_run.map(|r| r.2) {
        best_run = cur_run;
    }
    let (s, e, matched) = best_run?;
    if matched < MIN_MATCH_ROWS || matched * 2 < e - s {
        return None;
    }
    Some(MoveRect {
        x: area.x,
        y: (y0 + s) as u16,
        w: area.w,
        h: (e - s) as u16,
        dx: 0,
        dy: dy as i16,
    })
}

/// Rows of `m`'s destination that equal the shifted source rows in
/// `prev` -> `cur` (0 when the move does not fit the frame).
pub(crate) fn verify_move(
    prev: &[u8],
    cur: &[u8],
    w: u32,
    h: u32,
    stride: usize,
    m: &MoveRect,
) -> usize {
    if !m.fits(w, h) || prev.len() < stride * h as usize || cur.len() < stride * h as usize {
        return 0;
    }
    let (dx0, sx0) = (
        usize::from(m.x),
        (i64::from(m.x) - i64::from(m.dx)) as usize,
    );
    let wb = usize::from(m.w) * 4;
    (0..usize::from(m.h))
        .filter(|&i| {
            let dy = usize::from(m.y) + i;
            let sy = (i64::from(m.y) + i as i64 - i64::from(m.dy)) as usize;
            cur[dy * stride + dx0 * 4..dy * stride + dx0 * 4 + wb]
                == prev[sy * stride + sx0 * 4..sy * stride + sx0 * 4 + wb]
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bands::apply_move;
    use crate::tiles::diff_tiles;

    /// A "document" frame: text-like rows with unique content per row.
    fn doc(w: u32, h: u32, seed: u32) -> Vec<u8> {
        let mut px = vec![255u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let o = ((y * w + x) * 4) as usize;
                let v = ((x.wrapping_mul(7) ^ (y + seed).wrapping_mul(131)) % 251) as u8;
                if (x / 6 + y / 3) % 5 != 0 {
                    px[o] = v;
                    px[o + 1] = v.wrapping_mul(3);
                    px[o + 2] = (y % 256) as u8;
                }
            }
        }
        px
    }

    /// Scroll the window `win` of `prev` up by `dy` px (content moves up),
    /// filling the exposed strip with new content.
    fn scroll(prev: &[u8], w: u32, win: TileRect, dy: u32) -> Vec<u8> {
        let stride = w as usize * 4;
        let mut cur = prev.to_vec();
        let (x0, x1) = (usize::from(win.x), usize::from(win.x + win.w));
        let (y0, y1) = (usize::from(win.y), usize::from(win.y + win.h));
        for y in y0..y1 - dy as usize {
            let s = (y + dy as usize) * stride;
            let d = y * stride;
            let src = prev[s + x0 * 4..s + x1 * 4].to_vec();
            cur[d + x0 * 4..d + x1 * 4].copy_from_slice(&src);
        }
        for y in y1 - dy as usize..y1 {
            for x in x0..x1 {
                let o = y * stride + x * 4;
                cur[o] = (x * 31 + y * 17) as u8;
                cur[o + 1] = (x * 5) as u8;
            }
        }
        cur
    }

    #[test]
    fn detects_a_scroll_and_leaves_only_the_strip() {
        let (w, h) = (1280u32, 800u32);
        let prev = doc(w, h, 0);
        let win = TileRect {
            x: 180,
            y: 140,
            w: 900,
            h: 595,
        };
        for dy in [24u32, 3, 120] {
            let cur = scroll(&prev, w, win, dy);
            let stride = w as usize * 4;
            let mut map = TileMap::default();
            diff_tiles(&prev, &cur, w, h, stride, 4, None, &mut map);
            let area = dirty_bbox(&map, w, h).unwrap();
            let m = detect_vertical(&prev, &cur, stride, area).expect("scroll found");
            assert_eq!((m.dx, m.dy), (0, -(dy as i16)), "{m:?}");
            assert_eq!(verify_move(&prev, &cur, w, h, stride, &m), usize::from(m.h));
            let mut moved = prev.clone();
            apply_move(&mut moved, w, h, stride, 4, &m).unwrap();
            let mut res = TileMap::default();
            diff_tiles(&moved, &cur, w, h, stride, 4, None, &mut res);
            let strip_tiles = (dy as usize).div_ceil(64) + 1;
            assert!(
                res.count() <= strip_tiles * 16,
                "dy {dy}: residual {} of {}",
                res.count(),
                map.count()
            );
        }
    }

    #[test]
    fn scrolling_down_is_found_too() {
        let (w, h) = (800u32, 600u32);
        let base = doc(w, h, 5);
        let win = TileRect {
            x: 0,
            y: 0,
            w: 800,
            h: 600,
        };
        // `prev` = scrolled-up version, `cur` = base: content moved down.
        let prev = scroll(&base, w, win, 40);
        let mut cur = base.clone();
        // cur rows 40.. equal prev rows 0..(h-40)
        let stride = w as usize * 4;
        for y in 40..600usize {
            let src = prev[(y - 40) * stride..(y - 39) * stride].to_vec();
            cur[y * stride..(y + 1) * stride].copy_from_slice(&src);
        }
        let m = detect_vertical(&prev, &cur, stride, win).unwrap();
        assert_eq!(m.dy, 40);
        assert!(usize::from(m.h) >= 500);
    }

    #[test]
    fn unrelated_changes_are_not_moves() {
        let (w, h) = (640u32, 480u32);
        let prev = doc(w, h, 1);
        let cur = doc(w, h, 777); // everything different, no shift relation
        let stride = w as usize * 4;
        let area = TileRect {
            x: 0,
            y: 0,
            w: 640,
            h: 480,
        };
        if let Some(m) = detect_vertical(&prev, &cur, stride, area) {
            // Only acceptable when it really verifies.
            assert_eq!(verify_move(&prev, &cur, w, h, stride, &m), usize::from(m.h));
        }
        // A blank screen gives no unique rows: nothing.
        let blank = vec![200u8; prev.len()];
        let mut other = blank.clone();
        other[1000] = 1;
        assert!(detect_vertical(&blank, &other, stride, area).is_none());
        // Too small an area.
        assert!(detect_vertical(
            &prev,
            &cur,
            stride,
            TileRect {
                x: 0,
                y: 0,
                w: 64,
                h: 64
            }
        )
        .is_none());
    }

    #[test]
    fn verify_rejects_a_wrong_os_move() {
        let (w, h) = (640u32, 480u32);
        let prev = doc(w, h, 2);
        let win = TileRect {
            x: 0,
            y: 64,
            w: 640,
            h: 320,
        };
        let cur = scroll(&prev, w, win, 16);
        let stride = w as usize * 4;
        let good = MoveRect {
            x: 0,
            y: 64,
            w: 640,
            h: 304,
            dx: 0,
            dy: -16,
        };
        assert_eq!(verify_move(&prev, &cur, w, h, stride, &good), 304);
        let wrong = MoveRect { dy: -15, ..good };
        assert!(verify_move(&prev, &cur, w, h, stride, &wrong) < 10);
        let outside = MoveRect { y: 470, ..good };
        assert_eq!(verify_move(&prev, &cur, w, h, stride, &outside), 0);
    }

    /// Text-like content where every glyph band is many identical rows
    /// (the host's synthetic scroll pattern): the edge pairs still vote.
    #[test]
    fn repeated_rows_scroll_is_found_via_edges() {
        let (w, h) = (1024u32, 768u32);
        let mut prev = vec![0u8; (w * h * 4) as usize];
        let rect = TileRect {
            x: 0,
            y: 0,
            w: 1024,
            h: 768,
        };
        crate::video::synth_scroll(&mut prev, w, rect, 10, 12);
        let mut cur = prev.clone();
        crate::video::synth_scroll(&mut cur, w, rect, 11, 12);
        let stride = w as usize * 4;
        let m = detect_vertical(&prev, &cur, stride, rect).expect("scroll found");
        assert_eq!(m.dy, -12, "{m:?}");
        assert!(usize::from(m.h) > 700);
        assert_eq!(verify_move(&prev, &cur, w, h, stride, &m), usize::from(m.h));
    }
}
