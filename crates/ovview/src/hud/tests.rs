//! HUD line formatting tests.

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
