//! The Carrier trait, error type, and statistics — the uniform datagram
//! surface every transport backend implements (spec §5).

use std::io;

/// Errors a [`Carrier`] can produce. Minimal by design: everything else
/// (timeouts, reordering, loss) is a *property* of the path, not an error —
/// the upper layers already tolerate it.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// Sending failed; the carrier may or may not still be usable
    /// (e.g. an ICMP port-unreachable surfacing on a UDP `send`).
    #[error("send failed: {0}")]
    SendFailed(#[source] io::Error),
    /// Receiving failed hard (not a would-block, which `poll_recv` swallows).
    #[error("recv failed: {0}")]
    RecvFailed(#[source] io::Error),
    /// A datagram exceeds the carrier's framing limit (TCP: > 0xFFFF bytes).
    #[error("frame too large ({0} bytes > limit)")]
    FrameTooLarge(usize),
    /// The carrier is closed; send/recv are no-ops returning this.
    #[error("carrier closed")]
    Closed,
    /// Any other I/O failure (bind, connect, accept, ...).
    #[error("io error: {0}")]
    Io(#[source] io::Error),
}

/// Cumulative counters for one carrier instance. Monotonic; `close()` freezes them.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CarrierStats {
    pub sent_datagrams: u64,
    pub recv_datagrams: u64,
    pub bytes_sent: u64,
    pub bytes_recv: u64,
}

impl CarrierStats {
    /// Account one outgoing datagram. Crate-internal: only carrier backends record.
    pub(crate) fn record_sent(&mut self, bytes: usize) {
        self.sent_datagrams += 1;
        self.bytes_sent += bytes as u64;
    }
    /// Account one delivered datagram.
    pub(crate) fn record_recv(&mut self, bytes: usize) {
        self.recv_datagrams += 1;
        self.bytes_recv += bytes as u64;
    }
}

/// The OS handle of a carrier's socket (`SOCKET` on Windows, a file
/// descriptor elsewhere), for readiness waits (`WSAEventSelect`, `poll`).
#[cfg(windows)]
pub type RawSock = std::os::windows::io::RawSocket;
/// The OS handle of a carrier's socket (`SOCKET` on Windows, a file
/// descriptor elsewhere), for readiness waits (`WSAEventSelect`, `poll`).
#[cfg(not(windows))]
pub type RawSock = i32;

/// A datagram path between two peers.
///
/// Implementations must be usable from one thread (the pump's), hence
/// `Send` but no `Sync` requirement. Datagrams are opaque byte blobs of at
/// most the carrier's framing limit (65 535 bytes everywhere in v0.3 — the
/// same bound the ovcore wire formats already enforce).
pub trait Carrier: Send {
    /// Send one datagram. Returns the number of bytes handed to the OS
    /// (for UDP this is all-or-nothing). Non-blocking. A stream carrier may
    /// queue part of it when the OS send buffer is full (see
    /// [`pending_send`](Self::pending_send)); order is always preserved.
    fn send(&mut self, datagram: &[u8]) -> Result<usize, TransportError>;

    /// Drain currently-available inbound datagrams into `sink`. Returns the
    /// number of datagrams delivered (0 = nothing pending right now).
    /// Must not block.
    fn poll_recv(&mut self, sink: &mut dyn FnMut(&[u8])) -> Result<usize, TransportError>;

    /// Close the carrier. Idempotent; afterwards send/poll_recv yield
    /// [`TransportError::Closed`].
    fn close(&mut self) -> Result<(), TransportError>;

    /// `true` if the path is lossless and ordered (TCP, in-memory);
    /// `false` for UDP, where the record layer's seq guard carries the load.
    fn is_reliable(&self) -> bool;

    /// Cumulative counters for this carrier.
    fn stats(&self) -> &CarrierStats;

    /// Outbound bytes accepted by [`send`](Self::send) but not yet handed
    /// to the OS (a carrier that queues instead of blocking when the send
    /// buffer is full). Schedulers use it to hold back bulk data while
    /// still letting urgent records through; the queue drains on later
    /// sends and polls. Default: `0` (sends complete synchronously).
    fn pending_send(&self) -> usize {
        0
    }

    /// The socket a caller may wait on for readability instead of polling
    /// on a timer. Only carriers whose socket stays non-blocking for life
    /// (and never toggles the mode back, which an OS readiness registration
    /// such as `WSAEventSelect` forbids) return `Some`; the default is `None`
    /// (callers fall back to short timed waits).
    fn raw_socket(&self) -> Option<RawSock> {
        None
    }

    /// Whether this carrier can switch to the Phase 4 reliable framing
    /// (`ovtransport::rudp`) — i.e. it is a lossy datagram path that
    /// understands it. Lossless carriers say `false` (nothing to fix).
    fn reliable_capable(&self) -> bool {
        false
    }

    /// The peer advertised that it receives the reliable framing (CAPS
    /// `CAP_RUDP`): switch our sending to it. Returns whether the carrier
    /// switched; the default (lossless carriers) is a no-op.
    fn enable_reliable(&mut self) -> bool {
        false
    }

    /// When the carrier next needs a `poll_recv`/`send` call for its own
    /// timers (retransmissions); loops bound their waits by it. `None` = no
    /// timer pending.
    fn next_deadline(&self) -> Option<std::time::Duration> {
        None
    }

    /// More sockets a readiness wait should watch besides
    /// [`raw_socket`](Self::raw_socket) (a [`crate::DirectCarrier`]'s
    /// direct-path UDP sockets). Default: none.
    fn extra_sockets(&self) -> Vec<RawSock> {
        Vec::new()
    }

    /// The direct-path upgrade layer, when this carrier is one
    /// ([`crate::DirectCarrier`]). Default: `None`.
    fn direct_mut(&mut self) -> Option<&mut crate::direct::DirectCarrier> {
        None
    }

    /// Shared-reference twin of [`direct_mut`](Self::direct_mut).
    fn direct_ref(&self) -> Option<&crate::direct::DirectCarrier> {
        None
    }
}
