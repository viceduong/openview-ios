//! Local hotkeys.
//!
//! | Key | Action |
//! |---|---|
//! | H / F3 | Show/hide the HUD health panel |
//! | F1 | Cycle the requested quality AUTO → 10 → 30 → 60 fps → AUTO |
//! | F2 | Speaker playback on/off |
//! | M | Mute/unmute (playback keeps running so unmuting resumes in sync) |
//! | F4 | Capture the host's next monitor |
//! | F6 | Start/stop an `.ovrec` recording |
//! | F8 | Show/hide the host's wallpaper (host with `CAP_DESKTOP_FX`) |
//!
//! Only F8 is consumed; every other key, hotkeys included, is still
//! forwarded to the host.

use std::sync::atomic::Ordering;

use ovapp::terminal::{Command, CAP_DESKTOP_FX, OPT_HIDE_WALLPAPER};
use tracing::{debug, info};

use super::Viewer;

/// Windows virtual-key codes of the hotkeys.
const VK_F1: u16 = 0x70;
const VK_F2: u16 = 0x71;
const VK_F3: u16 = 0x72;
const VK_F4: u16 = 0x73;
const VK_F6: u16 = 0x75;
const VK_F8: u16 = 0x77;

/// F1's quality steps: `None` = AUTO (the host's RTT-aware governor),
/// `Some(n)` = manual n fps.
const QUALITY_STEPS: [Option<u16>; 4] = [None, Some(10), Some(30), Some(60)];

/// Whether a key event was used up by a hotkey.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KeyUse {
    /// Also forward the key to the host.
    Forward,
    /// The hotkey owns this key; do not forward it.
    Consumed,
}

/// The toggles the hotkeys drive.
pub(super) struct Hotkeys {
    /// HUD health panel shown (H / F3).
    pub(super) hud_visible: bool,
    /// Index into [`QUALITY_STEPS`] (F1).
    quality_step: usize,
    /// Speaker playback on (F2; `--audio` starts it on).
    pub(super) audio_on: bool,
    /// Incoming audio muted (M).
    audio_muted: bool,
    /// The host's wallpaper is hidden (F8); the host hides it by default.
    remote_wall_hidden: bool,
}

impl Hotkeys {
    pub(super) fn new(audio_on: bool) -> Self {
        Self {
            hud_visible: true,
            quality_step: 0,
            audio_on,
            audio_muted: false,
            remote_wall_hidden: true,
        }
    }

    /// The requested quality mode: `None` = AUTO.
    pub(super) fn quality(&self) -> Option<u16> {
        QUALITY_STEPS[self.quality_step]
    }
}

impl Viewer {
    /// Apply the hotkey bound to `vk`, if any.
    pub(super) fn hotkey(&mut self, vk: u16, down: bool) -> Result<KeyUse, String> {
        if vk == VK_F8 {
            if down && self.term.peer_caps().unwrap_or(0) & CAP_DESKTOP_FX != 0 {
                self.toggle_remote_wallpaper()?;
            }
            return Ok(KeyUse::Consumed);
        }
        if !down {
            return Ok(KeyUse::Forward);
        }
        match vk {
            v if v == u16::from(b'H') || v == u16::from(b'h') || v == VK_F3 => {
                self.hotkeys.hud_visible = !self.hotkeys.hud_visible;
            }
            VK_F1 => {
                let keys = &mut self.hotkeys;
                keys.quality_step = (keys.quality_step + 1) % QUALITY_STEPS.len();
                let target_fps = keys.quality().unwrap_or(0);
                self.send(&Command::QualitySet { target_fps })?;
            }
            VK_F2 => {
                self.hotkeys.audio_on = !self.hotkeys.audio_on;
                self.apply_audio_state();
                info!("audio {}", if self.hotkeys.audio_on { "on" } else { "off" });
            }
            v if v == u16::from(b'M') || v == u16::from(b'm') => {
                self.hotkeys.audio_muted = !self.hotkeys.audio_muted;
                self.apply_audio_state();
                info!(
                    "audio {}",
                    if self.hotkeys.audio_muted {
                        "muted"
                    } else {
                        "unmuted"
                    }
                );
            }
            VK_F6 => {
                let peer = hex::encode(self.peer_fp);
                self.recorder
                    .toggle(self.last_dims, self.hotkeys.quality(), &peer);
            }
            VK_F4 => self.next_monitor()?,
            _ => {}
        }
        Ok(KeyUse::Forward)
    }

    /// Playback runs only while on and not muted.
    fn apply_audio_state(&self) {
        self.audio_bus
            .set_enabled(self.hotkeys.audio_on && !self.hotkeys.audio_muted);
    }

    /// F8: ask the host to hide or show its wallpaper.
    fn toggle_remote_wallpaper(&mut self) -> Result<(), String> {
        self.hotkeys.remote_wall_hidden = !self.hotkeys.remote_wall_hidden;
        let hidden = self.hotkeys.remote_wall_hidden;
        self.send(&Command::SessionOption {
            key: OPT_HIDE_WALLPAPER,
            value: u8::from(hidden),
        })?;
        debug!(
            "remote wallpaper {}",
            if hidden { "hidden" } else { "shown" }
        );
        Ok(())
    }

    /// F4: point the capture at the host's next monitor (no-op with fewer
    /// than two).
    fn next_monitor(&mut self) -> Result<(), String> {
        let list = self.monitors.lock().map(|m| m.clone()).unwrap_or_default();
        if list.len() < 2 {
            return Ok(());
        }
        let next = (self.cur_mon.load(Ordering::Relaxed) + 1) % list.len();
        self.cur_mon.store(next, Ordering::Relaxed);
        let m = &list[next];
        self.send(&Command::CaptureRegion {
            x: m.x,
            y: m.y,
            w: m.w,
            h: m.h,
        })
    }
}
