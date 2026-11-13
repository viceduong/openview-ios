//! Per-session desktop effects (hidden wallpaper, animations off) and the
//! process-exit guarantee that they are restored.

use std::sync::{Arc, Mutex};

use tracing::{debug, warn};

use crate::{cleanup, desktop_fx};

/// Desktop effects configuration for screen sessions.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FxCfg {
    pub(crate) hide_wallpaper: bool,
    pub(crate) no_animations: bool,
    pub(crate) flat_color: Option<u32>,
    pub(crate) dry_run: bool,
}

impl FxCfg {
    /// No effects, and nothing applied for real (used when no configuration
    /// was stored).
    pub(crate) const NONE: Self = Self {
        hide_wallpaper: false,
        no_animations: false,
        flat_color: None,
        dry_run: true,
    };
}

/// The desktop-effects backend: the real Win32 one, or the logging dry run.
pub(crate) fn fx_backend(dry_run: bool) -> Box<dyn desktop_fx::FxBackend> {
    #[cfg(windows)]
    if !dry_run {
        return Box::new(desktop_fx::Win32Fx);
    }
    Box::new(desktop_fx::DryRunFx {
        verbose: dry_run,
        ..Default::default()
    })
}

/// One session's desktop effects: applied at start, restored on drop (and
/// at process exit through the cleanup registry if the drop never runs).
pub(crate) struct SessionFx {
    pub(crate) fx: Arc<Mutex<desktop_fx::DesktopFx>>,
    token: u64,
}

impl SessionFx {
    /// Apply `cfg` and register the restore-at-exit action.
    pub(crate) fn start(cfg: FxCfg) -> Self {
        let mut fx = desktop_fx::DesktopFx::new(
            fx_backend(cfg.dry_run),
            desktop_fx::default_marker(),
            cfg.flat_color,
        );
        if cfg.hide_wallpaper {
            match fx.set_wallpaper_hidden(true) {
                Ok(()) => debug!("wallpaper hidden for the session"),
                Err(e) => warn!("could not hide wallpaper ({e})"),
            }
        }
        if cfg.no_animations {
            match fx.set_animations_off(true) {
                Ok(()) => debug!("UI animations off for the session"),
                Err(e) => warn!("could not disable animations ({e})"),
            }
        }
        let fx = Arc::new(Mutex::new(fx));
        let at_exit = Arc::clone(&fx);
        let token = cleanup::register(move || {
            if let Ok(mut f) = at_exit.lock() {
                f.restore_all();
            }
        });
        Self { fx, token }
    }
}

impl Drop for SessionFx {
    fn drop(&mut self) {
        cleanup::unregister(self.token);
        if let Ok(mut f) = self.fx.lock() {
            let had = f.wallpaper_hidden() || f.animations_off();
            f.restore_all();
            if had {
                debug!("desktop effects restored");
            }
        }
    }
}
