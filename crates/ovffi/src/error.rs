//! FFI error codes, their mapping from Rust errors, and the panic guard
//! every `extern "C"` entry point runs inside.

use ovtransport::TransportError;

/// Error code returned when an entry point caught a Rust panic. The handle
/// may be in an inconsistent state (a poisoned session mutex reports -2 from
/// then on); the shell should free it and start over.
pub const OV_ERR_PANIC: i32 = -6;

/// The peer rejected our credentials, or theirs failed key confirmation:
/// wrong PIN or pairing code. The only error after which a shell should
/// discard a stored PIN.
pub const OV_ERR_AUTH_FAILED: i32 = -3;
/// The host refuses PIN attempts for a while (online brute-force
/// protection); [`ov_session_retry_after_secs`](crate::ov_session_retry_after_secs)
/// tells for how long.
pub const OV_ERR_RATE_LIMITED: i32 = -7;
/// The peer speaks an incompatible handshake version; both apps need an
/// update.
pub const OV_ERR_INCOMPATIBLE_VERSION: i32 = -8;
/// Any other handshake failure (malformed or unexpected message, bad
/// signature, peer or auth-mode mismatch). Not a credential problem.
pub const OV_ERR_HANDSHAKE: i32 = -9;

/// The FFI code for a failed handshake.
pub(crate) fn handshake_error_code(e: &ovcore::HandshakeError) -> i32 {
    if e.is_auth_failure() {
        OV_ERR_AUTH_FAILED
    } else if e.is_rate_limited() {
        OV_ERR_RATE_LIMITED
    } else if matches!(e, ovcore::HandshakeError::IncompatibleVersion(_)) {
        OV_ERR_INCOMPATIBLE_VERSION
    } else {
        OV_ERR_HANDSHAKE
    }
}

/// Whether `code` reports a handshake outcome (as opposed to transport or
/// usage errors).
pub(crate) fn is_handshake_code(code: i32) -> bool {
    matches!(
        code,
        OV_ERR_AUTH_FAILED | OV_ERR_RATE_LIMITED | OV_ERR_INCOMPATIBLE_VERSION | OV_ERR_HANDSHAKE
    )
}

/// The FFI code for a transport failure: -2 once closed, -4 otherwise.
pub(crate) fn err_from_transport(e: &TransportError) -> i32 {
    match e {
        TransportError::Closed => -2,
        _ => -4,
    }
}

/// Run the body of an `extern "C"` entry point, catching any panic so it
/// never unwinds into the C / Swift caller (undefined behaviour). On a panic
/// the entry point returns `$on_panic` instead: [`OV_ERR_PANIC`] for status
/// codes, null for constructors, a neutral value for queries.
///
/// The body runs inside a closure, so `return` inside it returns from the
/// entry point as expected. Unwind safety is asserted: after a panic the
/// only state that matters is the session mutex, which poisoning already
/// guards.
macro_rules! ffi_guard {
    ($on_panic:expr, $body:block) => {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| $body)) {
            Ok(value) => value,
            Err(_) => $on_panic,
        }
    };
}
