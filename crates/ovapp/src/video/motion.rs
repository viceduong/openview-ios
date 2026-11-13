//! Host-side motion detection: when a large screen region keeps changing,
//! switch it from lossless tiles to H.264.

use std::time::{Duration, Instant};

use crate::tiles::{TileMap, TileRect, TILE};

/// Thresholds of the [`MotionDetector`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MotionConfig {
    /// A compact dirty region at least this many pixels (bounding box) is
    /// "large" (320x240).
    pub min_area_px: u32,
    /// A sample dirtying at least this fraction of all tiles is "large"
    /// whatever its shape.
    pub screen_frac: f32,
    /// Dirty tiles / bounding-box tiles needed for a compact region (keeps
    /// scattered small updates — a clock and a caret — out).
    pub density: f32,
    /// Large samples must keep coming this long before motion starts.
    pub enter_after: Duration,
    /// Longest gap between large samples that still counts as continuous
    /// (a 24 fps video sampled at 30 fps has repeated frames).
    pub max_gap: Duration,
    /// The motion region must stay still this long before tiles return.
    pub exit_after: Duration,
    /// A motion rect covering this fraction of the screen snaps to the whole
    /// screen (one stable encoder size).
    pub whole_screen_frac: f32,
    /// Minimum time between two rect growths (each one restarts the
    /// encoder with an IDR).
    pub grow_every: Duration,
}

impl Default for MotionConfig {
    fn default() -> Self {
        Self {
            min_area_px: 320 * 240,
            screen_frac: 0.25,
            density: 0.5,
            // 65 ms: three samples at 30 fps (0, 33, 66 ms) — a video or a
            // drag switches to H.264 before many lossless frames of moving
            // content are sent.
            enter_after: Duration::from_millis(65),
            max_gap: Duration::from_millis(250),
            exit_after: Duration::from_secs(1),
            whole_screen_frac: 0.6,
            grow_every: Duration::from_secs(1),
        }
    }
}

/// What a [`MotionDetector`] sample changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotionEvent {
    /// Nothing changed.
    None,
    /// Motion started in this (tile-aligned) rect.
    Enter(TileRect),
    /// The motion rect grew to this rect.
    Grow(TileRect),
    /// Motion in this rect stopped.
    Exit(TileRect),
}

/// A box in tile units, `[c0, c1) x [r0, r1)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TBox {
    c0: u32,
    r0: u32,
    c1: u32,
    r1: u32,
}

impl TBox {
    fn tiles(&self) -> usize {
        ((self.c1 - self.c0) * (self.r1 - self.r0)) as usize
    }
    fn union(&self, o: &TBox) -> TBox {
        TBox {
            c0: self.c0.min(o.c0),
            r0: self.r0.min(o.r0),
            c1: self.c1.max(o.c1),
            r1: self.r1.max(o.r1),
        }
    }
    fn contains(&self, o: &TBox) -> bool {
        o.c0 >= self.c0 && o.r0 >= self.r0 && o.c1 <= self.c1 && o.r1 <= self.r1
    }
    fn px_rect(&self, w: u32, h: u32) -> TileRect {
        let x0 = (self.c0 * TILE).min(w);
        let y0 = (self.r0 * TILE).min(h);
        let x1 = (self.c1 * TILE).min(w);
        let y1 = (self.r1 * TILE).min(h);
        TileRect {
            x: x0 as u16,
            y: y0 as u16,
            w: (x1 - x0) as u16,
            h: (y1 - y0) as u16,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum MotionState {
    Idle {
        /// First large sample of the current run.
        since: Option<Instant>,
        last: Option<Instant>,
        acc: Option<TBox>,
        samples: u32,
    },
    Motion {
        bx: TBox,
        last_motion: Instant,
        last_grow: Instant,
    },
}

/// `b`, or the whole screen when it covers enough of it.
fn snap_box(cfg: &MotionConfig, b: TBox, cols: u32, rows: u32) -> TBox {
    if b.tiles() as f32 >= (cols * rows) as f32 * cfg.whole_screen_frac {
        TBox {
            c0: 0,
            r0: 0,
            c1: cols,
            r1: rows,
        }
    } else {
        b
    }
}

/// Host-side hysteresis state machine: feeds on the per-capture dirty-tile
/// map (consecutive captures) and reports when a large region starts moving,
/// grows, or comes to rest. Pure; the caller passes `now`.
#[derive(Debug, Clone)]
pub struct MotionDetector {
    cfg: MotionConfig,
    state: MotionState,
    /// The screen size of the last observed map (the session resets it on
    /// a forced geometry change).
    pub(super) dims: (u32, u32),
}

impl Default for MotionDetector {
    fn default() -> Self {
        Self::new(MotionConfig::default())
    }
}

impl MotionDetector {
    /// An idle detector with these thresholds.
    pub fn new(cfg: MotionConfig) -> Self {
        Self {
            cfg,
            state: MotionState::Idle {
                since: None,
                last: None,
                acc: None,
                samples: 0,
            },
            dims: (0, 0),
        }
    }

    /// The thresholds in use.
    pub fn config(&self) -> &MotionConfig {
        &self.cfg
    }

    /// Back to idle (no event).
    pub fn reset(&mut self) {
        self.state = MotionState::Idle {
            since: None,
            last: None,
            acc: None,
            samples: 0,
        };
    }

    /// The current motion rect, if in motion.
    pub fn motion_rect(&self) -> Option<TileRect> {
        match self.state {
            MotionState::Motion { bx, .. } => Some(bx.px_rect(self.dims.0, self.dims.1)),
            MotionState::Idle { .. } => None,
        }
    }

    /// Feed one capture's dirty-tile map (tiles that changed since the
    /// previous capture) of a `w x h` screen.
    pub fn observe(&mut self, now: Instant, map: &TileMap, w: u32, h: u32) -> MotionEvent {
        if map.is_empty() || w == 0 || h == 0 {
            return MotionEvent::None;
        }
        if self.dims != (w, h) {
            // A new geometry: whatever was moving is gone.
            let was = self.motion_rect();
            self.dims = (w, h);
            self.reset();
            if let Some(r) = was {
                return MotionEvent::Exit(r);
            }
        }
        let (cols, rows) = (map.cols(), map.rows());
        let cfg = self.cfg;
        // Dirty count + bounding box.
        let mut count = 0usize;
        let mut bb: Option<TBox> = None;
        for r in 0..rows {
            for c in 0..cols {
                if map.get(c, r) {
                    count += 1;
                    let t = TBox {
                        c0: c,
                        r0: r,
                        c1: c + 1,
                        r1: r + 1,
                    };
                    bb = Some(bb.map_or(t, |b| b.union(&t)));
                }
            }
        }
        let tile_px = (TILE * TILE) as f32;
        let large = bb.is_some_and(|b| {
            let n = count as f32;
            n >= map.len() as f32 * cfg.screen_frac
                || (b.tiles() as f32 * tile_px >= cfg.min_area_px as f32
                    && n >= b.tiles() as f32 * cfg.density
                    && n * tile_px * 2.0 >= cfg.min_area_px as f32)
        });
        match &mut self.state {
            MotionState::Idle {
                since,
                last,
                acc,
                samples,
            } => {
                if large {
                    let b = bb.expect("large implies a box");
                    let fresh_run =
                        last.is_none_or(|l| now.saturating_duration_since(l) > cfg.max_gap);
                    if fresh_run {
                        *since = Some(now);
                        *acc = Some(b);
                        *samples = 1;
                    } else {
                        *acc = Some(acc.map_or(b, |a| a.union(&b)));
                        *samples += 1;
                    }
                    *last = Some(now);
                    let run = since.map_or(Duration::ZERO, |s| now.saturating_duration_since(s));
                    if run >= cfg.enter_after && *samples >= 3 {
                        let bx = snap_box(&cfg, acc.expect("set above"), cols, rows);
                        self.state = MotionState::Motion {
                            bx,
                            last_motion: now,
                            last_grow: now,
                        };
                        return MotionEvent::Enter(bx.px_rect(w, h));
                    }
                } else if last.is_some_and(|l| now.saturating_duration_since(l) > cfg.max_gap) {
                    *since = None;
                    *last = None;
                    *acc = None;
                    *samples = 0;
                }
                MotionEvent::None
            }
            MotionState::Motion {
                bx,
                last_motion,
                last_grow,
            } => {
                let mut inside = 0usize;
                for r in bx.r0..bx.r1.min(rows) {
                    for c in bx.c0..bx.c1.min(cols) {
                        inside += usize::from(map.get(c, r));
                    }
                }
                // A blinking caret inside the region is not motion.
                let min_inside = if bx.tiles() <= 4 { 1 } else { 2 };
                if inside >= min_inside {
                    *last_motion = now;
                }
                if let Some(b) = bb {
                    if large
                        && !bx.contains(&b)
                        && now.saturating_duration_since(*last_grow) >= cfg.grow_every
                    {
                        let grown = snap_box(&cfg, bx.union(&b), cols, rows);
                        *bx = grown;
                        *last_grow = now;
                        *last_motion = now;
                        return MotionEvent::Grow(grown.px_rect(w, h));
                    }
                }
                if now.saturating_duration_since(*last_motion) >= cfg.exit_after {
                    let r = bx.px_rect(w, h);
                    self.reset();
                    return MotionEvent::Exit(r);
                }
                MotionEvent::None
            }
        }
    }
}

/// Dirty tiles of `map` inside the pixel rect `r`.
pub(crate) fn tiles_in_rect(map: &TileMap, r: &TileRect) -> usize {
    if map.is_empty() || r.w == 0 || r.h == 0 {
        return 0;
    }
    let c0 = u32::from(r.x) / TILE;
    let r0 = u32::from(r.y) / TILE;
    let c1 = (u32::from(r.x) + u32::from(r.w))
        .div_ceil(TILE)
        .min(map.cols());
    let r1 = (u32::from(r.y) + u32::from(r.h))
        .div_ceil(TILE)
        .min(map.rows());
    let mut n = 0;
    for row in r0..r1 {
        for col in c0..c1 {
            n += usize::from(map.get(col, row));
        }
    }
    n
}

/// H.264 encoder sizes are rounded up to this (px) so a growing
/// motion rect rarely forces a new encoder.
pub(crate) const ENCODER_BUCKET: u32 = 128;

/// The even-sized coded rect for a motion rect (H.264 4:2:0 needs even
/// dimensions; at an odd screen edge the last row/column stays with tiles).
pub(crate) fn coded_rect(r: TileRect) -> TileRect {
    TileRect {
        x: r.x,
        y: r.y,
        w: r.w & !1,
        h: r.h & !1,
    }
}
