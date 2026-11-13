//! Minimal RFC 6455 WebSocket server side for `ovrelay` (std only).
//!
//! The relay protocol is a byte stream; over WebSocket the same bytes travel
//! inside binary messages (any message boundary is allowed — the receiver
//! reassembles a byte stream). This lets clients reach the relay through an
//! HTTP-only front (a Cloudflare tunnel) at `wss://host/relay`.
//!
//! Only what the relay needs: the upgrade handshake, unmasking client frames
//! (RFC 6455 §5.1: an unmasked client frame is a protocol error), answering
//! PING, treating CLOSE as EOF, and framing outbound bytes as unmasked binary
//! messages.

use std::io::{self, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// RFC 6455 §1.3 GUID appended to `Sec-WebSocket-Key`.
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
/// Request head cap (a browser/proxy upgrade is well under 4 KiB).
const MAX_HEAD: usize = 16 * 1024;
/// Per-frame payload cap. Clients send relay-protocol chunks (a tunnel frame
/// is at most 64 KiB), so anything larger is abuse; 128 KiB leaves headroom
/// for a client that coalesces a couple of frames per message.
pub(crate) const MAX_FRAME: u64 = 128 * 1024;
/// Control frames (PING/PONG/CLOSE) carry at most 125 bytes (RFC 6455 §5.5).
const MAX_CONTROL: u64 = 125;
/// Buffered read size for the deframer (headers are tiny reads otherwise).
const READ_BUF: usize = 64 * 1024;

const OP_CONT: u8 = 0x0;
const OP_TEXT: u8 = 0x1;
pub(crate) const OP_BINARY: u8 = 0x2;
const OP_CLOSE: u8 = 0x8;
pub(crate) const OP_PING: u8 = 0x9;
const OP_PONG: u8 = 0xA;

/// Outcome of reading an HTTP request that started with `GET`.
pub(crate) enum Upgrade {
    /// Handshake answered (101); the stream now speaks WebSocket.
    Accepted,
    /// A plain HTTP request (health check, browser): answered and closed.
    Answered,
}

/// Read the HTTP request head and answer it. A valid WebSocket upgrade gets
/// `101 Switching Protocols`; anything else a short plain-text response.
/// The head must arrive before `deadline`.
pub(crate) fn handshake(stream: &mut TcpStream, deadline: Instant) -> io::Result<Upgrade> {
    let head = read_head(stream, deadline)?;
    let text = String::from_utf8_lossy(&head);
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut key = None;
    let mut upgrade_ws = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let (name, value) = (name.trim(), value.trim());
        if name.eq_ignore_ascii_case("sec-websocket-key") {
            key = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("upgrade") {
            upgrade_ws = value.eq_ignore_ascii_case("websocket");
        }
    }
    match (upgrade_ws, key) {
        (true, Some(key)) => {
            let resp = format!(
                "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
                accept_key(&key)
            );
            stream.write_all(resp.as_bytes())?;
            Ok(Upgrade::Accepted)
        }
        _ => {
            let healthy =
                request_line.starts_with("GET / ") || request_line.starts_with("GET /health ");
            let (status, body) = if healthy {
                ("200 OK", "ovrelay\n")
            } else {
                (
                    "426 Upgrade Required",
                    "ovrelay: WebSocket upgrade required\n",
                )
            };
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(resp.as_bytes())?;
            Ok(Upgrade::Answered)
        }
    }
}

/// Read up to and including the blank line ending the request head. Reads
/// one byte at a time so no WebSocket bytes after the head are consumed.
fn read_head(stream: &mut impl Read, deadline: Instant) -> io::Result<Vec<u8>> {
    let mut head = Vec::with_capacity(512);
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= MAX_HEAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "HTTP head too large",
            ));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "HTTP head not complete before the deadline",
            ));
        }
        match stream.read(&mut byte)? {
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            _ => head.push(byte[0]),
        }
    }
    Ok(head)
}

/// `Sec-WebSocket-Accept` = base64(SHA-1(key || GUID)).
pub(crate) fn accept_key(key: &str) -> String {
    let mut input = key.as_bytes().to_vec();
    input.extend_from_slice(WS_GUID.as_bytes());
    base64(&sha1(&input))
}

/// One unmasked server-to-client frame (FIN set).
pub(crate) fn encode(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    encode_into(&mut out, opcode, payload);
    out
}

/// [`encode`] into a reusable buffer (cleared first; capacity kept).
pub(crate) fn encode_into(out: &mut Vec<u8>, opcode: u8, payload: &[u8]) {
    out.clear();
    out.reserve(payload.len() + 10);
    out.push(0x80 | opcode);
    let n = payload.len();
    if n < 126 {
        out.push(n as u8);
    } else if n <= u16::MAX as usize {
        out.push(126);
        out.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        out.push(127);
        out.extend_from_slice(&(n as u64).to_be_bytes());
    }
    out.extend_from_slice(payload);
}

/// A normal-closure CLOSE frame (status 1000).
pub(crate) fn close_frame() -> Vec<u8> {
    encode(OP_CLOSE, &1000u16.to_be_bytes())
}

/// Reads the relay byte stream out of WebSocket messages. PINGs are answered
/// on `writer` (under the slot's write lock, so a PONG never interleaves with
/// a relayed frame); CLOSE reads as EOF.
///
/// Data-frame payloads are streamed straight into the caller's buffer and
/// unmasked in place (word-wise), through one buffered socket reader: no
/// per-message allocation and no per-header syscalls.
pub(crate) struct WsReader<R: Read = TcpStream, W: Write = TcpStream> {
    stream: BufReader<R>,
    writer: W,
    write_lock: Arc<Mutex<Vec<u8>>>,
    /// Unread payload bytes of the current data frame.
    remaining: u64,
    /// The current data frame's mask (None = unmasked).
    mask: Option<[u8; 4]>,
    /// Payload bytes of the current frame already delivered (mask phase).
    offset: usize,
    closed: bool,
    /// Diagnostics: when the client last answered one of our PINGs, and
    /// when the reader started (session-end log: a dead upstream shows as
    /// PONGs stopping long before the close).
    last_pong: Option<std::time::Instant>,
    started: std::time::Instant,
}

impl<R: Read, W: Write> WsReader<R, W> {
    pub(crate) fn new(stream: R, writer: W, write_lock: Arc<Mutex<Vec<u8>>>) -> Self {
        Self {
            stream: BufReader::with_capacity(READ_BUF, stream),
            writer,
            write_lock,
            remaining: 0,
            mask: None,
            offset: 0,
            closed: false,
            last_pong: None,
            started: std::time::Instant::now(),
        }
    }

    fn pong_note(&self) -> String {
        match self.last_pong {
            Some(t) => format!("last PONG {:.1} s ago", t.elapsed().as_secs_f64()),
            None => format!("no PONG in {:.1} s", self.started.elapsed().as_secs_f64()),
        }
    }

    /// Read the next frame header. A data frame becomes the current frame
    /// (its payload is then streamed by `read`); control frames are handled
    /// here. Returns false on CLOSE.
    fn next_frame(&mut self) -> io::Result<bool> {
        let mut h = [0u8; 2];
        self.stream.read_exact(&mut h)?;
        let opcode = h[0] & 0x0F;
        let masked = h[1] & 0x80 != 0;
        let mut len = u64::from(h[1] & 0x7F);
        if len == 126 {
            let mut b = [0u8; 2];
            self.stream.read_exact(&mut b)?;
            len = u64::from(u16::from_be_bytes(b));
        } else if len == 127 {
            let mut b = [0u8; 8];
            self.stream.read_exact(&mut b)?;
            len = u64::from_be_bytes(b);
        }
        let is_control = opcode & 0x8 != 0;
        if len > MAX_FRAME || (is_control && len > MAX_CONTROL) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "WebSocket frame too large",
            ));
        }
        if !masked {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unmasked WebSocket client frame",
            ));
        }
        let mask = if masked {
            let mut m = [0u8; 4];
            self.stream.read_exact(&mut m)?;
            Some(m)
        } else {
            None
        };
        match opcode {
            OP_BINARY | OP_TEXT | OP_CONT => {
                self.remaining = len;
                self.mask = mask;
                self.offset = 0;
            }
            OP_PING | OP_PONG | OP_CLOSE => {
                let mut payload = [0u8; MAX_CONTROL as usize];
                let payload = &mut payload[..len as usize];
                self.stream.read_exact(payload)?;
                if let Some(m) = mask {
                    unmask(payload, m, 0);
                }
                match opcode {
                    OP_PING => {
                        let mut scratch = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
                        encode_into(&mut scratch, OP_PONG, payload);
                        self.writer.write_all(&scratch)?;
                    }
                    OP_CLOSE => {
                        // Who closed, and why: the status code tells a client
                        // app close (1000/1001) from a proxy's (1006 is never
                        // sent; a FIN without CLOSE is logged below).
                        let code = (payload.len() >= 2)
                            .then(|| u16::from_be_bytes([payload[0], payload[1]]));
                        let reason = String::from_utf8_lossy(payload.get(2..).unwrap_or_default());
                        tracing::debug!(
                            "ws client sent CLOSE (code {}, reason {reason:?}); {}",
                            code.map_or_else(|| "none".to_owned(), |c| c.to_string()),
                            self.pong_note()
                        );
                        return Ok(false);
                    }
                    _ => self.last_pong = Some(std::time::Instant::now()), // PONG
                }
            }
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown WebSocket opcode 0x{other:x}"),
                ))
            }
        }
        Ok(true)
    }
}

impl<R: Read, W: Write> Read for WsReader<R, W> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        while self.remaining == 0 {
            if self.closed {
                return Ok(0);
            }
            match self.next_frame() {
                Ok(true) => {}
                Ok(false) => {
                    self.closed = true;
                    return Ok(0);
                }
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    tracing::debug!(
                        "ws connection ended without a CLOSE frame (TCP FIN from the client side / proxy); {}",
                        self.pong_note()
                    );
                    self.closed = true;
                    return Ok(0);
                }
                Err(e) => return Err(e),
            }
        }
        let want = out
            .len()
            .min(usize::try_from(self.remaining).unwrap_or(usize::MAX));
        let n = self.stream.read(&mut out[..want])?;
        if n == 0 {
            // Truncated frame: the connection is gone.
            tracing::debug!(
                "ws connection ended mid-frame (TCP FIN); {}",
                self.pong_note()
            );
            self.closed = true;
            self.remaining = 0;
            return Ok(0);
        }
        if let Some(m) = self.mask {
            unmask(&mut out[..n], m, self.offset);
        }
        self.offset += n;
        self.remaining -= n as u64;
        Ok(n)
    }
}

/// Fuzz entry: deframe `data` as a client byte stream (PONGs go to a sink)
/// until EOF or a protocol error. Must never panic.
#[cfg(any(test, feature = "fuzzing"))]
pub(crate) fn fuzz_deframe(data: &[u8]) {
    let mut r = WsReader::new(data, io::sink(), Arc::new(Mutex::new(Vec::new())));
    let mut buf = [0u8; 4096];
    while let Ok(n) = r.read(&mut buf) {
        if n == 0 {
            break;
        }
    }
}

/// XOR `buf` with the 4-byte WebSocket `mask`, where `buf[0]` is payload
/// byte number `offset` (so a payload can be unmasked in pieces). Works a
/// u64 at a time; the tail byte-wise.
fn unmask(buf: &mut [u8], mask: [u8; 4], offset: usize) {
    let r = offset % 4;
    let m = [
        mask[r],
        mask[(r + 1) % 4],
        mask[(r + 2) % 4],
        mask[(r + 3) % 4],
    ];
    let word = u64::from_ne_bytes([m[0], m[1], m[2], m[3], m[0], m[1], m[2], m[3]]);
    let mut chunks = buf.chunks_exact_mut(8);
    for c in &mut chunks {
        let v = u64::from_ne_bytes(c[..8].try_into().expect("8-byte chunk")) ^ word;
        c.copy_from_slice(&v.to_ne_bytes());
    }
    // Chunks are multiples of 8 (and so of 4): the tail starts at phase 0.
    for (i, b) in chunks.into_remainder().iter_mut().enumerate() {
        *b ^= m[i % 4];
    }
}

/// SHA-1 (FIPS 180-1). Used only for the handshake accept key.
fn sha1(data: &[u8]) -> [u8; 20] {
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
    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 80];
        for (i, word) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
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
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

fn base64(data: &[u8]) -> String {
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

    /// A masked client frame (FIN set).
    fn client_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mask = [1u8, 2, 3, 4];
        let mut out = vec![0x80 | opcode];
        let n = payload.len();
        if n < 126 {
            out.push(0x80 | n as u8);
        } else {
            out.push(0x80 | 126);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        }
        out.extend_from_slice(&mask);
        out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        out
    }

    fn deframe(wire: &[u8]) -> io::Result<Vec<u8>> {
        let mut r = WsReader::new(wire, io::sink(), Arc::new(Mutex::new(Vec::new())));
        let mut out = Vec::new();
        r.read_to_end(&mut out)?;
        Ok(out)
    }

    #[test]
    fn deframer_reassembles_masked_messages_and_answers_ping() {
        let mut wire = client_frame(OP_BINARY, b"hello ");
        wire.extend(client_frame(OP_PING, b"p"));
        wire.extend(client_frame(OP_BINARY, &[7u8; 300]));
        wire.extend(client_frame(OP_CLOSE, &1000u16.to_be_bytes()));
        wire.extend(client_frame(OP_BINARY, b"after close"));
        let out = deframe(&wire).unwrap();
        assert_eq!(&out[..6], b"hello ");
        assert_eq!(out.len(), 306, "CLOSE ends the stream");
    }

    #[test]
    fn deframer_rejects_unmasked_and_oversized_frames() {
        let unmasked = encode(OP_BINARY, b"x");
        assert_eq!(
            deframe(&unmasked).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let mut huge = vec![0x82, 0x80 | 127];
        huge.extend_from_slice(&(MAX_FRAME + 1).to_be_bytes());
        huge.extend_from_slice(&[0; 4]);
        assert!(deframe(&huge).is_err());
        let mut big_ping = vec![0x89, 0x80 | 126, 0, 200];
        big_ping.extend_from_slice(&[0; 4]);
        assert!(deframe(&big_ping).is_err());
        assert!(
            deframe(&[0x83, 0x80, 0, 0, 0, 0]).is_err(),
            "unknown opcode"
        );
    }

    #[test]
    fn head_reader_honours_the_deadline() {
        let past = Instant::now() - std::time::Duration::from_millis(1);
        let mut src: &[u8] = b"GET / HTTP/1.1\r\n\r\n";
        assert_eq!(
            read_head(&mut src, past).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        let mut src: &[u8] = b"GET / HTTP/1.1\r\n\r\nrest";
        let future = Instant::now() + std::time::Duration::from_secs(5);
        assert!(read_head(&mut src, future).unwrap().ends_with(b"\r\n\r\n"));
        assert_eq!(src, b"rest", "nothing past the head is consumed");
    }

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
        assert_eq!(&encode(OP_BINARY, &[1, 2])[..2], &[0x82, 2]);
        assert_eq!(&encode(OP_BINARY, &[0; 300])[..4], &[0x82, 126, 1, 44]);
        assert_eq!(&encode(OP_BINARY, &vec![0; 70_000])[..2], &[0x82, 127]);
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
}
