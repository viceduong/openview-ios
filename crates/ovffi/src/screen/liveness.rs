//! Keepalive pacing, the inbound watchdog, and the liveness sink, all on
//! the shell's millisecond clock.

use super::OvScreen;
use crate::error::OV_ERR_PANIC;

/// Keepalive tick: call periodically (e.g. every pump cycle) with
/// the shell's monotonic clock in milliseconds. Returns the PING sequence to
/// put on the wire (> 0) when a keepalive PING is due — the shell sends a
/// `PING` command (8-byte BE sequence) on channel 0 — or 0 when not due.
/// The matching `PONG` echo (fed back through [`ov_screen_feed`](crate::ov_screen_feed))
/// measures the RTT and fires the liveness sink.
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`](crate::ov_screen_new).
/// The screen is not internally locked: no other call on the same handle may
/// run concurrently (the shell drives it from one serial queue).
#[no_mangle]
pub unsafe extern "C" fn ov_screen_keepalive_tick(s: *mut OvScreen, now_ms: i64) -> i64 {
    ffi_guard!(0, {
        // SAFETY: `s` is null or a live screen with no concurrent use
        // (caller contract).
        let Some(screen) = (unsafe { s.as_mut() }) else {
            return 0;
        };
        let now = screen.shell_now(now_ms);
        match screen.keepalive.tick(now) {
            Some(seq) => {
                screen.ping_sent_real = Some(std::time::Instant::now());
                seq as i64
            }
            None => 0,
        }
    })
}

/// Watchdog: milliseconds since the last inbound record from the host (any
/// command, media included), on the shell's clock (`now_ms` as for
/// [`ov_screen_liveness_tick`], mapped the same way so reconnects keep
/// working). -1 on a null handle. The host sends at least a keepalive PING
/// every 5 s, so a few seconds of silence means the session is dead even if
/// no transport reported it.
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`](crate::ov_screen_new),
/// with no concurrent call on it.
#[no_mangle]
pub unsafe extern "C" fn ov_screen_inbound_age_ms(s: *mut OvScreen, now_ms: i64) -> i64 {
    ffi_guard!(-1, {
        // SAFETY: `s` is null or a live screen with no concurrent use
        // (caller contract).
        let Some(screen) = (unsafe { s.as_mut() }) else {
            return -1;
        };
        let now = screen.shell_now(now_ms);
        screen
            .live
            .since_inbound(now)
            .as_millis()
            .min(i64::MAX as u128) as i64
    })
}

/// Liveness tick: call periodically with the shell's monotonic clock
/// in milliseconds. Fires the liveness sink with the current alive/RTT/FPS
/// verdict and returns 0 (or -1 on a null handle).
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`](crate::ov_screen_new).
/// The screen is not internally locked: no other call on the same handle may
/// run concurrently (the shell drives it from one serial queue).
#[no_mangle]
pub unsafe extern "C" fn ov_screen_liveness_tick(s: *mut OvScreen, now_ms: i64) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        // SAFETY: `s` is null or a live screen with no concurrent use
        // (caller contract).
        let Some(screen) = (unsafe { s.as_mut() }) else {
            return -1;
        };
        let now = screen.shell_now(now_ms);
        let (fps, _) = screen.stats.snapshot(now);
        screen.last_fps = fps as f32;
        let alive = i32::from(!screen.live.is_dead(now, ovapp::liveness::DEAD_AFTER));
        if let Some(f) = screen.liveness_sink.f {
            f(
                screen.liveness_sink.user,
                alive,
                screen.last_rtt_ms,
                screen.last_fps,
            );
        }
        0
    })
}
