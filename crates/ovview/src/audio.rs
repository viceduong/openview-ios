//! Speaker playback.
//!
//! The host's `AUDIO` commands (raw PCM, or Opus once our CAPS advertised
//! `CAP_OPUS`) are decoded by the command hook into a shared [`AudioBus`];
//! a dedicated playback thread drains it into the default output device.
//!
//! The bus is a [`ovapp::audio::JitterBuffer`] (prefill target and ceiling
//! set by the session; oldest audio dropped past the ceiling, re-prefill
//! after an underrun), and the device is [`ovapp::winaudio::WinmmSpeaker`]:
//! three rotating 20 ms `waveOut` headers refilled on the driver's
//! completion event, so the device never idles between buffers. Shared with
//! `ovhost::speaker`.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use ovapp::audio::AudioBus;

/// Spawn the playback thread for `bus`; it runs until `stop` is set (join
/// the handle to release the device). `None` off Windows.
pub(crate) fn spawn(bus: AudioBus, stop: Arc<AtomicBool>) -> Option<std::thread::JoinHandle<()>> {
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
