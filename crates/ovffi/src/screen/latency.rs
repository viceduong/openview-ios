//! Per-stage latency telemetry for the shell (`CAP_TIMING`): the host's
//! TIMING stamps joined with reassembly / delivery / present, and the
//! clock offset measured over clock-probe PINGs (sent through
//! [`ov_screen_keepalive_tick`](crate::ov_screen_keepalive_tick)).

use std::time::{Duration, Instant};

use ovapp::latency::{is_clock_probe, LatencySnapshot, Stage, StagePercentiles};

use super::OvScreen;
use crate::error::OV_ERR_PANIC;

/// One stage's rolling percentiles (`ov_latency_stage` in shim.h).
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct OvLatencyStage {
    /// Median, µs.
    pub p50_us: u32,
    /// 95th percentile, µs.
    pub p95_us: u32,
    /// Samples behind them (0 = no data yet).
    pub samples: u32,
}

impl From<StagePercentiles> for OvLatencyStage {
    fn from(p: StagePercentiles) -> Self {
        Self {
            p50_us: p.p50_us,
            p95_us: p.p95_us,
            samples: p.n,
        }
    }
}

/// Rolling per-stage latency (`ov_latency_stats` in shim.h), filled by
/// [`ov_screen_latency_stats`]. Stages: host capture → grab, grab → encode,
/// encode → first byte sent, network (one way, needs the clock offset),
/// reassembled → delivered to the shell, delivered → presented, and the
/// total capture → presented (needs the clock offset).
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct OvLatencyStats {
    /// Host: present on the host → grab done.
    pub capture: OvLatencyStage,
    /// Host: grab done → encode done.
    pub encode: OvLatencyStage,
    /// Host: encode done → first byte handed to the transport.
    pub send: OvLatencyStage,
    /// Host first byte → update reassembled here (one way).
    pub network: OvLatencyStage,
    /// Reassembled → decoded / handed to the shell.
    pub decode: OvLatencyStage,
    /// Handed to the shell → presented (VideoToolbox decode included for
    /// H.264).
    pub present: OvLatencyStage,
    /// Host capture → presented, end to end.
    pub total: OvLatencyStage,
    /// 1 when the clock offset to the host is known.
    pub clock_valid: u32,
    /// Updates measured since the screen was created.
    pub frames: u64,
    /// Host clock minus device clock, µs (valid with `clock_valid`).
    pub clock_offset_us: i64,
    /// RTT of the probe the offset comes from, µs.
    pub clock_rtt_us: u64,
}

impl From<LatencySnapshot> for OvLatencyStats {
    fn from(s: LatencySnapshot) -> Self {
        let st = |x: Stage| OvLatencyStage::from(s.stage(x));
        Self {
            capture: st(Stage::Capture),
            encode: st(Stage::Encode),
            send: st(Stage::Send),
            network: st(Stage::Network),
            decode: st(Stage::Decode),
            present: st(Stage::Present),
            total: st(Stage::Total),
            clock_valid: u32::from(s.offset_us.is_some()),
            frames: s.frames,
            clock_offset_us: s.offset_us.unwrap_or(0),
            clock_rtt_us: s.clock_rtt_us.unwrap_or(0),
        }
    }
}

impl OvScreen {
    /// Both sides speak `CAP_TIMING`.
    pub(super) fn timing_enabled(&self) -> bool {
        let bit = ovapp::terminal::CAP_TIMING;
        self.local_caps() & bit != 0 && self.peer_caps & bit != 0
    }

    /// A clock probe due at `now` (shell clock): its nonce, recorded as
    /// sent now. `None` when not due or the host is not timing-capable.
    pub(super) fn clock_probe(&mut self, now: Instant) -> Option<u64> {
        if !self.timing_enabled() {
            return None;
        }
        let nonce = self.clock.due(now)?;
        self.clock.note_ping(nonce, ovcore::clocksync::mono_us());
        Some(nonce)
    }

    /// A timed PONG: a clock sample (when it answers one of our PINGs).
    pub(super) fn clock_pong(&mut self, nonce: u64, rx_us: u64, tx_us: u64) {
        self.clock
            .on_pong(nonce, rx_us, tx_us, ovcore::clocksync::mono_us());
    }

    /// Whether `nonce` echoes a clock probe (not the keepalive).
    pub(super) fn is_probe_echo(nonce: u64) -> bool {
        is_clock_probe(nonce)
    }

    /// Update `(kind, seq)` reached the shell (tiles: decoded and handed to
    /// the frame sink; H.264: handed to the video sink) right now.
    pub(super) fn latency_delivered(&mut self, kind: u8, seq: u64, received: Instant) {
        let now = Instant::now();
        self.latency.on_received(kind, seq, received);
        self.latency.on_decoded(kind, seq, now);
    }
}

/// Fill `out` with the rolling per-stage latency (last ~120 updates per
/// stage). Returns 0, or -1 on a null argument.
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`](crate::ov_screen_new),
/// with no concurrent call on it (session queue). `out` must be null or
/// valid for writing one `ov_latency_stats`.
#[no_mangle]
pub unsafe extern "C" fn ov_screen_latency_stats(
    s: *mut OvScreen,
    out: *mut OvLatencyStats,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        // SAFETY: `s` is null or a live screen with no concurrent use, and
        // `out` is null or writable (caller contract).
        let (Some(screen), Some(out)) = (unsafe { (s.as_mut(), out.as_mut()) }) else {
            return -1;
        };
        let snap = screen.latency.snapshot(Some(screen.clock.sync()));
        *out = OvLatencyStats::from(snap);
        0
    })
}

/// A picture reached the screen: every update the shell had been handed at
/// least `drawn_age_us` ago (when the picture was drawn) was presented
/// `presented_age_us` ago (both relative to this call; e.g. Metal:
/// `CACurrentMediaTime() − drawable.presentedTime`; AVSampleBufferDisplayLayer:
/// 0 / 0 right after enqueue). Returns how many updates were measured, or
/// -1 on a null handle.
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`](crate::ov_screen_new),
/// with no concurrent call on it (session queue).
#[no_mangle]
pub unsafe extern "C" fn ov_screen_latency_presented(
    s: *mut OvScreen,
    drawn_age_us: i64,
    presented_age_us: i64,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        // SAFETY: `s` is null or a live screen with no concurrent use
        // (caller contract).
        let Some(screen) = (unsafe { s.as_mut() }) else {
            return -1;
        };
        let now = Instant::now();
        let ago = |us: i64| {
            now.checked_sub(Duration::from_micros(us.max(0) as u64))
                .unwrap_or(now)
        };
        let n = screen.latency.on_presented(
            ago(drawn_age_us),
            ago(presented_age_us),
            Some(screen.clock.sync()),
        );
        n.min(i32::MAX as usize) as i32
    })
}
