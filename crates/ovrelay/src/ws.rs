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

use ovtransport::ws::{accept_key, unmask, OP_CLOSE, OP_CONT, OP_PONG, OP_TEXT};
pub(crate) use ovtransport::ws::{OP_BINARY, OP_PING};
/// Request head cap (a browser/proxy upgrade is well under 4 KiB).
const MAX_HEAD: usize = 16 * 1024;
/// Per-frame payload cap. Clients send relay-protocol chunks (a tunnel frame
/// is at most 64 KiB), so anything larger is abuse; 128 KiB leaves headroom
/// for a client that coalesces a couple of frames per message.
pub(crate) const MAX_FRAME: u64 = 128 * 1024;
/// Control frames (PING/PONG/CLOSE) carry at most 125 bytes (RFC 6455 §5.5).
const MAX_CONTROL: u64 = ovtransport::ws::MAX_CONTROL as u64;
/// Buffered read size for the deframer (headers are tiny reads otherwise).
const READ_BUF: usize = 64 * 1024;

/// Outcome of reading an HTTP request that started with `GET`.
pub(crate) enum Upgrade {
    /// Handshake answered (101); the stream now speaks WebSocket.
    Accepted,
    /// A plain HTTP request (health check, browser): answered and closed.
    Answered,
}

/// The request path WebSocket clients use unless configured otherwise
/// (`wss://host/relay`).
pub const DEFAULT_WS_PATH: &str = "/relay";

/// The only `Sec-WebSocket-Version` this server speaks (RFC 6455).
const WS_VERSION: &str = "13";

/// The parts of an HTTP request head the relay cares about.
#[derive(Debug, Default, PartialEq, Eq)]
struct Request {
    method: String,
    /// Request target without the query string.
    path: String,
    http11: bool,
    upgrade_ws: bool,
    connection_upgrade: bool,
    key: Option<String>,
    version: Option<String>,
}

fn parse_request(head: &[u8]) -> Request {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split("\r\n");
    let mut parts = lines.next().unwrap_or_default().split(' ');
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or_default();
    let http11 = parts.next() == Some("HTTP/1.1");
    let path = target.split('?').next().unwrap_or_default().to_owned();
    let mut req = Request {
        method,
        path,
        http11,
        ..Request::default()
    };
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let (name, value) = (name.trim(), value.trim());
        if name.eq_ignore_ascii_case("sec-websocket-key") {
            req.key = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("sec-websocket-version") {
            req.version = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("upgrade") {
            req.upgrade_ws = value
                .split(',')
                .any(|t| t.trim().eq_ignore_ascii_case("websocket"));
        } else if name.eq_ignore_ascii_case("connection") {
            req.connection_upgrade = value
                .split(',')
                .any(|t| t.trim().eq_ignore_ascii_case("upgrade"));
        }
    }
    req
}

/// What to answer a request with.
#[derive(Debug, PartialEq, Eq)]
enum Answer {
    /// `101 Switching Protocols` with this accept key.
    Upgrade(String),
    /// A plain response: status line, extra headers, body.
    Plain(&'static str, &'static str, &'static str),
}

/// Decide the answer (pure; unit-tested). `paths` are the accepted
/// WebSocket request paths.
fn answer(req: &Request, paths: &[String]) -> Answer {
    let ws_path = paths.contains(&req.path);
    if req.method != "GET" || !req.http11 {
        return Answer::Plain("400 Bad Request", "", "ovrelay: bad request\n");
    }
    if !req.upgrade_ws {
        return match req.path.as_str() {
            "/" | "/health" => Answer::Plain("200 OK", "", "ovrelay\n"),
            _ if ws_path => Answer::Plain(
                "426 Upgrade Required",
                "Upgrade: websocket\r\nSec-WebSocket-Version: 13\r\n",
                "ovrelay: WebSocket upgrade required\n",
            ),
            _ => Answer::Plain("404 Not Found", "", "ovrelay: not found\n"),
        };
    }
    if !ws_path {
        return Answer::Plain("404 Not Found", "", "ovrelay: unknown WebSocket path\n");
    }
    if req.version.as_deref() != Some(WS_VERSION) {
        // RFC 6455 §4.4: tell the client which version we speak.
        return Answer::Plain(
            "426 Upgrade Required",
            "Sec-WebSocket-Version: 13\r\n",
            "ovrelay: unsupported Sec-WebSocket-Version (need 13)\n",
        );
    }
    match (&req.key, req.connection_upgrade) {
        (Some(key), true) if !key.is_empty() => Answer::Upgrade(accept_key(key)),
        _ => Answer::Plain(
            "400 Bad Request",
            "",
            "ovrelay: bad WebSocket upgrade (Connection / Sec-WebSocket-Key)\n",
        ),
    }
}

/// Read the HTTP request head and answer it. A valid WebSocket upgrade on
/// one of `paths` with `Sec-WebSocket-Version: 13` gets `101 Switching
/// Protocols`; `GET /` and `GET /health` get `200`; an upgrade on another
/// path `404`, another version `426` (with `Sec-WebSocket-Version: 13`), a
/// malformed upgrade `400`. The head must arrive before `deadline`.
pub(crate) fn handshake(
    stream: &mut TcpStream,
    deadline: Instant,
    paths: &[String],
) -> io::Result<Upgrade> {
    let head = read_head(stream, deadline)?;
    let req = parse_request(&head);
    match answer(&req, paths) {
        Answer::Upgrade(accept) => {
            let resp = format!(
                "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
            );
            stream.write_all(resp.as_bytes())?;
            Ok(Upgrade::Accepted)
        }
        Answer::Plain(status, extra, body) => {
            if !status.starts_with("200") {
                tracing::debug!(
                    "refused HTTP request {} {:?}: {status}",
                    req.method,
                    req.path
                );
            }
            let resp = format!(
                "HTTP/1.1 {status}\r\n{extra}Content-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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

/// One unmasked server-to-client frame (FIN set).
pub(crate) fn encode(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    encode_into(&mut out, opcode, payload);
    out
}

/// [`encode`] into a reusable buffer (cleared first; capacity kept).
pub(crate) fn encode_into(out: &mut Vec<u8>, opcode: u8, payload: &[u8]) {
    ovtransport::ws::encode_into(out, opcode, payload, None);
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

    const UPGRADE: &str = "GET /relay HTTP/1.1\r\nHost: h\r\nUpgrade: websocket\r\nConnection: keep-alive, Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";

    fn ans(head: &str) -> Answer {
        answer(
            &parse_request(head.as_bytes()),
            &[DEFAULT_WS_PATH.to_owned()],
        )
    }

    fn status(a: &Answer) -> &str {
        match a {
            Answer::Upgrade(_) => "101",
            Answer::Plain(s, ..) => &s[..3],
        }
    }

    #[test]
    fn upgrade_on_the_relay_path_with_version_13_is_accepted() {
        assert_eq!(
            ans(UPGRADE),
            Answer::Upgrade("s3pPLMBiTxaQ9kYGzzhZRbK+xOo=".into())
        );
        // A query string does not change the path.
        assert_eq!(
            status(&ans(&UPGRADE.replace("/relay", "/relay?x=1"))),
            "101"
        );
    }

    #[test]
    fn wrong_path_and_version_are_refused_with_proper_statuses() {
        assert_eq!(status(&ans(&UPGRADE.replace("/relay", "/"))), "404");
        assert_eq!(status(&ans(&UPGRADE.replace("/relay", "/relayx"))), "404");
        let v8 = ans(&UPGRADE.replace("Version: 13", "Version: 8"));
        assert_eq!(status(&v8), "426");
        assert!(
            matches!(v8, Answer::Plain(_, extra, _) if extra.contains("Sec-WebSocket-Version: 13"))
        );
        let no_version = UPGRADE.replace("Sec-WebSocket-Version: 13\r\n", "");
        assert_eq!(status(&ans(&no_version)), "426");
        let no_key = UPGRADE.replace("Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n", "");
        assert_eq!(status(&ans(&no_key)), "400");
        let no_conn = UPGRADE.replace("Connection: keep-alive, Upgrade", "Connection: close");
        assert_eq!(status(&ans(&no_conn)), "400");
        assert_eq!(status(&ans(&UPGRADE.replace("GET", "POST"))), "400");
        assert_eq!(
            status(&ans(&UPGRADE.replace("HTTP/1.1", "HTTP/1.0"))),
            "400"
        );
    }

    #[test]
    fn plain_requests_get_health_or_hints() {
        assert_eq!(status(&ans("GET / HTTP/1.1\r\n\r\n")), "200");
        assert_eq!(status(&ans("GET /health HTTP/1.1\r\n\r\n")), "200");
        assert_eq!(status(&ans("GET /relay HTTP/1.1\r\n\r\n")), "426");
        assert_eq!(status(&ans("GET /etc/passwd HTTP/1.1\r\n\r\n")), "404");
    }

    #[test]
    fn configured_paths_replace_the_default() {
        let paths = vec!["/ov".to_owned(), "/".to_owned()];
        let req = |p: &str| parse_request(UPGRADE.replace("/relay", p).as_bytes());
        assert_eq!(status(&answer(&req("/ov"), &paths)), "101");
        assert_eq!(status(&answer(&req("/"), &paths)), "101");
        assert_eq!(status(&answer(&req("/relay"), &paths)), "404");
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
}
