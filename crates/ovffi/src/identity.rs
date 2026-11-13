//! `ov_identity_*`: a standalone device identity (key pair, fingerprint,
//! pairing code) for shells that show it before any session exists.

use std::ffi::c_char;

use ovcore::identity::Identity;

use crate::cabi::{copy_str, write};
use crate::error::OV_ERR_PANIC;

/// Create a fresh random identity. Returns null on failure.
#[no_mangle]
pub extern "C" fn ov_identity_create() -> *mut Identity {
    ffi_guard!(core::ptr::null_mut(), {
        Box::into_raw(Box::new(Identity::generate()))
    })
}

/// Free an identity previously returned by [`ov_identity_create`].
///
/// # Safety
/// `id` must be null or a pointer from [`ov_identity_create`] that was not
/// freed yet; it must not be used afterwards.
#[no_mangle]
pub unsafe extern "C" fn ov_identity_free(id: *mut Identity) {
    ffi_guard!((), {
        if !id.is_null() {
            // SAFETY: `id` came from `Box::into_raw` in `ov_identity_create`
            // and is freed only once (caller contract).
            drop(unsafe { Box::from_raw(id) });
        }
    })
}

/// Write the 16-byte fingerprint into `out`. 0 on success.
///
/// # Safety
/// `id` must be null or a live identity from [`ov_identity_create`]; `out`
/// must be null or writable for 16 bytes.
#[no_mangle]
pub unsafe extern "C" fn ov_identity_fingerprint(id: *const Identity, out: *mut u8) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        if out.is_null() {
            return -1;
        }
        // SAFETY: `id` is null or a live identity (caller contract).
        let Some(id) = (unsafe { id.as_ref() }) else {
            return -1;
        };
        // SAFETY: `out` is non-null and writable for 16 bytes (caller
        // contract).
        unsafe { write(&id.fingerprint(), out) };
        0
    })
}

/// Write the human-shareable fingerprint ("A1B2C3D4 E5F60718 ...", NUL
/// terminated) into `out`. `cap` includes the NUL. Returns the string length
/// (excluding NUL), or -1 if the buffer is too small.
///
/// # Safety
/// `id` must be null or a live identity from [`ov_identity_create`]; `out`
/// must be null or writable for `cap` bytes.
#[no_mangle]
pub unsafe extern "C" fn ov_identity_fingerprint_pine(
    id: *const Identity,
    out: *mut c_char,
    cap: usize,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        if out.is_null() || cap == 0 {
            return -1;
        }
        // SAFETY: `id` is null or a live identity (caller contract).
        let Some(id) = (unsafe { id.as_ref() }) else {
            return -1;
        };
        // SAFETY: `out` is non-null and writable for `cap` bytes (caller
        // contract).
        unsafe { copy_str(&id.fingerprint_pine(), out, cap) }
    })
}

/// Write the identity's 6-digit **fingerprint short code** (NUL terminated)
/// into `out`. Same return convention as [`ov_identity_fingerprint_pine`].
///
/// Public (derived from the public key) and not a credential: attended
/// sessions authenticate with a random one-time code from the host instead.
///
/// # Safety
/// `id` must be null or a live identity from [`ov_identity_create`]; `out`
/// must be null or writable for `cap` bytes.
#[no_mangle]
pub unsafe extern "C" fn ov_identity_pairing_code(
    id: *const Identity,
    out: *mut c_char,
    cap: usize,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        if out.is_null() || cap == 0 {
            return -1;
        }
        // SAFETY: `id` is null or a live identity (caller contract).
        let Some(id) = (unsafe { id.as_ref() }) else {
            return -1;
        };
        // SAFETY: `out` is non-null and writable for `cap` bytes (caller
        // contract).
        unsafe { copy_str(&id.pairing_code(), out, cap) }
    })
}
