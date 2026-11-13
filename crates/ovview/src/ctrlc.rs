//! Best-effort Ctrl-C handling: a console control handler that raises a
//! process-wide flag the viewer loop and playback poll.
//!
//! Windows only (`SetConsoleCtrlHandler`); elsewhere the flag never rises
//! and EOF on stdin ends the session instead.

use std::sync::atomic::{AtomicBool, Ordering};

/// Raised by the console control handler (Ctrl-C, Ctrl-Break, close).
static CTRL_C: AtomicBool = AtomicBool::new(false);

/// True once Ctrl-C was pressed (after [`install`]).
pub(crate) fn pressed() -> bool {
    CTRL_C.load(Ordering::Relaxed)
}

/// Install the console control handler once per process; later calls are
/// no-ops.
pub(crate) fn install() {
    #[cfg(windows)]
    {
        use windows::core::BOOL;
        use windows::Win32::System::Console::SetConsoleCtrlHandler;

        /// The console control handler: raise the flag and report the
        /// event as handled (the default handler would kill the process
        /// before the session can close cleanly).
        ///
        /// # Safety
        ///
        /// Only called by the system, on its own thread; it touches nothing
        /// but an atomic.
        unsafe extern "system" fn handler(_ctrl_type: u32) -> BOOL {
            CTRL_C.store(true, Ordering::Relaxed);
            BOOL(1)
        }

        static INSTALL: std::sync::Once = std::sync::Once::new();
        INSTALL.call_once(|| {
            // SAFETY: `handler` is a `'static` function with the
            // `PHANDLER_ROUTINE` signature and stays valid for the process
            // lifetime. Failure (no console) is ignored: Ctrl-C is best-effort.
            let _ = unsafe { SetConsoleCtrlHandler(Some(handler), true) };
        });
    }
}
