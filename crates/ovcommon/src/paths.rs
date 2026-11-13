//! Where OpenView keeps its files — one rule for every binary.
//!
//! | What | Location |
//! |------|----------|
//! | Data root (identity, settings, logs) | `%LOCALAPPDATA%\OpenView` |
//! | Portable mode (`portable.txt` next to the exe) | `<exe dir>\data` |
//! | Explicit override (`--data-dir <path>`) | `<path>` |
//! | Files received from a peer | `Downloads\OpenView` (the user's Downloads known folder) |
//!
//! The data root holds `ovhost-unattended.id` (the host's stable identity =
//! its device ID), `settings.json` / `connections.json` (GUI), and `logs\`.
//! Everything is per machine and per user, so it lives under *Local*
//! AppData, not the roaming profile. PINs are never stored here (the GUI
//! keeps them in Windows Credential Manager).

use std::io;
use std::path::{Path, PathBuf};

/// Folder name under `%LOCALAPPDATA%` (and under Downloads).
pub const APP_DIR_NAME: &str = "OpenView";
/// A file with this name next to the executable switches to portable mode.
pub const PORTABLE_MARKER: &str = "portable.txt";
/// Data folder (relative to the exe dir) in portable mode.
pub const PORTABLE_DATA_DIR: &str = "data";

/// The directory of the running executable.
pub fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
}

/// `%LOCALAPPDATA%` (falls back to the temp dir when the variable is unset).
pub fn local_app_data() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(std::env::temp_dir)
}

/// `%APPDATA%` (roaming) — only used to migrate files older releases kept
/// there.
pub fn roaming_app_data() -> Option<PathBuf> {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

/// True when `portable.txt` sits next to the running executable.
pub fn is_portable() -> bool {
    exe_dir().is_some_and(|d| d.join(PORTABLE_MARKER).is_file())
}

/// The data root for this process (see the module docs), not yet created.
pub fn data_dir(override_dir: Option<&Path>) -> PathBuf {
    resolve_data_dir(
        override_dir,
        exe_dir().as_deref(),
        &local_app_data(),
        APP_DIR_NAME,
    )
}

/// Pure resolution rule behind [`data_dir`]: the override wins, then
/// portable mode (`<exe dir>\portable.txt` exists), then
/// `<local app data>\<app_dir>`.
pub fn resolve_data_dir(
    override_dir: Option<&Path>,
    exe_dir: Option<&Path>,
    local_app_data: &Path,
    app_dir: &str,
) -> PathBuf {
    if let Some(d) = override_dir {
        return d.to_path_buf();
    }
    if let Some(e) = exe_dir.filter(|e| e.join(PORTABLE_MARKER).is_file()) {
        return e.join(PORTABLE_DATA_DIR);
    }
    local_app_data.join(app_dir)
}

/// [`data_dir`], created if missing.
pub fn ensure_data_dir(override_dir: Option<&Path>) -> io::Result<PathBuf> {
    let d = data_dir(override_dir);
    std::fs::create_dir_all(&d)?;
    Ok(d)
}

/// `<data>\logs`.
pub fn logs_dir(data: &Path) -> PathBuf {
    data.join("logs")
}

/// Where files received from a peer are saved: `Downloads\OpenView` (the
/// Downloads known folder, which the user may have relocated), else
/// `%USERPROFILE%\Downloads\OpenView`, else the temp dir. Not created.
pub fn received_files_dir() -> PathBuf {
    downloads_folder()
        .or_else(|| std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join("Downloads")))
        .unwrap_or_else(std::env::temp_dir)
        .join(APP_DIR_NAME)
}

/// The user's Downloads known folder (`FOLDERID_Downloads`).
#[cfg(windows)]
fn downloads_folder() -> Option<PathBuf> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{FOLDERID_Downloads, SHGetKnownFolderPath, KF_FLAG_DEFAULT};
    // SAFETY: on success the shell returns a CoTaskMem-allocated,
    // NUL-terminated wide string, which we copy and free exactly once.
    unsafe {
        let p = SHGetKnownFolderPath(&FOLDERID_Downloads, KF_FLAG_DEFAULT, None).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s.filter(|s| !s.is_empty()).map(PathBuf::from)
    }
}

#[cfg(not(windows))]
fn downloads_folder() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Downloads"))
}

/// One-time migration: copy `old` to `new` when `new` does not exist yet
/// and `old` does. Returns `Ok(true)` if a copy happened.
pub fn migrate_file(old: &Path, new: &Path) -> io::Result<bool> {
    if new.exists() || !old.is_file() {
        return Ok(false);
    }
    if let Some(parent) = new.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(old, new)?;
    Ok(true)
}

/// `dir\name`, or `dir\name (2).ext`, `(3)`, … when the name is taken — a
/// received file never overwrites an existing one.
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, Some(e)),
        _ => (name, None),
    };
    for n in 2..10_000 {
        let candidate = match ext {
            Some(e) => format!("{stem} ({n}).{e}"),
            None => format!("{stem} ({n})"),
        };
        let p = dir.join(candidate);
        if !p.exists() {
            return p;
        }
    }
    first
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ovcommon-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn override_then_portable_then_local_app_data() {
        let exe = scratch("resolve");
        let local = Path::new(r"C:\Users\x\AppData\Local");
        let over = Path::new(r"D:\ov-data");
        assert_eq!(
            resolve_data_dir(Some(over), Some(&exe), local, APP_DIR_NAME),
            over
        );
        assert_eq!(
            resolve_data_dir(None, Some(&exe), local, APP_DIR_NAME),
            local.join("OpenView")
        );
        std::fs::write(exe.join(PORTABLE_MARKER), b"").unwrap();
        assert_eq!(
            resolve_data_dir(None, Some(&exe), local, APP_DIR_NAME),
            exe.join("data")
        );
        assert_eq!(
            resolve_data_dir(Some(over), Some(&exe), local, APP_DIR_NAME),
            over,
            "an explicit override beats portable mode"
        );
        let _ = std::fs::remove_dir_all(exe);
    }

    #[test]
    fn migration_copies_once_and_never_overwrites() {
        let d = scratch("migrate");
        let old = d.join("old.id");
        let new = d.join("new").join("new.id");
        assert!(!migrate_file(&old, &new).unwrap(), "nothing to migrate");
        std::fs::write(&old, b"v1").unwrap();
        assert!(migrate_file(&old, &new).unwrap());
        assert_eq!(std::fs::read(&new).unwrap(), b"v1");
        std::fs::write(&old, b"v2").unwrap();
        assert!(!migrate_file(&old, &new).unwrap(), "new file wins");
        assert_eq!(std::fs::read(&new).unwrap(), b"v1");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn unique_paths_do_not_overwrite() {
        let d = scratch("unique");
        assert_eq!(unique_path(&d, "a.txt"), d.join("a.txt"));
        std::fs::write(d.join("a.txt"), b"").unwrap();
        assert_eq!(unique_path(&d, "a.txt"), d.join("a (2).txt"));
        std::fs::write(d.join("README"), b"").unwrap();
        assert_eq!(unique_path(&d, "README"), d.join("README (2)"));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn received_files_go_to_an_openview_folder() {
        assert!(received_files_dir().ends_with(APP_DIR_NAME));
    }
}
