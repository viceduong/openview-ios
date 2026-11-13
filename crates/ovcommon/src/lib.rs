//! Shared plumbing of the OpenView Windows binaries (`ovhost`, `ovview`,
//! `OpenView.exe`):
//!
//! - [`paths`]: where data, logs and received files live (one rule for every
//!   binary, including the portable mode);
//! - [`relay_addr`]: the relay address syntax (`host[:port]`, IPs,
//!   `ws://` / `wss://` URLs) shared by every `--relay` input;
//! - [`status`]: the machine-readable status events `ovhost` / `ovview`
//!   print as JSON lines on stdout, which the GUI parses;
//! - `logging` (feature `logging`): the `tracing` subscriber of the CLI
//!   binaries (stderr, `-v` / `--log-level` / `OPENVIEW_LOG`).
//!
//! The crate is deliberately tiny and free of the networking stack so the GUI
//! can depend on it without pulling in codecs or crypto.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(clippy::undocumented_unsafe_blocks)]
#![warn(missing_docs)]

#[cfg(feature = "logging")]
pub mod logging;
pub mod paths;
pub mod relay_addr;
pub mod status;
