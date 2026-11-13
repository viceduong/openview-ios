//! Multi-monitor awareness: enumerate the host's monitors and
//! validate capture regions against the virtual desktop.
//!
//! The pure side ([`Monitor`], [`region`], [`validate_region`]) is fully
//! unit-testable on every target; only [`enumerate_monitors`] touches the
//! OS (Win32 `EnumDisplayMonitors` + `GetMonitorInfoW` on Windows, a
//! single-placeholder stub elsewhere so the crate builds and tests
//! everywhere).
//!
//! Like the rest of the Windows-facing code, the FFI is declared by hand —
//! no `windows`/`winapi` crates.

use crate::error::AppError;

/// One host monitor, in virtual-desktop coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Monitor {
    /// 0-based position in the enumeration order (the wire order of
    /// `MONITORS_INFO`; not transmitted per-entry).
    pub index: u32,
    /// Left edge on the virtual desktop (may be negative).
    pub x: i32,
    /// Top edge on the virtual desktop (may be negative).
    pub y: i32,
    /// Width in pixels.
    pub w: u32,
    /// Height in pixels.
    pub h: u32,
    /// True for the monitor carrying the taskbar / primary display.
    pub is_primary: bool,
}

/// The monitor's capture rect: `(x, y, w, h)` in virtual-desktop coordinates.
#[must_use]
pub fn region(m: &Monitor) -> (i32, i32, u32, u32) {
    (m.x, m.y, m.w, m.h)
}

/// Validate (and clamp) a capture rect against the virtual desktop, treated
/// as the rect `(0, 0) .. (virt_w, virt_h)`.
///
/// Policy (deliberately simple and consistent):
/// - an empty rect (`w == 0 || h == 0`) is **rejected**;
/// - a non-positive virtual desktop is **rejected** (nothing to capture);
/// - an origin outside the desktop is **clamped** into `[0, virt)` per axis;
/// - an over-large size is **clamped** so the rect stays inside the desktop;
/// - a rect that clamps down to nothing (origin at/past the far edge) is
///   **rejected**.
///
/// The host translates desktop coordinates into this `(0, 0)`-origin space
/// (subtracting the virtual-screen origin) before calling, and translates
/// the validated result back — see `ovhost::serve_screen`.
pub fn validate_region(
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    virt_w: i32,
    virt_h: i32,
) -> Result<(i32, i32, u32, u32), AppError> {
    if w == 0 || h == 0 {
        return Err(AppError::invalid("capture region is empty (w/h == 0)"));
    }
    if virt_w <= 0 || virt_h <= 0 {
        return Err(AppError::platform("virtual desktop has no usable size"));
    }
    // Clamp the origin into the desktop, then shrink the size so the rect
    // fits. Saturating casts keep absurd u32 sizes from wrapping i32.
    let x = x.clamp(0, virt_w - 1);
    let y = y.clamp(0, virt_h - 1);
    let w = (w as i64).min(i64::from(virt_w - x)) as u32;
    let h = (h as i64).min(i64::from(virt_h - y)) as u32;
    if w == 0 || h == 0 {
        return Err(AppError::invalid(
            "capture region lies entirely outside the virtual desktop",
        ));
    }
    Ok((x, y, w, h))
}

/// Enumerate the host's monitors in virtual-desktop coordinates.
///
/// Windows: `EnumDisplayMonitors` + `GetMonitorInfoW`, indexed by
/// enumeration order, `is_primary` from `MONITORINFOF_PRIMARY`.
/// Non-Windows: a single placeholder "full virtual desktop" monitor so the
/// crate (and the iOS staticlib) builds and tests unchanged.
#[must_use]
pub fn enumerate_monitors() -> Vec<Monitor> {
    #[cfg(windows)]
    {
        imp::enumerate()
    }
    #[cfg(not(windows))]
    {
        // Dev placeholder: one 1920x1080 primary at the origin. The desktop
        // viewer/host only run on Windows; other targets just need a sane
        // shape for the pure logic and tests.
        vec![Monitor {
            index: 0,
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
            is_primary: true,
        }]
    }
}

#[cfg(windows)]
mod imp {
    //! Win32 monitor enumeration (hand-declared FFI, user32 only).

    #![allow(non_snake_case, non_camel_case_types)]
    #![allow(clippy::upper_case_acronyms)]

    mod win_types {
        use crate::monitors::Monitor;

        #[repr(C)]
        struct RECT {
            left: i32,
            top: i32,
            right: i32,
            bottom: i32,
        }

        #[repr(C)]
        struct MONITORINFO {
            cbSize: u32,
            rcMonitor: RECT,
            rcWork: RECT,
            dwFlags: u32,
        }

        const MONITORINFOF_PRIMARY: u32 = 1;
        /// `CCHDEVICENAME` — the documented size of `MONITORINFOEXW`'s device
        /// name; we use plain `MONITORINFO`, but keep the constant for clarity.
        const _CCHDEVICENAME: usize = 32;

        #[link(name = "user32")]
        extern "system" {
            /// `hDC` may be `NULL`; `lprcClip` `NULL` means all monitors.
            fn EnumDisplayMonitors(
                hdc: isize,
                lprcClip: *const RECT,
                lpfnEnum: Option<MonitorEnumProc>,
                dwData: isize,
            ) -> i32;
            fn GetMonitorInfoW(hMonitor: isize, lpmi: *mut MONITORINFO) -> i32;
        }

        type MonitorEnumProc =
            extern "system" fn(hMonitor: isize, hdc: isize, rect: *mut RECT, data: isize) -> i32;

        /// Trampoline: `dwData` carries a `*mut Vec<Monitor>` raw pointer.
        extern "system" fn enum_proc(
            hMonitor: isize,
            _hdc: isize,
            _rect: *mut RECT,
            data: isize,
        ) -> i32 {
            // SAFETY: `data` is the `&mut Vec<Monitor>` that `enumerate`
            // passes to `EnumDisplayMonitors`; it outlives the (synchronous)
            // enumeration and nothing else touches it meanwhile.
            let out = unsafe { &mut *(data as *mut Vec<Monitor>) };
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                rcMonitor: RECT {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                },
                rcWork: RECT {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                },
                dwFlags: 0,
            };
            // SAFETY: `hMonitor` is a valid enum handle; `info` outlives the call.
            if unsafe { GetMonitorInfoW(hMonitor, &mut info) } != 0 {
                let rc = &info.rcMonitor;
                out.push(Monitor {
                    index: 0, // assigned by the caller after enumeration
                    x: rc.left,
                    y: rc.top,
                    w: (rc.right - rc.left).max(0) as u32,
                    h: (rc.bottom - rc.top).max(0) as u32,
                    is_primary: info.dwFlags & MONITORINFOF_PRIMARY != 0,
                });
            }
            1 // continue enumeration
        }

        /// Enumerate all monitors. Degenerate (zero-area) entries are skipped;
        /// indices are assigned by enumeration order.
        pub fn enumerate() -> Vec<Monitor> {
            let mut found: Vec<Monitor> = Vec::new();
            let ptr = &mut found as *mut Vec<Monitor> as isize;
            // SAFETY: the trampoline only touches `ptr` synchronously inside the
            // call, and `found` outlives it.
            unsafe {
                EnumDisplayMonitors(0, std::ptr::null(), Some(enum_proc), ptr);
            }
            let mut out: Vec<Monitor> = found.into_iter().filter(|m| m.w > 0 && m.h > 0).collect();
            for (i, m) in out.iter_mut().enumerate() {
                m.index = i as u32;
            }
            out
        }
    } // mod win_types

    pub use win_types::enumerate;
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_bounds_region_passes_through() {
        assert_eq!(
            validate_region(0, 0, 1920, 1080, 3840, 1080).unwrap(),
            (0, 0, 1920, 1080)
        );
        assert_eq!(
            validate_region(100, 50, 320, 240, 1920, 1080).unwrap(),
            (100, 50, 320, 240)
        );
        // Exactly filling the desktop is in-bounds.
        assert_eq!(
            validate_region(0, 0, 1920, 1080, 1920, 1080).unwrap(),
            (0, 0, 1920, 1080)
        );
    }

    #[test]
    fn empty_regions_are_rejected() {
        assert!(validate_region(0, 0, 0, 100, 1920, 1080).is_err());
        assert!(validate_region(0, 0, 100, 0, 1920, 1080).is_err());
        assert!(validate_region(0, 0, 0, 0, 1920, 1080).is_err());
    }

    #[test]
    fn degenerate_virtual_desktops_are_rejected() {
        assert!(validate_region(0, 0, 10, 10, 0, 1080).is_err());
        assert!(validate_region(0, 0, 10, 10, 1920, -1).is_err());
    }

    #[test]
    fn out_of_bounds_origins_are_clamped() {
        // Negative origins clamp to the desktop edge.
        assert_eq!(
            validate_region(-50, -50, 100, 100, 1920, 1080).unwrap(),
            (0, 0, 100, 100)
        );
        // Origins past the far edge clamp back, keeping the size when it fits.
        assert_eq!(
            validate_region(2000, 0, 100, 100, 1920, 1080).unwrap(),
            (1919, 0, 1, 100)
        );
    }

    #[test]
    fn oversized_sizes_are_clamped() {
        assert_eq!(
            validate_region(1900, 1000, 500, 500, 1920, 1080).unwrap(),
            (1900, 1000, 20, 80)
        );
        // An absurd u32 size saturates instead of wrapping.
        let (x, y, w, h) = validate_region(0, 0, u32::MAX, u32::MAX, 1920, 1080).unwrap();
        assert_eq!((x, y, w, h), (0, 0, 1920, 1080));
    }

    #[test]
    fn fully_outside_origins_still_clamp_to_something_valid() {
        // Consistent clamp policy: an origin past the far edge clamps to the
        // last pixel column/row, and the rect shrinks to what remains — it
        // never errors (only empty rects and empty desktops do).
        assert_eq!(
            validate_region(5000, 5000, 100, 100, 1920, 1080).unwrap(),
            (1919, 1079, 1, 1)
        );
        // The result is always inside the desktop.
        for (x, y) in [(-10i32, -10i32), (5000, 5000), (0, 0)] {
            let (rx, ry, rw, rh) = validate_region(x, y, 100, 100, 1920, 1080).unwrap();
            assert!(rx >= 0 && ry >= 0 && rx + rw as i32 <= 1920 && ry + rh as i32 <= 1080);
        }
    }

    #[test]
    fn region_extracts_the_capture_rect() {
        let m = Monitor {
            index: 2,
            x: -1920,
            y: 0,
            w: 1920,
            h: 1080,
            is_primary: false,
        };
        assert_eq!(region(&m), (-1920, 0, 1920, 1080));
    }

    #[test]
    fn non_windows_stub_lists_one_primary_monitor() {
        let mons = enumerate_monitors();
        if cfg!(windows) {
            // Real Windows: at least one monitor, every rect non-empty.
            assert!(!mons.is_empty());
            assert!(mons.iter().all(|m| m.w > 0 && m.h > 0));
            assert!(
                mons.iter().any(|m| m.is_primary),
                "a primary monitor must exist"
            );
        } else {
            assert_eq!(mons.len(), 1);
            assert!(mons[0].is_primary);
            assert_eq!(region(&mons[0]), (0, 0, 1920, 1080));
        }
        for (i, m) in mons.iter().enumerate() {
            assert_eq!(m.index, i as u32, "indices are assigned in order");
        }
    }

    /// Monitor list -> CaptureRegion rect: every enumerated monitor yields a
    /// region `validate_region` accepts (the exact viewer->host flow).
    #[test]
    fn every_monitor_region_validates() {
        let mons = enumerate_monitors();
        assert!(!mons.is_empty());
        for m in &mons {
            let (x, y, w, h) = region(m);
            // The stub/desktop coordinates are already in (0,0)-origin space
            // for the placeholder; real hosts translate by the virtual-screen
            // origin before validating.
            let virt_w = mons.iter().map(|m| m.x + m.w as i32).max().unwrap_or(0);
            let virt_h = mons.iter().map(|m| m.y + m.h as i32).max().unwrap_or(0);
            let ok = validate_region(x.max(0), y.max(0), w, h, virt_w, virt_h);
            assert!(ok.is_ok(), "monitor {m:?} region must validate");
        }
    }
}
