//! Shared error type for `ovdiscovery`.

/// Errors surfaced by the discovery layer. `thiserror`, matching the
/// workspace style.
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    /// A DNS name was malformed (empty label, too long, bad pointer).
    #[error("bad DNS name: {0}")]
    BadName(String),
    /// A wire-format buffer ended mid-record.
    #[error("truncated DNS packet")]
    Truncated,
    /// Socket I/O failed (bind, send, recv).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// An advertisement could not be started.
    #[error("advertiser failed: {0}")]
    Advertise(String),
}
