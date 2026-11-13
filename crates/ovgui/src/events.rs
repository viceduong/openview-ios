//! Worker -> UI hand-off. Workers (child readers, timers, the tray window
//! procedure) `post` plain data; the UI thread drains the queue in ONE
//! event-loop callback per burst. Waking via `invoke_from_event_loop` (not a
//! polling Timer) keeps this working while the window is hidden in the tray,
//! when the D3D11 loop is otherwise idle.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Mutex, OnceLock};

use crate::procs::Output;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCmd {
    Open,
    CopyId,
    TogglePause,
    Quit,
}

pub enum Event {
    Host {
        gen: u64,
        out: Output,
    },
    Relay {
        gen: u64,
        out: Output,
    },
    Viewer {
        seq: u64,
        out: Output,
    },
    HostStopped {
        gen: u64,
    },
    RetryDue {
        gen: u64,
    },
    RelayRetry {
        gen: u64,
    },
    ExternalGone,
    TakeOverDone {
        err: Option<String>,
    },
    Tray(TrayCmd),
    ThemeChanged,
    /// The window was shown from the tray: repaint everything.
    Shown,
    ToastExpired(u64),
}

static TX: OnceLock<Mutex<Sender<Event>>> = OnceLock::new();
static PENDING: AtomicBool = AtomicBool::new(false);

pub type Drain = fn();

static DRAIN: OnceLock<Drain> = OnceLock::new();

/// Install the channel; `drain` runs on the UI thread after each wake.
pub fn init(drain: Drain) -> Receiver<Event> {
    let (tx, rx) = std::sync::mpsc::channel();
    let _ = TX.set(Mutex::new(tx));
    let _ = DRAIN.set(drain);
    rx
}

/// Queue an event from any thread and wake the UI (once per burst).
pub fn post(ev: Event) {
    let Some(tx) = TX.get() else { return };
    if let Ok(tx) = tx.lock() {
        if tx.send(ev).is_err() {
            return;
        }
    }
    if !PENDING.swap(true, Ordering::AcqRel) {
        let _ = slint::invoke_from_event_loop(|| {
            PENDING.store(false, Ordering::Release);
            if let Some(d) = DRAIN.get() {
                d();
            }
        });
    }
}
