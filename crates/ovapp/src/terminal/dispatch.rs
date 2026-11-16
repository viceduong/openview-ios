//! The receive side of [`TerminalSession`]: [`TerminalSession::pump`]
//! decrypts and decodes every inbound record, keeps the per-command session
//! state (auto-replies, CAPS, reassembly) and forwards commands to the hook.

use std::time::Instant;

use ovtransport::TransportError;

use crate::latency::{is_clock_probe, KIND_SCREEN};
use crate::tiles::{Delta, UpdateMeta};

use super::session::PendingFrame;
use super::{
    candidate_kind, decode, Command, TerminalSession, CAP_DIRECT, CAP_MEDIA, CAP_RUDP, CHANNEL,
    GAP_AUDIO, GAP_VIDEO,
};

impl TerminalSession {
    /// Pump the carrier: drain pending datagrams, decrypt each into a command,
    /// dispatch to the hook. Auto-replies:
    /// - PING -> PONG (same nonce),
    /// - ECHO -> ECHO (same payload).
    ///
    /// Returns the number of datagrams consumed. Non-blocking (carrier contract).
    /// Once the peer sent BYE ([`Command::Bye`]) the batch carrying it is
    /// still dispatched, then this (and every later call) returns
    /// [`TransportError::Closed`] ([`peer_said_goodbye`](Self::peer_said_goodbye)
    /// tells it apart from a lost carrier).
    pub fn pump(&mut self) -> Result<usize, TransportError> {
        if self.peer_bye {
            return Err(TransportError::Closed);
        }
        let mut n = 0usize;
        // The hook is driven through a scratch buffer so `self` stays free for
        // the auto-replies the hook may trigger.
        let mut inbox: Vec<Command> = Vec::new();
        let backlog = std::mem::take(&mut self.backlog);
        let mut on_datagram = |datagram: &[u8]| {
            n += 1;
            match self.session.decrypt_record(CHANNEL, datagram) {
                Ok(plain) => match decode(&plain) {
                    Ok(cmd) => inbox.push(cmd),
                    Err(_) => self.stats.decode_errors += 1,
                },
                // A duplicate / stale record (e.g. delivered on both paths
                // across a direct-path switch): dropped, only counted.
                Err(e) if e.is_replay() => self.stats.replayed += 1,
                Err(_) => self.stats.decrypt_errors += 1,
            }
        };
        for d in &backlog {
            on_datagram(d);
        }
        self.carrier.poll_recv(&mut on_datagram)?;
        // One receive stamp per batch (PING t2, PONG t4, reassembly).
        let rx_at = Instant::now();
        let rx_us = ovcore::clocksync::instant_us(rx_at);
        let timed = self.timing_enabled();
        for cmd in &inbox {
            self.stats.cmds_rx += 1;
            match cmd {
                Command::Ping(nonce) => {
                    let reply = if timed {
                        Command::PongTimed {
                            nonce: *nonce,
                            rx_us,
                            tx_us: ovcore::clocksync::mono_us(),
                        }
                    } else {
                        Command::Pong(*nonce)
                    };
                    let _ = self.send(&reply);
                }
                Command::PongTimed {
                    nonce,
                    rx_us: t2,
                    tx_us: t3,
                } => {
                    self.clock.on_pong(*nonce, *t2, *t3, rx_us);
                    if !is_clock_probe(*nonce) {
                        self.last_pong = Some(*nonce);
                    }
                }
                Command::Timing(t) => {
                    if self.latency_on {
                        self.latency.on_timing(*t);
                    }
                }
                Command::Echo(payload) => {
                    let _ = self.send(&Command::Echo(payload.clone()));
                }
                Command::FileBegin { size, name } => {
                    self.rx_file = Some((name.clone(), *size));
                    self.rx_file_data.clear();
                }
                Command::FileData(chunk) => self.rx_file_data.extend_from_slice(chunk),
                Command::FileEnd => {
                    // Finalize the reassembly, then still deliver FILE_END to
                    // the hook so it can react to the completion event.
                    if let Some((name, _size)) = self.rx_file.take() {
                        let data = std::mem::take(&mut self.rx_file_data);
                        self.finished_files.push_back((name, data));
                    }
                }
                // File-id-keyed transfers: the pump reassembles the bytes
                // via the pure `file_transfer` module (order-tolerant,
                // multiple concurrent transfers) and hands completed files
                // to `take_xfer_file`; FILE_XFER_COMPLETE still reaches the
                // hook as the completion event (FILE_XFER_DATA does not, see
                // below).
                Command::FileXferData { .. } | Command::FileXferComplete { .. } => {
                    // Malformed inbound transfer data is dropped (with a
                    // diagnostic), never a panic — same posture as frames.
                    if let Err(e) = self.xfer.poll(cmd) {
                        tracing::warn!("file transfer: {e} — dropped");
                    }
                }
                Command::Pong(nonce) => {
                    if !is_clock_probe(*nonce) {
                        self.last_pong = Some(*nonce);
                    }
                }
                // CAPS: remember the peer's bits and answer with
                // ours once (the viewer announces first; the host replies).
                Command::Caps { flags } => {
                    self.peer_caps = Some(*flags);
                    // The peer receives reliable UDP framing —
                    // switch our datagram sending to it (no-op on lossless
                    // carriers). Before the reply, so the reply is framed.
                    if flags & CAP_RUDP != 0 && self.local_caps & CAP_RUDP != 0 {
                        self.carrier.enable_reliable();
                    }
                    // Both speak the v2 direct framing — open the
                    // media channel (before the direct path is armed, so
                    // the link is v2 from its first datagram).
                    if flags & CAP_MEDIA != 0 && self.local_caps & CAP_MEDIA != 0 {
                        let keys = ovtransport::media::session_media_keys(&self.session);
                        if let Some(d) = self.carrier.direct_mut() {
                            d.enable_media(keys);
                        }
                    }
                    if !self.caps_sent {
                        self.caps_sent = true;
                        let flags = self.local_caps;
                        let _ = self.send(&Command::Caps { flags });
                    }
                    // Both sides can go direct - arm the engine;
                    // the host then offers its candidates (after its CAPS
                    // reply, so the viewer is armed when they arrive).
                    if flags & CAP_DIRECT != 0 && self.local_caps & CAP_DIRECT != 0 {
                        let mut offer = None;
                        if let Some(d) = self.carrier.direct_mut() {
                            let first = !d.is_armed();
                            d.arm();
                            if first && d.is_server() && !d.candidates().is_empty() {
                                offer = Some(d.candidates().to_vec());
                            }
                        }
                        if let Some(addrs) = offer {
                            let cands = addrs.iter().map(|a| (candidate_kind(a), *a)).collect();
                            let _ = self.send(&Command::DirectCandidates { cands });
                        }
                    }
                }
                Command::DirectCandidates { cands } => {
                    if let Some(d) = self.carrier.direct_mut() {
                        if !d.is_server() && d.is_armed() {
                            let addrs: Vec<std::net::SocketAddr> = cands.iter().map(|(_, a)| *a).collect();
                            d.start_probe(&addrs);
                        }
                    }
                }
                Command::SessionOption { .. } => {}
                Command::Bye => self.peer_bye = true,
                // Local media commands (produced from the media channel; a
                // peer never sends them, but one that does is handled the
                // same way).
                Command::VideoUnit { header, data } => {
                    let spare = self.spare_bufs.pop().unwrap_or_default();
                    let mut buf = spare;
                    buf.clear();
                    buf.extend_from_slice(data);
                    self.on_video_unit(*header, data.len() as u32, header.seq, buf);
                }
                Command::MediaGap { kind, .. } => {
                    if *kind == GAP_VIDEO {
                        self.video_rx.mark_broken();
                        if self.video_dec.is_some() {
                            self.request_video_idr();
                        }
                    }
                }
                // KEYFRAME_REQUEST is viewer->host state for the capture
                // loop: pure pass-through to the hook (so are
                // VIDEO_LTR_ACK and the recovery requests, which also drop
                // the video still queued on the media channel).
                Command::KeyframeRequest { .. } | Command::VideoLtrAck { .. } => {}
                Command::VideoIdrRequest { .. } | Command::VideoRecover { .. } => {
                    if let Some(d) = self.carrier.direct_mut() {
                        d.drop_queued_video();
                    }
                }
                Command::VideoBegin {
                    header,
                    compressed_len,
                } => {
                    self.rx_frame = None;
                    self.rx_video = Some((*header, *compressed_len));
                    self.rx_frame_data.clear();
                }
                Command::TilesBegin {
                    seq: _,
                    base_seq,
                    width,
                    height,
                    format,
                    codec,
                    total,
                    compressed_len,
                    rects,
                } => {
                    self.rx_frame = Some(UpdateMeta {
                        width: *width,
                        height: *height,
                        stride: width.saturating_mul(format.bytes_per_pixel()),
                        format: *format,
                        total: *total,
                        codec: *codec,
                        compressed_len: *compressed_len,
                        delta: Some(Delta {
                            base_seq: *base_seq,
                            rects: rects.clone(),
                        }),
                    });
                    self.rx_frame_data.clear();
                }
                Command::Type { .. } => {}
                // Input events carry no session state: no auto-reply, no
                // reassembly. They flow straight to the `on_cmd` hook below.
                // Cursor commands are host->viewer display state: also pure
                // pass-through (no auto-reply, no reassembly).
                Command::Pointer { .. }
                | Command::Key { .. }
                | Command::PointerEx { .. }
                | Command::Wheel { .. }
                | Command::KeyEx { .. }
                | Command::Text(_)
                | Command::ReleaseAll
                | Command::Sas
                | Command::Cursor { .. }
                | Command::CursorShape { .. }
                // QUALITY_SET is viewer->host state: pure pass-through to the
                // hook (the host's capture loop reads it there).
                | Command::QualitySet { .. }
                // VIEWPORT likewise (the host's capture loop reads it).
                | Command::Viewport { .. }
                // CAPTURE_REGION is viewer->host state (the host's capture
                // loop applies it on its next tick); MONITORS_INFO is
                // host->viewer display state. Both are pure pass-through.
                | Command::CaptureRegion { .. }
                | Command::MonitorsInfo { .. }
                // AUDIO is host->viewer media: pure pass-through to
                // the hook (the viewer's speaker sink drains it there).
                | Command::Audio(_)
                // CLIPBOARD is bidirectional state: pure pass-through
                // to the hook (each endpoint's clipboard sink applies it and
                // feeds the echo-loop guard there).
                | Command::Clipboard { .. } => {}
                Command::FrameBegin {
                    seq: _,
                    width,
                    height,
                    stride,
                    format,
                    total,
                    codec,
                    compressed_len,
                } => {
                    self.rx_frame = Some(UpdateMeta {
                        width: *width,
                        height: *height,
                        stride: *stride,
                        format: *format,
                        total: *total,
                        codec: *codec,
                        compressed_len: *compressed_len,
                        delta: None,
                    });
                    self.rx_frame_data.clear();
                }
                Command::FrameData(chunk) => self.rx_frame_data.extend_from_slice(chunk),
                Command::FrameEnd(seq) => {
                    if let Some((header, len)) = self.rx_video.take() {
                        let spare = self.spare_bufs.pop().unwrap_or_default();
                        let data = std::mem::replace(&mut self.rx_frame_data, spare);
                        self.on_video_unit(header, len, *seq, data);
                    } else if let Some(meta) = self.rx_frame.take() {
                        let spare = self.spare_bufs.pop().unwrap_or_default();
                        let mut data = std::mem::replace(&mut self.rx_frame_data, spare);
                        // The wire stream must be exactly `compressed_len`
                        // bytes; drop any over-streamed tail.
                        if data.len() > meta.compressed_len as usize {
                            data.truncate(meta.compressed_len as usize);
                        }
                        if self.latency_on && data.len() >= meta.compressed_len as usize {
                            self.latency.on_received(KIND_SCREEN, *seq, rx_at);
                        }
                        if data.len() < meta.compressed_len as usize {
                            tracing::debug!(
                                "frame {seq}: got {} wire bytes, declared {} — dropped",
                                data.len(),
                                meta.compressed_len
                            );
                            self.recycle_wire(data);
                            // A lost update breaks the delta chain.
                            self.note_desync();
                        } else if self.on_frame.is_some() {
                            // A hook wants every frame: apply eagerly and
                            // queue a snapshot.
                            let ok = self.apply_update(&meta, *seq, &data);
                            self.recycle_wire(data);
                            if ok {
                                if self.latency_on {
                                    self.latency
                                        .on_decoded(KIND_SCREEN, *seq, Instant::now());
                                }
                                if let Some(frame) = self.copy_fb() {
                                    if let Some(fh) = self.on_frame.as_mut() {
                                        fh(&frame);
                                    }
                                    self.queue_frame(PendingFrame::Decoded(frame));
                                }
                            }
                        } else {
                            // Lazy: inflate only when (and if) it is taken.
                            self.queue_frame(PendingFrame::Wire {
                                meta,
                                seq: *seq,
                                data,
                            });
                        }
                    }
                }
            }
            // Frame commands are reassembled into the screen path and are not
            // forwarded to the command hook (a frame is hundreds of chunks;
            // surfacing each to `on_cmd` would drown the terminal path).
            // FILE_XFER_DATA chunks likewise stay out of the hook (they are
            // already reassembled by the pump); FILE_XFER_COMPLETE is the
            // completion event and still reaches the hook.
            // A timed PONG reaches the hook as a plain PONG (clock-probe
            // echoes not at all); TIMING records stay in the session.
            let plain;
            let cmd = if let Command::PongTimed { nonce, .. } = cmd {
                plain = Command::Pong(*nonce);
                &plain
            } else {
                cmd
            };
            let internal = match cmd {
                Command::Pong(n) => is_clock_probe(*n),
                Command::Timing(_) => true,
                _ => false,
            };
            if !internal
                && !matches!(
                    cmd,
                    Command::FrameBegin { .. }
                        | Command::TilesBegin { .. }
                        | Command::VideoBegin { .. }
                        | Command::VideoUnit { .. }
                        | Command::FrameData(_)
                        | Command::FrameEnd(_)
                        | Command::FileXferData { .. }
                )
            {
                if let Some(hook) = self.on_cmd.as_mut() {
                    hook(cmd);
                }
            }
        }
        self.pump_media();
        self.clock_probe_tick(Instant::now());
        if self.peer_bye {
            return Err(TransportError::Closed);
        }
        Ok(n)
    }

    /// Hand what the media channel delivered to the video path and
    /// the command hook (audio: a [`Command::MediaGap`] before an
    /// [`Command::Audio`] that follows lost packets).
    fn pump_media(&mut self) {
        let Some(d) = self.carrier.direct_mut() else {
            return;
        };
        let events = d.take_media_events();
        for ev in events {
            match ev {
                ovtransport::media::MediaEvent::Video(mut unit) => {
                    let n = crate::video::VIDEO_HEADER_LEN;
                    if unit.len() < n {
                        continue;
                    }
                    let Ok((header, len)) = crate::video::VideoHeader::decode(&unit[..n]) else {
                        continue;
                    };
                    let data = unit.split_off(n);
                    self.media_rx.0 += 1;
                    self.on_video_unit(header, len, header.seq, data);
                }
                ovtransport::media::MediaEvent::VideoLost { frames } => {
                    self.media_rx.1 += u64::from(frames);
                    self.video_rx.mark_broken();
                    if self.video_dec.is_some() {
                        self.request_video_idr();
                    }
                }
                // Host: the viewer's PLI (the engine already dropped the
                // queued video) reaches the hook as the reliable request.
                ovtransport::media::MediaEvent::PictureLoss(r) => {
                    self.stats.cmds_rx += 1;
                    let cmd = Command::from(r);
                    if let Some(hook) = self.on_cmd.as_mut() {
                        hook(&cmd);
                    }
                }
                // Viewer: no answer to the PLI repeats (all lost on a
                // static screen, say) — the same request once reliably,
                // unless a way back in arrived meanwhile.
                ovtransport::media::MediaEvent::PictureUnanswered(r) => {
                    if self.video_rx.is_broken() {
                        let _ = self.send(&Command::from(r));
                    }
                }
                ovtransport::media::MediaEvent::Audio { lost, payload } => {
                    self.stats.cmds_rx += 1;
                    self.media_rx.2 += 1;
                    let Some(hook) = self.on_cmd.as_mut() else {
                        continue;
                    };
                    if lost > 0 {
                        self.media_rx.3 += u64::from(lost);
                        let per = match crate::audio::parse_audio_frame(&payload) {
                            Ok((_, crate::audio::AudioPayload::Opus { samples, .. })) => samples,
                            Ok((_, crate::audio::AudioPayload::Pcm(s))) => s.len() as u32,
                            Err(_) => crate::audio::OPUS10_SAMPLES,
                        };
                        let packets = lost.min(u32::from(u16::MAX));
                        hook(&Command::MediaGap {
                            kind: GAP_AUDIO,
                            packets: packets as u16,
                            samples: per.saturating_mul(packets),
                        });
                    }
                    hook(&Command::Audio(payload));
                }
            }
        }
    }
}
