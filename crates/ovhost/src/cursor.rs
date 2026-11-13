//! Remote-cursor tracking (spec v0.15 "cursor overlay channel").
//!
//! The screen frames stream at the adaptive capture rate (v0.14), so at idle
//! the captured cursor would be frozen between frames. This module tracks the
//! real cursor separately and feeds cheap `CURSOR` (position) /
//! `CURSOR_SHAPE` (bitmap) commands to the viewer, which composites the
//! cursor onto the presented frame — the usual remote-desktop split.
//!
//! Structure mirrors `ovhost::input` / `ovapp::gdisrc`: the decision logic is
//! pure and unit-tested everywhere; the Win32 polling (`GetCursorPos`,
//! `GetCursorInfo`, `GetIconInfo`, `GetDIBits`) is `#[cfg(windows)]` FFI.
//!
//! Coordinates: cursor positions are sent **relative to the captured region**
//! (the region origin the GDI capture uses), so the viewer maps them straight
//! through the frame scale with no knowledge of the desktop's monitor layout.
//!
//! Wire-rate control: position updates are sent only when the cursor moved,
//! at most [`CURSOR_MAX_HZ`] per second (120 Hz: smooth on ProMotion /
//! 120 Hz viewers; an 8-byte record per move, ~1 KB/s while moving and
//! nothing while still).

use std::time::{Duration, Instant};

use ovapp::terminal::Command;

/// Cap on cursor-position sends per second (sent only on movement).
pub const CURSOR_MAX_HZ: u32 = 120;

/// The minimum spacing between two cursor-position sends.
pub const CURSOR_MIN_INTERVAL: Duration =
    Duration::from_nanos(1_000_000_000 / CURSOR_MAX_HZ as u64);

/// Send-decision state for the cursor channel: what to emit on this pump
/// tick, given the polled cursor state and the throttle clock.
///
/// Pure and clock-injecting (the caller supplies `now`), so tests drive it
/// deterministically without sleeping.
#[derive(Debug, Default)]
pub struct CursorTracker {
    /// Last position we *sent* (region-relative pixels).
    last_pos: Option<(i32, i32)>,
    /// Last position we *polled* (may differ: throttled sends coalesce).
    pending_pos: Option<(i32, i32)>,
    /// Last `Instant` we sent a position update.
    last_send: Option<Instant>,
    /// Last cursor-shape handle we *sent* (opaque, compared by value).
    last_shape: Option<u64>,
}

/// What [`CursorTracker::update`] decided to send this tick.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CursorOut {
    /// A `CURSOR` position command to send (region-relative pixels).
    pub pos: Option<(i32, i32)>,
    /// A `CURSOR_SHAPE` command to send (fresh bitmap already rendered).
    pub shape: Option<Command>,
}

impl CursorTracker {
    /// Feed one poll: `pos` is the region-relative cursor position, `shape`
    /// the current shape handle (`None` when the cursor is hidden/off-screen
    /// or the poll failed) and `render` a closure that renders the shape
    /// handle to a `CURSOR_SHAPE` command (only invoked when the handle
    /// changed — rendering is the expensive part).
    ///
    /// Position sends are throttled to [`CURSOR_MAX_HZ`]: a changed position
    /// inside the throttle window is held in `pending_pos` and emitted by the
    /// first later tick that clears the window.
    pub fn update(
        &mut self,
        now: Instant,
        pos: Option<(i32, i32)>,
        shape: Option<u64>,
        mut render: impl FnMut(u64) -> Option<Command>,
    ) -> CursorOut {
        let mut out = CursorOut::default();

        // Shape first: it changes rarely, so it is never throttled, and the
        // viewer gets the new bitmap before/with the next position move.
        if let Some(handle) = shape {
            if self.last_shape != Some(handle) {
                if let Some(cmd) = render(handle) {
                    self.last_shape = Some(handle);
                    out.shape = Some(cmd);
                }
                // A failed render leaves `last_shape` untouched so the next
                // tick retries the same handle.
            }
        }

        // Position: only when it moved since the last *sent* position.
        if let Some(p) = pos {
            if self.last_pos != Some(p) {
                let throttled = self
                    .last_send
                    .is_some_and(|t| now.saturating_duration_since(t) < CURSOR_MIN_INTERVAL);
                if throttled {
                    self.pending_pos = Some(p);
                } else {
                    self.last_pos = Some(p);
                    self.last_send = Some(now);
                    self.pending_pos = None;
                    out.pos = Some(p);
                }
            } else if self.pending_pos.is_some() {
                // No further movement, but a throttled update is still owed.
                let throttled = self
                    .last_send
                    .is_some_and(|t| now.saturating_duration_since(t) < CURSOR_MIN_INTERVAL);
                if !throttled {
                    self.last_send = Some(now);
                    out.pos = self.pending_pos.take();
                }
            }
        }
        out
    }
}

// --------------------------------------------------------------------------------
// Shape composition (pure; unit-tested on every platform). The wire format
// (`CURSOR_SHAPE`) is premultiplied BGRA, drawn by the viewer as an
// alpha-blended sprite over the frame.
// --------------------------------------------------------------------------------

/// One pixel of a mask-based cursor (monochrome, or colour without alpha).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MaskPx {
    /// AND = 1, XOR = 0: the screen shows through.
    Transparent,
    /// AND = 0: this colour (B, G, R) replaces the screen.
    Opaque([u8; 3]),
    /// AND = 1, XOR != 0: the screen is inverted there (the I-beam, the
    /// cross, ...).
    Invert,
}

/// The DIB row stride of a 1bpp bitmap `w` pixels wide: DIB scan lines
/// are DWORD aligned (what `GetDIBits` writes).
#[must_use]
pub fn mono_stride(w: usize) -> usize {
    w.div_ceil(32) * 4
}

/// Bit `x` (MSB first) of top-down row `y` of a 1bpp bitmap.
fn mask_bit(bits: &[u8], stride: usize, x: usize, y: usize) -> bool {
    bits.get(y * stride + x / 8)
        .is_some_and(|b| (b >> (7 - (x % 8))) & 1 == 1)
}

/// Mask pixels -> premultiplied BGRA. An alpha sprite cannot invert what
/// is under it, so inverting pixels become
/// opaque black and their transparent neighbours (8-connected) opaque
/// white: a black shape with a 1 px white outline, legible on light and
/// dark backgrounds alike.
fn mask_pixels_to_bgra(px: &[MaskPx], w: usize, h: usize) -> Vec<u8> {
    let mut out = vec![0u8; w * h * 4];
    if px.len() < w * h {
        return out;
    }
    for (i, p) in px[..w * h].iter().enumerate() {
        let v = match *p {
            MaskPx::Transparent => [0, 0, 0, 0],
            MaskPx::Opaque([b, g, r]) => [b, g, r, 255],
            MaskPx::Invert => [0, 0, 0, 255],
        };
        out[i * 4..i * 4 + 4].copy_from_slice(&v);
    }
    for y in 0..h {
        for x in 0..w {
            if px[y * w + x] != MaskPx::Transparent {
                continue;
            }
            let near_invert = (y.saturating_sub(1)..=(y + 1).min(h - 1)).any(|ny| {
                (x.saturating_sub(1)..=(x + 1).min(w - 1))
                    .any(|nx| px[ny * w + nx] == MaskPx::Invert)
            });
            if near_invert {
                let i = (y * w + x) * 4;
                out[i..i + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
    }
    out
}

/// A monochrome cursor's `CURSOR_SHAPE` bitmap. `bits` is its mask bitmap
/// read **top-down** (`stride` bytes per row, 2h rows): AND rows `0..h`,
/// then XOR rows `h..2h`. AND 0 / XOR 0 = black, AND 0 / XOR 1 = white,
/// AND 1 / XOR 0 = transparent, AND 1 / XOR 1 = inverted (black + white
/// outline). `None` for an empty or short buffer.
#[must_use]
pub fn compose_mono(bits: &[u8], stride: usize, w: usize, h: usize) -> Option<Vec<u8>> {
    if w == 0 || h == 0 || stride * 8 < w || bits.len() < stride.checked_mul(2 * h)? {
        return None;
    }
    let px: Vec<MaskPx> = (0..w * h)
        .map(|i| {
            let (x, y) = (i % w, i / w);
            match (
                mask_bit(bits, stride, x, y),
                mask_bit(bits, stride, x, h + y),
            ) {
                (false, false) => MaskPx::Opaque([0, 0, 0]),
                (false, true) => MaskPx::Opaque([255, 255, 255]),
                (true, false) => MaskPx::Transparent,
                (true, true) => MaskPx::Invert,
            }
        })
        .collect();
    Some(mask_pixels_to_bgra(&px, w, h))
}

/// A colour cursor's `CURSOR_SHAPE` bitmap. `color` is its colour bitmap
/// as top-down 32bpp BGRA; `and_mask` its AND mask (top-down 1bpp, `h`
/// rows of the given stride) when readable.
///
/// * With an alpha channel (any alpha byte non-zero) the mask is ignored
///   and the colours are premultiplied: GDI keeps icon / cursor images in
///   straight alpha (the system hand cursor has colour > alpha pixels),
///   the wire wants premultiplied.
/// * Without one (old 4/8/24-bit colour cursors) the AND mask decides:
///   AND 0 = the opaque colour, AND 1 + black = transparent, AND 1 + any
///   other colour = inverted (black + white outline). Without a mask,
///   black = transparent, anything else opaque.
#[must_use]
pub fn compose_color(
    color: &[u8],
    and_mask: Option<(&[u8], usize)>,
    w: usize,
    h: usize,
) -> Option<Vec<u8>> {
    let n = w.checked_mul(h)?;
    if n == 0 || color.len() < n.checked_mul(4)? {
        return None;
    }
    let color = &color[..n * 4];
    if color.chunks_exact(4).any(|p| p[3] != 0) {
        let mut out = color.to_vec();
        for p in out.chunks_exact_mut(4) {
            let a = u32::from(p[3]);
            for c in &mut p[..3] {
                *c = ((u32::from(*c) * a + 127) / 255) as u8;
            }
        }
        return Some(out);
    }
    let mask = and_mask.filter(|(bits, stride)| stride * 8 >= w && bits.len() >= stride * h);
    let px: Vec<MaskPx> = (0..n)
        .map(|i| {
            let (x, y) = (i % w, i / w);
            let rgb = [color[i * 4], color[i * 4 + 1], color[i * 4 + 2]];
            let and = match mask {
                Some((bits, stride)) => mask_bit(bits, stride, x, y),
                None => rgb == [0, 0, 0],
            };
            if !and {
                MaskPx::Opaque(rgb)
            } else if rgb == [0, 0, 0] {
                MaskPx::Transparent
            } else {
                MaskPx::Invert
            }
        })
        .collect();
    Some(mask_pixels_to_bgra(&px, w, h))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ovapp::terminal::Command;

    fn t(ms: u64) -> Instant {
        // A fixed epoch (the throttle window is only ~8 ms at 120 Hz, so
        // real time must not leak in); only differences matter.
        static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        *EPOCH.get_or_init(Instant::now) + Duration::from_millis(ms)
    }

    #[test]
    fn first_poll_sends_position_and_shape() {
        let mut tr = CursorTracker::default();
        let shape_cmd = Command::CursorShape {
            hotspot_x: 1,
            hotspot_y: 1,
            w: 4,
            h: 4,
            bgra: vec![0; 64],
        };
        let out = tr.update(t(0), Some((10, 20)), Some(0xAA), |_| {
            Some(shape_cmd.clone())
        });
        assert_eq!(out.pos, Some((10, 20)));
        assert_eq!(out.shape, Some(shape_cmd));
    }

    #[test]
    fn unchanged_position_sends_nothing() {
        let mut tr = CursorTracker::default();
        tr.update(t(0), Some((10, 20)), Some(0xAA), |_| None);
        let out = tr.update(t(100), Some((10, 20)), Some(0xAA), |_| None);
        assert_eq!(out, CursorOut::default());
    }

    #[test]
    fn position_moves_are_throttled_to_120hz() {
        assert_eq!(CURSOR_MAX_HZ, 120);
        let mut tr = CursorTracker::default();
        // First send at t=0.
        let out = tr.update(t(0), Some((0, 0)), None, |_| None);
        assert_eq!(out.pos, Some((0, 0)));
        // A move 3 ms later is inside the ~8.3 ms window: held, not sent.
        let out = tr.update(t(3), Some((5, 5)), None, |_| None);
        assert_eq!(out.pos, None, "inside the throttle window");
        // The next tick past the window flushes the held position.
        let out = tr.update(t(10), Some((5, 5)), None, |_| None);
        assert_eq!(
            out.pos,
            Some((5, 5)),
            "owed update flushes after the window"
        );
        // And a further move right after is again throttled.
        let out = tr.update(t(12), Some((9, 9)), None, |_| None);
        assert_eq!(out.pos, None);
        let out = tr.update(t(20), Some((9, 9)), None, |_| None);
        assert_eq!(out.pos, Some((9, 9)));
    }

    #[test]
    fn rapid_jitter_coalesces_to_the_latest_position() {
        let mut tr = CursorTracker::default();
        tr.update(t(0), Some((0, 0)), None, |_| None);
        // 8 moves inside one window: all held, only the last survives.
        for i in 1..=8u64 {
            let out = tr.update(t(i), Some((i as i32, i as i32)), None, |_| None);
            assert_eq!(out.pos, None, "move {i} must be throttled");
        }
        let out = tr.update(t(10), Some((8, 8)), None, |_| None);
        assert_eq!(out.pos, Some((8, 8)), "the latest position flushes");
    }

    #[test]
    fn shape_change_is_never_throttled() {
        let mut tr = CursorTracker::default();
        tr.update(t(0), Some((0, 0)), Some(1), |_| None);
        // A shape change 1 ms after a position send goes out immediately.
        let out = tr.update(t(1), Some((1, 1)), Some(2), |_| {
            Some(Command::CursorShape {
                hotspot_x: 0,
                hotspot_y: 0,
                w: 1,
                h: 1,
                bgra: vec![0, 0, 0, 255],
            })
        });
        assert!(out.shape.is_some(), "shape changes bypass the throttle");
        assert_eq!(out.pos, None, "position is still throttled");
    }

    #[test]
    fn failed_render_retries_next_tick() {
        let mut tr = CursorTracker::default();
        tr.update(t(0), Some((0, 0)), Some(7), |_| None);
        let mut calls = 0;
        let out = tr.update(t(5), Some((0, 0)), Some(7), |_| {
            calls += 1;
            None
        });
        assert_eq!(calls, 1, "render runs again for the un-sent handle");
        assert_eq!(out.shape, None);
        // And a succeeding render then sends it.
        let out = tr.update(t(10), Some((0, 0)), Some(7), |_| {
            Some(Command::CursorShape {
                hotspot_x: 0,
                hotspot_y: 0,
                w: 2,
                h: 2,
                bgra: vec![0; 16],
            })
        });
        assert!(out.shape.is_some());
    }

    // ---- shape composition (pure) ----

    /// Build a top-down 1bpp mask (DWORD-aligned rows) from a predicate.
    fn mask_bits(w: usize, rows: usize, bit: impl Fn(usize, usize) -> bool) -> (Vec<u8>, usize) {
        let stride = mono_stride(w);
        let mut bits = vec![0u8; stride * rows];
        for y in 0..rows {
            for x in 0..w {
                if bit(x, y) {
                    bits[y * stride + x / 8] |= 0x80 >> (x % 8);
                }
            }
        }
        (bits, stride)
    }

    fn px(bgra: &[u8], w: usize, x: usize, y: usize) -> [u8; 4] {
        let i = (y * w + x) * 4;
        [bgra[i], bgra[i + 1], bgra[i + 2], bgra[i + 3]]
    }

    fn premultiplied_ok(bgra: &[u8]) -> bool {
        bgra.chunks_exact(4)
            .all(|p| p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3])
    }

    const CLEAR: [u8; 4] = [0, 0, 0, 0];
    const BLACK: [u8; 4] = [0, 0, 0, 255];
    const WHITE: [u8; 4] = [255; 4];

    #[test]
    fn mono_stride_is_dword_aligned() {
        assert_eq!(mono_stride(1), 4);
        assert_eq!(mono_stride(32), 4);
        assert_eq!(mono_stride(33), 8);
        assert_eq!(mono_stride(48), 8);
        assert_eq!(mono_stride(64), 8);
    }

    #[test]
    fn mono_and_xor_rows_are_not_swapped() {
        // AND rows 0..h then XOR rows h..2h (top-down), one pixel each
        // of the four classes in a 4x1 cursor.
        let (w, h) = (4, 1);
        let and = [false, false, true, true];
        let xor = [false, true, false, true];
        let (bits, stride) = mask_bits(w, 2 * h, |x, y| if y < h { and[x] } else { xor[x] });
        let out = compose_mono(&bits, stride, w, h).unwrap();
        assert_eq!(px(&out, w, 0, 0), BLACK, "AND 0 / XOR 0 = black");
        assert_eq!(px(&out, w, 1, 0), WHITE, "AND 0 / XOR 1 = white");
        // AND 1 / XOR 0 is transparent — except here it borders the
        // inverting pixel, so it is part of that pixel's white outline.
        assert_eq!(px(&out, w, 2, 0), WHITE, "outline of the inverted pixel");
        assert_eq!(px(&out, w, 3, 0), BLACK, "AND 1 / XOR 1 (invert) = black");
    }

    #[test]
    fn mono_ibeam_is_a_black_beam_with_white_outline_not_a_white_box() {
        // The classic I-beam: AND = 1 everywhere (screen shows through),
        // XOR = 1 on the beam (inverts the screen). The v0.41 bug read the
        // XOR rows as AND rows, turning the whole 32x32 box opaque white.
        let (w, h) = (32, 32);
        let beam = |x: usize, y: usize| x == 15 && (6..26).contains(&y);
        let (bits, stride) = mask_bits(w, 2 * h, |x, y| if y < h { true } else { beam(x, y - h) });
        let out = compose_mono(&bits, stride, w, h).unwrap();
        assert!(premultiplied_ok(&out));
        for y in 0..h {
            for x in 0..w {
                let p = px(&out, w, x, y);
                if beam(x, y) {
                    assert_eq!(p, BLACK, "beam pixel ({x},{y})");
                } else if (14..=16).contains(&x) && (5..=26).contains(&y) {
                    assert_eq!(p, WHITE, "outline pixel ({x},{y})");
                } else {
                    assert_eq!(p, CLEAR, "transparent pixel ({x},{y})");
                }
            }
        }
        let opaque = out.chunks_exact(4).filter(|p| p[3] != 0).count();
        assert_eq!(opaque, 3 * 22, "beam (20) + outline");
    }

    #[test]
    fn mono_arrow_keeps_its_transparent_surroundings() {
        // A mono arrow: black border + white body (AND = 0), transparent
        // elsewhere (AND = 1, XOR = 0).
        let (w, h) = (16, 16);
        let inside = |x: usize, y: usize| x <= y && y < 12;
        let border = |x: usize, y: usize| inside(x, y) && (x == 0 || x == y || y == 11);
        let (bits, stride) = mask_bits(w, 2 * h, |x, y| {
            if y < h {
                !inside(x, y)
            } else {
                let y = y - h;
                inside(x, y) && !border(x, y)
            }
        });
        let out = compose_mono(&bits, stride, w, h).unwrap();
        assert_eq!(px(&out, w, 0, 0), BLACK);
        assert_eq!(px(&out, w, 1, 5), WHITE);
        assert_eq!(px(&out, w, 15, 0), CLEAR);
        assert_eq!(px(&out, w, 15, 15), CLEAR);
        assert_eq!(px(&out, w, 3, 14), CLEAR);
    }

    #[test]
    fn mono_rejects_short_buffers() {
        assert!(compose_mono(&[0u8; 4], 4, 32, 32).is_none());
        assert!(
            compose_mono(&[0u8; 256], 2, 32, 32).is_none(),
            "stride too small for w"
        );
        assert!(compose_mono(&[], 4, 0, 0).is_none());
    }

    #[test]
    fn color_with_alpha_is_premultiplied_and_ignores_the_mask() {
        let (w, h) = (2, 2);
        // Straight alpha as GDI stores icon images: a half-transparent
        // white edge, an opaque red, a transparent black, a dark shadow.
        let color = [
            255, 255, 255, 128, //
            0, 0, 255, 255, //
            0, 0, 0, 0, //
            10, 20, 30, 64,
        ];
        // A mask that would say "all opaque": must be ignored.
        let (bits, stride) = mask_bits(w, h, |_, _| false);
        let out = compose_color(&color, Some((&bits, stride)), w, h).unwrap();
        assert_eq!(px(&out, w, 0, 0), [128, 128, 128, 128]);
        assert_eq!(px(&out, w, 1, 0), [0, 0, 255, 255]);
        assert_eq!(px(&out, w, 0, 1), CLEAR);
        assert_eq!(px(&out, w, 1, 1), [3, 5, 8, 64]);
        assert!(premultiplied_ok(&out));
    }

    #[test]
    fn color_without_alpha_uses_the_and_mask() {
        // Old-style colour cursor (24-bit image, alpha 0 everywhere):
        // AND 0 = opaque colour, AND 1 + black = transparent, AND 1 +
        // colour = inverting (approximated as black + outline).
        let (w, h) = (5, 1);
        let color = [
            0, 128, 255, 0, // AND 0: opaque orange
            0, 0, 0, 0, // AND 1, black: transparent
            0, 0, 0, 0, // AND 1, black: transparent (next to the inverter)
            255, 255, 255, 0, // AND 1, white: invert
            0, 0, 0, 0, // AND 0, black: opaque black
        ];
        let and = [false, true, true, true, false];
        let (bits, stride) = mask_bits(w, h, |x, _| and[x]);
        let out = compose_color(&color, Some((&bits, stride)), w, h).unwrap();
        assert_eq!(px(&out, w, 0, 0), [0, 128, 255, 255]);
        assert_eq!(px(&out, w, 1, 0), CLEAR);
        assert_eq!(
            px(&out, w, 2, 0),
            WHITE,
            "outline next to the inverted pixel"
        );
        assert_eq!(px(&out, w, 3, 0), BLACK);
        assert_eq!(px(&out, w, 4, 0), BLACK);
        assert!(premultiplied_ok(&out));
        // No mask at all: black = transparent, anything else opaque.
        let out = compose_color(&color, None, w, h).unwrap();
        assert_eq!(px(&out, w, 1, 0), CLEAR);
        assert_eq!(px(&out, w, 3, 0), WHITE);
        assert_eq!(px(&out, w, 4, 0), CLEAR);
    }

    /// The real system cursors (Windows): every shape is valid
    /// premultiplied BGRA with a mostly transparent box — the I-beam and the
    /// cross (monochrome, inverting) included, which v0.41 sent as a solid
    /// white 32x32 square.
    #[cfg(windows)]
    #[test]
    fn system_cursors_render_with_transparent_surroundings() {
        #[link(name = "user32")]
        extern "system" {
            fn LoadCursorW(h: isize, name: usize) -> isize;
        }
        let ids = [
            ("ARROW", 32512usize),
            ("IBEAM", 32513),
            ("WAIT", 32514),
            ("CROSS", 32515),
            ("SIZENWSE", 32642),
            ("SIZEWE", 32644),
            ("SIZEALL", 32646),
            ("NO", 32648),
            ("HAND", 32649),
            ("HELP", 32651),
        ];
        for (name, id) in ids {
            // SAFETY: a predefined system cursor (shared; never destroyed).
            let h = unsafe { LoadCursorW(0, id) };
            if h == 0 {
                continue;
            }
            let Some(Command::CursorShape { w, h: ch, bgra, .. }) = render_shape(h as u64) else {
                panic!("{name}: render failed");
            };
            let (w, ch) = (usize::from(w), usize::from(ch));
            assert_eq!(bgra.len(), w * ch * 4, "{name}");
            assert!(
                premultiplied_ok(&bgra),
                "{name}: colour above alpha (not premultiplied)"
            );
            let n = w * ch;
            let opaque = bgra.chunks_exact(4).filter(|p| p[3] != 0).count();
            eprintln!("{name}: {w}x{ch}, {opaque}/{n} non-transparent");
            assert!(opaque > 0, "{name}: invisible");
            assert!(
                opaque * 2 < n,
                "{name}: {opaque}/{n} pixels covered — a box, not a cursor"
            );
            assert_eq!(
                px(&bgra, w, w - 1, ch - 1),
                CLEAR,
                "{name}: bottom-right corner"
            );
        }
        // The I-beam specifically: a dark beam (it inverts the screen)
        // with a light outline.
        // SAFETY: as above.
        let h = unsafe { LoadCursorW(0, 32513) };
        let Some(Command::CursorShape { bgra, .. }) = render_shape(h as u64) else {
            unreachable!()
        };
        assert!(
            bgra.chunks_exact(4).any(|p| p == BLACK),
            "I-beam has black pixels"
        );
        assert!(
            bgra.chunks_exact(4).any(|p| p == WHITE),
            "I-beam has a white outline"
        );
    }

    #[test]
    fn hidden_cursor_polls_cleanly() {
        let mut tr = CursorTracker::default();
        tr.update(t(0), Some((0, 0)), Some(1), |_| None);
        // Cursor leaves the polled region (None position, None shape): no
        // sends, and reappearing with the same shape re-sends the position.
        let out = tr.update(t(5), None, None, |_| None);
        assert_eq!(out, CursorOut::default());
        let out = tr.update(t(50), Some((3, 4)), None, |_| None);
        assert_eq!(out.pos, Some((3, 4)));
    }
}

// --------------------------------------------------------------------------------
// Windows implementation: raw Win32 FFI in the house style of `ovhost::input`
// and `ovapp::gdisrc` — no `windows`/`winapi` crates.
// --------------------------------------------------------------------------------

#[cfg(windows)]
mod ffi {
    //! Raw Win32 imports (user32 + gdi32). Linked implicitly by MSVC.

    #![allow(non_snake_case)]
    // Win32 type names are conventionally ALL_CAPS; keep them as-is.
    #![allow(clippy::upper_case_acronyms)]

    #[link(name = "user32")]
    extern "system" {
        #[link_name = "GetCursorInfo"]
        pub fn GetCursorInfo(pci: *mut CURSORINFO) -> i32;
        #[link_name = "GetIconInfo"]
        pub fn GetIconInfo(hIcon: isize, piconinfo: *mut ICONINFO) -> i32;
        #[link_name = "DeleteObject"]
        pub fn DeleteObject(hObject: isize) -> i32;
    }

    #[link(name = "gdi32")]
    extern "system" {
        #[link_name = "GetDIBits"]
        pub fn GetDIBits(
            hdc: isize,
            hbm: isize,
            start: u32,
            lines: u32,
            lpvBits: *mut u8,
            lpbmi: *mut BITMAPINFO,
            usage: u32,
        ) -> i32;
        #[link_name = "CreateCompatibleDC"]
        pub fn CreateCompatibleDC(hdc: isize) -> isize;
        #[link_name = "DeleteDC"]
        pub fn DeleteDC(hdc: isize) -> i32;
    }

    /// `POINT` (windef.h).
    #[derive(Clone, Copy, Default)]
    #[repr(C)]
    pub struct POINT {
        pub x: i32,
        pub y: i32,
    }

    /// `CURSORINFO` (winuser.h).
    #[repr(C)]
    pub struct CURSORINFO {
        pub cbSize: u32,
        pub flags: u32,
        pub hCursor: isize,
        pub ptScreenPos: POINT,
    }

    pub const CURSOR_SHOWING: u32 = 0x0000_0001;

    /// `ICONINFO` (winuser.h).
    #[repr(C)]
    pub struct ICONINFO {
        pub fIcon: i32,
        pub xHotspot: u32,
        pub yHotspot: u32,
        pub hbmMask: isize,
        pub hbmColor: isize,
    }

    /// `BITMAP` (wingdi.h) — `GetObjectW` target for reading bitmap dims.
    #[repr(C)]
    pub struct BITMAP {
        pub bmType: i32,
        pub bmWidth: i32,
        pub bmHeight: i32,
        pub bmWidthBytes: i32,
        pub bmPlanes: u16,
        pub bmBitsPixel: u16,
        pub bmBits: *mut u8,
    }

    /// `BITMAPINFOHEADER` (wingdi.h); negative `biHeight` = top-down.
    #[repr(C)]
    pub struct BITMAPINFOHEADER {
        pub biSize: u32,
        pub biWidth: i32,
        pub biHeight: i32,
        pub biPlanes: u16,
        pub biBitCount: u16,
        pub biCompression: u32,
        pub biSizeImage: u32,
        pub biXPelsPerMeter: u32,
        pub biYPelsPerMeter: u32,
        pub biClrUsed: u32,
        pub biClrImportant: u32,
    }

    /// `BITMAPINFO` with room for a small color table: `GetDIBits` writes
    /// the 2-entry table of a 1bpp DIB (and up to 3 masks for bitfields)
    /// after the header — a 1-entry array (v0.41) let it write past the
    /// struct.
    #[repr(C)]
    pub struct BITMAPINFO {
        pub bmiHeader: BITMAPINFOHEADER,
        pub bmiColors: [u32; 4],
    }

    pub const DIB_RGB_COLORS: u32 = 0;
    pub const BI_RGB: u32 = 0;

    #[link(name = "gdi32")]
    extern "system" {
        #[link_name = "GetObjectW"]
        pub fn GetObjectW(h: isize, c: i32, pv: *mut u8) -> i32;
    }
}

/// The polled cursor state for one pump tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolledCursor {
    /// Virtual-screen position of the cursor hotspot.
    pub x: i32,
    pub y: i32,
    /// The cursor-shape handle (0 when hidden).
    pub handle: u64,
}

/// Poll the real cursor: `GetCursorInfo` gives position + `hCursor` in one
/// call (it also reports the show/hide state via `CURSOR_SHOWING`). Returns
/// `None` when the poll fails or the cursor is hidden.
#[cfg(windows)]
#[must_use]
pub fn poll() -> Option<PolledCursor> {
    let mut ci = ffi::CURSORINFO {
        cbSize: std::mem::size_of::<ffi::CURSORINFO>() as u32,
        flags: 0,
        hCursor: 0,
        ptScreenPos: ffi::POINT::default(),
    };
    // SAFETY: `ci` is a valid CURSORINFO with cbSize set, as documented.
    if unsafe { ffi::GetCursorInfo(&mut ci) } == 0 || ci.hCursor == 0 {
        return None;
    }
    if ci.flags & ffi::CURSOR_SHOWING == 0 {
        return None; // cursor hidden (e.g. during typing in some apps)
    }
    Some(PolledCursor {
        x: ci.ptScreenPos.x,
        y: ci.ptScreenPos.y,
        handle: ci.hCursor as u64,
    })
}

/// Largest cursor bitmap edge read from GDI (the wire cap downscales to
/// [`ovapp::terminal::MAX_CURSOR_DIM`] afterwards).
#[cfg(windows)]
const MAX_GDI_CURSOR_DIM: i32 = 1024;

/// Render a cursor-shape handle to a `CURSOR_SHAPE` command: premultiplied
/// BGRA bitmap + hotspot. A failure returns `None` and the tracker retries
/// on the next tick.
///
/// * Colour cursors: `hbmColor` read as top-down 32bpp (straight alpha) and
///   `hbmMask` as top-down 1bpp, combined by [`compose_color`].
/// * Monochrome cursors (no `hbmColor`; the I-beam, the cross): `hbmMask`
///   is 2x height, AND rows then XOR rows, read top-down and combined by
///   [`compose_mono`] (inverting pixels -> black with a white outline).
///
/// v0.41 read the monochrome mask bottom-up but indexed it as if AND came
/// first in the buffer, so AND and XOR were swapped: the transparent area
/// of the I-beam (AND 1, XOR 0) became opaque white — a 32x32 white square
/// around the pointer over text.
///
/// SAFETY notes: every GDI handle obtained here (`GetIconInfo`'s two bitmaps,
/// the compatible DC) is released on all paths.
#[cfg(windows)]
#[must_use]
pub fn render_shape(handle: u64) -> Option<Command> {
    let hcursor = handle as isize;
    let mut info = ffi::ICONINFO {
        fIcon: 0,
        xHotspot: 0,
        yHotspot: 0,
        hbmMask: 0,
        hbmColor: 0,
    };
    // SAFETY: `info` is a valid ICONINFO for this live cursor handle.
    if unsafe { ffi::GetIconInfo(hcursor, &mut info) } == 0 {
        return None;
    }
    // Scope guard: release both bitmaps on every exit path.
    let out = (|| {
        let (mw, mh) = bitmap_dims(info.hbmMask)?;
        let (w, h, bgra) = if info.hbmColor == 0 {
            // Monochrome: AND rows then XOR rows; real height = mh / 2.
            if mh % 2 != 0 {
                return None;
            }
            let (bits, stride) = read_mask(info.hbmMask, mw, mh)?;
            let (w, h) = (mw as usize, (mh / 2) as usize);
            (w, h, compose_mono(&bits, stride, w, h)?)
        } else {
            let (cw, ch) = bitmap_dims(info.hbmColor)?;
            let color = read_color(info.hbmColor, cw, ch)?;
            // The colour cursor's mask is its AND mask (same size).
            let mask = if (mw, mh) == (cw, ch) {
                read_mask(info.hbmMask, mw, mh)
            } else {
                None
            };
            let (w, h) = (cw as usize, ch as usize);
            let bgra = compose_color(&color, mask.as_ref().map(|(b, s)| (&b[..], *s)), w, h)?;
            (w, h, bgra)
        };
        Some(Command::CursorShape {
            hotspot_x: u16::try_from(info.xHotspot).unwrap_or(0),
            hotspot_y: u16::try_from(info.yHotspot).unwrap_or(0),
            w: u16::try_from(w).ok()?,
            h: u16::try_from(h).ok()?,
            bgra,
        })
    })()
    // Large (accessibility / high-DPI) cursors would not fit one record.
    .map(|cmd| ovapp::terminal::cap_cursor_shape(cmd, ovapp::terminal::MAX_CURSOR_DIM));
    // SAFETY: both handles are GDI bitmaps we own (from GetIconInfo).
    unsafe {
        if info.hbmColor != 0 {
            ffi::DeleteObject(info.hbmColor);
        }
        if info.hbmMask != 0 {
            ffi::DeleteObject(info.hbmMask);
        }
    }
    out
}

/// Read a GDI bitmap object's width/height via `GetObjectW` (both in
/// `1..=MAX_GDI_CURSOR_DIM * 2`).
#[cfg(windows)]
fn bitmap_dims(hbm: isize) -> Option<(i32, i32)> {
    if hbm == 0 {
        return None;
    }
    let mut bm = ffi::BITMAP {
        bmType: 0,
        bmWidth: 0,
        bmHeight: 0,
        bmWidthBytes: 0,
        bmPlanes: 0,
        bmBitsPixel: 0,
        bmBits: std::ptr::null_mut(),
    };
    // SAFETY: `bm` is a valid BITMAP of the declared size for this handle.
    let n = unsafe {
        ffi::GetObjectW(
            hbm,
            std::mem::size_of::<ffi::BITMAP>() as i32,
            &mut bm as *mut _ as *mut u8,
        )
    };
    if n == 0 || bm.bmWidth <= 0 || bm.bmHeight <= 0 {
        return None;
    }
    if bm.bmWidth > MAX_GDI_CURSOR_DIM || bm.bmHeight > 2 * MAX_GDI_CURSOR_DIM {
        return None;
    }
    Some((bm.bmWidth, bm.bmHeight))
}

/// `GetDIBits` of `hbm` as a **top-down** DIB (`bits` bpp, `w` x `h`) into
/// a buffer of `stride * h` bytes.
#[cfg(windows)]
fn read_dib(hbm: isize, w: i32, h: i32, bits: u16, stride: usize) -> Option<Vec<u8>> {
    let len = stride.checked_mul(h as usize)?;
    let mut buf = vec![0u8; len];
    let mut bmi = ffi::BITMAPINFO {
        bmiHeader: ffi::BITMAPINFOHEADER {
            biSize: std::mem::size_of::<ffi::BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // negative = top-down: buffer row 0 = bitmap row 0
            biPlanes: 1,
            biBitCount: bits,
            biCompression: ffi::BI_RGB,
            biSizeImage: len as u32,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        },
        bmiColors: [0; 4],
    };
    // GetDIBits needs a DC; a compatible DC for the screen is enough.
    // SAFETY: null hdc = screen DC for CreateCompatibleDC; released below.
    let hdc = unsafe { ffi::CreateCompatibleDC(0) };
    if hdc == 0 {
        return None;
    }
    // SAFETY: `buf` holds `h` DWORD-aligned rows of the declared DIB; `bmi`
    // has room for the colour table GetDIBits writes for 1bpp.
    let lines = unsafe {
        ffi::GetDIBits(
            hdc,
            hbm,
            0,
            h as u32,
            buf.as_mut_ptr(),
            &mut bmi,
            ffi::DIB_RGB_COLORS,
        )
    };
    // SAFETY: paired with CreateCompatibleDC above.
    unsafe {
        ffi::DeleteDC(hdc);
    }
    (lines == h).then_some(buf)
}

/// A mask bitmap as top-down 1bpp rows: (bits, stride).
#[cfg(windows)]
fn read_mask(hbm: isize, w: i32, h: i32) -> Option<(Vec<u8>, usize)> {
    let stride = mono_stride(w as usize);
    read_dib(hbm, w, h, 1, stride).map(|b| (b, stride))
}

/// A colour bitmap as top-down 32bpp BGRA (alpha 0 when it has none).
#[cfg(windows)]
fn read_color(hbm: isize, w: i32, h: i32) -> Option<Vec<u8>> {
    read_dib(hbm, w, h, 32, w as usize * 4)
}
