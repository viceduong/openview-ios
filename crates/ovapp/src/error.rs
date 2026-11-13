//! The crate-wide error type.
//!
//! Every fallible public API in `ovapp` returns [`AppError`] (or the
//! [`Result`] alias). Each variant carries the same human-readable message
//! the API has always produced, so log lines stay unchanged; the variant adds
//! the *category* callers can branch on.
//!
//! Callers that still propagate plain `String` errors keep compiling: there is
//! a `From<AppError> for String` conversion, so `?` and `.map_err(String::from)`
//! work.

use std::fmt;

/// An `ovapp` failure, categorised by where it came from.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AppError {
    /// Untrusted wire or file input failed validation (truncated, oversized,
    /// unknown version/flags, inconsistent lengths, corrupt compressed data).
    #[error("{0}")]
    Malformed(String),
    /// A codec (Opus, H.264, JPEG, zstd encoder, ...) rejected its input or
    /// failed internally.
    #[error("{0}")]
    Codec(String),
    /// An OS / platform API failed (Win32, GDI, Media Foundation, D3D11,
    /// audio devices).
    #[error("{0}")]
    Platform(String),
    /// File-system I/O failed (recordings, received files).
    #[error("{0}")]
    Io(String),
    /// The caller asked for something the current state or configuration
    /// does not allow (bad geometry, unsupported mode, feature not built).
    #[error("{0}")]
    Invalid(String),
}

impl AppError {
    /// A [`AppError::Malformed`] from any displayable message.
    pub fn malformed(msg: impl fmt::Display) -> Self {
        Self::Malformed(msg.to_string())
    }

    /// A [`AppError::Codec`] from any displayable message.
    pub fn codec(msg: impl fmt::Display) -> Self {
        Self::Codec(msg.to_string())
    }

    /// A [`AppError::Platform`] from any displayable message.
    pub fn platform(msg: impl fmt::Display) -> Self {
        Self::Platform(msg.to_string())
    }

    /// A [`AppError::Io`] from any displayable message.
    pub fn io(msg: impl fmt::Display) -> Self {
        Self::Io(msg.to_string())
    }

    /// A [`AppError::Invalid`] from any displayable message.
    pub fn invalid(msg: impl fmt::Display) -> Self {
        Self::Invalid(msg.to_string())
    }

    /// The message without the category.
    pub fn message(&self) -> &str {
        match self {
            Self::Malformed(m)
            | Self::Codec(m)
            | Self::Platform(m)
            | Self::Io(m)
            | Self::Invalid(m) => m,
        }
    }
}

impl From<AppError> for String {
    fn from(e: AppError) -> Self {
        e.to_string()
    }
}

/// `Result` with [`AppError`] as the error type.
pub type Result<T, E = AppError> = std::result::Result<T, E>;
