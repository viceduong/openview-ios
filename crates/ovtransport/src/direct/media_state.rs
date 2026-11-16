//! Session-wide media-channel state of the engine (keys, NACK memory,
//! counters).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::cc::CcStats;
use crate::media::{FecController, MediaKeys, MediaRx, MediaRxStats, PictureRequest, ReplayWindow};

/// Media sender keying (session-wide: survives path changes).
pub(super) struct MediaTx {
    pub(super) key: [u8; 32],
    /// Next packet number; starts at 1, never reused.
    pub(super) next_pn: u64,
}

/// One video frame kept for NACK retransmission.
pub(super) struct SentFrame {
    pub(super) id: u32,
    pub(super) k: u16,
    pub(super) msgs: Vec<Vec<u8>>,
    pub(super) at: Instant,
    pub(super) resends: u8,
}

/// Viewer: a PLI being repeated until the recovery picture arrives.
pub(super) struct PliRepeat {
    pub(super) msg: Vec<u8>,
    pub(super) next: Instant,
    pub(super) until: Instant,
}

/// Sender-side media counters.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct MediaStats {
    /// Video frames sent on the media channel.
    pub frames_tx: u64,
    /// Video data shards sent.
    pub shards_tx: u64,
    /// Reed-Solomon parity shards sent.
    pub parity_tx: u64,
    /// NACKs received.
    pub nack_rx: u64,
    /// Shards retransmitted for NACKs.
    pub shards_resent: u64,
    /// Audio packets sent.
    pub audio_tx: u64,
    /// PLI messages sent (repeats included).
    pub pli_tx: u64,
    /// PLI messages received (repeats included).
    pub pli_rx: u64,
    /// Congestion feedback messages sent.
    pub feedback_tx: u64,
    /// Media datagrams that failed authentication.
    pub bad_rx: u64,
    /// Media datagrams dropped as replays.
    pub replayed_rx: u64,
    /// Receiver counters.
    pub rx: MediaRxStats,
    /// Congestion-controller counters.
    pub cc: CcStats,
    /// Current controller target, bits/s.
    pub target_bps: f64,
    /// Current loss estimate (0..1).
    pub loss: f64,
    /// FEC parity ratio of the last video frame (adaptive: 2 % on a clean
    /// link with NACK in time, 10-30 % with loss).
    pub fec_ratio: f64,
    /// Estimated queueing delay, milliseconds.
    pub queue_delay_ms: f64,
}

/// Session-wide media state (see [`DirectEngine::enable_media`]).
pub(super) struct MediaState {
    pub(super) tx: MediaTx,
    pub(super) rx_key: [u8; 32],
    pub(super) replay: ReplayWindow,
    pub(super) rx: MediaRx,
    pub(super) next_frame: u32,
    pub(super) next_aseq: u32,
    pub(super) sent: VecDeque<SentFrame>,
    pub(super) sent_bytes: usize,
    /// Adaptive video FEC (parity ratio per frame).
    pub(super) fec: FecController,
    /// Viewer: the PLI being repeated, and the last request number used.
    pub(super) pli: Option<PliRepeat>,
    pub(super) pli_req: u32,
    /// Host: the newest PLI request number acted on, and the request
    /// waiting for [`take_media_events`](super::DirectEngine::take_media_events).
    pub(super) pli_seen: Option<u32>,
    pub(super) picture_request: Option<PictureRequest>,
    pub(super) stats: MediaStats,
}

/// NACK retransmission memory: frames, bytes, age.
const SENT_FRAMES: usize = 32;
const SENT_BYTES: usize = 8 << 20;
const SENT_AGE: Duration = Duration::from_millis(500);

impl MediaState {
    pub(super) fn new(keys: MediaKeys) -> Self {
        Self {
            tx: MediaTx {
                key: keys.tx,
                next_pn: 1,
            },
            rx_key: keys.rx,
            replay: ReplayWindow::new(),
            rx: MediaRx::new(),
            next_frame: 0,
            next_aseq: 0,
            sent: VecDeque::new(),
            sent_bytes: 0,
            fec: FecController::new(),
            pli: None,
            pli_req: 0,
            pli_seen: None,
            picture_request: None,
            stats: MediaStats::default(),
        }
    }

    pub(super) fn remember(&mut self, f: SentFrame, now: Instant) {
        self.sent_bytes += f.msgs.iter().map(Vec::len).sum::<usize>();
        self.sent.push_back(f);
        while self.sent.len() > SENT_FRAMES
            || self.sent_bytes > SENT_BYTES
            || self
                .sent
                .front()
                .is_some_and(|f| now.saturating_duration_since(f.at) > SENT_AGE)
        {
            match self.sent.pop_front() {
                Some(f) => self.sent_bytes -= f.msgs.iter().map(Vec::len).sum::<usize>(),
                None => break,
            }
        }
    }
}
