//! Process-exit cleanup (Section 4/5): actions that must run however the
//! host goes away — session end (normal drop), Ctrl-C / Ctrl-Break /
//! console close / logoff / shutdown (console control handler), and a
//! panic (panic hook). Each registered action runs at most once.
//!
//! Users: the desktop effects guard (restore wallpaper / animations) and
//! the UPnP port mapping (delete it).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

type Action = Box<dyn FnOnce() + Send>;

static ACTIONS: Mutex<Vec<(u64, Action)>> = Mutex::new(Vec::new());
static NEXT: AtomicU64 = AtomicU64::new(1);

/// Register `f`; returns a token for [`unregister`].
pub fn register(f: impl FnOnce() + Send + 'static) -> u64 {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut a) = ACTIONS.lock() {
        a.push((id, Box::new(f)));
    }
    id
}

/// Drop a registered action without running it (its owner cleaned up).
pub fn unregister(id: u64) {
    if let Ok(mut a) = ACTIONS.lock() {
        a.retain(|(i, _)| *i != id);
    }
}

/// Run (and remove) every registered action, newest first.
pub fn run_all() {
    let actions = match ACTIONS.lock() {
        Ok(mut a) => std::mem::take(&mut *a),
        // A panic while holding the lock: still try to recover the list.
        Err(p) => std::mem::take(&mut *p.into_inner()),
    };
    for (_, f) in actions.into_iter().rev() {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    }
}

/// Install the console control handler and the panic hook (idempotent
/// enough: call once from `main`).
pub fn install() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        run_all();
        prev(info);
    }));
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        extern "system" {
            fn SetConsoleCtrlHandler(
                handler: Option<unsafe extern "system" fn(u32) -> i32>,
                add: i32,
            ) -> i32;
        }
        unsafe extern "system" fn handler(_ctrl: u32) -> i32 {
            run_all();
            // Not handled: the default handler terminates the process.
            0
        }
        // SAFETY: registering a plain function pointer that lives for the
        // whole process.
        unsafe {
            SetConsoleCtrlHandler(Some(handler), 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    use std::sync::Arc;

    #[test]
    fn actions_run_once_and_unregister_skips() {
        let n = Arc::new(AtomicU32::new(0));
        let a = Arc::clone(&n);
        let b = Arc::clone(&n);
        let _ = register(move || {
            a.fetch_add(1, Ordering::SeqCst);
        });
        let skip = register(move || {
            b.fetch_add(100, Ordering::SeqCst);
        });
        unregister(skip);
        run_all();
        run_all();
        assert_eq!(n.load(Ordering::SeqCst), 1);
    }
}
