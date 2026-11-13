//! `ovcore` — the OpenView protocol core: crypto, identity, framing, records,
//! clock sync, commands, the PAKE-authenticated handshake, and the session key
//! schedule. Shared byte-for-byte between the desktop app and (via a
//! staticlib + FFI) the iOS app.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod clocksync;
pub mod commands;
pub mod crypto;
pub mod error;
pub mod framing;
pub mod handshake;
pub mod identity;
pub mod pin;
pub mod record;
pub mod session;
pub mod throttle;

pub use error::{CoreError, HandshakeError, RejectCode};
