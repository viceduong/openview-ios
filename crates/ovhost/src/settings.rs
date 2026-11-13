//! Process-wide switches set once from the command line (see [`apply`]) and
//! read by the session, capture and input code.
//!
//! Most of them are development / bench knobs; they live in statics so the
//! threads that read them (capture, input) need no plumbing.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::OnceLock;

use crate::cli::{Cli, H264Mode, Pattern};
use crate::direct_net;
use crate::session::fx::FxCfg;

/// `--dry-input`: log every `SendInput` call instead of performing it.
pub(crate) static DRY_INPUT: AtomicBool = AtomicBool::new(false);

/// `--gdi`: skip DXGI Desktop Duplication.
pub(crate) static FORCE_GDI: AtomicBool = AtomicBool::new(false);

/// `--h264`: which H.264 encoder the motion path may use (one of the
/// `H264_*` codes).
pub(crate) static H264_MODE: AtomicU8 = AtomicU8::new(H264_AUTO);
/// [`H264_MODE`]: hardware encoder first, software fallback.
pub(crate) const H264_AUTO: u8 = 0;
/// [`H264_MODE`]: no H.264 (lossless tiles only).
pub(crate) const H264_OFF: u8 = 1;
/// [`H264_MODE`]: Microsoft's software encoder only.
pub(crate) const H264_SW: u8 = 2;
/// [`H264_MODE`]: hardware encoders only.
pub(crate) const H264_HW: u8 = 3;

impl H264Mode {
    fn code(self) -> u8 {
        match self {
            Self::Auto => H264_AUTO,
            Self::Off => H264_OFF,
            Self::Sw => H264_SW,
            Self::Hw => H264_HW,
        }
    }
}

/// `--test-pattern`: 0 = the real screen, 1 video, 2 scroll, 3 photo.
pub(crate) static TEST_PATTERN: AtomicU8 = AtomicU8::new(0);

/// `--lossless`: never send lossy JPEG image tiles (even to a CAP_LOSSY
/// viewer).
pub(crate) static NO_LOSSY: AtomicBool = AtomicBool::new(false);
/// `--no-tile-cache`: no CACHE REF / STORE records.
pub(crate) static NO_TILE_CACHE: AtomicBool = AtomicBool::new(false);
/// `--lossy Q`: JPEG quality of lossy image tiles.
pub(crate) static LOSSY_QUALITY: AtomicU8 = AtomicU8::new(ovapp::lossy::DEFAULT_QUALITY);
/// `--video-bpp`: H.264 bitrate budget override (bench knob; unset = the
/// codec default).
pub(crate) static VIDEO_BPP: OnceLock<f64> = OnceLock::new();
/// `--no-media`: no media channel / v2 congestion control (A/B benches).
pub(crate) static NO_MEDIA: AtomicBool = AtomicBool::new(false);
/// `--no-pacing`: the unpaced sender (A/B benches).
pub(crate) static NO_PACING: AtomicBool = AtomicBool::new(false);
/// `--max-fps`: highest AUTO capture rate; the rate only goes above `--fps`
/// when the pacer's delivery rate carries it.
pub(crate) static MAX_AUTO_FPS: AtomicU32 = AtomicU32::new(60);

/// The direct-path socket + candidates (unset = `--no-direct` or the UDP
/// port could not be bound).
pub(crate) static DIRECT: OnceLock<direct_net::DirectNet> = OnceLock::new();

/// Desktop effects configuration for screen sessions.
pub(crate) static FX_CFG: OnceLock<FxCfg> = OnceLock::new();

/// Store the command line's process-wide switches.
pub(crate) fn apply(cli: &Cli) {
    DRY_INPUT.store(cli.dry_input, Ordering::Relaxed);
    FORCE_GDI.store(cli.gdi, Ordering::Relaxed);
    H264_MODE.store(cli.h264.code(), Ordering::Relaxed);
    #[cfg(windows)]
    if cli.h264 != H264Mode::Off && !ovapp::mfcodec::available() {
        tracing::info!(
            "{}; H.264 off, streaming lossless tiles",
            ovapp::mfcodec::UNAVAILABLE
        );
        H264_MODE.store(H264_OFF, Ordering::Relaxed);
    }
    #[cfg(windows)]
    crate::dxgi::GPU_ENCODE.store(!cli.no_gpu_encode, Ordering::Relaxed);
    TEST_PATTERN.store(
        match cli.test_pattern {
            None => 0,
            Some(Pattern::Video) => 1,
            Some(Pattern::Scroll) => 2,
            Some(Pattern::Photo) => 3,
        },
        Ordering::Relaxed,
    );
    NO_LOSSY.store(cli.lossless, Ordering::Relaxed);
    NO_TILE_CACHE.store(cli.no_tile_cache, Ordering::Relaxed);
    if let Some(q) = cli.lossy {
        LOSSY_QUALITY.store(q, Ordering::Relaxed);
    }
    MAX_AUTO_FPS.store(cli.max_fps, Ordering::Relaxed);
    if let Some(bpp) = cli.video_bpp {
        let _ = VIDEO_BPP.set(bpp);
    }
    NO_MEDIA.store(cli.no_media, Ordering::Relaxed);
    NO_PACING.store(cli.no_pacing, Ordering::Relaxed);
}
