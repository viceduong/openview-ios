//! H.264 pass-through: access units are checked for continuity and handed
//! to the shell's VideoToolbox decoder as [`OvVideoPacket`]s.

use std::ffi::c_void;

use super::OvScreen;
use crate::error::OV_ERR_PANIC;

/// [`OvVideoPacket::flags`]: an IDR access unit; `sps`/`pps` are set.
pub const OV_VIDEO_KEYFRAME: u32 = 1;
/// [`OvVideoPacket::flags`]: motion over — no picture; the next frame-sink
/// delivery carries the lossless refresh of `x, y, w, h`, after which the
/// shell removes its video overlay.
pub const OV_VIDEO_END: u32 = 2;
/// [`OvVideoPacket::flags`]: a newer picture of the same burst follows —
/// decode this unit (the reference chain needs it) without presenting it
/// (VideoToolbox `kVTDecodeFrame_DoNotOutputFrame`).
pub const OV_VIDEO_SUPERSEDED: u32 = 4;

/// One H.264 access unit for the shell, laid out for C (`ov_video_packet`
/// in shim.h). All pointers are valid only for the duration of the callback.
///
/// The picture (`w x h`, even) goes at `(x, y)` of the `screen_w x
/// screen_h` framebuffer the frame sink delivers. `data` is **AVCC**: each
/// NAL unit prefixed by its 4-byte big-endian length, parameter sets and
/// access-unit delimiters removed (exactly what a `CMSampleBuffer` for
/// VideoToolbox wants, with `nalUnitHeaderLength` 4). On a keyframe `sps` /
/// `pps` are the raw parameter-set NAL units (no start code, no length) for
/// `CMVideoFormatDescriptionCreateFromH264ParameterSets`; otherwise null/0.
/// `pts_us` is the host's capture time (microseconds since the UNIX epoch).
#[repr(C)]
#[derive(Debug)]
pub struct OvVideoPacket {
    /// The access unit's sequence number.
    pub seq: u64,
    /// Host capture time, microseconds since the UNIX epoch.
    pub pts_us: u64,
    /// [`OV_VIDEO_KEYFRAME`] or [`OV_VIDEO_END`] (or 0), plus
    /// [`OV_VIDEO_SUPERSEDED`].
    pub flags: u32,
    /// Left edge of the picture in the framebuffer.
    pub x: u32,
    /// Top edge of the picture in the framebuffer.
    pub y: u32,
    /// Picture width (even).
    pub w: u32,
    /// Picture height (even).
    pub h: u32,
    /// Framebuffer width the picture is placed on.
    pub screen_w: u32,
    /// Framebuffer height the picture is placed on.
    pub screen_h: u32,
    /// The codec: 1 = H.264 (`sps`/`pps`, AVCC `data`), 2 = HEVC
    /// (`vps`/`sps`/`pps`, HVCC `data` with 4-byte lengths). Only 2 after
    /// the shell opted in with [`ov_screen_set_video_caps`] (`OV_VIDEO_CAP_HEVC`).
    pub codec: u32,
    /// Raw SPS NAL unit on a keyframe, else null.
    pub sps: *const u8,
    /// Length of `sps` (0 when null).
    pub sps_len: usize,
    /// Raw PPS NAL unit on a keyframe, else null.
    pub pps: *const u8,
    /// Length of `pps` (0 when null).
    pub pps_len: usize,
    /// The AVCC picture data (null when empty).
    pub data: *const u8,
    /// Length of `data`.
    pub data_len: usize,
    /// The stream's colour description as H.264 /
    /// ISO 23001-8 code points, from the SPS VUI (the host always signals
    /// BT.709 limited range: 1 / 1 / 1 / 0). Map `matrix` to
    /// `kCMFormatDescriptionExtension_YCbCrMatrix` (1 = ITU_R_709_2,
    /// 6 = ITU_R_601_4), `primaries` to `..._ColorPrimaries`, `transfer`
    /// to `..._TransferFunction`; `full_range` 0 = video range (420v).
    pub colour_primaries: u8,
    /// Transfer characteristics code point (see `colour_primaries`).
    pub transfer: u8,
    /// Matrix coefficients code point (see `colour_primaries`).
    pub matrix: u8,
    /// 1 = full range, 0 = video range (see `colour_primaries`).
    pub full_range: u8,
    /// HEVC only: the raw VPS NAL unit on a keyframe, else null (for
    /// `CMVideoFormatDescriptionCreateFromHEVCParameterSets` with `vps`,
    /// `sps`, `pps`).
    pub vps: *const u8,
    /// Length of `vps` (0 when null).
    pub vps_len: usize,
}

/// [`ov_screen_set_video_caps`] bit: the shell decodes HEVC (VideoToolbox
/// hardware HEVC): advertise `CAP_HEVC`.
pub const OV_VIDEO_CAP_HEVC: u32 = 1;
/// [`ov_screen_set_video_caps`] bit: long-term-reference loss recovery
/// (`CAP_LTR`; on by default). The shell's decoder must keep decoding
/// after units were dropped before it (the next unit it gets is a
/// recovery P-frame); clear it if VideoToolbox refuses those.
pub const OV_VIDEO_CAP_LTR: u32 = 2;

/// H.264 video sink, fired on the `ov_screen_feed` caller thread.
pub type VideoSink = extern "C" fn(user: *mut c_void, packet: *const OvVideoPacket);

impl OvScreen {
    /// The chain broke: queue a `VIDEO_RECOVER` (both peers speak
    /// `CAP_LTR` and we hold a long-term reference) or a
    /// `VIDEO_IDR_REQUEST` — rate limited, at most one queued.
    pub(super) fn request_idr(&mut self) {
        use ovapp::terminal::{Command, OpCode, CAP_LTR};
        let now = std::time::Instant::now();
        if self
            .last_idr_request
            .is_some_and(|t| now.saturating_duration_since(t) < ovapp::terminal::KEYFRAME_RETRY)
        {
            return;
        }
        let ops = [OpCode::VideoIdrRequest as u8, OpCode::VideoRecover as u8];
        if self.outbox.iter().any(|r| ops.contains(&r[0])) {
            return;
        }
        self.last_idr_request = Some(now);
        let ltr = self.local_caps() & self.peer_caps & CAP_LTR != 0;
        let cmd = match self.video_rx.request(ltr) {
            ovapp::video::VideoRequest::Recover { ltr_seq } => Command::VideoRecover { ltr_seq },
            ovapp::video::VideoRequest::Idr => Command::VideoIdrRequest {
                have_seq: self.last_video_seq,
            },
        };
        self.queue_small(&cmd);
    }

    /// A complete video unit: continuity check, then the sink.
    pub(super) fn video_unit(&mut self, h: ovapp::video::VideoHeader, data: &[u8]) {
        use ovapp::video::VideoVerdict;
        let Some(f) = self.video_sink.f else {
            self.last_video_seq = h.seq;
            return; // never advertised CAP_H264
        };
        let verdict = self.video_rx.accept(&h);
        if verdict == VideoVerdict::Stale {
            return; // Lost the race between media and reliable
        }
        self.last_video_seq = h.seq;
        if verdict == VideoVerdict::Drop {
            self.request_idr();
            return;
        }
        let codec = h.video_codec();
        let au = if verdict == VideoVerdict::End {
            ovapp::video::AccessUnit::default()
        } else {
            ovapp::video::AccessUnit::from_annexb_codec(codec, data)
        };
        let keyframe = verdict == VideoVerdict::Deliver && h.is_keyframe();
        if keyframe && !au.has_param_sets(codec) {
            // A keyframe the shell cannot build a format description from.
            self.video_rx.mark_corrupt();
            self.request_idr();
            return;
        }
        if codec == ovapp::video::VideoCodec::H264 {
            if let Some(c) = au.sps.as_deref().and_then(ovapp::video::sps::colour) {
                self.video_colour = if c.colour_description_present {
                    [c.primaries, c.transfer, c.matrix, c.full_range as u8]
                } else {
                    // Unspecified: what the host's converter produces.
                    [1, 1, 1, c.full_range as u8]
                };
            }
        } else if keyframe {
            // HEVC: the host converts BT.709 limited range.
            self.video_colour = [1, 1, 1, 0];
        }
        let raw = |p: &Option<Vec<u8>>| {
            p.as_deref()
                .filter(|_| keyframe)
                .map_or((std::ptr::null(), 0), |p| (p.as_ptr(), p.len()))
        };
        let (vps, vps_len) = raw(&au.vps);
        let (sps, sps_len) = raw(&au.sps);
        let (pps, pps_len) = raw(&au.pps);
        let pkt = OvVideoPacket {
            seq: h.seq,
            pts_us: h.pts_us,
            flags: if verdict == VideoVerdict::End {
                OV_VIDEO_END
            } else if keyframe {
                OV_VIDEO_KEYFRAME
            } else {
                0
            } | if h.flags & ovapp::video::VIDEO_SUPERSEDED != 0 {
                OV_VIDEO_SUPERSEDED
            } else {
                0
            },
            x: u32::from(h.rect.x),
            y: u32::from(h.rect.y),
            w: u32::from(h.rect.w),
            h: u32::from(h.rect.h),
            screen_w: h.screen_w,
            screen_h: h.screen_h,
            codec: u32::from(codec.wire()),
            sps,
            sps_len,
            pps,
            pps_len,
            data: if au.avcc.is_empty() {
                std::ptr::null()
            } else {
                au.avcc.as_ptr()
            },
            data_len: au.avcc.len(),
            colour_primaries: self.video_colour[0],
            transfer: self.video_colour[1],
            matrix: self.video_colour[2],
            full_range: self.video_colour[3],
            vps,
            vps_len,
        };
        if verdict == VideoVerdict::Deliver {
            // Reassembled and handed to VideoToolbox now (its decode
            // counts toward the present stage).
            self.latency_delivered(ovapp::latency::KIND_VIDEO, h.seq, std::time::Instant::now());
        }
        f(self.video_sink.user, &pkt);
        if verdict == VideoVerdict::Deliver {
            // Handed to the decoder: acknowledge a long-term reference (a
            // later VideoToolbox failure calls ov_screen_request_video_idr,
            // which forgets every LTR again).
            if let Some(seq) = self.video_rx.decoded(&h) {
                self.queue_small(&ovapp::terminal::Command::VideoLtrAck { seq });
            }
        }
    }
}

/// The shell's decoder failed (or it dropped a unit): skip
/// P-frames until the next keyframe and queue a `VIDEO_IDR_REQUEST` for
/// the host (sent through [`ov_screen_take_pong_reply`](crate::ov_screen_take_pong_reply);
/// rate limited). The decoder's references are treated as lost, so the
/// recovery is always a keyframe (never `VIDEO_RECOVER`). Returns 0, or -1
/// on a null handle.
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`](crate::ov_screen_new).
/// The screen is not internally locked: no other call on the same handle may
/// run concurrently (the shell drives it from one serial queue). Never call
/// it from inside one of this screen's sinks.
#[no_mangle]
pub unsafe extern "C" fn ov_screen_request_video_idr(s: *mut OvScreen) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        // SAFETY: `s` is null or a live screen with no concurrent use
        // (caller contract).
        let Some(screen) = (unsafe { s.as_mut() }) else {
            return -1;
        };
        screen.video_rx.mark_corrupt();
        screen.request_idr();
        0
    })
}

/// Which optional video features the shell's decoder handles
/// ([`OV_VIDEO_CAP_HEVC`] | [`OV_VIDEO_CAP_LTR`]; default
/// `OV_VIDEO_CAP_LTR`). Call before the first `ov_screen_feed`, with the
/// video sink installed: `OV_VIDEO_CAP_HEVC` advertises `CAP_HEVC` (the host
/// may then send `codec == 2` packets), clearing `OV_VIDEO_CAP_LTR` stops
/// advertising `CAP_LTR`. Returns 0, or -1 on a null handle.
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`](crate::ov_screen_new),
/// not used concurrently.
#[no_mangle]
pub unsafe extern "C" fn ov_screen_set_video_caps(s: *mut OvScreen, caps: u32) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        // SAFETY: `s` is null or a live screen with no concurrent use
        // (caller contract).
        let Some(screen) = (unsafe { s.as_mut() }) else {
            return -1;
        };
        screen.video_caps = caps & (OV_VIDEO_CAP_HEVC | OV_VIDEO_CAP_LTR);
        0
    })
}
