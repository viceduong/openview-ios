//! Per-packet congestion control and pacing for the direct UDP
//! path — sans-IO, shared by the desktop [`crate::DirectCarrier`] and the
//! iOS FFI engine.
//!
//! Every datagram the [`crate::direct`] engine puts on a v2 link carries a
//! transport-wide sequence number (`tseq`) and its send timestamp
//! ([`crate::media`] wire format). The sender remembers `(tseq, send time,
//! size)` in a `SentHistory`; the receiver (`FeedbackRx`) records the
//! arrival time of every `tseq` and returns TWCC-style feedback every
//! `FEEDBACK_INTERVAL`. [`Gcc`] turns the feedback into a target rate:
//!
//! - **delay-based** (a port of the GCC trendline estimator): packets are
//!   grouped by send time (5 ms bursts); the inter-group delay variation
//!   `d = Δarrival - Δsend` is accumulated, smoothed, and a least-squares
//!   slope over the last `TREND_WINDOW` groups is compared against an
//!   adaptive threshold → overuse / normal / underuse;
//! - **AIMD**: overuse cuts the target to 0.85 x the measured receive rate
//!   (at most once per RTT); normal grows it (fast startup, then
//!   multiplicative, additive near the last known capacity); the target
//!   never runs far ahead of what the receiver actually gets (app-limited
//!   senders keep a sane estimate);
//! - **loss**: only a sustained loss rate above `LOSS_HIGH` lowers the rate
//!   (`x (1 - 0.5 loss)`). Random loss below that is the FEC's problem, not
//!   a congestion signal — throughput does not collapse on a lossy Wi-Fi.
//!
//! [`Pacer`] spreads the sender's datagrams at `PACING_GAIN x target` with a
//! small burst budget, so a video frame leaves over about half a frame
//! interval instead of as a line-rate burst that overflows a shallow
//! bottleneck queue.

use std::time::Duration;

mod feedback;
mod gcc;
mod pacer;

pub use feedback::PacketReport;
pub(crate) use feedback::{parse_feedback, FeedbackRx, SentHistory};
pub use gcc::{CcStats, Gcc, Usage};
pub use pacer::{Lane, Pacer};

/// How often the receiver returns feedback.
pub(crate) const FEEDBACK_INTERVAL: Duration = Duration::from_millis(25);
/// Feedback arrival-delta resolution (microseconds per unit).
pub(crate) const FEEDBACK_UNIT_US: u64 = 50;
/// Most packets one feedback message reports.
pub(crate) const FEEDBACK_MAX: usize = 500;
/// Pacing rate = this x the target rate. The encoder spends ~70 % of the
/// target (plus FEC), so a frame leaves within ~half its interval; a higher
/// gain only adds self-inflicted queueing at the bottleneck once the target
/// approaches the capacity.
pub(crate) const PACING_GAIN: f64 = 1.5;
/// Window of the standing-queue filter (see [Gcc]): about three frames.
pub(crate) const STANDING_WINDOW_US: i64 = 100_000;
/// A trendline overuse only counts while the queue has not drained for
/// [STANDING_WINDOW_US] — at least this much standing delay. Paced video
/// bursts that exceed the capacity for a few ms inside a frame (and drain
/// before the next one) are not congestion.
pub(crate) const STANDING_MIN_US: f64 = 4_000.0;
/// Start rate before any feedback (bits/s).
pub(crate) const START_RATE: f64 = 6_000_000.0;
/// Rate floor. Live Wi-Fi sessions showed what a low floor costs: an idle
/// sender's estimate ratcheted down to 0.3 Mbps on jitter, and the next
/// full-screen update (with the PONGs and input acks queued behind it on the
/// reliable lane) took seconds. 2 Mbps keeps any update within ~1 s even
/// in the worst case; a genuinely slower path still sees the delay signal.
pub(crate) const MIN_RATE: f64 = 2_000_000.0;
/// An app-limited sender (receiving well below its target) only treats an
/// overuse as congestion once the standing queue exceeds this: Wi-Fi
/// aggregation / power-save spikes of 20-40 ms on a few packets are not.
pub(crate) const APP_LIMITED_STANDING_US: f64 = 25_000.0;
/// ... and only if that queue never drained for this long.
pub(crate) const APP_LIMITED_WINDOW_US: i64 = 400_000;
/// How long a learned capacity is trusted for the fast restore after idle.
pub(crate) const CAPACITY_MEMORY: Duration = Duration::from_secs(60);
pub(crate) const MAX_RATE: f64 = 200_000_000.0;
/// Loss rate above which the controller backs off (random loss below it is
/// left to FEC / retransmission).
pub(crate) const LOSS_HIGH: f64 = 0.10;
/// Window the loss rate is measured over.
pub(crate) const LOSS_WINDOW: Duration = Duration::from_secs(1);
/// Groups the trendline regression spans.
pub(crate) const TREND_WINDOW: usize = 20;
const TREND_SMOOTHING: f64 = 0.9;
const TREND_GAIN: f64 = 4.0;
const THRESH_INIT: f64 = 12.5;
const K_UP: f64 = 0.0087;
const K_DOWN: f64 = 0.039;
const OVERUSE_TIME_MS: f64 = 10.0;
const BURST_US: u64 = 5_000;
const HISTORY: usize = 8192;

#[cfg(test)]
mod tests;
