//! Screen and video on [`TerminalSession`]: sending full frames, and the
//! viewer side that turns reassembled updates and H.264 units into
//! framebuffer snapshots (lazily, latest-frame-only when asked).

use std::time::Instant;

use ovtransport::TransportError;

use crate::screen::{Frame, FrameHook};
use crate::tiles::{ApplyError, UpdateMeta};

use super::session::PendingFrame;
use super::{
    compress_frame, Command, FrameCodec, TerminalSession, VideoRxStats, CAP_H264, CAP_KEYFRAME,
    KEYFRAME_RETRY, MAX_LATENCY_SAMPLES, MAX_QUEUED_FRAMES, VIDEO_STALL_GAP,
};

impl TerminalSession {
    /// Capture-to-paint latency of every painted H.264 picture (us),
    /// oldest first (capped at [`MAX_LATENCY_SAMPLES`]).
    pub fn video_latencies(&self) -> &[u32] {
        &self.video_latencies
    }

    /// Install an H.264 decoder and advertise [`CAP_H264`] (call
    /// before [`announce_caps`](Self::announce_caps)). Video units are then
    /// decoded in arrival order and painted over the framebuffer that
    /// `take_*frame` snapshots.
    pub fn set_video_decoder(&mut self, dec: Box<dyn crate::video::VideoDecode>) {
        self.video_dec = Some(dec);
        self.local_caps |= CAP_H264;
    }

    /// H.264 receive counters.
    pub fn video_stats(&self) -> VideoRxStats {
        self.video_stats
    }

    /// Stream one [`Frame`]: a `FRAME_BEGIN` header, the frame's pixels in
    /// `chunk_size`-byte `FRAME_DATA` chunks, then a `FRAME_END`.
    ///
    /// The pixels are compressed with raw DEFLATE first (codec 1): the
    /// FRAME_DATA chunks carry the compressed stream and the header declares
    /// both the uncompressed `total` and the compressed length. Desktop
    /// screens are mostly flat regions, so this cuts wire bandwidth ~5–15x
    /// (a raw 1080p BGRA frame is ~8 MB ≈ 240 Mbps at 30 fps).
    ///
    /// The default chunk size (when `chunk_size` is 0) is 8 KiB — small enough
    /// that the 33-byte record overhead (header + tag + opcode) keeps each
    /// chunk's record far under the tightest carrier's DATA-frame limit (the
    /// relay tunnel caps a frame at `0xFFFF - 3` ≈ 64 KiB, the direct TCP
    /// carrier at `0xFFFF`), and small enough to give the viewer low-latency
    /// progressive updates. An explicit `chunk_size` larger than the tunnel
    /// limit is still safe here (the record is what the tunnel caps, and a
    /// single chunk's record stays well inside it at the 8 KiB default).
    pub fn send_frame(&mut self, frame: &Frame, chunk_size: usize) -> Result<(), TransportError> {
        let chunk = if chunk_size == 0 {
            8 * 1024
        } else {
            chunk_size
        };
        let total = frame.pixels.len() as u32;
        let compressed = compress_frame(&frame.pixels);
        self.send(&Command::FrameBegin {
            seq: frame.seq,
            width: frame.width,
            height: frame.height,
            stride: frame.stride,
            format: frame.format,
            total,
            codec: FrameCodec::Deflate,
            compressed_len: compressed.len() as u32,
        })?;
        for c in compressed.chunks(chunk.max(1)) {
            self.send(&Command::FrameData(c.to_vec()))?;
        }
        self.send(&Command::FrameEnd(frame.seq))
    }

    /// Install the frame receive hook; reassembled inbound frames land here.
    /// Pass `None` to clear.
    pub fn on_frame(&mut self, f: Option<FrameHook>) {
        self.on_frame = f;
    }

    /// Take the next completed inbound frame (oldest first): the
    /// framebuffer right after that update was applied. `None` when no
    /// update finished since the last call. At most [`MAX_QUEUED_FRAMES`]
    /// are queued (older full frames are dropped, older deltas applied); a
    /// renderer that only shows the newest frame should use
    /// [`take_latest_frame`](Self::take_latest_frame).
    pub fn take_frame(&mut self) -> Option<Frame> {
        while let Some(p) = self.finished_frames.pop_front() {
            match p {
                PendingFrame::Decoded(f) => return Some(f),
                PendingFrame::Wire { meta, seq, data } => {
                    let ok = self.apply_update(&meta, seq, &data);
                    self.recycle_wire(data);
                    if ok {
                        return self.copy_fb();
                    }
                }
                PendingFrame::Video { header, data } => {
                    let ok = self.apply_video(&header, &data);
                    self.recycle_wire(data);
                    if ok {
                        return self.copy_fb();
                    }
                }
            }
        }
        None
    }

    /// Take only the newest state of the screen: queued updates older than
    /// the newest full frame are discarded without decompressing them
    /// (latest-frame-only rendering: a slow consumer skips stale frames
    /// instead of falling behind); tile deltas after it are applied in
    /// order. Returns a snapshot of the framebuffer, or `None` when nothing
    /// new could be applied.
    pub fn take_latest_frame(&mut self) -> Option<Frame> {
        if let Some(i) = self.finished_frames.iter().rposition(PendingFrame::is_full) {
            for _ in 0..i {
                match self.finished_frames.pop_front() {
                    // Video units are never skipped (the decoder needs the
                    // whole chain); the full frame paints over them anyway.
                    Some(PendingFrame::Video { header, data }) => {
                        self.apply_video(&header, &data);
                        self.recycle_wire(data);
                    }
                    Some(old) => self.drop_pending(old),
                    None => {}
                }
            }
        }
        let mut out: Option<Frame> = None;
        let mut applied = false;
        while let Some(p) = self.finished_frames.pop_front() {
            match p {
                PendingFrame::Decoded(f) => {
                    if let Some(old) = out.replace(f) {
                        self.recycle_pixels(old.pixels);
                    }
                    applied = false;
                }
                PendingFrame::Wire { meta, seq, data } => {
                    if self.apply_update(&meta, seq, &data) {
                        applied = true;
                    }
                    self.recycle_wire(data);
                }
                PendingFrame::Video { header, data } => {
                    if self.apply_video(&header, &data) {
                        applied = true;
                    }
                    self.recycle_wire(data);
                }
            }
        }
        if applied {
            if let Some(old) = out.take() {
                self.recycle_pixels(old.pixels);
            }
            out = self.copy_fb();
        }
        out
    }

    /// Apply one complete update to the framebuffer; on failure count it
    /// and ask for a keyframe. Returns whether it applied.
    pub(super) fn apply_update(&mut self, meta: &UpdateMeta, seq: u64, data: &[u8]) -> bool {
        match self.screen.apply(meta, seq, data) {
            Ok(()) => true,
            Err(e) => {
                if let ApplyError::Corrupt(why) = &e {
                    tracing::debug!("frame {seq}: {why} — dropped");
                }
                self.note_desync();
                false
            }
        }
    }

    /// The framebuffer is stale or missing: request a keyframe (if the host
    /// understands the request), at most every [`KEYFRAME_RETRY`].
    pub(super) fn note_desync(&mut self) {
        self.stats.screen_desyncs += 1;
        if self.peer_caps.is_some_and(|c| c & CAP_KEYFRAME != 0) {
            let now = Instant::now();
            if self
                .last_kf_request
                .is_none_or(|t| now.saturating_duration_since(t) >= KEYFRAME_RETRY)
            {
                self.last_kf_request = Some(now);
                let have_seq = self.screen.seq().unwrap_or(0);
                if self.send(&Command::KeyframeRequest { have_seq }).is_ok() {
                    self.stats.keyframe_requests += 1;
                }
            }
        }
    }

    /// Snapshot the framebuffer into a recycled pixel buffer.
    pub(super) fn copy_fb(&mut self) -> Option<Frame> {
        let fb = self.screen.frame()?;
        let mut px = self.spare_pixels.pop().unwrap_or_default();
        px.clear();
        px.extend_from_slice(&fb.pixels);
        Some(Frame {
            width: fb.width,
            height: fb.height,
            stride: fb.stride,
            format: fb.format,
            seq: fb.seq,
            pixels: px,
        })
    }

    /// Hand a consumed frame's pixel buffer back for reuse by the next
    /// decode (optional; saves one large allocation per frame).
    pub fn recycle_frame(&mut self, frame: Frame) {
        self.recycle_pixels(frame.pixels);
    }

    pub(super) fn recycle_wire(&mut self, buf: Vec<u8>) {
        Self::recycle_into(&mut self.spare_bufs, buf);
    }

    fn recycle_pixels(&mut self, buf: Vec<u8>) {
        Self::recycle_into(&mut self.spare_pixels, buf);
    }

    fn recycle_into(pool: &mut Vec<Vec<u8>>, mut buf: Vec<u8>) {
        if pool.len() < 2 && buf.capacity() > 0 {
            buf.clear();
            pool.push(buf);
        }
    }

    fn drop_pending(&mut self, p: PendingFrame) {
        match p {
            PendingFrame::Decoded(f) => self.recycle_pixels(f.pixels),
            PendingFrame::Wire { data, .. } | PendingFrame::Video { data, .. } => {
                self.recycle_wire(data)
            }
        }
    }

    /// A complete VIDEO_BEGIN unit arrived: sequence check, then
    /// queue it for painting (or paint now when an `on_frame` hook wants
    /// every frame).
    pub(super) fn on_video_unit(
        &mut self,
        header: crate::video::VideoHeader,
        len: u32,
        end_seq: u64,
        mut data: Vec<u8>,
    ) {
        use crate::video::VideoVerdict;
        if data.len() > len as usize {
            data.truncate(len as usize);
        }
        if end_seq != header.seq || data.len() < len as usize {
            // Torn unit: the chain is broken.
            self.recycle_wire(data);
            self.video_rx.mark_broken();
            if self.video_dec.is_some() {
                self.request_video_idr();
            }
            return;
        }
        self.last_video_seq = header.seq;
        if self.video_dec.is_none() {
            // We never advertised CAP_H264: nothing to do with it.
            self.recycle_wire(data);
            return;
        }
        match self.video_rx.accept(&header) {
            VideoVerdict::End => {
                // Desktop paints video into the framebuffer itself; the
                // lossless refresh that follows overwrites it.
                self.video_stats.ends += 1;
                self.last_paint = None;
                self.recycle_wire(data);
            }
            VideoVerdict::Drop => {
                self.video_stats.dropped += 1;
                self.recycle_wire(data);
                self.request_video_idr();
            }
            VideoVerdict::Stale => {
                self.video_stats.dropped += 1;
                self.recycle_wire(data);
            }
            VideoVerdict::Deliver => {
                self.video_stats.units += 1;
                self.video_stats.bytes += data.len() as u64;
                if self.on_frame.is_some() {
                    let ok = self.apply_video(&header, &data);
                    self.recycle_wire(data);
                    if ok {
                        if let Some(frame) = self.copy_fb() {
                            if let Some(fh) = self.on_frame.as_mut() {
                                fh(&frame);
                            }
                            self.queue_frame(PendingFrame::Decoded(frame));
                        }
                    }
                } else {
                    self.queue_frame(PendingFrame::Video { header, data });
                }
            }
        }
    }

    /// Decode one access unit and paint it over the framebuffer. Returns
    /// whether the framebuffer changed.
    fn apply_video(&mut self, header: &crate::video::VideoHeader, data: &[u8]) -> bool {
        let Some(dec) = self.video_dec.as_mut() else {
            return false;
        };
        let (w, h) = (u32::from(header.rect.w), u32::from(header.rect.h));
        let t0 = Instant::now();
        match dec.decode(data, w, h, &mut self.video_px) {
            Ok(true) => {
                self.video_stats.decode_time += t0.elapsed();
                let painted = self.screen.blit(
                    header.rect,
                    header.screen_w,
                    header.screen_h,
                    &self.video_px,
                );
                if painted {
                    self.video_stats.painted += 1;
                    let lat = crate::video::unix_micros().saturating_sub(header.pts_us);
                    if lat < 10_000_000 {
                        self.video_stats.latency_sum_us += lat;
                        self.video_stats.latency_n += 1;
                        self.video_stats.latency_max_us = self.video_stats.latency_max_us.max(lat);
                        if self.video_latencies.len() < MAX_LATENCY_SAMPLES {
                            self.video_latencies.push(lat as u32);
                        }
                    }
                    let now = Instant::now();
                    if let Some(prev) = self.last_paint {
                        let gap = now.saturating_duration_since(prev);
                        if gap > VIDEO_STALL_GAP {
                            self.video_stats.stalls += 1;
                            self.video_stats.stall_us += gap.as_micros() as u64;
                        }
                    }
                    self.last_paint = Some(now);
                }
                painted
            }
            Ok(false) => false,
            Err(e) => {
                tracing::debug!("video {}: decode failed: {e}", header.seq);
                self.video_stats.decode_errors += 1;
                self.video_rx.mark_broken();
                self.request_video_idr();
                false
            }
        }
    }

    /// Ask the host for an IDR, at most every [`KEYFRAME_RETRY`].
    pub(super) fn request_video_idr(&mut self) {
        let now = Instant::now();
        if self
            .last_idr_request
            .is_some_and(|t| now.saturating_duration_since(t) < KEYFRAME_RETRY)
        {
            return;
        }
        self.last_idr_request = Some(now);
        let have_seq = self.last_video_seq;
        if self.send(&Command::VideoIdrRequest { have_seq }).is_ok() {
            self.video_stats.idr_requests += 1;
        }
    }

    /// Queue a completed update, capping the queue: past
    /// [`MAX_QUEUED_FRAMES`] the oldest entry is dropped when a newer full
    /// frame supersedes it, otherwise applied now (a delta can never be
    /// skipped without breaking the chain).
    pub(super) fn queue_frame(&mut self, p: PendingFrame) {
        self.finished_frames.push_back(p);
        while self.finished_frames.len() > MAX_QUEUED_FRAMES {
            let superseded = self
                .finished_frames
                .iter()
                .skip(1)
                .any(PendingFrame::is_full);
            let Some(old) = self.finished_frames.pop_front() else {
                break;
            };
            match old {
                PendingFrame::Wire { meta, seq, data } if !superseded => {
                    self.apply_update(&meta, seq, &data);
                    self.recycle_wire(data);
                }
                PendingFrame::Video { header, data } => {
                    self.apply_video(&header, &data);
                    self.recycle_wire(data);
                }
                other => self.drop_pending(other),
            }
        }
    }
}
