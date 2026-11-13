//! Speaker playback (full-duplex audio).
//!
//! The viewer's mic arrives as AUDIO (0x40) commands on the host's command
//! channel; the hook decodes each tick (raw PCM or Opus) into a shared
//! [`AudioBus`] and a dedicated playback thread drains it into the default
//! output device. The two ends play ONLY what they receive, so there is no
//! local feedback loop.
//!
//! The bus is a [`ovapp::audio::JitterBuffer`] (60 ms prefill, 120 ms
//! ceiling) and the device is [`ovapp::winaudio::WinmmSpeaker`] — three
//! rotating 20 ms `waveOut` headers refilled on the driver's completion
//! event. The playback thread is joined when the session ends (see
//! `HostHooks`' `Drop`), so neither the thread nor the device outlive a
//! session.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

pub(crate) use ovapp::audio::AudioBus;

/// Spawn the playback thread for `bus`. It runs until `stop` is set; join
/// the returned handle to release the device. `None` off Windows (no
/// device) or when the thread cannot start.
pub(crate) fn spawn(bus: AudioBus, stop: Arc<AtomicBool>) -> Option<std::thread::JoinHandle<()>> {
    #[cfg(windows)]
    {
        ovapp::winaudio::spawn_playback(bus, stop, "ovhost-audio")
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
        bus.push(&vec![1; TICK_SAMPLES as usize]);
        assert_eq!(bus.status().0, 20);
    }

    #[test]
    fn playback_thread_exits_on_stop() {
        let bus = AudioBus::new();
        bus.set_enabled(true);
        let stop = Arc::new(AtomicBool::new(false));
        let handle = spawn(bus, Arc::clone(&stop));
        std::thread::sleep(std::time::Duration::from_millis(60));
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = handle {
            // Bounded by the 50 ms device wait (or immediate without a device).
            let t0 = std::time::Instant::now();
            h.join().unwrap();
            assert!(t0.elapsed() < std::time::Duration::from_secs(2));
        }
    }
}
