//! Over the FFI: an iOS-style client session (relay + shell-owned
//! direct paths through the C sinks) upgrades to a direct UDP path offered
//! by a desktop host (`TerminalSession` + `DirectCarrier`), then records
//! flow both ways on it.

use std::ffi::{c_char, c_void, CStr, CString};
use std::net::{SocketAddr, UdpSocket};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use ovapp::terminal::{Command, TerminalSession, CAP_DIRECT};
use ovcore::handshake::{Auth, Handshake, HandshakeEvent, Role};
use ovcore::identity::Identity;
use ovtransport::{Carrier, DirectSockets, RelayTunnelCarrier, TunnelRole};

use crate::{
    ov_session_begin, ov_session_create, ov_session_direct_path, ov_session_direct_state,
    ov_session_free, ov_session_on_direct_datagram, ov_session_pump_relay,
    ov_session_relay_connect, ov_session_send_record, ov_session_set_direct_sinks,
    ov_session_set_record_sink, OvSession,
};

#[derive(Default)]
struct Shell {
    records: Vec<Vec<u8>>,
    /// path id -> the shell's UDP socket "connection" to that candidate.
    paths: Vec<(u32, SocketAddr)>,
    sock: Option<UdpSocket>,
}

type Shared = Mutex<Shell>;

fn shell(user: *mut c_void) -> &'static Shared {
    // SAFETY: a leaked Box<Shared> outliving the session.
    unsafe { &*(user as *const Shared) }
}

extern "C" fn record_sink(user: *mut c_void, _ch: u8, data: *const u8, len: usize) {
    let b = unsafe { std::slice::from_raw_parts(data, len) }.to_vec();
    shell(user).lock().unwrap().records.push(b);
}

extern "C" fn direct_send(user: *mut c_void, path: u32, data: *const u8, len: usize) {
    let b = unsafe { std::slice::from_raw_parts(data, len) };
    let g = shell(user).lock().unwrap();
    if let Some((_, to)) = g.paths.iter().find(|(p, _)| *p == path) {
        let _ = g.sock.as_ref().unwrap().send_to(b, to);
    }
}

extern "C" fn direct_open(user: *mut c_void, path: u32, addr: *const c_char) {
    let a = unsafe { CStr::from_ptr(addr) }
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    shell(user).lock().unwrap().paths.push((path, a));
}

fn start_relay() -> SocketAddr {
    let relay = ovrelay::Relay::bind("127.0.0.1:0".parse().unwrap()).expect("bind relay");
    let addr = relay.local_addr();
    thread::spawn(move || {
        let _ = relay.serve(None);
    });
    addr
}

#[test]
fn ffi_client_upgrades_to_direct_path() {
    let relay = start_relay();
    let code = "424242";
    let sid = u32::from_be_bytes(code.as_bytes()[..4].try_into().unwrap());

    // Host thread: relay responder + handshake + direct carrier.
    let host = thread::spawn(move || {
        let me = Identity::generate();
        let mut carrier = RelayTunnelCarrier::connect(relay, sid, TunnelRole::Responder).unwrap();
        assert!(carrier.wait_attached(Duration::from_secs(10)).unwrap());
        let fp = me.fingerprint();
        let dev = u64::from_be_bytes([0, 0, 0, fp[0], fp[1], fp[2], fp[3], fp[4]]);
        let mut hs = Handshake::new(Role::Server, dev, &me, Auth::PairingCode(code.into()));
        let mut backlog = Vec::new();
        let end = Instant::now() + Duration::from_secs(10);
        while !hs.is_established() && Instant::now() < end {
            carrier
                .poll_recv(&mut |d| {
                    if hs.is_established() {
                        backlog.push(d.to_vec());
                    } else {
                        let _ = hs.feed(d);
                    }
                })
                .unwrap();
            for ev in hs.take_events() {
                let HandshakeEvent::Sent(d) = ev;
                carrier.send(&d).unwrap();
            }
            thread::sleep(Duration::from_millis(2));
        }
        let mut term = TerminalSession::new(hs.take_session().unwrap(), carrier);
        term.queue_inbound(backlog);
        let sockets = DirectSockets::bind_loopback().unwrap();
        let port = sockets.port();
        term.enable_direct(sockets, vec![format!("127.0.0.1:{port}").parse().unwrap()]);
        let echoes = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let e2 = echoes.clone();
        term.on_cmd(Some(Box::new(move |c| {
            if matches!(c, Command::Echo(_)) {
                e2.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        })));
        let end = Instant::now() + Duration::from_secs(15);
        while Instant::now() < end && echoes.load(std::sync::atomic::Ordering::Relaxed) < 50 {
            term.pump().unwrap();
            thread::sleep(Duration::from_millis(1));
        }
        // Keep serving briefly so the last echoes flush.
        let end = Instant::now() + Duration::from_millis(500);
        while Instant::now() < end {
            let _ = term.pump();
            thread::sleep(Duration::from_millis(1));
        }
        (
            term.direct_state(),
            echoes.load(std::sync::atomic::Ordering::Relaxed),
        )
    });

    // FFI client.
    let user = Box::into_raw(Box::new(Shared::default())) as *mut c_void;
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_nonblocking(true).unwrap();
    shell(user).lock().unwrap().sock = Some(sock.try_clone().unwrap());
    let ccode = CString::new(code).unwrap();
    let host_c = CString::new("127.0.0.1").unwrap();
    let s: *mut OvSession = unsafe { ov_session_create(0, ccode.as_ptr()) };
    unsafe {
        assert_eq!(ov_session_set_record_sink(s, user, record_sink), 0);
        assert_eq!(
            ov_session_set_direct_sinks(s, user, direct_send, direct_open),
            0
        );
        assert_eq!(
            ov_session_relay_connect(s, host_c.as_ptr(), relay.port(), sid),
            0
        );
        assert_eq!(ov_session_begin(s, 0), 0);
    }
    let pump = |s: *mut OvSession| {
        let mut buf = [0u8; 2048];
        while let Ok((n, from)) = sock.recv_from(&mut buf) {
            let path = shell(user)
                .lock()
                .unwrap()
                .paths
                .iter()
                .find(|(_, a)| *a == from)
                .map(|(p, _)| *p);
            if let Some(p) = path {
                unsafe { ov_session_on_direct_datagram(s, p, buf.as_ptr(), n) };
            }
        }
        let rc = unsafe { ov_session_pump_relay(s) };
        assert!(rc == 0, "pump_relay {rc}");
    };
    // Establish, then advertise CAP_DIRECT (what OvScreen's CAPS does).
    let end = Instant::now() + Duration::from_secs(10);
    let caps = ovapp::terminal::encode(&Command::Caps {
        flags: ovapp::terminal::LOCAL_CAPS | CAP_DIRECT,
    });
    let mut sent_caps = false;
    while Instant::now() < end && unsafe { ov_session_direct_state(s) } != 2 {
        pump(s);
        if !sent_caps && unsafe { ov_session_send_record(s, 0, caps.as_ptr(), caps.len()) } == 0 {
            sent_caps = true;
        }
        thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(
        unsafe { ov_session_direct_state(s) },
        2,
        "promoted to direct"
    );
    assert!(unsafe { ov_session_direct_path(s) } >= 0);
    // 50 echoes over the direct path; the host echoes each back.
    for i in 0..50u8 {
        let e = ovapp::terminal::encode(&Command::Echo(vec![i; 2000]));
        assert_eq!(
            unsafe { ov_session_send_record(s, 0, e.as_ptr(), e.len()) },
            0
        );
        pump(s);
    }
    let end = Instant::now() + Duration::from_secs(10);
    let echoed = |sh: &Shell| {
        sh.records
            .iter()
            .filter(|r| r.first() == Some(&0x01))
            .count()
    };
    while Instant::now() < end && echoed(&shell(user).lock().unwrap()) < 50 {
        pump(s);
        thread::sleep(Duration::from_millis(1));
    }
    let (host_state, host_echoes) = host.join().unwrap();
    assert_eq!(host_echoes, 50);
    assert_eq!(host_state, Some(ovtransport::DirectState::Direct));
    let g = shell(user).lock().unwrap();
    let got: Vec<u8> = g
        .records
        .iter()
        .filter(|r| r.first() == Some(&0x01))
        .map(|r| r[1])
        .collect();
    assert_eq!(got, (0..50).collect::<Vec<u8>>(), "echoes in order");
    // The host's DIRECT_CANDIDATES record also reached the shell.
    assert!(g.records.iter().any(|r| r.first() == Some(&0x61)));
    drop(g);
    unsafe { ov_session_free(s) };
}

/// A relay carrier the test can kill mid-session (sends DISCONNECT through
/// the relay, which then closes the client's side too).
struct Killable {
    inner: RelayTunnelCarrier,
    kill: std::sync::Arc<std::sync::atomic::AtomicBool>,
    dead: bool,
}

impl Carrier for Killable {
    fn send(&mut self, d: &[u8]) -> Result<usize, ovtransport::TransportError> {
        if self.dead {
            return Err(ovtransport::TransportError::Closed);
        }
        self.inner.send(d)
    }
    fn poll_recv(
        &mut self,
        sink: &mut dyn FnMut(&[u8]),
    ) -> Result<usize, ovtransport::TransportError> {
        if !self.dead && self.kill.load(std::sync::atomic::Ordering::Relaxed) {
            self.dead = true;
            let _ = self.inner.close();
        }
        if self.dead {
            return Err(ovtransport::TransportError::Closed);
        }
        self.inner.poll_recv(sink)
    }
    fn close(&mut self) -> Result<(), ovtransport::TransportError> {
        self.inner.close()
    }
    fn is_reliable(&self) -> bool {
        true
    }
    fn stats(&self) -> &ovtransport::CarrierStats {
        self.inner.stats()
    }
}

/// Follow-up (live: the relay WebSocket dies ~30 s after going direct):
/// once the relay is gone the FFI session keeps running on the direct path
/// — `ov_session_pump_relay` keeps returning 0 and records flow both ways.
#[test]
fn ffi_client_survives_relay_loss_on_the_direct_path() {
    let relay = start_relay();
    let code = "454545";
    let sid = u32::from_be_bytes(code.as_bytes()[..4].try_into().unwrap());
    let kill = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let host_kill = kill.clone();
    let host = thread::spawn(move || {
        let me = Identity::generate();
        let mut carrier = RelayTunnelCarrier::connect(relay, sid, TunnelRole::Responder).unwrap();
        assert!(carrier.wait_attached(Duration::from_secs(10)).unwrap());
        let fp = me.fingerprint();
        let dev = u64::from_be_bytes([0, 0, 0, fp[0], fp[1], fp[2], fp[3], fp[4]]);
        let mut hs = Handshake::new(Role::Server, dev, &me, Auth::PairingCode(code.into()));
        let mut backlog = Vec::new();
        let end = Instant::now() + Duration::from_secs(10);
        while !hs.is_established() && Instant::now() < end {
            carrier
                .poll_recv(&mut |d| {
                    if hs.is_established() {
                        backlog.push(d.to_vec());
                    } else {
                        let _ = hs.feed(d);
                    }
                })
                .unwrap();
            for ev in hs.take_events() {
                let HandshakeEvent::Sent(d) = ev;
                carrier.send(&d).unwrap();
            }
            thread::sleep(Duration::from_millis(2));
        }
        let carrier = Killable {
            inner: carrier,
            kill: host_kill,
            dead: false,
        };
        let mut term = TerminalSession::new(hs.take_session().unwrap(), carrier);
        term.queue_inbound(backlog);
        let sockets = DirectSockets::bind_loopback().unwrap();
        let port = sockets.port();
        term.enable_direct(sockets, vec![format!("127.0.0.1:{port}").parse().unwrap()]);
        let echoes = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let e2 = echoes.clone();
        term.on_cmd(Some(Box::new(move |c| {
            if matches!(c, Command::Echo(_)) {
                e2.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        })));
        let end = Instant::now() + Duration::from_secs(20);
        let mut failed = None;
        while Instant::now() < end && echoes.load(std::sync::atomic::Ordering::Relaxed) < 40 {
            if let Err(e) = term.pump() {
                failed = Some(e.to_string());
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        let end = Instant::now() + Duration::from_millis(500);
        while Instant::now() < end && failed.is_none() {
            let _ = term.pump();
            thread::sleep(Duration::from_millis(1));
        }
        (
            term.direct_state(),
            echoes.load(std::sync::atomic::Ordering::Relaxed),
            failed,
            term.take_direct_events(),
        )
    });

    let user = Box::into_raw(Box::new(Shared::default())) as *mut c_void;
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_nonblocking(true).unwrap();
    shell(user).lock().unwrap().sock = Some(sock.try_clone().unwrap());
    let ccode = CString::new(code).unwrap();
    let host_c = CString::new("127.0.0.1").unwrap();
    let s: *mut OvSession = unsafe { ov_session_create(0, ccode.as_ptr()) };
    unsafe {
        assert_eq!(ov_session_set_record_sink(s, user, record_sink), 0);
        assert_eq!(
            ov_session_set_direct_sinks(s, user, direct_send, direct_open),
            0
        );
        assert_eq!(
            ov_session_relay_connect(s, host_c.as_ptr(), relay.port(), sid),
            0
        );
        assert_eq!(ov_session_begin(s, 0), 0);
    }
    let pump = |s: *mut OvSession| {
        let mut buf = [0u8; 2048];
        while let Ok((n, from)) = sock.recv_from(&mut buf) {
            let path = shell(user)
                .lock()
                .unwrap()
                .paths
                .iter()
                .find(|(_, a)| *a == from)
                .map(|(p, _)| *p);
            if let Some(p) = path {
                unsafe { ov_session_on_direct_datagram(s, p, buf.as_ptr(), n) };
            }
        }
        let rc = unsafe { ov_session_pump_relay(s) };
        assert_eq!(
            rc, 0,
            "pump_relay after the relay died must keep the direct session"
        );
    };
    let caps = ovapp::terminal::encode(&Command::Caps {
        flags: ovapp::terminal::LOCAL_CAPS | CAP_DIRECT | ovapp::terminal::CAP_MEDIA,
    });
    let mut sent_caps = false;
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end && unsafe { ov_session_direct_state(s) } != 2 {
        pump(s);
        if !sent_caps && unsafe { ov_session_send_record(s, 0, caps.as_ptr(), caps.len()) } == 0 {
            sent_caps = true;
        }
        thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(unsafe { ov_session_direct_state(s) }, 2);
    // Let the SYNC markers settle, then the relay dies.
    let t = Instant::now() + Duration::from_millis(300);
    while Instant::now() < t {
        pump(s);
        thread::sleep(Duration::from_millis(2));
    }
    kill.store(true, std::sync::atomic::Ordering::Relaxed);
    let t = Instant::now() + Duration::from_millis(500);
    while Instant::now() < t {
        pump(s);
        thread::sleep(Duration::from_millis(2));
    }
    // 40 echoes over the direct path only.
    for i in 0..40u8 {
        let e = ovapp::terminal::encode(&Command::Echo(vec![i; 500]));
        assert_eq!(
            unsafe { ov_session_send_record(s, 0, e.as_ptr(), e.len()) },
            0
        );
        pump(s);
    }
    let echoed = |sh: &Shell| {
        sh.records
            .iter()
            .filter(|r| r.first() == Some(&0x01))
            .count()
    };
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end && echoed(&shell(user).lock().unwrap()) < 40 {
        pump(s);
        thread::sleep(Duration::from_millis(1));
    }
    let (state, echoes, failed, events) = host.join().unwrap();
    assert_eq!(failed, None, "host session ended: {events:?}");
    assert_eq!(echoes, 40);
    assert_eq!(state, Some(ovtransport::DirectState::Direct));
    assert!(
        events.iter().any(|e| e.contains("relay lost")),
        "{events:?}"
    );
    assert_eq!(
        echoed(&shell(user).lock().unwrap()),
        40,
        "echoes back to the client"
    );
    assert_eq!(unsafe { ov_session_direct_state(s) }, 2);
    unsafe { ov_session_free(s) };
}

/// The FFI session negotiates `CAP_MEDIA` (peeking the CAPS the
/// shell sends and the host's reply), then receives H.264 units and audio
/// over the media channel as ready-made channel-0 `VIDEO_UNIT` / `AUDIO`
/// records — exactly what `ov_screen_feed` consumes.
#[test]
fn ffi_client_receives_the_media_channel() {
    let relay = start_relay();
    let code = "434343";
    let sid = u32::from_be_bytes(code.as_bytes()[..4].try_into().unwrap());
    let host = thread::spawn(move || {
        let me = Identity::generate();
        let mut carrier = RelayTunnelCarrier::connect(relay, sid, TunnelRole::Responder).unwrap();
        assert!(carrier.wait_attached(Duration::from_secs(10)).unwrap());
        let fp = me.fingerprint();
        let dev = u64::from_be_bytes([0, 0, 0, fp[0], fp[1], fp[2], fp[3], fp[4]]);
        let mut hs = Handshake::new(Role::Server, dev, &me, Auth::PairingCode(code.into()));
        let mut backlog = Vec::new();
        let end = Instant::now() + Duration::from_secs(10);
        while !hs.is_established() && Instant::now() < end {
            carrier
                .poll_recv(&mut |d| {
                    if hs.is_established() {
                        backlog.push(d.to_vec());
                    } else {
                        let _ = hs.feed(d);
                    }
                })
                .unwrap();
            for ev in hs.take_events() {
                let HandshakeEvent::Sent(d) = ev;
                carrier.send(&d).unwrap();
            }
            thread::sleep(Duration::from_millis(2));
        }
        let mut term = TerminalSession::new(hs.take_session().unwrap(), carrier);
        term.queue_inbound(backlog);
        let sockets = DirectSockets::bind_loopback().unwrap();
        let port = sockets.port();
        term.enable_direct(sockets, vec![format!("127.0.0.1:{port}").parse().unwrap()]);
        let end = Instant::now() + Duration::from_secs(15);
        while Instant::now() < end && !term.media_ready() {
            term.pump().unwrap();
            thread::sleep(Duration::from_millis(1));
        }
        let ready = term.media_ready();
        for seq in 0..10u64 {
            let header = ovapp::video::VideoHeader {
                seq,
                flags: if seq == 0 {
                    ovapp::video::VIDEO_KEYFRAME
                } else {
                    0
                },
                codec: ovapp::video::CODEC_H264,
                rect: ovapp::tiles::TileRect {
                    x: 0,
                    y: 0,
                    w: 64,
                    h: 64,
                },
                screen_w: 128,
                screen_h: 128,
                pts_us: 1,
            };
            assert!(term
                .send_video_media(&header, &vec![seq as u8; 9000])
                .unwrap());
            term.send_audio(vec![1, 0, 0, 0, 1, 224, 7, 7]).unwrap();
            let t = Instant::now() + Duration::from_millis(10);
            while Instant::now() < t {
                let _ = term.pump();
                thread::sleep(Duration::from_millis(1));
            }
        }
        let end = Instant::now() + Duration::from_millis(800);
        while Instant::now() < end {
            let _ = term.pump();
            thread::sleep(Duration::from_millis(1));
        }
        (ready, term.media_stats().map(|s| s.frames_tx))
    });

    let user = Box::into_raw(Box::new(Shared::default())) as *mut c_void;
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_nonblocking(true).unwrap();
    shell(user).lock().unwrap().sock = Some(sock.try_clone().unwrap());
    let ccode = CString::new(code).unwrap();
    let host_c = CString::new("127.0.0.1").unwrap();
    let s: *mut OvSession = unsafe { ov_session_create(0, ccode.as_ptr()) };
    unsafe {
        assert_eq!(ov_session_set_record_sink(s, user, record_sink), 0);
        assert_eq!(
            ov_session_set_direct_sinks(s, user, direct_send, direct_open),
            0
        );
        assert_eq!(
            ov_session_relay_connect(s, host_c.as_ptr(), relay.port(), sid),
            0
        );
        assert_eq!(ov_session_begin(s, 0), 0);
    }
    let pump = |s: *mut OvSession| {
        let mut buf = [0u8; 2048];
        while let Ok((n, from)) = sock.recv_from(&mut buf) {
            let path = shell(user)
                .lock()
                .unwrap()
                .paths
                .iter()
                .find(|(_, a)| *a == from)
                .map(|(p, _)| *p);
            if let Some(p) = path {
                unsafe { ov_session_on_direct_datagram(s, p, buf.as_ptr(), n) };
            }
        }
        assert_eq!(unsafe { ov_session_pump_relay(s) }, 0);
    };
    let caps = ovapp::terminal::encode(&Command::Caps {
        flags: ovapp::terminal::LOCAL_CAPS | CAP_DIRECT | ovapp::terminal::CAP_MEDIA,
    });
    let mut sent_caps = false;
    let units = |sh: &Shell| {
        sh.records
            .iter()
            .filter(|r| r.first() == Some(&0x27))
            .count()
    };
    let end = Instant::now() + Duration::from_secs(20);
    while Instant::now() < end && units(&shell(user).lock().unwrap()) < 10 {
        pump(s);
        if !sent_caps && unsafe { ov_session_send_record(s, 0, caps.as_ptr(), caps.len()) } == 0 {
            sent_caps = true;
        }
        thread::sleep(Duration::from_millis(1));
    }
    let (ready, frames_tx) = host.join().unwrap();
    assert!(ready, "host saw the media channel open");
    assert_eq!(frames_tx, Some(10));
    let g = shell(user).lock().unwrap();
    let mut seqs = Vec::new();
    for r in g.records.iter().filter(|r| r.first() == Some(&0x27)) {
        match ovapp::terminal::decode(r).unwrap() {
            Command::VideoUnit { header, data } => {
                assert_eq!(data, vec![header.seq as u8; 9000]);
                seqs.push(header.seq);
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(seqs, (0..10).collect::<Vec<u64>>());
    let audio = g
        .records
        .iter()
        .filter(|r| r.first() == Some(&0x40))
        .count();
    assert_eq!(audio, 10);
    drop(g);
    unsafe { ov_session_free(s) };
}
