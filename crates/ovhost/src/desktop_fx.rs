//! Session desktop effects (Section 4): while a viewer is
//! connected, hide the host's wallpaper (optionally showing a flat colour)
//! and turn off UI animations — less to encode, crisper text, snappier
//! feel. Everything is restored when the session ends, on Ctrl-C / console
//! close / panic (via [`crate::cleanup`]), and — after a hard crash — on
//! the next host start from a marker file holding the original values.
//!
//! Nothing is persisted to the user profile: the wallpaper is changed with
//! `SPIF_SENDCHANGE` only (never `SPIF_UPDATEINIFILE`), animation settings
//! likewise, and the desktop colour with `SetSysColors` (session-only by
//! design) — so even a lost marker is undone by the next logon.
//!
//! The OS calls sit behind [`FxBackend`]; tests and `--fx-dry-run` use
//! [`DryRunFx`], which only logs and records.

use std::path::{Path, PathBuf};

/// The OS surface desktop effects touch.
pub trait FxBackend: Send {
    /// Current wallpaper path (`""` = none).
    fn wallpaper(&mut self) -> Result<String, String>;
    /// Set the wallpaper for this session only (`""` = none).
    fn set_wallpaper(&mut self, path: &str) -> Result<(), String>;
    /// `COLOR_DESKTOP` as a COLORREF (0x00BBGGRR).
    fn desktop_color(&mut self) -> Result<u32, String>;
    /// Set `COLOR_DESKTOP` (session-only).
    fn set_desktop_color(&mut self, c: u32) -> Result<(), String>;
    /// `SPI_GETCLIENTAREAANIMATION`.
    fn client_animation(&mut self) -> Result<bool, String>;
    fn set_client_animation(&mut self, on: bool) -> Result<(), String>;
    /// `SPI_GETANIMATION` (`ANIMATIONINFO.iMinAnimate`: minimize/maximize).
    fn window_animation(&mut self) -> Result<bool, String>;
    fn set_window_animation(&mut self, on: bool) -> Result<(), String>;
}

/// The values we changed, saved before the first change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Original {
    pub wallpaper: Option<String>,
    pub desktop_color: Option<u32>,
    pub client_animation: Option<bool>,
    pub window_animation: Option<bool>,
}

impl Original {
    /// Marker file text (`key=value` lines; the wallpaper path last, raw).
    pub fn to_text(&self) -> String {
        let mut s = String::from("openview-desktopfx 1\n");
        if let Some(c) = self.desktop_color {
            s.push_str(&format!("color={c}\n"));
        }
        if let Some(b) = self.client_animation {
            s.push_str(&format!("client_anim={}\n", b as u8));
        }
        if let Some(b) = self.window_animation {
            s.push_str(&format!("window_anim={}\n", b as u8));
        }
        if let Some(w) = &self.wallpaper {
            s.push_str(&format!("wallpaper={w}\n"));
        }
        s
    }

    pub fn from_text(t: &str) -> Option<Self> {
        let mut lines = t.lines();
        if lines.next()? != "openview-desktopfx 1" {
            return None;
        }
        let mut o = Original::default();
        for l in lines {
            let Some((k, v)) = l.split_once('=') else {
                continue;
            };
            match k {
                "color" => o.desktop_color = v.parse().ok(),
                "client_anim" => o.client_animation = Some(v == "1"),
                "window_anim" => o.window_animation = Some(v == "1"),
                "wallpaper" => o.wallpaper = Some(v.to_owned()),
                _ => {}
            }
        }
        Some(o)
    }

    fn is_empty(&self) -> bool {
        *self == Original::default()
    }
}

/// The session's effects state (see module docs).
pub struct DesktopFx {
    backend: Box<dyn FxBackend>,
    marker: PathBuf,
    orig: Original,
    flat_color: Option<u32>,
    wallpaper_hidden: bool,
    animations_off: bool,
}

impl DesktopFx {
    /// `marker`: where originals are kept while effects are active.
    /// `flat_color`: COLORREF shown instead of the wallpaper (`None` =
    /// leave the desktop colour alone).
    pub fn new(backend: Box<dyn FxBackend>, marker: PathBuf, flat_color: Option<u32>) -> Self {
        Self {
            backend,
            marker,
            orig: Original::default(),
            flat_color,
            wallpaper_hidden: false,
            animations_off: false,
        }
    }

    /// Restore a previous run's leftovers (hard crash with effects on).
    /// Returns whether a marker was found.
    pub fn recover_stale(backend: &mut dyn FxBackend, marker: &Path) -> bool {
        let Ok(text) = std::fs::read_to_string(marker) else {
            return false;
        };
        if let Some(o) = Original::from_text(&text) {
            restore_values(backend, &o);
        }
        let _ = std::fs::remove_file(marker);
        true
    }

    pub fn wallpaper_hidden(&self) -> bool {
        self.wallpaper_hidden
    }

    pub fn animations_off(&self) -> bool {
        self.animations_off
    }

    fn save_marker(&self) {
        if self.orig.is_empty() {
            return;
        }
        let _ = std::fs::write(&self.marker, self.orig.to_text());
    }

    /// Hide (`true`) or restore (`false`) the wallpaper.
    pub fn set_wallpaper_hidden(&mut self, hide: bool) -> Result<(), String> {
        if hide == self.wallpaper_hidden {
            return Ok(());
        }
        if hide {
            if self.orig.wallpaper.is_none() {
                self.orig.wallpaper = Some(self.backend.wallpaper()?);
            }
            if self.flat_color.is_some() && self.orig.desktop_color.is_none() {
                self.orig.desktop_color = self.backend.desktop_color().ok();
            }
            // Originals on disk before the first change.
            self.save_marker();
            if let Some(c) = self.flat_color {
                let _ = self.backend.set_desktop_color(c);
            }
            self.backend.set_wallpaper("")?;
        } else {
            if let Some(w) = self.orig.wallpaper.take() {
                self.backend.set_wallpaper(&w)?;
            }
            if let Some(c) = self.orig.desktop_color.take() {
                let _ = self.backend.set_desktop_color(c);
            }
            self.after_restore();
        }
        self.wallpaper_hidden = hide;
        Ok(())
    }

    /// Disable (`true`) or restore (`false`) UI animations.
    pub fn set_animations_off(&mut self, off: bool) -> Result<(), String> {
        if off == self.animations_off {
            return Ok(());
        }
        if off {
            if self.orig.client_animation.is_none() {
                self.orig.client_animation = Some(self.backend.client_animation()?);
            }
            if self.orig.window_animation.is_none() {
                self.orig.window_animation = Some(self.backend.window_animation()?);
            }
            self.save_marker();
            self.backend.set_client_animation(false)?;
            self.backend.set_window_animation(false)?;
        } else {
            if let Some(b) = self.orig.client_animation.take() {
                self.backend.set_client_animation(b)?;
            }
            if let Some(b) = self.orig.window_animation.take() {
                self.backend.set_window_animation(b)?;
            }
            self.after_restore();
        }
        self.animations_off = off;
        Ok(())
    }

    fn after_restore(&self) {
        if self.orig.is_empty() {
            let _ = std::fs::remove_file(&self.marker);
        } else {
            self.save_marker();
        }
    }

    /// Undo everything (idempotent).
    pub fn restore_all(&mut self) {
        let _ = self.set_wallpaper_hidden(false);
        let _ = self.set_animations_off(false);
        // Anything left (a failed setter above): best-effort raw restore.
        if !self.orig.is_empty() {
            restore_values(&mut *self.backend, &self.orig);
            self.orig = Original::default();
        }
        let _ = std::fs::remove_file(&self.marker);
        self.wallpaper_hidden = false;
        self.animations_off = false;
    }
}

impl Drop for DesktopFx {
    fn drop(&mut self) {
        self.restore_all();
    }
}

fn restore_values(b: &mut dyn FxBackend, o: &Original) {
    if let Some(w) = &o.wallpaper {
        let _ = b.set_wallpaper(w);
    }
    if let Some(c) = o.desktop_color {
        let _ = b.set_desktop_color(c);
    }
    if let Some(v) = o.client_animation {
        let _ = b.set_client_animation(v);
    }
    if let Some(v) = o.window_animation {
        let _ = b.set_window_animation(v);
    }
}

/// Parse `RRGGBB` into a COLORREF (0x00BBGGRR).
pub fn parse_rgb(s: &str) -> Option<u32> {
    let s = s.trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(s, 16).ok()?;
    let (r, g, b) = ((v >> 16) & 0xFF, (v >> 8) & 0xFF, v & 0xFF);
    Some(r | (g << 8) | (b << 16))
}

/// A backend that changes nothing: it logs each call and keeps the state
/// in memory (tests, `--fx-dry-run`).
#[derive(Debug, Clone)]
pub struct DryRunFx {
    pub wallpaper: String,
    pub color: u32,
    pub client_anim: bool,
    pub window_anim: bool,
    pub log: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    pub verbose: bool,
}

impl Default for DryRunFx {
    fn default() -> Self {
        Self {
            wallpaper: r"C:\Users\me\Pictures\wall.jpg".into(),
            color: 0x0080_4000,
            client_anim: true,
            window_anim: true,
            log: Default::default(),
            verbose: false,
        }
    }
}

impl DryRunFx {
    fn note(&self, s: String) {
        if self.verbose {
            tracing::info!("fx dry-run: {s}");
        }
        if let Ok(mut l) = self.log.lock() {
            l.push(s);
        }
    }
}

impl FxBackend for DryRunFx {
    fn wallpaper(&mut self) -> Result<String, String> {
        Ok(self.wallpaper.clone())
    }
    fn set_wallpaper(&mut self, path: &str) -> Result<(), String> {
        self.note(format!("set wallpaper `{path}`"));
        self.wallpaper = path.into();
        Ok(())
    }
    fn desktop_color(&mut self) -> Result<u32, String> {
        Ok(self.color)
    }
    fn set_desktop_color(&mut self, c: u32) -> Result<(), String> {
        self.note(format!("set desktop color {c:#08x}"));
        self.color = c;
        Ok(())
    }
    fn client_animation(&mut self) -> Result<bool, String> {
        Ok(self.client_anim)
    }
    fn set_client_animation(&mut self, on: bool) -> Result<(), String> {
        self.note(format!("set client-area animation {on}"));
        self.client_anim = on;
        Ok(())
    }
    fn window_animation(&mut self) -> Result<bool, String> {
        Ok(self.window_anim)
    }
    fn set_window_animation(&mut self, on: bool) -> Result<(), String> {
        self.note(format!("set window animation {on}"));
        self.window_anim = on;
        Ok(())
    }
}

/// The real Win32 backend (`SystemParametersInfoW`, `SetSysColors`).
#[cfg(windows)]
pub struct Win32Fx;

#[cfg(windows)]
mod win {
    #[repr(C)]
    pub struct AnimationInfo {
        pub cb_size: u32,
        pub i_min_animate: i32,
    }
    #[link(name = "user32")]
    extern "system" {
        pub fn SystemParametersInfoW(
            action: u32,
            uiparam: u32,
            pvparam: *mut core::ffi::c_void,
            winini: u32,
        ) -> i32;
        pub fn GetSysColor(index: i32) -> u32;
        pub fn SetSysColors(n: i32, elements: *const i32, values: *const u32) -> i32;
    }
    pub const SPI_GETDESKWALLPAPER: u32 = 0x0073;
    pub const SPI_SETDESKWALLPAPER: u32 = 0x0014;
    pub const SPI_GETANIMATION: u32 = 0x0048;
    pub const SPI_SETANIMATION: u32 = 0x0049;
    pub const SPI_GETCLIENTAREAANIMATION: u32 = 0x1042;
    pub const SPI_SETCLIENTAREAANIMATION: u32 = 0x1043;
    /// Broadcast WM_SETTINGCHANGE only — never `SPIF_UPDATEINIFILE` (1).
    pub const SPIF_SENDCHANGE: u32 = 0x02;
    pub const COLOR_DESKTOP: i32 = 1;
}

#[cfg(windows)]
impl FxBackend for Win32Fx {
    fn wallpaper(&mut self) -> Result<String, String> {
        let mut buf = [0u16; 1024];
        // SAFETY: buffer of the advertised length (in characters).
        let ok = unsafe {
            win::SystemParametersInfoW(
                win::SPI_GETDESKWALLPAPER,
                buf.len() as u32,
                buf.as_mut_ptr().cast(),
                0,
            )
        };
        if ok == 0 {
            return Err(format!(
                "SPI_GETDESKWALLPAPER: {}",
                std::io::Error::last_os_error()
            ));
        }
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Ok(String::from_utf16_lossy(&buf[..n]))
    }
    fn set_wallpaper(&mut self, path: &str) -> Result<(), String> {
        let mut w: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        // SAFETY: NUL-terminated wide string that outlives the call.
        let ok = unsafe {
            win::SystemParametersInfoW(
                win::SPI_SETDESKWALLPAPER,
                0,
                w.as_mut_ptr().cast(),
                win::SPIF_SENDCHANGE,
            )
        };
        if ok == 0 {
            return Err(format!(
                "SPI_SETDESKWALLPAPER: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }
    fn desktop_color(&mut self) -> Result<u32, String> {
        // SAFETY: plain query.
        Ok(unsafe { win::GetSysColor(win::COLOR_DESKTOP) })
    }
    fn set_desktop_color(&mut self, c: u32) -> Result<(), String> {
        let el = [win::COLOR_DESKTOP];
        let v = [c];
        // SAFETY: one element / value pair. SetSysColors is session-only.
        if unsafe { win::SetSysColors(1, el.as_ptr(), v.as_ptr()) } == 0 {
            return Err(format!("SetSysColors: {}", std::io::Error::last_os_error()));
        }
        Ok(())
    }
    fn client_animation(&mut self) -> Result<bool, String> {
        let mut v: i32 = 0;
        // SAFETY: pvParam points to a BOOL.
        let ok = unsafe {
            win::SystemParametersInfoW(
                win::SPI_GETCLIENTAREAANIMATION,
                0,
                (&mut v as *mut i32).cast(),
                0,
            )
        };
        if ok == 0 {
            return Err(format!(
                "SPI_GETCLIENTAREAANIMATION: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(v != 0)
    }
    fn set_client_animation(&mut self, on: bool) -> Result<(), String> {
        // SPI_SETCLIENTAREAANIMATION takes the BOOL in pvParam itself.
        // SAFETY: no pointer is dereferenced for this action.
        let ok = unsafe {
            win::SystemParametersInfoW(
                win::SPI_SETCLIENTAREAANIMATION,
                0,
                on as usize as *mut core::ffi::c_void,
                win::SPIF_SENDCHANGE,
            )
        };
        if ok == 0 {
            return Err(format!(
                "SPI_SETCLIENTAREAANIMATION: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }
    fn window_animation(&mut self) -> Result<bool, String> {
        let mut ai = win::AnimationInfo {
            cb_size: std::mem::size_of::<win::AnimationInfo>() as u32,
            i_min_animate: 0,
        };
        // SAFETY: ANIMATIONINFO with cbSize set, as the API requires.
        let ok = unsafe {
            win::SystemParametersInfoW(
                win::SPI_GETANIMATION,
                ai.cb_size,
                (&mut ai as *mut win::AnimationInfo).cast(),
                0,
            )
        };
        if ok == 0 {
            return Err(format!(
                "SPI_GETANIMATION: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(ai.i_min_animate != 0)
    }
    fn set_window_animation(&mut self, on: bool) -> Result<(), String> {
        let mut ai = win::AnimationInfo {
            cb_size: std::mem::size_of::<win::AnimationInfo>() as u32,
            i_min_animate: on as i32,
        };
        // SAFETY: as above.
        let ok = unsafe {
            win::SystemParametersInfoW(
                win::SPI_SETANIMATION,
                ai.cb_size,
                (&mut ai as *mut win::AnimationInfo).cast(),
                win::SPIF_SENDCHANGE,
            )
        };
        if ok == 0 {
            return Err(format!(
                "SPI_SETANIMATION: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }
}

/// File name of the crash-recovery marker.
const MARKER_FILE: &str = "ovhost-desktopfx.state";

/// The data dir the marker lives in (set once at startup).
static MARKER_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Keep the marker in the host's data dir (next to the unattended
/// identity). Call once at startup, before [`default_marker`].
pub fn set_marker_dir(dir: &std::path::Path) {
    let _ = MARKER_DIR.set(dir.to_path_buf());
}

/// The marker location: `<data dir>\ovhost-desktopfx.state` (falls back
/// to the working directory when no data dir was set, e.g. in tests).
pub fn default_marker() -> PathBuf {
    MARKER_DIR
        .get()
        .map_or_else(|| PathBuf::from(MARKER_FILE), |d| d.join(MARKER_FILE))
}

/// Where releases before 0.2 kept the marker: next to the executable. A
/// crash under such a release leaves it there; startup recovers it too.
pub fn legacy_marker() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .map(|p| p.with_file_name(MARKER_FILE))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ovfx-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&d);
        d
    }

    #[test]
    fn hide_and_restore_roundtrip() {
        let marker = tmp("roundtrip");
        let be = DryRunFx::default();
        let log = be.log.clone();
        let mut fx = DesktopFx::new(Box::new(be), marker.clone(), Some(0));
        fx.set_wallpaper_hidden(true).unwrap();
        fx.set_animations_off(true).unwrap();
        // Originals are on disk while effects are active.
        let saved = Original::from_text(&std::fs::read_to_string(&marker).unwrap()).unwrap();
        assert_eq!(
            saved.wallpaper.as_deref(),
            Some(r"C:\Users\me\Pictures\wall.jpg")
        );
        assert_eq!(saved.desktop_color, Some(0x0080_4000));
        assert_eq!(saved.client_animation, Some(true));
        assert_eq!(saved.window_animation, Some(true));
        drop(fx);
        assert!(!marker.exists(), "marker removed after restore");
        let log = log.lock().unwrap().clone();
        assert_eq!(
            log,
            vec![
                "set desktop color 0x000000".to_string(),
                "set wallpaper ``".into(),
                "set client-area animation false".into(),
                "set window animation false".into(),
                r"set wallpaper `C:\Users\me\Pictures\wall.jpg`".into(),
                "set desktop color 0x804000".into(),
                "set client-area animation true".into(),
                "set window animation true".into(),
            ]
        );
    }

    #[test]
    fn toggles_are_idempotent_and_partial() {
        let marker = tmp("partial");
        let be = DryRunFx {
            client_anim: false,
            ..Default::default()
        };
        let log = be.log.clone();
        let mut fx = DesktopFx::new(Box::new(be), marker.clone(), None);
        fx.set_animations_off(true).unwrap();
        fx.set_animations_off(true).unwrap();
        fx.set_animations_off(false).unwrap();
        assert!(!marker.exists());
        fx.set_wallpaper_hidden(true).unwrap();
        fx.set_wallpaper_hidden(false).unwrap();
        // The client animation was already off: it is restored to off.
        let log = log.lock().unwrap().clone();
        assert!(log.contains(&"set client-area animation false".to_string()));
        assert!(!log.contains(&"set client-area animation true".to_string()));
        assert!(
            !log.iter().any(|l| l.contains("desktop color")),
            "no flat colour requested"
        );
    }

    #[test]
    fn stale_marker_is_restored_on_next_start() {
        let marker = tmp("stale");
        let orig = Original {
            wallpaper: Some(r"D:\w p\a=b.png".into()),
            desktop_color: None,
            client_animation: Some(true),
            window_animation: Some(false),
        };
        std::fs::write(&marker, orig.to_text()).unwrap();
        let mut be = DryRunFx {
            wallpaper: String::new(),
            client_anim: false,
            ..Default::default()
        };
        assert!(DesktopFx::recover_stale(&mut be, &marker));
        assert_eq!(be.wallpaper, r"D:\w p\a=b.png");
        assert!(be.client_anim);
        assert!(!be.window_anim);
        assert!(!marker.exists());
        assert!(!DesktopFx::recover_stale(&mut be, &marker));
    }

    #[test]
    fn rgb_parse() {
        assert_eq!(parse_rgb("102030"), Some(0x0030_2010));
        assert_eq!(parse_rgb("#000000"), Some(0));
        assert_eq!(parse_rgb("xyz"), None);
    }
}
