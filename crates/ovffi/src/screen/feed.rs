//! `ov_screen_feed`: decode one channel-0 command and route it to the
//! framebuffer, the video / audio pass-through, or a sink.

use super::OvScreen;
use crate::cabi::bytes;
use crate::error::OV_ERR_PANIC;

/// Feed one terminal-channel command record. Returns 0 on success, -1 on a
/// null/undecodable command. A completed frame fires the frame sink inline.
///
/// # Safety
/// `s` must be null or a live handle from [`ov_screen_new`](crate::ov_screen_new). The screen is
/// not internally locked: no other call on the same handle may run
/// concurrently (the shell drives it from one serial queue). `data` must be
/// readable for `len` bytes (or null with `len == 0`). Never call it from
/// inside one of this screen's sinks.
#[no_mangle]
pub unsafe extern "C" fn ov_screen_feed(s: *mut OvScreen, data: *const u8, len: usize) -> i32 {
    ffi_guard!(OV_ERR_PANIC, {
        // SAFETY: `s` is null or a live screen with no concurrent use, and
        // `data` is readable for `len` bytes for the whole call (caller
        // contract).
        let args = unsafe { (s.as_mut(), bytes(data, len)) };
        let (Some(screen), Some(bytes)) = args else {
            return -1;
        };
        let now = std::time::Instant::now();
        let cmd = match ovapp::terminal::decode(bytes) {
            Ok(c) => c,
            Err(_) => return -1,
        };
        match cmd {
            ovapp::terminal::Command::FrameBegin {
                seq,
                width,
                height,
                stride,
                format,
                total,
                codec,
                compressed_len,
            } => {
                screen.queue_caps();
                screen.rx = Some((
                    seq,
                    ovapp::tiles::UpdateMeta {
                        width,
                        height,
                        stride,
                        format,
                        total,
                        codec,
                        compressed_len,
                        delta: None,
                    },
                ));
                screen.rx_video = None;
                screen.buf.clear();
                screen.buf.reserve(compressed_len as usize);
            }
            ovapp::terminal::Command::TilesBegin {
                seq,
                base_seq,
                width,
                height,
                format,
                codec,
                total,
                compressed_len,
                rects,
            } => {
                screen.queue_caps();
                screen.rx = Some((
                    seq,
                    ovapp::tiles::UpdateMeta {
                        width,
                        height,
                        stride: width.saturating_mul(format.bytes_per_pixel()),
                        format,
                        total,
                        codec,
                        compressed_len,
                        delta: Some(ovapp::tiles::Delta { base_seq, rects }),
                    },
                ));
                screen.rx_video = None;
                screen.buf.clear();
                screen.buf.reserve(compressed_len as usize);
            }
            // An H.264 access unit (only after our CAP_H264).
            ovapp::terminal::Command::VideoBegin {
                header,
                compressed_len,
            } => {
                screen.queue_caps();
                screen.rx = None;
                screen.rx_video = Some((header, compressed_len));
                screen.buf.clear();
                screen.buf.reserve(compressed_len.min(8 << 20) as usize);
            }
            ovapp::terminal::Command::FrameData(chunk) => {
                if screen.rx.is_none() && screen.rx_video.is_none() {
                    return -1; // FRAME_DATA before FRAME_BEGIN
                }
                screen.buf.extend_from_slice(&chunk);
            }
            ovapp::terminal::Command::FrameEnd(seq) => {
                if let Some((h, len)) = screen.rx_video.take() {
                    screen.live.record_inbound(now);
                    let n = len as usize;
                    if seq != h.seq || screen.buf.len() < n {
                        screen.buf.clear();
                        screen.last_video_seq = h.seq;
                        screen.video_rx.mark_broken();
                        if screen.video_sink.f.is_some() {
                            screen.request_idr();
                        }
                        return -1;
                    }
                    screen.stats.frame_rx(now, n as u64);
                    let data = std::mem::take(&mut screen.buf);
                    screen.video_unit(h, &data[..n]);
                    screen.buf = data;
                    screen.buf.clear();
                    return 0;
                }
                let Some((open_seq, meta)) = screen.rx.take() else {
                    return -1;
                };
                screen.live.record_inbound(now);
                let n = meta.compressed_len as usize;
                if seq != open_seq || screen.buf.len() < n {
                    // Torn/short update: the delta chain is broken.
                    screen.buf.clear();
                    screen.desync();
                    return -1;
                }
                screen.buf.truncate(n);
                screen.stats.frame_rx(now, u64::from(meta.total));
                // Decode (DEFLATE / zstd / raw) and apply onto the persistent
                // framebuffer; a corrupt update or a delta on the wrong base
                // leaves it untouched (never a torn frame) and asks for a
                // keyframe.
                let res = screen.dec.apply(&meta, seq, &screen.buf);
                screen.buf.clear();
                if res.is_err() {
                    screen.desync();
                    return -1;
                }
                if let (Some(f), Some(fb)) = (screen.sink.f, screen.dec.frame()) {
                    f(
                        screen.sink.user,
                        fb.seq,
                        fb.width,
                        fb.height,
                        fb.stride,
                        fb.format.tag(),
                        fb.pixels.as_ptr(),
                        fb.pixels.len(),
                    );
                }
            }
            // CAPS: the host's capability reply.
            ovapp::terminal::Command::Caps { flags } => {
                screen.live.record_inbound(now);
                screen.peer_caps = flags;
            }
            // ECHO / TYPE / FILE_* / PING / PONG ride the same channel.
            // Liveness: any inbound record counts as fresh traffic; a
            // PONG closes the in-flight PING and measures the RTT; a PING is
            // answered by the shell via [`ov_screen_take_pong_reply`]. Frames
            // feed the FPS/bandwidth stats.
            ovapp::terminal::Command::Pong(seq) => {
                screen.live.record_inbound(now);
                // RTT is measured against the real send instant (feed has no
                // shell clock); the shell ms only paces the keepalive.
                if let Some(sent_at) = screen.ping_sent_real.take() {
                    let rtt = std::time::Instant::now().saturating_duration_since(sent_at);
                    if screen.keepalive.on_pong(seq, sent_at + rtt).is_some() {
                        screen.last_rtt_ms = rtt.as_millis().min(i32::MAX as u128) as i32;
                    }
                }
            }
            ovapp::terminal::Command::Ping(nonce) => {
                screen.live.record_inbound(now);
                screen.pong_reply_nonce = Some(nonce);
            }
            ovapp::terminal::Command::Echo(_) | ovapp::terminal::Command::Type { .. } => {
                screen.live.record_inbound(now);
            }
            ovapp::terminal::Command::MonitorsInfo { monitors } => {
                screen.live.record_inbound(now);
                screen.queue_caps();
                if let Some(f) = screen.monitors_sink.f {
                    // Packed 17-byte records: i32 x, i32 y, u32 w, u32 h, u8
                    // is_primary (big-endian, matching the wire layout). The iOS
                    // onMonitors unpack MUST use the same stride (17) — a mismatch
                    // misaligns every record after the first on multi-monitor hosts.
                    let mut packed = Vec::with_capacity(monitors.len() * 17);
                    for m in &monitors {
                        packed.extend_from_slice(&m.x.to_be_bytes());
                        packed.extend_from_slice(&m.y.to_be_bytes());
                        packed.extend_from_slice(&m.w.to_be_bytes());
                        packed.extend_from_slice(&m.h.to_be_bytes());
                        packed.push(u8::from(m.is_primary));
                    }
                    f(screen.monitors_sink.user, packed.as_ptr(), monitors.len());
                }
            }
            ovapp::terminal::Command::Cursor { x, y } => {
                if let Some(f) = screen.cursor_sink.f {
                    f(screen.cursor_sink.user, x, y);
                }
            }
            ovapp::terminal::Command::CursorShape {
                hotspot_x,
                hotspot_y,
                w,
                h,
                bgra,
            } => {
                if let Some(f) = screen.cursor_shape_sink.f {
                    f(
                        screen.cursor_shape_sink.user,
                        hotspot_x,
                        hotspot_y,
                        w,
                        h,
                        bgra.as_ptr(),
                        bgra.len(),
                    );
                }
            }
            ovapp::terminal::Command::Clipboard { kind, text } => {
                screen.live.record_inbound(now);
                if let Some(f) = screen.clipboard_sink.f {
                    f(screen.clipboard_sink.user, kind, text.as_ptr(), text.len());
                }
            }
            // File transfer: the reassembly mirrors the desktop
            // `TerminalSession` (chunks stay out of any command hook; the completed
            // file is surfaced through the file sink). Malformed inbound transfer
            // data is dropped and reported as -1, like a corrupt frame.
            ovapp::terminal::Command::FileXferData { .. }
            | ovapp::terminal::Command::FileXferComplete { .. } => {
                screen.live.record_inbound(now);
                let malformed = screen.xfer.poll(&cmd).is_err();
                while let Some(file) = screen.xfer.take() {
                    if let Some(f) = screen.file_sink.f {
                        let name = ovapp::file_transfer::FileTransfer::sanitize_name(&file.name);
                        if let Ok(cname) = std::ffi::CString::new(name) {
                            f(
                                screen.file_sink.user,
                                cname.as_ptr(),
                                file.data.as_ptr(),
                                file.data.len(),
                            );
                        } // unreachable Err: sanitize removes control characters
                    }
                }
                if malformed {
                    return -1;
                }
            }
            ovapp::terminal::Command::Audio(payload) => {
                screen.live.record_inbound(now);
                // Raw PCM (codec 0): the sink gets the s16le sample bytes (the
                // shell converts to Float32). Opus (codec 1, only sent after our
                // CAP_OPUS): the packet is passed through to the Opus sink.
                match ovapp::audio::parse_audio_frame(payload.as_slice()) {
                    Ok((_, ovapp::audio::AudioPayload::Pcm(samples))) => {
                        if let Some(f) = screen.audio_sink.f {
                            let ptr = samples.as_ptr().cast::<u8>();
                            let n = samples.len() * 2;
                            f(screen.audio_sink.user, ptr, n);
                        }
                    }
                    Ok((_, ovapp::audio::AudioPayload::Opus { samples, packet })) => {
                        if let Some(f) = screen.opus_sink.f {
                            f(
                                screen.opus_sink.user,
                                packet.as_ptr(),
                                packet.len(),
                                samples,
                            );
                        }
                    }
                    Err(_) => {}
                }
            }
            // A whole access unit from the direct path's media channel
            // (the session synthesizes it; never mixed into `buf`, so a tile
            // update in progress is untouched).
            ovapp::terminal::Command::VideoUnit { header, data } => {
                screen.live.record_inbound(now);
                screen.stats.frame_rx(now, data.len() as u64);
                screen.video_unit(header, &data);
            }
            // Media lost for good. Video: drop P-frames until the next
            // IDR and ask for one. Audio: the Opus sink gets a NULL packet with
            // the lost sample count (the shell conceals / skips that span).
            ovapp::terminal::Command::MediaGap { kind, samples, .. } => {
                if kind == ovapp::terminal::GAP_VIDEO {
                    screen.video_rx.mark_broken();
                    if screen.video_sink.f.is_some() {
                        screen.request_idr();
                    }
                } else if kind == ovapp::terminal::GAP_AUDIO {
                    if let Some(f) = screen.opus_sink.f {
                        f(screen.opus_sink.user, std::ptr::null(), 0, samples);
                    }
                }
            }
            _ => {}
        }
        0
    })
}
