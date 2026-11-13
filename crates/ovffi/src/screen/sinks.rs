//! Screen callback types and their `ov_screen_set_*_sink` setters.
//!
//! Every setter shares one contract: `s` must be null or a live handle from
//! [`ov_screen_new`](crate::ov_screen_new) with no concurrent call on it, and
//! the callback plus `user` must stay valid until the sink is replaced or the
//! screen is freed. Pass a null callback (where the parameter is optional) to
//! remove the sink. Setters return 0, or -1 on a null handle.

use std::ffi::{c_char, c_void};

use super::{OvScreen, VideoSink};
use crate::callbacks::Sink;
use crate::error::OV_ERR_PANIC;

/// Reassembled-screen-frame sink. Fired on the `ov_screen_feed` caller thread
/// once a frame's `FRAME_END` has closed it. `pixels` points into the
/// assembler's buffer and is valid only for the duration of the callback —
/// copy it out.
pub type FrameSink = extern "C" fn(
    user: *mut c_void,
    seq: u64,
    width: u32,
    height: u32,
    stride: u32,
    format: u8,
    pixels: *const u8,
    len: usize,
);

/// Remote-cursor-position sink (`CURSOR` commands). Coordinates are in frame
/// pixel space (the captured region).
pub type CursorSink = extern "C" fn(user: *mut c_void, x: i32, y: i32);

/// Remote-cursor-shape sink (`CURSOR_SHAPE` commands). `bgra` is a
/// premultiplied-alpha 32-bit BGRA bitmap of `w * h * 4` bytes, valid only
/// for the duration of the callback — copy it out.
pub type CursorShapeSink = extern "C" fn(
    user: *mut c_void,
    hotspot_x: u16,
    hotspot_y: u16,
    w: u16,
    h: u16,
    bgra: *const u8,
    len: usize,
);

/// Liveness sink, fired by [`ov_screen_liveness_tick`](crate::ov_screen_liveness_tick):
/// `alive` is 1 while inbound traffic is fresh and 0 once the peer has been
/// silent past the dead window; `rtt_ms` is the last measured round trip
/// (-1 before the first PONG); `fps` is the inbound frame rate over the
/// rolling window.
pub type LivenessSink = extern "C" fn(user: *mut c_void, alive: i32, rtt_ms: i32, fps: f32);

/// Monitors sink, fired on MONITORS_INFO: `mons` points to
/// `count` packed records of 17 bytes each (big-endian) — `i32 x, i32 y, u32 w, u32 h,
/// u8 is_primary` — valid only for the duration of the callback (copy it
/// out). `count` may be 0.
pub type MonitorsSink = extern "C" fn(user: *mut c_void, mons: *const u8, count: usize);

/// Clipboard sink, fired on CLIPBOARD commands: `kind` is the
/// payload kind (0 = text) and `text` points to `len` UTF-8 bytes, valid
/// only for the duration of the callback (copy it out).
pub type ClipboardSink = extern "C" fn(user: *mut c_void, kind: u8, text: *const u8, len: usize);

/// File sink, fired when a peer-to-phone file transfer completes:
/// `name` is a NUL-terminated UTF-8 file name (sanitized by the reassembler)
/// and `data` points to `len` reassembled bytes, valid only for the duration
/// of the callback (copy it out — the whole file is held in memory here; a
/// streaming-to-disk receive is a documented future refinement).
pub type FileSink =
    extern "C" fn(user: *mut c_void, name: *const c_char, data: *const u8, len: usize);

/// Audio sink, fired on AUDIO commands: `samples` points to `len`
/// s16le PCM samples (48 kHz mono — 960 per 20 ms tick), valid only for the
/// duration of the callback (copy it out).
pub type AudioSink = extern "C" fn(user: *mut c_void, samples: *const u8, len: usize);

/// Opus audio sink, fired on Opus AUDIO commands: `packet` points
/// to `len` bytes of ONE Opus packet (48 kHz mono, normally 20 ms) that
/// decodes to `samples` samples; valid only for the duration of the
/// callback (copy it out). Rust does not decode Opus (the staticlib stays
/// pure Rust) — the shell feeds the packet to its native decoder.
///
/// On the direct path's media channel, `packet` NULL with `len` 0 signals
/// that `samples` samples of audio were lost for good: the shell conceals
/// that span (or simply skips it; the current Swift sink ignores NULL).
pub type OpusSink = extern "C" fn(user: *mut c_void, packet: *const u8, len: usize, samples: u32);

/// Store `sink` on the screen through `slot`; -1 on a null handle.
///
/// # Safety
/// `s` must be null or a live handle from
/// [`ov_screen_new`](crate::ov_screen_new) with no concurrent call on it.
unsafe fn set_sink<F>(
    s: *mut OvScreen,
    slot: fn(&mut OvScreen) -> &mut Sink<F>,
    sink: Option<F>,
    user: *mut c_void,
) -> i32 {
    // SAFETY: `s` is null or a live screen with no concurrent use (caller
    // contract).
    let Some(screen) = (unsafe { s.as_mut() }) else {
        return -1;
    };
    *slot(screen) = Sink::new(sink, user);
    0
}

/// Wire the frame sink. `sink`/`user` must outlive the assembler.
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`](crate::ov_screen_new)
/// with no concurrent call on it; `sink` and `user` must stay valid until the
/// sink is replaced or the screen is freed.
#[no_mangle]
pub unsafe extern "C" fn ov_screen_set_frame_sink(
    s: *mut OvScreen,
    user: *mut c_void,
    sink: FrameSink,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        // SAFETY: forwarded caller contract (see `# Safety`).
        unsafe { set_sink(s, |s| &mut s.sink, Some(sink), user) }
    })
}

/// Wire the remote-cursor-position sink. Pass a null `sink` to remove it.
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`](crate::ov_screen_new)
/// with no concurrent call on it; `sink` and `user` must stay valid until the
/// sink is replaced or the screen is freed.
#[no_mangle]
pub unsafe extern "C" fn ov_screen_set_cursor_sink(
    s: *mut OvScreen,
    user: *mut c_void,
    sink: Option<CursorSink>,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        // SAFETY: forwarded caller contract (see `# Safety`).
        unsafe { set_sink(s, |s| &mut s.cursor_sink, sink, user) }
    })
}

/// Wire the remote-cursor-shape sink. Pass a null `sink` to remove it.
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`](crate::ov_screen_new)
/// with no concurrent call on it; `sink` and `user` must stay valid until the
/// sink is replaced or the screen is freed.
#[no_mangle]
pub unsafe extern "C" fn ov_screen_set_cursor_shape_sink(
    s: *mut OvScreen,
    user: *mut c_void,
    sink: Option<CursorShapeSink>,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        // SAFETY: forwarded caller contract (see `# Safety`).
        unsafe { set_sink(s, |s| &mut s.cursor_shape_sink, sink, user) }
    })
}

/// Wire the monitors sink. Pass a null `sink` to remove it.
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`](crate::ov_screen_new)
/// with no concurrent call on it; `sink` and `user` must stay valid until the
/// sink is replaced or the screen is freed.
#[no_mangle]
pub unsafe extern "C" fn ov_screen_set_monitors_sink(
    s: *mut OvScreen,
    user: *mut c_void,
    sink: Option<MonitorsSink>,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        // SAFETY: forwarded caller contract (see `# Safety`).
        unsafe { set_sink(s, |s| &mut s.monitors_sink, sink, user) }
    })
}

/// Wire the audio sink. Pass a null `sink` to remove it.
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`](crate::ov_screen_new)
/// with no concurrent call on it; `sink` and `user` must stay valid until the
/// sink is replaced or the screen is freed.
#[no_mangle]
pub unsafe extern "C" fn ov_screen_set_audio_sink(
    s: *mut OvScreen,
    user: *mut c_void,
    sink: Option<AudioSink>,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        // SAFETY: forwarded caller contract (see `# Safety`).
        unsafe { set_sink(s, |s| &mut s.audio_sink, sink, user) }
    })
}

/// Wire the Opus audio sink. Set it **before** feeding the first
/// screen command: our CAPS (sent once, with the first screen command) then
/// advertise `CAP_OPUS` and an Opus-capable host switches its AUDIO to Opus
/// packets, delivered here instead of to the PCM audio sink. Pass a null
/// `sink` to remove it (raw PCM keeps flowing to the audio sink for hosts
/// without Opus).
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`](crate::ov_screen_new)
/// with no concurrent call on it; `sink` and `user` must stay valid until the
/// sink is replaced or the screen is freed.
#[no_mangle]
pub unsafe extern "C" fn ov_screen_set_opus_sink(
    s: *mut OvScreen,
    user: *mut c_void,
    sink: Option<OpusSink>,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        // SAFETY: forwarded caller contract (see `# Safety`).
        unsafe { set_sink(s, |s| &mut s.opus_sink, sink, user) }
    })
}

/// Wire the H.264 video sink. Set it **before** feeding the first
/// screen command, and only when the device can decode H.264 (VideoToolbox):
/// our CAPS then advertise `CAP_H264`, and a capable host streams
/// high-motion regions as [`OvVideoPacket`](crate::OvVideoPacket)s
/// (composited by the shell over the frame-sink framebuffer) instead of
/// lossless tiles. Pass a null `sink` to remove it.
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`](crate::ov_screen_new)
/// with no concurrent call on it; `sink` and `user` must stay valid until the
/// sink is replaced or the screen is freed.
#[no_mangle]
pub unsafe extern "C" fn ov_screen_set_video_sink(
    s: *mut OvScreen,
    user: *mut c_void,
    sink: Option<VideoSink>,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        // SAFETY: forwarded caller contract (see `# Safety`).
        unsafe { set_sink(s, |s| &mut s.video_sink, sink, user) }
    })
}

/// Wire the clipboard sink. Pass a null `sink` to remove it.
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`](crate::ov_screen_new)
/// with no concurrent call on it; `sink` and `user` must stay valid until the
/// sink is replaced or the screen is freed.
#[no_mangle]
pub unsafe extern "C" fn ov_screen_set_clipboard_sink(
    s: *mut OvScreen,
    user: *mut c_void,
    sink: Option<ClipboardSink>,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        // SAFETY: forwarded caller contract (see `# Safety`).
        unsafe { set_sink(s, |s| &mut s.clipboard_sink, sink, user) }
    })
}

/// Wire the file sink. Fired when a peer's file transfer completes
/// (name + reassembled bytes). Pass a null `sink` to remove it.
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`](crate::ov_screen_new)
/// with no concurrent call on it; `sink` and `user` must stay valid until the
/// sink is replaced or the screen is freed.
#[no_mangle]
pub unsafe extern "C" fn ov_screen_set_file_sink(
    s: *mut OvScreen,
    user: *mut c_void,
    sink: Option<FileSink>,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        // SAFETY: forwarded caller contract (see `# Safety`).
        unsafe { set_sink(s, |s| &mut s.file_sink, sink, user) }
    })
}

/// Wire the liveness sink. Pass a null `sink` to remove it.
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`](crate::ov_screen_new)
/// with no concurrent call on it; `sink` and `user` must stay valid until the
/// sink is replaced or the screen is freed.
#[no_mangle]
pub unsafe extern "C" fn ov_screen_set_liveness_sink(
    s: *mut OvScreen,
    user: *mut c_void,
    sink: Option<LivenessSink>,
) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        // SAFETY: forwarded caller contract (see `# Safety`).
        unsafe { set_sink(s, |s| &mut s.liveness_sink, sink, user) }
    })
}
