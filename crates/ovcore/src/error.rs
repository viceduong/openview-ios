//! Typed errors of the `ovcore` public API.
//!
//! Two enums, one per layer:
//!
//! - [`HandshakeError`] — everything [`crate::handshake::Handshake`] can
//!   fail with. Each variant maps to a stable [`RejectCode`] so the reason a
//!   server refused a client (wrong PIN, too many attempts, incompatible
//!   version) survives the wire and reaches the client UI.
//! - [`CoreError`] — the record layer ([`crate::session::Session`]).
//!
//! Both convert into `String` (`impl From<_> for String`) so callers that
//! still thread `Result<_, String>` can use `?` unchanged.

use std::fmt;

/// Why a peer refused the handshake. Sent in a REJECT message
/// (`docs/WIRE_PROTOCOL.md` §3.5); the numeric values are wire-stable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum RejectCode {
    /// The peer speaks a different handshake version.
    IncompatibleVersion = 1,
    /// Wrong PIN / pairing code, or key confirmation failed.
    AuthFailed = 2,
    /// Too many failed PIN attempts: the host is locked out for a while.
    RateLimited = 3,
    /// The two sides were configured for different auth modes.
    AuthModeMismatch = 4,
    /// The peer could not parse one of our messages.
    Malformed = 5,
}

impl RejectCode {
    /// Decode a wire byte. Unknown codes map to `None` (a newer peer).
    #[must_use]
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => Self::IncompatibleVersion,
            2 => Self::AuthFailed,
            3 => Self::RateLimited,
            4 => Self::AuthModeMismatch,
            5 => Self::Malformed,
            _ => return None,
        })
    }
}

impl fmt::Display for RejectCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::IncompatibleVersion => "incompatible version",
            Self::AuthFailed => "authentication failed",
            Self::RateLimited => "too many attempts",
            Self::AuthModeMismatch => "auth mode mismatch",
            Self::Malformed => "malformed message",
        })
    }
}

/// A handshake failure.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HandshakeError {
    /// The peer runs an incompatible handshake version (e.g. a pre-PAKE
    /// v1 client talking to a v2 host). Both sides must be updated.
    #[error("incompatible handshake version: {0}")]
    IncompatibleVersion(String),
    /// Wrong PIN or pairing code, or a key-confirmation MAC did not verify.
    #[error("authentication failed: {0}")]
    AuthFailed(String),
    /// The host refuses PIN attempts for `retry_after_secs` more seconds
    /// (online brute-force protection). Show "too many attempts".
    #[error("too many failed PIN attempts; retry in {retry_after_secs} s")]
    RateLimited {
        /// Seconds until the host accepts another attempt (rounded up).
        retry_after_secs: u32,
    },
    /// The two sides were configured for different auth modes.
    #[error("auth mode mismatch: {0}")]
    AuthModeMismatch(String),
    /// The peer is not the one the caller pinned (fingerprint / device ID).
    #[error("peer identity mismatch: {0}")]
    PeerMismatch(String),
    /// A signature over the peer's keys or transcript did not verify.
    #[error("invalid signature: {0}")]
    BadSignature(String),
    /// A message could not be parsed (truncated, wrong sizes, trailing bytes).
    #[error("malformed handshake message: {0}")]
    Malformed(String),
    /// A well-formed message arrived in the wrong state.
    #[error("unexpected handshake message: {0}")]
    Unexpected(String),
    /// The peer sent a REJECT with a code this build does not know.
    #[error("peer rejected the handshake (code {0})")]
    Rejected(u8),
}

impl HandshakeError {
    /// The REJECT code a server sends for this failure, if any.
    #[must_use]
    pub fn reject_code(&self) -> Option<RejectCode> {
        Some(match self {
            Self::IncompatibleVersion(_) => RejectCode::IncompatibleVersion,
            Self::AuthFailed(_) => RejectCode::AuthFailed,
            Self::RateLimited { .. } => RejectCode::RateLimited,
            Self::AuthModeMismatch(_) => RejectCode::AuthModeMismatch,
            Self::Malformed(_) => RejectCode::Malformed,
            _ => return None,
        })
    }

    /// `true` for [`HandshakeError::RateLimited`].
    #[must_use]
    pub fn is_rate_limited(&self) -> bool {
        matches!(self, Self::RateLimited { .. })
    }

    /// `true` when the credentials (PIN / code) were wrong — the case where
    /// a client may discard a stored PIN. Network/protocol failures and rate
    /// limiting are *not* credential failures.
    #[must_use]
    pub fn is_auth_failure(&self) -> bool {
        matches!(self, Self::AuthFailed(_))
    }
}

impl From<HandshakeError> for String {
    fn from(e: HandshakeError) -> Self {
        e.to_string()
    }
}

/// A record-layer failure.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CoreError {
    /// Shorter than header + tag.
    #[error("record too short ({0} bytes)")]
    RecordTooShort(usize),
    /// The header names a sender other than our peer.
    #[error("invalid sender ID {0}")]
    InvalidSender(u64),
    /// The header sequence is not the one we expect (loss / replay / reorder).
    #[error("sequence desync: expected {expected} got {got}")]
    SequenceDesync {
        /// Low 32 bits of our receive counter.
        expected: u32,
        /// The header's sequence field.
        got: u32,
    },
    /// AEAD authentication failed (wrong key, channel, or tampering).
    #[error("AEAD open failed (auth)")]
    Aead,
}

impl From<CoreError> for String {
    fn from(e: CoreError) -> Self {
        e.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reject_codes_roundtrip() {
        for c in [
            RejectCode::IncompatibleVersion,
            RejectCode::AuthFailed,
            RejectCode::RateLimited,
            RejectCode::AuthModeMismatch,
            RejectCode::Malformed,
        ] {
            assert_eq!(RejectCode::from_u8(c as u8), Some(c));
        }
        assert_eq!(RejectCode::from_u8(0), None);
        assert_eq!(RejectCode::from_u8(200), None);
    }

    #[test]
    fn errors_convert_to_string() {
        let s: String = HandshakeError::RateLimited {
            retry_after_secs: 30,
        }
        .into();
        assert!(s.contains("too many"), "{s}");
        let s: String = CoreError::Aead.into();
        assert!(s.contains("AEAD"));
    }
}
