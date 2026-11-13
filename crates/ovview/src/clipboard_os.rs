//! OS clipboard I/O (v0.26 "bidirectional clipboard sync").
//!
//! Structure mirrors `ovhost::cursor` / `ovhost::mic`: the decision logic
//! (the echo-loop guard) is the pure [`ovapp::clipboard::ClipboardSync`];
//! this module is only the Win32 FFI that reads and writes the local
//! clipboard, behind `#[cfg(windows)]` so the crate still builds on other
//! hosts (where the clipboard feature is simply absent).
//!
//! Raw Win32 imports (user32 + kernel32), no `windows`/`winapi` crates —
//! linked implicitly by MSVC, same as the rest of the host crates.
//!
//! Polling strategy: [`clip_sequence`] (`GetClipboardSequenceNumber`) is a
//! cheap, lock-free call the serve loop can make every iteration; only when
//! it changes do we take the clipboard lock for [`clip_read_text`].

/// The Win32 `CF_UNICODETEXT` clipboard format.
#[cfg(windows)]
const CF_UNICODETEXT: u32 = 13;

/// The current clipboard sequence number — bumped on every clipboard
/// content change. Cheap enough to poll every loop iteration.
#[cfg(windows)]
pub fn clip_sequence() -> u32 {
    #[link(name = "user32")]
    extern "system" {
        fn GetClipboardSequenceNumber() -> u32;
    }
    unsafe { GetClipboardSequenceNumber() }
}

/// Read the clipboard as UTF-8 text, if it currently holds text.
///
/// `None` when the clipboard has no text format or cannot be opened (another
/// process holding it open is a normal, transient condition — callers just
/// retry on the next poll).
#[cfg(windows)]
pub fn clip_read_text() -> Option<String> {
    #[link(name = "user32")]
    extern "system" {
        fn OpenClipboard(owner: isize) -> i32;
        fn IsClipboardFormatAvailable(format: u32) -> i32;
        fn GetClipboardData(format: u32) -> isize;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalLock(h: isize) -> *mut u8;
        fn GlobalUnlock(h: isize) -> i32;
        fn GlobalSize(h: isize) -> usize;
    }

    unsafe {
        if IsClipboardFormatAvailable(CF_UNICODETEXT) == 0 {
            return None;
        }
        if OpenClipboard(0) == 0 {
            return None;
        }
        // Always close, even on the early-outs below (the guard owns
        // CloseClipboard).
        let _guard = CloseOnDrop;
        let h = GetClipboardData(CF_UNICODETEXT);
        if h == 0 {
            return None;
        }
        let ptr = GlobalLock(h);
        if ptr.is_null() {
            return None;
        }
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
        let slice = std::slice::from_raw_parts(ptr as *const u16, end);
        let out = String::from_utf16_lossy(slice);
        GlobalUnlock(h);
        Some(out)
    }
}

/// Write UTF-8 `text` to the clipboard (empty text clears it). Returns
/// whether the clipboard was updated.
#[cfg(windows)]
pub fn clip_write_text(text: &str) -> bool {
    #[link(name = "user32")]
    extern "system" {
        fn OpenClipboard(owner: isize) -> i32;
        fn EmptyClipboard() -> i32;
        fn SetClipboardData(format: u32, h: isize) -> isize;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalAlloc(flags: u32, bytes: usize) -> isize;
        fn GlobalLock(h: isize) -> *mut u8;
        fn GlobalUnlock(h: isize) -> i32;
    }

    const GMEM_MOVEABLE: u32 = 0x0002;

    // UTF-16 with a NUL terminator.
    let mut units: Vec<u16> = text.encode_utf16().collect();
    units.push(0);

    unsafe {
        if OpenClipboard(0) == 0 {
            return false;
        }
        let _guard = CloseOnDrop;
        if EmptyClipboard() == 0 {
            return false;
        }
        let h = GlobalAlloc(GMEM_MOVEABLE, units.len() * 2);
        if h == 0 {
            return false;
        }
        let ptr = GlobalLock(h);
        if ptr.is_null() {
            return false;
        }
        std::ptr::copy_nonoverlapping(units.as_ptr() as *const u8, ptr, units.len() * 2);
        GlobalUnlock(h);
        // On success the system owns `h`; do not free it.
        SetClipboardData(CF_UNICODETEXT, h) != 0
    }
}

/// `CloseClipboard` on scope exit so an early return can never leave the
/// clipboard locked (the lock is system-global).
#[cfg(windows)]
struct CloseOnDrop;
#[cfg(windows)]
impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        #[link(name = "user32")]
        extern "system" {
            fn CloseClipboard() -> i32;
        }
        unsafe {
            CloseClipboard();
        }
    }
}

#[cfg(not(windows))]
mod non_windows {
    //! Non-Windows stubs so the crate builds (and tests) on other hosts;
    //! the clipboard feature is simply unavailable there.

    pub fn clip_sequence() -> u32 {
        0
    }

    pub fn clip_read_text() -> Option<String> {
        None
    }

    pub fn clip_write_text(_text: &str) -> bool {
        false
    }
}

#[cfg(not(windows))]
pub use non_windows::{clip_read_text, clip_sequence, clip_write_text};
