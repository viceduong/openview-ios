//! OS clipboard I/O for the bidirectional clipboard sync.
//!
//! The decision logic (the echo-loop guard) is the pure
//! [`ovapp::clipboard::ClipboardSync`]; this module only reads and writes
//! the local clipboard through Win32, behind `#[cfg(windows)]` so the crate
//! still builds on other hosts (where the clipboard feature is simply
//! absent).
//!
//! Polling strategy: [`clip_sequence`] (`GetClipboardSequenceNumber`) is a
//! cheap, lock-free call the serve loop can make every iteration; only when
//! it changes do we take the clipboard lock for [`clip_read_text`].

#[cfg(windows)]
use windows::Win32::Foundation::{HANDLE, HGLOBAL};
#[cfg(windows)]
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
    IsClipboardFormatAvailable, OpenClipboard, SetClipboardData,
};
#[cfg(windows)]
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};

/// The Win32 `CF_UNICODETEXT` clipboard format.
#[cfg(windows)]
const CF_UNICODETEXT: u32 = windows::Win32::System::Ole::CF_UNICODETEXT.0 as u32;

/// The current clipboard sequence number — bumped on every clipboard
/// content change. Cheap enough to poll every loop iteration.
#[cfg(windows)]
pub(crate) fn clip_sequence() -> u32 {
    // SAFETY: a plain query without arguments.
    unsafe { GetClipboardSequenceNumber() }
}

/// Read the clipboard as UTF-8 text, if it currently holds text.
///
/// `None` when the clipboard has no text format or cannot be opened (another
/// process holding it open is a normal, transient condition — callers just
/// retry on the next poll).
#[cfg(windows)]
pub(crate) fn clip_read_text() -> Option<String> {
    // SAFETY: plain queries / the clipboard lock; on success the clipboard
    // is closed again by `CloseOnDrop` on every path below.
    unsafe {
        IsClipboardFormatAvailable(CF_UNICODETEXT).ok()?;
        OpenClipboard(None).ok()?;
    }
    // Always close, even on the early-outs below.
    let _guard = CloseOnDrop;
    // SAFETY: the clipboard is open; the returned handle is owned by the
    // clipboard and stays valid until it is closed (after this function).
    let h = HGLOBAL(unsafe { GetClipboardData(CF_UNICODETEXT) }.ok()?.0);
    // SAFETY: `h` is a live global memory handle (see above).
    let ptr = unsafe { GlobalLock(h) }.cast::<u8>().cast_const();
    if ptr.is_null() {
        return None;
    }
    // SAFETY: `ptr` points to the locked object of `GlobalSize(h)` bytes;
    // every read below stays within it, and it is unlocked before return.
    unsafe {
        // The object is NUL-terminated UTF-16; GlobalSize may over-report
        // (it returns in bytes, rounded up), so scan to the NUL.
        let size = GlobalSize(h);
        let mut units = size / 2;
        while units > 0 && *ptr.add((units - 1) * 2) == 0 && *ptr.add((units - 1) * 2 + 1) == 0 {
            units -= 1;
        }
        // Trim the terminating NUL itself (keep interior NULs out too: text
        // ends at the first NUL by Win32 convention).
        let mut end = 0usize;
        while end < units && !(*ptr.add(end * 2) == 0 && *ptr.add(end * 2 + 1) == 0) {
            end += 1;
        }
        let slice = std::slice::from_raw_parts(ptr.cast::<u16>(), end);
        let out = String::from_utf16_lossy(slice);
        let _ = GlobalUnlock(h);
        Some(out)
    }
}

/// Write UTF-8 `text` to the clipboard (empty text clears it). Returns
/// whether the clipboard was updated.
#[cfg(windows)]
pub(crate) fn clip_write_text(text: &str) -> bool {
    // UTF-16 with a NUL terminator.
    let mut units: Vec<u16> = text.encode_utf16().collect();
    units.push(0);

    // SAFETY: takes the clipboard lock, released by `CloseOnDrop` below.
    if unsafe { OpenClipboard(None) }.is_err() {
        return false;
    }
    let _guard = CloseOnDrop;
    // SAFETY: the clipboard is open (by us).
    if unsafe { EmptyClipboard() }.is_err() {
        return false;
    }
    // SAFETY: a plain allocation of `units.len() * 2` bytes.
    let Ok(h) = (unsafe { GlobalAlloc(GMEM_MOVEABLE, units.len() * 2) }) else {
        return false;
    };
    // SAFETY: `h` is the live allocation from above.
    let ptr = unsafe { GlobalLock(h) }.cast::<u8>();
    if ptr.is_null() {
        return false;
    }
    // SAFETY: `ptr` is the locked allocation of exactly `units.len() * 2`
    // bytes, which cannot overlap `units`; it is unlocked right after.
    unsafe {
        std::ptr::copy_nonoverlapping(units.as_ptr().cast::<u8>(), ptr, units.len() * 2);
        let _ = GlobalUnlock(h);
    }
    // SAFETY: the clipboard is open and `h` is an unlocked movable global
    // allocation; on success the system owns it (do not free it).
    unsafe { SetClipboardData(CF_UNICODETEXT, Some(HANDLE(h.0))) }.is_ok()
}

/// `CloseClipboard` on scope exit so an early return can never leave the
/// clipboard locked (the lock is system-global).
#[cfg(windows)]
struct CloseOnDrop;

#[cfg(windows)]
impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        // SAFETY: only constructed right after a successful OpenClipboard.
        let _ = unsafe { CloseClipboard() };
    }
}

#[cfg(not(windows))]
mod non_windows {
    //! Non-Windows stubs so the crate builds (and tests) on other hosts;
    //! the clipboard feature is simply unavailable there.

    pub(crate) fn clip_sequence() -> u32 {
        0
    }

    pub(crate) fn clip_read_text() -> Option<String> {
        None
    }

    pub(crate) fn clip_write_text(_text: &str) -> bool {
        false
    }
}

#[cfg(not(windows))]
pub(crate) use non_windows::{clip_read_text, clip_sequence, clip_write_text};
