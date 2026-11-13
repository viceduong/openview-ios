//! Local-input normalization for the viewer window (spec v0.12 "desktop
//! render window + local input injection").
//!
//! The render window receives mouse events in *client* pixels. To send them
//! to the host as POINTER commands they must be re-normalized into the
//! letterboxed *content* rect (the part of the window that actually shows the
//! remote frame) and scaled to the protocol's 0..=65535 space — exactly what
//! the iOS viewer's `normalizedPoint` does for touches.
//!
//! Everything here is pure math so it is unit-testable on every target,
//! including non-Windows CI where the window itself does not exist.

use ovapp::terminal::{MouseButton, PointerKind};

/// A rectangle in window *client* coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    /// Left edge.
    pub x: i32,
    /// Top edge.
    pub y: i32,
    /// Width in pixels.
    pub w: i32,
    /// Height in pixels.
    pub h: i32,
}

/// One local input event captured by the render window's `WndProc`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalEvent {
    /// A pointer event at client pixel `(client_x, client_y)`.
    Pointer {
        /// Move / Down / Up.
        kind: PointerKind,
        /// The button for Down / Up (Left for moves).
        button: MouseButton,
        /// Client-space x pixel.
        client_x: i32,
        /// Client-space y pixel.
        client_y: i32,
    },
    /// A keyboard event for a Windows virtual-key code.
    Key {
        /// Windows virtual-key code (`WM_KEYDOWN`'s `wParam`).
        vk: u16,
        /// Set-1 scancode (`lParam` bits 16..=23).
        scan: u16,
        /// Extended key (`lParam` bit 24).
        extended: bool,
        /// `true` on key-down, `false` on key-up.
        down: bool,
    },
    /// Mouse wheel, in 1/120 notch units (`dy > 0` = away from the user,
    /// `dx > 0` = right).
    Wheel { dy: i16, dx: i16 },
    /// A line of text typed into the (hidden) console stdin, forwarded from
    /// the background stdin reader thread.
    Line(String),
}

/// Map a client-space point into the normalized POINTER space (0..=65535)
/// relative to the letterboxed content `rect`.
///
/// Returns `None` when the point falls outside `rect` (the black letterbox
/// bars around the frame are not part of the remote screen). Inside, the
/// result is clamped to 0..=65535 so rounding can never wrap.
#[must_use]
pub fn client_to_normalized(client_x: i32, client_y: i32, rect: &Rect) -> Option<(u32, u32)> {
    if rect.w <= 0 || rect.h <= 0 || client_x < rect.x || client_y < rect.y {
        return None;
    }
    let dx = client_x - rect.x;
    let dy = client_y - rect.y;
    if dx > rect.w || dy > rect.h {
        return None;
    }
    // i128 avoids overflow for extreme client sizes; the clamp keeps the
    // right/bottom edge inclusive-ish at exactly 65535.
    let nx = ((i64::from(dx) * 65_535 / i64::from(rect.w)).clamp(0, 65_535)) as u32;
    let ny = ((i64::from(dy) * 65_535 / i64::from(rect.h)).clamp(0, 65_535)) as u32;
    Some((nx, ny))
}

/// Like [`client_to_normalized`], but a point outside `rect` is clamped to
/// its edge instead of dropped — for button releases and drags, which must
/// reach the host even when the mouse left the frame (a dropped UP leaves
/// the button stuck down on the host).
#[must_use]
pub fn client_to_normalized_clamped(
    client_x: i32,
    client_y: i32,
    rect: &Rect,
) -> Option<(u32, u32)> {
    if rect.w <= 0 || rect.h <= 0 {
        return None;
    }
    client_to_normalized(
        client_x.clamp(rect.x, rect.x + rect.w),
        client_y.clamp(rect.y, rect.y + rect.h),
        rect,
    )
}

/// Compute the letterboxed content rect: the largest rect of `frame_aspect`
/// (w/h) that fits inside the `client` rect, centered both ways.
///
/// Degenerate inputs (zero client size or zero frame dimension) collapse to a
/// 1x1 rect at the client origin so callers never divide by zero.
#[must_use]
pub fn letterbox_rect(client_w: i32, client_h: i32, frame_w: u32, frame_h: u32) -> Rect {
    if client_w <= 0 || client_h <= 0 || frame_w == 0 || frame_h == 0 {
        return Rect {
            x: 0,
            y: 0,
            w: 1,
            h: 1,
        };
    }
    let fw = f64::from(frame_w);
    let fh = f64::from(frame_h);
    let scale = (f64::from(client_w) / fw).min(f64::from(client_h) / fh);
    let w = (fw * scale).round().max(1.0) as i32;
    let h = (fh * scale).round().max(1.0) as i32;
    Rect {
        x: (client_w - w) / 2,
        y: (client_h - h) / 2,
        w,
        h,
    }
}

#[cfg(test)]
mod tests {
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
}
