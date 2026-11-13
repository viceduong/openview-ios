//! Helpers that turn raw C arguments into Rust values and write results back
//! into caller-provided buffers. Each one states its pointer contract once,
//! so entry points only have to forward their own `# Safety` guarantees.

use std::ffi::{c_char, CStr};

/// Borrow a NUL-terminated UTF-8 C string; `None` for null or non-UTF-8.
///
/// # Safety
/// `p` must be null or point to a NUL-terminated string that stays valid
/// and unmodified for `'a`.
pub(crate) unsafe fn cstr<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        return None;
    }
    // SAFETY: `p` is non-null and, per the caller, NUL-terminated and live
    // for `'a`.
    unsafe { CStr::from_ptr(p) }.to_str().ok()
}

/// Borrow `len` bytes at `data`. A zero length yields an empty slice
/// whatever `data` is; a null `data` with `len > 0` yields `None`.
///
/// # Safety
/// When `len > 0` and `data` is non-null, `data` must be readable for `len`
/// bytes that stay valid and unmodified for `'a`.
pub(crate) unsafe fn bytes<'a>(data: *const u8, len: usize) -> Option<&'a [u8]> {
    if len == 0 {
        return Some(&[]);
    }
    if data.is_null() {
        return None;
    }
    // SAFETY: `data` is non-null and, per the caller, readable for `len`
    // bytes live for `'a`.
    Some(unsafe { std::slice::from_raw_parts(data, len) })
}

/// Copy `src` to `out`.
///
/// # Safety
/// `out` must be writable for `src.len()` elements and must not overlap
/// `src`.
pub(crate) unsafe fn write<T: Copy>(src: &[T], out: *mut T) {
    // SAFETY: `src` is a valid slice; the caller guarantees `out` is
    // writable for `src.len()` elements and disjoint from it.
    unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), out, src.len()) };
}

/// Copy `s` into `out` NUL-terminated; returns its length, or -1 if `cap`
/// is too small.
///
/// # Safety
/// `out` must be writable for `cap` bytes.
pub(crate) unsafe fn copy_str(s: &str, out: *mut c_char, cap: usize) -> i32 {
    let bytes = s.as_bytes();
    if bytes.len() + 1 > cap {
        return -1;
    }
    // SAFETY: `bytes.len() + 1 <= cap`, so the string and its NUL fit in
    // the `cap` writable bytes the caller guarantees.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr().cast::<c_char>(), out, bytes.len());
        *out.add(bytes.len()) = 0;
    }
    bytes.len() as i32
}
