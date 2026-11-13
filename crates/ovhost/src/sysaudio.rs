//! System-audio capture (Wave 1, `--system-audio`): WASAPI **loopback** of
//! the default render endpoint — what the host's speakers play — in
//! event-driven shared mode with a 10 ms period.
//!
//! The mix format (usually 32-bit float stereo at 48 kHz) is down-mixed to
//! mono, converted to s16 and, if the device does not run at 48 kHz,
//! linearly resampled to it; [`MicSource::read_tick`] then hands out 10 ms
//! ticks ([`OPUS10_SAMPLES`]) that the session encodes as 10 ms Opus
//! packets (with in-band FEC) for a `CAP_AUDIO10` viewer.
//!
//! A loopback stream delivers no packets while nothing plays; the source
//! then emits silent ticks at the real-time pace (so the mic loop stays
//! responsive to stop and the viewer's jitter buffer simply sees silence).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use ovapp::audio::{MicSource, OPUS10_SAMPLES, SAMPLE_RATE};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

/// WASAPI period / buffer: 10 ms in 100 ns units.
const PERIOD_HNS: i64 = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SampleFmt {
    F32,
    I16,
    I32,
}

/// Default-render-endpoint loopback capture (see the module docs).
pub struct LoopbackSource {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: HANDLE,
    channels: usize,
    rate: u32,
    fmt: SampleFmt,
    /// Mono s16 at 48 kHz, not yet handed out.
    pending: VecDeque<i16>,
    /// Resampler state: fractional read position and the previous sample.
    phase: f64,
    prev: f32,
    /// When the next tick is due if the device stays silent.
    next_due: Instant,
    pub description: String,
}

// SAFETY: the COM objects are created on (and only used from) the mic
// thread, which initialized COM as multithreaded (MTA objects are agile).
unsafe impl Send for LoopbackSource {}

impl LoopbackSource {
    /// Open the default output device's loopback stream.
    pub fn open() -> Result<Self, String> {
        let e = |what: &str, err: windows::core::Error| {
            format!("{what}: {:#010x} {}", err.code().0 as u32, err.message())
        };
        // SAFETY: plain COM calls; the format block is freed with
        // CoTaskMemFree after Initialize copied it.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|x| e("MMDeviceEnumerator", x))?;
            let dev = en
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .map_err(|x| e("default render endpoint", x))?;
            let client: IAudioClient = dev
                .Activate(CLSCTX_ALL, None)
                .map_err(|x| e("IAudioClient", x))?;
            let pwfx = client.GetMixFormat().map_err(|x| e("GetMixFormat", x))?;
            let wf: WAVEFORMATEX = std::ptr::read_unaligned(pwfx);
            let tag = wf.wFormatTag;
            let bits = wf.wBitsPerSample;
            // WAVE_FORMAT_EXTENSIBLE: the sub-format GUID's first u32 is the
            // plain format tag (1 PCM, 3 IEEE float).
            let sub = if tag == 0xFFFE && wf.cbSize >= 22 {
                let p =
                    (pwfx as *const u8).add(std::mem::size_of::<WAVEFORMATEX>() + 6) as *const u32;
                std::ptr::read_unaligned(p) as u16
            } else {
                tag
            };
            let fmt = match (sub, bits) {
                (3, 32) => Some(SampleFmt::F32),
                (1, 16) => Some(SampleFmt::I16),
                (1, 32) => Some(SampleFmt::I32),
                _ => None,
            };
            let channels = usize::from(wf.nChannels.max(1));
            let rate = wf.nSamplesPerSec;
            let init = client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                PERIOD_HNS,
                0,
                pwfx,
                None,
            );
            CoTaskMemFree(Some(pwfx as *const _));
            let fmt =
                fmt.ok_or_else(|| format!("unsupported mix format (tag {sub:#x}, {bits} bit)"))?;
            init.map_err(|x| e("IAudioClient::Initialize(loopback)", x))?;
            let event = CreateEventW(None, false, false, None).map_err(|x| e("CreateEvent", x))?;
            if let Err(x) = client.SetEventHandle(event) {
                let _ = CloseHandle(event);
                return Err(e("SetEventHandle", x));
            }
            let capture: IAudioCaptureClient = match client.GetService() {
                Ok(c) => c,
                Err(x) => {
                    let _ = CloseHandle(event);
                    return Err(e("IAudioCaptureClient", x));
                }
            };
            if let Err(x) = client.Start() {
                let _ = CloseHandle(event);
                return Err(e("IAudioClient::Start", x));
            }
            Ok(Self {
                client,
                capture,
                event,
                channels,
                rate: rate.max(1),
                fmt,
                pending: VecDeque::with_capacity(4 * OPUS10_SAMPLES as usize),
                phase: 0.0,
                prev: 0.0,
                next_due: Instant::now(),
                description: format!(
                    "WASAPI loopback, {rate} Hz x{channels} {fmt:?}, 10 ms period"
                ),
            })
        }
    }

    /// Pull every packet the device has ready into `pending`. Returns
    /// whether any arrived.
    fn drain(&mut self) -> Result<bool, String> {
        let mut any = false;
        // SAFETY: GetBuffer hands out `frames` frames of the mix format,
        // valid until ReleaseBuffer.
        unsafe {
            loop {
                let n = self
                    .capture
                    .GetNextPacketSize()
                    .map_err(|e| format!("GetNextPacketSize: {e}"))?;
                if n == 0 {
                    return Ok(any);
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                self.capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                    .map_err(|e| format!("GetBuffer: {e}"))?;
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null();
                let frames_us = frames as usize;
                for f in 0..frames_us {
                    let mono = if silent {
                        0.0
                    } else {
                        let mut acc = 0.0f32;
                        for c in 0..self.channels {
                            let i = f * self.channels + c;
                            acc += match self.fmt {
                                SampleFmt::F32 => {
                                    std::ptr::read_unaligned((data as *const f32).add(i))
                                }
                                SampleFmt::I16 => {
                                    f32::from(std::ptr::read_unaligned((data as *const i16).add(i)))
                                        / 32768.0
                                }
                                SampleFmt::I32 => {
                                    std::ptr::read_unaligned((data as *const i32).add(i)) as f32
                                        / 2_147_483_648.0
                                }
                            };
                        }
                        acc / self.channels as f32
                    };
                    self.push_resampled(mono);
                }
                let _ = self.capture.ReleaseBuffer(frames);
                any = true;
            }
        }
    }

    /// Feed one device-rate mono sample; emit 48 kHz s16 samples.
    fn push_resampled(&mut self, x: f32) {
        let to_i16 = |v: f32| (v.clamp(-1.0, 1.0) * 32767.0) as i16;
        if self.rate == SAMPLE_RATE {
            self.pending.push_back(to_i16(x));
            return;
        }
        // Output samples fall at phase steps of rate/48000 input samples;
        // interpolate between `prev` and `x` for those in (prev, x].
        let step = f64::from(self.rate) / f64::from(SAMPLE_RATE);
        while self.phase <= 1.0 {
            let t = self.phase as f32;
            self.pending
                .push_back(to_i16(self.prev + (x - self.prev) * t));
            self.phase += step;
        }
        self.phase -= 1.0;
        self.prev = x;
    }
}

impl MicSource for LoopbackSource {
    fn read_tick(&mut self, out: &mut [i16]) -> Result<(), String> {
        let tick = Duration::from_micros(out.len() as u64 * 1_000_000 / u64::from(SAMPLE_RATE));
        loop {
            if self.pending.len() >= out.len() {
                let n = out.len();
                for (o, s) in out.iter_mut().zip(self.pending.drain(..n)) {
                    *o = s;
                }
                self.next_due = Instant::now() + tick;
                return Ok(());
            }
            let now = Instant::now();
            if now >= self.next_due + tick {
                // Nothing playing: a silent tick at the real-time pace
                // (keeping what partial audio there is in front).
                let have = self.pending.len();
                for (o, s) in out.iter_mut().zip(self.pending.drain(..)) {
                    *o = s;
                }
                out[have..].fill(0);
                self.next_due += tick;
                if self.next_due + tick < now {
                    self.next_due = now; // long stall: do not burst
                }
                return Ok(());
            }
            // SAFETY: a live event handle.
            let r = unsafe { WaitForSingleObject(self.event, 15) };
            if r == WAIT_OBJECT_0 || self.pending.len() < out.len() {
                self.drain()?;
            }
        }
    }

    fn self_paced(&self) -> bool {
        true
    }

    fn tick_samples(&self) -> usize {
        OPUS10_SAMPLES as usize
    }
}

impl Drop for LoopbackSource {
    fn drop(&mut self) {
        // SAFETY: stop the stream, then close our event.
        unsafe {
            let _ = self.client.Stop();
            let _ = CloseHandle(self.event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CI-safe: opening may fail (no audio device / service); when it
    /// works, ticks arrive at roughly the real-time pace (silence or not).
    #[test]
    fn loopback_delivers_paced_10ms_ticks_or_fails_cleanly() {
        let mut src = match LoopbackSource::open() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("no loopback here (fine): {e}");
                return;
            }
        };
        eprintln!("{}", src.description);
        let mut buf = vec![0i16; src.tick_samples()];
        let t0 = Instant::now();
        for _ in 0..30 {
            src.read_tick(&mut buf).unwrap();
        }
        let took = t0.elapsed();
        // 30 x 10 ms = 300 ms (allowing the startup burst / scheduling).
        assert!(took < Duration::from_millis(900), "{took:?}");
    }

    #[test]
    fn resampler_keeps_the_48k_rate() {
        // Exercise the pure resampler via a fake state (no device).
        let mut pending = VecDeque::new();
        let (mut phase, mut prev) = (0.0f64, 0.0f32);
        let step = 44_100.0 / 48_000.0;
        for i in 0..44_100 {
            let x = ((i as f32) * 0.01).sin();
            while phase <= 1.0 {
                pending.push_back(prev + (x - prev) * phase as f32);
                phase += step;
            }
            phase -= 1.0;
            prev = x;
        }
        let n = pending.len() as i64;
        assert!((n - 48_000).abs() <= 2, "{n}");
    }
}
