//! The iOS LAN path (`.direct` mode) end to end over real UDP sockets: an
//! `OvSession` in datagram mode, driven exactly like
//! `SessionController+Transport.swift` drives it (datagram sink = one UDP
//! datagram, inbound datagram -> `ov_session_on_datagram` + pump, a 50 ms
//! pump tick), against a host built like `ovhost`'s LAN listener
//! (`crates/ovhost/src/lan.rs` + `session::listen_once`): the first
//! datagram is only peeked, the socket becomes a `UdpP2pCarrier` connected
//! to the viewer with the reliable-UDP framing switched on, and a PIN
//! server handshake runs on it.
//!
//! Regression for the live failure (iPhone -> host LAN port): with bare
//! datagrams one lost handshake datagram stalled the handshake ("handshake
//! timed out" on the host), and the host's first screen record — larger
//! than a UDP datagram, sent before the app's CAPS — was lost, leaving the
//! record sequence broken for good (the app saw nothing and its watchdog
//! ended the session). Here 15 % of the datagrams are dropped in both
//! directions and the host sends a 200 KB record straight after the
//! handshake.

use std::ffi::{c_void, CString};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ovcore::handshake::{Auth, Handshake, HandshakeEvent, Role};
use ovcore::identity::Identity;
use ovcore::throttle::AuthThrottle;
use ovtransport::{Carrier, UdpP2pCarrier};

use crate::{
    ov_session_begin_pin, ov_session_create, ov_session_free, ov_session_on_datagram,
    ov_session_pump, ov_session_send_record, ov_session_set_datagram_sink,
    ov_session_set_record_sink, ov_session_set_state_sink,
};

const PIN: &str = "2468";
/// Share of datagrams dropped, both directions.
const LOSS_PERCENT: u64 = 15;

/// The viewer side's sockets and observations (one test, one instance).
struct Viewer {
    sock: UdpSocket,
    rng: AtomicU64,
    established: AtomicBool,
    records: Mutex<Vec<(u8, Vec<u8>)>>,
}

impl Viewer {
    fn lose(&self) -> bool {
        let mut x = self.rng.load(Ordering::Relaxed);
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng.store(x, Ordering::Relaxed);
        x % 100 < LOSS_PERCENT
    }
}

extern "C" fn datagram_sink(user: *mut c_void, data: *const u8, len: usize) {
    let v = unsafe { &*(user as *const Viewer) };
    if v.lose() {
        return;
    }
    let d = unsafe { std::slice::from_raw_parts(data, len) };
    assert!(
        d.len() <= 1214,
        "a {} B datagram would be IP-fragmented",
        d.len()
    );
    let _ = v.sock.send(d);
}

extern "C" fn state_sink(user: *mut c_void, state: i32, _fp: *const u8, _n: usize) {
    let v = unsafe { &*(user as *const Viewer) };
    if state == 1 {
        v.established.store(true, Ordering::SeqCst);
    }
}

extern "C" fn record_sink(user: *mut c_void, ch: u8, data: *const u8, len: usize) {
    let v = unsafe { &*(user as *const Viewer) };
    let d = unsafe { std::slice::from_raw_parts(data, len) }.to_vec();
    v.records.lock().unwrap().push((ch, d));
}

/// The host: `ovhost`'s LAN path minus the screen code. Returns the records
/// it received after the handshake.
fn host(listener: UdpSocket, big: Vec<u8>, until: Instant) -> Result<Vec<Vec<u8>>, String> {
    listener.set_nonblocking(true).unwrap();
    // Wait for the viewer's first datagram; leave it queued (peek).
    let mut buf = vec![0u8; 65_536];
    let peer = loop {
        match listener.peek_from(&mut buf) {
            Ok((_, peer)) => break peer,
            Err(_) if Instant::now() < until => std::thread::sleep(Duration::from_millis(2)),
            Err(e) => return Err(format!("no HELLO: {e}")),
        }
    };
    let mut carrier = UdpP2pCarrier::from_socket(listener, peer);
    carrier.enable_reliable();
    let me = Identity::generate();
    let psk = ovcore::pin::pin_psk(PIN.as_bytes());
    let mut hs = Handshake::new(Role::Server, 4242, &me, Auth::Pin(psk));
    // A private limiter: the PIN tests drive the process-wide one.
    hs.set_throttle(Arc::new(AuthThrottle::default()));
    hs.set_auth_source(peer.ip());
    let mut got = Vec::new();
    while !hs.is_established() {
        if let Some(e) = hs.failed() {
            return Err(format!("host handshake failed: {e}"));
        }
        if Instant::now() > until {
            return Err("host: handshake timed out".into());
        }
        carrier
            .poll_recv(&mut |d| {
                if hs.is_established() {
                    got.push(d.to_vec());
                } else {
                    let _ = hs.feed(d);
                }
            })
            .map_err(|e| e.to_string())?;
        for ev in hs.take_events() {
            let HandshakeEvent::Sent(d) = ev;
            carrier.send(&d).map_err(|e| e.to_string())?;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let mut session = hs.take_session().ok_or("no session")?;
    // Straight away, before any CAPS from the viewer: a keyframe-sized
    // record, then a small one behind it.
    carrier
        .send(&session.encrypt_record(0, &big))
        .map_err(|e| e.to_string())?;
    carrier
        .send(&session.encrypt_record(1, b"after-the-big-one"))
        .map_err(|e| e.to_string())?;
    let mut plain = Vec::new();
    while Instant::now() < until {
        carrier
            .poll_recv(&mut |d| got.push(d.to_vec()))
            .map_err(|e| e.to_string())?;
        for d in got.drain(..) {
            if let Ok(p) = session.decrypt_record(1, &d) {
                plain.push(p);
            }
        }
        if !plain.is_empty() && carrier.pending_send() == 0 {
            // Keep servicing ACKs / retransmissions a little longer.
            std::thread::sleep(Duration::from_millis(300));
            carrier.poll_recv(&mut |_| {}).map_err(|e| e.to_string())?;
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Ok(plain)
}

#[test]
fn ios_datagram_mode_survives_a_lossy_lan_to_a_host_lan_port() {
    let listener = UdpSocket::bind("127.0.0.1:0").unwrap();
    let host_addr: SocketAddr = listener.local_addr().unwrap();
    let big: Vec<u8> = (0..200_000u32).map(|i| (i * 7 + i / 251) as u8).collect();
    let deadline = Instant::now() + Duration::from_secs(20);
    let host_big = big.clone();
    let host = std::thread::spawn(move || host(listener, host_big, deadline));

    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.connect(host_addr).unwrap();
    sock.set_nonblocking(true).unwrap();
    let viewer = Box::new(Viewer {
        sock,
        rng: AtomicU64::new(0x2545_F491_4F6C_DD1D),
        established: AtomicBool::new(false),
        records: Mutex::new(Vec::new()),
    });
    let user = &*viewer as *const Viewer as *mut c_void;
    let code = CString::new("000000").unwrap();
    let s = unsafe { ov_session_create(0, code.as_ptr()) };
    assert!(!s.is_null());
    unsafe {
        assert_eq!(ov_session_set_datagram_sink(s, user, datagram_sink), 0);
        assert_eq!(ov_session_set_state_sink(s, user, state_sink), 0);
        assert_eq!(ov_session_set_record_sink(s, user, record_sink), 0);
        assert_eq!(ov_session_begin_pin(s, PIN.as_ptr(), PIN.len()), 0);
    }

    let mut buf = vec![0u8; 65_536];
    let mut next_tick = Instant::now();
    let mut sent_reply = false;
    let mut got_big = false;
    while Instant::now() < deadline && !(got_big && sent_reply && host.is_finished()) {
        // NwUdpBridge.receiveLoop -> onDatagram -> (batched) pump.
        let mut any = false;
        while let Ok(n) = viewer.sock.recv(&mut buf) {
            if viewer.lose() {
                continue;
            }
            unsafe { ov_session_on_datagram(s, buf.as_ptr(), n) };
            any = true;
        }
        let tick = Instant::now() >= next_tick;
        if tick {
            next_tick += Duration::from_millis(50);
        }
        if any || tick {
            let rc = unsafe { ov_session_pump(s) };
            assert_eq!(rc, 0, "viewer pump failed");
        }
        let (big_seen, after_seen) = {
            let recs = viewer.records.lock().unwrap();
            (
                recs.iter().any(|(ch, d)| *ch == 0 && *d == big),
                recs.iter()
                    .any(|(ch, d)| *ch == 1 && d == b"after-the-big-one"),
            )
        };
        got_big = big_seen;
        // Reply only once everything the host sent has arrived: the host
        // stops retransmitting when it sees the reply, so replying earlier
        // could strand a lost trailing record.
        if big_seen && after_seen && !sent_reply {
            let msg = b"viewer-hello";
            let rc = unsafe { ov_session_send_record(s, 1, msg.as_ptr(), msg.len()) };
            assert_eq!(rc, 0);
            sent_reply = true;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        viewer.established.load(Ordering::SeqCst),
        "the PIN handshake completes despite lost datagrams"
    );
    let records = viewer.records.lock().unwrap().clone();
    assert!(
        got_big,
        "the 200 KB record arrives intact (got {} records)",
        records.len()
    );
    assert!(
        records
            .iter()
            .any(|(ch, d)| *ch == 1 && d == b"after-the-big-one"),
        "records behind it arrive too"
    );
    // Give the host the rest of its time to collect our record.
    let from_viewer = host.join().expect("host thread").expect("host side");
    assert_eq!(from_viewer, vec![b"viewer-hello".to_vec()]);
    unsafe { ov_session_free(s) };
}
