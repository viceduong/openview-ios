//! The inbound command hook: everything the host sends besides frames —
//! the monitor list, audio (with loss concealment), clipboard text, the
//! remote cursor overlay, and chat lines.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use ovapp::audio::{AudioDecoder, CODEC_OPUS};
use ovapp::terminal::{Command, GAP_AUDIO};
use tracing::{info, warn};

use super::Viewer;
use crate::clipboard_os;
use crate::window::CursorBmp;

/// An audio inter-arrival gap longer than this (us) counts as an audible
/// gap (the 30 ms playback cushion plus one 10 ms packet).
const AUDIO_GAP_US: u64 = 40_000;

/// Received-audio counters for the end-of-session summary, updated by the
/// hook.
#[derive(Debug, Default)]
pub(super) struct AudioStats {
    /// AUDIO commands received.
    pub(super) ticks: AtomicU64,
    /// Their payload bytes.
    pub(super) bytes: AtomicU64,
    /// How many of them were Opus.
    pub(super) opus_ticks: AtomicU64,
    /// Arrival time of the last AUDIO (us since session start; 0 = none).
    last_arrival_us: AtomicU64,
    /// Inter-arrival gaps longer than [`AUDIO_GAP_US`].
    pub(super) gaps: AtomicU64,
    /// Total length of those gaps (us).
    pub(super) gap_us: AtomicU64,
    /// Packets lost on the media channel and concealed (FEC / PLC).
    pub(super) concealed: AtomicU64,
}

impl Viewer {
    /// Install the command hook on the session. Window handles are only
    /// touched when a window exists (headless sessions still decode AUDIO).
    pub(super) fn install_cmd_hook(&mut self) {
        let win = self.win.clone();
        let bus = self.audio_bus.clone();
        let stats = Arc::clone(&self.audio_stats);
        let monitors = Arc::clone(&self.monitors);
        let cur_mon = Arc::clone(&self.cur_mon);
        let clip = self.clip_sync.clone();
        let session_start = self.session_start;
        // Raw PCM or Opus (the host encodes Opus only for our CAP_OPUS);
        // one decoder state for the stream.
        let mut audio_dec = AudioDecoder::new();
        // (packets, samples) lost right before the next AUDIO.
        let mut audio_lost: (u16, u32) = (0, 0);
        self.term.on_cmd(Some(Box::new(move |cmd| match cmd {
            Command::MonitorsInfo { monitors: list } => {
                if let Ok(mut m) = monitors.lock() {
                    *m = list.clone();
                }
                cur_mon.store(0, Ordering::Relaxed);
            }
            Command::Audio(payload) => {
                // Inter-arrival gaps longer than the playback cushion (each
                // one is an audible gap unless concealed).
                let now_us = session_start.elapsed().as_micros() as u64;
                let last = stats.last_arrival_us.swap(now_us, Ordering::Relaxed);
                if last != 0 && now_us.saturating_sub(last) > AUDIO_GAP_US {
                    stats.gaps.fetch_add(1, Ordering::Relaxed);
                    stats.gap_us.fetch_add(now_us - last, Ordering::Relaxed);
                }
                stats.ticks.fetch_add(1, Ordering::Relaxed);
                stats
                    .bytes
                    .fetch_add(payload.len() as u64, Ordering::Relaxed);
                if payload.first() == Some(&CODEC_OPUS) {
                    stats.opus_ticks.fetch_add(1, Ordering::Relaxed);
                }
                // Packets lost on the media channel right before this one:
                // Opus PLC for all but the last, which the in-band FEC of
                // this packet reconstructs.
                let (lost, lost_samples) = std::mem::take(&mut audio_lost);
                if bus.is_enabled() {
                    if lost > 0 {
                        let per = lost_samples / u32::from(lost).max(1);
                        for _ in 1..lost {
                            if let Ok(s) = audio_dec.conceal(per) {
                                bus.push(&s);
                            }
                        }
                        if let Ok(s) = audio_dec.decode_fec(payload, per) {
                            bus.push(&s);
                        }
                    }
                    if let Ok(samples) = audio_dec.decode(payload) {
                        bus.push(&samples);
                    }
                }
            }
            Command::MediaGap {
                kind,
                packets,
                samples,
            } if *kind == GAP_AUDIO => {
                audio_lost = (
                    audio_lost.0.saturating_add(*packets),
                    audio_lost.1.saturating_add(*samples),
                );
                // Concealed (FEC / PLC), not a stall: counted separately.
                stats
                    .concealed
                    .fetch_add(u64::from(*packets), Ordering::Relaxed);
            }
            // The host's clipboard text: write it to the local OS clipboard,
            // then record it in the echo-loop guard so the loop's own poll
            // does not send it straight back.
            Command::Clipboard { kind: 0, text } => {
                if let Some(sync) = &clip {
                    let text = String::from_utf8_lossy(text).to_string();
                    if clipboard_os::clip_write_text(&text) {
                        if let Ok(mut s) = sync.lock() {
                            s.note_remote(&text);
                        }
                    } else {
                        warn!("clipboard write failed (peer text dropped)");
                    }
                }
            }
            // Reserved clipboard kinds are rejected by decode; this arm only
            // keeps the match exhaustive.
            Command::Clipboard { .. } => {}
            // The remote cursor overlay, composited onto the presented frame.
            Command::Cursor { x, y } => {
                if let Some(w) = &win {
                    w.set_cursor(*x, *y);
                }
            }
            Command::CursorShape {
                hotspot_x,
                hotspot_y,
                w: cw,
                h: ch,
                bgra,
            } => {
                if let Some(w) = &win {
                    w.set_cursor_shape(CursorBmp {
                        hotspot_x: *hotspot_x,
                        hotspot_y: *hotspot_y,
                        w: *cw,
                        h: *ch,
                        bgra: bgra.clone(),
                    });
                }
            }
            // A chat line (TYPE with key 0): the terminal path's echo.
            Command::Type {
                key: 0,
                mods: 0,
                payload,
            } => {
                let end = payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(payload.len());
                info!("[peer] {}", String::from_utf8_lossy(&payload[..end]));
            }
            _ => {}
        })));
    }
}
