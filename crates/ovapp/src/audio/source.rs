//! Capture-side sources: the drift-free tick clock, the [`MicSource`] trait,
//! the synthetic mic, the capture loop and the sine generator.

use super::{SAMPLE_RATE, TICK_SAMPLES};

/// Drift-free tick clock for a clock-paced audio source (the synthetic
/// mic): deadlines advance by exactly one interval (`next += interval`), so
/// timer jitter never accumulates into a lower tick rate. Only after a stall
/// longer than ten intervals does it resynchronise to "now" (dropping the
/// stale backlog instead of bursting it).
#[derive(Debug, Clone)]
pub struct TickClock {
    interval: std::time::Duration,
    next: std::time::Instant,
    max_behind: std::time::Duration,
}

impl TickClock {
    /// A clock whose first tick is due at `now`.
    pub fn new(interval: std::time::Duration, now: std::time::Instant) -> Self {
        Self {
            interval,
            next: now,
            max_behind: interval * 10,
        }
    }

    /// Time until the next tick (zero when due).
    pub fn until_due(&self, now: std::time::Instant) -> std::time::Duration {
        self.next.saturating_duration_since(now)
    }

    /// Whether a tick is due at `now`; if so, schedules the next one exactly
    /// one interval later (catching up tick by tick after a short delay).
    pub fn tick(&mut self, now: std::time::Instant) -> bool {
        if now < self.next {
            return false;
        }
        if now.saturating_duration_since(self.next) > self.max_behind {
            self.next = now;
        }
        self.next += self.interval;
        true
    }
}

/// Where microphone ticks come from: fill `out` with the next 20 ms tick
/// ([`TICK_SAMPLES`] samples of mono s16le 48 kHz audio). A real device
/// blocks until the tick has been captured; a synthetic source returns
/// immediately and is paced by a [`TickClock`].
pub trait MicSource: Send {
    /// Read the next tick into `out` (always exactly [`TICK_SAMPLES`]).
    fn read_tick(&mut self, out: &mut [i16]) -> Result<(), String>;
    /// `true` when `read_tick` paces itself (a capture device).
    fn self_paced(&self) -> bool {
        false
    }
    /// Samples per tick this source delivers ([`TICK_SAMPLES`] =
    /// 20 ms by default; the system-audio loopback uses 10 ms).
    fn tick_samples(&self) -> usize {
        TICK_SAMPLES as usize
    }
}

/// A synthetic microphone: a 440 Hz sine at moderate amplitude. Lets the
/// full audio path run on machines with no mic (and gives tests a
/// deterministic signal).
pub struct SynthMic {
    gen: SineGen,
}

impl SynthMic {
    /// A 440 Hz synthetic mic.
    pub fn new() -> Self {
        Self {
            gen: SineGen::new(440.0, 8_000),
        }
    }
}

impl Default for SynthMic {
    fn default() -> Self {
        Self::new()
    }
}

impl MicSource for SynthMic {
    fn read_tick(&mut self, out: &mut [i16]) -> Result<(), String> {
        self.gen.tick(out);
        Ok(())
    }
}

/// Run a mic capture loop on the current thread until `stop` (or until
/// `deliver` returns `false`): every captured tick is handed to `deliver`
/// in order. A self-paced device is read back to back (it blocks per tick,
/// and its buffer ring absorbs scheduling hiccups — nothing is dropped); a
/// synthetic source is paced by a drift-free [`TickClock`].
pub fn run_mic_loop(
    src: &mut dyn MicSource,
    stop: &std::sync::atomic::AtomicBool,
    mut deliver: impl FnMut(Vec<i16>) -> bool,
) {
    use std::sync::atomic::Ordering;
    let tick = std::time::Duration::from_micros(
        src.tick_samples() as u64 * 1_000_000 / u64::from(SAMPLE_RATE),
    );
    let mut clock = TickClock::new(tick, std::time::Instant::now());
    // A failing device fails every tick: warn once, then only at debug.
    let mut warned = false;
    while !stop.load(Ordering::Relaxed) {
        if !src.self_paced() {
            let now = std::time::Instant::now();
            if !clock.tick(now) {
                std::thread::sleep(clock.until_due(now));
                continue;
            }
        }
        let mut buf = vec![0i16; src.tick_samples()];
        match src.read_tick(&mut buf) {
            Ok(()) => {
                if !deliver(buf) {
                    return;
                }
            }
            Err(e) => {
                if warned {
                    tracing::debug!("mic read failed: {e}");
                } else {
                    tracing::warn!("mic read failed: {e}");
                    warned = true;
                }
                std::thread::sleep(tick);
            }
        }
    }
}

/// A pure sine-wave generator standing in for a microphone.
///
/// Used by the synthetic mic source (tests, `--mic-synth`) so the whole
/// audio path is exercisable without any audio hardware.
pub struct SineGen {
    freq_hz: f32,
    amplitude: i16,
    phase: f32,
}

impl SineGen {
    /// A generator at `freq_hz` with peak amplitude `amplitude`.
    pub fn new(freq_hz: f32, amplitude: i16) -> Self {
        Self {
            freq_hz,
            amplitude,
            phase: 0.0,
        }
    }

    /// Produce the next `out.len()` samples, advancing the phase.
    pub fn tick(&mut self, out: &mut [i16]) {
        let step = 2.0 * std::f32::consts::PI * self.freq_hz / SAMPLE_RATE as f32;
        for slot in out.iter_mut() {
            *slot = (self.phase.sin() * self.amplitude as f32) as i16;
            self.phase += step;
        }
    }
}
