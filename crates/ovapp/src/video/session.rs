//! The host video session: drives the motion detector, owns the encoder
//! and queues access units in the frame mailbox.

use std::time::{Duration, Instant};

use super::{
    coded_rect, tiles_in_rect, unix_micros, EncoderFactory, MotionConfig, MotionDetector,
    MotionEvent, VideoEncode, VideoHeader, CODEC_H264, ENCODER_BUCKET, MAX_VIDEO_QUEUE, VIDEO_END,
    VIDEO_KEYFRAME,
};
use crate::sendq::{EncodedFrame, FrameMailbox};
use crate::tiles::{TileMap, TileRect, TILE};

/// Counters of a [`VideoSession`].
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct VideoStats {
    /// Access units queued.
    pub frames: u64,
    /// Of which IDRs.
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
}

/// Host-side H.264 state: drives the [`MotionDetector`], owns the encoder
/// (created lazily through the factory, cached across motion periods of
/// the same size) and queues access units / END markers in the
/// [`FrameMailbox`] video FIFO.
///
/// Per capture the host calls, in order:
/// 1. `UpdateEncoder::submit_masked(.., mask = active_rect(), ..)`,
/// 2. [`on_capture`](Self::on_capture) with the encoder's fresh-tile map,
/// 3. when [`take_refresh`](Self::take_refresh) yields a rect, `submit_masked`
///    again with it as the forced refresh (lossless, after `VIDEO_END`).
pub struct VideoSession {
    detector: MotionDetector,
    pub(super) factory: EncoderFactory,
    /// The cached encoder and its `(w, h)`.
    pub(super) enc: Option<(u32, u32, Box<dyn VideoEncode>)>,
    /// Tile-aligned motion rect (the mask), while in motion.
    active: Option<TileRect>,
    /// The screen size `active` refers to.
    active_screen: (u32, u32),
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
            active: None,
            active_screen: (0, 0),
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
        }
    }

    /// The area-based bitrate budget in bits per pixel per frame
    /// (default 0.2; benches raise it).
    pub fn set_bits_per_pixel(&mut self, bpp: f64) {
        if bpp.is_finite() && bpp > 0.0 {
            self.bpp = bpp;
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

    /// Next encoded picture is an IDR (keyframe / IDR request).
    pub fn request_idr(&mut self) {
        if self.active.is_some() {
            self.want_idr = true;
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
        self.stats
    }

    /// The encoder in use (for logs).
    pub fn encoder_name(&self) -> Option<String> {
        self.enc.as_ref().map(|(_, _, e)| e.name())
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
        let header = VideoHeader {
            seq: self.seq,
            flags: VIDEO_END,
            codec: CODEC_H264,
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
                    if matches!(ev, MotionEvent::Enter(_)) {
                        self.stats.enters += 1;
                    } else {
                        self.stats.grows += 1;
                    }
                    self.active = Some(r);
                    self.active_screen = (w, h);
                    self.want_idr = true;
                }
            }
            MotionEvent::Exit(_) => self.end(mailbox, true),
            MotionEvent::None => {}
        }
        let Some(rect) = self.active else {
            return false;
        };
        let changed = !fresh_valid || tiles_in_rect(fresh, &rect) > 0;
        if !changed && !self.want_idr {
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
        // The encoder size comes in 128 px buckets and is kept while
        // the motion rect grows inside it (no MF re-open + IDR per Grow);
        // the coded rect is placed to contain the motion rect, and becomes
        // the mask (the margin streams as video too, not as tiles).
        let cr = self.encoder_rect(need, w, h);
        if self.active != Some(cr) {
            self.active = Some(cr);
        }
        let (cw, ch) = (u32::from(cr.w), u32::from(cr.h));
        let b = target_bitrate_bpp(cw, ch, self.fps, self.link_rate, self.bpp);
        if self
            .enc
            .as_ref()
            .is_none_or(|(ew, eh, _)| (*ew, *eh) != (cw, ch))
        {
            self.enc = None;
            match (self.factory)(cw, ch, self.fps, b) {
                Ok(e) => {
                    self.enc = Some((cw, ch, e));
                    self.applied_bitrate = b;
                    self.want_idr = true;
                }
                Err(e) => return self.fail(now, mailbox, e),
            }
        }
        let Some((_, _, enc)) = self.enc.as_mut() else {
            return false;
        };
        if b.abs_diff(self.applied_bitrate) * 100 > self.applied_bitrate.max(1) * 15 {
            enc.set_bitrate(b);
            self.applied_bitrate = b;
        }
        let off = usize::from(cr.y) * stride + usize::from(cr.x) * 4;
        let t0 = Instant::now();
        let force = self.want_idr;
        let ts = unix_micros();
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
        self.stats.encode_time += t0.elapsed();
        if force {
            self.want_idr = false;
        }
        let mut any = false;
        for au in aus {
            let header = VideoHeader {
                seq: self.seq,
                flags: if au.keyframe { VIDEO_KEYFRAME } else { 0 },
                codec: CODEC_H264,
                rect: cr,
                screen_w: w,
                screen_h: h,
                pts_us: au.pts_us,
            };
            self.seq = self.seq.wrapping_add(1);
            self.stats.frames += 1;
            self.stats.keyframes += u64::from(au.keyframe);
            self.stats.bytes += au.data.len() as u64;
            mailbox.push_video(EncodedFrame::video(header, au.data));
            any = true;
        }
        any
    }

    /// The coded rect for motion rect `need` (even, inside the `sw x sh`
    /// screen): the current encoder's size when `need` fits in it (and it
    /// is not wastefully large), else `need` rounded up to 128 px buckets;
    /// placed tile-aligned where possible. Falls back to `need` itself.
    pub(super) fn encoder_rect(&self, need: TileRect, sw: u32, sh: u32) -> TileRect {
        let bucket =
            |v: u16, s: u32| (u32::from(v).div_ceil(ENCODER_BUCKET) * ENCODER_BUCKET).min(s & !1);
        let (mut ew, mut eh) = (bucket(need.w, sw), bucket(need.h, sh));
        if let Some((w0, h0, _)) = &self.enc {
            let fits =
                *w0 >= u32::from(need.w) && *h0 >= u32::from(need.h) && *w0 <= sw && *h0 <= sh;
            if fits && u64::from(*w0) * u64::from(*h0) <= u64::from(ew) * u64::from(eh) * 2 {
                (ew, eh) = (*w0, *h0);
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
