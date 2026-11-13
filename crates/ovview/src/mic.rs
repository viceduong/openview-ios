//! Microphone capture (v0.23 full-duplex audio; Phase 4 rework).
//!
//! The viewer captures its own microphone in 20 ms ticks on a dedicated
//! thread ([`ovapp::audio::run_mic_loop`]); the viewer loop encodes each
//! tick (Opus for a host that advertised `CAP_OPUS`, raw PCM otherwise)
//! into an `AUDIO` command. Shared with `ovhost::mic` since Phase 4: the
//! WinMM capture ([`ovapp::winaudio::WinmmMic`]) is event-driven over a ring
//! of 10 x 20 ms buffers and delivers every buffer; the synthetic tone
//! (`--audio-synth`) is paced by a drift-free [`ovapp::audio::TickClock`].

pub use ovapp::audio::{MicSource, SynthMic};
#[cfg(windows)]
pub use ovapp::winaudio::WinmmMic;
