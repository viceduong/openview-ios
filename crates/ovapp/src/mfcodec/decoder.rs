//! The H.264 decoder MFT (Microsoft software decoder, low-latency mode).

use windows::core::Interface;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Variant::VARIANT;

use super::mft::{memory_sample, pull, sample_bytes, Pulled};
use super::{err, startup};
use crate::error::AppError;
use crate::video::{nv12_to_bgra, VideoDecode};

/// Microsoft's H.264 decoder MFT, NV12 out (see the module docs).
pub struct H264Decoder {
    mft: IMFTransform,
    out_info: MFT_OUTPUT_STREAM_INFO,
    /// Output picture geometry (coded size, e.g. 1920x1088) and pitch.
    out_w: u32,
    out_h: u32,
    stride: u32,
    nv12: Vec<u8>,
    time: i64,
}

impl H264Decoder {
    /// Create and start Microsoft's H.264 decoder MFT (NV12 output).
    pub fn new() -> Result<Self, AppError> {
        startup()?;
        // SAFETY: COM/MF calls on the decoder transform created here and on
        // the media type it is given; MF + COM were initialised by `startup`.
        unsafe {
            let mft: IMFTransform =
                CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_INPROC_SERVER)
                    .map_err(|e| err("H.264 decoder", e))?;
            if let Ok(a) = mft.GetAttributes() {
                let _ = a.SetUINT32(&MF_LOW_LATENCY, 1);
            }
            if let Ok(api) = mft.cast::<ICodecAPI>() {
                let _ = api.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true));
            }
            let t = MFCreateMediaType().map_err(|e| err("media type", e))?;
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| err("type", e))?;
            t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
                .map_err(|e| err("type", e))?;
            mft.SetInputType(0, &t, 0)
                .map_err(|e| err("SetInputType(H264)", e))?;
            let mut d = Self {
                mft,
                out_info: MFT_OUTPUT_STREAM_INFO::default(),
                out_w: 0,
                out_h: 0,
                stride: 0,
                nv12: Vec::new(),
                time: 0,
            };
            d.pick_output()?;
            d.mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|e| err("begin streaming", e))?;
            d.mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|e| err("start of stream", e))?;
            Ok(d)
        }
    }

    /// Pick the decoder's NV12 output type and record its geometry.
    fn pick_output(&mut self) -> Result<(), AppError> {
        // SAFETY: COM calls on the decoder's own transform and the media types
        // it offers.
        unsafe {
            for i in 0..64 {
                let Ok(t) = self.mft.GetOutputAvailableType(0, i) else {
                    break;
                };
                if t.GetGUID(&MF_MT_SUBTYPE).ok() != Some(MFVideoFormat_NV12) {
                    continue;
                }
                self.mft
                    .SetOutputType(0, &t, 0)
                    .map_err(|e| err("SetOutputType(NV12)", e))?;
                let fs = t.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
                self.out_w = (fs >> 32) as u32;
                self.out_h = fs as u32;
                let stride = t
                    .GetUINT32(&MF_MT_DEFAULT_STRIDE)
                    .map(|s| (s as i32).unsigned_abs())
                    .unwrap_or(0);
                self.stride = if stride >= self.out_w {
                    stride
                } else {
                    self.out_w
                };
                self.out_info = self
                    .mft
                    .GetOutputStreamInfo(0)
                    .map_err(|e| err("output info", e))?;
                return Ok(());
            }
            Err(AppError::codec("the H.264 decoder offers no NV12 output"))
        }
    }

    /// Decode one Annex-B access unit. `Ok(true)` when a picture came out
    /// (see [`picture`](Self::picture)).
    pub fn decode_nv12(&mut self, annexb: &[u8]) -> Result<bool, AppError> {
        let s = memory_sample(annexb, annexb.len()).map_err(|e| err("input sample", e))?;
        // SAFETY: COM calls on the decoder's own transform with the live
        // sample created above.
        unsafe {
            self.time += 333_333;
            let _ = s.SetSampleTime(self.time);
            if let Err(e) = self.mft.ProcessInput(0, &s, 0) {
                if e.code() != MF_E_NOTACCEPTING {
                    return Err(err("ProcessInput", e));
                }
                self.pull_all()?;
                self.mft
                    .ProcessInput(0, &s, 0)
                    .map_err(|e| err("ProcessInput", e))?;
            }
            self.pull_all()
        }
    }

    /// Pull every picture available now; keeps the newest. Whether any came.
    fn pull_all(&mut self) -> Result<bool, AppError> {
        let mut got = false;
        for _ in 0..16 {
            let cap = (self.out_w.max(16) * self.out_h.max(16) * 3 / 2) as usize;
            match pull(&self.mft, &self.out_info, cap)? {
                Pulled::NeedInput => return Ok(got),
                Pulled::StreamChange => self.pick_output()?,
                Pulled::Sample(s) => {
                    sample_bytes(&s, &mut self.nv12).map_err(|e| err("picture bytes", e))?;
                    // A tight contiguous copy has pitch = width.
                    if self.nv12.len() == (self.out_w * self.out_h * 3 / 2) as usize {
                        self.stride = self.out_w;
                    }
                    got = true;
                }
            }
        }
        Ok(got)
    }

    /// The last decoded picture: `(nv12, pitch, coded_w, coded_h)`.
    pub fn picture(&self) -> (&[u8], u32, u32, u32) {
        (&self.nv12, self.stride, self.out_w, self.out_h)
    }
}

impl VideoDecode for H264Decoder {
    fn decode(&mut self, annexb: &[u8], w: u32, h: u32, out: &mut Vec<u8>) -> Result<bool, String> {
        if !self.decode_nv12(annexb)? {
            return Ok(false);
        }
        if w > self.out_w || h > self.out_h {
            return Err(format!(
                "decoded picture {}x{} smaller than the declared {w}x{h}",
                self.out_w, self.out_h
            ));
        }
        let uv = self.stride as usize * self.out_h as usize;
        nv12_to_bgra(&self.nv12, self.stride as usize, uv, w, h, out)?;
        Ok(true)
    }
}

impl Drop for H264Decoder {
    fn drop(&mut self) {
        // SAFETY: end-of-stream messages to the transform we own, once, as
        // it is dropped.
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
    }
}
