//! Speaker playback.
//!
//! The host's `AUDIO` commands (raw PCM, or Opus once our CAPS advertised
//! `CAP_OPUS`; stereo Opus system audio once they advertised `CAP_STEREO`)
//! are decoded by the command hook into a shared **stereo** [`AudioBus`]
//! (mono payloads play as L = R); a dedicated playback thread drains it
//! into the default output device, opened with 2 channels.
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

    #[test]
    fn stereo_bus_measures_depth_in_audio_time() {
        let bus = AudioBus::with_ms_channels(30, 90, 2);
        assert_eq!(bus.channels(), 2);
        bus.set_enabled(true);
        // 20 ms of interleaved stereo = 2 x 960 samples.
        bus.push(&vec![1; TICK_SAMPLES as usize * 2]);
        assert_eq!(bus.status().0, 20);
    }
}
