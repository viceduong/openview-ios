//! Relay tunnel framing — the tiny wire format every party on a relay
//! connection speaks (`docs/WIRE_PROTOCOL.md`, "Tunnel frames"). The relay is a pure byte
//! pipe: it forwards frames opaquely; only CONNECT/ATTACH/REJECT control
//! lines (a separate, frame-less prefix on the TCP stream) are interpreted.
//!
//! ```text
//! frame := kind(1B) len(2B BE) payload(len B)     len <= MAX_FRAME
//! kind  := 0x00 DATA | 0x01 PING | 0x02 PONG | 0x03 DISCONNECT
//!       |  0x04 PROBE | 0x05 PUNCH_ACK                (hole punching)
//! ```
//!
//! PING/PONG are the tunnel's liveness probe: a carrier that receives a PING
//! answers PONG immediately (see `relay::RelayTunnelCarrier`), and the
//! initiator can time the round trip via `last_pong()`.

/// DATA: opaque payload bytes, delivered to the peer carrier as a datagram.
pub const DATA: u8 = 0x00;
/// PING: liveness probe; answered by a PONG with the same nonce payload.
pub const PING: u8 = 0x01;
/// PONG: the reply to a PING; carries the PING's nonce back.
pub const PONG: u8 = 0x02;
/// DISCONNECT: graceful tunnel teardown (peer closed or relay tore it down).
pub const DISCONNECT: u8 = 0x03;
/// PROBE (hole punching): payload = 2B u16-BE — the sender's local UDP
/// P2P port the peer may punch to. Sent over the relay tunnel; the relay
/// forwards it opaquely like any other frame.
pub const PROBE: u8 = 0x04;
/// PUNCH_ACK (hole punching): payload = 2B u16-BE — echoes the peer's
/// local UDP port, acknowledging that a punch datagram arrived on it.
pub const PUNCH_ACK: u8 = 0x05;

/// Maximum frame payload. A tunnel DATA frame wraps one opaque carrier
/// datagram — at worst a full AEAD record of up to 65 535 bytes — and the
/// frame itself is a single TCP-carrier datagram `[2B len][body]` whose body
/// is capped at 0xFFFF. So the largest payload that still fits in one TCP
/// frame is 0xFFFF - 3 (the 3-byte tunnel header): 65 532. That comfortably
/// holds every record the app layer produces (a 16 384-byte screen chunk is
/// 16 412 bytes framed) and keeps a hostile peer from making us buffer
/// unbounded data.
pub const MAX_FRAME: usize = 0xFFFF - 3;

/// One decoded frame pulled off the stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedFrame {
    /// One of [`DATA`], [`PING`], [`PONG`], [`DISCONNECT`].
    pub kind: u8,
    /// Payload bytes (may be empty).
    pub payload: Vec<u8>,
}

/// Errors surfaced by the framing layer. `thiserror`, matching the crate style.
#[derive(Debug, Clone, thiserror::Error)]
pub enum FrameError {
    /// A frame declared a payload longer than [`MAX_FRAME`].
    #[error("frame payload too large ({0} > {1})")]
    TooLarge(usize, usize),
    /// An unknown frame kind byte.
    #[error("unknown frame kind 0x{0:02x}")]
    UnknownKind(u8),
}

/// Incremental frame reader for a byte stream. Feed raw TCP bytes in any
/// chunking via [`feed`](Self::feed); complete frames pop out of
/// [`next`](Self::next). The first bad frame is sticky: `next` keeps
/// returning the error until the reader is dropped — a desynchronized or
/// hostile stream is not recoverable.
///
/// Consumed frames only advance a read offset; the buffer is compacted once
/// per [`feed`](Self::feed) (not once per frame), so decoding N frames out of
/// one large feed is linear rather than quadratic.
#[derive(Debug, Default)]
pub struct FrameReader {
    buf: Vec<u8>,
    /// Start of the unconsumed bytes in `buf`.
    off: usize,
    error: Option<FrameError>,
}

impl FrameReader {
    /// Append raw stream bytes. Never decodes; call `next` afterwards.
    pub fn feed(&mut self, bytes: &[u8]) {
        if self.off > 0 {
            if self.off >= self.buf.len() {
                self.buf.clear();
            } else {
                self.buf.drain(..self.off);
            }
            self.off = 0;
        }
        self.buf.extend_from_slice(bytes);
    }

    /// Pop the next complete frame without copying its payload: returns
    /// `(kind, payload)` borrowing the reader's buffer (valid until the next
    /// call). Same error/latching semantics as [`next`](Self::next).
    pub fn next_ref(&mut self) -> Result<Option<(u8, &[u8])>, FrameError> {
        match self.peek_header()? {
            None => Ok(None),
            Some((kind, len)) => {
                let start = self.off + 3;
                self.off = start + len;
                Ok(Some((kind, &self.buf[start..start + len])))
            }
        }
    }

    /// Validate the header at the read offset; `Some((kind, len))` when a
    /// complete frame is buffered.
    fn peek_header(&mut self) -> Result<Option<(u8, usize)>, FrameError> {
        if let Some(e) = &self.error {
            return Err(e.clone());
        }
        let avail = &self.buf[self.off..];
        if avail.len() < 3 {
            return Ok(None);
        }
        let kind = avail[0];
        let len = u16::from_be_bytes([avail[1], avail[2]]) as usize;
        if kind > PUNCH_ACK {
            let e = FrameError::UnknownKind(kind);
            self.error = Some(e.clone());
            return Err(e);
        }
        if len > MAX_FRAME {
            let e = FrameError::TooLarge(len, MAX_FRAME);
            self.error = Some(e.clone());
            return Err(e);
        }
        if avail.len() < 3 + len {
            return Ok(None);
        }
        Ok(Some((kind, len)))
    }

    /// Pop the next complete frame, or `None` if more bytes are needed.
    /// On a protocol violation the error is latched and returned on every
    /// subsequent call.
    #[allow(clippy::should_implement_trait)] // not an Iterator: fallible, non-lifetimed
    pub fn next(&mut self) -> Result<Option<DecodedFrame>, FrameError> {
        Ok(self.next_ref()?.map(|(kind, payload)| DecodedFrame {
            kind,
            payload: payload.to_vec(),
        }))
    }

    /// Bytes buffered but not yet forming a complete frame.
    pub fn pending(&self) -> usize {
        self.buf.len() - self.off
    }
}

/// Encode one frame: `[1B kind][2B u16-BE len][payload]`.
///
/// Returns `Err(payload.len())` if the payload exceeds [`MAX_FRAME`] —
/// callers surface that as their own error type rather than silently
/// fragmenting (same policy as the TCP carrier).
pub fn encode_frame(kind: u8, payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    if kind > PUNCH_ACK {
        return Err(FrameError::UnknownKind(kind));
    }
    if payload.len() > MAX_FRAME {
        return Err(FrameError::TooLarge(payload.len(), MAX_FRAME));
    }
    let mut out = Vec::with_capacity(payload.len() + 3);
    out.push(kind);
    out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_single_frame() {
        let wire = encode_frame(DATA, b"hello").unwrap();
        assert_eq!(wire, [0x00, 0, 5, b'h', b'e', b'l', b'l', b'o']);
        let mut r = FrameReader::default();
        r.feed(&wire);
        let f = r.next().unwrap().unwrap();
        assert_eq!(f.kind, DATA);
        assert_eq!(f.payload, b"hello");
        assert!(r.next().unwrap().is_none());
    }

    #[test]
    fn split_feeds_assemble() {
        let wire = encode_frame(PING, &1234u64.to_be_bytes()).unwrap();
        let mut r = FrameReader::default();
        // Byte-at-a-time chunking: worst case any reader must tolerate.
        for b in &wire {
            r.feed(std::slice::from_ref(b));
        }
        let f = r.next().unwrap().unwrap();
        assert_eq!(f.kind, PING);
        assert_eq!(f.payload, 1234u64.to_be_bytes());
    }

    #[test]
    fn multiple_frames_in_one_feed() {
        let mut wire = encode_frame(DATA, b"a").unwrap();
        wire.extend(encode_frame(PONG, b"bb").unwrap());
        wire.extend(encode_frame(DISCONNECT, b"").unwrap());
        let mut r = FrameReader::default();
        r.feed(&wire);
        let kinds: Vec<(u8, Vec<u8>)> = (0..3)
            .map(|_| r.next().unwrap().unwrap())
            .map(|f| (f.kind, f.payload))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (DATA, b"a".to_vec()),
                (PONG, b"bb".to_vec()),
                (DISCONNECT, vec![])
            ]
        );
    }

    #[test]
    fn offset_reader_compacts_across_partial_feeds() {
        // Many frames + a trailing partial in one feed, then the rest of the
        // partial: the read offset must carry over the compaction correctly.
        let mut wire = Vec::new();
        for i in 0..1000u16 {
            wire.extend(encode_frame(DATA, &i.to_be_bytes()).unwrap());
        }
        let tail = encode_frame(PONG, b"tail-payload").unwrap();
        wire.extend_from_slice(&tail[..5]);
        let mut r = FrameReader::default();
        r.feed(&wire);
        for i in 0..1000u16 {
            let (k, p) = r.next_ref().unwrap().unwrap();
            assert_eq!((k, p), (DATA, &i.to_be_bytes()[..]));
        }
        assert!(r.next().unwrap().is_none());
        assert_eq!(r.pending(), 5);
        r.feed(&tail[5..]);
        let f = r.next().unwrap().unwrap();
        assert_eq!((f.kind, f.payload.as_slice()), (PONG, &b"tail-payload"[..]));
        assert_eq!(r.pending(), 0);
    }

    #[test]
    fn max_size_frame_ok() {
        let payload = vec![0xAB; MAX_FRAME];
        let wire = encode_frame(DATA, &payload).unwrap();
        assert_eq!(wire.len(), MAX_FRAME + 3);
        let mut r = FrameReader::default();
        r.feed(&wire);
        let f = r.next().unwrap().unwrap();
        assert_eq!(f.payload.len(), MAX_FRAME);
    }

    #[test]
    fn oversized_frame_rejected() {
        let mut wire = vec![DATA];
        wire.extend_from_slice(&(MAX_FRAME as u16 + 1).to_be_bytes());
        let mut r = FrameReader::default();
        r.feed(&wire);
        assert!(matches!(r.next(), Err(FrameError::TooLarge(_, MAX_FRAME))));
        // Sticky: the reader stays poisoned.
        assert!(matches!(r.next(), Err(FrameError::TooLarge(_, _))));
    }

    #[test]
    fn unknown_kind_rejected() {
        let mut r = FrameReader::default();
        r.feed(&[0x7F, 0, 0]);
        assert!(matches!(r.next(), Err(FrameError::UnknownKind(0x7F))));
        assert!(matches!(r.next(), Err(FrameError::UnknownKind(0x7F))));
    }

    #[test]
    fn encode_rejects_bad_kind_and_oversize() {
        // PROBE / PUNCH_ACK are now valid; the first invalid kind is 0x06.
        assert!(encode_frame(PROBE, &8080u16.to_be_bytes()).is_ok());
        assert!(encode_frame(PUNCH_ACK, &8080u16.to_be_bytes()).is_ok());
        assert!(encode_frame(0x06, b"").is_err());
        assert!(encode_frame(0x42, b"").is_err());
        assert!(encode_frame(DATA, &vec![0; MAX_FRAME + 1]).is_err());
    }
}
