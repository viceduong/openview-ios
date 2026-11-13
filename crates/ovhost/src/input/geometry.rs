//! Pure coordinate mapping: normalized POINTER points → desktop pixels →
//! `MOUSEEVENTF_VIRTUALDESK` coordinates.

/// A captured screen region: `(x, y)` origin on the desktop plus `w x h` size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Region {
    /// Left edge of the region on the desktop.
    pub(crate) x: i32,
    /// Top edge of the region on the desktop.
    pub(crate) y: i32,
    /// Region width in pixels.
    pub(crate) w: u32,
    /// Region height in pixels.
    pub(crate) h: u32,
}

/// The virtual screen (all monitors): origin `(x, y)` and size `(w, h)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VirtualScreen {
    /// Left edge of the virtual screen (can be negative with side monitors).
    pub(crate) x: i32,
    /// Top edge of the virtual screen.
    pub(crate) y: i32,
    /// Total width of the virtual screen.
    pub(crate) w: u32,
    /// Total height of the virtual screen.
    pub(crate) h: u32,
}

impl VirtualScreen {
    /// Query the desktop's virtual-screen geometry via `GetSystemMetrics`
    /// (`SM_XVIRTUALSCREEN`/`SM_YVIRTUALSCREEN`/`SM_CXVIRTUALSCREEN`/
    /// `SM_CYVIRTUALSCREEN`). Falls back to a 1x1 screen when the metrics
    /// report nothing usable.
    #[must_use]
    pub(crate) fn query() -> Self {
        #[cfg(windows)]
        {
            let (x, y, w, h) = super::win32::virtual_screen_metrics();
            if w > 0 && h > 0 {
                Self {
                    x,
                    y,
                    w: w as u32,
                    h: h as u32,
                }
            } else {
                Self::primary(w, h)
            }
        }
        #[cfg(not(windows))]
        {
            Self::primary(0, 0)
        }
    }

    /// A fallback virtual screen from (possibly invalid) primary-monitor
    /// metrics: degenerate sizes collapse to a 1x1 screen so the mapping math
    /// never divides by zero.
    #[must_use]
    fn primary(w: i32, h: i32) -> Self {
        Self {
            x: 0,
            y: 0,
            w: w.max(1) as u32,
            h: h.max(1) as u32,
        }
    }
}

/// Map a normalized POINTER point (0..=65535 in the viewed frame, 65535 =
/// the last pixel) to the desktop pixel it designates inside `region`
/// (`region.x + round(x * (w - 1) / 65535)`, always inside the region).
#[must_use]
pub(super) fn map_to_region(x: u32, y: u32, region: &Region) -> (i32, i32) {
    let axis = |n: u32, origin: i32, span: u32| -> i32 {
        let last = i64::from(span.max(1) - 1);
        let n = i64::from(n.min(65_535));
        let off = (n * last + 32_767) / 65_535;
        (i64::from(origin) + off) as i32
    };
    (axis(x, region.x, region.w), axis(y, region.y, region.h))
}

/// Map a desktop pixel to `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`
/// coordinates (0..=65535 across the virtual screen, whose origin may be
/// negative). Windows maps such a coordinate `n` back to pixel
/// `origin + floor(n * span / 65536)`; this picks the smallest `n` that lands
/// exactly on the requested pixel (`ceil(off * 65536 / span)`). Pixels off
/// the virtual screen clamp to its edge.
#[must_use]
pub(super) fn to_virtual_desk(px: i32, py: i32, vs: &VirtualScreen) -> (i32, i32) {
    let axis = |p: i32, origin: i32, span: u32| -> i32 {
        let span = i64::from(span.max(1));
        let off = (i64::from(p) - i64::from(origin)).clamp(0, span - 1);
        ((off * 65_536 + span - 1) / span).min(65_535) as i32
    };
    (axis(px, vs.x, vs.w), axis(py, vs.y, vs.h))
}

/// The pixel Windows lands on for a VIRTUALDESK coordinate (inverse of
/// [`to_virtual_desk`]; used by the dry-run backend and the tests).
#[must_use]
pub(super) fn from_virtual_desk(nx: i32, ny: i32, vs: &VirtualScreen) -> (i32, i32) {
    let axis = |n: i32, origin: i32, span: u32| -> i32 {
        (i64::from(origin) + i64::from(n.clamp(0, 65_535)) * i64::from(span.max(1)) / 65_536) as i32
    };
    (axis(nx, vs.x, vs.w), axis(ny, vs.y, vs.h))
}

/// Map a normalized POINTER point straight to VIRTUALDESK coordinates
/// ([`map_to_region`] then [`to_virtual_desk`]).
#[must_use]
pub(super) fn map_to_absolute(
    x: u32,
    y: u32,
    region: &Region,
    virtual_screen: &VirtualScreen,
) -> (i32, i32) {
    let (px, py) = map_to_region(x, y, region);
    to_virtual_desk(px, py, virtual_screen)
}
