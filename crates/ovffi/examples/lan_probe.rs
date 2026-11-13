//! Drive an `OvSession` + `OvScreen` in datagram (`.direct`) mode exactly
//! like the iOS app does (`SessionController+Transport.swift`), against a
//! real host's LAN port, and report what arrives.
//!
//! ```text
//! cargo run -p ovffi --example lan_probe -- 127.0.0.1:24601 <PIN> [seconds]
//! ```
//!
//! Swift equivalents: the datagram sink is `NwUdpBridge.send` (one UDP
//! datagram per call), inbound datagrams go to `ov_session_on_datagram`
//! followed by a pump, a 50 ms tick pumps, runs the keepalive and sends at
//! most one queued small control record (`sendPongIfOwed`), and channel-0
//! records are fed to the screen assembler.
//!
//! `PROBE_LOSS=<percent>` drops datagrams in both directions;
//! `PROBE_TRACE=1` prints every outbound datagram's size and first bytes.

use std::ffi::{c_void, CString};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ovffi::*;

static SOCK: std::sync::OnceLock<UdpSocket> = std::sync::OnceLock::new();
static ESTABLISHED: AtomicBool = AtomicBool::new(false);
static RECORDS: AtomicU64 = AtomicU64::new(0);
static FRAMES: AtomicU64 = AtomicU64::new(0);
static SENT: AtomicU64 = AtomicU64::new(0);
static MAX_OUT: AtomicU64 = AtomicU64::new(0);
static SCREEN: AtomicU64 = AtomicU64::new(0);
static FRAMED_IN: AtomicU64 = AtomicU64::new(0);
static RAW_IN: AtomicU64 = AtomicU64::new(0);

/// `PROBE_LOSS=<percent>`: drop that share of datagrams in both
/// directions (deterministic xorshift), like a lossy Wi-Fi link.
fn lose() -> bool {
    static RNG: AtomicU64 = AtomicU64::new(0x9E37_79B9_7F4A_7C15);
    let pct: u64 = std::env::var("PROBE_LOSS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if pct == 0 {
        return false;
    }
    let mut x = RNG.load(Ordering::Relaxed);
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    RNG.store(x, Ordering::Relaxed);
    x % 100 < pct
}

extern "C" fn datagram_sink(_u: *mut c_void, data: *const u8, len: usize) {
    // SAFETY: valid for the call (ovffi contract).
    let d = unsafe { std::slice::from_raw_parts(data, len) };
    SENT.fetch_add(1, Ordering::Relaxed);
    if lose() {
        return;
    }
    if std::env::var_os("PROBE_TRACE").is_some() {
        eprintln!("out {} bytes {:02x?}", d.len(), &d[..d.len().min(8)]);
    }
    if let Some(s) = SOCK.get() {
        let _ = s.send(d);
    }
}

extern "C" fn state_sink(_u: *mut c_void, state: i32, _fp: *const u8, _l: usize) {
    if state == 1 {
        ESTABLISHED.store(true, Ordering::SeqCst);
    }
}

extern "C" fn record_sink(_u: *mut c_void, ch: u8, data: *const u8, len: usize) {
    RECORDS.fetch_add(1, Ordering::Relaxed);
    if ch == 0 {
        let screen = SCREEN.load(Ordering::Relaxed) as *mut OvScreen;
        // SAFETY: the screen outlives the session loop; same thread.
        unsafe { ov_screen_feed(screen, data, len) };
    }
}

#[allow(clippy::too_many_arguments)]
extern "C" fn frame_sink(
    _u: *mut c_void,
    _seq: u64,
    _w: u32,
    _h: u32,
    _stride: u32,
    _fmt: u8,
    _px: *const u8,
    _len: usize,
) {
    FRAMES.fetch_add(1, Ordering::Relaxed);
}

fn main() {
    let mut args = std::env::args().skip(1);
    let host: SocketAddr = args.next().expect("host:port").parse().expect("host:port");
    let pin = args.next().expect("PIN");
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(8);

    let sock = UdpSocket::bind("0.0.0.0:0").expect("bind");
    sock.connect(host).expect("connect");
    sock.set_nonblocking(true).unwrap();
    SOCK.set(sock).unwrap();

    let code = CString::new("000000").unwrap();
    // SAFETY: plain C API use, single thread.
    unsafe {
        let screen = ov_screen_new();
        SCREEN.store(screen as u64, Ordering::Relaxed);
        ov_screen_set_frame_sink(screen, std::ptr::null_mut(), frame_sink);
        let s = ov_session_create(0, code.as_ptr());
        assert!(!s.is_null());
        ov_session_set_datagram_sink(s, std::ptr::null_mut(), datagram_sink);
        ov_session_set_state_sink(s, std::ptr::null_mut(), state_sink);
        ov_session_set_record_sink(s, std::ptr::null_mut(), record_sink);
        assert_eq!(ov_session_begin_pin(s, pin.as_ptr(), pin.len()), 0);
        let start = Instant::now();
        let mut first_rc = 0;
        let mut next_tick = Instant::now();
        let mut buf = vec![0u8; 65536];
        let mut est_at = None;
        while start.elapsed() < Duration::from_secs(secs) {
            let mut got = false;
            while let Ok(n) = SOCK.get().unwrap().recv(&mut buf) {
                if lose() {
                    continue;
                }
                MAX_OUT.fetch_max(n as u64, Ordering::Relaxed);
                if buf[..n].starts_with(&[0xB7, b'R', b'U', b'1']) {
                    FRAMED_IN.fetch_add(1, Ordering::Relaxed);
                } else {
                    RAW_IN.fetch_add(1, Ordering::Relaxed);
                }
                ov_session_on_datagram(s, buf.as_ptr(), n);
                got = true;
            }
            if got {
                let rc = ov_session_pump(s);
                if rc != 0 && first_rc == 0 {
                    first_rc = rc;
                    eprintln!("pump rc {rc} at {:?}", start.elapsed());
                }
            }
            if Instant::now() >= next_tick {
                next_tick += Duration::from_millis(50);
                let rc = ov_session_pump(s);
                if rc != 0 && first_rc == 0 {
                    first_rc = rc;
                    eprintln!("pump rc {rc} at {:?}", start.elapsed());
                }
                let now_ms = start.elapsed().as_millis() as i64;
                let seq = ov_screen_keepalive_tick(screen, now_ms);
                if seq > 0 {
                    let mut ping = vec![0x10u8];
                    ping.extend_from_slice(&(seq as u64).to_be_bytes());
                    ov_session_send_record(s, 0, ping.as_ptr(), ping.len());
                }
                let mut rec = [0u8; 9];
                if ov_screen_take_pong_reply(screen, rec.as_mut_ptr(), rec.len()) > 0 {
                    ov_session_send_record(s, 0, rec.as_ptr(), rec.len());
                }
            }
            if est_at.is_none() && ESTABLISHED.load(Ordering::SeqCst) {
                est_at = Some(start.elapsed());
                eprintln!("established at {:?}", start.elapsed());
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        println!(
            "established={} records={} frames={} sent={} largest_in={} rc={} framed_in={} raw_in={}",
            ESTABLISHED.load(Ordering::SeqCst),
            RECORDS.load(Ordering::Relaxed),
            FRAMES.load(Ordering::Relaxed),
            SENT.load(Ordering::Relaxed),
            MAX_OUT.load(Ordering::Relaxed),
            first_rc,
            FRAMED_IN.load(Ordering::Relaxed),
            RAW_IN.load(Ordering::Relaxed)
        );
        ov_session_free(s);
        ov_screen_free(screen);
    }
}
