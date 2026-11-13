use std::time::{Duration, Instant};

use ovapp::terminal::Command;

use super::shape::{compose_color, compose_mono, mono_stride};
#[cfg(windows)]
use super::win32::render_shape;
use super::*;

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
    bgra.as_chunks::<4>()
        .0
        .iter()
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
    // XOR = 1 on the beam (inverts the screen). Reading the XOR rows as
    // AND rows would turn the whole 32x32 box opaque white.
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
    let opaque = out.as_chunks::<4>().0.iter().filter(|p| p[3] != 0).count();
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
/// cross (monochrome, inverting) included, which must not come out as a
/// solid white 32x32 square.
#[cfg(windows)]
#[test]
fn system_cursors_render_with_transparent_surroundings() {
    use windows::core::PCWSTR;
    use windows::Win32::UI::WindowsAndMessaging::{
        LoadCursorW, IDC_ARROW, IDC_CROSS, IDC_HAND, IDC_HELP, IDC_IBEAM, IDC_NO, IDC_SIZEALL,
        IDC_SIZENWSE, IDC_SIZEWE, IDC_WAIT,
    };
    // A predefined system cursor (shared; never destroyed), or `None`.
    let load = |id: PCWSTR| {
        // SAFETY: `id` is a predefined resource id (MAKEINTRESOURCE), which
        // LoadCursorW accepts with no module handle.
        unsafe { LoadCursorW(None, id) }
            .ok()
            .map(|h| h.0 as usize as u64)
    };
    let ids = [
        ("ARROW", IDC_ARROW),
        ("IBEAM", IDC_IBEAM),
        ("WAIT", IDC_WAIT),
        ("CROSS", IDC_CROSS),
        ("SIZENWSE", IDC_SIZENWSE),
        ("SIZEWE", IDC_SIZEWE),
        ("SIZEALL", IDC_SIZEALL),
        ("NO", IDC_NO),
        ("HAND", IDC_HAND),
        ("HELP", IDC_HELP),
    ];
    for (name, id) in ids {
        let Some(h) = load(id) else {
            continue;
        };
        let Some(Command::CursorShape { w, h: ch, bgra, .. }) = render_shape(h) else {
            panic!("{name}: render failed");
        };
        let (w, ch) = (usize::from(w), usize::from(ch));
        assert_eq!(bgra.len(), w * ch * 4, "{name}");
        assert!(
            premultiplied_ok(&bgra),
            "{name}: colour above alpha (not premultiplied)"
        );
        let n = w * ch;
        let opaque = bgra.as_chunks::<4>().0.iter().filter(|p| p[3] != 0).count();
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
    let h = load(IDC_IBEAM).expect("the I-beam cursor");
    let Some(Command::CursorShape { bgra, .. }) = render_shape(h) else {
        unreachable!()
    };
    assert!(
        bgra.as_chunks::<4>().0.contains(&BLACK),
        "I-beam has black pixels"
    );
    assert!(
        bgra.as_chunks::<4>().0.contains(&WHITE),
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
