//! The screen session's capture thread: grabs the screen at the rate the
//! viewer, the link and the quality governor allow, and encodes each grab
//! into the frame mailbox (tile updates, or H.264 for high-motion regions).

use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ovapp::adapt::{AutoQuality, RateGovernor, RateStats};
use ovapp::sendq::{SubmitOutcome, UpdateEncoder};
use tracing::{debug, warn};

use super::hooks::{PendingRegion, SharedInput};
use crate::capture;
use crate::input::{Region, VirtualScreen};
use crate::settings::{
    H264_HW, H264_MODE, H264_OFF, H264_SW, LOSSY_QUALITY, MAX_AUTO_FPS, NO_LOSSY, NO_TILE_CACHE,
    VIDEO_BPP,
};

#[cfg(test)]
mod tests;

/// Capture rate while a region streams as H.264 (AUTO quality only; a
/// manual QUALITY_SET wins).
const VIDEO_FPS: u32 = 30;
/// Idle capture rate for a static screen (frames per second).
const IDLE_FPS: u32 = 4;
/// How often the host logs capture metrics while streaming.
pub(super) const METRICS_EVERY: Duration = Duration::from_secs(30);

/// State shared between the screen session thread and its capture thread.
pub(super) struct CaptureCtl {
    /// Ask the capture thread to exit.
    pub(super) stop: AtomicBool,
    /// Peer dead: stop capturing (no wire traffic) until it recovers.
    pub(super) paused: AtomicBool,
    /// The viewer's QUALITY_SET target (0 = AUTO), shared with the hooks.
    pub(super) target_fps: Arc<AtomicU16>,
    /// The host's smoothed RTT (us), shared with the hooks.
    pub(super) rtt_us: Arc<AtomicU64>,
    /// The session thread's smoothed per-frame drain time (us; 0 = none
    /// yet) — what the link actually sustains, for the link-limited log.
    pub(super) drain_us: AtomicU64,
    /// Estimated time (us) until the in-flight frame is fully handed to the
    /// carrier (0 = link free): the capture thread times its next capture
    /// to finish just as the link frees up.
    pub(super) link_eta_us: AtomicU64,
    /// CAPTURE_REGION requests parked by the hook.
    pub(super) pending_region: PendingRegion,
    /// The input controller (its region is re-mapped on retarget).
    pub(super) input: SharedInput,
    /// The pacer's excess queueing delay (us; `u64::MAX` = no fresh
    /// signal): the auto-quality rule's congestion input.
    pub(super) queue_delay_us: AtomicU64,
    /// The captured region's origin (cursor coordinates are relative to it).
    pub(super) origin: Mutex<(i32, i32)>,
    /// Set after a retarget: the session re-announces MONITORS_INFO.
    pub(super) monitors_dirty: AtomicBool,
    /// The viewer's CAPS bits (shared with the hooks).
    pub(super) peer_caps: Arc<AtomicU32>,
    /// Pending KEYFRAME_REQUEST (shared with the hooks).
    pub(super) keyframe: Arc<AtomicBool>,
    /// Pending VIDEO_IDR_REQUEST (shared with the hooks).
    pub(super) video_idr: Arc<AtomicBool>,
    /// The link pacer's delivery-rate estimate, bytes/s (0 = none yet):
    /// the H.264 bitrate follows it.
    pub(super) link_rate: AtomicU64,
}

/// Drain pacing: how long the capture thread should hold off so its next
/// frame is ready just as the link finishes the frame in flight — `None` =
/// capture now. `eta` is the in-flight frame's remaining drain time, `cost`
/// the smoothed capture+compress time, and `queued` whether a captured
/// frame is already waiting to be sent (then a new capture would only
/// replace it, wasting a compress).
fn capture_hold(eta: Duration, cost: Duration, queued: bool) -> Option<Duration> {
    let max_slice = Duration::from_millis(20);
    if eta > cost {
        Some((eta - cost).min(max_slice))
    } else if queued {
        Some(Duration::from_millis(1))
    } else {
        None
    }
}

/// Rolling capture-thread counters for the periodic metrics line.
#[derive(Default)]
struct CaptureMetrics {
    grabs: u64,
    grab_time: Duration,
    encodes: u64,
    encode_time: Duration,
    full: u64,
    deltas: u64,
    tiles: u64,
    bytes: u64,
    /// Capture-phase latency (OS present -> grab), DXGI only.
    latency: Duration,
    latency_n: u64,
    latency_max: Duration,
    /// Refinement-lane updates and the tiles they carried.
    refines: u64,
    refine_tiles: u64,
}

impl CaptureMetrics {
    fn log(&self, backend: capture::Backend, over: Duration) {
        let per = |t: Duration, n: u64| t.as_secs_f64() * 1000.0 / n.max(1) as f64;
        debug!(
            "capture {backend}: {:.2} ms/grab, encode {:.2} ms/update; {} full + {} delta \
             updates ({:.1} tiles/delta), {:.1} KB/s",
            per(self.grab_time, self.grabs),
            per(self.encode_time, self.encodes),
            self.full,
            self.deltas,
            self.tiles as f64 / self.deltas.max(1) as f64,
            self.bytes as f64 / 1024.0 / over.as_secs_f64().max(0.001)
        );
        if self.latency_n > 0 {
            debug!(
                "capture latency (present -> grab): avg {:.2} ms, max {:.2} ms over {} grabs",
                per(self.latency, self.latency_n),
                self.latency_max.as_secs_f64() * 1000.0,
                self.latency_n
            );
        }
        if self.refines > 0 {
            debug!(
                "refinement lane: {} update(s), {} tile(s) refined to exact pixels",
                self.refines, self.refine_tiles
            );
        }
    }
}

/// The AUTO base rate the link allows: `base` raised to 30 or `max` fps
/// when the pacer's delivery rate (`rate` bytes/s) carries that many
/// updates of the smoothed size (`update_bytes`) in about half the link.
/// No signal (`rate == 0` or no updates yet) = `base`.
fn link_base_fps(base: u32, max: u32, rate: u64, update_bytes: f64) -> u32 {
    if rate == 0 || update_bytes <= 0.0 || max <= base {
        return base;
    }
    let carried = rate as f64 * 0.5 / update_bytes.max(1024.0);
    let mut fps = base;
    for step in [30, max] {
        if f64::from(step) <= carried && step > fps {
            fps = step.min(max);
        }
    }
    fps
}

/// Registration of the calling thread with MMCSS (the "Capture" task) so
/// the capture/encode thread is scheduled ahead of background work; undone
/// on drop.
#[cfg(windows)]
struct MmcssGuard(windows::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl MmcssGuard {
    /// Register the calling thread for `task`; `None` when MMCSS refused.
    fn enter(task: &str) -> Option<Self> {
        let name: Vec<u16> = task.encode_utf16().chain(std::iter::once(0)).collect();
        let mut idx = 0u32;
        // SAFETY: `name` is a NUL-terminated UTF-16 string that outlives
        // the call and `idx` is a valid out-pointer.
        unsafe {
            windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW(
                windows::core::PCWSTR(name.as_ptr()),
                &mut idx,
            )
        }
        .ok()
        .map(Self)
    }
}

#[cfg(windows)]
impl Drop for MmcssGuard {
    fn drop(&mut self) {
        // SAFETY: the handle came from AvSetMmThreadCharacteristicsW on this
        // thread (the guard lives on the capture thread's stack) and is
        // reverted exactly once.
        let _ =
            unsafe { windows::Win32::System::Threading::AvRevertMmThreadCharacteristics(self.0) };
    }
}

/// The capture thread: owns the screen source (DXGI or GDI); captures at
/// the governor's rate — but never faster than the link drains frames (see
/// [`capture_hold`]) — and hands each capture to the [`UpdateEncoder`],
/// which diffs it against what the viewer holds and queues a full frame or
/// a tile delta (or nothing) in the mailbox. Never touches the session.
pub(super) fn capture_loop(
    mut src: capture::ScreenSource,
    base_fps: u32,
    ctl: Arc<CaptureCtl>,
    mailbox: Arc<ovapp::sendq::FrameMailbox>,
) {
    let mut gov = RateGovernor::new(base_fps.max(1), IDLE_FPS);
    let mut auto_q = AutoQuality::new();
    let mut last_scale_step: i32 = 10;
    let mut enc = UpdateEncoder::new();
    // Lossy-first image tiles + tile cache (CAPS-gated per viewer).
    enc.set_lossy(!NO_LOSSY.load(Ordering::Relaxed));
    enc.set_cache(!NO_TILE_CACHE.load(Ordering::Relaxed));
    enc.set_lossy_params(ovapp::lossy::LossyParams {
        quality: LOSSY_QUALITY.load(Ordering::Relaxed),
        ..ovapp::lossy::LossyParams::default()
    });
    // High-motion regions as H.264 (only to a CAP_H264 viewer).
    let mut video = ovapp::video::VideoSession::new(video_encoder_factory(), VIDEO_FPS);
    // Benches: `--video-bpp` raises the area-based bitrate budget.
    if let Some(&bpp) = VIDEO_BPP.get() {
        video.set_bits_per_pixel(bpp);
    }
    let mut video_was: Option<ovapp::tiles::TileRect> = None;
    let mut video_logged = ovapp::video::VideoStats::default();
    let mut metrics = CaptureMetrics::default();
    let mut backend = src.backend();
    let mut frames_sent: u64 = 0;
    let mut frames_skipped: u64 = 0;
    let mut last_metrics = Instant::now();
    let mut last_effective: u32 = 0;
    let mut last_link_ms: u64 = 0;
    let mut next_capture = Instant::now();
    let mut last_started = Instant::now();
    // Smoothed capture + change-detect + compress time.
    let mut cost = Duration::from_millis(20);
    // A change was pulled in by `wait_change` since the last grab.
    let mut buffered = false;
    // Smoothed wire bytes of a changed update (for the 60 fps rule).
    let mut update_bytes: f64 = 0.0;
    let mut enc_logged = ovapp::sendq::EncoderStats::default();
    #[cfg(windows)]
    let _mmcss = MmcssGuard::enter("Capture").or_else(|| {
        debug!("MMCSS registration failed (capture thread keeps normal priority)");
        None
    });
    while !ctl.stop.load(Ordering::Relaxed) {
        // Capture-target retarget: re-create the source, re-map pointer
        // input, and have the session re-announce the monitors.
        if let Some(rect) = ctl.pending_region.lock().ok().and_then(|mut s| s.take()) {
            match src.retarget(rect.0, rect.1, rect.2, rect.3) {
                Ok(()) => {
                    if let Ok(mut o) = ctl.origin.lock() {
                        *o = src.origin();
                    }
                    if let Ok(mut input) = ctl.input.lock() {
                        input.set_region(Region {
                            x: rect.0,
                            y: rect.1,
                            w: rect.2,
                            h: rect.3,
                        });
                        input.set_virtual_screen(VirtualScreen::query());
                    }
                    // New monitor: the next update is a full frame (and
                    // whatever streamed as video is gone).
                    enc.reset();
                    video.stop(&mailbox, false);
                    ctl.monitors_dirty.store(true, Ordering::Relaxed);
                    debug!(
                        "capture retargeted to {}x{} at ({},{})",
                        rect.2, rect.3, rect.0, rect.1
                    );
                    next_capture = Instant::now();
                }
                Err(e) => warn!("retarget failed: {e}"),
            }
        }
        if ctl.paused.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        // Follow the viewer's CAPS; a KEYFRAME_REQUEST is served by the
        // very next capture.
        let caps = ctl.peer_caps.load(Ordering::Relaxed);
        enc.set_peer_caps(caps);
        enc.set_link_rate(ctl.link_rate.load(Ordering::Relaxed));
        let h264 =
            caps & ovapp::terminal::CAP_H264 != 0 && H264_MODE.load(Ordering::Relaxed) != H264_OFF;
        if ctl.keyframe.swap(false, Ordering::Relaxed) {
            enc.request_keyframe();
            video.request_idr();
            next_capture = Instant::now();
        }
        if ctl.video_idr.swap(false, Ordering::Relaxed) {
            video.request_idr();
            next_capture = Instant::now();
        }
        // Quality: a manual QUALITY_SET wins; otherwise the auto rule
        // (AutoQuality) follows the pacer's excess queueing delay — never
        // the raw RTT — and lowers the H.264 bitrate before the fps.
        let want = ctl.target_fps.load(Ordering::Relaxed);
        let rtt_us = ctl.rtt_us.load(Ordering::Relaxed);
        let qd_us = ctl.queue_delay_us.load(Ordering::Relaxed);
        let queue_delay = (qd_us != u64::MAX).then(|| Duration::from_micros(qd_us));
        // Smooth motion while a region streams as H.264 (AUTO quality
        // only); its bitrate follows the pacer's delivery rate.
        let link_base = link_base_fps(
            base_fps.max(1),
            MAX_AUTO_FPS.load(Ordering::Relaxed),
            ctl.link_rate.load(Ordering::Relaxed),
            update_bytes,
        );
        let auto_base = if video.active_rect().is_some() {
            link_base.max(VIDEO_FPS)
        } else {
            link_base
        };
        let decision = if want == 0 {
            auto_q.update(Instant::now(), auto_base, queue_delay)
        } else {
            ovapp::adapt::QualityDecision {
                fps: u32::from(want),
                bitrate_scale: 1.0,
                degraded: false,
            }
        };
        let effective = decision.fps;
        let rate = ctl.link_rate.load(Ordering::Relaxed);
        video.set_link_rate((rate > 0).then_some(rate as f64 * decision.bitrate_scale));
        video.set_fps(if want == 0 && !decision.degraded {
            effective.max(VIDEO_FPS)
        } else {
            effective
        });
        let scale_step = (decision.bitrate_scale * 10.0).round() as i32;
        if effective != last_effective || scale_step != last_scale_step {
            gov.set_base_fps(effective);
            last_effective = effective;
            last_scale_step = scale_step;
            let mode = if want == 0 { "AUTO" } else { "manual" };
            debug!(
                "capture rate -> {effective} fps ({mode}, queue delay {}, H.264 bitrate x{:.1}, rtt {rtt_us} us)",
                queue_delay.map_or_else(|| "n/a".to_owned(), |d| format!("{} ms", d.as_millis())),
                decision.bitrate_scale
            );
        }
        // Report when the link, not the governor, sets the frame rate.
        let drain = Duration::from_micros(ctl.drain_us.load(Ordering::Relaxed));
        let link_ms = drain.as_millis() as u64;
        if drain > gov.interval() && link_ms.abs_diff(last_link_ms) * 4 > last_link_ms.max(20) {
            debug!(
                "link-limited: ~{:.1} fps (a frame drains in {link_ms} ms)",
                1000.0 / link_ms.max(1) as f64
            );
            last_link_ms = link_ms;
        }
        // Refinement lane: lossy (JPEG / video-painted) tiles that settled
        // go out as small exact chunks whenever the previous update left the
        // mailbox — no capture needed, never ahead of a waiting change (a
        // capture replaces an unsent chunk).
        let refine_due = enc.refine_due();
        if refine_due && !mailbox.is_pending() {
            let t0 = Instant::now();
            if let SubmitOutcome::Delta { tiles, bytes, .. } = enc.submit_refine(&mailbox) {
                metrics.refines += 1;
                metrics.refine_tiles += tiles as u64;
                metrics.bytes += bytes as u64;
                metrics.encode_time += t0.elapsed();
                metrics.encodes += 1;
            }
        }
        let poll = if refine_due {
            Duration::from_millis(4)
        } else {
            Duration::from_millis(20)
        };
        let now = Instant::now();
        let active = Duration::from_secs_f64(1.0 / f64::from(effective.max(1)));
        if now < next_capture {
            // Short slices so stop/retarget/pause are noticed promptly.
            let slice = (next_capture - now).min(poll);
            // Event-driven capture, idle *and* active: block on the
            // duplication instead of sleeping. While idle (a slow governor
            // tick) a change pulls the next capture in to the active frame
            // interval; while streaming it is buffered for the slot (the
            // capture below then runs without waiting).
            let idle = gov.interval() > active;
            match src.wait_change(slice) {
                Some(true) => {
                    buffered = true;
                    if idle {
                        next_capture = next_capture.min(last_started + active);
                    }
                }
                Some(false) => {}
                None => std::thread::sleep(slice),
            }
            continue;
        }
        let eta = Duration::from_micros(ctl.link_eta_us.load(Ordering::Relaxed));
        if let Some(hold) = capture_hold(eta, cost, mailbox.is_pending()) {
            std::thread::sleep(hold);
            continue;
        }
        // The slot is open but nothing new arrived yet: block for the first
        // change (up to one more active interval) and capture the moment it
        // arrives, instead of grabbing an unchanged screen and then making
        // the change wait a whole interval for the next slot. Past that the
        // grab runs anyway (idle accounting, video exit). Settled lossy
        // tiles need no grab — the refinement lane above sends them from
        // the last capture.
        if !buffered && src.can_wait() {
            let deadline = next_capture + active;
            if now < deadline {
                let slice = (deadline - now).min(poll);
                if src.wait_change(slice) == Some(false) {
                    continue; // nothing yet: keep waiting (up to the deadline)
                }
            }
        }
        buffered = false;
        let started = Instant::now();
        last_started = started;
        // GPU encode input: the encoder reads the GPU copy.
        #[cfg(windows)]
        video.set_gpu_frame(
            src.gpu_frame()
                .map(|f| Box::new(f) as Box<dyn std::any::Any>),
        );
        match src.grab() {
            Ok(g) => {
                backend = g.backend;
                let grabbed = Instant::now();
                metrics.grabs += 1;
                metrics.grab_time += grabbed - started;
                // The video region is masked out of tile updates; the
                // motion detector then sees this capture's dirty tiles
                // (after copy-rect, so a scroll is not motion).
                let out = enc.submit_ex(
                    g.px,
                    g.w,
                    g.h,
                    g.hints,
                    g.moves,
                    video.active_rect(),
                    None,
                    &mailbox,
                );
                let encoded_video = video.on_capture(
                    Instant::now(),
                    g.px,
                    g.w,
                    g.h,
                    g.w as usize * 4,
                    enc.fresh(),
                    enc.fresh_valid(),
                    h264,
                    &mailbox,
                );
                if let Some(e) = video.take_error() {
                    warn!("H.264 encoder failed ({e}); staying on lossless tiles");
                }
                // Motion over: VIDEO_END is queued; now the lossless refresh
                // of the region (ordered after the END), queued as a backlog
                // the following updates drain ~64 KB at a time, so
                // input-driven changes never wait behind the whole region.
                let refresh = video.take_refresh();
                let out = match refresh {
                    Some(r) => {
                        enc.queue_refresh(r);
                        let o =
                            enc.submit_ex(g.px, g.w, g.h, Some(&[]), None, None, None, &mailbox);
                        if o == SubmitOutcome::Unchanged {
                            out
                        } else {
                            o
                        }
                    }
                    None => out,
                };
                let now_active = video.active_rect();
                if now_active != video_was {
                    match (video_was, now_active) {
                        (_, Some(r)) => debug!(
                            "motion -> H.264 region {}x{} at ({},{}) via {}",
                            r.w,
                            r.h,
                            r.x,
                            r.y,
                            video.encoder_name().unwrap_or_else(|| "(opening)".into())
                        ),
                        (Some(r), None) => debug!(
                            "motion over -> lossless refresh of {}x{} at ({},{})",
                            r.w, r.h, r.x, r.y
                        ),
                        (None, None) => {}
                    }
                    video_was = now_active;
                }
                match out {
                    SubmitOutcome::Unchanged if encoded_video => {
                        // Only the video region changed: full rate.
                        frames_sent += 1;
                        let _ = gov.on_change();
                    }
                    SubmitOutcome::Unchanged => {
                        // Unchanged screen: no wire traffic at all.
                        frames_skipped += 1;
                        let _ = gov.on_idle();
                    }
                    out => {
                        metrics.encodes += 1;
                        metrics.encode_time += grabbed.elapsed();
                        match out {
                            SubmitOutcome::Full { bytes } => {
                                metrics.full += 1;
                                metrics.bytes += bytes as u64;
                            }
                            SubmitOutcome::Delta { tiles, bytes, .. } => {
                                metrics.deltas += 1;
                                metrics.tiles += tiles as u64;
                                metrics.bytes += bytes as u64;
                                update_bytes = if update_bytes == 0.0 {
                                    bytes as f64
                                } else {
                                    update_bytes * 0.9 + bytes as f64 * 0.1
                                };
                            }
                            SubmitOutcome::Unchanged => {}
                        }
                        frames_sent += 1;
                        let _ = gov.on_change();
                    }
                }
            }
            Err(e) => debug!("capture error: {e}"),
        }
        if let Some(l) = src.take_latency() {
            metrics.latency += l;
            metrics.latency_n += 1;
            metrics.latency_max = metrics.latency_max.max(l);
        }
        // Schedule from the capture start so capture+compress time counts
        // against the interval (the effective rate is the requested one).
        let took = started.elapsed();
        cost = (cost * 3 + took) / 4;
        next_capture = started + gov.interval();
        if last_metrics.elapsed() >= METRICS_EVERY {
            log_screen_metrics(frames_sent, frames_skipped, &gov.stats());
            metrics.log(backend, last_metrics.elapsed());
            log_video(&video.stats(), &mut video_logged, last_metrics.elapsed());
            metrics = CaptureMetrics::default();
            let es = enc.stats();
            if es != enc_logged {
                debug!(
                    "copy-rect: {} move update(s) ({} detected scrolls), {} tiles saved; bands lz4/zstd/raw {}/{}/{}",
                    es.moves - enc_logged.moves,
                    es.detected_moves - enc_logged.detected_moves,
                    es.tiles_saved - enc_logged.tiles_saved,
                    es.lz4_bands - enc_logged.lz4_bands,
                    es.zstd_bands - enc_logged.zstd_bands,
                    es.raw_bands - enc_logged.raw_bands
                );
                if es.lossy_tiles != enc_logged.lossy_tiles
                    || es.refined_tiles != enc_logged.refined_tiles
                    || es.cache_refs != enc_logged.cache_refs
                {
                    debug!(
                        "wave2: {} lossy tile(s) in {:.1} KB, {} refined, cache {} ref(s) / {} store(s), {} awaiting refinement",
                        es.lossy_tiles - enc_logged.lossy_tiles,
                        (es.lossy_bytes - enc_logged.lossy_bytes) as f64 / 1024.0,
                        es.refined_tiles - enc_logged.refined_tiles,
                        es.cache_refs - enc_logged.cache_refs,
                        es.cache_stores - enc_logged.cache_stores,
                        enc.refresh_pending()
                    );
                }
                enc_logged = es;
            }
            debug!(
                "{} stale frame(s) superseded before sending",
                mailbox.replaced_count()
            );
            last_metrics = Instant::now();
        }
    }
    log_screen_metrics(frames_sent, frames_skipped, &gov.stats());
    log_video(&video.stats(), &mut video_logged, last_metrics.elapsed());
}

/// The H.264 encoder factory of the capture thread: Media Foundation,
/// hardware first unless `--h264 sw`.
fn video_encoder_factory() -> ovapp::video::EncoderFactory {
    Box::new(|w, h, fps, bitrate| {
        #[cfg(windows)]
        {
            let pref = match H264_MODE.load(Ordering::Relaxed) {
                H264_SW => ovapp::mfcodec::EncoderPref::Software,
                H264_HW => ovapp::mfcodec::EncoderPref::Hardware,
                _ => ovapp::mfcodec::EncoderPref::Auto,
            };
            let t0 = Instant::now();
            let e = ovapp::mfcodec::MfVideoEncoder::new(w, h, fps, bitrate, pref)?;
            debug!(
                "H.264 encoder {} for {w}x{h} @ {fps} fps, {:.1} Mbps (opened in {} ms)",
                ovapp::video::VideoEncode::name(&e),
                f64::from(bitrate) / 1e6,
                t0.elapsed().as_millis()
            );
            Ok(Box::new(e) as Box<dyn ovapp::video::VideoEncode>)
        }
        #[cfg(not(windows))]
        {
            let _ = (w, h, fps, bitrate);
            Err("no H.264 encoder on this platform".to_owned())
        }
    })
}

/// One H.264 line per metrics period (only when video was streamed).
fn log_video(st: &ovapp::video::VideoStats, last: &mut ovapp::video::VideoStats, over: Duration) {
    let frames = st.frames - last.frames;
    if frames > 0 || st.enters != last.enters {
        let bytes = st.bytes - last.bytes;
        let t = st.encode_time.saturating_sub(last.encode_time);
        debug!(
            "H.264: {frames} pictures ({} IDR), {:.2} ms/picture encode, {:.0} KB/s; \
             {} motion period(s), {} grow(s), {} back to tiles, {} skipped (link busy)",
            st.keyframes - last.keyframes,
            t.as_secs_f64() * 1000.0 / frames.max(1) as f64,
            bytes as f64 / 1024.0 / over.as_secs_f64().max(0.001),
            st.enters - last.enters,
            st.grows - last.grows,
            st.exits - last.exits,
            st.skipped - last.skipped
        );
    }
    *last = *st;
}

/// One-line capture metrics: frames sent vs idle captures skipped, plus the
/// governor's current rate — makes the static-screen savings observable.
fn log_screen_metrics(sent: u64, skipped: u64, stats: &RateStats) {
    let ms = stats.interval.as_millis().max(1) as f64;
    let fps_now = 1000.0 / ms;
    debug!(
        "screen capture — sent {sent}, skipped {skipped} unchanged, \
         capturing at ~{fps_now:.1} fps{}",
        if stats.at_idle_rate {
            " (idle rate)"
        } else {
            ""
        }
    );
}
