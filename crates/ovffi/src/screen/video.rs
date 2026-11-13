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
    /// [`OV_VIDEO_KEYFRAME`], [`OV_VIDEO_END`], or 0.
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
    /// Always 0.
    pub reserved: u32,
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
}

/// H.264 video sink, fired on the `ov_screen_feed` caller thread.
pub type VideoSink = extern "C" fn(user: *mut c_void, packet: *const OvVideoPacket);

impl OvScreen {
    /// Queue a VIDEO_IDR_REQUEST (rate limited, at most one queued).
    pub(super) fn request_idr(&mut self) {
        let now = std::time::Instant::now();
        if self
            .last_idr_request
            .is_some_and(|t| now.saturating_duration_since(t) < ovapp::terminal::KEYFRAME_RETRY)
        {
            return;
        }
        let op = ovapp::terminal::OpCode::VideoIdrRequest as u8;
        if self.outbox.iter().any(|r| r[0] == op) {
            return;
        }
        self.last_idr_request = Some(now);
        let have_seq = self.last_video_seq;
        self.queue_small(&ovapp::terminal::Command::VideoIdrRequest { have_seq });
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
        let au = if verdict == VideoVerdict::End {
            ovapp::video::AccessUnit::default()
        } else {
            ovapp::video::AccessUnit::from_annexb(data)
        };
        let keyframe = verdict == VideoVerdict::Deliver && h.is_keyframe();
        if keyframe && (au.sps.is_none() || au.pps.is_none()) {
            // A keyframe the shell cannot build a format description from.
            self.video_rx.mark_broken();
            self.request_idr();
            return;
        }
        if let Some(c) = au.sps.as_deref().and_then(ovapp::video::sps::colour) {
            self.video_colour = if c.colour_description_present {
                [c.primaries, c.transfer, c.matrix, c.full_range as u8]
            } else {
                // Unspecified: what the host's converter produces.
                [1, 1, 1, c.full_range as u8]
            };
        }
        let (sps, sps_len) = au
            .sps
            .as_deref()
            .map_or((std::ptr::null(), 0), |p| (p.as_ptr(), p.len()));
        let (pps, pps_len) = au
            .pps
            .as_deref()
            .map_or((std::ptr::null(), 0), |p| (p.as_ptr(), p.len()));
        let pkt = OvVideoPacket {
            seq: h.seq,
            pts_us: h.pts_us,
            flags: if verdict == VideoVerdict::End {
                OV_VIDEO_END
            } else if keyframe {
                OV_VIDEO_KEYFRAME
            } else {
                0
            },
            x: u32::from(h.rect.x),
            y: u32::from(h.rect.y),
            w: u32::from(h.rect.w),
            h: u32::from(h.rect.h),
            screen_w: h.screen_w,
            screen_h: h.screen_h,
            reserved: 0,
            sps: if keyframe { sps } else { std::ptr::null() },
            sps_len: if keyframe { sps_len } else { 0 },
            pps: if keyframe { pps } else { std::ptr::null() },
            pps_len: if keyframe { pps_len } else { 0 },
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
        };
        f(self.video_sink.user, &pkt);
    }
}

/// The shell's decoder failed (or it dropped a unit): skip
/// P-frames until the next keyframe and queue a `VIDEO_IDR_REQUEST` for
/// the host (sent through [`ov_screen_take_pong_reply`](crate::ov_screen_take_pong_reply);
/// rate limited). Returns 0, or -1 on a null handle.
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
        screen.video_rx.mark_broken();
        screen.request_idr();
        0
    })
}
