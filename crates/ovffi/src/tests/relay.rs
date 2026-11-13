//! FFI relay mode end to end: two sessions handshake through a real
//! in-process `ovrelay::Relay` — over the Rust-owned TCP relay
//! (`ov_session_relay_connect`) and over the host-owned byte-stream bridge
//! (`ov_session_relay_connect_stream`, here a hand-rolled WebSocket client
//! standing in for iOS `URLSessionWebSocketTask`) — then exchange records
//! and observe the relay-closed code (-5).
//!
//! Each session's sinks write into its own `Collector` (passed as the sink
//! `user` pointer), so these tests share no global state.

use std::ffi::{c_void, CString};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{mpsc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::{
    ov_session_begin, ov_session_create, ov_session_free, ov_session_pump_relay,
    ov_session_relay_connect, ov_session_relay_connect_stream, ov_session_relay_eof,
    ov_session_relay_feed, ov_session_send_record, ov_session_set_record_sink,
    ov_session_set_state_sink, OvSession,
};

#[derive(Default)]
struct Collector {
    established: bool,
    records: Vec<(u8, Vec<u8>)>,
    /// Outbound relay-stream chunks (bridge mode only).
    stream_out: Vec<Vec<u8>>,
}

type Shared = Mutex<Collector>;

fn col(user: *mut c_void) -> &'static Shared {
    // SAFETY: `user` is a leaked `Box<Shared>` that outlives every session.
    unsafe { &*(user as *const Shared) }
}

extern "C" fn record_sink(user: *mut c_void, channel: u8, data: *const u8, len: usize) {
    let bytes = unsafe { std::slice::from_raw_parts(data, len) }.to_vec();
    col(user).lock().unwrap().records.push((channel, bytes));
}

extern "C" fn state_sink(user: *mut c_void, state: i32, _fp: *const u8, _fp_len: usize) {
    if state == 1 {
        col(user).lock().unwrap().established = true;
    }
}

extern "C" fn stream_sink(user: *mut c_void, data: *const u8, len: usize) {
    let bytes = unsafe { std::slice::from_raw_parts(data, len) }.to_vec();
    col(user).lock().unwrap().stream_out.push(bytes);
}

fn start_relay() -> SocketAddr {
    let relay = ovrelay::Relay::bind("127.0.0.1:0".parse().unwrap()).expect("bind relay");
    let addr = relay.local_addr();
    thread::spawn(move || {
        let _ = relay.serve(None);
    });
    addr
}

/// Create a session with record/state sinks wired to a fresh collector and
/// the pairing-code handshake begun.
fn new_session(is_server: bool, code: &str) -> (*mut OvSession, *mut c_void) {
    let code = CString::new(code).unwrap();
    let user = Box::into_raw(Box::new(Shared::default())) as *mut c_void;
    let s = unsafe { ov_session_create(i32::from(is_server), code.as_ptr()) };
    assert!(!s.is_null());
    unsafe {
        assert_eq!(ov_session_set_record_sink(s, user, record_sink), 0);
        assert_eq!(ov_session_set_state_sink(s, user, state_sink), 0);
        assert_eq!(ov_session_begin(s, 0), 0);
    }
    (s, user)
}

/// Minimal WebSocket client "bridge": the socket after the 101, plus a
/// reader thread that forwards each inbound binary message's payload.
struct WsBridge {
    sock: TcpStream,
    inbound: mpsc::Receiver<Option<Vec<u8>>>,
}

impl WsBridge {
    fn open(addr: SocketAddr) -> Self {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_nodelay(true).unwrap();
        s.write_all(
            b"GET /relay HTTP/1.1\r\nHost: openview.test\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
        )
        .unwrap();
        let mut head = Vec::new();
        let mut b = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            s.read_exact(&mut b).unwrap();
            head.push(b[0]);
        }
        assert!(head.starts_with(b"HTTP/1.1 101"));
        let (tx, rx) = mpsc::channel();
        let mut r = s.try_clone().unwrap();
        thread::spawn(move || loop {
            let mut h = [0u8; 2];
            if r.read_exact(&mut h).is_err() {
                let _ = tx.send(None);
                return;
            }
            let mut len = u64::from(h[1] & 0x7F);
            if len == 126 {
                let mut l = [0u8; 2];
                r.read_exact(&mut l).unwrap();
                len = u64::from(u16::from_be_bytes(l));
            } else if len == 127 {
                let mut l = [0u8; 8];
                r.read_exact(&mut l).unwrap();
                len = u64::from_be_bytes(l);
            }
            let mut p = vec![0u8; len as usize];
            if r.read_exact(&mut p).is_err() {
                let _ = tx.send(None);
                return;
            }
            match h[0] & 0x0F {
                0x2 | 0x0 => {
                    let _ = tx.send(Some(p));
                }
                0x8 => {
                    let _ = tx.send(None);
                    return;
                }
                _ => {}
            }
        });
        Self {
            sock: s,
            inbound: rx,
        }
    }

    /// One masked client binary message.
    fn send(&mut self, payload: &[u8]) {
        let mask = [0x12u8, 0x34, 0x56, 0x78];
        let mut out = vec![0x82];
        let n = payload.len();
        if n < 126 {
            out.push(0x80 | n as u8);
        } else if n <= 0xFFFF {
            out.push(0x80 | 126);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        } else {
            out.push(0x80 | 127);
            out.extend_from_slice(&(n as u64).to_be_bytes());
        }
        out.extend_from_slice(&mask);
        out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        self.sock.write_all(&out).unwrap();
    }

    /// Shuttle bytes both ways between the session and the relay: outbound
    /// chunks the session emitted, inbound messages fed into it (EOF too).
    fn shuttle(&mut self, s: *mut OvSession, user: *mut c_void) {
        let out = std::mem::take(&mut col(user).lock().unwrap().stream_out);
        for chunk in out {
            self.send(&chunk);
        }
        while let Ok(msg) = self.inbound.try_recv() {
            match msg {
                Some(p) => assert_eq!(unsafe { ov_session_relay_feed(s, p.as_ptr(), p.len()) }, 0),
                None => assert_eq!(unsafe { ov_session_relay_eof(s) }, 0),
            }
        }
    }
}

/// Pump both sessions until `done` holds or the deadline passes. `bridge`
/// shuttles the client's bytes when it uses the stream relay.
fn pump_until(
    client: (*mut OvSession, *mut c_void),
    server: (*mut OvSession, *mut c_void),
    mut bridge: Option<&mut WsBridge>,
    mut done: impl FnMut() -> bool,
) -> (i32, i32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let (mut rc, mut rs) = (0, 0);
    while !done() && Instant::now() < deadline {
        if let Some(b) = bridge.as_deref_mut() {
            b.shuttle(client.0, client.1);
        }
        rc = unsafe { ov_session_pump_relay(client.0) };
        rs = unsafe { ov_session_pump_relay(server.0) };
        if let Some(b) = bridge.as_deref_mut() {
            b.shuttle(client.0, client.1);
        }
        assert!(rc == 0 || rc == -5, "client pump rc {rc}");
        assert!(rs == 0 || rs == -5, "server pump rc {rs}");
        thread::sleep(Duration::from_millis(2));
    }
    (rc, rs)
}

fn handshake_and_exchange(
    client: (*mut OvSession, *mut c_void),
    server: (*mut OvSession, *mut c_void),
    mut bridge: Option<&mut WsBridge>,
) {
    pump_until(client, server, bridge.as_deref_mut(), || {
        col(client.1).lock().unwrap().established && col(server.1).lock().unwrap().established
    });
    assert!(
        col(client.1).lock().unwrap().established,
        "client must establish over the relay"
    );
    assert!(
        col(server.1).lock().unwrap().established,
        "server must establish over the relay"
    );

    let up = b"hello via relay";
    let down = b"and back";
    unsafe {
        assert_eq!(
            ov_session_send_record(client.0, 3, up.as_ptr(), up.len()),
            0
        );
        assert_eq!(
            ov_session_send_record(server.0, 4, down.as_ptr(), down.len()),
            0
        );
    }
    pump_until(client, server, bridge, || {
        !col(server.1).lock().unwrap().records.is_empty()
            && !col(client.1).lock().unwrap().records.is_empty()
    });
    assert_eq!(
        col(server.1).lock().unwrap().records,
        vec![(3u8, up.to_vec())]
    );
    assert_eq!(
        col(client.1).lock().unwrap().records,
        vec![(4u8, down.to_vec())]
    );
}

/// Pump `s` alone until it reports the relay closed (-5).
fn pump_until_closed(s: *mut OvSession, mut bridge: Option<(&mut WsBridge, *mut c_void)>) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some((b, user)) = bridge.as_mut() {
            b.shuttle(s, *user);
        }
        let rc = unsafe { ov_session_pump_relay(s) };
        if rc != 0 || Instant::now() >= deadline {
            return rc;
        }
        thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn ffi_relay_tcp_handshake_records_and_close() {
    let addr = start_relay();
    let host = CString::new("127.0.0.1").unwrap();
    let sid = crate::relay_session_id("246810");
    let server = new_session(true, "246810");
    let client = new_session(false, "246810");
    unsafe {
        assert_eq!(
            ov_session_relay_connect(server.0, host.as_ptr(), addr.port(), sid),
            0
        );
        assert_eq!(
            ov_session_relay_connect(client.0, host.as_ptr(), addr.port(), sid),
            0
        );
    }
    handshake_and_exchange(client, server, None);

    // The server goes away: the client's pump reports relay-closed (-5),
    // and keeps reporting it.
    unsafe { ov_session_free(server.0) };
    assert_eq!(pump_until_closed(client.0, None), -5);
    assert_eq!(unsafe { ov_session_pump_relay(client.0) }, -5);
    unsafe { ov_session_free(client.0) };
}

#[test]
fn ffi_relay_stream_bridge_handshake_records_and_close() {
    let addr = start_relay();
    let host = CString::new("127.0.0.1").unwrap();
    let sid = crate::relay_session_id("135790");
    // Host on the raw TCP relay, phone on the WebSocket byte-stream bridge.
    let server = new_session(true, "135790");
    let client = new_session(false, "135790");
    let mut ws = WsBridge::open(addr);
    unsafe {
        assert_eq!(
            ov_session_relay_connect(server.0, host.as_ptr(), addr.port(), sid),
            0
        );
        assert_eq!(
            ov_session_relay_connect_stream(client.0, client.1, stream_sink, sid),
            0
        );
    }
    handshake_and_exchange(client, server, Some(&mut ws));

    unsafe { ov_session_free(server.0) };
    assert_eq!(pump_until_closed(client.0, Some((&mut ws, client.1))), -5);
    unsafe { ov_session_free(client.0) };
}

#[test]
fn ffi_relay_stream_rejection_and_eof_report_closed() {
    // XT (rejected / timed out) fed through the bridge is relay-closed.
    let (s, user) = new_session(false, "123456");
    unsafe {
        assert_eq!(
            ov_session_relay_connect_stream(s, user, stream_sink, 0x3132_3334),
            0
        );
        let xt = [b'X', b'T', 0x31, 0x32, 0x33, 0x34];
        assert_eq!(ov_session_relay_feed(s, xt.as_ptr(), xt.len()), 0);
        assert_eq!(ov_session_pump_relay(s), -5);
        assert_eq!(ov_session_pump_relay(s), -5);
        ov_session_free(s);
    }
    // The host's stream closing (EOF) is relay-closed too.
    let (s, user) = new_session(false, "123456");
    unsafe {
        assert_eq!(
            ov_session_relay_connect_stream(s, user, stream_sink, 0x3132_3334),
            0
        );
        assert_eq!(ov_session_relay_eof(s), 0);
        assert_eq!(ov_session_pump_relay(s), -5);
        ov_session_free(s);
    }
}

// ---------------- event-driven relay receive ----------------

/// A shell-side wake flag: the wake sink sets it from the watcher thread.
#[derive(Default)]
struct WakeFlag {
    set: Mutex<u64>,
    cv: std::sync::Condvar,
}

extern "C" fn wake_sink(user: *mut c_void) {
    // SAFETY: `user` is a leaked `Box<WakeFlag>` that outlives the session.
    let w = unsafe { &*(user as *const WakeFlag) };
    *w.set.lock().unwrap() += 1;
    w.cv.notify_all();
}

/// Raw session handle moved into a sender thread (the session is
/// internally locked, so cross-thread calls are fine).
struct SendPtr(*mut OvSession);
unsafe impl Send for SendPtr {}

/// Median / max of the samples in milliseconds.
fn stats_ms(v: &mut [Duration]) -> (f64, f64) {
    v.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    (ms(v[v.len() / 2]), ms(v[v.len() - 1]))
}

/// The host sends `n` records at an odd cadence; the phone side pumps
/// either on a fixed 50 ms tick (the old iOS TCP-relay loop) or on the
/// relay wake sink (with the 50 ms tick kept as the fallback). Returns
/// the send -> delivered-after-pump latency of every record.
fn measure_relay_latency(event_driven: bool, n: usize) -> Vec<Duration> {
    let addr = start_relay();
    let host = CString::new("127.0.0.1").unwrap();
    let code = if event_driven { "424242" } else { "434343" };
    let sid = crate::relay_session_id(code);
    let server = new_session(true, code);
    let client = new_session(false, code);
    unsafe {
        assert_eq!(
            ov_session_relay_connect(server.0, host.as_ptr(), addr.port(), sid),
            0
        );
        assert_eq!(
            ov_session_relay_connect(client.0, host.as_ptr(), addr.port(), sid),
            0
        );
    }
    pump_until(client, server, None, || {
        col(client.1).lock().unwrap().established && col(server.1).lock().unwrap().established
    });
    assert!(col(client.1).lock().unwrap().established);
    let flag: &'static WakeFlag = Box::leak(Box::default());
    if event_driven {
        let rc = unsafe {
            crate::ov_session_set_wake_sink(
                client.0,
                flag as *const WakeFlag as *mut c_void,
                Some(wake_sink),
            )
        };
        assert_eq!(rc, 1, "the TCP relay socket must be watched");
    }
    col(client.1).lock().unwrap().records.clear();

    let sent: &'static Mutex<Vec<Instant>> = Box::leak(Box::default());
    let srv = SendPtr(server.0);
    let sender = thread::spawn(move || {
        let srv = srv;
        for i in 0..n as u32 {
            // 7..20 ms apart: arrivals land at every phase of the tick.
            thread::sleep(Duration::from_millis(7 + u64::from(i * 13 % 14)));
            sent.lock().unwrap().push(Instant::now());
            let p = i.to_be_bytes();
            assert_eq!(
                unsafe { ov_session_send_record(srv.0, 0, p.as_ptr(), p.len()) },
                0
            );
        }
    });

    let mut lat = Vec::with_capacity(n);
    let mut seen = 0usize;
    let deadline = Instant::now() + Duration::from_secs(20);
    while seen < n && Instant::now() < deadline {
        if event_driven {
            // Wait for a wake, or the 50 ms fallback tick.
            let g = flag.set.lock().unwrap();
            let (mut g, _) = flag
                .cv
                .wait_timeout_while(g, Duration::from_millis(50), |c| *c == 0)
                .unwrap();
            *g = 0;
        } else {
            thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(unsafe { ov_session_pump_relay(client.0) }, 0);
        let now = Instant::now();
        let recs = std::mem::take(&mut col(client.1).lock().unwrap().records);
        let sent = sent.lock().unwrap();
        for (_, p) in recs {
            let i = u32::from_be_bytes(p[..4].try_into().unwrap()) as usize;
            lat.push(now.saturating_duration_since(sent[i]));
            seen += 1;
        }
    }
    sender.join().unwrap();
    unsafe {
        crate::ov_session_set_wake_sink(client.0, std::ptr::null_mut(), None);
        ov_session_free(client.0);
        ov_session_free(server.0);
    }
    assert_eq!(lat.len(), n, "every record must arrive");
    lat
}

#[test]
fn relay_wake_sink_delivers_without_waiting_for_the_tick() {
    let n = 40;
    let (tick_med, tick_max) = stats_ms(&mut measure_relay_latency(false, n));
    let (wake_med, wake_max) = stats_ms(&mut measure_relay_latency(true, n));
    eprintln!(
        "relay arrival->pump latency over {n} records: 50 ms tick median {tick_med:.2} ms / max {tick_max:.2} ms; \
         wake sink median {wake_med:.2} ms / max {wake_max:.2} ms"
    );
    // Loopback: the wake path is a thread handoff, well under a tick.
    assert!(wake_med < 5.0, "wake-driven median {wake_med:.2} ms");
    assert!(
        wake_med < tick_med,
        "wake {wake_med:.2} ms vs tick {tick_med:.2} ms"
    );
}

#[test]
fn relay_wake_sink_is_inert_without_a_tcp_relay() {
    let (s, user) = new_session(false, "123456");
    let flag: &'static WakeFlag = Box::leak(Box::default());
    let fu = flag as *const WakeFlag as *mut c_void;
    unsafe {
        // Stored, nothing to watch yet (no transport).
        assert_eq!(crate::ov_session_set_wake_sink(s, fu, Some(wake_sink)), 0);
        // The WebSocket bridge is event driven already: still nothing to watch.
        assert_eq!(
            ov_session_relay_connect_stream(s, user, stream_sink, 0x3132_3334),
            0
        );
        assert_eq!(crate::ov_session_set_wake_sink(s, fu, Some(wake_sink)), 0);
        assert_eq!(
            crate::ov_session_set_wake_sink(std::ptr::null_mut(), fu, Some(wake_sink)),
            -1
        );
        ov_session_free(s);
    }
}
