//! System-audio capture (`--system-audio`): WASAPI **loopback** of
//! the default render endpoint — what the host's speakers play — in
//! event-driven shared mode with a 10 ms period.
//!
//! The mix format (usually 32-bit float stereo at 48 kHz) is mapped to
//! **stereo** (a mono device is duplicated, surround is folded down with
//! `fold_to_stereo`), converted to s16 and, if the device does not run at
//! 48 kHz, linearly resampled to it; [`MicSource::read_tick`] then hands out
//! 10 ms interleaved stereo ticks ([`OPUS10_SAMPLES`] frames) that the
//! session encodes as 10 ms stereo Opus packets (with in-band FEC) for a
//! `CAP_STEREO` viewer, and as the mono down-mix for any other.
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

/// Most mix-format channels we read (7.1); further ones are ignored.
const MAX_CHANNELS: usize = 8;

/// Fold one device frame (WAVE channel order: FL FR FC LFE BL BR SL SR) to
/// `(left, right)`: mono is duplicated, stereo passes through, and
/// surround mixes the centre (-3 dB) into both sides and the back / side
/// pairs into their side, normalised by the weights (LFE dropped).
fn fold_to_stereo(frame: &[f32]) -> (f32, f32) {
    const C: f32 = std::f32::consts::FRAC_1_SQRT_2;
    // (weight into left, weight into right) per channel.
    const W: [(f32, f32); MAX_CHANNELS] = [
        (1.0, 0.0),
        (0.0, 1.0),
        (C, C),
        (0.0, 0.0),
        (C, 0.0),
        (0.0, C),
        (C, 0.0),
        (0.0, C),
    ];
    match frame {
        [] => (0.0, 0.0),
        [m] => (*m, *m),
        [l, r] => (*l, *r),
        _ => {
            let (mut l, mut r, mut wl, mut wr) = (0.0, 0.0, 0.0, 0.0);
            for (x, (a, b)) in frame.iter().zip(W) {
                l += x * a;
                r += x * b;
                wl += a;
                wr += b;
            }
            (l / wl.max(1.0), r / wr.max(1.0))
        }
    }
}

/// Linear device-rate -> 48 kHz resampler for interleaved stereo; emits s16.
struct StereoResampler {
    rate: u32,
    /// Fractional read position between the previous and the next frame.
    phase: f64,
    prev: (f32, f32),
}

impl StereoResampler {
    fn new(rate: u32) -> Self {
        Self {
            rate: rate.max(1),
            phase: 0.0,
            prev: (0.0, 0.0),
        }
    }

    /// Feed one device-rate frame; append the 48 kHz frames it completes.
    fn push(&mut self, l: f32, r: f32, out: &mut VecDeque<i16>) {
        let to_i16 = |v: f32| (v.clamp(-1.0, 1.0) * 32767.0) as i16;
        if self.rate == SAMPLE_RATE {
            out.push_back(to_i16(l));
            out.push_back(to_i16(r));
            return;
        }
        // Output frames fall at phase steps of rate/48000 input frames;
        // interpolate between `prev` and this frame for those in (prev, x].
        let step = f64::from(self.rate) / f64::from(SAMPLE_RATE);
        let (pl, pr) = self.prev;
        while self.phase <= 1.0 {
            let t = self.phase as f32;
            out.push_back(to_i16(pl + (l - pl) * t));
            out.push_back(to_i16(pr + (r - pr) * t));
            self.phase += step;
        }
        self.phase -= 1.0;
        self.prev = (l, r);
    }
}

/// Default-render-endpoint loopback capture (see the module docs).
pub(crate) struct LoopbackSource {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: HANDLE,
    channels: usize,
    fmt: SampleFmt,
    /// Interleaved stereo s16 at 48 kHz, not yet handed out.
    pending: VecDeque<i16>,
    /// Device rate -> 48 kHz.
    resampler: StereoResampler,
    /// When the next tick is due if the device stays silent.
    next_due: Instant,
    pub(crate) description: String,
}

// SAFETY: the COM objects are created on (and only used from) the mic
// thread, which initialized COM as multithreaded (MTA objects are agile).
unsafe impl Send for LoopbackSource {}

impl LoopbackSource {
    /// Open the default output device's loopback stream.
    pub(crate) fn open() -> Result<Self, String> {
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
                fmt,
                pending: VecDeque::with_capacity(8 * OPUS10_SAMPLES as usize),
                resampler: StereoResampler::new(rate.max(1)),
                next_due: Instant::now(),
                description: format!(
                    "WASAPI loopback, {rate} Hz x{channels} {fmt:?} -> 48 kHz stereo, 10 ms period"
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
                let mut frame = [0.0f32; MAX_CHANNELS];
                let ch = self.channels.min(MAX_CHANNELS);
                for f in 0..frames_us {
                    let (l, r) = if silent {
                        (0.0, 0.0)
                    } else {
                        for (c, slot) in frame.iter_mut().enumerate().take(ch) {
                            let i = f * self.channels + c;
                            *slot = match self.fmt {
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
                        fold_to_stereo(&frame[..ch])
                    };
                    self.resampler.push(l, r, &mut self.pending);
                }
                let _ = self.capture.ReleaseBuffer(frames);
                any = true;
            }
        }
    }
}

impl MicSource for LoopbackSource {
    fn read_tick(&mut self, out: &mut [i16]) -> Result<(), String> {
        // `out` is interleaved stereo: half as many frames as samples.
        let frames = out.len() / 2;
        let tick = Duration::from_micros(frames as u64 * 1_000_000 / u64::from(SAMPLE_RATE));
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

    fn channels(&self) -> usize {
        2
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
        assert_eq!(src.channels(), 2, "loopback delivers stereo");
        let mut buf = vec![0i16; src.tick_samples() * src.channels()];
        let t0 = Instant::now();
        for _ in 0..30 {
            src.read_tick(&mut buf).unwrap();
        }
        let took = t0.elapsed();
        // 30 x 10 ms = 300 ms (allowing the startup burst / scheduling).
        assert!(took < Duration::from_millis(900), "{took:?}");
    }

    #[test]
    fn resampler_keeps_the_48k_rate_in_stereo() {
        let mut out = VecDeque::new();
        let mut rs = StereoResampler::new(44_100);
        for i in 0..44_100 {
            let x = ((i as f32) * 0.01).sin();
            rs.push(x, -x, &mut out);
        }
        assert_eq!(out.len() % 2, 0, "whole frames only");
        let frames = out.len() as i64 / 2;
        assert!((frames - 48_000).abs() <= 2, "{frames}");
        // The channels stay apart (R = -L).
        let v: Vec<i16> = out.into_iter().collect();
        assert!(v
            .chunks(2)
            .all(|f| (i32::from(f[0]) + i32::from(f[1])).abs() <= 1));
    }

    #[test]
    fn resampler_passes_48k_through() {
        let mut out = VecDeque::new();
        let mut rs = StereoResampler::new(SAMPLE_RATE);
        rs.push(0.5, -0.25, &mut out);
        assert_eq!(out, [16_383, -8_191]);
    }

    #[test]
    fn fold_maps_mono_stereo_and_surround() {
        assert_eq!(fold_to_stereo(&[0.3]), (0.3, 0.3));
        assert_eq!(fold_to_stereo(&[0.1, 0.2]), (0.1, 0.2));
        // 5.1: a centre-only signal lands equally on both sides, LFE never.
        let (l, r) = fold_to_stereo(&[0.0, 0.0, 0.5, 1.0, 0.0, 0.0]);
        assert!((l - r).abs() < 1e-6 && l > 0.0 && l <= 0.5, "{l} {r}");
        // A left-only 7.1 signal stays left.
        let (l, r) = fold_to_stereo(&[1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 1.0, 0.0]);
        assert!(l > 0.0 && r == 0.0, "{l} {r}");
        // Full scale everywhere never exceeds full scale.
        let (l, r) = fold_to_stereo(&[1.0; 8]);
        assert!(l <= 1.0 + 1e-6 && r <= 1.0 + 1e-6, "{l} {r}");
    }
}
