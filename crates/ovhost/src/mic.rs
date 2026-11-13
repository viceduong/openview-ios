//! Audio streamed to the viewer: the microphone, the system audio
//! (loopback) or a synthetic tone.
//!
//! The host reads the source in 20 ms ticks ([`ovapp::audio::
//! TICK_SAMPLES`] at 48 kHz mono s16le) on its own thread
//! ([`ovapp::audio::run_mic_loop`]) and hands each tick to the session
//! thread, which encodes it (Opus for a viewer with `CAP_OPUS`, raw PCM
//! otherwise) into an `AUDIO` command.
//!
//! The capture code is shared with `ovview` in [`ovapp::winaudio`] /
//! [`ovapp::audio`]:
//!
//! - the WinMM `waveIn` ring (10 buffers) is drained on the driver's
//!   completion **event**, with headers re-queued as still-prepared —
//!   every captured buffer is delivered in order;
//! - the synthetic mic is paced by a drift-free [`ovapp::audio::TickClock`]
//!   (`next += interval`), so wake-up latency never accumulates.

use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Receiver;
use std::sync::Arc;

use tracing::{debug, warn};

use ovapp::audio::{MicSource, SynthMic};
#[cfg(windows)]
use ovapp::winaudio::WinmmMic;

/// Which audio source is streamed to the viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MicKind {
    /// The real microphone (WinMM `waveIn`, default input device).
    Real,
    /// A 440 Hz synthetic sine — hardware-free demos and tests.
    Synth,
    /// What the host's speakers play (WASAPI loopback of the default render
    /// device, 10 ms ticks).
    System,
}

/// Open the source on its own thread and stream its PCM ticks (one per
/// 20 ms) over a channel, waking the session loop for each; the session
/// thread encodes them (Opus or raw, per the viewer's CAPS). The real mic
/// paces itself on its buffer-completion event and delivers every buffer;
/// the synthetic one is paced by a drift-free clock. `None` when the source
/// cannot be opened.
pub(crate) fn spawn_mic(
    kind: MicKind,
    waker: ovapp::netwait::Waker,
    stop: Arc<AtomicBool>,
) -> Option<Receiver<Vec<i16>>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<bool>();
    std::thread::spawn(move || {
        let mut src: Box<dyn MicSource> = match kind {
            MicKind::Synth => Box::new(SynthMic::new()),
            #[cfg(windows)]
            MicKind::Real => match WinmmMic::open() {
                Ok(m) => {
                    debug!(
                        "mic open (WinMM waveIn, 48 kHz mono, {} x 20 ms buffers)",
                        ovapp::winaudio::MIC_BUFFERS
                    );
                    Box::new(m)
                }
                Err(e) => {
                    warn!("mic unavailable, audio disabled: {e}");
                    let _ = ready_tx.send(false);
                    return;
                }
            },
            #[cfg(windows)]
            MicKind::System => match crate::sysaudio::LoopbackSource::open() {
                Ok(m) => {
                    debug!("system audio open ({})", m.description);
                    Box::new(m)
                }
                Err(e) => {
                    warn!("system audio unavailable, audio disabled: {e}");
                    let _ = ready_tx.send(false);
                    return;
                }
            },
            #[cfg(not(windows))]
            MicKind::System => {
                warn!("no system-audio capture on this platform");
                let _ = ready_tx.send(false);
                return;
            }
            #[cfg(not(windows))]
            MicKind::Real => {
                warn!("no microphone support on this platform");
                let _ = ready_tx.send(false);
                return;
            }
        };
        let _ = ready_tx.send(true);
        ovapp::audio::run_mic_loop(src.as_mut(), &stop, |pcm| {
            if tx.send(pcm).is_err() {
                return false; // session gone
            }
            waker.wake();
            true
        });
    });
    if ready_rx.recv().unwrap_or(false) {
        debug!("audio streaming on (10 ms Opus packets to a CAP_AUDIO10 viewer, 20 ms otherwise)");
        Some(rx)
    } else {
        None
    }
}

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
