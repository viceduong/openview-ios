//! Stereo system audio: the channel helpers and the link-rate gate that
//! squeezes a stereo stream to mono on a thin link.
//!
//! A stereo stream is interleaved `L R L R …` s16 at [`super::SAMPLE_RATE`];
//! the AUDIO header's sample count stays **per channel** (480 for a 10 ms
//! packet, stereo or not). Stereo is only ever Opus: the sender encodes
//! two channels only for a peer that advertised
//! [`crate::terminal::CAP_STEREO`] (and [`crate::terminal::CAP_OPUS`]), and
//! an Opus packet describes its own channel count (TOC `s` bit), so the
//! receiver needs no header change — libopus (and Apple's decoder) decode
//! a mono packet in a stereo stream and vice versa.

use std::time::{Duration, Instant};

/// Opus bitrate (bits/s, VBR) of a stereo stream on a healthy link.
pub const OPUS_STEREO_BITRATE: i32 = 128_000;

/// Opus bitrate of a stereo stream squeezed to mono on a thin link
/// ([`StereoGate`]).
pub const OPUS_SQUEEZED_BITRATE: i32 = 64_000;

/// Link rate (bytes/s) below which a stereo stream drops to mono at
/// [`OPUS_SQUEEZED_BITRATE`]: 1 Mbit/s.
pub const STEREO_MIN_LINK_RATE: u64 = 125_000;

/// Link rate (bytes/s) a squeezed stream must hold for
/// [`STEREO_RESUME_AFTER`] before it goes stereo again: 1.5 Mbit/s (the gap
/// to [`STEREO_MIN_LINK_RATE`] is hysteresis).
pub const STEREO_RESUME_LINK_RATE: u64 = 187_500;

/// How long the link must stay at [`STEREO_RESUME_LINK_RATE`] or above
/// before a squeezed stream returns to stereo.
pub const STEREO_RESUME_AFTER: Duration = Duration::from_secs(5);

/// Whether a stereo stream should be squeezed to mono / 64 kbps, from the
/// transport's rate estimate (the direct path's congestion-controller target,
/// or the link pacer's delivery rate on the relay).
///
/// Drops to mono at once when the rate falls below
/// [`STEREO_MIN_LINK_RATE`]; returns to stereo only after the rate held
/// [`STEREO_RESUME_LINK_RATE`] for [`STEREO_RESUME_AFTER`], so a link near
/// the threshold does not flap. An unknown rate (`0`) changes nothing.
#[derive(Debug, Clone, Default)]
pub struct StereoGate {
    squeezed: bool,
    good_since: Option<Instant>,
}

impl StereoGate {
    /// A gate that starts unsqueezed (stereo).
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed the current link rate (bytes/s, `0` = unknown); returns whether
    /// the stream is squeezed to mono now.
    pub fn update(&mut self, link_rate: u64, now: Instant) -> bool {
        if link_rate == 0 {
            self.good_since = None;
            return self.squeezed;
        }
        if !self.squeezed {
            if link_rate < STEREO_MIN_LINK_RATE {
                self.squeezed = true;
                self.good_since = None;
            }
        } else if link_rate >= STEREO_RESUME_LINK_RATE {
            let since = *self.good_since.get_or_insert(now);
            if now.saturating_duration_since(since) >= STEREO_RESUME_AFTER {
                self.squeezed = false;
                self.good_since = None;
            }
        } else {
            self.good_since = None;
        }
        self.squeezed
    }

    /// Whether the stream is squeezed to mono.
    pub fn squeezed(&self) -> bool {
        self.squeezed
    }
}

/// Average interleaved stereo down to mono (`(L + R) / 2`, no clipping).
/// A trailing odd sample is dropped.
pub fn downmix_stereo(interleaved: &[i16]) -> Vec<i16> {
    let (pairs, _) = interleaved.as_chunks::<2>();
    pairs
        .iter()
        .map(|&[l, r]| ((i32::from(l) + i32::from(r)) / 2) as i16)
        .collect()
}

/// Duplicate mono into interleaved stereo (`L = R`).
pub fn upmix_mono(mono: &[i16]) -> Vec<i16> {
    mono.iter().flat_map(|&s| [s, s]).collect()
}
