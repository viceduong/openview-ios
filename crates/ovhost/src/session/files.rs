//! Inbound file transfers: saving received files and the iOS app's
//! diagnostic log uploads.

use std::path::{Path, PathBuf};

use ovapp::terminal::TerminalSession;
use tracing::{debug, info, warn};

/// File-name prefix of the iOS app's diagnostic log uploads.
const PHONE_LOG_PREFIX: &str = "ovlog-ios-";

/// Write completed inbound file transfers to disk / log them (both the
/// file-id-keyed path and the legacy FILE_BEGIN path). Received files land
/// in `Downloads\OpenView` (never overwriting an existing file); the iOS
/// app's diagnostic logs go to `<data dir>\logs\phone`.
pub(super) fn drain_received_files(term: &mut TerminalSession, data_dir: &Path) {
    while let Some(f) = term.take_xfer_file() {
        let name = ovapp::file_transfer::FileTransfer::sanitize_name(&f.name);
        // Diagnostics: the iOS app uploads its session logs from earlier
        // runs right after connecting. They go to the host's log folder,
        // not to the user's received files.
        if let Some(dir) = phone_log_dir(data_dir, &name) {
            let _ = std::fs::create_dir_all(&dir);
            let dest = dir.join(&name);
            match std::fs::write(&dest, &f.data) {
                Ok(()) => debug!(
                    "phone log saved: {} ({} bytes)",
                    dest.display(),
                    f.data.len()
                ),
                Err(e) => warn!("phone log `{name}` not saved: {e}"),
            }
            continue;
        }
        let dir = ovcommon::paths::received_files_dir();
        let saved = std::fs::create_dir_all(&dir).and_then(|()| {
            let dest = ovcommon::paths::unique_path(&dir, &name);
            std::fs::write(&dest, &f.data).map(|()| dest)
        });
        match saved {
            Ok(dest) => info!(
                "received `{}` -> {} ({} bytes)",
                f.name,
                dest.display(),
                f.data.len()
            ),
            Err(e) => warn!("received `{}` but could not save it: {e}", f.name),
        }
    }
    while let Some((name, data)) = term.take_file() {
        info!(
            "received file `{name}` ({} bytes, legacy transfer, not saved)",
            data.len()
        );
    }
}

/// Where an uploaded phone log goes: `<data dir>\logs\phone` (`None` = not
/// a phone log). Only plain `ovlog-ios-*.log` names qualify.
fn phone_log_dir(data_dir: &Path, name: &str) -> Option<PathBuf> {
    let ok = name.starts_with(PHONE_LOG_PREFIX)
        && name.ends_with(".log")
        && name.len() <= 160
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    ok.then(|| ovcommon::paths::logs_dir(data_dir).join("phone"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phone_logs_are_recognized_strictly() {
        let data = Path::new(r"C:\data");
        let d = phone_log_dir(data, "ovlog-ios-1a2b3c4d-20261113-101500.log").expect("a phone log");
        assert_eq!(d, data.join("logs").join("phone"));
        for bad in [
            "photo.jpg",
            "ovlog-ios-x.txt",
            "ovlog-ios-a b.log",
            "xovlog-ios-1.log",
            "ovlog-ios-..\\x.log",
        ] {
            assert!(phone_log_dir(data, bad).is_none(), "{bad}");
        }
    }
}
