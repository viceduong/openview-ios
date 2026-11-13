//! The host's screen source (Phase 3): DXGI Desktop Duplication when it is
//! available, GDI `BitBlt` otherwise — switched transparently.
//!
//! - DXGI is tried at start and after every retarget (monitor switch).
//! - A lost duplication (desktop switch, mode change, UAC secure desktop)
//!   is re-created at once; if that fails the source falls back to GDI and
//!   retries DXGI every [`DXGI_RETRY`].
//! - A duplication that exists but has no image yet (DWM presents nothing
//!   while the output has no active monitor: TV off, display asleep) is
//!   kept *pending*: GDI serves the grabs and every grab polls the
//!   duplication without blocking; its first frame switches to DXGI.
//! - `--gdi` / `OV_CAPTURE=gdi` force GDI.
//! - `--test-pattern video|scroll` (Phase 5) replaces the screen with a
//!   synthetic desktop whose middle moves (a 640x360 video, or a scrolling
//!   page) for 5 s, then rests for 2 s — exercises the H.264 motion path
//!   on a static machine.
//!
//! Every grab yields the region's pixels (tight BGRA, alpha opaque) plus,
//! from DXGI, the OS's changed rects as diff hints (`None` from GDI or after
//! a switch: compare everything).

use std::time::{Duration, Instant};

use ovapp::gdisrc::GdiScreen;
use ovapp::tiles::TileRect;

/// How long to wait before trying DXGI again after it failed.
pub const DXGI_RETRY: Duration = Duration::from_secs(2);
/// How long a (re)created duplication may wait for its initial image
/// before the source runs on GDI and keeps the duplication pending: once
/// per start / retarget / loss, never on the periodic retries.
const DXGI_INITIAL_WAIT_MS: u32 = 1000;

/// Which backend produced a grab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Dxgi,
    Gdi,
    Pattern,
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Backend::Dxgi => "DXGI",
            Backend::Gdi => "GDI",
            Backend::Pattern => "test pattern",
        })
    }
}

/// One captured image (borrowed from the source until the next grab).
pub struct Grab<'a> {
    pub px: &'a [u8],
    pub w: u32,
    pub h: u32,
    pub hints: Option<&'a [TileRect]>,
    /// Wave 1: the OS's move rects (DXGI; copy-rect candidates the encoder
    /// verifies). `None` from GDI / the test pattern.
    pub moves: Option<&'a [ovapp::bands::MoveRect]>,
    pub backend: Backend,
}

pub struct ScreenSource {
    gdi: GdiScreen,
    #[cfg(windows)]
    dxgi: Option<crate::dxgi::DxgiCapture>,
    /// A duplication still waiting for its first image (see the module
    /// docs); polled by every grab, promoted to `dxgi` on its first frame.
    #[cfg(windows)]
    dxgi_pending: Option<crate::dxgi::DxgiCapture>,
    /// The pending state was logged (once per source).
    pending_logged: bool,
    prefer_dxgi: bool,
    retry_at: Option<Instant>,
    /// Last backend reported (to log switches once).
    last: Option<Backend>,
    /// Phase 5 `--test-pattern`.
    pattern: Option<TestPattern>,
    /// Wave 1: present-to-grab delay of the last DXGI grab (capture-phase
    /// latency probe; `None` from GDI or when nothing new was presented).
    latency: Option<Duration>,
}

/// The QPC clock (what DXGI `LastPresentTime` is in), in ticks and ticks/s.
#[cfg(windows)]
fn qpc_now() -> (i64, i64) {
    #[link(name = "kernel32")]
    extern "system" {
        fn QueryPerformanceCounter(c: *mut i64) -> i32;
        fn QueryPerformanceFrequency(f: *mut i64) -> i32;
    }
    let (mut c, mut f) = (0i64, 0i64);
    // SAFETY: valid out-pointers.
    unsafe {
        QueryPerformanceCounter(&mut c);
        QueryPerformanceFrequency(&mut f);
    }
    (c, f.max(1))
}

/// Which synthetic motion `--test-pattern` draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternKind {
    Video,
    Scroll,
    /// Wave 2: three "windows" (the desktop, and two photo windows)
    /// switching every [`TestPattern::SWITCH`]: lossy-first photos, their
    /// refinement, and cache hits when a window comes back.
    Photo,
}

/// A synthetic screen: a static desktop with a moving region that plays
/// for [`TestPattern::MOVE`] then rests for [`TestPattern::REST`].
pub struct TestPattern {
    kind: PatternKind,
    px: Vec<u8>,
    w: u32,
    h: u32,
    rect: TileRect,
    start: Instant,
    t: u32,
    hint: [TileRect; 1],
    /// Wave 2 photo pattern: the screens it cycles through, and which one
    /// is shown.
    screens: Vec<Vec<u8>>,
    shown: usize,
}

impl TestPattern {
    pub const MOVE: Duration = Duration::from_secs(5);
    pub const REST: Duration = Duration::from_secs(2);
    /// Wave 2 photo pattern: one window switch per this.
    pub const SWITCH: Duration = Duration::from_millis(1500);

    pub fn new(kind: PatternKind, w: u32, h: u32) -> Self {
        let mut px = vec![0u8; (w as usize) * (h as usize) * 4];
        ovapp::video::synth_desktop(&mut px, w, h);
        let (rw, rh) = match kind {
            PatternKind::Video => (640.min(w), 360.min(h)),
            PatternKind::Scroll => (1280.min(w), 800.min(h)),
            PatternKind::Photo => (1100.min(w), 700.min(h)),
        };
        let rect = TileRect {
            x: ((w - rw) / 2) as u16,
            y: ((h - rh) / 2) as u16,
            w: rw as u16,
            h: rh as u16,
        };
        let screens = if kind == PatternKind::Photo {
            let mut a = px.clone();
            photo_fill(&mut a, w, rect, 1);
            let mut b = px.clone();
            let r2 = TileRect {
                x: (w.saturating_sub(u32::from(rect.w)) / 4) as u16,
                y: (h / 8).min(h - u32::from(rect.h)) as u16,
                ..rect
            };
            photo_fill(&mut b, w, r2, 2);
            vec![px.clone(), a, b]
        } else {
            Vec::new()
        };
        Self {
            kind,
            px,
            w,
            h,
            rect,
            start: Instant::now(),
            t: 0,
            hint: [rect],
            screens,
            shown: 0,
        }
    }

    /// The next image (moves only during the play phase).
    fn grab(&mut self) -> Grab<'_> {
        if self.kind == PatternKind::Photo {
            let period = Self::SWITCH.as_millis();
            let i = ((self.start.elapsed().as_millis() / period) % 3) as usize;
            let switched = i != self.shown;
            if switched {
                self.shown = i;
                self.px.copy_from_slice(&self.screens[i]);
            }
            self.hint[0] = TileRect {
                x: 0,
                y: 0,
                w: self.w as u16,
                h: self.h as u16,
            };
            return Grab {
                px: &self.px,
                w: self.w,
                h: self.h,
                hints: Some(if switched { &self.hint[..] } else { &[] }),
                moves: None,
                backend: Backend::Pattern,
            };
        }
        let cycle = Self::MOVE + Self::REST;
        let phase =
            Duration::from_nanos((self.start.elapsed().as_nanos() % cycle.as_nanos()) as u64);
        let moving = phase < Self::MOVE;
        if moving {
            self.t = self.t.wrapping_add(1);
            match self.kind {
                PatternKind::Video => {
                    ovapp::video::synth_video(&mut self.px, self.w, self.rect, self.t)
                }
                PatternKind::Scroll => {
                    ovapp::video::synth_scroll(&mut self.px, self.w, self.rect, self.t, 12)
                }
                PatternKind::Photo => {}
            }
        }
        Grab {
            px: &self.px,
            w: self.w,
            h: self.h,
            hints: Some(if moving { &self.hint[..] } else { &[] }),
            moves: None,
            backend: Backend::Pattern,
        }
    }
}

/// Wave 2 photo pattern: a photo-like picture (gradients, a sine, fine
/// noise) in `r` of a `w`-wide BGRA frame.
fn photo_fill(px: &mut [u8], w: u32, r: TileRect, seed: u64) {
    let mut s: u64 = 0x9e37_79b9_7f4a_7c15 ^ seed;
    for y in usize::from(r.y)..usize::from(r.y) + usize::from(r.h) {
        for x in usize::from(r.x)..usize::from(r.x) + usize::from(r.w) {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            let n = (s >> 58) as i32 - 32;
            let o = (y * w as usize + x) * 4;
            let k = seed as usize;
            let base = [
                ((x * (7 + k) + y * 3) / 9 % 256) as i32,
                ((x * 2 + y * (5 + k)) / 8 % 256) as i32,
                ((x as f32 / 37.0).sin() * 60.0 + 128.0) as i32 + (y / 9) as i32 % 64,
            ];
            for c in 0..3 {
                px[o + c] = (base[c] + n / 3).clamp(0, 255) as u8;
            }
            px[o + 3] = 255;
        }
    }
}

impl ScreenSource {
    /// Wrap a GDI source (which also defines the region) and try DXGI on
    /// the same region.
    pub fn new(gdi: GdiScreen, prefer_dxgi: bool) -> Self {
        let mut s = Self {
            gdi,
            #[cfg(windows)]
            dxgi: None,
            #[cfg(windows)]
            dxgi_pending: None,
            pending_logged: false,
            prefer_dxgi,
            retry_at: None,
            last: None,
            pattern: None,
            latency: None,
        };
        s.try_dxgi(DXGI_INITIAL_WAIT_MS);
        s
    }

    /// Replace the screen with a synthetic test pattern (Phase 5).
    pub fn set_pattern(&mut self, kind: PatternKind) {
        let (w, h) = self.gdi.size();
        self.pattern = Some(TestPattern::new(kind, w & !1, h & !1));
    }

    pub fn origin(&self) -> (i32, i32) {
        self.gdi.origin()
    }

    pub fn size(&self) -> (u32, u32) {
        self.gdi.size()
    }

    /// Re-target to a new desktop rect (monitor switch).
    pub fn retarget(&mut self, x: i32, y: i32, w: u32, h: u32) -> Result<(), String> {
        self.gdi.retarget(x, y, w, h)?;
        if let Some(kind) = self.pattern.as_ref().map(|p| p.kind) {
            self.set_pattern(kind);
        }
        #[cfg(windows)]
        {
            self.dxgi = None;
            self.dxgi_pending = None;
        }
        self.retry_at = None;
        self.try_dxgi(DXGI_INITIAL_WAIT_MS);
        Ok(())
    }

    /// The backend currently in use.
    pub fn backend(&self) -> Backend {
        #[cfg(windows)]
        if self.dxgi.is_some() {
            return Backend::Dxgi;
        }
        Backend::Gdi
    }

    /// Create the duplication (waiting at most `wait_ms` for its initial
    /// image): active with an image, pending without one, retried after
    /// [`DXGI_RETRY`] when creation itself fails.
    fn try_dxgi(&mut self, wait_ms: u32) {
        #[cfg(windows)]
        {
            if !self.prefer_dxgi || self.dxgi.is_some() || self.dxgi_pending.is_some() {
                return;
            }
            let (x, y) = self.gdi.origin();
            let (w, h) = self.gdi.size();
            match crate::dxgi::DxgiCapture::open_region(x, y, w, h, wait_ms) {
                Ok(d) if d.has_image() => {
                    self.dxgi = Some(d);
                    self.retry_at = None;
                }
                Ok(d) => {
                    if !self.pending_logged {
                        self.pending_logged = true;
                        tracing::info!(
                            "DXGI delivers no desktop image (no active monitor / display off?); \
                             using GDI until it does"
                        );
                    }
                    self.dxgi_pending = Some(d);
                    self.retry_at = None;
                }
                Err(e) => {
                    if self.retry_at.is_none() {
                        tracing::warn!("DXGI capture unavailable ({e}); using GDI");
                    }
                    self.retry_at = Some(Instant::now() + DXGI_RETRY);
                }
            }
        }
        #[cfg(not(windows))]
        let _ = wait_ms;
    }

    /// Poll a pending duplication (never blocks): promote it on its first
    /// image, drop it (retry later) when it is lost.
    #[cfg(windows)]
    fn poll_pending(&mut self) {
        let Some(p) = self.dxgi_pending.as_mut() else {
            return;
        };
        match p.update() {
            Ok(()) if p.has_image() => {
                self.dxgi = self.dxgi_pending.take();
                self.retry_at = None;
            }
            Ok(()) => {}
            Err(e) => {
                tracing::debug!("pending {e}; retrying in {DXGI_RETRY:?}");
                self.dxgi_pending = None;
                self.retry_at = Some(Instant::now() + DXGI_RETRY);
            }
        }
    }

    /// Section 5 `--gpu-encode`: the GPU copy of what the next
    /// [`grab`](Self::grab) returns (DXGI only).
    #[cfg(windows)]
    pub fn gpu_frame(&self) -> Option<ovapp::mfcodec::GpuFrame> {
        if self.pattern.is_some() {
            return None;
        }
        self.dxgi.as_ref()?.gpu_frame()
    }

    /// Wave 1: whether [`wait_change`](Self::wait_change) can block on the
    /// OS (an active DXGI duplication; not GDI / a pending duplication /
    /// the test pattern).
    pub fn can_wait(&self) -> bool {
        #[cfg(windows)]
        {
            self.pattern.is_none() && self.dxgi.is_some()
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    /// Wave 1: present-to-grab delay of the last grab (see `latency`).
    pub fn take_latency(&mut self) -> Option<Duration> {
        self.latency.take()
    }

    /// Section 5 (event-driven capture): block up to `timeout` for the
    /// screen to change. `None` = this backend cannot wait (GDI / test
    /// pattern: the caller sleeps instead); `Some(true)` = a change is
    /// buffered for the next [`grab`](Self::grab).
    pub fn wait_change(&mut self, timeout: std::time::Duration) -> Option<bool> {
        if self.pattern.is_some() {
            return None;
        }
        #[cfg(windows)]
        {
            let d = self.dxgi.as_mut()?;
            match d.wait(timeout.as_millis().min(u128::from(u32::MAX)) as u32) {
                Ok(changed) => Some(changed),
                Err(e) => {
                    // The next grab re-creates the duplication.
                    tracing::debug!("{e}; re-creating");
                    self.dxgi = None;
                    self.retry_at = Some(Instant::now());
                    Some(true)
                }
            }
        }
        #[cfg(not(windows))]
        {
            let _ = timeout;
            None
        }
    }

    /// Grab the current image.
    pub fn grab(&mut self) -> Result<Grab<'_>, String> {
        if self.pattern.is_some() {
            self.note(Backend::Pattern);
            return Ok(self.pattern.as_mut().expect("checked above").grab());
        }
        #[cfg(windows)]
        {
            if self.dxgi.is_none() {
                self.poll_pending();
            }
            if self.dxgi.is_none()
                && self.dxgi_pending.is_none()
                && self.retry_at.is_some_and(|t| Instant::now() >= t)
            {
                // Periodic retry: never wait for an image here (that
                // stalled a grab ~1.1 s per retry on a monitor-less
                // output); a duplication without one stays pending.
                self.try_dxgi(0);
            }
            if let Some(d) = self.dxgi.as_mut() {
                if let Err(e) = d.update() {
                    tracing::debug!("{e}; re-creating");
                    self.dxgi = None;
                    self.retry_at = None;
                    self.try_dxgi(DXGI_INITIAL_WAIT_MS);
                }
            }
            if self.dxgi.is_some() {
                self.note(Backend::Dxgi);
                let Some(d) = self.dxgi.as_mut() else {
                    unreachable!()
                };
                self.latency = d.take_present_qpc().map(|t| {
                    let (now, f) = qpc_now();
                    Duration::from_secs_f64((now - t).max(0) as f64 / f as f64)
                });
                let d = &*d;
                let (px, w, h, hints) = d.frame();
                return Ok(Grab {
                    px,
                    w,
                    h,
                    hints,
                    moves: d.moves(),
                    backend: Backend::Dxgi,
                });
            }
        }
        self.note(Backend::Gdi);
        let (px, w, h) = self.gdi.capture_ref()?;
        Ok(Grab {
            px,
            w,
            h,
            hints: None,
            moves: None,
            backend: Backend::Gdi,
        })
    }

    fn note(&mut self, b: Backend) {
        if self.last != Some(b) {
            tracing::info!("screen capture via {b}");
            self.last = Some(b);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression (Start menu shown late on the phone): when Desktop
    /// Duplication delivers no frames (a display output without an active
    /// monitor — the PC's TV is off — so DWM never presents), the source
    /// runs on GDI. Retrying DXGI must never stall a grab: each retry used
    /// to block ~1.1 s waiting for an initial image, every 2 s, so a third
    /// of all screen changes (a Start menu opened by the Win key) reached
    /// the viewer only after that stall. On machines where DXGI works this
    /// passes trivially (no retries).
    #[test]
    fn grabs_never_stall_on_a_duplication_without_frames() {
        ovapp::gdisrc::enable_dpi_awareness();
        let Ok(gdi) = GdiScreen::full_primary() else {
            eprintln!("no screen: skipping");
            return;
        };
        let mut src = ScreenSource::new(gdi, true);
        let t0 = Instant::now();
        let mut worst = Duration::ZERO;
        let mut grabs = 0;
        // Longer than DXGI_RETRY, so at least one retry falls inside.
        while t0.elapsed() < DXGI_RETRY + Duration::from_millis(1500) {
            let t = Instant::now();
            src.grab().expect("grab");
            worst = worst.max(t.elapsed());
            grabs += 1;
            std::thread::sleep(Duration::from_millis(20));
        }
        eprintln!("{grabs} grabs via {}, worst {worst:?}", src.backend());
        assert!(
            worst < Duration::from_millis(400),
            "a grab stalled for {worst:?}"
        );
    }
}
