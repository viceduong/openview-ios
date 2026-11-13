//! Cursor blending and composite-cache tests.

use super::*;

fn solid(w: u16, h: u16, bgra: [u8; 4]) -> CursorBmp {
    CursorBmp {
        hotspot_x: 1,
        hotspot_y: 1,
        w,
        h,
        bgra: bgra
            .iter()
            .flat_map(|&b| [b; 1])
            .cycle()
            .take(w as usize * h as usize * 4)
            .collect(),
    }
}

#[test]
fn opaque_cursor_replaces_pixels() {
    // 2x2 fully-opaque red cursor on a 4x4 gray frame.
    let mut frame = vec![64u8; 4 * 4 * 4];
    let cur = solid(2, 2, [0, 0, 255, 255]); // BGRA red
    blend_cursor(&mut frame, 16, 4, 4, &cur, 2, 2);
    // Hotspot (1,1) means the bitmap covers (1,1)..(3,3); top-left pixel
    // of the frame is untouched (frame alpha stays 64).
    assert_eq!(&frame[0..4], &[64, 64, 64, 64][..]);
    // Pixel (1,1) -> byte offset 1*16 + 1*4 = 20: red, alpha forced 255.
    assert_eq!(&frame[20..24], &[0, 0, 255, 255][..]);
    // Pixel (2,2) -> offset (2*4 + 2) * 4 = 40.
    assert_eq!(&frame[40..44], &[0, 0, 255, 255][..]);
    // Pixel (3,3) outside the bitmap: untouched (frame alpha stays 64).
    assert_eq!(
        &frame[(3 * 4 + 3) * 4..(3 * 4 + 3) * 4 + 4],
        &[64, 64, 64, 64][..]
    );
}

#[test]
fn translucent_cursor_blends_premultiplied() {
    // 1x1 cursor, 50% alpha, premultiplied half-red (b=0, g=0, r=128).
    let mut frame = vec![0u8; 4]; // single black pixel (B=0)
    let cur = CursorBmp {
        hotspot_x: 0,
        hotspot_y: 0,
        w: 1,
        h: 1,
        bgra: vec![0, 0, 128, 128],
    };
    blend_cursor(&mut frame, 4, 1, 1, &cur, 0, 0);
    // out = src + dst*(127)/255 = 0 + 0 = 0 for b/g; r = 128 + 0; a = 255.
    assert_eq!(frame, [0, 0, 128, 255]);
}

#[test]
fn cursor_is_clipped_at_frame_edges() {
    // 2x2 opaque cursor whose bottom-right hangs off a 2x2 frame.
    let mut frame = vec![10u8; 2 * 2 * 4];
    let cur = solid(2, 2, [0, 0, 255, 255]);
    blend_cursor(&mut frame, 8, 2, 2, &cur, 1, 1);
    // Bitmap covers all four pixels (hotspot 1,1 → origin 0,0).
    assert_eq!(&frame[0..4], &[0, 0, 255, 255][..]);
    assert_eq!(&frame[4..8], &[0, 0, 255, 255][..]);
}

#[test]
fn malformed_bitmap_does_not_panic() {
    // Claims 2x2 but carries too few bytes: clipped loop must stop.
    let mut frame = vec![0u8; 4 * 4];
    let cur = CursorBmp {
        hotspot_x: 0,
        hotspot_y: 0,
        w: 2,
        h: 2,
        bgra: vec![255; 5],
    };
    blend_cursor(&mut frame, 16, 1, 1, &cur, 0, 0);
    assert!(frame.iter().all(|&b| b == 0 || b == 255));
}

#[test]
fn zero_sized_cursor_is_a_noop() {
    let mut frame = vec![7u8; 4];
    let cur = CursorBmp {
        hotspot_x: 0,
        hotspot_y: 0,
        w: 0,
        h: 0,
        bgra: vec![],
    };
    blend_cursor(&mut frame, 4, 1, 1, &cur, 0, 0);
    assert_eq!(frame, [7, 7, 7, 7]);
}

#[test]
fn compose_incremental_cursor_move_matches_a_fresh_composite() {
    use ovapp::screen::{Frame, PixelFormat};
    let frame = Frame::new(
        16,
        8,
        PixelFormat::Bgra,
        1,
        (0..16 * 8 * 4).map(|i| (i % 251) as u8).collect(),
    );
    let cur = solid(3, 3, [200, 100, 50, 255]);
    let at = |x, y| CursorOverlay {
        pos: Some((x, y)),
        shape: Some(cur.clone()),
    };
    let mut disp: Display = None;
    let _ = compose(&mut disp, &frame, at(2, 2));
    // Same frame, cursor moved (incl. partially off-frame): restore + blend.
    for (x, y) in [(10, 5), (15, 7), (0, 0), (-1, 3)] {
        let inc = compose(&mut disp, &frame, at(x, y)).to_vec();
        let mut fresh: Display = None;
        let full = compose(&mut fresh, &frame, at(x, y)).to_vec();
        assert_eq!(inc, full, "cursor at ({x},{y})");
    }
    // A new frame reuses the allocation and still composites correctly.
    let frame2 = Frame::new(16, 8, PixelFormat::Bgra, 2, vec![9; 16 * 8 * 4]);
    let out = compose(&mut disp, &frame2, at(4, 4)).to_vec();
    let mut fresh: Display = None;
    assert_eq!(out, compose(&mut fresh, &frame2, at(4, 4)));
}
