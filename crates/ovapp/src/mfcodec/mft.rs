//! Shared Media Foundation transform plumbing: enumeration, media types,
//! memory samples and the `ProcessOutput` loop.

use std::mem::ManuallyDrop;

use windows::core::{GUID, PWSTR};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;

use super::{err, pack};
use crate::error::AppError;

/// The MFT's `MFT_FRIENDLY_NAME_Attribute` ("unnamed MFT" when absent).
pub(super) fn friendly_name(a: &IMFActivate) -> String {
    let mut p = PWSTR::null();
    let mut n = 0u32;
    // SAFETY: `a` is a live COM reference; the out-pointers are valid locals.
    // On success `p` is a CoTaskMem-allocated, NUL-terminated string that we
    // read once and free exactly once.
    unsafe {
        if a.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut p, &mut n)
            .is_ok()
            && !p.is_null()
        {
            let s = p.to_string().unwrap_or_default();
            CoTaskMemFree(Some(p.0 as *const _));
            s
        } else {
            "unnamed MFT".into()
        }
    }
}

/// Registered hardware H.264 encoders (best first).
pub(super) fn hardware_encoders() -> Vec<IMFActivate> {
    let out = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let mut pp: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut n = 0u32;
    let flags = MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0);
    // SAFETY: MFTEnumEx only writes the two out-pointers (valid locals); the
    // type-info struct outlives the call.
    let r = unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            None,
            Some(&out),
            &mut pp,
            &mut n,
        )
    };
    if r.is_err() || pp.is_null() {
        return Vec::new();
    }
    let mut v = Vec::with_capacity(n as usize);
    for i in 0..n as usize {
        // SAFETY: on success `pp` is a CoTaskMem array of `n` initialised
        // `Option<IMFActivate>`; `take` moves each reference out exactly once.
        if let Some(a) = unsafe { (*pp.add(i)).take() } {
            v.push(a);
        }
    }
    // SAFETY: every element was moved out above (left `None`), so freeing
    // the array releases no COM reference twice; `pp` is freed only here.
    unsafe { CoTaskMemFree(Some(pp as *const _)) };
    v
}

/// A progressive `w x h` video media type of `subtype` at `fps`, signalling
/// BT.709 limited range.
pub(super) fn video_type(
    subtype: &GUID,
    w: u32,
    h: u32,
    fps: u32,
) -> windows::core::Result<IMFMediaType> {
    // SAFETY: MF attribute calls on a media type we just created and own;
    // every argument is a valid reference or plain value.
    unsafe {
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h))?;
        t.SetUINT64(&MF_MT_FRAME_RATE, pack(fps.max(1), 1))?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        // BT.709 limited range: what `bgra_to_nv12` produces.
        let _ = t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32);
        let _ = t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32);
        // Full colour description (the SPS VUI the encoder writes;
        // `video::sps` rewrites the SPS when an encoder ignores these).
        let _ = t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32);
        let _ = t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32);
        Ok(t)
    }
}

/// A sample with one system-memory buffer of at least `cap` bytes holding
/// a copy of `data`.
pub(super) fn memory_sample(data: &[u8], cap: usize) -> windows::core::Result<IMFSample> {
    // SAFETY: the buffer is created with a max length of at least
    // `data.len()` bytes, so the locked pointer is valid for that many
    // writes until `Unlock`; `data` cannot overlap MF's allocation.
    unsafe {
        let buf = MFCreateMemoryBuffer(cap.max(data.len()).max(1) as u32)?;
        if !data.is_empty() {
            let mut p: *mut u8 = std::ptr::null_mut();
            buf.Lock(&mut p, None, None)?;
            std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
            buf.Unlock()?;
        }
        buf.SetCurrentLength(data.len() as u32)?;
        let s = MFCreateSample()?;
        s.AddBuffer(&buf)?;
        Ok(s)
    }
}

/// Copy a sample's bytes out (contiguous).
pub(super) fn sample_bytes(s: &IMFSample, out: &mut Vec<u8>) -> windows::core::Result<()> {
    // SAFETY: while the buffer is locked, `p` points to `len` initialised
    // bytes owned by MF; they are copied out before `Unlock`.
    unsafe {
        let buf = s.ConvertToContiguousBuffer()?;
        let mut p: *mut u8 = std::ptr::null_mut();
        let mut len = 0u32;
        buf.Lock(&mut p, None, Some(&mut len))?;
        out.clear();
        if !p.is_null() {
            out.extend_from_slice(std::slice::from_raw_parts(p, len as usize));
        }
        buf.Unlock()?;
        Ok(())
    }
}

/// The outcome of one [`pull`].
pub(super) enum Pulled {
    /// An output sample.
    Sample(IMFSample),
    /// The MFT needs more input first.
    NeedInput,
    /// The output type changed; renegotiate it and pull again.
    StreamChange,
}

/// One `ProcessOutput` call on stream 0.
pub(super) fn pull(
    mft: &IMFTransform,
    info: &MFT_OUTPUT_STREAM_INFO,
    fallback_cap: usize,
) -> Result<Pulled, AppError> {
    let provides = info.dwFlags
        & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32
        != 0;
    let ours = if provides {
        None
    } else {
        let cap = (info.cbSize as usize).max(fallback_cap);
        Some(memory_sample(&[], cap).map_err(|e| err("output sample", e))?)
    };
    let mut odb = [MFT_OUTPUT_DATA_BUFFER {
        dwStreamID: 0,
        pSample: ManuallyDrop::new(ours),
        dwStatus: 0,
        pEvents: ManuallyDrop::new(None),
    }];
    let mut status = 0u32;
    // SAFETY: `mft` is a live transform; `odb` and `status` are valid for the
    // call. Afterwards each `ManuallyDrop` field is taken exactly once (the
    // sample is returned, the events collection released) and `odb` is not
    // touched again, so no reference leaks or is released twice.
    let (r, sample) = unsafe {
        let r = mft.ProcessOutput(0, &mut odb, &mut status);
        let sample = ManuallyDrop::take(&mut odb[0].pSample);
        drop(ManuallyDrop::take(&mut odb[0].pEvents));
        (r, sample)
    };
    match r {
        Ok(()) => sample
            .map(Pulled::Sample)
            .ok_or_else(|| AppError::codec("ProcessOutput returned no sample")),
        Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(Pulled::NeedInput),
        Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => Ok(Pulled::StreamChange),
        Err(e) => Err(err("ProcessOutput", e)),
    }
}
