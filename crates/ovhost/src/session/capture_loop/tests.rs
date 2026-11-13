use ovapp::adapt::{frame_changed, should_send};

use super::*;

#[test]
fn auto_rate_rises_to_60_only_when_the_link_carries_it() {
    assert_eq!(link_base_fps(10, 60, 0, 50_000.0), 10, "no rate yet");
    assert_eq!(link_base_fps(10, 60, 1_000_000, 0.0), 10, "no updates yet");
    // 1 MB/s, 40 KB updates: half the link = 12 updates/s.
    assert_eq!(link_base_fps(10, 60, 1_000_000, 40_000.0), 10);
    // 4 MB/s, 40 KB: 50/s -> 30.
    assert_eq!(link_base_fps(10, 60, 4_000_000, 40_000.0), 30);
    // 20 MB/s, 40 KB: 250/s -> 60.
    assert_eq!(link_base_fps(10, 60, 20_000_000, 40_000.0), 60);
    assert_eq!(
        link_base_fps(10, 45, 20_000_000, 40_000.0),
        45,
        "--max-fps caps"
    );
    assert_eq!(
        link_base_fps(30, 20, 20_000_000, 1.0),
        30,
        "never below --fps"
    );
}

#[test]
fn capture_waits_for_the_link_to_free_up() {
    let ms = Duration::from_millis;
    // Link free, nothing queued: capture now.
    assert_eq!(capture_hold(Duration::ZERO, ms(20), false), None);
    // In-flight frame finishes within one capture's time: start now so
    // the new frame is ready as the link frees up.
    assert_eq!(capture_hold(ms(15), ms(20), false), None);
    // Link busy for a while: hold (in short slices).
    assert_eq!(capture_hold(ms(30), ms(20), false), Some(ms(10)));
    assert_eq!(capture_hold(ms(300), ms(20), false), Some(ms(20)));
    // A captured frame is already waiting: don't compress another.
    assert_eq!(capture_hold(Duration::ZERO, ms(20), true), Some(ms(1)));
}

/// An in-memory "screen": a fixed BGRA frame that never changes.
struct StaticScreen {
    px: Vec<u8>,
    dims: (u32, u32),
}

impl StaticScreen {
    fn capture(&self) -> (Vec<u8>, u32, u32) {
        (self.px.clone(), self.dims.0, self.dims.1)
    }
}

/// Replays the capture-decision core (no screen source, no wire):
/// governor-gated captures, `should_send` decision, sent/skipped
/// accounting. Returns the counters so the test can assert a static
/// screen sends exactly one frame and eases to the idle rate.
fn run_static_loop(screen: &StaticScreen, ticks: usize) -> (u64, u64, RateStats) {
    let mut gov = RateGovernor::new(10, 4);
    // Simulated clock: each pump tick advances 5 ms, so the test is
    // deterministic and instant.
    let mut now = Instant::now();
    let mut last_capture = now;
    let mut last_px: Option<Vec<u8>> = None;
    let mut last_dims = (0u32, 0u32);
    let mut sent = 0u64;
    let mut skipped = 0u64;
    // The governor gate decides whether a capture actually happens.
    for _ in 0..ticks {
        now += Duration::from_millis(5);
        if now.saturating_duration_since(last_capture) >= gov.interval() {
            last_capture = now;
            let (px, w, h) = screen.capture();
            let dims = (w, h);
            if should_send(
                last_px.as_deref().map(|p| (p, last_dims)),
                dims != last_dims,
                &px,
            ) {
                last_px = Some(px);
                last_dims = dims;
                sent += 1;
                let _ = gov.on_change();
            } else {
                skipped += 1;
                let _ = gov.on_idle();
            }
        }
    }
    (sent, skipped, gov.stats())
}

#[test]
fn static_screen_sends_once_then_skips_at_idle_rate() {
    let screen = StaticScreen {
        px: vec![0x40; 320 * 240 * 4],
        dims: (320, 240),
    };
    // ~3 s of pump ticks at 5 ms: at 10 fps base the first capture
    // sends; everything after is unchanged and skipped as the governor
    // eases toward 4 fps.
    let (sent, skipped, stats) = run_static_loop(&screen, 600);
    assert_eq!(sent, 1, "a static screen sends exactly one frame");
    assert!(skipped > 0, "unchanged captures must be skipped");
    assert!(stats.at_idle_rate, "governor must reach the idle rate");
    assert!(
        stats.interval >= Duration::from_millis(250),
        "idle interval = 1/4 fps"
    );
    // Sanity: the skipped frame really is identical to the sent one.
    let (px, _, _) = screen.capture();
    assert!(!frame_changed(&screen.px, &px));
}

#[test]
fn metrics_log_formats() {
    let stats = RateStats {
        idle_streak: 480,
        interval: Duration::from_millis(250),
        at_idle_rate: true,
    };
    // No panic + mentions both counters and the idle rate.
    log_screen_metrics(12, 480, &stats);
}
