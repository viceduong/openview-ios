//! Playback-side buffering: the zero-filling ring, the jitter buffer and
//! the shared playback bus.

use super::{SAMPLE_RATE, TICK_SAMPLES};

/// A fixed-capacity FIFO of audio samples with **zero-fill on underrun**.
///
/// This is the viewer-side jitter buffer: the speaker callback pops a fixed
/// tick regardless of how much arrived, and a starved pop returns silence
/// and counts the underrun so the UI can show audio health. Pushes beyond
/// capacity drop the *oldest* samples (a live stream is worth more than a
/// buffered one).
pub struct AudioRingBuffer {
    buf: std::collections::VecDeque<i16>,
    capacity: usize,
    underruns: u64,
}

impl AudioRingBuffer {
    /// A buffer holding `capacity` samples (~`capacity / 48000` seconds).
    pub fn new(capacity: usize) -> Self {
        Self {
            buf: std::collections::VecDeque::with_capacity(capacity.max(1)),
            capacity: capacity.max(1),
            underruns: 0,
        }
    }

    /// Push samples, dropping the oldest when full.
    pub fn push(&mut self, samples: &[i16]) {
        for &s in samples {
            if self.buf.len() == self.capacity {
                self.buf.pop_front();
            }
            self.buf.push_back(s);
        }
    }

    /// Pop exactly `out.len()` samples, zero-filling (and counting) any
    /// shortfall so playback timing never slips.
    pub fn pop(&mut self, out: &mut [i16]) {
        let have = self.buf.len().min(out.len());
        for (o, s) in out.iter_mut().zip(self.buf.drain(..have)) {
            *o = s;
        }
        if have < out.len() {
            out[have..].fill(0);
            self.underruns += 1;
        }
    }

    /// Samples currently buffered.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether the buffer holds no samples.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// How many pops have been starved since creation.
    pub fn underruns(&self) -> u64 {
        self.underruns
    }
}

/// Playback jitter buffer: absorbs network jitter between the
/// sender's capture clock and the local device clock with bounded latency.
///
/// - **Prefill**: after start (and after every underrun) it outputs silence
///   until [`target`](Self::target) samples are buffered, so playback
///   starts with a cushion instead of stuttering tick by tick.
/// - **Bounded depth**: when a burst pushes the depth past
///   [`max`](Self::max), the oldest audio is dropped back down to the
///   target — latency can never ratchet up behind a stall.
/// - **Underrun**: a pop that cannot be fully served plays what is there,
///   zero-fills the rest, counts it, and re-enters prefill.
pub struct JitterBuffer {
    buf: std::collections::VecDeque<i16>,
    target: usize,
    max: usize,
    primed: bool,
    underruns: u64,
    dropped: u64,
}

/// Default jitter-buffer target depth (60 ms).
pub(crate) const JITTER_TARGET_MS: u32 = 60;
/// Default jitter-buffer ceiling (120 ms): beyond it the oldest is dropped.
pub(crate) const JITTER_MAX_MS: u32 = 120;

impl Default for JitterBuffer {
    fn default() -> Self {
        Self::with_ms(JITTER_TARGET_MS, JITTER_MAX_MS)
    }
}

impl JitterBuffer {
    /// A buffer with `target_ms` prefill and a `max_ms` ceiling.
    pub fn with_ms(target_ms: u32, max_ms: u32) -> Self {
        let per_ms = (SAMPLE_RATE / 1000) as usize;
        let target = (target_ms as usize * per_ms).max(1);
        let max = (max_ms as usize * per_ms).max(target);
        Self {
            buf: std::collections::VecDeque::with_capacity(max + TICK_SAMPLES as usize),
            target,
            max,
            primed: false,
            underruns: 0,
            dropped: 0,
        }
    }

    /// Append received samples; trims back to the target past the ceiling.
    pub fn push(&mut self, samples: &[i16]) {
        self.buf.extend(samples.iter().copied());
        if self.buf.len() > self.max {
            let drop = self.buf.len() - self.target;
            self.buf.drain(..drop);
            self.dropped += drop as u64;
        }
    }

    /// Fill `out` with the next samples (silence while prefilling; partial
    /// + zero-fill on underrun). Returns `true` when real audio was played.
    pub fn pop(&mut self, out: &mut [i16]) -> bool {
        if !self.primed {
            if self.buf.len() < self.target {
                out.fill(0);
                return false;
            }
            self.primed = true;
        }
        let have = self.buf.len().min(out.len());
        for (o, s) in out.iter_mut().zip(self.buf.drain(..have)) {
            *o = s;
        }
        if have < out.len() {
            out[have..].fill(0);
            self.underruns += 1;
            self.primed = false;
        }
        have > 0
    }

    /// Discard everything and re-enter prefill (playback toggled off).
    pub fn clear(&mut self) {
        self.buf.clear();
        self.primed = false;
    }

    /// Samples buffered.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether nothing is buffered.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Buffered depth in milliseconds.
    pub fn depth_ms(&self) -> u32 {
        (self.buf.len() as u64 * 1000 / u64::from(SAMPLE_RATE)) as u32
    }

    /// Prefill target (samples).
    pub fn target(&self) -> usize {
        self.target
    }

    /// Ceiling (samples).
    pub fn max(&self) -> usize {
        self.max
    }

    /// Pops that ran dry after playback had started.
    pub fn underruns(&self) -> u64 {
        self.underruns
    }

    /// Samples dropped by the ceiling.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

/// Shared playback bus: the command hook pushes decoded ticks into the
/// [`JitterBuffer`], the device thread pops one tick per free device buffer.
/// Disabled (the default) it drops pushes, so a muted/off speaker never
/// accumulates stale audio.
#[derive(Clone)]
pub struct AudioBus {
    buf: std::sync::Arc<std::sync::Mutex<JitterBuffer>>,
    enabled: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Default for AudioBus {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioBus {
    /// A disabled bus with the default 60 ms / 120 ms jitter buffer.
    pub fn new() -> Self {
        Self::with_ms(JITTER_TARGET_MS, JITTER_MAX_MS)
    }

    /// A disabled bus with a `target_ms` prefill / `max_ms`
    /// ceiling jitter buffer (the desktop viewer uses 30 / 90 ms).
    pub fn with_ms(target_ms: u32, max_ms: u32) -> Self {
        Self {
            buf: std::sync::Arc::new(std::sync::Mutex::new(JitterBuffer::with_ms(
                target_ms, max_ms,
            ))),
            enabled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Enable/disable playback; disabling discards buffered audio.
    pub fn set_enabled(&self, on: bool) {
        let was = self.enabled.swap(on, std::sync::atomic::Ordering::Relaxed);
        if was && !on {
            if let Ok(mut b) = self.buf.lock() {
                b.clear();
            }
        }
    }

    /// Whether playback is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Push decoded samples (called from the command hook); dropped while
    /// disabled.
    pub fn push(&self, samples: &[i16]) {
        if self.is_enabled() {
            if let Ok(mut b) = self.buf.lock() {
                b.push(samples);
            }
        }
    }

    /// Pop one device buffer's worth (see [`JitterBuffer::pop`]).
    pub fn pop_tick(&self, out: &mut [i16]) -> bool {
        match self.buf.lock() {
            Ok(mut b) => b.pop(out),
            Err(_) => {
                out.fill(0);
                false
            }
        }
    }

    /// `(depth_ms, underruns, dropped_samples)` for logs / the HUD.
    pub fn status(&self) -> (u32, u64, u64) {
        self.buf
            .lock()
            .map(|b| (b.depth_ms(), b.underruns(), b.dropped()))
            .unwrap_or_default()
    }
}
