//! Microphone capture (v0.21; Phase 4 rework).
//!
//! The host reads the microphone in 20 ms ticks ([`ovapp::audio::
//! TICK_SAMPLES`] at 48 kHz mono s16le) on its own thread
//! ([`ovapp::audio::run_mic_loop`]) and hands each tick to the session
//! thread, which encodes it (Opus for a viewer with `CAP_OPUS`, raw PCM
//! otherwise) into an `AUDIO` command.
//!
//! Phase 4 fixed two loss sources of the v0.21 capture, both now shared
//! with `ovview` in [`ovapp::winaudio`] / [`ovapp::audio`]:
//!
//! - the WinMM `waveIn` ring grew from 4 to 10 buffers and is drained on the
//!   driver's completion **event** (not a 2 ms sleep-poll of a non-volatile
//!   flag), with headers re-queued as still-prepared — every captured
//!   buffer is delivered in order;
//! - the synthetic mic is paced by a drift-free [`ovapp::audio::TickClock`]
//!   (`next += interval`) instead of re-arming from "now", which lost the
//!   wake-up latency on every tick.

pub use ovapp::audio::{MicSource, SynthMic};
#[cfg(windows)]
pub use ovapp::winaudio::WinmmMic;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synth_tick_is_bounded_and_nonzero() {
        let mut mic = SynthMic::new();
        let mut buf = vec![0i16; ovapp::audio::TICK_SAMPLES as usize];
        mic.read_tick(&mut buf).unwrap();
        assert!(buf.iter().all(|&s| s.abs() <= 8_000));
        assert!(buf.iter().any(|&s| s != 0));
        assert!(!mic.self_paced(), "the synthetic mic is clock-paced");
    }
}
