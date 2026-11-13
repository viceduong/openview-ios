//! The GDI path's HUD overlay: the health lines and the dead-peer banner,
//! drawn with `TextOutW` over the blitted frame. (The Direct2D presenter
//! draws its own with DirectWrite.)

use windows::Win32::Foundation::{COLORREF, SIZE};
use windows::Win32::Graphics::Gdi::{
    GetTextExtentPoint32W, SetBkMode, SetTextColor, TextOutW, HDC, TRANSPARENT,
};

use super::HudState;

/// HUD text color: pale green (`COLORREF` is 0x00BBGGRR).
const HUD_COLOR: COLORREF = COLORREF(0x00B0_FF90);
/// Dead-peer banner color: warm red.
const DEAD_COLOR: COLORREF = COLORREF(0x0000_4DFF);
/// Line height of the HUD text (px).
const LINE_H: i32 = 16;

/// The banner shown while the peer is silent.
const DEAD_BANNER: &str = "Peer disconnected — waiting for traffic…";

/// UTF-16 with a NUL terminator. The terminator is part of the count passed
/// to `TextOutW`, as it always has been on this path.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Draw the HUD lines (default font, transparent background over the
/// frame) at `(x, y)` and, when the peer is dead, the banner centered in
/// `area_w`.
///
/// # Safety
///
/// `hdc` must be a live DC (from `BeginPaint`) on the painting thread.
pub(super) unsafe fn draw_hud(hdc: HDC, hud: &HudState, x: i32, y: i32, area_w: i32) {
    // SAFETY: (all blocks below) `hdc` is live per the contract; every text
    // buffer outlives the call that reads it.
    unsafe {
        // TRANSPARENT background so the frame shows through the text.
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, HUD_COLOR);
    }
    if hud.visible {
        for (i, line) in hud.lines.iter().enumerate() {
            let w = wide(line);
            // SAFETY: as above.
            let _ = unsafe { TextOutW(hdc, x, y + (i as i32) * LINE_H, &w) };
        }
    }
    if hud.dead {
        let w = wide(DEAD_BANNER);
        let mut size = SIZE::default();
        // SAFETY: as above; measured with the DC the text is drawn on.
        unsafe {
            let _ = GetTextExtentPoint32W(hdc, &w, &mut size);
            SetTextColor(hdc, DEAD_COLOR);
        }
        let bx = ((area_w - size.cx) / 2).max(0);
        let by = if hud.visible {
            y + hud.lines.len() as i32 * LINE_H + 12
        } else {
            y + 12
        };
        // SAFETY: as above.
        let _ = unsafe { TextOutW(hdc, bx, by, &w) };
    }
}
