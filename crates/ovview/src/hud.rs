//! The desktop status HUD (v0.16): live link health in a small corner
//! overlay — frames/s, round-trip time, resolution, bandwidth.
//!
//! The *pure* side lives here (line formatting, from [`ovapp::liveness`]'s
//! measured numbers) so it is unit-testable on every target; the Windows
//! side draws the lines with GDI `TextOutW` in the window's `WM_PAINT`.

use ovapp::liveness::LinkStats;

/// How many seconds of frame samples the FPS/bandwidth window covers.
pub const STATS_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

/// The six HUD lines, in display order. `mon` is the capture target (v0.18):
/// `None` = unknown (no MONITORS_INFO yet / single-monitor host).
/// `rtt_ms`/`fps`/`mbps` are already
/// computed by the caller (keepalive + [`LinkStats`]); `None` RTT means no
/// PONG has come back yet. `quality` is the requested quality mode (v0.17):
/// `None` = AUTO (RTT-aware host governor), `Some(n)` = manual n fps.
/// `recording` is the live recording indicator (v0.19): elapsed ms, bytes
/// written, frames captured — `None` when not recording. When active, it
/// takes the MON line's slot (the monitor target returns once recording
/// stops, so no information is permanently lost).
// 8 args is the natural shape here (the caller already owns each value
// individually); grouping them into a struct would just add noise.
#[allow(clippy::too_many_arguments)]
pub fn hud_lines(
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
pub fn new_link_stats() -> LinkStats {
    LinkStats::new(STATS_WINDOW)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hud_lines_format_all_fields() {
        let lines = hud_lines(Some(42.4), 9.96, 1.2345, 1920, 1080, None, None, None);
        assert_eq!(lines[0], "RTT 42 ms");
        assert_eq!(lines[1], "FPS 10.0");
        assert_eq!(lines[2], "RES 1920x1080");
        assert_eq!(lines[3], "BW 1.23 MB/s");
        assert_eq!(lines[4], "Q AUTO");
        assert_eq!(lines[5], "MON —");
    }

    #[test]
    fn hud_lines_shows_the_capture_target() {
        let lines = hud_lines(
            None,
            0.0,
            0.0,
            0,
            0,
            None,
            Some("2/3 2560x1440".to_owned()),
            None,
        );
        assert_eq!(lines[5], "MON 2/3 2560x1440");
    }

    #[test]
    fn hud_lines_shows_a_dash_before_the_first_pong() {
        assert_eq!(
            hud_lines(None, 0.0, 0.0, 0, 0, Some(30), None, None)[0],
            "RTT —"
        );
    }

    #[test]
    fn hud_lines_shows_the_manual_quality_mode() {
        assert_eq!(
            hud_lines(Some(1.0), 0.0, 0.0, 0, 0, Some(30), None, None)[4],
            "Q 30 fps"
        );
        assert_eq!(
            hud_lines(Some(1.0), 0.0, 0.0, 0, 0, Some(60), None, None)[4],
            "Q 60 fps"
        );
    }

    #[test]
    fn hud_lines_shows_the_recording_indicator() {
        let lines = hud_lines(
            None,
            0.0,
            0.0,
            0,
            0,
            None,
            None,
            Some((65_000, 1_572_864, 42)),
        );
        assert_eq!(lines[5], "REC ● 65s 1.50M 42f");
        assert_eq!(
            hud_lines(None, 0.0, 0.0, 0, 0, None, Some("1/1".to_owned()), None)[5],
            "MON 1/1"
        );
    }
}
