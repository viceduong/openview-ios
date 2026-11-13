//! Normalization and letterbox tests.

use super::*;

/// A 1920x1080 client area showing a 1920x1080 frame: content fills it.
fn full() -> Rect {
    letterbox_rect(1920, 1080, 1920, 1080)
}

#[test]
fn center_maps_to_middle() {
    let r = full();
    let (nx, ny) = client_to_normalized(960, 540, &r).expect("center inside");
    assert!((i64::from(nx) - 32_767).abs() <= 1, "nx {nx}");
    assert!((i64::from(ny) - 32_767).abs() <= 1, "ny {ny}");
}

#[test]
fn corners_map_to_zero_and_max() {
    let r = full();
    assert_eq!(client_to_normalized(0, 0, &r), Some((0, 0)));
    let (nx, ny) = client_to_normalized(1920, 1080, &r).expect("far corner");
    assert_eq!(nx, 65_535, "right edge");
    assert_eq!(ny, 65_535, "bottom edge");
}

#[test]
fn outside_is_none() {
    let r = full();
    assert_eq!(client_to_normalized(-1, 0, &r), None, "left of rect");
    assert_eq!(client_to_normalized(0, -1, &r), None, "above rect");
    assert_eq!(client_to_normalized(1921, 540, &r), None, "right of rect");
    assert_eq!(client_to_normalized(960, 1081, &r), None, "below rect");
}

#[test]
fn clamped_mapping_keeps_releases_outside_the_frame() {
    let r = letterbox_rect(800, 600, 1600, 900); // y 75..525
    assert_eq!(client_to_normalized(-40, 10, &r), None);
    assert_eq!(client_to_normalized_clamped(-40, 10, &r), Some((0, 0)));
    assert_eq!(
        client_to_normalized_clamped(5000, 5000, &r),
        Some((65_535, 65_535))
    );
    let zero = Rect {
        x: 0,
        y: 0,
        w: 0,
        h: 0,
    };
    assert_eq!(client_to_normalized_clamped(1, 1, &zero), None);
}

#[test]
fn degenerate_rect_is_none() {
    let zero = Rect {
        x: 0,
        y: 0,
        w: 0,
        h: 100,
    };
    assert_eq!(client_to_normalized(0, 0, &zero), None);
}

#[test]
fn letterboxed_16_9_in_4_3_client() {
    // A 16:9 frame in a 4:3 client area: width-limited, pillarboxed.
    let r = letterbox_rect(800, 600, 1600, 900);
    assert_eq!(r.w, 800, "width-limited");
    assert_eq!(r.h, 450, "half the client height");
    assert_eq!(r.y, 75, "centered vertically");
    // Points in the top letterbox bar are outside the content.
    assert_eq!(client_to_normalized(400, 10, &r), None);
    // The content's center still maps to the middle.
    let (nx, ny) = client_to_normalized(400, 75 + 225, &r).expect("center");
    assert!((i64::from(nx) - 32_767).abs() <= 1);
    assert!((i64::from(ny) - 32_767).abs() <= 1);
}

#[test]
fn degenerate_client_gets_1x1_fallback() {
    let r = letterbox_rect(0, 0, 100, 100);
    assert_eq!(
        r,
        Rect {
            x: 0,
            y: 0,
            w: 1,
            h: 1
        }
    );
    let r = letterbox_rect(100, 100, 0, 0);
    assert_eq!(
        r,
        Rect {
            x: 0,
            y: 0,
            w: 1,
            h: 1
        }
    );
}
