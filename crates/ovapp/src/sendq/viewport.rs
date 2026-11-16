//! [`UpdateEncoder`] viewport priority (`CAP_VIEWPORT`): tiles the viewer
//! shows go first; changed tiles outside its visible rect are deferred into
//! the refinement backlog (sent once they settle, see `refine.rs`) — but
//! never for longer than [`OFFVIEW_MAX_DEFER`]: past that an update carries
//! them all again, so off-screen content stays at most that stale.

use std::time::Duration;

use crate::tiles::{TileMap, TileRect};

use super::*;

/// Longest a changed off-viewport tile waits before an update carries it
/// regardless (the refinement lane usually sends it sooner, once settled).
pub(crate) const OFFVIEW_MAX_DEFER: Duration = Duration::from_millis(500);

impl UpdateEncoder {
    /// The viewer's visible rect in frame pixels (`None` = it shows
    /// everything / did not say). Backlog tiles that just came into view
    /// are refined at once.
    pub fn set_viewport(&mut self, v: Option<TileRect>) {
        let v = v.filter(|r| r.w > 0 && r.h > 0);
        if v == self.viewport {
            return;
        }
        self.viewport = v;
        let (Some(r), Some((w, h))) = (v, self.last_dims) else {
            return;
        };
        let now = self.now_ms();
        self.ensure_change_clock(w, h, now);
        let mut m = TileMap::new(w, h);
        m.mark_rect(&r);
        if self.backlog.cols() != m.cols() || self.backlog.rows() != m.rows() {
            return;
        }
        let settled = now.saturating_sub(REFINE_DELAY.as_millis() as u32);
        let cols = m.cols();
        for row in 0..m.rows() {
            for col in 0..cols {
                if m.get(col, row) && self.backlog.get(col, row) {
                    let i = (row * cols + col) as usize;
                    self.changed_ms[i] = self.changed_ms[i].min(settled);
                }
            }
        }
    }

    /// The viewport last set.
    pub fn viewport(&self) -> Option<TileRect> {
        self.viewport
    }

    /// Shape `view_map` (tiles touching the viewport) for a `w x h` frame;
    /// `false` when there is no viewport or it covers every tile.
    pub(super) fn update_view_map(&mut self, w: u32, h: u32) -> bool {
        let Some(r) = self.viewport else {
            return false;
        };
        self.view_map.reset(w, h);
        self.view_map.mark_rect(&r);
        self.view_map.count() < self.view_map.len()
    }

    /// Move the off-viewport tiles of `acc` (except this update's refresh
    /// chunk) into `defer_map`, unless deferring has gone on for
    /// [`OFFVIEW_MAX_DEFER`] (then this update carries everything).
    pub(super) fn defer_offview(&mut self, w: u32, h: u32, now: u32) {
        self.defer_map.reset(w, h);
        if !self.update_view_map(w, h) {
            self.defer_since = None;
            return;
        }
        let max = OFFVIEW_MAX_DEFER.as_millis() as u32;
        if self
            .defer_since
            .is_some_and(|t| now.saturating_sub(t) >= max)
        {
            self.defer_since = None; // flush: this update sends it all
            return;
        }
        self.defer_map.copy_from(&self.acc);
        self.defer_map.subtract(&self.view_map);
        self.defer_map.subtract(&self.chunk);
        if self.defer_map.count() == 0 {
            return;
        }
        self.acc.subtract(&self.defer_map);
        self.defer_since.get_or_insert(now);
        self.stats.deferred_tiles += self.defer_map.count() as u64;
    }
}
