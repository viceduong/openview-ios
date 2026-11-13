//! WebSocket client transport for the relay carrier (feature `ws-client`):
//! reach `ovrelay` at `ws://host/path` or `wss://host/path`, e.g. through a
//! reverse proxy or a Cloudflare tunnel that only passes HTTP(S).
//!
//! Design: the relay carrier stays a plain non-blocking TCP carrier. [`open`]
//! dials the server (TLS for `wss`), performs the HTTP/1.1 upgrade, and then
//! hands the caller one end of a private loopback TCP pair; a pump thread
//! owns the WebSocket and the other end and shuttles bytes:
//!
//! ```text
//! RelayTunnelCarrier ⇄ loopback TCP ⇄ pump thread ⇄ (TLS) WebSocket ⇄ ovrelay
//! ```
//!
//! So every carrier feature built on a real socket keeps working unchanged:
//! read-until-`WouldBlock`, send backpressure, [`set_read_waker`] and
//! [`raw_socket`] (OS readiness waits). The pump is event driven too: it
//! blocks in one `poll` on both sockets, answers server PINGs, frames each
//! outbound read as one masked binary message, and turns a close on either
//! side into a close on the other (WebSocket CLOSE / TCP shutdown).
//!
//! TLS is the OS stack (`native-tls`, i.e. SChannel on Windows): no C
//! toolchain, system certificate store and proxy-inspection roots honoured.
//! It is compiled on Windows only; elsewhere `wss://` reports `Unsupported`
//! (`ws://` works everywhere).
//!
//! [`set_read_waker`]: super::RelayTunnelCarrier::set_read_waker
//! [`raw_socket`]: crate::Carrier::raw_socket

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::time::Duration;

use crate::ws::{self, Deframer, OP_BINARY, OP_CLOSE, OP_CONT, OP_PING, OP_PONG, OP_TEXT};

use super::watch::{raw_of, wait_readable_any};

/// A WebSocket relay endpoint (`ws[s]://host[:port]/path`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WsEndpoint {
    /// Hostname or IP literal (IPv6 without brackets); also the TLS server
    /// name and the HTTP `Host`.
    pub host: String,
    /// TCP port.
    pub port: u16,
    /// HTTP request target (path plus query), starting with `/`.
    pub path: String,
    /// `wss://` (TLS).
    pub tls: bool,
}

/// Largest inbound WebSocket message payload accepted. The relay sends one
/// message per pipe read (64 KiB); proxies may coalesce, so leave headroom.
const MAX_INBOUND: usize = 4 * 1024 * 1024;
/// Largest outbound message: one loopback read. The relay accepts client
/// frames up to 128 KiB.
const OUT_CHUNK: usize = 64 * 1024;
/// Response head cap for the upgrade.
const MAX_HEAD: usize = 16 * 1024;
/// A network write that makes no progress this long ends the pump (the
/// carrier side gives up after the same time).
const NET_WRITE_TIMEOUT: Duration = super::WRITE_STALL_TIMEOUT;

/// Dial `ep`, upgrade to WebSocket, and return the carrier's end of the
/// loopback pair (blocking mode; the caller configures it). Every step is
/// bounded by `timeout` (per resolved address for the TCP connect).
pub(crate) fn open(ep: &WsEndpoint, timeout: Duration) -> io::Result<TcpStream> {
    let tcp = super::dial_tcp((ep.host.as_str(), ep.port), timeout)?;
    let _ = tcp.set_nodelay(true);
    tcp.set_read_timeout(Some(timeout))?;
    tcp.set_write_timeout(Some(timeout))?;
    let mut stream = if ep.tls {
        WsStream::tls(&ep.host, tcp)?
    } else {
        WsStream::Plain(tcp)
    };
    upgrade(&mut stream, ep)?;
    stream.tcp().set_read_timeout(None)?;
    stream.tcp().set_write_timeout(Some(NET_WRITE_TIMEOUT))?;
    let (near, far) = loopback_pair()?;
    let _ = near.set_nodelay(true);
    let _ = far.set_nodelay(true);
    std::thread::Builder::new()
        .name("ov-relay-ws".into())
        .spawn(move || {
            let _ = Pump::new(stream, far).run();
        })?;
    Ok(near)
}

/// The network side: plain TCP or TLS over TCP.
enum WsStream {
    Plain(TcpStream),
    #[cfg(windows)]
    Tls(Box<native_tls::TlsStream<TcpStream>>),
}

impl WsStream {
    #[cfg(windows)]
    fn tls(host: &str, tcp: TcpStream) -> io::Result<Self> {
        let connector = native_tls::TlsConnector::new().map_err(io::Error::other)?;
        match connector.connect(host, tcp) {
            Ok(s) => Ok(Self::Tls(Box::new(s))),
            Err(native_tls::HandshakeError::Failure(e)) => Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                format!("TLS connection to {host} failed: {e}"),
            )),
            Err(native_tls::HandshakeError::WouldBlock(_)) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("TLS connection to {host} timed out"),
            )),
        }
    }

    #[cfg(not(windows))]
    fn tls(_host: &str, _tcp: TcpStream) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "wss:// relays need the Windows build (use ws:// or host:port)",
        ))
    }

    fn tcp(&self) -> &TcpStream {
        match self {
            Self::Plain(s) => s,
            #[cfg(windows)]
            Self::Tls(s) => s.get_ref(),
        }
    }

    fn shutdown(&mut self) {
        match self {
            Self::Plain(s) => {
                let _ = s.shutdown(std::net::Shutdown::Both);
            }
            #[cfg(windows)]
            Self::Tls(s) => {
                let _ = s.shutdown();
                let _ = s.get_ref().shutdown(std::net::Shutdown::Both);
            }
        }
    }
}

impl Read for WsStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(s) => s.read(buf),
            #[cfg(windows)]
            Self::Tls(s) => s.read(buf),
        }
    }
}

impl Write for WsStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Plain(s) => s.write(buf),
            #[cfg(windows)]
            Self::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(s) => s.flush(),
            #[cfg(windows)]
            Self::Tls(s) => s.flush(),
        }
    }
}

/// Send the HTTP/1.1 upgrade request and validate the `101` answer
/// (status, `Upgrade`, `Sec-WebSocket-Accept`).
fn upgrade(stream: &mut WsStream, ep: &WsEndpoint) -> io::Result<()> {
    let key = ws::base64(&random_bytes::<16>()?);
    let default_port = if ep.tls { 443 } else { 80 };
    let host = if ep.host.contains(':') {
        format!("[{}]", ep.host)
    } else {
        ep.host.clone()
    };
    let host = if ep.port == default_port {
        host
    } else {
        format!("{host}:{}", ep.port)
    };
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\nUser-Agent: OpenView/{}\r\n\r\n",
        ep.path,
        env!("CARGO_PKG_VERSION"),
    );
    stream.write_all(request.as_bytes())?;
    stream.flush()?;
    let head = read_head(stream)?;
    check_upgrade_response(&String::from_utf8_lossy(&head), &key)
}

/// Read the response head up to the blank line, one byte at a time so no
/// WebSocket bytes behind it are consumed.
fn read_head(stream: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut head = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= MAX_HEAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "HTTP response head too large",
            ));
        }
        match stream.read(&mut byte) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "relay closed the connection during the WebSocket upgrade",
                ))
            }
            Ok(_) => head.push(byte[0]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(head)
}

/// Validate an upgrade response head against the key we sent.
fn check_upgrade_response(head: &str, key: &str) -> io::Result<()> {
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap_or_default();
    let code = status.split_whitespace().nth(1).unwrap_or_default();
    if code != "101" {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!("WebSocket upgrade rejected: {status:?} (is this an ovrelay URL?)"),
        ));
    }
    let mut accept_ok = false;
    let mut upgrade_ok = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let (name, value) = (name.trim(), value.trim());
        if name.eq_ignore_ascii_case("sec-websocket-accept") {
            accept_ok = value == ws::accept_key(key);
        } else if name.eq_ignore_ascii_case("upgrade") {
            upgrade_ok = value.eq_ignore_ascii_case("websocket");
        }
    }
    if accept_ok && upgrade_ok {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad WebSocket upgrade response (Upgrade / Sec-WebSocket-Accept)",
        ))
    }
}

/// A connected loopback TCP pair. The accepted end is checked against the
/// connecting end's address, so another local process racing to the
/// ephemeral listener cannot become the carrier's peer.
fn loopback_pair() -> io::Result<(TcpStream, TcpStream)> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let near = TcpStream::connect(listener.local_addr()?)?;
    let want = near.local_addr()?;
    loop {
        let (far, from) = listener.accept()?;
        if from == want {
            return Ok((near, far));
        }
    }
}

/// `N` bytes from the OS RNG (WebSocket key and frame masks).
fn random_bytes<const N: usize>() -> io::Result<[u8; N]> {
    use rand_core::RngCore;
    let mut b = [0u8; N];
    rand_core::OsRng
        .try_fill_bytes(&mut b)
        .map_err(io::Error::other)?;
    Ok(b)
}

/// Why the pump stopped.
enum End {
    /// The relay side closed (CLOSE, EOF or error).
    Net,
    /// The carrier dropped its end of the loopback pair.
    Local,
}

/// The thread bridging the WebSocket and the loopback socket.
struct Pump {
    net: WsStream,
    local: TcpStream,
    deframer: Deframer,
    net_buf: Vec<u8>,
    local_buf: Vec<u8>,
    payload: Vec<u8>,
    frame: Vec<u8>,
}

impl Pump {
    fn new(net: WsStream, local: TcpStream) -> Self {
        Self {
            net,
            local,
            deframer: Deframer::new(MAX_INBOUND),
            net_buf: vec![0u8; 64 * 1024],
            local_buf: vec![0u8; OUT_CHUNK],
            payload: Vec::new(),
            frame: Vec::new(),
        }
    }

    fn run(mut self) -> io::Result<()> {
        let end = self.pump();
        match end {
            Ok(End::Local) | Err(_) => {
                // Carrier gone (or we failed): say goodbye to the relay.
                let _ = self.send(OP_CLOSE, &1000u16.to_be_bytes());
                self.net.shutdown();
                let _ = self.local.shutdown(std::net::Shutdown::Both);
            }
            Ok(End::Net) => {
                // Relay gone: the carrier reads EOF after the buffered bytes.
                let _ = self.local.shutdown(std::net::Shutdown::Write);
                self.net.shutdown();
            }
        }
        end.map(|_| ())
    }

    fn pump(&mut self) -> io::Result<End> {
        let net_sock = raw_of(self.net.tcp());
        let local_sock = raw_of(&self.local);
        // The upgrade may have left decrypted bytes in the TLS buffer that
        // a socket poll cannot see: drain once up front.
        if let Some(end) = self.drain_net()? {
            return Ok(end);
        }
        loop {
            let ready = wait_readable_any(&[net_sock, local_sock], Duration::from_secs(5))?;
            if ready & 1 != 0 {
                if let Some(end) = self.drain_net()? {
                    return Ok(end);
                }
            }
            if ready & 2 != 0 {
                let n = match self.local.read(&mut self.local_buf) {
                    Ok(0) => return Ok(End::Local),
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => return Ok(End::Local),
                };
                let mask = random_bytes::<4>()?;
                ws::encode_into(&mut self.frame, OP_BINARY, &self.local_buf[..n], Some(mask));
                self.net.write_all(&self.frame)?;
                self.net.flush()?;
            }
        }
    }

    /// Read everything the network side has, handling each complete frame
    /// as it arrives. Reads are non-blocking until `WouldBlock`, so
    /// TLS-buffered plaintext (invisible to a socket poll) is drained too;
    /// writes (PONG/CLOSE, data) stay blocking with a stall timeout.
    /// `Some(End::Net)` when the relay side is done.
    fn drain_net(&mut self) -> io::Result<Option<End>> {
        loop {
            self.net.tcp().set_nonblocking(true)?;
            let read = self.net.read(&mut self.net_buf);
            self.net.tcp().set_nonblocking(false)?;
            match read {
                Ok(0) => return Ok(Some(End::Net)),
                Ok(n) => {
                    self.deframer.push(&self.net_buf[..n]);
                    if let Some(end) = self.handle_frames()? {
                        return Ok(Some(end));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }

    /// Act on every complete frame in the deframer.
    fn handle_frames(&mut self) -> io::Result<Option<End>> {
        loop {
            self.payload.clear();
            let Some(op) = self.deframer.next_frame(&mut self.payload)? else {
                return Ok(None);
            };
            match op {
                OP_BINARY | OP_TEXT | OP_CONT => {
                    if self.local.write_all(&self.payload).is_err() {
                        return Ok(Some(End::Local));
                    }
                }
                OP_PING => {
                    let p = std::mem::take(&mut self.payload);
                    self.send(OP_PONG, &p)?;
                    self.payload = p;
                }
                OP_CLOSE => {
                    // Echo the close (RFC 6455 §5.5.1), then end.
                    let code = self.payload.get(..2).unwrap_or_default().to_vec();
                    let _ = self.send(OP_CLOSE, &code);
                    return Ok(Some(End::Net));
                }
                _ => {} // PONG
            }
        }
    }

    /// Send one masked frame.
    fn send(&mut self, op: u8, payload: &[u8]) -> io::Result<()> {
        let mask = random_bytes::<4>()?;
        ws::encode_into(&mut self.frame, op, payload, Some(mask));
        self.net.write_all(&self.frame)?;
        self.net.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upgrade_response_validation() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let good = "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n";
        assert!(check_upgrade_response(good, key).is_ok());
        let wrong_key = good.replace("s3pP", "AAAA");
        assert!(check_upgrade_response(&wrong_key, key).is_err());
        let no_upgrade = good.replace("Upgrade: websocket\r\n", "");
        assert!(check_upgrade_response(&no_upgrade, key).is_err());
        let refused = "HTTP/1.1 426 Upgrade Required\r\n\r\n";
        let e = check_upgrade_response(refused, key).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::ConnectionRefused);
    }

    #[test]
    fn loopback_pair_is_connected() {
        let (mut a, mut b) = loopback_pair().unwrap();
        a.write_all(b"ping").unwrap();
        let mut buf = [0u8; 4];
        b.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"ping");
    }

    /// The pump against a scripted server: PING answered with a masked
    /// PONG, data both ways, server CLOSE echoed and seen as EOF locally.
    #[test]
    fn pump_bridges_a_scripted_server() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let ep = WsEndpoint {
            host: "127.0.0.1".into(),
            port: listener.local_addr().unwrap().port(),
            path: "/relay".into(),
            tls: false,
        };
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let head = read_head(&mut s).unwrap();
            let head = String::from_utf8(head).unwrap();
            assert!(head.starts_with("GET /relay HTTP/1.1\r\n"), "{head}");
            let key = head
                .lines()
                .find_map(|l| l.strip_prefix("Sec-WebSocket-Key: "))
                .unwrap()
                .to_owned();
            let resp = format!(
                "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
                ws::accept_key(&key)
            );
            // Head, PING and a data message in one write.
            let mut out = resp.into_bytes();
            out.extend(ws::encode(OP_PING, b"hi", None));
            out.extend(ws::encode(OP_BINARY, b"from server", None));
            s.write_all(&out).unwrap();
            let mut d = Deframer::new(1 << 20);
            let mut got = Vec::new();
            let mut buf = [0u8; 4096];
            let mut saw_pong = false;
            let mut first = true;
            while got.len() < 11 || !saw_pong {
                let n = s.read(&mut buf).unwrap();
                assert!(n > 0, "client closed early");
                if std::mem::take(&mut first) {
                    assert_eq!(buf[1] & 0x80, 0x80, "client frames are masked");
                }
                d.push(&buf[..n]);
                let mut p = Vec::new();
                while let Some(op) = d.next_frame(&mut p).unwrap() {
                    match op {
                        OP_PONG => {
                            assert_eq!(p, b"hi");
                            saw_pong = true;
                        }
                        OP_BINARY => got.extend_from_slice(&p),
                        other => panic!("unexpected opcode {other}"),
                    }
                    p.clear();
                }
            }
            assert_eq!(got, b"from client");
            s.write_all(&ws::encode(OP_CLOSE, &1000u16.to_be_bytes(), None))
                .unwrap();
            // The client echoes the close.
            let mut rest = Vec::new();
            let _ = s.read_to_end(&mut rest);
            d.push(&rest);
            let mut p = Vec::new();
            assert_eq!(d.next_frame(&mut p).unwrap(), Some(OP_CLOSE));
        });
        let mut near = open(&ep, Duration::from_secs(5)).unwrap();
        near.write_all(b"from client").unwrap();
        let mut got = Vec::new();
        near.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"from server");
        server.join().unwrap();
    }
}
