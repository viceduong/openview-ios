//! [`PrioritySender`]: the control-first, one-frame-in-flight outbound scheduler.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use ovtransport::TransportError;

use crate::pacing::LinkPacer;
use crate::terminal::{Command, TerminalSession};

use super::*;

/// Smoothed per-frame drain time: from the FRAME_BEGIN send to the
/// FRAME_END send. On a backlogged link this is the link's real per-frame
/// cost; the capture thread uses it as a floor for its capture interval.
#[derive(Debug, Clone, Default)]
pub struct DrainMeter {
    /// EWMA of the drain time, seconds.
    smooth: Option<f64>,
    /// EWMA of the drained bytes per frame.
    bytes: Option<f64>,
    frames: u64,
}

impl DrainMeter {
    /// EWMA weight of a new sample.
    const ALPHA: f64 = 0.3;

    /// Record one fully drained frame.
    pub fn record(&mut self, bytes: usize, took: Duration) {
        let s = took.as_secs_f64();
        let ewma = |prev: Option<f64>, x: f64| match prev {
            Some(p) => p * (1.0 - Self::ALPHA) + x * Self::ALPHA,
            None => x,
        };
        self.smooth = Some(ewma(self.smooth, s));
        self.bytes = Some(ewma(self.bytes, bytes as f64));
        self.frames += 1;
    }

    /// Smoothed drain time per frame (`None` before the first frame).
    pub fn per_frame(&self) -> Option<Duration> {
        self.smooth.map(Duration::from_secs_f64)
    }

    /// Smoothed drain throughput in bytes per second.
    pub fn bytes_per_sec(&self) -> Option<f64> {
        match (self.smooth, self.bytes) {
            (Some(t), Some(b)) if t > 0.0 => Some(b / t),
            _ => None,
        }
    }

    /// Frames measured so far.
    pub fn frames(&self) -> u64 {
        self.frames
    }
}

/// The frame currently being streamed.
struct InFlight {
    frame: EncodedFrame,
    /// Bytes of `frame.data` already sent as FRAME_DATA.
    offset: usize,
    /// FRAME_BEGIN already sent?
    begun: bool,
    /// When FRAME_BEGIN went out.
    started: Instant,
}

/// One record the sender wants on the wire next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outgoing {
    /// A control command (always drained first).
    Control(Command),
    /// The next record of the in-flight frame.
    Frame(Command),
    /// A whole H.264 access unit for the media channel (FEC,
    /// paced by the transport; it never occupies the in-flight slot).
    Media(EncodedFrame),
}

/// Control-first, one-frame-in-flight outbound scheduler (see the module
/// docs). Pure state: [`next`](Self::next) decides, [`flush`](Self::flush)
/// pushes the decisions through a [`TerminalSession`].
pub struct PrioritySender {
    control: VecDeque<Command>,
    inflight: Option<InFlight>,
    chunk: usize,
    meter: DrainMeter,
    frames_sent: u64,
    pacer: LinkPacer,
    /// Video units go to the media channel (set per flush).
    media: bool,
    /// The transport's delay-based controller governs the rate
    /// (direct v2 path): the PING-window pacer is bypassed.
    governed: bool,
}

impl PrioritySender {
    /// A sender cutting FRAME_DATA into `chunk`-byte records (0 = default).
    pub fn new(chunk: usize) -> Self {
        Self {
            control: VecDeque::new(),
            inflight: None,
            chunk: if chunk == 0 { DEFAULT_CHUNK } else { chunk },
            meter: DrainMeter::default(),
            frames_sent: 0,
            pacer: LinkPacer::new(),
            media: false,
            governed: false,
        }
    }

    /// The link pacer.
    pub fn pacer(&self) -> &LinkPacer {
        &self.pacer
    }

    /// Mutable pacer access (e.g. to disable pacing).
    pub fn pacer_mut(&mut self) -> &mut LinkPacer {
        &mut self.pacer
    }

    /// Route a received PONG: `true` when it answered one of the pacer's
    /// probe PINGs (consumed), `false` when it belongs to someone else (the
    /// keepalive).
    pub fn on_pong(&mut self, nonce: u64, now: Instant) -> bool {
        self.pacer.on_pong(nonce, now)
    }

    /// Bytes that entered the network path (handed to the OS).
    fn path_bytes(term: &TerminalSession) -> u64 {
        term.bytes_sent().saturating_sub(term.pending_send() as u64)
    }

    /// Frame data is being held back by the pacer's window right now.
    pub fn paced(&self, term: &TerminalSession) -> bool {
        !self.governed
            && (self.inflight.is_some())
            && self
                .pacer
                .reopen_in(Instant::now(), Self::path_bytes(term))
                .is_some()
    }

    /// The longest a loop should block before calling [`flush`](Self::flush)
    /// again on the pacer's behalf (window reopening, or the next probe
    /// while frames flow); `None` = no pacing deadline.
    pub fn pacing_deadline(
        &self,
        term: &TerminalSession,
        mailbox: &FrameMailbox,
    ) -> Option<Duration> {
        if !self.pacer.enabled() || self.governed {
            return None;
        }
        let now = Instant::now();
        let active = self.inflight.is_some() || mailbox.is_pending();
        let reopen = self
            .inflight
            .as_ref()
            .and_then(|_| self.pacer.reopen_in(now, Self::path_bytes(term)));
        let probe = active
            .then(|| self.pacer.next_probe_at())
            .flatten()
            .map(|t| t.saturating_duration_since(now));
        match (reopen, probe) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Queue a control command; it goes out before any further frame data.
    pub fn push_control(&mut self, cmd: Command) {
        self.control.push_back(cmd);
    }

    /// Control commands waiting.
    pub fn pending_control(&self) -> usize {
        self.control.len()
    }

    /// A frame is partially sent.
    pub fn frame_in_flight(&self) -> bool {
        self.inflight.is_some()
    }

    /// Anything to send right now (control, an in-flight frame, or a new
    /// frame waiting in `mailbox`)?
    pub fn has_work(&self, mailbox: &FrameMailbox) -> bool {
        !self.control.is_empty() || self.inflight.is_some() || mailbox.is_pending()
    }

    /// Frames fully sent.
    pub fn frames_sent(&self) -> u64 {
        self.frames_sent
    }

    /// The drain-time meter.
    pub fn meter(&self) -> &DrainMeter {
        &self.meter
    }

    /// Estimated time until the in-flight frame is fully handed off, from
    /// its remaining bytes and the measured drain throughput. Zero when no
    /// frame is in flight or nothing has been measured yet. A capture
    /// thread that starts its next capture about this long before the link
    /// frees up delivers the freshest possible frame with no idle link.
    pub fn inflight_eta(&self) -> Duration {
        let (Some(f), Some(bps)) = (&self.inflight, self.meter.bytes_per_sec()) else {
            return Duration::ZERO;
        };
        let left = f.frame.data.len().saturating_sub(f.offset);
        if bps <= 0.0 {
            return Duration::ZERO;
        }
        Duration::from_secs_f64(left as f64 / bps)
    }

    /// The next record to send: queued control first; otherwise the next
    /// record of the in-flight frame; otherwise a FRAME_BEGIN for the frame
    /// waiting in `mailbox` (taken only when nothing else is in flight).
    pub fn next(&mut self, mailbox: &FrameMailbox, now: Instant) -> Option<Outgoing> {
        if let Some(cmd) = self.control.pop_front() {
            return Some(Outgoing::Control(cmd));
        }
        if self.inflight.is_none() {
            let frame = mailbox.take()?;
            if self.media && frame.video.is_some_and(|h| !h.is_end()) && !frame.data.is_empty() {
                self.frames_sent += 1;
                return Some(Outgoing::Media(frame));
            }
            self.inflight = Some(InFlight {
                frame,
                offset: 0,
                begun: false,
                started: now,
            });
        }
        let f = self.inflight.as_mut()?;
        if !f.begun {
            f.begun = true;
            f.started = now;
            return Some(Outgoing::Frame(f.frame.begin()));
        }
        if f.offset < f.frame.data.len() {
            let end = (f.offset + self.chunk).min(f.frame.data.len());
            let chunk = f.frame.data[f.offset..end].to_vec();
            f.offset = end;
            return Some(Outgoing::Frame(Command::FrameData(chunk)));
        }
        // All data out: close the frame and record how long it drained.
        let done = self.inflight.take()?;
        self.meter.record(
            done.frame.data.len(),
            now.saturating_duration_since(done.started),
        );
        self.frames_sent += 1;
        Some(Outgoing::Frame(Command::FrameEnd(done.frame.seq)))
    }

    /// Whether frame data must wait for the carrier: more than one chunk is
    /// still queued below us (the link is the bottleneck). Control is never
    /// held back by this.
    pub fn link_backlogged(&self, term: &TerminalSession) -> bool {
        term.pending_send() > self.chunk
    }

    /// Send every queued control command, then at most `max_frame_records`
    /// frame records — fewer when the carrier already queues more than one
    /// chunk (frame data never piles up ahead of later control). Returns the
    /// number of records sent. A control command too large for one record is
    /// skipped (nothing was sent, the session is intact); any other error is
    /// returned (the session is gone).
    pub fn flush(
        &mut self,
        term: &mut TerminalSession,
        mailbox: &FrameMailbox,
        max_frame_records: usize,
    ) -> Result<usize, TransportError> {
        let mut sent = 0usize;
        let mut frame_records = 0usize;
        // A probe PING ahead of the frame data (a cumulative ack
        // + RTT sample once the peer echoes it).
        let now = Instant::now();
        // On the direct v2 path the transport's controller paces
        // and rate-limits; video rides the media channel.
        self.governed = term.transport_rate().is_some();
        self.media = term.media_ready();
        if !self.governed
            && self
                .pacer
                .probe_due(now, self.inflight.is_some() || mailbox.is_pending())
        {
            let nonce = self.pacer.next_nonce();
            term.send(&Command::Ping(nonce))?;
            // The PING's stream position: everything handed to the carrier,
            // its own queue included (the PING may still sit in it).
            self.pacer.probe_sent(nonce, now, term.bytes_sent());
            sent += 1;
        }
        loop {
            if self.control.is_empty() {
                if frame_records >= max_frame_records {
                    break;
                }
                let has_frame = self.inflight.is_some() || mailbox.is_pending();
                if has_frame && self.link_backlogged(term) {
                    self.pacer.note_limited();
                    break;
                }
                if has_frame
                    && !self.governed
                    && !self.pacer.allow(Instant::now(), Self::path_bytes(term))
                {
                    break;
                }
            }
            let Some(out) = self.next(mailbox, Instant::now()) else {
                break;
            };
            match out {
                Outgoing::Control(cmd) => match term.send(&cmd) {
                    Ok(()) => sent += 1,
                    Err(TransportError::FrameTooLarge(n)) => {
                        tracing::warn!("control command too large ({n} B); skipped");
                    }
                    Err(e) => return Err(e),
                },
                Outgoing::Frame(cmd) => {
                    term.send(&cmd)?;
                    sent += 1;
                    frame_records += 1;
                }
                Outgoing::Media(f) => {
                    let header = f.video.unwrap_or_default();
                    if term.send_video_media(&header, &f.data)? {
                        sent += 1;
                        frame_records += 1;
                    } else {
                        // The channel closed meanwhile (path demoted):
                        // stream it reliably instead.
                        self.frames_sent = self.frames_sent.saturating_sub(1);
                        self.inflight = Some(InFlight {
                            frame: f,
                            offset: 0,
                            begun: false,
                            started: Instant::now(),
                        });
                    }
                }
            }
        }
        Ok(sent)
    }
}
