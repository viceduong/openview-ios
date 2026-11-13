//! Shared plumbing of the OpenView Windows binaries (`ovhost`, `ovview`,
//! `OpenView.exe`):
//!
//! - [`paths`]: where data, logs and received files live (one rule for every
//!   binary, including the portable mode);
//! - [`status`]: the machine-readable status events `ovhost` / `ovview`
//!   print as JSON lines on stdout, which the GUI parses;
//! - [`logging`] (feature `logging`): the `tracing` subscriber of the CLI
//!   binaries (stderr, `-v` / `--log-level` / `OPENVIEW_LOG`).
//!
//! The crate is deliberately tiny and free of the networking stack so the GUI
//! can depend on it without pulling in codecs or crypto.

#[cfg(feature = "logging")]
pub mod logging;
pub mod paths;
pub mod status;
