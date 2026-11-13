//! End-of-session metrics, logged at `debug` (`-v`).
//!
//! The single `metrics:` line is parsed by benchmarks; keep its format.

use std::sync::atomic::Ordering;

use tracing::debug;

use super::Viewer;

/// The `q` quantile of an ascending slice by nearest rank (`None` when empty).
fn quantile<T: Copy>(sorted: &[T], q: f64) -> Option<T> {
    if sorted.is_empty() {
        return None;
    }
    let i = ((sorted.len() as f64 - 1.0) * q).round() as usize;
    Some(sorted[i.min(sorted.len() - 1)])
}

impl Viewer {
    /// Log the session summary: mic, frames/commands, tile state, H.264,
    /// latency/RTT/audio percentiles, and audio receive/playback.
    pub(super) fn log_summary(&self) {
        let term = &self.term;
        let secs = self.session_start.elapsed().as_secs_f64().max(0.001);
        if self.mic_enc.ticks > 0 {
            debug!(
                "mic sent {} ticks at {:.1} kbps",
                self.mic_enc.ticks,
                self.mic_enc.bitrate() / 1000.0
            );
        }
        let st = term.stats();
        debug!(
            "session summary: {} frame(s) presented ({}x{}), {} cmds rx ({:.0} KB/s), {} desync(s), {} keyframe request(s), peer caps {:#x}, last RTT {}",
            self.frames_presented,
            self.last_dims.0,
            self.last_dims.1,
            st.cmds_rx,
            term.bytes_received() as f64 / 1024.0 / secs,
            st.screen_desyncs,
            st.keyframe_requests,
            term.peer_caps().unwrap_or(0),
            self.last_rtt_ms
                .map_or("n/a".to_owned(), |r| format!("{r:.1} ms"))
        );
        // Lossy tiles still waiting for refinement (0 = exact) and the
        // tile-cache slots in use.
        let scr = term.screen();
        debug!(
            "wave2: {} lossy tile(s) on screen, {} tile-cache slot(s) filled",
            scr.lossy_tiles().count(),
            scr.cache_slots_filled()
        );
        let vs = term.video_stats();
        if vs.units > 0 || vs.ends > 0 {
            debug!(
                "H.264: {} unit(s) ({:.1} MB), {} painted, {:.2} ms/decode, capture-to-paint latency avg {:.1} ms / max {:.1} ms, \
                 {} dropped, {} decode error(s), {} IDR request(s), {} motion end(s)",
                vs.units,
                vs.bytes as f64 / 1e6,
                vs.painted,
                vs.decode_time.as_secs_f64() * 1000.0 / vs.painted.max(1) as f64,
                vs.latency_sum_us as f64 / 1000.0 / vs.latency_n.max(1) as f64,
                vs.latency_max_us as f64 / 1000.0,
                vs.dropped,
                vs.decode_errors,
                vs.idr_requests,
                vs.ends
            );
        }
        let mut lat: Vec<u32> = term.video_latencies().to_vec();
        lat.sort_unstable();
        let lat_ms = |q: f64| quantile(&lat, q).map_or(0.0, |us| f64::from(us) / 1000.0);
        let mut rtt = self.rtt_samples.clone();
        rtt.sort_by(|a, b| a.total_cmp(b));
        let rtt_ms = |q: f64| quantile(&rtt, q).unwrap_or(0.0);
        let audio = &self.audio_stats;
        debug!(
            "metrics: video_lat_ms p50 {:.1} p95 {:.1} p99 {:.1} n {} | stalls {} total {:.0} ms | \
             rtt_ms p50 {:.1} p95 {:.1} n {} | audio_gaps {} total {:.0} ms concealed {} | rx {:.2} Mbps | {}",
            lat_ms(0.5),
            lat_ms(0.95),
            lat_ms(0.99),
            lat.len(),
            vs.stalls,
            vs.stall_us as f64 / 1000.0,
            rtt_ms(0.5),
            rtt_ms(0.95),
            rtt.len(),
            audio.gaps.load(Ordering::Relaxed),
            audio.gap_us.load(Ordering::Relaxed) as f64 / 1000.0,
            audio.concealed.load(Ordering::Relaxed),
            term.bytes_received() as f64 * 8.0 / secs / 1e6,
            term.media_summary()
        );
        let ticks = audio.ticks.load(Ordering::Relaxed);
        if ticks > 0 {
            let bytes = audio.bytes.load(Ordering::Relaxed);
            debug!(
                "audio received {ticks} ticks ({} Opus), {:.1} kbps",
                audio.opus_ticks.load(Ordering::Relaxed),
                bytes as f64 * 8.0 / (ticks as f64 * 0.020) / 1000.0
            );
        }
        let (depth, underruns, dropped) = self.audio_bus.status();
        debug!(
            "audio playback: depth {depth} ms, {underruns} underrun(s), {dropped} sample(s) dropped"
        );
    }
}
