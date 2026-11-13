//! Pure frame→image helpers (v0.20) — no platform calls, fully unit-tested.
//!
//! Two concerns live here:
//!
//! - [`bgra_to_ppm`]: encode one BGRA frame as a binary PPM (P6, 8-bit RGB)
//!   file. PPM was chosen over PGM because the frames are color (BGRA); P6
//!   is a three-line header + raw RGB, trivially byte-verifiable in tests and
//!   readable by every image tool. BGRA quads are interpreted as
//!   `byte0=B, byte1=G, byte2=R, byte3=A` and emitted as `R,G,B`.
//! - [`frame_pacing`]: the per-frame display delay that reproduces the
//!   recording's original variable-rate timing (the stream is change-detected,
//!   so inter-frame gaps vary).

use crate::record::RecordedFrame;
use std::time::Duration;

/// Encode one BGRA frame as binary PPM (P6, 8-bit RGB) bytes.
///
/// `bgra` must be exactly `width * height * 4` bytes (row-major, no padding).
/// On any other length this returns `Err` — it never panics. The alpha byte
/// is dropped (PPM has no alpha channel).
pub fn bgra_to_ppm(width: u32, height: u32, bgra: &[u8]) -> Result<Vec<u8>, String> {
    let expected = (width as u64) * (height as u64) * 4;
    if bgra.len() as u64 != expected {
        return Err(format!(
            "bgra_to_ppm: pixel buffer is {} bytes, expected {expected} \
             (width {width} x height {height} x 4)",
            bgra.len()
        ));
    }
    let (w, h) = (width as usize, height as usize);
    let mut out = Vec::with_capacity(16 + bgra.len());
    out.extend_from_slice(format!("P6\n{w} {h}\n255\n").as_bytes());
    for px in bgra.chunks_exact(4) {
        // BGRA quad: [0]=B, [1]=G, [2]=R, [3]=A -> emit R,G,B.
        out.extend_from_slice(&[px[2], px[1], px[0]]);
    }
    Ok(out)
}

/// Per-frame display delay for a ts_ms-ordered frame sequence.
///
/// Entry `i` is the delay to wait *before* presenting frame `i`: frame 0 is
/// shown immediately (`Duration::ZERO`), frame `i` waits
/// `frames[i].ts_ms - frames[i-1].ts_ms`. Out-of-order timestamps are clamped
/// to zero (a recording must be ordered, but the player must not sleep on a
/// negative duration if one ever slips through).
pub fn frame_pacing(frames: &[RecordedFrame]) -> Vec<Duration> {
    frames
        .iter()
        .enumerate()
        .map(|(i, f)| {
            if i == 0 {
                Duration::ZERO
            } else {
                let delta = f.ts_ms.saturating_sub(frames[i - 1].ts_ms);
                Duration::from_millis(delta)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ppm_header_and_known_pixels() {
        // 2x1 frame: pixel 0 = BGRA (10, 20, 30, 255) -> RGB (30, 20, 10);
        // pixel 1 = BGRA (0, 0, 255, 128) -> RGB (255, 0, 0).
        let bgra = [10u8, 20, 30, 255, 0, 0, 255, 128];
        let ppm = bgra_to_ppm(2, 1, &bgra).unwrap();
        assert_eq!(&ppm[..11], b"P6\n2 1\n255\n");
        assert_eq!(&ppm[11..], &[30, 20, 10, 255, 0, 0]);
        // Header is exactly "P6\n2 1\n255\n" (11 bytes), then raw RGB.
        assert_eq!(&ppm[..11], b"P6\n2 1\n255\n");
        assert_eq!(ppm.len(), 11 + 6);
    }

    #[test]
    fn ppm_rejects_wrong_buffer_length() {
        assert!(bgra_to_ppm(4, 4, &[0u8; 63]).is_err());
        assert!(bgra_to_ppm(4, 4, &[0u8; 65]).is_err());
        assert!(bgra_to_ppm(0, 0, &[]).is_ok());
    }

    #[test]
    fn ppm_zero_sized_frame_is_header_only() {
        let ppm = bgra_to_ppm(0, 0, &[]).unwrap();
        assert_eq!(ppm, b"P6\n0 0\n255\n");
    }

    fn frame(seq: u32, ts_ms: u64) -> RecordedFrame {
        RecordedFrame {
            seq,
            ts_ms,
            pixels: vec![0; 4],
            width: 1,
            height: 1,
        }
    }

    #[test]
    fn pacing_follows_timestamp_deltas() {
        let frames = [frame(1, 0), frame(2, 33), frame(3, 100), frame(4, 100)];
        let pacing = frame_pacing(&frames);
        assert_eq!(
            pacing,
            vec![
                Duration::ZERO,
                Duration::from_millis(33),
                Duration::from_millis(67),
                Duration::ZERO,
            ]
        );
    }

    #[test]
    fn pacing_clamps_out_of_order_timestamps_to_zero() {
        let frames = [frame(1, 50), frame(2, 40), frame(3, 90)];
        let pacing = frame_pacing(&frames);
        assert_eq!(
            pacing,
            vec![
                Duration::ZERO,
                Duration::ZERO, // 40 < 50: clamped, never negative
                Duration::from_millis(50),
            ]
        );
    }

    #[test]
    fn pacing_of_empty_and_single() {
        assert!(frame_pacing(&[]).is_empty());
        assert_eq!(frame_pacing(&[frame(1, 999)]), vec![Duration::ZERO]);
    }
}
