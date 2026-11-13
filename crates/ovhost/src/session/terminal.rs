//! The terminal-mode serve loop (no screen).

use std::path::Path;
use std::time::{Duration, Instant};

use ovapp::liveness::{Keepalive, KEEPALIVE_INTERVAL};
use ovapp::terminal::{Command, TerminalSession};
use tracing::debug;

use super::direct::log_direct;
use super::files::drain_received_files;
use super::hooks::HostHooks;
use super::liveness::{absorb_pongs, check_peer, session_over, PeerState};
use super::{clipboard_poll, CLIPBOARD_POLL, UNWATCHED_WAIT};
use crate::clipboard_os;

/// Longest idle wait in terminal mode (keepalive/clipboard housekeeping).
const TERMINAL_IDLE_WAIT: Duration = Duration::from_millis(50);

/// The terminal-mode serve loop: chat echo, file transfers, clipboard sync,
/// keepalive + dead-peer detection, and — with `--audio` (full duplex) —
/// the viewer's mic played on the host speakers. Blocks on the socket
/// between events instead of polling on a sleep.
pub(super) fn serve_terminal(
    mut term: TerminalSession,
    clipboard: bool,
    audio_playback: bool,
    data_dir: &Path,
) -> Result<(), String> {
    let mut hooks = HostHooks::new();
    if audio_playback {
        hooks.enable_speaker();
        debug!("speaker playback on (viewer's mic, 20 ms ticks)");
    }
    if clipboard {
        hooks.enable_clipboard();
    }
    hooks.register(&mut term);
    let wake = ovapp::netwait::Wakeup::with_sockets(term.raw_socket(), &term.extra_sockets())
        .map_err(|e| format!("wait setup failed: {e}"))?;
    let mut keepalive = Keepalive::new(KEEPALIVE_INTERVAL, Instant::now());
    let mut peer = PeerState::Alive;
    let mut clip_seq = clipboard_os::clip_sequence();
    let mut next_clip = Instant::now();
    loop {
        if let Err(e) = term.pump() {
            log_direct(&mut term);
            session_over(&e.to_string());
            return Ok(());
        }
        log_direct(&mut term);
        peer = check_peer(&hooks, peer);
        if peer == PeerState::Gone {
            let _ = term.close();
            return Ok(());
        }
        if let Some(seq) = keepalive.tick(Instant::now()) {
            if term.send(&Command::Ping(seq)).is_err() {
                session_over("keepalive send failed");
                return Ok(());
            }
        }
        absorb_pongs(&hooks, &mut keepalive);
        if Instant::now() >= next_clip {
            next_clip = Instant::now() + CLIPBOARD_POLL;
            if let Some(text) = clipboard_poll(&hooks.clipboard, &mut clip_seq) {
                let _ = term.send(&ovapp::clipboard::clipboard_command(&text));
            }
        }
        // Echo any chat lines received this cycle back to the peer.
        for payload in hooks.drain() {
            if term
                .send(&Command::Type {
                    key: 0,
                    mods: 0,
                    payload,
                })
                .is_err()
            {
                break;
            }
        }
        drain_received_files(&mut term, data_dir);
        let mut wait = if wake.watches_socket() {
            TERMINAL_IDLE_WAIT
        } else {
            UNWATCHED_WAIT
        };
        if let Some(d) = term.transport_deadline() {
            wait = wait.min(d.max(Duration::from_millis(1)));
        }
        wake.wait(wait);
    }
}
