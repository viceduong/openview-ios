//! Runtime environment: normal vs isolated test mode, data dir, ports, and
//! where the helper binaries live.
//!
//! **Data** follows the shared rule of [`ovcommon::paths`]:
//! `%LOCALAPPDATA%\OpenView` (settings, connections, logs, the host's
//! identity), `<exe dir>\data` in portable mode (`portable.txt` next to
//! `OpenView.exe`). The GUI passes its data dir to `ovhost` (`--data-dir`),
//! so both always agree. Releases before 0.2 kept the GUI settings in
//! `%APPDATA%\OpenView`; they are copied over once on first start.
//!
//! **Binaries**: `ovhost.exe`, `ovrelay.exe` and `ovview.exe` must sit next
//! to `OpenView.exe` (`OPENVIEW_BIN_DIR` overrides this for development).
//!
//! **Test mode** (`OPENVIEW_DEV_PORTS=<port>`, a development feature) never
//! touches the real OpenView install: the relay binds `127.0.0.1:<port>`,
//! the host runs with `--dry-input --fx-dry-run --no-upnp --no-direct`, data
//! lives in `%LOCALAPPDATA%\OpenView-dev` (or `OPENVIEW_DATA_DIR`),
//! credentials, the Run key and the single-instance lock use a `-dev`
//! namespace, and no process started outside this instance is ever adopted
//! or stopped.

use std::path::{Path, PathBuf};

use ovcommon::paths;

/// Port of the relay OpenView runs on this PC (TCP, all interfaces: remote
/// viewers dial it; allow it in the firewall).
pub const PROD_RELAY_PORT: u16 = 4569;

/// The helper binaries OpenView.exe supervises / launches.
pub const HOST_EXE: &str = "ovhost.exe";
pub const RELAY_EXE: &str = "ovrelay.exe";
pub const VIEWER_EXE: &str = "ovview.exe";

#[derive(Debug, Clone)]
pub struct Env {
    /// Isolated test mode (see module docs).
    pub dev: bool,
    /// Where the bundled relay listens (`0.0.0.0:4569`).
    pub relay_bind: String,
    /// How our host reaches the bundled relay (`127.0.0.1:4569`).
    pub local_relay: String,
    /// Test mode only: the default relay for outgoing connections (the test
    /// relay). Normal mode has no default — the user enters one.
    pub dev_outgoing_relay: Option<String>,
    /// The data root (settings, connections, logs, host identity).
    pub data_dir: PathBuf,
    /// Namespace for Credential Manager targets ("OpenView").
    pub cred_prefix: &'static str,
    /// Registry key whose values start apps at sign-in.
    pub run_key: &'static str,
    /// Folder holding ovhost/ovrelay/ovview.
    pub bin_dir: PathBuf,
    /// Started by the Run key (`--minimized`): start hidden in the tray.
    pub minimized: bool,
}

impl Env {
    pub fn detect() -> Self {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let minimized = args.iter().any(|a| a == "--minimized");
        let dev_port = std::env::var("OPENVIEW_DEV_PORTS")
            .ok()
            .and_then(|v| v.trim().parse::<u16>().ok());
        let data_override = std::env::var_os("OPENVIEW_DATA_DIR").map(PathBuf::from);
        let bin_dir = std::env::var_os("OPENVIEW_BIN_DIR")
            .map(PathBuf::from)
            .or_else(paths::exe_dir)
            .unwrap_or_else(|| PathBuf::from("."));
        match dev_port {
            Some(port) => Self {
                dev: true,
                relay_bind: format!("127.0.0.1:{port}"),
                local_relay: format!("127.0.0.1:{port}"),
                dev_outgoing_relay: Some(format!("127.0.0.1:{port}")),
                data_dir: data_override
                    .unwrap_or_else(|| paths::local_app_data().join("OpenView-dev")),
                cred_prefix: "OpenView-dev",
                run_key: r"Software\OpenView-dev\Run",
                bin_dir,
                minimized,
            },
            None => Self {
                dev: false,
                relay_bind: format!("0.0.0.0:{PROD_RELAY_PORT}"),
                local_relay: format!("127.0.0.1:{PROD_RELAY_PORT}"),
                dev_outgoing_relay: None,
                data_dir: paths::data_dir(data_override.as_deref()),
                cred_prefix: "OpenView",
                run_key: r"Software\Microsoft\Windows\CurrentVersion\Run",
                bin_dir,
                minimized,
            },
        }
    }

    pub fn logs_dir(&self) -> PathBuf {
        paths::logs_dir(&self.data_dir)
    }

    /// Full path of one helper binary.
    pub fn exe(&self, name: &str) -> PathBuf {
        self.bin_dir.join(name)
    }

    /// A user-facing error when a helper binary is missing, else `None`.
    pub fn missing_binary(&self, name: &str) -> Option<String> {
        (!self.exe(name).is_file()).then(|| {
            format!(
                "{name} is missing. It must be in the same folder as OpenView.exe ({}). \
                 Reinstall OpenView.",
                self.bin_dir.display()
            )
        })
    }

    /// One-time copy of `settings.json` / `connections.json` from the
    /// roaming `%APPDATA%\OpenView` older releases used (normal mode only,
    /// never over existing files).
    pub fn migrate_legacy_data(&self) {
        if self.dev || paths::is_portable() {
            return;
        }
        let Some(old_dir) = paths::roaming_app_data().map(|d| d.join("OpenView")) else {
            return;
        };
        if old_dir == self.data_dir {
            return;
        }
        for name in ["settings.json", "connections.json"] {
            migrate(&old_dir.join(name), &self.data_dir.join(name));
        }
    }
}

fn migrate(old: &Path, new: &Path) {
    match paths::migrate_file(old, new) {
        Ok(true) => crate::log::line(&format!("migrated {} -> {}", old.display(), new.display())),
        Ok(false) => {}
        Err(e) => crate::log::line(&format!("could not migrate {}: {e}", old.display())),
    }
}
