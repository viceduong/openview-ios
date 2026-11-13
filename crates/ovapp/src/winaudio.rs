//! WinMM audio devices shared by `ovhost` and `ovview` (Phase 4).
//!
//! Before Phase 4 each binary carried its own copy of a `waveIn` mic and a
//! `waveOut` speaker, both polling a single header's `WHDR_DONE` bit with
//! `sleep(2)` (a non-volatile read of memory the driver writes). This module
//! replaces them with **event-driven** devices:
//!
//! - [`WinmmMic`]: `CALLBACK_EVENT` + a ring of [`MIC_BUFFERS`] x 20 ms
//!   buffers (200 ms of slack). `read_tick` blocks on the event until the
//!   oldest buffer completes, copies it out and re-queues it, so the device
//!   always has buffers to fill and no captured audio is ever dropped — a
//!   reader that stalls < 200 ms just drains several completed buffers back
//!   to back. Headers are re-queued with `WHDR_PREPARED` intact (clearing
//!   all flags, as the old code did, makes `waveInAddBuffer` refuse them).
//! - [`WinmmSpeaker`] + [`spawn_playback`]: [`SPEAKER_BUFFERS`] rotating
//!   20 ms headers; the playback thread refills every completed header from
//!   the [`AudioBus`] jitter buffer and sleeps on the device event in
//!   between, so the device never starves while the thread is asleep.
//!
//! The FFI is declared by hand (no `windows` crate), like the rest of this
//! crate's Win32 code. All driver-shared header fields are accessed through
//! raw pointers with volatile reads.

use std::ptr::{addr_of, addr_of_mut};
use std::sync::atomic::{fence, AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::audio::{AudioBus, MicSource, SAMPLE_RATE, TICK_SAMPLES};

/// waveIn buffers in the capture ring (x 20 ms = 200 ms of slack).
pub const MIC_BUFFERS: usize = 10;
/// waveOut buffers rotating under playback (x 20 ms).
pub const SPEAKER_BUFFERS: usize = 3;

#[allow(non_snake_case, clippy::upper_case_acronyms, dead_code)]
mod ffi {
    pub const WAVE_MAPPER: u32 = 0xFFFF_FFFF;
    pub const WAVE_FORMAT_PCM: u16 = 1;
    pub const CALLBACK_EVENT: u32 = 0x0005_0000;
    pub const WHDR_DONE: u32 = 0x0001;
    pub const WHDR_PREPARED: u32 = 0x0002;
    pub const WHDR_INQUEUE: u32 = 0x0010;
    pub const MMSYSERR_NOERROR: u32 = 0;
    pub const WAIT_OBJECT_0: u32 = 0;

    pub type Handle = isize;

    #[repr(C)]
    pub struct WAVEFORMATEX {
        pub wFormatTag: u16,
        pub nChannels: u16,
        pub nSamplesPerSec: u32,
        pub nAvgBytesPerSec: u32,
        pub nBlockAlign: u16,
        pub wBitsPerSample: u16,
        pub cbSize: u16,
    }

    #[repr(C)]
    pub struct WAVEHDR {
        pub lpData: *mut u8,
        pub dwBufferLength: u32,
        pub dwBytesRecorded: u32,
        pub dwUser: usize,
        pub dwFlags: u32,
        pub dwLoops: u32,
        pub lpNext: usize,
        pub reserved: usize,
    }

    #[link(name = "winmm")]
    extern "system" {
        pub fn waveInOpen(
            phwi: *mut usize,
            uDeviceID: u32,
            pwfx: *const WAVEFORMATEX,
            dwCallback: usize,
            dwInstance: usize,
            fdwOpen: u32,
        ) -> u32;
        pub fn waveInPrepareHeader(hwi: usize, pwh: *mut WAVEHDR, cbwh: u32) -> u32;
        pub fn waveInUnprepareHeader(hwi: usize, pwh: *mut WAVEHDR, cbwh: u32) -> u32;
        pub fn waveInAddBuffer(hwi: usize, pwh: *mut WAVEHDR, cbwh: u32) -> u32;
        pub fn waveInStart(hwi: usize) -> u32;
        pub fn waveInReset(hwi: usize) -> u32;
        pub fn waveInClose(hwi: usize) -> u32;

        pub fn waveOutOpen(
            phwo: *mut usize,
            uDeviceID: u32,
            pwfx: *const WAVEFORMATEX,
            dwCallback: usize,
            dwInstance: usize,
            fdwOpen: u32,
        ) -> u32;
        pub fn waveOutPrepareHeader(hwo: usize, pwh: *mut WAVEHDR, cbwh: u32) -> u32;
        pub fn waveOutUnprepareHeader(hwo: usize, pwh: *mut WAVEHDR, cbwh: u32) -> u32;
        pub fn waveOutWrite(hwo: usize, pwh: *mut WAVEHDR, cbwh: u32) -> u32;
        pub fn waveOutReset(hwo: usize) -> u32;
        pub fn waveOutClose(hwo: usize) -> u32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        pub fn CreateEventW(
            attrs: *const core::ffi::c_void,
            manual: i32,
            initial: i32,
            name: *const u16,
        ) -> Handle;
        pub fn WaitForSingleObject(h: Handle, ms: u32) -> u32;
        pub fn CloseHandle(h: Handle) -> i32;
    }

    pub fn pcm_format() -> WAVEFORMATEX {
        WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM,
            nChannels: 1,
            nSamplesPerSec: super::SAMPLE_RATE,
            nAvgBytesPerSec: super::SAMPLE_RATE * 2,
            nBlockAlign: 2,
            wBitsPerSample: 16,
            cbSize: 0,
        }
    }

    pub const HDR_SIZE: u32 = std::mem::size_of::<WAVEHDR>() as u32;
}

/// An auto-reset Win32 event the driver signals per completed buffer.
struct Event(ffi::Handle);

impl Event {
    fn new() -> Result<Self, String> {
        // SAFETY: plain object creation; closed in Drop.
        let h = unsafe { ffi::CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
        if h == 0 {
            return Err("CreateEventW failed".into());
        }
        Ok(Self(h))
    }

    fn wait(&self, timeout: Duration) -> bool {
        let ms = timeout.as_millis().min(u128::from(u32::MAX - 1)) as u32;
        // SAFETY: valid handle owned by self.
        unsafe { ffi::WaitForSingleObject(self.0, ms) == ffi::WAIT_OBJECT_0 }
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: handle created in `new`, closed once.
        unsafe {
            ffi::CloseHandle(self.0);
        }
    }
}

/// A ring of WAVEHDRs + their sample buffers at stable heap addresses
/// (the driver holds raw pointers to both while a header is queued).
struct HdrRing {
    hdrs: Vec<*mut ffi::WAVEHDR>,
    bufs: Vec<Box<[i16]>>,
}

impl HdrRing {
    fn new(n: usize) -> Self {
        let mut bufs: Vec<Box<[i16]>> = (0..n)
            .map(|_| vec![0i16; TICK_SAMPLES as usize].into_boxed_slice())
            .collect();
        let hdrs = bufs
            .iter_mut()
            .map(|b| {
                Box::into_raw(Box::new(ffi::WAVEHDR {
                    lpData: b.as_mut_ptr().cast::<u8>(),
                    dwBufferLength: (b.len() * 2) as u32,
                    dwBytesRecorded: 0,
                    dwUser: 0,
                    dwFlags: 0,
                    dwLoops: 0,
                    lpNext: 0,
                    reserved: 0,
                }))
            })
            .collect();
        Self { hdrs, bufs }
    }

    /// Volatile read of a header's driver-owned flags.
    fn flags(&self, i: usize) -> u32 {
        // SAFETY: the pointer is a live Box owned by this ring.
        unsafe { std::ptr::read_volatile(addr_of!((*self.hdrs[i]).dwFlags)) }
    }

    fn bytes_recorded(&self, i: usize) -> u32 {
        // SAFETY: as above.
        unsafe { std::ptr::read_volatile(addr_of!((*self.hdrs[i]).dwBytesRecorded)) }
    }

    /// Clear DONE (keep PREPARED) and the recorded count before re-queueing.
    fn rearm(&self, i: usize) {
        let f = self.flags(i);
        let h = self.hdrs[i];
        // SAFETY: the header is not queued (DONE was observed).
        unsafe {
            std::ptr::write_volatile(addr_of_mut!((*h).dwFlags), f & !ffi::WHDR_DONE);
            std::ptr::write_volatile(addr_of_mut!((*h).dwBytesRecorded), 0);
        }
    }
}

impl Drop for HdrRing {
    fn drop(&mut self) {
        for &h in &self.hdrs {
            // SAFETY: created by Box::into_raw in `new`, freed once (the
            // device was reset + unprepared by the owner's Drop first).
            unsafe { drop(Box::from_raw(h)) };
        }
    }
}

/// The real microphone: WinMM `waveIn` on the default input device, 48 kHz
/// mono s16le, event-driven (see the module docs).
pub struct WinmmMic {
    hwi: usize,
    event: Event,
    ring: HdrRing,
    next: usize,
}

// SAFETY: the handle and the raw header pointers are only used by the
// owning thread; the driver's concurrent writes go through volatile reads.
unsafe impl Send for WinmmMic {}

impl WinmmMic {
    /// Open the default input device. Errors (no device, denied access)
    /// surface as `Err`, never a panic.
    pub fn open() -> Result<Self, String> {
        let event = Event::new()?;
        let fmt = ffi::pcm_format();
        let mut hwi: usize = 0;
        // SAFETY: valid pointers; the handle is closed in Drop.
        let err = unsafe {
            ffi::waveInOpen(
                &mut hwi,
                ffi::WAVE_MAPPER,
                &fmt,
                event.0 as usize,
                0,
                ffi::CALLBACK_EVENT,
            )
        };
        if err != ffi::MMSYSERR_NOERROR {
            return Err(format!("waveInOpen failed (error {err}) — no microphone?"));
        }
        let mic = Self {
            hwi,
            event,
            ring: HdrRing::new(MIC_BUFFERS),
            next: 0,
        };
        for &h in &mic.ring.hdrs {
            // SAFETY: header + buffer live in `mic.ring` (stable addresses).
            let err = unsafe { ffi::waveInPrepareHeader(hwi, h, ffi::HDR_SIZE) };
            if err != ffi::MMSYSERR_NOERROR {
                return Err(format!("waveInPrepareHeader failed (error {err})"));
            }
            // SAFETY: prepared header we own.
            let err = unsafe { ffi::waveInAddBuffer(hwi, h, ffi::HDR_SIZE) };
            if err != ffi::MMSYSERR_NOERROR {
                return Err(format!("waveInAddBuffer failed (error {err})"));
            }
        }
        // SAFETY: handle from a successful open.
        let err = unsafe { ffi::waveInStart(hwi) };
        if err != ffi::MMSYSERR_NOERROR {
            return Err(format!("waveInStart failed (error {err})"));
        }
        Ok(mic)
    }
}

impl MicSource for WinmmMic {
    fn read_tick(&mut self, out: &mut [i16]) -> Result<(), String> {
        let i = self.next;
        let deadline = Instant::now() + Duration::from_millis(500);
        while self.ring.flags(i) & ffi::WHDR_DONE == 0 {
            let now = Instant::now();
            if now >= deadline {
                return Err("mic buffer did not complete in 500 ms".into());
            }
            self.event
                .wait((deadline - now).min(Duration::from_millis(100)));
        }
        fence(Ordering::Acquire);
        let n = (self.ring.bytes_recorded(i) as usize / 2).min(out.len());
        out[..n].copy_from_slice(&self.ring.bufs[i][..n]);
        // Short reads (only at reset) are zero-filled: silence beats garbage.
        out[n..].fill(0);
        self.ring.rearm(i);
        // SAFETY: prepared header we own; DONE observed so not queued.
        let err = unsafe { ffi::waveInAddBuffer(self.hwi, self.ring.hdrs[i], ffi::HDR_SIZE) };
        if err != ffi::MMSYSERR_NOERROR {
            return Err(format!("waveInAddBuffer failed (error {err})"));
        }
        self.next = (i + 1) % MIC_BUFFERS;
        Ok(())
    }

    fn self_paced(&self) -> bool {
        true
    }
}

impl Drop for WinmmMic {
    fn drop(&mut self) {
        // SAFETY: reset returns every queued buffer (marked DONE); then the
        // headers are unprepared and the handle closed before the ring and
        // event (fields) are freed.
        unsafe {
            ffi::waveInReset(self.hwi);
            for &h in &self.ring.hdrs {
                ffi::waveInUnprepareHeader(self.hwi, h, ffi::HDR_SIZE);
            }
            ffi::waveInClose(self.hwi);
        }
    }
}

/// The default output device: WinMM `waveOut`, 48 kHz mono s16le, with
/// [`SPEAKER_BUFFERS`] rotating headers and a completion event.
pub struct WinmmSpeaker {
    hwo: usize,
    event: Event,
    ring: HdrRing,
    queued: [bool; SPEAKER_BUFFERS],
}

// SAFETY: as for `WinmmMic`.
unsafe impl Send for WinmmSpeaker {}

impl WinmmSpeaker {
    /// Open the default output device.
    pub fn open() -> Result<Self, String> {
        let event = Event::new()?;
        let fmt = ffi::pcm_format();
        let mut hwo: usize = 0;
        // SAFETY: valid pointers; the handle is closed in Drop.
        let err = unsafe {
            ffi::waveOutOpen(
                &mut hwo,
                ffi::WAVE_MAPPER,
                &fmt,
                event.0 as usize,
                0,
                ffi::CALLBACK_EVENT,
            )
        };
        if err != ffi::MMSYSERR_NOERROR {
            return Err(format!("waveOutOpen failed (error {err}) — no speaker?"));
        }
        let spk = Self {
            hwo,
            event,
            ring: HdrRing::new(SPEAKER_BUFFERS),
            queued: [false; SPEAKER_BUFFERS],
        };
        for &h in &spk.ring.hdrs {
            // SAFETY: header + buffer live in `spk.ring`.
            let err = unsafe { ffi::waveOutPrepareHeader(hwo, h, ffi::HDR_SIZE) };
            if err != ffi::MMSYSERR_NOERROR {
                return Err(format!("waveOutPrepareHeader failed (error {err})"));
            }
        }
        Ok(spk)
    }

    /// Indices of headers the device is not holding (never queued, or done).
    fn free_slot(&mut self) -> Option<usize> {
        (0..SPEAKER_BUFFERS).find(|&i| {
            if self.queued[i] && self.ring.flags(i) & ffi::WHDR_DONE != 0 {
                self.queued[i] = false;
            }
            !self.queued[i]
        })
    }

    /// Queue one tick produced by `fill` into a free header, if any.
    /// Returns `Ok(false)` when every header is still playing.
    pub fn feed(&mut self, fill: impl FnOnce(&mut [i16])) -> Result<bool, String> {
        let Some(i) = self.free_slot() else {
            return Ok(false);
        };
        fence(Ordering::Acquire);
        fill(&mut self.ring.bufs[i]);
        self.ring.rearm(i);
        // SAFETY: prepared header we own, not queued.
        let err = unsafe { ffi::waveOutWrite(self.hwo, self.ring.hdrs[i], ffi::HDR_SIZE) };
        if err != ffi::MMSYSERR_NOERROR {
            return Err(format!("waveOutWrite failed (error {err})"));
        }
        self.queued[i] = true;
        Ok(true)
    }

    /// Headers currently queued on the device.
    pub fn queued(&mut self) -> usize {
        let _ = self.free_slot();
        self.queued.iter().filter(|&&q| q).count()
    }

    /// Block until the device completes a buffer (or `timeout`).
    pub fn wait(&self, timeout: Duration) {
        self.event.wait(timeout);
    }
}

impl Drop for WinmmSpeaker {
    fn drop(&mut self) {
        // SAFETY: reset returns queued buffers; unprepare + close before the
        // ring/event fields are freed.
        unsafe {
            ffi::waveOutReset(self.hwo);
            for &h in &self.ring.hdrs {
                ffi::waveOutUnprepareHeader(self.hwo, h, ffi::HDR_SIZE);
            }
            ffi::waveOutClose(self.hwo);
        }
    }
}

/// Spawn the playback thread for `bus` (named `name`). It opens the default
/// speaker, keeps every free header filled from the bus's jitter buffer
/// while the bus is enabled, and exits (closing the device) once `stop` is
/// set — join the handle to make sure the device is released.
pub fn spawn_playback(
    bus: AudioBus,
    stop: Arc<AtomicBool>,
    name: &str,
) -> Option<std::thread::JoinHandle<()>> {
    let tag = name.to_owned();
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || playback_loop(bus, stop, &tag))
        .map_err(|e| tracing::warn!("{name}: audio thread failed to start: {e}"))
        .ok()
}

fn playback_loop(bus: AudioBus, stop: Arc<AtomicBool>, tag: &str) {
    let mut out = match WinmmSpeaker::open() {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!("{tag}: speaker unavailable, audio playback disabled: {e}");
            return;
        }
    };
    while !stop.load(Ordering::Relaxed) {
        if !bus.is_enabled() {
            // Idle: let queued buffers finish; nothing new is queued.
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        loop {
            match out.feed(|tick| {
                bus.pop_tick(tick);
            }) {
                Ok(true) => continue,
                Ok(false) => break,
                Err(e) => {
                    tracing::warn!("{tag}: audio playback stopped: {e}");
                    return;
                }
            }
        }
        out.wait(Duration::from_millis(50));
    }
}
