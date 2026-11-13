//! Tiny synchronous file log (`logs\openview.log`) + a panic hook that writes
//! `logs\panics.log` with a forced backtrace (a GUI has no console).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

static LOG: OnceLock<Mutex<Option<std::fs::File>>> = OnceLock::new();
static DIR: OnceLock<PathBuf> = OnceLock::new();

/// Truncate a log that grew past this at startup.
const MAX_LOG: u64 = 4 * 1024 * 1024;

pub fn init(dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
    let _ = DIR.set(dir.to_path_buf());
    let path = dir.join("openview.log");
    let file = open_capped(&path);
    let _ = LOG.set(Mutex::new(file));
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let bt = std::backtrace::Backtrace::force_capture();
        let msg = format!("PANIC: {info}\n{bt}\n");
        line(&msg);
        if let Some(d) = DIR.get() {
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(d.join("panics.log"))
            {
                let _ = writeln!(f, "[{}] {msg}", crate::util::now_secs());
            }
        }
        prev(info);
    }));
}

/// Open `path` for append, truncating it first if it is over the cap.
pub fn open_capped(path: &Path) -> Option<std::fs::File> {
    let too_big = std::fs::metadata(path)
        .map(|m| m.len() > MAX_LOG)
        .unwrap_or(false);
    std::fs::OpenOptions::new()
        .create(true)
        .append(!too_big)
        .write(true)
        .truncate(too_big)
        .open(path)
        .ok()
}

pub fn line(msg: &str) {
    if let Some(m) = LOG.get() {
        if let Ok(mut g) = m.lock() {
            if let Some(f) = g.as_mut() {
                let _ = writeln!(f, "{} {msg}", crate::util::local_hhmm());
            }
        }
    }
    #[cfg(debug_assertions)]
    eprintln!("{msg}");
}
