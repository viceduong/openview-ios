//! Persistent state: `settings.json` and `connections.json` under the data
//! dir. PINs never land here (Credential Manager holds them).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum FpsMode {
    #[default]
    Auto,
    Fps30,
    Fps60,
}

impl FpsMode {
    pub fn index(self) -> i32 {
        match self {
            Self::Auto => 0,
            Self::Fps30 => 1,
            Self::Fps60 => 2,
        }
    }
    pub fn from_index(i: i32) -> Self {
        match i {
            1 => Self::Fps30,
            2 => Self::Fps60,
            _ => Self::Auto,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub send_audio: bool,
    pub play_mic: bool,
    pub hide_wallpaper: bool,
    pub fps: FpsMode,
    pub direct: bool,
    pub upnp: bool,
    /// Relay for outgoing connections (and for this PC's host when
    /// `run_relay` is off), `host[:port]` or a `ws://` / `wss://` URL
    /// ([`ovcommon::relay_addr`]); empty = not configured yet.
    pub relay_address: String,
    /// Run the bundled relay on this PC and park this PC's host on it.
    pub run_relay: bool,
    /// Remote access paused by the user (host stopped).
    pub paused: bool,
    /// Last device id the host reported (shown while it starts).
    pub last_device_id: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            send_audio: true,
            play_mic: true,
            hide_wallpaper: true,
            fps: FpsMode::Auto,
            direct: true,
            upnp: true,
            relay_address: String::new(),
            run_relay: true,
            paused: false,
            last_device_id: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Connection {
    /// Device id, digits only.
    pub id: String,
    pub name: String,
    /// Relay used for this partner; empty = the settings default.
    pub relay: String,
    pub last_connected: u64,
    pub favorite: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Connections {
    pub items: Vec<Connection>,
}

impl Connections {
    /// Record a connection attempt (most recent first).
    pub fn touch(&mut self, id: &str, relay: &str, now: u64) {
        if let Some(pos) = self.items.iter().position(|c| c.id == id) {
            let mut c = self.items.remove(pos);
            c.last_connected = now;
            relay.clone_into(&mut c.relay);
            self.items.insert(0, c);
        } else {
            self.items.insert(
                0,
                Connection {
                    id: id.to_owned(),
                    relay: relay.to_owned(),
                    last_connected: now,
                    ..Default::default()
                },
            );
        }
        // Keep the list bounded; favorites always survive.
        let mut kept_recent = 0;
        self.items.retain(|c| {
            if c.favorite {
                return true;
            }
            kept_recent += 1;
            kept_recent <= 30
        });
    }

    pub fn get(&self, id: &str) -> Option<&Connection> {
        self.items.iter().find(|c| c.id == id)
    }

    pub fn toggle_favorite(&mut self, id: &str) {
        if let Some(c) = self.items.iter_mut().find(|c| c.id == id) {
            c.favorite = !c.favorite;
        }
    }

    pub fn remove(&mut self, id: &str) {
        self.items.retain(|c| c.id != id);
    }
}

fn load_json<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> T {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Write via a temp file + rename so a crash never leaves half a file.
fn save_json<T: Serialize>(path: &Path, v: &T) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_vec_pretty(v).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }
    fn settings_path(&self) -> PathBuf {
        self.dir.join("settings.json")
    }
    fn connections_path(&self) -> PathBuf {
        self.dir.join("connections.json")
    }
    pub fn load_settings(&self) -> Settings {
        load_json(&self.settings_path())
    }
    pub fn save_settings(&self, s: &Settings) {
        if let Err(e) = save_json(&self.settings_path(), s) {
            crate::log::line(&format!("save settings failed: {e}"));
        }
    }
    pub fn load_connections(&self) -> Connections {
        load_json(&self.connections_path())
    }
    pub fn save_connections(&self, c: &Connections) {
        if let Err(e) = save_json(&self.connections_path(), c) {
            crate::log::line(&format!("save connections failed: {e}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touch_orders_and_dedups() {
        let mut c = Connections::default();
        c.touch("1", "r", 10);
        c.touch("2", "r", 20);
        c.touch("1", "r2", 30);
        assert_eq!(c.items.len(), 2);
        assert_eq!(c.items[0].id, "1");
        assert_eq!(c.items[0].relay, "r2");
        assert_eq!(c.items[0].last_connected, 30);
    }

    #[test]
    fn caps_recent_but_keeps_favorites() {
        let mut c = Connections::default();
        c.touch("fav", "", 0);
        c.toggle_favorite("fav");
        for i in 0..40 {
            c.touch(&i.to_string(), "", i);
        }
        assert!(c.get("fav").is_some());
        assert_eq!(c.items.iter().filter(|x| !x.favorite).count(), 30);
    }

    #[test]
    fn settings_roundtrip_with_missing_fields() {
        let s: Settings = serde_json::from_str(r#"{"send_audio":false}"#).unwrap();
        assert!(!s.send_audio);
        assert!(s.play_mic);
        assert_eq!(s.fps, FpsMode::Auto);
        assert!(
            s.run_relay,
            "older settings files keep the bundled relay on"
        );
        let old: Settings =
            serde_json::from_str(r#"{"binaries_dir":"x","pin_user_set":true}"#).unwrap();
        assert_eq!(old, Settings::default(), "removed fields are ignored");
        let dir = std::env::temp_dir().join(format!("ovgui-test-{}", std::process::id()));
        let st = Store::new(dir.clone());
        st.save_settings(&s);
        assert_eq!(st.load_settings(), s);
        let _ = std::fs::remove_dir_all(dir);
    }
}
