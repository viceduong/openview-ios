//! TCP carrier — the fallback path for networks that block UDP. Each logical
//! datagram is framed `[2B u16-BE len][body]` so the upper layers keep plain
//! datagram semantics over a byte stream.
//!
//! The stream is non-blocking for life (a full send buffer queues
//! the rest in `tx`, flushed on later sends/polls, visible via
//! [`Carrier::pending_send`]), so the socket is exposed through
//! [`Carrier::raw_socket`] and session loops block on it instead of polling
//! every 2 ms.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};

use crate::carrier::{Carrier, CarrierStats, TransportError};

/// Maximum frame body: the u16 length prefix caps it, matching the 65 535-byte
/// datagram limit of the UDP carrier and ovcore's wire formats.
const MAX_FRAME: usize = 0xFFFF;

/// A carrier over one established TCP connection.
///
/// Framing state machine in `poll_recv`: TCP is a byte stream, so inbound
/// bytes accumulate in `rx` and are cut into frames as they complete. The
/// stream is read non-blocking and drained until `WouldBlock`; only complete
/// frames are delivered to the sink, partial frames stay buffered for the
/// next poll. `send` never fragments: a datagram is one frame, and a datagram
/// larger than 0xFFFF bytes is rejected up front rather than silently split.
pub struct TcpCarrier {
    stream: Option<TcpStream>,
    /// Inbound byte accumulator (partial frame prefix + body bytes).
    rx: Vec<u8>,
    /// Outbound framed bytes the OS has not accepted yet (`tx[tx_off..]`).
    tx: Vec<u8>,
    tx_off: usize,
    stats: CarrierStats,
}

impl TcpCarrier {
    /// Wrap an established stream (from `TcpListener::accept` or `TcpStream::connect`).
    pub fn from_stream(stream: TcpStream) -> Self {
        // Best-effort nodelay: datagrams here are handshake/record sized, and
        // Nagle would add up to 200 ms of latency on the request/response pump.
        let _ = stream.set_nodelay(true);
        // Non-blocking for life: sends that hit a full buffer queue instead
        // of blocking, and the socket can be registered for readiness waits.
        let _ = stream.set_nonblocking(true);
        Self {
            stream: Some(stream),
            rx: Vec::new(),
            tx: Vec::new(),
            tx_off: 0,
            stats: CarrierStats::default(),
        }
    }

    /// Push queued outbound bytes into the OS until it refuses (WouldBlock).
    fn flush_tx(&mut self) -> Result<(), TransportError> {
        let Some(stream) = self.stream.as_mut() else {
            return Err(TransportError::Closed);
        };
        while self.tx_off < self.tx.len() {
            match stream.write(&self.tx[self.tx_off..]) {
                Ok(0) => {
                    return Err(TransportError::SendFailed(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "tcp stream closed",
                    )))
                }
                Ok(n) => self.tx_off += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(TransportError::SendFailed(e)),
            }
        }
        if self.tx_off == self.tx.len() {
            self.tx.clear();
            self.tx_off = 0;
        } else if self.tx_off > 64 * 1024 {
            self.tx.drain(..self.tx_off);
            self.tx_off = 0;
        }
        Ok(())
    }

    /// Dial a peer and wrap the established connection.
    pub fn dial(addr: SocketAddr) -> Result<Self, TransportError> {
        let stream = TcpStream::connect(addr).map_err(TransportError::Io)?;
        Ok(Self::from_stream(stream))
    }

    /// Accept one inbound connection on `listener` and wrap it.
    pub fn accept(listener: &TcpListener) -> Result<Self, TransportError> {
        let (stream, _) = listener.accept().map_err(TransportError::Io)?;
        Ok(Self::from_stream(stream))
    }
}

impl Carrier for TcpCarrier {
    fn send(&mut self, datagram: &[u8]) -> Result<usize, TransportError> {
        if datagram.len() > MAX_FRAME {
            return Err(TransportError::FrameTooLarge(datagram.len()));
        }
        if self.stream.is_none() {
            return Err(TransportError::Closed);
        }
        // Queue the frame behind anything still pending (order preserved),
        // then hand as much as the OS takes.
        self.tx
            .extend_from_slice(&(datagram.len() as u16).to_be_bytes());
        self.tx.extend_from_slice(datagram);
        self.flush_tx()?;
        self.stats.record_sent(datagram.len());
        Ok(datagram.len())
    }

    fn poll_recv(&mut self, sink: &mut dyn FnMut(&[u8])) -> Result<usize, TransportError> {
        // Queued outbound bytes drain on every poll too (a send error
        // resurfaces on the next `send`; reading may still deliver frames).
        let _ = self.flush_tx();
        let mut stream = self.stream.take().ok_or(TransportError::Closed)?;
        let mut delivered = 0usize;
        let mut chunk = [0u8; 16 * 1024];
        // Step 1: read whatever is available into the accumulator.
        let read_result = loop {
            match stream.read(&mut chunk) {
                Ok(0) => {
                    // Peer closed the stream mid-frame: bytes we already hold
                    // may still complete frames, so stop reading, not erroring.
                    break Ok(false);
                }
                Ok(n) => {
                    self.rx.extend_from_slice(&chunk[..n]);
                    if n < chunk.len() {
                        // Short read: likely drained for now; keep going until
                        // WouldBlock to be sure, cheap since the next read tells us.
                        continue;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break Ok(true),
                Err(e) => break Err(e),
            }
        };
        // Step 2: cut complete frames out of the accumulator. A frame's full
        // length is 2 (prefix) + body; we only deliver when all of it is present.
        let mut off = 0usize;
        while off + 2 <= self.rx.len() {
            let len = u16::from_be_bytes([self.rx[off], self.rx[off + 1]]) as usize;
            let end = off + 2 + len;
            if end > self.rx.len() {
                break; // partial frame; wait for more bytes next poll
            }
            sink(&self.rx[off + 2..end]);
            self.stats.record_recv(len);
            delivered += 1;
            off = end;
        }
        if off > 0 {
            self.rx.drain(..off);
        }
        self.stream = Some(stream);
        read_result
            .map(|_| delivered)
            .map_err(TransportError::RecvFailed)
    }

    fn close(&mut self) -> Result<(), TransportError> {
        // Drop the stream: TCP FIN is sent by the OS. Buffered partial frames
        // are discarded — the peer's record-layer seq guard flags the gap.
        self.stream = None;
        self.rx.clear();
        self.tx.clear();
        self.tx_off = 0;
        Ok(())
    }

    fn pending_send(&self) -> usize {
        self.tx.len() - self.tx_off
    }

    fn raw_socket(&self) -> Option<crate::carrier::RawSock> {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawSocket;
            self.stream.as_ref().map(|s| s.as_raw_socket())
        }
        #[cfg(not(windows))]
        {
            use std::os::fd::AsRawFd;
            self.stream.as_ref().map(|s| s.as_raw_fd())
        }
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
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn tcp_loopback_bidirectional_frames() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let mut server = TcpCarrier::accept(&listener).unwrap();
            // Echo everything the client sends, then one extra frame.
            let mut out = Vec::new();
            // Poll until all three frames arrived: a single poll right after
            // accept can race the client's sends (flaky under load).
            let mut n = 0;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while n < 3 && std::time::Instant::now() < deadline {
                n += server.poll_recv(&mut |d| out.push(d.to_vec())).unwrap();
                if n < 3 {
                    thread::sleep(std::time::Duration::from_millis(2));
                }
            }
            (server, n, out)
        });
        // Give the accept thread a moment to start listening (bind already done,
        // so the connection is queued in the backlog even before accept() runs).
        thread::sleep(std::time::Duration::from_millis(50));
        let mut client = TcpCarrier::dial(addr).unwrap();
        client.send(b"frame-one").unwrap();
        client.send(b"").unwrap(); // empty datagram is legal
        client.send(b"frame-three").unwrap();

        let (mut server, n, out) = handle.join().unwrap();
        assert_eq!(n, 3);
        assert_eq!(out, vec![&b"frame-one"[..], b"", b"frame-three"]);

        // Server -> client direction.
        server.send(b"reply").unwrap();
        let mut got = Vec::new();
        let mut tries = 0;
        while got.is_empty() && tries < 50 {
            tries += 1;
            client.poll_recv(&mut |d| got.push(d.to_vec())).unwrap();
            if got.is_empty() {
                thread::sleep(std::time::Duration::from_millis(2));
            }
        }
        assert_eq!(got, vec![&b"reply"[..]]);
        assert!(client.is_reliable());
    }

    #[test]
    fn tcp_frame_too_large_rejected() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut client = TcpCarrier::dial(addr).unwrap();
        drop(listener); // we never exchange data; the dial already succeeded
        let big = vec![0u8; 0x1_0000];
        assert!(matches!(
            client.send(&big),
            Err(TransportError::FrameTooLarge(0x1_0000))
        ));
        // Stats untouched by the rejected send.
        assert_eq!(client.stats().sent_datagrams, 0);
    }
}
