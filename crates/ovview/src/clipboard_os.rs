//! OS clipboard I/O for bidirectional clipboard sync.
//!
//! Structure mirrors `ovhost::cursor` / `ovhost::mic`: the decision logic
//! (the echo-loop guard) is the pure [`ovapp::clipboard::ClipboardSync`];
//! this module is only the Win32 calls (through the `windows` crate) that
//! read and write the local clipboard, behind `#[cfg(windows)]` so the crate
//! still builds on other hosts (where the clipboard feature is simply
//! absent).
//!
//! Polling strategy: [`clip_sequence`] (`GetClipboardSequenceNumber`) is a
//! cheap, lock-free call the serve loop can make every iteration; only when
//! it changes do we take the clipboard lock for [`clip_read_text`].

#[cfg(windows)]
pub(crate) use windows_impl::{clip_read_text, clip_sequence, clip_write_text};

#[cfg(not(windows))]
pub(crate) use non_windows::{clip_read_text, clip_sequence, clip_write_text};

#[cfg(windows)]
mod windows_impl {
    use windows::Win32::Foundation::{HANDLE, HGLOBAL};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
        IsClipboardFormatAvailable, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
    };
    use windows::Win32::System::Ole::CF_UNICODETEXT;

    /// `CF_UNICODETEXT` as the `u32` the clipboard calls take.
    const UNICODE_TEXT: u32 = CF_UNICODETEXT.0 as u32;

    /// The current clipboard sequence number — bumped on every clipboard
    /// content change. Cheap enough to poll every loop iteration.
    pub(crate) fn clip_sequence() -> u32 {
        // SAFETY: no preconditions; a plain counter read.
        unsafe { GetClipboardSequenceNumber() }
    }

    /// Read the clipboard as UTF-8 text, if it currently holds text.
    ///
    /// `None` when the clipboard has no text format or cannot be opened
    /// (another process holding it open is a normal, transient condition —
    /// callers just retry on the next poll).
    pub(crate) fn clip_read_text() -> Option<String> {
        // SAFETY: no preconditions.
        unsafe { IsClipboardFormatAvailable(UNICODE_TEXT) }.ok()?;
        // SAFETY: no owner window; balanced by the guard below.
        unsafe { OpenClipboard(None) }.ok()?;
        // Always close, even on the early-outs below.
        let _guard = CloseOnDrop;
        // SAFETY: the clipboard is open (guarded above).
        let h = HGLOBAL(unsafe { GetClipboardData(UNICODE_TEXT) }.ok()?.0);
        // SAFETY: `h` is the clipboard's live CF_UNICODETEXT memory object,
        // valid while the clipboard stays open.
        let ptr = unsafe { GlobalLock(h) } as *const u16;
        if ptr.is_null() {
            return None;
        }
        // SAFETY: a locked global object spans `GlobalSize` bytes (possibly
        // over-reported, rounded up, never under) and is suitably aligned
        // for u16; it stays locked until `GlobalUnlock` below.
        let units = unsafe { std::slice::from_raw_parts(ptr, GlobalSize(h) / 2) };
        // NUL-terminated UTF-16: text ends at the first NUL by Win32
        // convention (GlobalSize may over-report, so scan for it).
        let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
        let out = String::from_utf16_lossy(&units[..end]);
        // SAFETY: balances the GlobalLock above; `units` is not used after.
        let _ = unsafe { GlobalUnlock(h) };
        Some(out)
    }

    /// Write UTF-8 `text` to the clipboard (empty text clears it). Returns
    /// whether the clipboard was updated.
    pub(crate) fn clip_write_text(text: &str) -> bool {
        // UTF-16 with a NUL terminator.
        let mut units: Vec<u16> = text.encode_utf16().collect();
        units.push(0);

        // SAFETY: no owner window; balanced by the guard below.
        if unsafe { OpenClipboard(None) }.is_err() {
            return false;
        }
        let _guard = CloseOnDrop;
        // SAFETY: the clipboard is open (guarded above).
        if unsafe { EmptyClipboard() }.is_err() {
            return false;
        }
        // SAFETY: a plain movable allocation of the buffer's byte size.
        let Ok(h) = (unsafe { GlobalAlloc(GMEM_MOVEABLE, units.len() * 2) }) else {
            return false;
        };
        // SAFETY: `h` is the live allocation just made.
        let ptr = unsafe { GlobalLock(h) } as *mut u16;
        if ptr.is_null() {
            return false;
        }
        // SAFETY: the locked object holds `units.len() * 2` bytes and does
        // not overlap `units`; the unlock balances the lock.
        unsafe {
            std::ptr::copy_nonoverlapping(units.as_ptr(), ptr, units.len());
            let _ = GlobalUnlock(h);
        }
        // SAFETY: the clipboard is open and emptied; `h` is an unlocked
        // movable global holding NUL-terminated UTF-16. On success the
        // system owns `h`; do not free it.
        unsafe { SetClipboardData(UNICODE_TEXT, Some(HANDLE(h.0))) }.is_ok()
    }

    /// `CloseClipboard` on scope exit so an early return can never leave the
    /// clipboard locked (the lock is system-global).
    struct CloseOnDrop;

    impl Drop for CloseOnDrop {
        fn drop(&mut self) {
            // SAFETY: only constructed right after a successful
            // OpenClipboard on this thread.
            let _ = unsafe { CloseClipboard() };
        }
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
