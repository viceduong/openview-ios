//! The direct UDP path and the unreliable media channel on
//! [`TerminalSession`]: installing the direct carrier, sending video / audio
//! on the media channel, and the transport's rate / delay estimates.

use std::time::Duration;

use ovtransport::{Carrier, TransportError};

use super::{Command, TerminalSession, CAP_DIRECT, CAP_MEDIA};

impl TerminalSession {
    /// One-line summary of the unreliable media channel (`media: off`
    /// while no direct carrier is installed).
    pub fn media_summary(&self) -> String {
        let Some(s) = self.media_stats() else {
            return String::from("media: off");
        };
        format!(
            "media: rx {} unit(s) / {} lost frame(s) (fec-recovered {}, nack-recovered {}, {} nack(s)), audio {} pkt / {} lost; \
             tx {} frame(s) {} shard(s) ({} parity, {} resent); cc target {:.1} Mbps, loss {:.3}, fec {:.0}%, qdelay {:.1} ms, \
             {} overuse(s), {} loss cut(s); bad {} replay {}",
            self.media_rx.0,
            self.media_rx.1,
            s.rx.fec_recovered,
            s.rx.nack_recovered,
            s.rx.nacks,
            self.media_rx.2,
            self.media_rx.3,
            s.frames_tx,
            s.shards_tx,
            s.parity_tx,
            s.shards_resent,
            s.target_bps / 1e6,
            s.loss,
            s.fec_ratio * 100.0,
            s.queue_delay_ms,
            s.cc.overuses,
            s.cc.loss_cuts,
            s.bad_rx,
            s.replayed_rx
        )
    }

    /// Whether the unreliable media channel is open right now (both
    /// peers advertised [`CAP_MEDIA`] and the direct path carries the
    /// session).
    pub fn media_ready(&self) -> bool {
        self.local_caps & CAP_MEDIA != 0
            && self.peer_caps.is_some_and(|c| c & CAP_MEDIA != 0)
            && self.carrier.direct_ref().is_some_and(|d| d.media_ready())
    }

    /// Send one H.264 access unit on the media channel (FEC, paced).
    /// `Ok(false)` = the channel is not available; send it reliably.
    pub fn send_video_media(
        &mut self,
        header: &crate::video::VideoHeader,
        data: &[u8],
    ) -> Result<bool, TransportError> {
        if !self.media_ready() {
            return Ok(false);
        }
        let mut unit = Vec::with_capacity(crate::video::VIDEO_HEADER_LEN + data.len());
        header.encode(data.len() as u32, &mut unit);
        unit.extend_from_slice(data);
        let ok = self
            .carrier
            .direct_mut()
            .is_some_and(|d| d.send_media_video(&unit, header.is_keyframe()));
        if ok {
            self.stats.cmds_tx += 1;
        }
        Ok(ok)
    }

    /// Send one AUDIO payload: on the media channel when it is open (a lost
    /// packet is concealed by the receiver instead of stalling the
    /// session), as a reliable AUDIO record otherwise.
    pub fn send_audio(&mut self, payload: Vec<u8>) -> Result<(), TransportError> {
        if self.media_ready()
            && self
                .carrier
                .direct_mut()
                .is_some_and(|d| d.send_media_audio(&payload))
        {
            self.stats.cmds_tx += 1;
            return Ok(());
        }
        self.send(&Command::Audio(payload))
    }

    /// The direct path's congestion-controller target (bytes/s)
    /// while the media channel is open — the rate the encoders and the
    /// sender should plan with (`None` = relay or no v2: use the link
    /// pacer's estimate).
    pub fn transport_rate(&self) -> Option<f64> {
        if !self.media_ready() {
            return None;
        }
        self.carrier
            .direct_ref()
            .and_then(|d| d.cc_target())
            .map(|b| b / 8.0)
    }

    /// The congestion controller's one-way queueing-delay estimate.
    pub fn transport_queue_delay(&self) -> Option<Duration> {
        if !self.media_ready() {
            return None;
        }
        self.carrier.direct_ref().and_then(|d| d.cc_queue_delay())
    }

    /// The FEC share of the video sent on the media channel.
    pub fn media_fec_ratio(&self) -> f64 {
        self.carrier
            .direct_ref()
            .and_then(|d| d.media_stats())
            .map_or(0.0, |s| s.fec_ratio)
    }

    /// Media channel counters (`None` = not enabled).
    pub fn media_stats(&self) -> Option<ovtransport::direct::MediaStats> {
        self.carrier.direct_ref().and_then(|d| d.media_stats())
    }

    /// Wrap the carrier in an [`ovtransport::DirectCarrier`] on
    /// `sockets` and advertise [`CAP_DIRECT`] (call before
    /// [`announce_caps`](Self::announce_caps) / before the peer's CAPS is
    /// pumped). A host passes its candidate list (offered once the viewer's
    /// CAPS arrives); a viewer passes none and probes what the host offers.
    pub fn enable_direct(
        &mut self,
        sockets: ovtransport::DirectSockets,
        candidates: Vec<std::net::SocketAddr>,
    ) {
        if self.carrier.direct_ref().is_some() {
            return;
        }
        let key = ovtransport::direct::session_direct_key(&self.session);
        let inner = std::mem::replace(&mut self.carrier, Box::new(ClosedCarrier::default()));
        let mut d =
            ovtransport::DirectCarrier::new(inner, sockets, self.session.role_is_server(), key);
        d.set_candidates(candidates);
        self.carrier = Box::new(d);
        // The direct carrier speaks the v2 framing / media channel.
        self.local_caps |= CAP_DIRECT | CAP_MEDIA;
    }

    /// The direct-path state (`None` = no direct carrier installed).
    pub fn direct_state(&self) -> Option<ovtransport::DirectState> {
        self.carrier.direct_ref().map(|d| d.state())
    }

    /// The direct peer address, while sending directly.
    pub fn direct_peer(&self) -> Option<std::net::SocketAddr> {
        self.carrier.direct_ref().and_then(|d| d.direct_peer())
    }

    /// Direct-path counters (`None` = no direct carrier).
    pub fn direct_stats(&self) -> Option<ovtransport::direct::DirectStats> {
        self.carrier.direct_ref().map(|d| d.direct_stats())
    }

    /// Path events (promotions / demotions / probe results) since the last
    /// call, for the shells' logs.
    pub fn take_direct_events(&mut self) -> Vec<String> {
        self.carrier
            .direct_mut()
            .map(|d| d.take_events())
            .unwrap_or_default()
    }

    /// Extra sockets a readiness wait should watch (the direct-path UDP
    /// sockets); pair with [`raw_socket`](Self::raw_socket) in
    /// [`crate::netwait::Wakeup::with_sockets`].
    pub fn extra_sockets(&self) -> Vec<ovtransport::RawSock> {
        self.carrier.extra_sockets()
    }

    /// The carrier's socket, when a loop may wait on it for readability
    /// (see [`Carrier::raw_socket`]); `None` = fall back to timed waits.
    pub fn raw_socket(&self) -> Option<ovtransport::RawSock> {
        self.carrier.raw_socket()
    }
}

/// Placeholder carrier while [`TerminalSession::enable_direct`] swaps the
/// real one.
#[derive(Default)]
struct ClosedCarrier {
    stats: ovtransport::CarrierStats,
}

impl Carrier for ClosedCarrier {
    fn send(&mut self, _d: &[u8]) -> Result<usize, TransportError> {
        Err(TransportError::Closed)
    }
    fn poll_recv(&mut self, _s: &mut dyn FnMut(&[u8])) -> Result<usize, TransportError> {
        Err(TransportError::Closed)
    }
    fn close(&mut self) -> Result<(), TransportError> {
        Ok(())
    }
    fn is_reliable(&self) -> bool {
        true
    }
    fn stats(&self) -> &ovtransport::CarrierStats {
        &self.stats
    }
}
