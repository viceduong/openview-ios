//! Microphone capture for full-duplex audio.
//!
//! The viewer captures its own microphone in 20 ms ticks on a dedicated
//! thread ([`ovapp::audio::run_mic_loop`]); the viewer loop encodes each
//! tick (Opus for a host that advertised `CAP_OPUS`, raw PCM otherwise)
//! into an `AUDIO` command. The sources are shared with `ovhost::mic`: the
//! WinMM capture ([`ovapp::winaudio::WinmmMic`]) is event-driven over a ring
//! of 10 x 20 ms buffers and delivers every buffer; the synthetic tone
//! (`--audio-synth`) is paced by a drift-free [`ovapp::audio::TickClock`].

use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};

use tracing::{debug, warn};

use ovapp::audio::{MicSource, SynthMic};
#[cfg(windows)]
use ovapp::winaudio::WinmmMic;

/// Which microphone source `--mic` / `--audio-synth` selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MicKind {
    /// The real microphone (WinMM `waveIn`, default input device).
    Real,
    /// A 440 Hz synthetic sine — hardware-free demos and tests.
    Synth,
}

/// Open the mic on its own thread and stream its PCM ticks (one per 20 ms)
/// over a channel, waking the viewer loop for each; the loop encodes them
/// (Opus or raw, per the host's CAPS). The real mic paces itself on its
/// buffer-completion event and delivers every buffer; the synthetic tone is
/// paced by a drift-free clock. `None` when the mic cannot be opened
/// (capture disabled for the session, never a crash).
pub(crate) fn spawn(
    kind: MicKind,
    waker: ovapp::netwait::Waker,
    stop: Arc<AtomicBool>,
) -> Option<mpsc::Receiver<Vec<i16>>> {
    let (tx, rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel::<bool>();
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
                    warn!("mic unavailable, mic capture disabled: {e}");
                    let _ = ready_tx.send(false);
                    return;
                }
            },
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
        debug!("mic streaming on (20 ms ticks)");
        Some(rx)
    } else {
        None
    }
}
