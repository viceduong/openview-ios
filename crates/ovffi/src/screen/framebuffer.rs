//! Zero-copy access to the persistent framebuffer and its dirty rects.

use std::ffi::c_void;

use super::OvScreen;
use crate::cabi::write;
use crate::error::OV_ERR_PANIC;

/// Zero-copy framebuffer visitor: `pixels` points at the
/// assembler's persistent framebuffer (`len` bytes, `height` rows of
/// `stride` bytes, `format` 0 = BGRA / 1 = BGR), `rects` at `rect_count`
/// packed `u16` quadruples `x, y, w, h` — the rects the last delivered frame
/// changed. `rect_count == 0` means "the whole frame" (a full refresh was
/// asked for, or the last update had no rect list). Everything is valid only
/// for the duration of the call.
pub type FramebufferVisitor = extern "C" fn(
    user: *mut c_void,
    pixels: *const u8,
    len: usize,
    width: u32,
    height: u32,
    stride: u32,
    format: u8,
    seq: u64,
    rects: *const u16,
    rect_count: usize,
);

/// The rects the last delivered frame changed, as
/// `count` packed `u16` quadruples `x, y, w, h` (frame pixels) written to
/// `out` (room for `max_rects`). A full frame reports one rect covering it.
/// Returns the rect count (may exceed `max_rects`: only the first
/// `max_rects` are written), 0 before any frame, -1 on a null handle.
/// Shells that ignore this keep redrawing the whole frame, which is correct.
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`](crate::ov_screen_new),
/// with no concurrent call on it; `out` must be null or writable for
/// `max_rects * 4` u16s.
#[no_mangle]
pub unsafe extern "C" fn ov_screen_last_dirty_rects(
    s: *mut OvScreen,
    out: *mut u16,
    max_rects: usize,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        // SAFETY: `s` is null or a live screen with no concurrent mutation
        // (caller contract).
        let Some(screen) = (unsafe { s.as_ref() }) else {
            return -1;
        };
        let rects = screen.dec.last_dirty();
        if !out.is_null() {
            for (i, r) in rects.iter().take(max_rects).enumerate() {
                // SAFETY: `i < max_rects` and `out` is writable for
                // `max_rects * 4` u16s (caller contract), so the 4 u16s at
                // `out + i * 4` are in bounds.
                unsafe { write(&[r.x, r.y, r.w, r.h], out.add(i * 4)) };
            }
        }
        rects.len().min(i32::MAX as usize) as i32
    })
}

/// Zero-copy frames: call `visit` once, synchronously, with a
/// pointer to the current framebuffer plus the rects the last delivered
/// frame changed (`full != 0`: no rects — the shell wants everything, e.g.
/// a fresh texture). The shell uploads straight from the pointer (only the
/// dirty rects) instead of copying the whole framebuffer per update. Call
/// it right after the `ov_screen_feed` whose frame sink fired (or any time
/// for a full refresh) on the thread that feeds the screen — never from
/// inside a sink. Returns 0 (visited), -2 (no framebuffer yet), -1 (null
/// handle / visitor).
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`](crate::ov_screen_new),
/// with no concurrent call on it; `visit` must not call back into this
/// screen.
#[no_mangle]
pub unsafe extern "C" fn ov_screen_visit_framebuffer(
    s: *mut OvScreen,
    user: *mut c_void,
    visit: Option<FramebufferVisitor>,
    full: i32,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        // SAFETY: `s` is null or a live screen with no concurrent mutation
        // (caller contract).
        let (Some(screen), Some(visit)) = (unsafe { s.as_ref() }, visit) else {
            return -1;
        };
        let Some(fb) = screen.dec.frame() else {
            return -2;
        };
        let rects: Vec<u16> = if full != 0 {
            Vec::new()
        } else {
            screen
                .dec
                .last_dirty()
                .iter()
                .flat_map(|r| [r.x, r.y, r.w, r.h])
                .collect()
        };
        visit(
            user,
            fb.pixels.as_ptr(),
            fb.pixels.len(),
            fb.width,
            fb.height,
            fb.stride,
            fb.format.tag(),
            fb.seq,
            if rects.is_empty() {
                std::ptr::null()
            } else {
                rects.as_ptr()
            },
            rects.len() / 4,
        );
        0
    })
}
