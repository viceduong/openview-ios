//! Win32 cursor polling (`GetCursorInfo`) and shape rendering
//! (`GetIconInfo` + `GetDIBits`).

use ovapp::terminal::Command;
use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BITMAP, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, RGBQUAD,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorInfo, GetIconInfo, CURSORINFO, CURSOR_SHOWING, HICON, ICONINFO,
};

use super::shape::{compose_color, compose_mono, mono_stride};

/// The polled cursor state for one pump tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PolledCursor {
    /// Virtual-screen position of the cursor hotspot.
    pub(crate) x: i32,
    pub(crate) y: i32,
    /// The cursor-shape handle (never 0: a hidden cursor is not polled).
    pub(crate) handle: u64,
}

/// Poll the real cursor: `GetCursorInfo` gives position + `hCursor` in one
/// call (it also reports the show/hide state via `CURSOR_SHOWING`). Returns
/// `None` when the poll fails or the cursor is hidden.
#[must_use]
pub(crate) fn poll() -> Option<PolledCursor> {
    let mut ci = CURSORINFO {
        cbSize: std::mem::size_of::<CURSORINFO>() as u32,
        ptScreenPos: POINT::default(),
        ..Default::default()
    };
    // SAFETY: `ci` is a writable CURSORINFO with cbSize set, as documented.
    if unsafe { GetCursorInfo(&mut ci) }.is_err() || ci.hCursor.0.is_null() {
        return None;
    }
    if ci.flags.0 & CURSOR_SHOWING.0 == 0 {
        return None; // cursor hidden (e.g. during typing in some apps)
    }
    Some(PolledCursor {
        x: ci.ptScreenPos.x,
        y: ci.ptScreenPos.y,
        handle: ci.hCursor.0 as usize as u64,
    })
}

/// Largest cursor bitmap edge read from GDI (the wire cap downscales to
/// [`ovapp::terminal::MAX_CURSOR_DIM`] afterwards).
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
///   The mask must be read top-down for "AND first" indexing to hold;
///   reading it bottom-up swaps AND and XOR and turns the I-beam's
///   transparent box opaque white.
///
/// Every GDI handle obtained here (`GetIconInfo`'s two bitmaps, the
/// compatible DC) is released on all paths.
#[must_use]
pub(crate) fn render_shape(handle: u64) -> Option<Command> {
    let hcursor = HICON(handle as usize as *mut core::ffi::c_void);
    let mut info = ICONINFO::default();
    // SAFETY: `info` is a writable ICONINFO; GetIconInfo validates the
    // handle and fails cleanly on a stale one.
    if unsafe { GetIconInfo(hcursor, &mut info) }.is_err() {
        return None;
    }
    // Scope guard: release both bitmaps on every exit path.
    let out = (|| {
        let (mw, mh) = bitmap_dims(info.hbmMask)?;
        let (w, h, bgra) = if info.hbmColor.is_invalid() {
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
    for hbm in [info.hbmColor, info.hbmMask] {
        if !hbm.is_invalid() {
            // SAFETY: both bitmaps were created for us by GetIconInfo; we
            // own them and delete each exactly once.
            let _ = unsafe { DeleteObject(hbm.into()) };
        }
    }
    out
}

/// Read a GDI bitmap object's width/height via `GetObjectW` (both in
/// `1..=MAX_GDI_CURSOR_DIM * 2`).
fn bitmap_dims(hbm: HBITMAP) -> Option<(i32, i32)> {
    if hbm.is_invalid() {
        return None;
    }
    let mut bm = BITMAP {
        bmType: 0,
        bmWidth: 0,
        bmHeight: 0,
        bmWidthBytes: 0,
        bmPlanes: 0,
        bmBitsPixel: 0,
        bmBits: std::ptr::null_mut(),
    };
    // SAFETY: `bm` is a writable BITMAP of the size passed, for a bitmap
    // handle (GetObjectW fails cleanly on anything else).
    let n = unsafe {
        GetObjectW(
            hbm.into(),
            std::mem::size_of::<BITMAP>() as i32,
            Some((&mut bm as *mut BITMAP).cast()),
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

/// `BITMAPINFO` with room for a small colour table: `GetDIBits` writes the
/// 2-entry table of a 1bpp DIB (and up to 3 masks for bitfields) after the
/// header, which would overrun the 1-entry array of the plain struct.
#[repr(C)]
struct BitmapInfoWithTable {
    header: BITMAPINFOHEADER,
    colors: [RGBQUAD; 4],
}

/// `GetDIBits` of `hbm` as a **top-down** DIB (`bits` bpp, `w` x `h`) into
/// a buffer of `stride * h` bytes.
fn read_dib(hbm: HBITMAP, w: i32, h: i32, bits: u16, stride: usize) -> Option<Vec<u8>> {
    let len = stride.checked_mul(h as usize)?;
    let mut buf = vec![0u8; len];
    let mut bmi = BitmapInfoWithTable {
        header: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // negative = top-down: buffer row 0 = bitmap row 0
            biPlanes: 1,
            biBitCount: bits,
            biCompression: BI_RGB.0,
            biSizeImage: len as u32,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        },
        colors: [RGBQUAD::default(); 4],
    };
    // GetDIBits needs a DC; a compatible DC for the screen is enough.
    // SAFETY: no DC = a memory DC compatible with the screen; released below.
    let hdc = unsafe { CreateCompatibleDC(None) };
    if hdc.is_invalid() {
        return None;
    }
    // SAFETY: `buf` holds `h` DWORD-aligned rows of the declared DIB, and
    // `bmi` starts with the BITMAPINFO header followed by room for the
    // colour table GetDIBits writes for 1bpp.
    let lines = unsafe {
        GetDIBits(
            hdc,
            hbm,
            0,
            h as u32,
            Some(buf.as_mut_ptr().cast()),
            (&mut bmi as *mut BitmapInfoWithTable).cast::<BITMAPINFO>(),
            DIB_RGB_COLORS,
        )
    };
    // SAFETY: paired with CreateCompatibleDC above.
    let _ = unsafe { DeleteDC(hdc) };
    (lines == h).then_some(buf)
}

/// A mask bitmap as top-down 1bpp rows: (bits, stride).
fn read_mask(hbm: HBITMAP, w: i32, h: i32) -> Option<(Vec<u8>, usize)> {
    let stride = mono_stride(w as usize);
    read_dib(hbm, w, h, 1, stride).map(|b| (b, stride))
}

/// A colour bitmap as top-down 32bpp BGRA (alpha 0 when it has none).
fn read_color(hbm: HBITMAP, w: i32, h: i32) -> Option<Vec<u8>> {
    read_dib(hbm, w, h, 32, w as usize * 4)
}
