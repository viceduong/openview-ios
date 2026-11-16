//! The host video session: drives the motion detector, owns the encoder
//! (codec choice, LTR loss recovery, stable encode surface) and queues
//! access units in the frame mailbox.

use std::time::{Duration, Instant};

use super::{
    coded_rect, tiles_in_rect, unix_micros, EncoderFactory, EncoderSpec, FeedbackState, LtrTracker,
    MotionConfig, MotionDetector, MotionEvent, VideoCodec, VideoEncode, VideoHeader,
    ENCODER_BUCKET, LTR_SLOTS, MAX_VIDEO_QUEUE, VIDEO_END, VIDEO_KEYFRAME,
};
use crate::sendq::{EncodedFrame, FrameMailbox};
use crate::tiles::{TileMap, TileRect, TILE};

/// Counters of a [`VideoSession`].
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct VideoStats {
    /// Access units queued.
    pub frames: u64,
    /// Of which keyframes (IDR / IRAP).
    pub keyframes: u64,
    /// Annex-B bytes queued.
    pub bytes: u64,
    /// Total time spent in the encoder.
    pub encode_time: Duration,
    /// Motion periods started.
    pub enters: u64,
    /// Motion-rect growths.
    pub grows: u64,
    /// Motion periods ended (back to tiles).
    pub exits: u64,
    /// Captures not encoded because the video queue was full.
    pub skipped: u64,
    /// Encoder creation / encode failures (each falls back to tiles).
    pub encoder_failures: u64,
    /// Encoders created (each starts with a keyframe).
    pub encoders: u64,
    /// Pictures marked as long-term references.
    pub ltr_marks: u64,
    /// Recovery pictures sent in answer to `VIDEO_RECOVER` (no IDR).
    pub recoveries: u64,
    /// `VIDEO_RECOVER`s that had to be answered with an IDR.
    pub recover_idrs: u64,
    /// `VIDEO_IDR_REQUEST`s honoured.
    pub idr_requests: u64,
}

/// The cached encoder and what it was built for.
pub(super) struct CachedEncoder {
    pub(super) spec: EncoderSpec,
    pub(super) enc: Box<dyn VideoEncode>,
}

/// Host-side video state: drives the [`MotionDetector`], owns the encoder
/// (created lazily through the factory, cached across motion periods of
/// the same size) and queues access units / END markers in the
/// [`FrameMailbox`] video FIFO.
///
/// Per capture the host calls, in order:
/// 1. `UpdateEncoder::submit_ex` with `active_rect()` as the mask,
/// 2. [`on_capture`](Self::on_capture) with the encoder's fresh-tile map,
/// 3. when [`take_refresh`](Self::take_refresh) yields a rect, hand it to
///    `UpdateEncoder::queue_refresh` (a lossless backlog ordered after
///    `VIDEO_END`, drained by the following submits).
///
/// Viewer feedback (IDR requests, `VIDEO_RECOVER`, `VIDEO_LTR_ACK`) goes in
/// through [`apply_feedback`](Self::apply_feedback) (or the single-event
/// methods) before `on_capture`.
pub struct VideoSession {
    detector: MotionDetector,
    pub(super) factory: EncoderFactory,
    /// The cached encoder.
    pub(super) enc: Option<CachedEncoder>,
    /// LTR bookkeeping of the cached encoder.
    ltr: LtrTracker,
    /// Tile-aligned motion rect (the mask), while in motion.
    active: Option<TileRect>,
    /// The screen size `active` refers to.
    active_screen: (u32, u32),
    /// The encoder was used in the current motion period (growth past it
    /// then jumps straight to the full screen, see `encoder_rect`).
    episode_encoded: bool,
    seq: u64,
    want_idr: bool,
    refresh: Option<TileRect>,
    retry_at: Option<Instant>,
    fps: u32,
    /// The pacer's delivery-rate estimate (bytes/s), if any.
    link_rate: Option<f64>,
    applied_bitrate: u32,
    stats: VideoStats,
    last_error: Option<String>,
    /// The GPU copy of the current capture (see
    /// [`set_gpu_frame`](Self::set_gpu_frame)).
    gpu_frame: Option<Box<dyn std::any::Any>>,
    /// Bits per pixel per frame of the area-based bitrate (default 0.2).
    bpp: f64,
    /// The viewer decodes HEVC (`CAP_HEVC`).
    peer_hevc: bool,
    /// The viewer speaks LTR recovery (`CAP_LTR`).
    peer_ltr: bool,
    /// An HEVC encoder could not be opened: H.264 for the session.
    hevc_failed: bool,
    /// Stable encode surface during motion (growth past the encoder jumps
    /// to the full screen once); `false` = re-open per 128 px bucket.
    fixed_surface: bool,
}

impl VideoSession {
    /// A session with the default [`MotionConfig`].
    pub fn new(factory: EncoderFactory, fps: u32) -> Self {
        Self::with_config(factory, fps, MotionConfig::default())
    }

    /// A session with explicit motion thresholds.
    pub fn with_config(factory: EncoderFactory, fps: u32, cfg: MotionConfig) -> Self {
        Self {
            detector: MotionDetector::new(cfg),
            factory,
            enc: None,
            ltr: LtrTracker::new(0, fps),
            active: None,
            active_screen: (0, 0),
            episode_encoded: false,
            seq: 0,
            want_idr: false,
            refresh: None,
            retry_at: None,
            fps: fps.max(1),
            link_rate: None,
            applied_bitrate: 0,
            stats: VideoStats::default(),
            last_error: None,
            gpu_frame: None,
            bpp: 0.2,
            peer_hevc: false,
            peer_ltr: false,
            hevc_failed: false,
            fixed_surface: true,
        }
    }

    /// The area-based bitrate budget in bits per pixel per frame
    /// (default 0.2; benches raise it).
    pub fn set_bits_per_pixel(&mut self, bpp: f64) {
        if bpp.is_finite() && bpp > 0.0 {
            self.bpp = bpp;
        }
    }

    /// What the viewer's CAPS allow: HEVC (`CAP_HEVC`) and LTR recovery
    /// (`CAP_LTR`). A change takes effect with the next encoder (the
    /// cached one is dropped when it no longer matches).
    pub fn set_peer_caps(&mut self, hevc: bool, ltr: bool) {
        self.peer_hevc = hevc;
        self.peer_ltr = ltr;
    }

    /// Stable encode surface during motion (default on): within a motion
    /// period, a growth past the encoder re-opens it once at the full
    /// screen (and the encoder is then kept until the period ends) instead
    /// of one re-open + IDR per 128 px bucket. Tests compare both modes.
    #[cfg(test)]
    pub(crate) fn set_fixed_surface(&mut self, on: bool) {
        self.fixed_surface = on;
    }

    /// The codec new encoders are opened with.
    pub fn wanted_codec(&self) -> VideoCodec {
        if self.peer_hevc && !self.hevc_failed {
            VideoCodec::Hevc
        } else {
            VideoCodec::H264
        }
    }

    /// The region currently streamed as video (mask it out of tile updates).
    pub fn active_rect(&self) -> Option<TileRect> {
        self.active
    }

    /// A region that left video mode and needs a lossless tile refresh.
    pub fn take_refresh(&mut self) -> Option<TileRect> {
        self.refresh.take()
    }

    /// Next encoded picture is a keyframe (`VIDEO_IDR_REQUEST`).
    pub fn request_idr(&mut self) {
        if self.active.is_some() {
            self.want_idr = true;
            self.stats.idr_requests += 1;
        }
    }

    /// `VIDEO_RECOVER(ltr_seq)`: the next picture recovers from that
    /// long-term reference, or is a keyframe when the encoder cannot.
    pub fn request_recover(&mut self, ltr_seq: u64) {
        if self.active.is_none() {
            return;
        }
        if self.ltr.enabled() {
            self.ltr.request_recover(ltr_seq);
        } else {
            self.want_idr = true;
            self.stats.recover_idrs += 1;
        }
    }

    /// `VIDEO_LTR_ACK(seq)`: the viewer holds that long-term reference.
    pub fn ltr_ack(&mut self, seq: u64) {
        self.ltr.ack(seq);
    }

    /// Apply everything a [`super::VideoFeedback`] collected.
    pub fn apply_feedback(&mut self, fb: FeedbackState) {
        for seq in fb.acks {
            self.ltr_ack(seq);
        }
        if fb.idr {
            self.request_idr();
        } else if let Some(seq) = fb.recover {
            self.request_recover(seq);
        }
    }

    /// The link pacer's delivery-rate estimate (bytes/s; `None` = none
    /// yet): the encoder's bitrate follows [`target_bitrate`] of it.
    pub fn set_link_rate(&mut self, bytes_per_sec: Option<f64>) {
        self.link_rate = bytes_per_sec;
    }

    /// The bitrate the encoder was last set to (bits/s; 0 = no encoder).
    pub fn bitrate(&self) -> u32 {
        self.applied_bitrate
    }

    /// The GPU-resident copy of the next capture (the same
    /// pixels as `on_capture`'s `px`), used by encoders that take
    /// [`VideoEncode::encode_gpu`]. `None` = CPU pixels only.
    pub fn set_gpu_frame(&mut self, frame: Option<Box<dyn std::any::Any>>) {
        self.gpu_frame = frame;
    }

    /// The capture rate (fps) used for bitrate targets and new encoders.
    pub fn set_fps(&mut self, fps: u32) {
        self.fps = fps.max(1);
    }

    /// The counters so far.
    pub fn stats(&self) -> VideoStats {
        let mut s = self.stats;
        s.recoveries += self.ltr.recoveries;
        s.recover_idrs += self.ltr.recover_idrs;
        s
    }

    /// The encoder in use (for logs).
    pub fn encoder_name(&self) -> Option<String> {
        self.enc.as_ref().map(|c| {
            let ltr = c.enc.ltr_slots();
            if ltr > 0 {
                format!("{} [{}, LTR x{ltr}]", c.enc.name(), c.enc.codec())
            } else {
                format!("{} [{}]", c.enc.name(), c.enc.codec())
            }
        })
    }

    /// The last encoder error (cleared when read).
    pub fn take_error(&mut self) -> Option<String> {
        self.last_error.take()
    }

    /// Stop video mode (monitor switch, viewer without H.264, ...): queue
    /// `VIDEO_END`; with `refresh` the region also gets a lossless refresh.
    pub fn stop(&mut self, mailbox: &FrameMailbox, refresh: bool) {
        self.detector.reset();
        self.end(mailbox, refresh);
    }

    fn end(&mut self, mailbox: &FrameMailbox, refresh: bool) {
        let Some(r) = self.active.take() else {
            return;
        };
        let (sw, sh) = self.active_screen;
        let codec = self
            .enc
            .as_ref()
            .map_or(VideoCodec::H264, |c| c.enc.codec());
        let header = VideoHeader {
            seq: self.seq,
            flags: VIDEO_END,
            codec: codec.wire(),
            rect: r,
            screen_w: sw,
            screen_h: sh,
            pts_us: unix_micros(),
        };
        self.seq = self.seq.wrapping_add(1);
        mailbox.push_video(EncodedFrame::video(header, Vec::new()));
        if refresh {
            self.refresh = Some(r);
        }
        self.want_idr = false;
        self.episode_encoded = false;
        // The next period starts with a keyframe: every LTR is gone.
        self.ltr.reset();
        self.stats.exits += 1;
    }

    /// One capture: `px` is the `w x h` screen (`stride` bytes per row),
    /// `fresh` the tiles that changed since the previous capture
    /// (`fresh_valid` false when it was force-filled, e.g. a keyframe).
    /// `enabled` = the viewer advertised H.264. Returns whether a video
    /// picture was encoded (the capture counts as a change).
    #[allow(clippy::too_many_arguments)]
    pub fn on_capture(
        &mut self,
        now: Instant,
        px: &[u8],
        w: u32,
        h: u32,
        stride: usize,
        fresh: &TileMap,
        fresh_valid: bool,
        enabled: bool,
        mailbox: &FrameMailbox,
    ) -> bool {
        if !enabled {
            if self.active.is_some() {
                self.stop(mailbox, true);
            }
            return false;
        }
        let ev = if fresh_valid {
            self.detector.observe(now, fresh, w, h)
        } else {
            if self.detector.dims != (w, h) {
                // Forced (new geometry): the old region is meaningless; the
                // full frame that went out covers everything. (END first: its
                // rect refers to the old screen.)
                self.end(mailbox, false);
                self.detector.dims = (w, h);
                self.detector.reset();
            }
            MotionEvent::None
        };
        match ev {
            MotionEvent::Enter(r) | MotionEvent::Grow(r) => {
                if self.retry_at.is_some_and(|t| now < t) {
                    // The encoder failed recently: stay on tiles.
                    self.detector.reset();
                } else {
                    let enter = matches!(ev, MotionEvent::Enter(_));
                    if enter {
                        self.stats.enters += 1;
                        // A new period always starts with a keyframe.
                        self.want_idr = true;
                    } else {
                        self.stats.grows += 1;
                    }
                    // A grow inside the current encoder keeps the reference
                    // chain (the coded rect does not move); `encoder_rect`
                    // decides below.
                    self.active = Some(match (enter, self.active) {
                        (false, Some(a)) => union(a, r),
                        _ => r,
                    });
                    self.active_screen = (w, h);
                }
            }
            MotionEvent::Exit(_) => self.end(mailbox, true),
            MotionEvent::None => {}
        }
        let Some(rect) = self.active else {
            return false;
        };
        let changed = !fresh_valid || tiles_in_rect(fresh, &rect) > 0;
        if !changed && !self.want_idr && !self.ltr.recover_pending() {
            return false;
        }
        if mailbox.video_queued() >= MAX_VIDEO_QUEUE {
            self.stats.skipped += 1;
            return false;
        }
        let need = coded_rect(rect);
        if need.w < 16 || need.h < 16 || px.len() < stride * h as usize {
            return false;
        }
        // The coded rect contains the motion rect and becomes the mask (the
        // margin streams as video too, not as tiles).
        let cr = self.encoder_rect(need, w, h);
        // A moved coded rect of the same size needs no keyframe (the
        // P-frame still decodes exactly; only its prediction is worse); a
        // new size means a new encoder, which starts with one.
        self.active = Some(cr);
        let (cw, ch) = (u32::from(cr.w), u32::from(cr.h));
        let b = target_bitrate_bpp(cw, ch, self.fps, self.link_rate, self.bpp);
        if let Err(e) = self.ensure_encoder(cw, ch, b) {
            return self.fail(now, mailbox, e);
        }
        self.episode_encoded = true;
        let Some(cached) = self.enc.as_mut() else {
            return false;
        };
        let enc = &mut cached.enc;
        if b.abs_diff(self.applied_bitrate) * 100 > self.applied_bitrate.max(1) * 15 {
            enc.set_bitrate(b);
            self.applied_bitrate = b;
        }
        let off = usize::from(cr.y) * stride + usize::from(cr.x) * 4;
        let t0 = Instant::now();
        let ts = unix_micros();
        let plan = self.ltr.plan(ts, self.want_idr);
        let force = plan.force_idr;
        enc.set_frame_refs(plan.refs);
        let rect = (
            u32::from(cr.x),
            u32::from(cr.y),
            u32::from(cr.w),
            u32::from(cr.h),
        );
        let gpu = self
            .gpu_frame
            .as_deref()
            .and_then(|g| enc.encode_gpu(g, rect, force, ts));
        let aus = match gpu.unwrap_or_else(|| enc.encode(&px[off..], stride, force, ts)) {
            Ok(a) => a,
            Err(e) => return self.fail(now, mailbox, e),
        };
        let codec = enc.codec();
        self.stats.encode_time += t0.elapsed();
        if force {
            self.want_idr = false;
        }
        let mut any = false;
        for au in aus {
            let ltr_flags = self.ltr.on_output(self.seq, &au);
            let header = VideoHeader {
                seq: self.seq,
                flags: if au.keyframe { VIDEO_KEYFRAME } else { 0 } | ltr_flags,
                codec: codec.wire(),
                rect: cr,
                screen_w: w,
                screen_h: h,
                pts_us: au.pts_us,
            };
            self.seq = self.seq.wrapping_add(1);
            self.stats.frames += 1;
            self.stats.keyframes += u64::from(au.keyframe);
            self.stats.ltr_marks += u64::from(header.is_ltr());
            self.stats.bytes += au.data.len() as u64;
            mailbox.push_video(EncodedFrame::video(header, au.data));
            any = true;
        }
        any
    }

    /// The spec a new `cw x ch` encoder is opened with.
    fn spec(&self, cw: u32, ch: u32, bitrate: u32) -> EncoderSpec {
        EncoderSpec {
            w: cw,
            h: ch,
            fps: self.fps,
            bitrate,
            codec: self.wanted_codec(),
            ltr_slots: if self.peer_ltr { LTR_SLOTS } else { 0 },
        }
    }

    /// Keep the cached encoder when it matches `cw x ch` and the wanted
    /// codec / LTR use, else open a new one (HEVC failing falls back to
    /// H.264 for the session).
    fn ensure_encoder(&mut self, cw: u32, ch: u32, bitrate: u32) -> Result<(), String> {
        let want = self.spec(cw, ch, bitrate);
        let matches = |s: &EncoderSpec| {
            (s.w, s.h, s.codec, s.ltr_slots) == (want.w, want.h, want.codec, want.ltr_slots)
        };
        if self.enc.as_ref().is_some_and(|c| matches(&c.spec)) {
            return Ok(());
        }
        self.enc = None;
        let enc = match (self.factory)(&want) {
            Ok(e) => e,
            Err(e) if want.codec == VideoCodec::Hevc => {
                self.hevc_failed = true;
                self.last_error = Some(format!("HEVC encoder unavailable, using H.264: {e}"));
                let h264 = self.spec(cw, ch, bitrate);
                (self.factory)(&h264)?
            }
            Err(e) => return Err(e),
        };
        let spec = EncoderSpec {
            codec: enc.codec(),
            ..self.spec(cw, ch, bitrate)
        };
        // Keep the counters of the tracker being replaced.
        self.stats.recoveries += self.ltr.recoveries;
        self.stats.recover_idrs += self.ltr.recover_idrs;
        self.ltr = LtrTracker::new(enc.ltr_slots().min(spec.ltr_slots), self.fps);
        self.enc = Some(CachedEncoder { spec, enc });
        self.applied_bitrate = bitrate;
        self.want_idr = true;
        self.stats.encoders += 1;
        Ok(())
    }

    /// The coded rect for motion rect `need` (even, inside the `sw x sh`
    /// screen), placed tile-aligned where possible (falls back to `need`):
    ///
    /// * the current encoder's size when `need` fits in it — always within
    ///   a motion period, at the start of one only when it is not more
    ///   than twice the fresh bucket;
    /// * fixed surface, growth past the encoder within a motion period:
    ///   the full screen (one re-open per period at most);
    /// * else `need` rounded up to [`ENCODER_BUCKET`] px.
    pub(super) fn encoder_rect(&self, need: TileRect, sw: u32, sh: u32) -> TileRect {
        let (sw2, sh2) = (sw & !1, sh & !1);
        let bucket =
            |v: u16, s: u32| (u32::from(v).div_ceil(ENCODER_BUCKET) * ENCODER_BUCKET).min(s & !1);
        let (mut ew, mut eh) = (bucket(need.w, sw), bucket(need.h, sh));
        if let Some(c) = &self.enc {
            let (w0, h0) = (c.spec.w, c.spec.h);
            let fits = w0 >= u32::from(need.w) && h0 >= u32::from(need.h) && w0 <= sw && h0 <= sh;
            let thrifty = u64::from(w0) * u64::from(h0) <= u64::from(ew) * u64::from(eh) * 2;
            if fits && (thrifty || (self.fixed_surface && self.episode_encoded)) {
                (ew, eh) = (w0, h0);
            } else if !fits && self.fixed_surface && self.episode_encoded {
                (ew, eh) = (sw2, sh2);
            }
        }
        let place = |pos: u16, len: u16, e: u32, s: u32| -> Option<u32> {
            let end = u32::from(pos) + u32::from(len);
            let mut x = u32::from(pos) / TILE * TILE;
            if x + e > s {
                x = s.checked_sub(e)?;
            }
            (x <= u32::from(pos) && x + e >= end).then_some(x)
        };
        // Same size as now and still covering `need`: stay put (a moving
        // coded rect costs bits, the static content no longer matches).
        if let Some(a) = self
            .active
            .filter(|a| (u32::from(a.w), u32::from(a.h)) == (ew, eh))
        {
            let covers = a.x <= need.x
                && a.y <= need.y
                && u32::from(a.x) + u32::from(a.w) >= u32::from(need.x) + u32::from(need.w)
                && u32::from(a.y) + u32::from(a.h) >= u32::from(need.y) + u32::from(need.h)
                && u32::from(a.x) + ew <= sw
                && u32::from(a.y) + eh <= sh;
            if covers {
                return a;
            }
        }
        match (place(need.x, need.w, ew, sw), place(need.y, need.h, eh, sh)) {
            (Some(x), Some(y)) => TileRect {
                x: x as u16,
                y: y as u16,
                w: ew as u16,
                h: eh as u16,
            },
            _ => need,
        }
    }

    fn fail(&mut self, now: Instant, mailbox: &FrameMailbox, e: String) -> bool {
        self.enc = None;
        self.stats.encoder_failures += 1;
        self.last_error = Some(e);
        self.retry_at = Some(now + Duration::from_secs(5));
        self.stop(mailbox, true);
        false
    }
}

/// The bounding box of two rects.
fn union(a: TileRect, b: TileRect) -> TileRect {
    let x0 = a.x.min(b.x);
    let y0 = a.y.min(b.y);
    let x1 = (u32::from(a.x) + u32::from(a.w)).max(u32::from(b.x) + u32::from(b.w));
    let y1 = (u32::from(a.y) + u32::from(a.h)).max(u32::from(b.y) + u32::from(b.h));
    TileRect {
        x: x0,
        y: y0,
        w: (x1 - u32::from(x0)) as u16,
        h: (y1 - u32::from(y0)) as u16,
    }
}

/// The video bitrate (bits/s) for a `w x h` region at `fps`, given the
/// pacer's delivery-rate estimate (bytes/s, `None` before one exists): about
/// 0.2 bit per pixel per frame, at most 70% of the measured link, within
/// [0.5, 40] Mbps.
pub fn target_bitrate(w: u32, h: u32, fps: u32, link_bytes_per_sec: Option<f64>) -> u32 {
    target_bitrate_bpp(w, h, fps, link_bytes_per_sec, 0.2)
}

/// [`target_bitrate`] with an explicit bits-per-pixel-per-frame budget
/// (benches raise it to load the link).
pub(crate) fn target_bitrate_bpp(
    w: u32,
    h: u32,
    fps: u32,
    link_bytes_per_sec: Option<f64>,
    bpp: f64,
) -> u32 {
    let by_area = f64::from(w) * f64::from(h) * f64::from(fps.max(1)) * bpp;
    let by_link = link_bytes_per_sec.map_or(f64::INFINITY, |r| r * 8.0 * 0.7);
    by_area.min(by_link).clamp(500_000.0, 40_000_000.0) as u32
}
