//! VIEWPORT reporting (`CAP_VIEWPORT`): the desktop window always shows
//! the whole remote frame (letterboxed), so it reports the full frame and
//! the scale it is drawn at — sent again only when either changes.

use ovapp::terminal::{Command, CAP_VIEWPORT};

use super::Viewer;

/// VIEWPORT for a `fw x fh` frame drawn `content_w` pixels wide: the full
/// frame, scale = content pixels per frame pixel x 1000 (`None` for an
/// empty frame / window).
pub(super) fn full_frame_viewport(fw: u32, fh: u32, content_w: i32) -> Option<Command> {
    if fw == 0 || fh == 0 || content_w <= 0 {
        return None;
    }
    let scale = (f64::from(content_w) * 1000.0 / f64::from(fw)).round();
    Some(Command::Viewport {
        x: 0,
        y: 0,
        w: fw.min(u32::from(u16::MAX)) as u16,
        h: fh.min(u32::from(u16::MAX)) as u16,
        scale_milli: scale.clamp(1.0, f64::from(u16::MAX)) as u16,
    })
}

impl Viewer {
    /// Report the viewport to a `CAP_VIEWPORT` host when it changed.
    pub(super) fn report_viewport(&mut self) -> Result<(), String> {
        let Some(w) = &self.win else {
            return Ok(());
        };
        if !self.term.peer_caps().is_some_and(|c| c & CAP_VIEWPORT != 0) {
            return Ok(());
        }
        let (fw, fh) = self.last_dims;
        let Some(cmd) = full_frame_viewport(fw, fh, w.content_rect().w) else {
            return Ok(());
        };
        if self.last_viewport.as_ref() == Some(&cmd) {
            return Ok(());
        }
        self.send(&cmd)?;
        self.last_viewport = Some(cmd);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_reports_the_full_frame_at_its_scale() {
        assert_eq!(
            full_frame_viewport(1920, 1080, 960),
            Some(Command::Viewport {
                x: 0,
                y: 0,
                w: 1920,
                h: 1080,
                scale_milli: 500
            })
        );
        assert_eq!(full_frame_viewport(0, 1080, 960), None);
        assert_eq!(full_frame_viewport(1920, 1080, 0), None);
    }
}
