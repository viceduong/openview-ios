//! `ovview` — the OpenView desktop viewer: shows a remote host's screen in a
//! native window and sends it local mouse/keyboard input.
//!
//! Three ways to reach a host:
//!
//! - `ovview --connect <device-id>` (PIN in `OPENVIEW_PIN` or `--pin`):
//!   unattended access through the relay — what the GUI runs.
//! - `ovview --code <pairing-code>`: an attended host's one-time code.
//! - `ovview --id <device-id>` (PIN as for `--connect`): LAN access without
//!   the relay — mDNS lookup (or `--lan-addr IP:PORT`), then direct UDP to
//!   the host's LAN port.
//!
//! Offline recording tools (no network): `--info`, `--export`, `--play`.
//!
//! Output contract: **stdout** carries machine-readable status events, one
//! JSON object per line ([`ovcommon::status`]; the GUI parses these), except
//! for the offline `--info` / `--export` reports, which print plain text.
//! **stderr** carries human-readable logs (`tracing`): lifecycle lines at
//! the default `info` level, session metrics with `-v`.
//!
//! Received files are saved to `Downloads\OpenView`; recordings (F6) to
//! `%USERPROFILE%\ovrecord` unless `--record-dir` says otherwise.
//!
//! Module map:
//!
//! - [`cli`]: command-line options and their translation into a [`cli::Mode`];
//! - [`connect`]: the three dial paths and the client handshake;
//! - [`session`]: the post-handshake viewer loop (input, hotkeys, audio,
//!   presentation, end-of-session metrics);
//! - [`record`]: F6 recording plus the offline `--info` / `--export` /
//!   `--play` tools;
//! - [`window`]: the native render window (Direct2D or GDI presenter);
//! - [`localinput`], [`hud`]: the pure input-normalization and HUD-text
//!   helpers the window and the loop share;
//! - [`audio`], [`mic`], [`clipboard_os`], [`ctrlc`]: OS integration.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(clippy::undocumented_unsafe_blocks)]

use std::process::ExitCode;

use clap::Parser;
use ovcommon::status::{self, StatusEvent};
use tracing::error;

mod audio;
mod cli;
mod clipboard_os;
mod connect;
mod ctrlc;
mod hud;
mod localinput;
mod mic;
mod record;
mod session;
mod window;

fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    ovcommon::logging::init(cli.verbose, cli.log_level.as_deref());
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e}");
            status::emit(&StatusEvent::Error { message: e });
            ExitCode::FAILURE
        }
    }
}

/// Dispatch the parsed command line: an offline recording tool, or a live
/// session against a host.
fn run(cli: cli::Cli) -> Result<(), String> {
    match cli.into_mode()? {
        cli::Mode::Info(file) => record::info(&file),
        cli::Mode::Export { file, out, frame } => record::export(&file, &out, frame),
        cli::Mode::Play { file, loop_forever } => record::play(&file, loop_forever),
        cli::Mode::Connect {
            target,
            relay,
            options,
        } => connect::run(target, &relay, options),
    }
}
