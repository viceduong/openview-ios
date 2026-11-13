//! Session recording (`.ovrec`): the F6 recorder used by the live session,
//! and the offline `--info`, `--export` and `--play` tools.

use std::path::Path;
use std::time::{Duration, Instant};

use ovapp::record::{RecordHeader, RecordWriter, RecordedFrame, CODEC_DEFLATE};
use ovapp::screen::{Frame, PixelFormat};
use tracing::{debug, info, warn};

/// `--export` without `--frame` warns (Ctrl-C to abort) before writing more
/// than this many frames.
const EXPORT_WARN_FRAMES: u64 = 500;

/// `--info <file.ovrec>`: print the header summary and recording stats.
pub(crate) fn info(path: &Path) -> Result<(), String> {
    let s = ovapp::record::validate(path)?;
    let h = &s.header;
    println!("file:            {}", path.display());
    println!("app_version:     {}", h.app_version);
    println!("pixel_format:    {} (0 = BGRA)", h.pixel_format);
    println!(
        "codec:           {} ({})",
        h.codec,
        if h.codec == CODEC_DEFLATE {
            "raw DEFLATE"
        } else {
            "unknown"
        }
    );
    println!("initial size:    {}x{}", h.initial_width, h.initial_height);
    println!(
        "quality_mode:    {}",
        if h.quality_mode == 0 {
            "AUTO".to_owned()
        } else {
            format!("{} fps", h.quality_mode)
        }
    );
    println!(
        "host_name:       {}",
        if h.host_name.is_empty() {
            "(none)"
        } else {
            &h.host_name
        }
    );
    println!("start_unix_ms:   {}", h.start_unix_ms);
    println!("frame_count:     {}", s.frame_count);
    println!("duration_ms:     {}", s.total_duration_ms);
    println!("bytes:           {}", s.bytes);
    Ok(())
}

/// `--export <file.ovrec> --out <dir> [--frame N]`: write frame(s) as PPM.
/// Without `--frame`, every frame is written (with a warning first if the
/// recording holds more than [`EXPORT_WARN_FRAMES`] frames).
pub(crate) fn export(path: &Path, out_dir: &Path, only_frame: Option<u32>) -> Result<(), String> {
    let summary = ovapp::record::validate(path)?;
    let frames = ovapp::record::read_frames(path)?;
    if let Some(n) = only_frame {
        let Some(f) = frames.iter().find(|f| f.seq == n) else {
            return Err(format!(
                "frame seq {n} not found (recording holds seqs {}..={})",
                frames.first().map_or(0, |f| f.seq),
                frames.last().map_or(0, |f| f.seq)
            ));
        };
        write_ppm(out_dir, f)?;
        info!(
            "wrote {}",
            out_dir.join(format!("frame-{}.ppm", f.seq)).display()
        );
        return Ok(());
    }
    if summary.frame_count > EXPORT_WARN_FRAMES {
        warn!(
            "about to export {} frames ({} MiB of PPM); Ctrl-C to abort",
            summary.frame_count,
            summary.frame_count
                * u64::from(summary.header.initial_width)
                * u64::from(summary.header.initial_height)
                * 3
                / (1024 * 1024)
        );
    }
    std::fs::create_dir_all(out_dir).map_err(|e| format!("create dir {:?}: {e}", out_dir))?;
    for f in &frames {
        write_ppm(out_dir, f)?;
    }
    info!(
        "exported {} frame(s) to {}",
        frames.len(),
        out_dir.display()
    );
    Ok(())
}

/// Encode one frame as PPM and write it to `<dir>/frame-<seq>.ppm`.
fn write_ppm(dir: &Path, f: &RecordedFrame) -> Result<(), String> {
    let ppm = ovapp::image::bgra_to_ppm(f.width, f.height, &f.pixels)?;
    let path = dir.join(format!("frame-{}.ppm", f.seq));
    std::fs::write(&path, &ppm).map_err(|e| format!("write {:?}: {e}", path))
}

/// `--play <file.ovrec> [--loop]`: present the recording in the render
/// window at the recorded pace. Reuses the live session's window + present
/// path (`RenderWindow::present`); closing the window or Ctrl-C stops.
pub(crate) fn play(path: &Path, loop_forever: bool) -> Result<(), String> {
    let summary = ovapp::record::validate(path)?;
    let frames = ovapp::record::read_frames(path)?;
    if frames.is_empty() {
        return Err("recording has no frames".into());
    }
    let pacing = ovapp::image::frame_pacing(&frames);
    info!(
        "playing {} ({} frames, {:.1} s) — Escape or Ctrl-C stops",
        path.display(),
        frames.len(),
        summary.total_duration_ms as f64 / 1000.0,
    );
    crate::ctrlc::install();
    let win =
        crate::window::RenderWindow::new().map_err(|e| format!("no window available: {e}"))?;
    let mut pass = 0u64;
    loop {
        pass += 1;
        if loop_forever && pass > 1 {
            debug!("loop pass {pass}");
        }
        for (f, delay) in frames.iter().zip(pacing.iter()) {
            if delay.is_zero() {
                win.pump_messages();
            } else {
                let deadline = Instant::now() + *delay;
                while Instant::now() < deadline {
                    win.pump_messages();
                    if win.should_quit() || crate::ctrlc::pressed() {
                        info!("playback stopped");
                        win.close();
                        return Ok(());
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            // Same present path the live session uses: build a `Frame` from
            // the recorded pixels and hand it to the letterboxing painter.
            let frame = Frame {
                width: f.width,
                height: f.height,
                stride: f.width * 4,
                format: PixelFormat::Bgra,
                seq: u64::from(f.seq),
                pixels: f.pixels.clone(),
            };
            win.present(&frame);
        }
        if !loop_forever {
            break;
        }
    }
    info!("playback finished");
    win.close();
    Ok(())
}

/// Default recording directory: `$USERPROFILE/ovrecord` (fallback: temp dir).
fn default_record_dir() -> String {
    let base =
        std::env::var("USERPROFILE").unwrap_or_else(|_| std::env::temp_dir().display().to_string());
    format!("{base}\\ovrecord")
}

/// Timestamped recording file name: `ovview-YYYYMMDD-HHMMSS-mmm.ovrec`,
/// computed from Unix time (UTC) without external deps — civil-from-days
/// per Howard Hinnant.
fn record_file_name(now_unix_ms: u64) -> String {
    let secs = now_unix_ms / 1000;
    let ms = now_unix_ms % 1000;
    let days = secs / 86_400;
    let sod = secs % 86_400;
    let (h, m, s) = (sod / 3600, (sod / 60) % 60, sod % 60);
    // Shift to a civil date (days since 1970-03-01 era algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("ovview-{y:04}{mo:02}{d:02}-{h:02}{m:02}{s:02}-{ms:03}.ovrec")
}

/// The F6 recorder of a live session: an `.ovrec` capture of the presented
/// frames. The writer is finalized on toggle-off and on any session exit so
/// the trailer is never missing.
pub(crate) struct Recorder {
    /// The active writer, `None` when not recording.
    writer: Option<RecordWriter>,
    /// When the active recording started (frame timestamps are relative).
    start: Instant,
    /// Folder new recordings go to.
    dir: String,
}

impl Recorder {
    /// A stopped recorder writing into `dir` (default
    /// `%USERPROFILE%\ovrecord`).
    pub(crate) fn new(dir: Option<String>) -> Self {
        Self {
            writer: None,
            start: Instant::now(),
            dir: dir.unwrap_or_else(default_record_dir),
        }
    }

    /// F6: stop the active recording, or start a new one (building the
    /// header from the current frame size, quality mode and peer).
    pub(crate) fn toggle(&mut self, dims: (u32, u32), quality: Option<u16>, host_name: &str) {
        if self.writer.is_some() {
            self.stop();
        } else {
            self.start(dims, quality, host_name);
        }
    }

    /// Start a new recording; failures are logged and leave it stopped.
    fn start(&mut self, dims: (u32, u32), quality: Option<u16>, host_name: &str) {
        let now_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let dir_path = Path::new(&self.dir);
        if let Err(e) = std::fs::create_dir_all(dir_path) {
            warn!("cannot create record dir {}: {e}", self.dir);
            return;
        }
        let path = dir_path.join(record_file_name(now_unix_ms));
        let header = RecordHeader {
            app_version: "0.23.0".to_owned(),
            pixel_format: PixelFormat::Bgra.tag(),
            codec: CODEC_DEFLATE,
            initial_width: dims.0,
            initial_height: dims.1,
            quality_mode: quality.map_or(0, |q| q.min(255) as u8),
            host_name: host_name.to_owned(),
            start_unix_ms: now_unix_ms,
        };
        match RecordWriter::create(&path, &header) {
            Ok(w) => {
                info!("recording to {}", path.display());
                self.start = Instant::now();
                self.writer = Some(w);
            }
            Err(e) => warn!("cannot start recording: {e}"),
        }
    }

    /// Finalize an active recording (if any) and log the summary.
    pub(crate) fn stop(&mut self) {
        if let Some(w) = self.writer.take() {
            match w.finalize() {
                Ok(s) => info!(
                    "recording saved: {} frames, {:.1} s, {:.2} MiB",
                    s.frame_count,
                    s.total_duration_ms as f64 / 1000.0,
                    s.bytes as f64 / (1024.0 * 1024.0),
                ),
                Err(e) => warn!("finalize recording failed: {e}"),
            }
        }
    }

    /// Append a presented frame to the active recording (no-op when
    /// stopped). A write error stops the recording.
    pub(crate) fn append(&mut self, f: &Frame) {
        let Some(rec) = self.writer.as_mut() else {
            return;
        };
        let ts = Instant::now().duration_since(self.start).as_millis() as u64;
        if let Err(e) = rec.append_frame(
            f.seq as u32,
            ts,
            &f.pixels,
            f.pixels.len() as u32,
            f.width,
            f.height,
        ) {
            warn!("recording stopped: {e}");
            self.writer = None;
        }
    }

    /// The HUD's recording indicator: elapsed ms, bytes written, frames
    /// captured — `None` when not recording.
    pub(crate) fn hud_status(&self) -> Option<(u64, u64, u64)> {
        self.writer.as_ref().map(|r| {
            (
                Instant::now().duration_since(self.start).as_millis() as u64,
                r.bytes_written(),
                r.frame_count(),
            )
        })
    }
}
