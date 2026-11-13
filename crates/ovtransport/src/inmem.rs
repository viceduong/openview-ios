//! In-memory carrier pair — an in-process datagram pipe for tests and for
//! embedding OpenView inside a single process (e.g. the desktop app talking
//! to a local test host).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::carrier::{Carrier, CarrierStats, TransportError};

/// Shared spool between the two halves of a pair. A mutex-protected queue
/// (rather than `std::sync::mpsc`) because the pair is often pumped from the
/// *same* thread in tests: `a.send(x)` then `b.poll_recv()` must observe `x`
/// with no thread hop — a shared queue guarantees that ordering. It also lets
/// `close()` drain pending datagrams deterministically.
#[derive(Default)]
struct Spool {
    queue: VecDeque<Vec<u8>>,
    closed: bool,
}

fn lock(m: &Mutex<Spool>) -> std::sync::MutexGuard<'_, Spool> {
    // A poisoned lock here can only mean a test panicked while holding it;
    // unwrapping the poison and continuing is more useful than propagating.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// One half of a connected in-process carrier pair ([`InMemoryCarrier::pair`]).
/// Cheap to clone (the spools are shared); each half keeps its own stats.
#[derive(Clone)]
pub struct InMemoryCarrier {
    /// Our inbound queue (the peer appends to it when it sends).
    spool: Arc<Mutex<Spool>>,
    /// The peer's spool — `send` enqueues here so the peer's next `poll_recv`
    /// sees our datagram even when both halves live on the same thread.
    peer_spool: Arc<Mutex<Spool>>,
    stats: CarrierStats,
}

impl InMemoryCarrier {
    /// Create a connected pair. Datagrams sent by one half are delivered to
    /// the other half's next `poll_recv`, in order, losslessly.
    pub fn pair() -> (Self, Self) {
        let a = Arc::new(Mutex::new(Spool::default()));
        let b = Arc::new(Mutex::new(Spool::default()));
        (
            Self {
                spool: a.clone(),
                peer_spool: b.clone(),
                stats: CarrierStats::default(),
            },
            Self {
                spool: b,
                peer_spool: a,
                stats: CarrierStats::default(),
            },
        )
    }
}

impl Carrier for InMemoryCarrier {
    fn send(&mut self, datagram: &[u8]) -> Result<usize, TransportError> {
        let mut peer = lock(&self.peer_spool);
        if peer.closed {
            return Err(TransportError::Closed);
        }
        peer.queue.push_back(datagram.to_vec());
        drop(peer);
        self.stats.record_sent(datagram.len());
        Ok(datagram.len())
    }

    fn poll_recv(&mut self, sink: &mut dyn FnMut(&[u8])) -> Result<usize, TransportError> {
        let mut spool = lock(&self.spool);
        if spool.closed {
            return Err(TransportError::Closed);
        }
        let mut delivered = 0usize;
        while let Some(d) = spool.queue.pop_front() {
            sink(&d);
            self.stats.record_recv(d.len());
            delivered += 1;
        }
        Ok(delivered)
    }

    fn close(&mut self) -> Result<(), TransportError> {
        // Mark our side closed and drop any undelivered datagrams — a closed
        // carrier is a torn-down path, not a graceful drain.
        let mut spool = lock(&self.spool);
        spool.closed = true;
        spool.queue.clear();
        Ok(())
    }

    fn is_reliable(&self) -> bool {
        true
    }

    fn stats(&self) -> &CarrierStats {
        &self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::carrier::Carrier;

    #[test]
    fn inmem_roundtrip_same_thread() {
        let (mut a, mut b) = InMemoryCarrier::pair();
        assert!(a.is_reliable() && b.is_reliable());
        a.send(b"hello").unwrap();
        a.send(b"").unwrap(); // empty datagram is legal
                              // Same-thread pump: b sees a's datagrams with no thread hop.
        let mut got = Vec::new();
        let n = b.poll_recv(&mut |d| got.push(d.to_vec())).unwrap();
        assert_eq!(n, 2);
        assert_eq!(got, vec![&b"hello"[..], &b""[..]]);
        assert_eq!(b.stats().recv_datagrams, 2);
        assert_eq!(b.stats().bytes_recv, 5);
        assert_eq!(a.stats().sent_datagrams, 2);
        assert_eq!(a.stats().bytes_sent, 5);
    }

    #[test]
    fn inmem_empty_poll_returns_zero() {
        let (mut a, mut _b) = InMemoryCarrier::pair();
        let mut calls = 0;
        assert_eq!(a.poll_recv(&mut |_| calls += 1).unwrap(), 0);
        assert_eq!(calls, 0);
    }

    #[test]
    fn inmem_close_semantics() {
        let (mut a, mut b) = InMemoryCarrier::pair();
        a.send(b"in flight").unwrap();
        // Closing the *receiving* side drops its queued datagrams.
        b.close().unwrap();
        b.close().unwrap(); // idempotent
        let mut got = Vec::new();
        assert!(matches!(
            b.poll_recv(&mut |d| got.push(d.to_vec())),
            Err(TransportError::Closed)
        ));
        assert!(got.is_empty());
        // Sending *into* a closed peer fails.
        assert!(matches!(a.send(b"late"), Err(TransportError::Closed)));
        assert_eq!(a.stats().sent_datagrams, 1, "failed send is not counted");
        // Closing our own side still works and freezes stats.
        a.close().unwrap();
        let mut sink = |_: &[u8]| {};
        assert!(matches!(
            a.poll_recv(&mut sink),
            Err(TransportError::Closed)
        ));
        assert_eq!(a.stats().sent_datagrams, 1);
    }
}
