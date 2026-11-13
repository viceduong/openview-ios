//! Local-input normalization for the viewer window.
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
pub(crate) struct Rect {
    /// Left edge.
    pub(crate) x: i32,
    /// Top edge.
    pub(crate) y: i32,
    /// Width in pixels.
    pub(crate) w: i32,
    /// Height in pixels.
    pub(crate) h: i32,
}

/// One local input event captured by the render window's `WndProc`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LocalEvent {
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
pub(crate) fn client_to_normalized(
    client_x: i32,
    client_y: i32,
    rect: &Rect,
) -> Option<(u32, u32)> {
    if rect.w <= 0 || rect.h <= 0 || client_x < rect.x || client_y < rect.y {
        return None;
    }
    let dx = client_x - rect.x;
    let dy = client_y - rect.y;
    if dx > rect.w || dy > rect.h {
        return None;
    }
    // i64 avoids overflow for extreme client sizes; the clamp keeps the
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
pub(crate) fn client_to_normalized_clamped(
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
pub(crate) fn letterbox_rect(client_w: i32, client_h: i32, frame_w: u32, frame_h: u32) -> Rect {
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
mod tests;
