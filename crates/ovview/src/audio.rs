//! Speaker playback (v0.21; Phase 4 rework).
//!
//! The host's `AUDIO` commands (raw PCM, or Opus once our CAPS advertised
//! `CAP_OPUS`) are decoded by the command hook into a shared [`AudioBus`];
//! a dedicated playback thread drains it into the default output device.
//!
//! Phase 4: the bus is a [`ovapp::audio::JitterBuffer`] (60 ms prefill
//! target, oldest audio dropped past 120 ms, re-prefill after an underrun)
//! instead of a 400 ms drop-oldest ring, and the device is
//! [`ovapp::winaudio::WinmmSpeaker`]: three rotating 20 ms `waveOut`
//! headers refilled on the driver's completion event, replacing one header
//! re-written after a `sleep(2)` poll (which left the device idle between
//! buffers). Shared with `ovhost::speaker`.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

pub use ovapp::audio::AudioBus;

/// Spawn the playback thread for `bus`; it runs until `stop` is set (join
/// the handle to release the device). `None` off Windows.
pub fn spawn(bus: AudioBus, stop: Arc<AtomicBool>) -> Option<std::thread::JoinHandle<()>> {
    #[cfg(windows)]
    {
        ovapp::winaudio::spawn_playback(bus, stop, "ovview-audio")
    }
    #[cfg(not(windows))]
    {
        let _ = (bus, stop);
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ovapp::audio::TICK_SAMPLES;

    #[test]
    fn bus_pushes_only_when_enabled() {
        let bus = AudioBus::new();
        bus.push(&[1, 2, 3]);
        assert_eq!(bus.status().0, 0, "disabled bus drops audio");
        bus.set_enabled(true);
        bus.push(&vec![1; TICK_SAMPLES as usize * 2]);
        assert_eq!(bus.status().0, 40);
    }
}
