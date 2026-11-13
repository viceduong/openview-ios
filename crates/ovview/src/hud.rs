//! The desktop status HUD: live link health in a small corner
//! overlay — frames/s, round-trip time, resolution, bandwidth.
//!
//! The *pure* side lives here (line formatting, from [`ovapp::liveness`]'s
//! measured numbers) so it is unit-testable on every target; the Windows
//! window draws the lines (DirectWrite, or GDI `TextOutW` on the GDI path).

use ovapp::liveness::LinkStats;

/// How many seconds of frame samples the FPS/bandwidth window covers.
const STATS_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

/// The six HUD lines, in display order. `mon` is the capture target:
/// `None` = unknown (no MONITORS_INFO yet / single-monitor host).
/// `rtt_ms`/`fps`/`mbps` are already computed by the caller (keepalive +
/// [`LinkStats`]); `None` RTT means no PONG has come back yet. `quality` is
/// the requested quality mode: `None` = AUTO (RTT-aware host governor),
/// `Some(n)` = manual n fps.
/// `recording` is the live recording indicator: elapsed ms, bytes
/// written, frames captured — `None` when not recording. When active, it
/// takes the MON line's slot (the monitor target returns once recording
/// stops, so no information is permanently lost).
// 8 args is the natural shape here (the caller already owns each value
// individually); grouping them into a struct would just add noise.
#[allow(clippy::too_many_arguments)]
pub(crate) fn hud_lines(
    rtt_ms: Option<f64>,
    fps: f64,
    mbps: f64,
    width: u32,
    height: u32,
    quality: Option<u16>,
    mon: Option<String>,
    recording: Option<(u64, u64, u64)>,
) -> [String; 6] {
    [
        format!(
            "RTT {}",
            match rtt_ms {
                Some(ms) => format!("{ms:.0} ms"),
                None => "—".to_owned(),
            }
        ),
        format!("FPS {fps:.1}"),
        format!("RES {width}x{height}"),
        format!("BW {mbps:.2} MB/s"),
        format!(
            "Q {}",
            match quality {
                None => "AUTO".to_owned(),
                Some(n) => format!("{n} fps"),
            }
        ),
        match (recording, mon) {
            (Some((ms, bytes, frames)), _) => format!(
                "REC ● {}s {:.2}M {}f",
                ms / 1000,
                bytes as f64 / (1024.0 * 1024.0),
                frames
            ),
            (None, Some(m)) => format!("MON {m}"),
            (None, None) => "MON —".to_owned(),
        },
    ]
}

/// A new [`LinkStats`] window for the HUD's FPS/bandwidth counters.
pub(crate) fn new_link_stats() -> LinkStats {
    LinkStats::new(STATS_WINDOW)
}

#[cfg(test)]
mod tests;
