//! OpenView C FFI — the iOS-facing surface over `ovcore` + `ovtransport`.
//!
//! Design notes (mirrors the wiring used by `ovview`/`ovhost`):
//! * `OvSession` is a `Mutex<Inner>`; all entry points lock it. Sinks (C
//!   function pointers) are invoked on whatever thread calls
//!   [`ov_session_pump`], never from a Rust-internal thread, so Swift can
//!   marshal them safely. Never re-enter the same session synchronously
//!   from a sink — buffer and call [`ov_session_pump`] again instead.
//! * Transport is pluggable: a datagram transport is owned by the host
//!   (Swift `NWConnection`); outbound datagrams are delivered through the
//!   datagram sink, inbound ones are pushed with [`ov_session_on_datagram`].
//!   A relay transport is owned by Rust (blocking TCP) via
//!   [`ov_session_relay_connect`].
//! * No entry point unwinds across the FFI boundary: pointer arguments are
//!   null-checked, fallible ops return negative error codes, and every
//!   `extern "C"` body runs inside `ffi_guard!`, which turns a Rust panic
//!   into [`OV_ERR_PANIC`] (or a null / neutral return value).
//!
//! Error codes: 0 = success, -1 = null/invalid argument, -2 = wrong state,
//! -3 = authentication failed (wrong PIN / pairing code; the only code that
//! means "the credentials are wrong"), -4 = transport error, -5 = relay
//! closed (`ov_session_pump_relay` only; terminal), -6 = internal error (a
//! Rust panic was caught; the handle should be torn down), -7 = rate
//! limited (too many PIN attempts; see [`ov_session_retry_after_secs`]),
//! -8 = incompatible handshake version, -9 = any other handshake / protocol
//! failure (malformed message, bad signature, peer mismatch, ...).
//!
//! The C declarations live in `ios/OvFFI/include/shim.h`; the header test
//! fails when the header and the exported symbols drift apart.
//!
//! Module map:
//! * `error` — error codes, their mapping, and the panic guard.
//! * `cabi` — C argument / buffer helpers shared by every entry point.
//! * `callbacks` — session callback types and the `Sink` wrapper.
//! * `identity` — `ov_identity_*`.
//! * `session` — session lifecycle, sinks, handshake, records.
//! * `pump` — the session pump (handshake progress, record decryption).
//! * `transport` — the host datagram carrier and the transport enum.
//! * `relay` — TCP relay, host-owned stream relay, wake sink, relay pump.
//! * `direct` — the direct UDP path over shell-owned sockets.
//! * `media` — the direct path's media channel.
//! * `file_transfer` — `ov_file_send`.
//! * `screen` — the screen assembler (`ov_screen_*`): frames, tiles,
//!   video, audio, cursor, clipboard, files, liveness.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(clippy::undocumented_unsafe_blocks)]
#![warn(missing_docs)]

#[macro_use]
mod error;

mod cabi;
mod callbacks;
mod direct;
mod file_transfer;
mod identity;
mod media;
mod pump;
mod relay;
mod screen;
mod session;
mod transport;

pub use callbacks::*;
pub use direct::*;
pub use error::*;
pub use file_transfer::*;
pub use identity::*;
pub use relay::*;
pub use screen::*;
pub use session::*;

#[cfg(test)]
use crate::{relay::relay_session_id, transport::Transport};

#[cfg(test)]
mod tests;
