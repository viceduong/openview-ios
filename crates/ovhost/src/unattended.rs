//! Unattended mode's stable identity: the device ID viewers dial must not
//! change between runs, so the host's identity is persisted in the data
//! directory and reused.

use std::path::Path;

use ovcore::identity::Identity;
use tracing::{info, warn};

/// File name of the persisted unattended identity.
const IDENTITY_FILE: &str = "ovhost-unattended.id";

/// Load the persisted identity from `<data dir>\ovhost-unattended.id`, or
/// generate and save a new one. A file left next to the exe by older
/// releases is copied over once so existing device IDs survive the upgrade.
pub(crate) fn load_or_create_identity(data_dir: &Path) -> Result<Identity, String> {
    let path = data_dir.join(IDENTITY_FILE);
    if let Some(old) = ovcommon::paths::exe_dir().map(|d| d.join(IDENTITY_FILE)) {
        match ovcommon::paths::migrate_file(&old, &path) {
            Ok(true) => info!(
                "migrated the device identity from {} to {}",
                old.display(),
                path.display()
            ),
            Ok(false) => {}
            Err(e) => warn!("could not migrate {}: {e}", old.display()),
        }
    }
    match Identity::load(&path) {
        Ok(id) => Ok(id),
        Err(_) => {
            let id = Identity::generate();
            id.save(&path)
                .map_err(|e| format!("cannot save unattended identity: {e}"))?;
            Ok(id)
        }
    }
}

/// The device id of an identity: the fingerprint's first five bytes, as a
/// big-endian integer (the same recipe on the pairing-code path).
pub(crate) fn device_id(me: &Identity) -> u64 {
    let fp = me.fingerprint();
    u64::from_be_bytes([0, 0, 0, fp[0], fp[1], fp[2], fp[3], fp[4]])
}
