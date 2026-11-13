//! Minimal RFC 6455 WebSocket codec shared by the relay server (`ovrelay`)
//! and the WebSocket relay client (feature `ws-client`).
//!
//! The relay protocol is a byte stream; over WebSocket the same bytes travel
//! inside binary messages, and any message boundary is allowed (both ends
//! reassemble a byte stream). Only what that needs is here: the handshake
//! accept key, frame encoding (masked for clients, unmasked for servers),
//! unmasking, and a buffering deframer for the client side. Pure std, no
//! I/O.

use std::io;

/// RFC 6455 §1.3 GUID appended to `Sec-WebSocket-Key`.
pub const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Continuation frame.
pub const OP_CONT: u8 = 0x0;
/// Text message (accepted as bytes; the relay never sends text).
pub const OP_TEXT: u8 = 0x1;
/// Binary message: carries relay-protocol bytes.
pub const OP_BINARY: u8 = 0x2;
/// Connection close.
pub const OP_CLOSE: u8 = 0x8;
/// Ping (answered with a PONG echoing the payload).
pub const OP_PING: u8 = 0x9;
/// Pong.
pub const OP_PONG: u8 = 0xA;

/// Control frames (PING/PONG/CLOSE) carry at most 125 bytes (RFC 6455 §5.5).
pub const MAX_CONTROL: usize = 125;

/// `Sec-WebSocket-Accept` = base64(SHA-1(key || GUID)).
pub fn accept_key(key: &str) -> String {
    let mut input = key.as_bytes().to_vec();
    input.extend_from_slice(WS_GUID.as_bytes());
    base64(&sha1(&input))
}

/// Encode one frame (FIN set) into `out`, which is cleared first (its
/// capacity is kept). `mask` = `Some` for client-to-server frames (RFC 6455
/// §5.1 requires them masked), `None` for server frames.
pub fn encode_into(out: &mut Vec<u8>, opcode: u8, payload: &[u8], mask: Option<[u8; 4]>) {
    out.clear();
    out.reserve(payload.len() + 14);
    out.push(0x80 | opcode);
    let mask_bit = if mask.is_some() { 0x80 } else { 0 };
    let n = payload.len();
    if n < 126 {
        out.push(mask_bit | n as u8);
    } else if let Ok(n16) = u16::try_from(n) {
        out.push(mask_bit | 126);
        out.extend_from_slice(&n16.to_be_bytes());
    } else {
        out.push(mask_bit | 127);
        out.extend_from_slice(&(n as u64).to_be_bytes());
    }
    let start = out.len();
    match mask {
        Some(m) => {
            out.extend_from_slice(&m);
            out.extend_from_slice(payload);
            unmask(&mut out[start + 4..], m, 0);
        }
        None => out.extend_from_slice(payload),
    }
}

/// [`encode_into`] into a fresh buffer.
pub fn encode(opcode: u8, payload: &[u8], mask: Option<[u8; 4]>) -> Vec<u8> {
    let mut out = Vec::new();
    encode_into(&mut out, opcode, payload, mask);
    out
}

/// XOR `buf` with the 4-byte WebSocket `mask`, where `buf[0]` is payload
/// byte number `offset` (so a payload can be (un)masked in pieces). Works a
/// u64 at a time; the tail byte-wise. Masking and unmasking are the same
/// operation.
pub fn unmask(buf: &mut [u8], mask: [u8; 4], offset: usize) {
    let r = offset % 4;
    let m = [
        mask[r],
        mask[(r + 1) % 4],
        mask[(r + 2) % 4],
        mask[(r + 3) % 4],
    ];
    let word = u64::from_ne_bytes([m[0], m[1], m[2], m[3], m[0], m[1], m[2], m[3]]);
    let (chunks, tail) = buf.as_chunks_mut::<8>();
    for c in chunks {
        *c = (u64::from_ne_bytes(*c) ^ word).to_ne_bytes();
    }
    // Chunks are multiples of 8 (and so of 4): the tail starts at phase 0.
    for (i, b) in tail.iter_mut().enumerate() {
        *b ^= m[i % 4];
    }
}

/// Incremental deframer for frames received by a client: feed raw bytes
/// with [`push`](Self::push), then take whole frames with
/// [`next_frame`](Self::next_frame). Server frames are normally unmasked; a
/// masked one is unmasked rather than rejected.
#[derive(Debug)]
pub struct Deframer {
    buf: Vec<u8>,
    /// Start of the unparsed bytes in `buf`.
    off: usize,
    max_payload: usize,
}

impl Deframer {
    /// A deframer that rejects data frames larger than `max_payload` bytes.
    pub fn new(max_payload: usize) -> Self {
        Self {
            buf: Vec::new(),
            off: 0,
            max_payload,
        }
    }

    /// Append received bytes.
    pub fn push(&mut self, bytes: &[u8]) {
        if self.off > 0 && self.off == self.buf.len() {
            self.buf.clear();
            self.off = 0;
        } else if self.off > 64 * 1024 {
            self.buf.drain(..self.off);
            self.off = 0;
        }
        self.buf.extend_from_slice(bytes);
    }

    /// Bytes buffered but not yet returned as frames.
    pub fn buffered(&self) -> usize {
        self.buf.len() - self.off
    }

    /// The next complete frame: appends its (unmasked) payload to `payload`
    /// and returns its opcode. `Ok(None)` = need more bytes. Oversized,
    /// fragmented-control, reserved-bit and unknown-opcode frames are
    /// `InvalidData` errors (the connection must then be dropped).
    pub fn next_frame(&mut self, payload: &mut Vec<u8>) -> io::Result<Option<u8>> {
        let b = &self.buf[self.off..];
        if b.len() < 2 {
            return Ok(None);
        }
        let invalid = |why: &str| io::Error::new(io::ErrorKind::InvalidData, why.to_owned());
        let (fin, rsv, opcode) = (b[0] & 0x80 != 0, b[0] & 0x70, b[0] & 0x0F);
        let masked = b[1] & 0x80 != 0;
        let (len, mut head) = match b[1] & 0x7F {
            126 if b.len() >= 4 => (u64::from(u16::from_be_bytes([b[2], b[3]])), 4),
            127 if b.len() >= 10 => {
                let mut l = [0u8; 8];
                l.copy_from_slice(&b[2..10]);
                (u64::from_be_bytes(l), 10)
            }
            126 | 127 => return Ok(None),
            n => (u64::from(n), 2),
        };
        if rsv != 0 {
            return Err(invalid("WebSocket frame with reserved bits set"));
        }
        let control = opcode & 0x8 != 0;
        match opcode {
            OP_CONT | OP_TEXT | OP_BINARY | OP_CLOSE | OP_PING | OP_PONG => {}
            _ => return Err(invalid("unknown WebSocket opcode")),
        }
        if control && (len > MAX_CONTROL as u64 || !fin) {
            return Err(invalid("bad WebSocket control frame"));
        }
        if len > self.max_payload as u64 {
            return Err(invalid("WebSocket frame too large"));
        }
        let len = len as usize;
        let mask = if masked {
            if b.len() < head + 4 {
                return Ok(None);
            }
            let m = [b[head], b[head + 1], b[head + 2], b[head + 3]];
            head += 4;
            Some(m)
        } else {
            None
        };
        if b.len() < head + len {
            return Ok(None);
        }
        let start = payload.len();
        payload.extend_from_slice(&b[head..head + len]);
        if let Some(m) = mask {
            unmask(&mut payload[start..], m, 0);
        }
        self.off += head + len;
        Ok(Some(opcode))
    }
}

/// SHA-1 (FIPS 180-1). Used only for the handshake accept key.
pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in msg.as_chunks::<64>().0 {
        let mut w = [0u32; 80];
        for (i, word) in chunk.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*word);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        for (hv, v) in h.iter_mut().zip([a, b, c, d, e]) {
            *hv = hv.wrapping_add(v);
        }
    }
    let mut out = [0u8; 20];
    for (o, v) in out.as_chunks_mut::<4>().0.iter_mut().zip(h) {
        *o = v.to_be_bytes();
    }
    out
}

/// Standard base64 with padding.
pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc6455_accept_key_example() {
        // RFC 6455 §1.3.
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn sha1_known_vectors() {
        assert_eq!(base64(&sha1(b"")), "2jmj7l5rSw0yVb/vlWAYkK/YBwk=");
        assert_eq!(base64(&sha1(b"abc")), "qZk+NkcGgWq6PiVxeFDCbJzQ2J0=");
    }

    #[test]
    fn frame_length_encodings() {
        assert_eq!(&encode(OP_BINARY, &[1, 2], None)[..2], &[0x82, 2]);
        assert_eq!(
            &encode(OP_BINARY, &[0; 300], None)[..4],
            &[0x82, 126, 1, 44]
        );
        assert_eq!(
            &encode(OP_BINARY, &vec![0; 70_000], None)[..2],
            &[0x82, 127]
        );
        // Masked: the mask bit, then the key, then the masked payload.
        let f = encode(OP_BINARY, b"abcd", Some([1, 2, 3, 4]));
        assert_eq!(
            f,
            [
                0x82,
                0x84,
                1,
                2,
                3,
                4,
                b'a' ^ 1,
                b'b' ^ 2,
                b'c' ^ 3,
                b'd' ^ 4
            ]
        );
    }

    #[test]
    fn unmask_matches_bytewise_at_any_offset_and_split() {
        let mask = [0x12, 0x34, 0x56, 0x78];
        let plain: Vec<u8> = (0..1000u32).map(|i| (i * 7 % 256) as u8).collect();
        let masked: Vec<u8> = plain
            .iter()
            .enumerate()
            .map(|(i, b)| b ^ mask[i % 4])
            .collect();
        for split in [0, 1, 3, 5, 8, 13, 999, 1000] {
            let mut buf = masked.clone();
            let (a, b) = buf.split_at_mut(split);
            unmask(a, mask, 0);
            unmask(b, mask, split);
            assert_eq!(buf, plain, "split at {split}");
        }
    }

    #[test]
    fn deframer_round_trips_any_split() {
        let mut wire = encode(OP_BINARY, b"hello", None);
        wire.extend(encode(OP_PING, b"p", None));
        wire.extend(encode(OP_BINARY, &[9u8; 300], Some([5, 6, 7, 8])));
        wire.extend(encode(OP_BINARY, &vec![3u8; 70_000], None));
        wire.extend(encode(OP_CLOSE, &1000u16.to_be_bytes(), None));
        for step in [1usize, 2, 3, 7, 100, 4096, wire.len()] {
            let mut d = Deframer::new(1 << 20);
            let mut frames = Vec::new();
            for piece in wire.chunks(step) {
                d.push(piece);
                loop {
                    let mut p = Vec::new();
                    match d.next_frame(&mut p).unwrap() {
                        Some(op) => frames.push((op, p)),
                        None => break,
                    }
                }
            }
            assert_eq!(d.buffered(), 0);
            let ops: Vec<u8> = frames.iter().map(|f| f.0).collect();
            assert_eq!(
                ops,
                [OP_BINARY, OP_PING, OP_BINARY, OP_BINARY, OP_CLOSE],
                "step {step}"
            );
            assert_eq!(frames[0].1, b"hello");
            assert_eq!(frames[2].1, [9u8; 300], "masked frame unmasked");
            assert_eq!(frames[3].1.len(), 70_000);
        }
    }

    #[test]
    fn deframer_rejects_bad_frames() {
        let check = |wire: &[u8]| {
            let mut d = Deframer::new(1024);
            d.push(wire);
            d.next_frame(&mut Vec::new())
        };
        assert!(
            check(&encode(OP_BINARY, &[0; 1025], None)).is_err(),
            "too large"
        );
        assert!(check(&[0x83, 0]).is_err(), "unknown opcode");
        assert!(check(&[0xC2, 0]).is_err(), "reserved bit");
        assert!(check(&[0x09, 0]).is_err(), "fragmented control");
        assert!(check(&[0x89, 126, 0, 200]).is_err(), "oversized control");
        assert!(
            check(&[0x82, 127, 0xFF]).unwrap().is_none(),
            "partial length"
        );
    }
}
