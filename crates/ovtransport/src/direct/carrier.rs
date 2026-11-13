//! Desktop carrier: the engine over real UDP sockets.

use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use super::{DirectEngine, DirectIo, DirectState, DirectStats, MediaStats};
use crate::carrier::{Carrier, CarrierStats, RawSock, TransportError};
use crate::media::{MediaEvent, MediaKeys};

/// The direct-path UDP sockets (IPv4 and, when available, IPv6), unconnected
/// and non-blocking. A host binds them once on its fixed direct port and
/// hands clones to each session; a viewer binds ephemeral ones.
pub struct DirectSockets {
    /// The IPv4 socket, if bound.
    pub v4: Option<UdpSocket>,
    /// The IPv6 (v6-only) socket, if bound.
    pub v6: Option<UdpSocket>,
}

impl DirectSockets {
    /// Bind `0.0.0.0:port` and `[::]:port` (v6-only; best effort).
    pub fn bind(port: u16) -> io::Result<Self> {
        let v4 = UdpSocket::bind(("0.0.0.0", port))?;
        let port = v4.local_addr()?.port();
        let v6 = bind_v6(port).ok();
        let s = Self { v4: Some(v4), v6 };
        s.tune();
        Ok(s)
    }

    /// Bind loopback-only sockets (tests).
    pub fn bind_loopback() -> io::Result<Self> {
        let v4 = UdpSocket::bind("127.0.0.1:0")?;
        let s = Self {
            v4: Some(v4),
            v6: None,
        };
        s.tune();
        Ok(s)
    }

    fn tune(&self) {
        for s in [&self.v4, &self.v6].into_iter().flatten() {
            let _ = s.set_nonblocking(true);
            let r = socket2::SockRef::from(s);
            let _ = r.set_recv_buffer_size(crate::udp::SOCKET_BUFFER);
            let _ = r.set_send_buffer_size(crate::udp::SOCKET_BUFFER);
            #[cfg(windows)]
            disable_connreset(s);
        }
    }

    /// The bound IPv4 port.
    pub fn port(&self) -> u16 {
        self.v4
            .as_ref()
            .and_then(|s| s.local_addr().ok())
            .map_or(0, |a| a.port())
    }

    /// Independent handles to the same sockets (one per session).
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            v4: self.v4.as_ref().map(|s| s.try_clone()).transpose()?,
            v6: self
                .v6
                .as_ref()
                .map(|s| s.try_clone())
                .transpose()
                .ok()
                .flatten(),
        })
    }

    fn send_to(&self, d: &[u8], to: SocketAddr) -> io::Result<()> {
        let s = if to.is_ipv4() { &self.v4 } else { &self.v6 };
        match s {
            Some(s) => s.send_to(d, to).map(|_| ()),
            None => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "no socket for address family",
            )),
        }
    }

    fn raw(&self) -> Vec<RawSock> {
        let mut v = Vec::new();
        for s in [&self.v4, &self.v6].into_iter().flatten() {
            #[cfg(windows)]
            {
                use std::os::windows::io::AsRawSocket;
                v.push(s.as_raw_socket());
            }
            #[cfg(not(windows))]
            {
                use std::os::fd::AsRawFd;
                v.push(s.as_raw_fd());
            }
        }
        v
    }
}

fn bind_v6(port: u16) -> io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_only_v6(true)?;
    let addr: SocketAddr = (std::net::Ipv6Addr::UNSPECIFIED, port).into();
    s.bind(&addr.into())?;
    Ok(s.into())
}

/// Windows reports an ICMP port-unreachable as `WSAECONNRESET` on the next
/// `recvfrom` of an unconnected UDP socket, which would abort the drain loop
/// for every dead candidate we probed. `SIO_UDP_CONNRESET = FALSE` turns
/// that off.
#[cfg(windows)]
fn disable_connreset(s: &UdpSocket) {
    use std::os::windows::io::AsRawSocket;
    #[link(name = "ws2_32")]
    extern "system" {
        fn WSAIoctl(
            s: usize,
            code: u32,
            inbuf: *const core::ffi::c_void,
            inlen: u32,
            outbuf: *mut core::ffi::c_void,
            outlen: u32,
            ret: *mut u32,
            overlapped: *mut core::ffi::c_void,
            completion: *mut core::ffi::c_void,
        ) -> i32;
    }
    const SIO_UDP_CONNRESET: u32 = 0x9800_000C;
    let off: u32 = 0;
    let mut ret = 0u32;
    // SAFETY: `s` is a live socket borrowed for the whole call, so its raw
    // handle is valid. The input buffer is `off` (a `u32` on this stack
    // frame, exactly the 4 bytes passed as `inlen`); there is no output
    // buffer (null, length 0); `ret` is a valid `*mut u32`; and the call is
    // synchronous (null `overlapped` / `completion`), so no pointer is
    // retained after it returns.
    unsafe {
        WSAIoctl(
            s.as_raw_socket() as usize,
            SIO_UDP_CONNRESET,
            (&off as *const u32).cast(),
            4,
            std::ptr::null_mut(),
            0,
            &mut ret,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
    }
}

/// Most distinct source addresses a carrier tracks as paths.
const MAX_PATHS: usize = 256;

struct SockIo<'a> {
    primary: &'a mut dyn Carrier,
    sockets: &'a DirectSockets,
    paths: &'a [SocketAddr],
}

impl DirectIo for SockIo<'_> {
    fn primary_send(&mut self, d: &[u8]) -> Result<(), TransportError> {
        self.primary.send(d).map(|_| ())
    }
    fn direct_send(&mut self, path: u32, d: &[u8]) -> io::Result<()> {
        let Some(&to) = self.paths.get(path as usize) else {
            return Err(io::Error::new(io::ErrorKind::NotFound, "unknown path"));
        };
        self.sockets.send_to(d, to)
    }
}

/// A [`Carrier`] that runs a session on its primary (relay) carrier and
/// upgrades it to a direct UDP path when one validates (see module docs).
pub struct DirectCarrier {
    primary: Box<dyn Carrier>,
    pub(super) engine: DirectEngine,
    pub(super) sockets: DirectSockets,
    /// Path id = index.
    paths: Vec<SocketAddr>,
    /// Host: the candidates to advertise.
    candidates: Vec<SocketAddr>,
    pub(super) primary_dead: bool,
    stats: CarrierStats,
    buf: Vec<u8>,
}

impl DirectCarrier {
    /// Wrap `primary`. `key` comes from [`session_direct_key`](super::session_direct_key).
    pub fn new(
        primary: Box<dyn Carrier>,
        sockets: DirectSockets,
        is_server: bool,
        key: [u8; 32],
    ) -> Self {
        Self {
            primary,
            engine: DirectEngine::new(is_server, key),
            sockets,
            paths: Vec::new(),
            candidates: Vec::new(),
            primary_dead: false,
            stats: CarrierStats::default(),
            buf: vec![0u8; 65_535],
        }
    }

    /// Whether this end is the host (session server).
    pub fn is_server(&self) -> bool {
        self.engine.is_server()
    }

    /// Host: the candidates [`candidates`](Self::candidates) reports.
    pub fn set_candidates(&mut self, c: Vec<SocketAddr>) {
        self.candidates = c;
    }

    /// Host: the candidate addresses to advertise.
    pub fn candidates(&self) -> &[SocketAddr] {
        &self.candidates
    }

    /// The local direct port.
    pub fn local_port(&self) -> u16 {
        self.sockets.port()
    }

    /// The peer advertised `CAP_DIRECT` (see [`DirectEngine::arm`]).
    pub fn arm(&mut self) {
        self.engine.arm();
    }

    /// Whether [`arm`](Self::arm) was called.
    pub fn is_armed(&self) -> bool {
        self.engine.is_armed()
    }

    /// Where sending currently goes.
    pub fn state(&self) -> DirectState {
        self.engine.state()
    }

    /// The engine's counters.
    pub fn direct_stats(&self) -> DirectStats {
        self.engine.stats()
    }

    /// Smoothed RTT of the direct link, once measured.
    pub fn link_rtt(&self) -> Option<Duration> {
        self.engine.link_rtt()
    }

    /// Drain the engine's path-event log lines (path ids replaced by addresses).
    pub fn take_events(&mut self) -> Vec<String> {
        let paths = &self.paths;
        self.engine
            .take_events()
            .into_iter()
            .map(|mut e| {
                // Longest ids first so `path#1` never eats `path#10`.
                for (i, a) in paths.iter().enumerate().rev() {
                    e = e.replace(&format!("path#{i}"), &format!("path {a}"));
                }
                e
            })
            .collect()
    }

    /// The direct peer address in use, if sending directly.
    pub fn direct_peer(&self) -> Option<SocketAddr> {
        self.engine
            .active_path()
            .and_then(|p| self.paths.get(p as usize).copied())
    }

    /// Open the media channel (see [`DirectEngine::enable_media`]).
    pub fn enable_media(&mut self, keys: MediaKeys) {
        self.engine.enable_media(keys);
    }

    /// See [`DirectEngine::media_ready`].
    pub fn media_ready(&self) -> bool {
        self.engine.media_ready()
    }

    /// See [`DirectEngine::cc_target`].
    pub fn cc_target(&self) -> Option<f64> {
        self.engine.cc_target()
    }

    /// See [`DirectEngine::cc_queue_delay`].
    pub fn cc_queue_delay(&self) -> Option<Duration> {
        self.engine.cc_queue_delay()
    }

    /// See [`DirectEngine::media_stats`].
    pub fn media_stats(&self) -> Option<MediaStats> {
        self.engine.media_stats()
    }

    /// See [`DirectEngine::take_media_events`].
    pub fn take_media_events(&mut self) -> Vec<MediaEvent> {
        self.engine.take_media_events()
    }

    /// Send a video unit on the media channel (`false` = send it reliably).
    pub fn send_media_video(&mut self, unit: &[u8], keyframe: bool) -> bool {
        let mut io = SockIo {
            primary: &mut *self.primary,
            sockets: &self.sockets,
            paths: &self.paths,
        };
        let ok = self
            .engine
            .send_media_video(unit, keyframe, Instant::now(), &mut io);
        if ok {
            self.stats.record_sent(unit.len());
        }
        ok
    }

    /// Send an AUDIO payload on the media channel (`false` = send it
    /// reliably).
    pub fn send_media_audio(&mut self, payload: &[u8]) -> bool {
        let mut io = SockIo {
            primary: &mut *self.primary,
            sockets: &self.sockets,
            paths: &self.paths,
        };
        let ok = self
            .engine
            .send_media_audio(payload, Instant::now(), &mut io);
        if ok {
            self.stats.record_sent(payload.len());
        }
        ok
    }

    fn path_id(&mut self, a: SocketAddr) -> Option<u32> {
        let a = normalize(a);
        if let Some(i) = self.paths.iter().position(|p| *p == a) {
            return Some(i as u32);
        }
        if self.paths.len() >= MAX_PATHS {
            return None;
        }
        self.paths.push(a);
        Some((self.paths.len() - 1) as u32)
    }

    /// Viewer: probe the host's candidates.
    pub fn start_probe(&mut self, addrs: &[SocketAddr]) {
        let (has4, has6) = (self.sockets.v4.is_some(), self.sockets.v6.is_some());
        let ids: Vec<u32> = addrs
            .iter()
            .filter(|a| if a.is_ipv4() { has4 } else { has6 })
            .filter_map(|a| self.path_id(*a))
            .collect();
        self.engine.start_probe(ids, Instant::now());
        self.service(&mut |_| {});
    }

    /// Drain the direct sockets and run the engine timers.
    fn service(&mut self, sink: &mut dyn FnMut(&[u8])) -> usize {
        let now = Instant::now();
        let mut n = 0usize;
        let mut inbound: Vec<(SocketAddr, Vec<u8>)> = Vec::new();
        for s in [&self.sockets.v4, &self.sockets.v6].into_iter().flatten() {
            for _ in 0..4096 {
                match s.recv_from(&mut self.buf) {
                    Ok((len, from)) => inbound.push((from, self.buf[..len].to_vec())),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    // ICMP errors etc. on an unconnected socket: skip.
                    Err(_) => continue,
                }
            }
        }
        let inbound: Vec<(u32, Vec<u8>)> = inbound
            .into_iter()
            .filter_map(|(from, d)| self.path_id(from).map(|p| (p, d)))
            .collect();
        let stats = &mut self.stats;
        let mut deliver = |r: &[u8]| {
            stats.record_recv(r.len());
            n += 1;
            sink(r);
        };
        for (pid, d) in inbound {
            let mut io = SockIo {
                primary: &mut *self.primary,
                sockets: &self.sockets,
                paths: &self.paths,
            };
            self.engine.on_direct(pid, &d, now, &mut io, &mut deliver);
        }
        let mut io = SockIo {
            primary: &mut *self.primary,
            sockets: &self.sockets,
            paths: &self.paths,
        };
        self.engine.poll(now, &mut io, &mut deliver);
        n
    }
}

/// Map an IPv4-mapped IPv6 address to plain IPv4 (paths compare equal).
fn normalize(a: SocketAddr) -> SocketAddr {
    match a {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::new(v4.into(), v6.port()),
            None => a,
        },
        _ => a,
    }
}

impl Carrier for DirectCarrier {
    fn send(&mut self, datagram: &[u8]) -> Result<usize, TransportError> {
        let now = Instant::now();
        let mut io = SockIo {
            primary: &mut *self.primary,
            sockets: &self.sockets,
            paths: &self.paths,
        };
        self.engine.send(datagram, now, &mut io)?;
        self.stats.record_sent(datagram.len());
        Ok(datagram.len())
    }

    fn poll_recv(&mut self, sink: &mut dyn FnMut(&[u8])) -> Result<usize, TransportError> {
        let now = Instant::now();
        let mut relay_in: Vec<Vec<u8>> = Vec::new();
        let mut primary_err = None;
        if !self.primary_dead {
            if let Err(e) = self.primary.poll_recv(&mut |d| relay_in.push(d.to_vec())) {
                primary_err = Some(e);
            }
        }
        let mut n = 0usize;
        {
            let stats = &mut self.stats;
            let mut deliver = |r: &[u8]| {
                stats.record_recv(r.len());
                n += 1;
                sink(r);
            };
            for d in relay_in {
                let mut io = SockIo {
                    primary: &mut *self.primary,
                    sockets: &self.sockets,
                    paths: &self.paths,
                };
                self.engine.on_primary(&d, now, &mut io, &mut deliver);
            }
        }
        n += self.service(sink);
        if let Some(e) = primary_err {
            // The relay died: carry on while the direct path holds.
            if self.engine.state() == DirectState::Direct {
                if !self.primary_dead {
                    self.primary_dead = true;
                    self.engine
                        .events
                        .push(format!("relay lost ({e}); continuing on the direct path"));
                    self.engine.primary_lost(now);
                }
            } else {
                return Err(e);
            }
        }
        if self.primary_dead && self.engine.state() != DirectState::Direct {
            return Err(TransportError::Closed);
        }
        Ok(n)
    }

    fn close(&mut self) -> Result<(), TransportError> {
        self.sockets.v4 = None;
        self.sockets.v6 = None;
        self.primary.close()
    }

    fn is_reliable(&self) -> bool {
        true
    }

    /// The primary path decides: a UDP bootstrap carrier (a LAN session)
    /// must switch its own link to the reliable framing once the peer's
    /// CAPS advertise `CAP_RUDP`, or large records stay raw datagrams.
    fn reliable_capable(&self) -> bool {
        self.primary.reliable_capable()
    }

    fn enable_reliable(&mut self) -> bool {
        self.primary.enable_reliable()
    }

    fn stats(&self) -> &CarrierStats {
        &self.stats
    }

    fn pending_send(&self) -> usize {
        if self.engine.state() == DirectState::Direct {
            self.engine.pending_bytes()
        } else {
            self.primary.pending_send()
        }
    }

    fn raw_socket(&self) -> Option<RawSock> {
        self.primary.raw_socket()
    }

    fn extra_sockets(&self) -> Vec<RawSock> {
        self.sockets.raw()
    }

    fn next_deadline(&self) -> Option<Duration> {
        let a = self.primary.next_deadline();
        let b = self.engine.next_deadline(Instant::now());
        match (a, b) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn direct_mut(&mut self) -> Option<&mut DirectCarrier> {
        Some(self)
    }

    fn direct_ref(&self) -> Option<&DirectCarrier> {
        Some(self)
    }
}
