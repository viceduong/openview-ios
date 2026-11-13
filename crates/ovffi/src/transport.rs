//! The session's transport: a host-owned datagram path or a Rust-owned /
//! host-bridged relay tunnel.

use std::collections::VecDeque;

use ovtransport::{Carrier, CarrierStats, RelayTunnelCarrier, TransportError};

use crate::callbacks::{DatagramSink, Sink};

/// A [`Carrier`] whose send path calls a host callback and whose receive
/// path is a queue fed by `ov_session_on_datagram`.
///
/// Datagrams pass through an [`ovtransport::rudp::ReliableLink`] in the
/// reliable framing **from the first packet** (<= 1.2 KB fragments,
/// ACK/retransmit, in-order), the handshake included: this transport is a
/// LAN session to a host's LAN port (`ovhost --lan-port`), which accepts a
/// framed HELLO and frames its side too. Starting raw (bare datagrams until
/// the CAPS exchange) lost the session on real Wi-Fi: one lost handshake
/// datagram stalled the handshake for good, and the host's first screen
/// records went out as oversized bare datagrams before our CAPS (sent only
/// after the first screen update) could switch it. Inbound bare datagrams
/// are still accepted. The shell keeps moving opaque datagrams; ACKs
/// and retransmissions go out through the same sink from `ov_session_pump`
/// (call it at least every ~50 ms in direct mode so RTO timers run).
pub(crate) struct FfiDatagramCarrier {
    sink: Sink<DatagramSink>,
    /// Datagrams from the shell, not yet run through the link.
    pub(crate) inbound: VecDeque<Vec<u8>>,
    /// Records the link released, in order.
    delivered: VecDeque<Vec<u8>>,
    link: ovtransport::rudp::ReliableLink,
    closed: bool,
    stats: CarrierStats,
}

impl FfiDatagramCarrier {
    pub(crate) fn new(sink: Sink<DatagramSink>) -> Self {
        let mut link = ovtransport::rudp::ReliableLink::new();
        link.enable();
        Self {
            sink,
            inbound: VecDeque::new(),
            delivered: VecDeque::new(),
            link,
            closed: false,
            stats: CarrierStats::default(),
        }
    }

    /// Run queued inbound datagrams + the link timers (ACKs, retransmits,
    /// queued sends), moving released records to `delivered`.
    fn service(&mut self) {
        let now = std::time::Instant::now();
        let sink = self.sink;
        let mut out = |d: &[u8]| -> std::io::Result<()> {
            match sink.f {
                Some(f) => {
                    f(sink.user, d.as_ptr(), d.len());
                    Ok(())
                }
                None => Err(std::io::Error::other("no datagram sink")),
            }
        };
        let delivered = &mut self.delivered;
        let stats = &mut self.stats;
        let mut deliver = |r: &[u8]| {
            stats.recv_datagrams += 1;
            stats.bytes_recv += r.len() as u64;
            delivered.push_back(r.to_vec());
        };
        let had = !self.inbound.is_empty();
        while let Some(d) = self.inbound.pop_front() {
            self.link.on_datagram(&d, now, &mut deliver, &mut out);
        }
        if had {
            self.link.end_batch(now, &mut deliver, &mut out);
        } else {
            self.link.poll(now, &mut deliver, &mut out);
        }
    }

    /// The next in-order record (servicing the link first when empty).
    pub(crate) fn take_inbound(&mut self) -> Option<Vec<u8>> {
        if self.delivered.is_empty() {
            self.service();
        }
        self.delivered.pop_front()
    }
}

impl Carrier for FfiDatagramCarrier {
    fn send(&mut self, datagram: &[u8]) -> Result<usize, TransportError> {
        if self.closed {
            return Err(TransportError::Closed);
        }
        let sink = self.sink;
        if sink.f.is_none() {
            return Err(TransportError::SendFailed(std::io::Error::other(
                "no datagram sink",
            )));
        }
        let mut out = |d: &[u8]| -> std::io::Result<()> {
            if let Some(f) = sink.f {
                f(sink.user, d.as_ptr(), d.len());
            }
            Ok(())
        };
        self.link
            .send(datagram, std::time::Instant::now(), &mut out)
            .map_err(TransportError::SendFailed)?;
        self.stats.sent_datagrams += 1;
        self.stats.bytes_sent += datagram.len() as u64;
        Ok(datagram.len())
    }

    fn poll_recv(&mut self, sink: &mut dyn FnMut(&[u8])) -> Result<usize, TransportError> {
        if self.closed {
            return Err(TransportError::Closed);
        }
        let mut n = 0;
        while let Some(d) = self.take_inbound() {
            sink(&d);
            n += 1;
        }
        Ok(n)
    }

    fn pending_send(&self) -> usize {
        self.link.pending_bytes()
    }

    fn reliable_capable(&self) -> bool {
        true
    }

    fn enable_reliable(&mut self) -> bool {
        self.link.enable();
        true
    }

    fn close(&mut self) -> Result<(), TransportError> {
        self.closed = true;
        Ok(())
    }

    fn is_reliable(&self) -> bool {
        false
    }

    fn stats(&self) -> &CarrierStats {
        &self.stats
    }
}

/// Whichever transport the session is wired to.
pub(crate) enum Transport {
    /// Not wired yet.
    None,
    /// Host-owned datagram path (Swift NWConnection / UDP).
    Datagram(Box<FfiDatagramCarrier>),
    /// Rust-owned relay TCP tunnel, or the host-bridged stream relay.
    Relay(Box<RelayTunnelCarrier>),
}

impl Transport {
    /// Send one datagram over whichever transport is wired (host datagram
    /// sink or the Rust-owned / bridged relay tunnel).
    pub(crate) fn send(&mut self, d: &[u8]) -> Result<usize, TransportError> {
        match self {
            Self::Datagram(c) => c.send(d),
            Self::Relay(c) => c.send(d),
            Self::None => Err(TransportError::Closed),
        }
    }
}
