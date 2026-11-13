//! Session desktop effects: while a viewer is
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
pub(crate) trait FxBackend: Send {
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
pub(crate) struct Original {
    pub(crate) wallpaper: Option<String>,
    pub(crate) desktop_color: Option<u32>,
    pub(crate) client_animation: Option<bool>,
    pub(crate) window_animation: Option<bool>,
}

impl Original {
    /// Marker file text (`key=value` lines; the wallpaper path last, raw).
    pub(crate) fn to_text(&self) -> String {
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

    pub(crate) fn from_text(t: &str) -> Option<Self> {
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
pub(crate) struct DesktopFx {
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
    pub(crate) fn new(
        backend: Box<dyn FxBackend>,
        marker: PathBuf,
        flat_color: Option<u32>,
    ) -> Self {
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
    pub(crate) fn recover_stale(backend: &mut dyn FxBackend, marker: &Path) -> bool {
        let Ok(text) = std::fs::read_to_string(marker) else {
            return false;
        };
        if let Some(o) = Original::from_text(&text) {
            restore_values(backend, &o);
        }
        let _ = std::fs::remove_file(marker);
        true
    }

    pub(crate) fn wallpaper_hidden(&self) -> bool {
        self.wallpaper_hidden
    }

    pub(crate) fn animations_off(&self) -> bool {
        self.animations_off
    }

    fn save_marker(&self) {
        if self.orig.is_empty() {
            return;
        }
        let _ = std::fs::write(&self.marker, self.orig.to_text());
    }

    /// Hide (`true`) or restore (`false`) the wallpaper.
    pub(crate) fn set_wallpaper_hidden(&mut self, hide: bool) -> Result<(), String> {
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
    pub(crate) fn set_animations_off(&mut self, off: bool) -> Result<(), String> {
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
    pub(crate) fn restore_all(&mut self) {
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
pub(crate) fn parse_rgb(s: &str) -> Option<u32> {
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
pub(crate) struct DryRunFx {
    pub(crate) wallpaper: String,
    pub(crate) color: u32,
    pub(crate) client_anim: bool,
    pub(crate) window_anim: bool,
    pub(crate) log: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    pub(crate) verbose: bool,
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
pub(crate) struct Win32Fx;

#[cfg(windows)]
mod win {
    //! `SystemParametersInfoW` / `SetSysColors` wrappers with `io::Error`
    //! failures.

    use windows::Win32::Foundation::COLORREF;
    use windows::Win32::Graphics::Gdi::{GetSysColor, SetSysColors, COLOR_DESKTOP};
    use windows::Win32::UI::WindowsAndMessaging::{
        SystemParametersInfoW, SPIF_SENDCHANGE, SYSTEM_PARAMETERS_INFO_ACTION,
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    };
    pub(super) use windows::Win32::UI::WindowsAndMessaging::{
        ANIMATIONINFO, SPI_GETANIMATION, SPI_GETCLIENTAREAANIMATION, SPI_GETDESKWALLPAPER,
        SPI_SETANIMATION, SPI_SETCLIENTAREAANIMATION, SPI_SETDESKWALLPAPER,
    };

    /// The Win32 error behind a `windows` error, as `GetLastError` would
    /// have reported it.
    fn os_error(e: &windows::core::Error) -> std::io::Error {
        let hr = e.code().0 as u32;
        let code = if hr & 0xFFFF_0000 == 0x8007_0000 {
            hr & 0xFFFF
        } else {
            hr
        };
        std::io::Error::from_raw_os_error(code as i32)
    }

    /// `SystemParametersInfoW(action, ui_param, pv_param, flags)`; `notify`
    /// broadcasts WM_SETTINGCHANGE (`SPIF_SENDCHANGE`) — never
    /// `SPIF_UPDATEINIFILE`, so nothing is persisted to the user profile.
    ///
    /// # Safety
    ///
    /// `pv_param` must be what `action` expects: a valid pointer to a
    /// buffer / struct of the size `ui_param` describes, or (for actions
    /// that take a value in `pv_param`) that value.
    pub(super) unsafe fn spi(
        action: SYSTEM_PARAMETERS_INFO_ACTION,
        ui_param: u32,
        pv_param: *mut core::ffi::c_void,
        notify: bool,
    ) -> Result<(), std::io::Error> {
        let flags = if notify {
            SPIF_SENDCHANGE
        } else {
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0)
        };
        // SAFETY: the caller guarantees `pv_param` matches `action`.
        unsafe { SystemParametersInfoW(action, ui_param, Some(pv_param), flags) }
            .map_err(|e| os_error(&e))
    }

    /// The desktop background colour (`COLORREF`).
    pub(super) fn desktop_color() -> u32 {
        // SAFETY: a plain query without pointers.
        unsafe { GetSysColor(COLOR_DESKTOP) }
    }

    /// Set the desktop background colour for this session only.
    pub(super) fn set_desktop_color(c: u32) -> Result<(), std::io::Error> {
        let el = [COLOR_DESKTOP.0];
        let v = [COLORREF(c)];
        // SAFETY: one element / value pair, both arrays live for the call.
        unsafe { SetSysColors(1, el.as_ptr(), v.as_ptr()) }.map_err(|e| os_error(&e))
    }
}

#[cfg(windows)]
impl FxBackend for Win32Fx {
    fn wallpaper(&mut self) -> Result<String, String> {
        let mut buf = [0u16; 1024];
        // SAFETY: a writable buffer of the advertised length (in characters).
        unsafe {
            win::spi(
                win::SPI_GETDESKWALLPAPER,
                buf.len() as u32,
                buf.as_mut_ptr().cast(),
                false,
            )
        }
        .map_err(|e| format!("SPI_GETDESKWALLPAPER: {e}"))?;
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Ok(String::from_utf16_lossy(&buf[..n]))
    }
    fn set_wallpaper(&mut self, path: &str) -> Result<(), String> {
        let mut w: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        // SAFETY: a NUL-terminated wide string that outlives the call.
        unsafe { win::spi(win::SPI_SETDESKWALLPAPER, 0, w.as_mut_ptr().cast(), true) }
            .map_err(|e| format!("SPI_SETDESKWALLPAPER: {e}"))
    }
    fn desktop_color(&mut self) -> Result<u32, String> {
        Ok(win::desktop_color())
    }
    fn set_desktop_color(&mut self, c: u32) -> Result<(), String> {
        win::set_desktop_color(c).map_err(|e| format!("SetSysColors: {e}"))
    }
    fn client_animation(&mut self) -> Result<bool, String> {
        let mut v: i32 = 0;
        // SAFETY: for this action pvParam points to a BOOL.
        unsafe {
            win::spi(
                win::SPI_GETCLIENTAREAANIMATION,
                0,
                (&mut v as *mut i32).cast(),
                false,
            )
        }
        .map_err(|e| format!("SPI_GETCLIENTAREAANIMATION: {e}"))?;
        Ok(v != 0)
    }
    fn set_client_animation(&mut self, on: bool) -> Result<(), String> {
        // SPI_SETCLIENTAREAANIMATION takes the BOOL in pvParam itself.
        // SAFETY: no pointer is dereferenced for this action.
        unsafe {
            win::spi(
                win::SPI_SETCLIENTAREAANIMATION,
                0,
                on as usize as *mut core::ffi::c_void,
                true,
            )
        }
        .map_err(|e| format!("SPI_SETCLIENTAREAANIMATION: {e}"))
    }
    fn window_animation(&mut self) -> Result<bool, String> {
        let mut ai = win::ANIMATIONINFO {
            cbSize: std::mem::size_of::<win::ANIMATIONINFO>() as u32,
            iMinAnimate: 0,
        };
        // SAFETY: an ANIMATIONINFO with cbSize set, as the API requires.
        unsafe {
            win::spi(
                win::SPI_GETANIMATION,
                ai.cbSize,
                (&mut ai as *mut win::ANIMATIONINFO).cast(),
                false,
            )
        }
        .map_err(|e| format!("SPI_GETANIMATION: {e}"))?;
        Ok(ai.iMinAnimate != 0)
    }
    fn set_window_animation(&mut self, on: bool) -> Result<(), String> {
        let mut ai = win::ANIMATIONINFO {
            cbSize: std::mem::size_of::<win::ANIMATIONINFO>() as u32,
            iMinAnimate: on as i32,
        };
        // SAFETY: an ANIMATIONINFO with cbSize set, as the API requires.
        unsafe {
            win::spi(
                win::SPI_SETANIMATION,
                ai.cbSize,
                (&mut ai as *mut win::ANIMATIONINFO).cast(),
                true,
            )
        }
        .map_err(|e| format!("SPI_SETANIMATION: {e}"))
    }
}

/// File name of the crash-recovery marker.
const MARKER_FILE: &str = "ovhost-desktopfx.state";

/// The data dir the marker lives in (set once at startup).
static MARKER_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Keep the marker in the host's data dir (next to the unattended
/// identity). Call once at startup, before [`default_marker`].
pub(crate) fn set_marker_dir(dir: &std::path::Path) {
    let _ = MARKER_DIR.set(dir.to_path_buf());
}

/// The marker location: `<data dir>\ovhost-desktopfx.state` (falls back
/// to the working directory when no data dir was set, e.g. in tests).
pub(crate) fn default_marker() -> PathBuf {
    MARKER_DIR
        .get()
        .map_or_else(|| PathBuf::from(MARKER_FILE), |d| d.join(MARKER_FILE))
}

/// Where releases before 0.2 kept the marker: next to the executable. A
/// crash under such a release leaves it there; startup recovers it too.
pub(crate) fn legacy_marker() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .map(|p| p.with_file_name(MARKER_FILE))
}

#[cfg(test)]
mod tests;
