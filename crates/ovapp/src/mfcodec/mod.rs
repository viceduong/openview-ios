//! Media Foundation H.264 encoder / decoder MFTs (Windows, `mf` feature —
//! `ovhost` encodes, `ovview` decodes).
//!
//! **Encoder** ([`H264Encoder`]): a hardware MFT first (NVENC / AMF / Quick
//! Sync through `MFTEnumEx(MFT_ENUM_FLAG_HARDWARE)`, driven with the async
//! event model), falling back to Microsoft's software MFT (synchronous).
//! Low-latency mode, no B-frames, CBR at the requested bitrate (retargeted
//! at run time from the link pacer), a long GOP and IDR on demand
//! (`CODECAPI_AVEncVideoForceKeyFrame`). Input is system-memory NV12
//! converted from the captured BGRA on the CPU ([`crate::video::bgra_to_nv12`]).
//! Output is Annex-B; every IDR is made self-contained (SPS/PPS) by
//! [`ParamSetCache`](crate::video::ParamSetCache).
//!
//! **GPU input** ([`H264Encoder::new_gpu`], [`MfVideoEncoder`]): a hardware
//! MFT can instead take D3D11 NV12 surfaces converted from the captured BGRA
//! texture by the D3D11 video processor, skipping the CPU conversion and the
//! system-memory upload.
//!
//! **Decoder** ([`H264Decoder`]): Microsoft's H.264 decoder MFT (software,
//! low-latency mode: one access unit in, one picture out), NV12 converted to
//! BGRA on the CPU.
//!
//! Layout: `encoder` (the encoder MFT), `decoder` (the decoder MFT),
//! `gpu` (D3D11 input), `mft` (shared MFT plumbing), and here the
//! availability probe, start-up and the host-facing [`MfVideoEncoder`].

mod decoder;
mod encoder;
mod gpu;
mod mft;
#[cfg(test)]
mod tests;

pub use decoder::H264Decoder;
pub use encoder::H264Encoder;
pub use gpu::{gpu_device, GpuFrame};

use std::cell::Cell;
use std::sync::OnceLock;

use windows::Win32::Media::MediaFoundation::{MFStartup, MFSTARTUP_LITE, MF_VERSION};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

use crate::error::AppError;
use crate::video::{EncodedAu, VideoEncode};

/// Why [`available`] is false, for logs and errors.
pub const UNAVAILABLE: &str =
    "Media Foundation is not installed (Windows N/KN without the Media Feature Pack)";

/// Whether Media Foundation (`mfplat.dll`) is present. Windows N/KN editions
/// lack it until the user installs the Media Feature Pack; `ovhost` and
/// `ovview` **delay-load** `mfplat.dll` (see their build.rs), so no MF
/// function may run before this probe succeeded — `startup` checks it,
/// and every public constructor here calls `startup` first.
pub fn available() -> bool {
    use windows::core::w;
    use windows::Win32::System::LibraryLoader::{LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    // SAFETY: loading a system DLL by name from System32 only; the module
    // stays loaded for the process (the delay-load stub then binds to it).
    *AVAILABLE.get_or_init(|| unsafe {
        LoadLibraryExW(w!("mfplat.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32).is_ok()
    })
}

/// MF + COM for this thread (COM once per thread; MF once per process).
/// Fails with [`UNAVAILABLE`] when Media Foundation is missing.
fn startup() -> Result<(), AppError> {
    if !available() {
        return Err(AppError::platform(UNAVAILABLE));
    }
    thread_local! {
        static COM: Cell<bool> = const { Cell::new(false) };
    }
    COM.with(|c| {
        if !c.get() {
            // RPC_E_CHANGED_MODE (an STA thread) is fine: COM is usable.
            // SAFETY: plain COM initialisation of the calling thread with no
            // reserved pointer; a failure is reported, never UB.
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            c.set(true);
        }
    });
    static MF: OnceLock<Result<(), AppError>> = OnceLock::new();
    MF.get_or_init(|| {
        // SAFETY: `available()` confirmed mfplat.dll is loadable, so the
        // delay-loaded import resolves; MFStartup takes no pointers.
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) }
            .map_err(|e| AppError::platform(format!("MFStartup: {e}")))
    })
    .clone()
}

/// A Media Foundation / D3D11 call failure as `"{what}: {e}"`.
fn err(what: &str, e: windows::core::Error) -> AppError {
    AppError::platform(format!("{what}: {e}"))
}

/// Which encoder [`H264Encoder::new`] may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderPref {
    /// Hardware first, software fallback.
    Auto,
    /// Hardware MFTs only.
    Hardware,
    /// Microsoft's software MFT only.
    Software,
}

/// Pack two u32s into an MF "size"/"ratio" attribute.
fn pack(hi: u32, lo: u32) -> u64 {
    (u64::from(hi) << 32) | u64::from(lo)
}

/// The host's encoder: CPU (system-memory NV12) until the first GPU frame
/// arrives, then — on a hardware MFT that accepts the capture device — GPU
/// input; any GPU failure falls back to the CPU path for good.
pub struct MfVideoEncoder {
    enc: H264Encoder,
    w: u32,
    h: u32,
    fps: u32,
    bitrate: u32,
    pref: EncoderPref,
    gpu_failed: bool,
    /// Why the GPU input path was abandoned (set once it fell back to CPU).
    pub gpu_error: Option<String>,
}

impl MfVideoEncoder {
    /// A CPU-input encoder for `w x h` at `fps` and `bitrate` bits/s; GPU
    /// input is tried on the first [`VideoEncode::encode_gpu`] call (never
    /// with [`EncoderPref::Software`]).
    pub fn new(
        w: u32,
        h: u32,
        fps: u32,
        bitrate: u32,
        pref: EncoderPref,
    ) -> Result<Self, AppError> {
        Ok(Self {
            enc: H264Encoder::new(w, h, fps, bitrate, pref)?,
            w,
            h,
            fps,
            bitrate,
            pref,
            gpu_failed: pref == EncoderPref::Software,
            gpu_error: None,
        })
    }
}

impl VideoEncode for MfVideoEncoder {
    fn encode(
        &mut self,
        bgra: &[u8],
        stride: usize,
        force_idr: bool,
        pts_us: u64,
    ) -> Result<Vec<EncodedAu>, String> {
        if self.enc.is_gpu() {
            // A GPU encoder fed from memory (no GPU frame this time): the
            // texture path is unavailable for this capture; back to CPU.
            self.enc = H264Encoder::new(self.w, self.h, self.fps, self.bitrate, self.pref)?;
            self.gpu_failed = true;
        }
        self.enc.encode(bgra, stride, force_idr, pts_us)
    }

    fn encode_gpu(
        &mut self,
        frame: &dyn std::any::Any,
        rect: (u32, u32, u32, u32),
        force_idr: bool,
        pts_us: u64,
    ) -> Option<Result<Vec<EncodedAu>, String>> {
        if self.gpu_failed {
            return None;
        }
        let f = frame.downcast_ref::<GpuFrame>()?;
        if !self.enc.is_gpu() {
            match H264Encoder::new_gpu(&f.device, self.w, self.h, self.fps, self.bitrate) {
                Ok(e) => self.enc = e,
                Err(e) => {
                    self.gpu_failed = true;
                    self.gpu_error = Some(e.to_string());
                    return None;
                }
            }
        }
        match self.enc.encode_texture(&f.texture, rect, force_idr, pts_us) {
            Ok(v) => Some(Ok(v)),
            Err(e) => {
                // Fall back to the CPU encoder (a fresh IDR follows).
                self.gpu_failed = true;
                self.gpu_error = Some(e.to_string());
                match H264Encoder::new(self.w, self.h, self.fps, self.bitrate, self.pref) {
                    Ok(c) => {
                        self.enc = c;
                        None
                    }
                    Err(e) => Some(Err(e.to_string())),
                }
            }
        }
    }

    fn set_bitrate(&mut self, bps: u32) {
        self.bitrate = bps;
        self.enc.set_bitrate(bps);
    }

    fn name(&self) -> String {
        self.enc.name()
    }
}
