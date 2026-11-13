//! FFI round-trip: two in-process sessions complete a pairing-code handshake
//! and exchange an encrypted record, entirely through the C entry points with
//! sinks routed through queues (no direct cross-session calls — that would
//! deadlock on the session mutex).

use std::ffi::c_void;
use std::sync::Mutex;

use crate::{
    ov_session_begin, ov_session_create, ov_session_free, ov_session_on_datagram, ov_session_pump,
    ov_session_send_record, ov_session_set_datagram_sink, ov_session_set_record_sink,
    ov_session_set_state_sink,
};

static mut QUEUES: Option<Queues> = None;

struct Queues {
    /// (from, bytes) — 0 = client, 1 = server.
    wire: Vec<(u8, Vec<u8>)>,
    records: Vec<(u8, u8, Vec<u8>)>, // (from, channel, payload)
    established: Vec<u8>,
}

fn q() -> &'static mut Queues {
    #[allow(static_mut_refs)]
    unsafe {
        QUEUES.get_or_insert(Queues {
            wire: Vec::new(),
            records: Vec::new(),
            established: Vec::new(),
        })
    }
}

extern "C" fn datagram_sink(user: *mut c_void, data: *const u8, len: usize) {
    let from = user as u8;
    let bytes = unsafe { std::slice::from_raw_parts(data, len) }.to_vec();
    q().wire.push((from, bytes));
}

extern "C" fn record_sink(user: *mut c_void, channel: u8, data: *const u8, len: usize) {
    let from = user as u8;
    let bytes = unsafe { std::slice::from_raw_parts(data, len) }.to_vec();
    q().records.push((from, channel, bytes));
}

extern "C" fn state_sink(user: *mut c_void, state: i32, _fp: *const u8, _fp_len: usize) {
    if state == 1 {
        q().established.push(user as u8);
    }
}

fn pump(side: u8, s: *mut crate::OvSession) -> i32 {
    let r = unsafe { ov_session_pump(s) };
    // Deliver everything this side emitted to the other side's inbound queue.
    let out: Vec<Vec<u8>> = q()
        .wire
        .iter()
        .filter(|(f, _)| *f == side)
        .map(|(_, b)| b.clone())
        .collect();
    q().wire.retain(|(f, _)| *f != side);
    let peer = if side == 0 { 1 } else { 0 };
    let peer_ptr = if peer == 0 {
        CLIENT.load()
    } else {
        SERVER.load()
    };
    for d in out {
        unsafe { ov_session_on_datagram(peer_ptr, d.as_ptr(), d.len()) };
    }
    r
}

// The two session pointers, stashed by the test (statics can't be captured).
static CLIENT: Slot = Slot::new();
static SERVER: Slot = Slot::new();
struct Slot(Mutex<usize>);
impl Slot {
    const fn new() -> Self {
        Self(Mutex::new(0))
    }
    fn store(&self, p: *mut crate::OvSession) {
        *self.0.lock().unwrap() = p as usize;
    }
    fn load(&self) -> *mut crate::OvSession {
        *self.0.lock().unwrap() as *mut crate::OvSession
    }
}

#[test]
fn ffi_two_peer_roundtrip_pairing_code_and_record() {
    let code = std::ffi::CString::new("123456").unwrap();
    // Client presents the server's code; server validates against its own.
    let client = unsafe { ov_session_create(0, code.as_ptr()) };
    let server = unsafe { ov_session_create(1, code.as_ptr()) };
    assert!(!client.is_null() && !server.is_null());
    CLIENT.store(client);
    SERVER.store(server);

    unsafe {
        assert_eq!(
            ov_session_set_datagram_sink(client, std::ptr::null_mut::<c_void>(), datagram_sink),
            0
        );
        assert_eq!(
            ov_session_set_datagram_sink(server, std::ptr::dangling_mut::<c_void>(), datagram_sink),
            0
        );
        assert_eq!(
            ov_session_set_record_sink(client, std::ptr::null_mut::<c_void>(), record_sink),
            0
        );
        assert_eq!(
            ov_session_set_record_sink(server, std::ptr::dangling_mut::<c_void>(), record_sink),
            0
        );
        assert_eq!(
            ov_session_set_state_sink(client, std::ptr::null_mut::<c_void>(), state_sink),
            0
        );
        assert_eq!(
            ov_session_set_state_sink(server, std::ptr::dangling_mut::<c_void>(), state_sink),
            0
        );
        assert_eq!(ov_session_begin(client, 0), 0);
        assert_eq!(ov_session_begin(server, 0), 0);
    }

    // A few pump rounds complete the 2-message handshake.
    for i in 0..10 {
        let a = pump(0, client);
        let b = pump(1, server);
        let mut fpc = [0u8; 16];
        let rc = unsafe { crate::ov_session_peer_fingerprint(client, fpc.as_mut_ptr()) };
        let mut fps = [0u8; 16];
        let rs = unsafe { crate::ov_session_peer_fingerprint(server, fps.as_mut_ptr()) };
        eprintln!("dbg pump a={a} b={b} rc={rc} rs={rs}");
        let _ = (a, b);
        if q().established.len() == 2 {
            break;
        }
        if i >= 0 {
            eprintln!(
                "i={i} rcs {a}/{b} wire={} est={}",
                q().wire.len(),
                q().established.len()
            );
            eprintln!("pump rcs: client={a} server={b} wire={:?}", q().wire.len());
        }
    }
    assert_eq!(
        q().established.len(),
        2,
        "both sides must reach established"
    );

    // Client -> server record on channel 1.
    let msg = b"hello from ios";
    let r = unsafe { ov_session_send_record(client, 1, msg.as_ptr(), msg.len()) };
    assert_eq!(r, 0);
    pump(0, client);
    pump(1, server);

    let got = q()
        .records
        .iter()
        .find(|(f, ch, p)| *f == 1 && *ch == 1 && p == &msg.to_vec());
    assert!(got.is_some(), "server must receive the decrypted record");
}

#[test]
fn ffi_null_and_state_errors() {
    let code = std::ffi::CString::new("654321").unwrap();
    assert!(unsafe { ov_session_create(0, std::ptr::null()) }.is_null());
    unsafe { ov_session_free(std::ptr::null_mut()) };
    assert_eq!(unsafe { ov_session_pump(std::ptr::null_mut()) }, -1);
    let s = unsafe { ov_session_create(0, code.as_ptr()) };
    assert!(!s.is_null());
    // Sending a record before the handshake is a wrong-state error.
    let msg = b"x";
    assert_eq!(
        unsafe { ov_session_send_record(s, 1, msg.as_ptr(), msg.len()) },
        -2
    );
    unsafe { ov_session_free(s) };
}

#[test]
fn screen_cursor_sinks_fire_on_cursor_commands() {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex;

    static SHAPE_CALLS: AtomicU32 = AtomicU32::new(0);
    static POS_CALLS: AtomicU32 = AtomicU32::new(0);
    static LAST_POS: Mutex<(i32, i32)> = Mutex::new((0, 0));

    extern "C" fn on_cursor(_user: *mut std::ffi::c_void, x: i32, y: i32) {
        *LAST_POS.lock().unwrap() = (x, y);
        POS_CALLS.fetch_add(1, Ordering::Relaxed);
    }
    extern "C" fn on_shape(
        user: *mut std::ffi::c_void,
        _hx: u16,
        _hy: u16,
        _w: u16,
        _h: u16,
        bgra: *const u8,
        len: usize,
    ) {
        unsafe {
            assert!(!bgra.is_null() && len == 16);
            let slot = &*(user as *mut Mutex<usize>);
            *slot.lock().unwrap() = len;
            SHAPE_CALLS.fetch_add(1, Ordering::Relaxed);
        }
    }

    let len_box = Box::into_raw(Box::new(Mutex::new(0usize)));
    let screen = crate::ov_screen_new();
    unsafe {
        crate::ov_screen_set_cursor_sink(screen, std::ptr::null_mut(), Some(on_cursor));
        crate::ov_screen_set_cursor_shape_sink(screen, len_box.cast(), Some(on_shape));
        assert_eq!(
            crate::ov_screen_set_cursor_sink(std::ptr::null_mut(), std::ptr::null_mut(), None),
            -1
        );

        // Position command (0x32, two i32 LE): decoded and accepted.
        let pos_cmd = [0x32u8, 10, 0, 0, 0, 250, 255, 255, 255]; // x=10, y=-6
        assert_eq!(
            crate::ov_screen_feed(screen, pos_cmd.as_ptr(), pos_cmd.len()),
            0
        );
        assert_eq!(POS_CALLS.load(Ordering::Relaxed), 1);
        assert_eq!(*LAST_POS.lock().unwrap(), (10, -6));

        // Shape command (0x33): hotspot 1x2, 2x2 bitmap.
        let mut shape_cmd = vec![0x33u8, 1, 0, 2, 0, 2, 0, 2, 0];
        shape_cmd.extend_from_slice(&[9u8; 16]);
        assert_eq!(
            crate::ov_screen_feed(screen, shape_cmd.as_ptr(), shape_cmd.len()),
            0
        );
        assert_eq!(*(*len_box).lock().unwrap(), 16);
        assert_eq!(SHAPE_CALLS.load(Ordering::Relaxed), 1);

        // A malformed shape (bgra not w*h*4) is rejected by decode.
        let bad = vec![0x33u8, 0, 0, 0, 0, 2, 0, 2, 0, 1, 2, 3];
        assert_eq!(crate::ov_screen_feed(screen, bad.as_ptr(), bad.len()), -1);

        crate::ov_screen_free(screen);
        drop(Box::from_raw(len_box));
    }
}

/// A shell clock far ahead of a fresh screen (controller outlived an
/// earlier connection): live inbound traffic must still read as alive.
#[test]
fn liveness_survives_shell_clock_skew_after_reconnect() {
    use std::sync::Mutex;
    static ALIVE: Mutex<i32> = Mutex::new(-2);
    extern "C" fn on_liveness(_user: *mut c_void, alive: i32, _rtt: i32, _fps: f32) {
        *ALIVE.lock().unwrap() = alive;
    }
    unsafe {
        let screen = crate::ov_screen_new();
        assert_eq!(
            crate::ov_screen_set_liveness_sink(screen, std::ptr::null_mut(), Some(on_liveness)),
            0
        );
        // The shell's clock already reads 10 minutes (previous connection).
        let skew = 600_000i64;
        assert_eq!(crate::ov_screen_liveness_tick(screen, skew), 0);
        assert_eq!(*ALIVE.lock().unwrap(), 1, "fresh screen is alive");
        let ping = {
            let mut b = vec![0x10u8];
            b.extend_from_slice(&7u64.to_be_bytes());
            b
        };
        assert_eq!(crate::ov_screen_feed(screen, ping.as_ptr(), ping.len()), 0);
        assert_eq!(crate::ov_screen_liveness_tick(screen, skew + 50), 0);
        assert_eq!(
            *ALIVE.lock().unwrap(),
            1,
            "inbound traffic keeps it alive despite the skew"
        );
        crate::ov_screen_free(screen);
    }
}

#[test]
fn screen_liveness_ticks_pong_reply_and_rtt() {
    use std::sync::Mutex;

    static LAST: Mutex<(i32, i32)> = Mutex::new((-2, -2)); // (alive, rtt_ms)
    extern "C" fn on_liveness(user: *mut c_void, alive: i32, rtt_ms: i32, _fps: f32) {
        *LAST.lock().unwrap() = (alive, rtt_ms);
        let _ = user;
    }

    unsafe {
        let screen = crate::ov_screen_new();
        assert_eq!(
            crate::ov_screen_set_liveness_sink(screen, std::ptr::null_mut(), Some(on_liveness)),
            0
        );

        // No keepalive is due immediately after construction.
        assert_eq!(crate::ov_screen_keepalive_tick(screen, 0), 0);

        // After the 5s interval the shell gets a PING sequence to send.
        let seq = crate::ov_screen_keepalive_tick(screen, 5_000);
        assert!(seq > 0, "keepalive due at 5s");

        // No PONG yet: 16s past the seeded construction instant the liveness
        // sink reports dead (no inbound traffic) and RTT -1 (never measured).
        assert_eq!(crate::ov_screen_liveness_tick(screen, 16_000), 0);
        assert_eq!(*LAST.lock().unwrap(), (0, -1));

        // The peer echoes the PING: feed a PONG wire record (0x11 + nonce).
        let mut pong = vec![0x11u8];
        pong.extend_from_slice(&seq.to_be_bytes());
        assert_eq!(crate::ov_screen_feed(screen, pong.as_ptr(), pong.len()), 0);
        assert_eq!(crate::ov_screen_liveness_tick(screen, 6_020), 0);
        let (alive, rtt) = *LAST.lock().unwrap();
        assert_eq!(alive, 1, "inbound PONG revives liveness");
        // RTT is real-clock (sub-millisecond in-process), so it must be a
        // non-negative measurement — not the -1 "never measured" sentinel.
        assert!(rtt >= 0, "PONG measured an RTT (got {rtt})");

        // The peer PINGs us: the shell takes the 9-byte PONG echo reply.
        let ping = {
            let mut b = vec![0x10u8];
            b.extend_from_slice(&0xAB_u64.to_be_bytes());
            b
        };
        assert_eq!(crate::ov_screen_feed(screen, ping.as_ptr(), ping.len()), 0);
        let mut out = [0u8; 9];
        assert_eq!(
            crate::ov_screen_take_pong_reply(screen, out.as_mut_ptr(), out.len()),
            1
        );
        assert_eq!(out[0], 0x11);
        assert_eq!(&out[1..], &0xAB_u64.to_be_bytes());
        // Drained: a second take reports nothing pending (0) — even with a
        // too-small buffer, since there is nothing to copy.
        assert_eq!(
            crate::ov_screen_take_pong_reply(screen, out.as_mut_ptr(), 8),
            0
        );

        // Null-handle errors.
        assert_eq!(crate::ov_screen_liveness_tick(std::ptr::null_mut(), 0), -1);
        assert_eq!(crate::ov_screen_keepalive_tick(std::ptr::null_mut(), 0), 0);
        assert_eq!(
            crate::ov_screen_take_pong_reply(std::ptr::null_mut(), out.as_mut_ptr(), 9),
            -1
        );

        crate::ov_screen_free(screen);
    }
}

#[test]
fn screen_file_sink_fires_on_completed_transfer() {
    use std::sync::Mutex;

    static CALLS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    #[allow(dead_code)]
    static GOT: Mutex<Option<(String, Vec<u8>)>> = Mutex::new(None);

    extern "C" fn on_file(
        user: *mut std::ffi::c_void,
        name: *const std::ffi::c_char,
        data: *const u8,
        len: usize,
    ) {
        unsafe {
            let name = std::ffi::CStr::from_ptr(name)
                .to_string_lossy()
                .into_owned();
            let bytes = std::slice::from_raw_parts(data, len).to_vec();
            *(&*(user as *mut Mutex<Option<(String, Vec<u8>)>>))
                .lock()
                .unwrap() = Some((name, bytes));
            CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    let got_box: *mut Mutex<Option<(String, Vec<u8>)>> = Box::into_raw(Box::new(Mutex::new(None)));
    let screen = crate::ov_screen_new();
    unsafe {
        crate::ov_screen_set_file_sink(screen, got_box.cast(), Some(on_file));
        assert_eq!(
            crate::ov_screen_set_file_sink(std::ptr::null_mut(), std::ptr::null_mut(), None),
            -1
        );

        // Build the wire form of a two-chunk transfer of "hello-file" named
        // "notes.txt" via the pure module, then feed it record by record.
        let mut xfer = ovapp::file_transfer::FileTransfer::new();
        let payload = b"hello-file".to_vec();
        for cmd in xfer
            .send_commands("notes.txt", &payload, 4)
            .expect("send_commands")
        {
            let wire = ovapp::terminal::encode(&cmd);
            assert_eq!(crate::ov_screen_feed(screen, wire.as_ptr(), wire.len()), 0);
        }
        assert_eq!(CALLS.load(std::sync::atomic::Ordering::Relaxed), 1);
        let (name, bytes) = (*got_box).lock().unwrap().clone().expect("sink fired");
        assert_eq!(name, "notes.txt");
        assert_eq!(bytes, payload);

        // A non-file command must not touch the file path.
        let ping = ovapp::terminal::encode(&ovapp::terminal::Command::Ping(7));
        assert_eq!(crate::ov_screen_feed(screen, ping.as_ptr(), ping.len()), 0);
        assert_eq!(CALLS.load(std::sync::atomic::Ordering::Relaxed), 1);

        // A path-y peer name is sanitized before it reaches the sink.
        let mut xfer2 = ovapp::file_transfer::FileTransfer::new();
        for cmd in xfer2
            .send_commands(r"..\evil.exe", b"x", 0)
            .expect("send_commands")
        {
            let wire = ovapp::terminal::encode(&cmd);
            assert_eq!(crate::ov_screen_feed(screen, wire.as_ptr(), wire.len()), 0);
        }
        assert_eq!(CALLS.load(std::sync::atomic::Ordering::Relaxed), 2);
        let (name, _) = (*got_box).lock().unwrap().clone().expect("second sink");
        assert_eq!(name, "evil.exe");

        crate::ov_screen_free(screen);
        drop(Box::from_raw(got_box));
    }
}

#[test]
fn file_send_produces_reassemblable_channel0_records() {
    // `ov_file_send` cannot run without an established session + transport,
    // but the exact record stream it emits (begin-less chunk records on
    // channel 0) must be the same one the desktop shells send. Drive the
    // shared module the same way the FFI does and verify round-trip.
    let mut xfer = ovapp::file_transfer::FileTransfer::new();
    let payload: Vec<u8> = (0..=255u8).cycle().take(20_000).collect();
    let mut records: Vec<Vec<u8>> = Vec::new();
    xfer.send("big.bin", &payload, 0, |cmd| {
        records.push(ovapp::terminal::encode(cmd));
        Ok::<(), String>(())
    })
    .expect("send");
    // 20_000 / 16 KiB = 2 chunks + completion.
    assert_eq!(records.len(), 3);
    let mut rx = ovapp::file_transfer::FileTransfer::new();
    for wire in &records {
        let cmd = ovapp::terminal::decode(wire).expect("decode");
        rx.poll(&cmd).expect("poll");
    }
    let got = rx.take().expect("completed");
    assert_eq!(got.name, "big.bin");
    assert_eq!(got.data, payload);
}

/// The iOS assembler applies full frames and dirty-tile deltas onto
/// its persistent framebuffer and always hands the shell the WHOLE frame
/// through the unchanged frame sink; CAPS and KEYFRAME_REQUEST ride the
/// existing 9-byte reply path.
#[test]
fn screen_applies_tile_deltas_and_delivers_full_frames() {
    use ovapp::sendq::{FrameMailbox, SubmitOutcome, UpdateEncoder};
    use ovapp::terminal::{encode, Command, LOCAL_CAPS};
    use std::sync::Mutex;

    /// (seq, w, h, stride, format, pixels) per delivered frame.
    type Delivered = (u64, u32, u32, u32, u8, Vec<u8>);
    static FRAMES: Mutex<Vec<Delivered>> = Mutex::new(Vec::new());
    extern "C" fn on_frame(
        _user: *mut c_void,
        seq: u64,
        w: u32,
        h: u32,
        stride: u32,
        format: u8,
        px: *const u8,
        len: usize,
    ) {
        let px = unsafe { std::slice::from_raw_parts(px, len) }.to_vec();
        FRAMES.lock().unwrap().push((seq, w, h, stride, format, px));
    }

    fn feed(screen: *mut crate::OvScreen, cmd: &Command) -> i32 {
        let wire = encode(cmd);
        unsafe { crate::ov_screen_feed(screen, wire.as_ptr(), wire.len()) }
    }
    /// Stream one queued update as the host would; returns FRAME_END's rc.
    fn stream(screen: *mut crate::OvScreen, f: &ovapp::sendq::EncodedFrame) -> i32 {
        assert_eq!(feed(screen, &f.begin()), 0);
        for c in f.data.chunks(1000) {
            assert_eq!(feed(screen, &Command::FrameData(c.to_vec())), 0);
        }
        feed(screen, &Command::FrameEnd(f.seq))
    }
    fn take_reply(screen: *mut crate::OvScreen) -> Option<[u8; 9]> {
        let mut out = [0u8; 9];
        match unsafe { crate::ov_screen_take_pong_reply(screen, out.as_mut_ptr(), 9) } {
            1 => Some(out),
            0 => None,
            rc => panic!("take_pong_reply rc {rc}"),
        }
    }

    let (w, h) = (200u32, 130u32);
    let mut px: Vec<u8> = (0..w * h * 4).map(|i| (i % 253) as u8 | 1).collect();
    let mb = FrameMailbox::default();
    let mut enc = UpdateEncoder::new();
    enc.set_peer_caps(LOCAL_CAPS);
    unsafe {
        let screen = crate::ov_screen_new();
        assert_eq!(
            crate::ov_screen_set_frame_sink(screen, std::ptr::null_mut(), on_frame),
            0
        );
        // No screen traffic yet: nothing owed.
        assert!(take_reply(screen).is_none());

        // Keyframe.
        assert!(matches!(
            enc.submit(&px, w, h, None, &mb),
            SubmitOutcome::Full { .. }
        ));
        assert_eq!(stream(screen, &mb.take().unwrap()), 0);
        // The first screen command queued our CAPS (9 bytes, opcode 0x60).
        let caps = take_reply(screen).expect("CAPS queued");
        assert_eq!(
            ovapp::terminal::decode(&caps).unwrap(),
            Command::Caps {
                flags: LOCAL_CAPS | ovapp::terminal::CAP_LZ4 | ovapp::terminal::VIEWER_W2_CAPS
            }
        );
        assert!(take_reply(screen).is_none(), "CAPS goes out once");
        // The host's CAPS reply arrives; the shell can read it (input v2).
        assert_eq!(crate::ov_screen_peer_caps(screen), 0, "no host CAPS yet");
        assert_eq!(feed(screen, &Command::Caps { flags: LOCAL_CAPS }), 0);
        let host_caps = crate::ov_screen_peer_caps(screen);
        assert_eq!(host_caps, LOCAL_CAPS);
        assert!(host_caps & ovapp::terminal::CAP_INPUT2 != 0);
        assert_eq!(crate::ov_screen_peer_caps(std::ptr::null()), 0);

        // A few deltas: each delivers the full, correct framebuffer.
        for i in 0..4u32 {
            let at = ((i * 37 % h) * w + i * 53 % w) as usize * 4;
            px[at] ^= 0xFF;
            match enc.submit(&px, w, h, None, &mb) {
                SubmitOutcome::Delta { tiles, .. } => assert!(tiles >= 1),
                other => panic!("expected a delta, got {other:?}"),
            }
            let f = mb.take().unwrap();
            assert!(matches!(f.begin(), Command::TilesBegin { .. }));
            assert_eq!(stream(screen, &f), 0);
            let got = FRAMES.lock().unwrap().last().cloned().unwrap();
            assert_eq!((got.0, got.1, got.2, got.3, got.4), (f.seq, w, h, w * 4, 0));
            assert_eq!(got.5, px, "delta {i}: full framebuffer delivered");
            // The optional dirty-rect API reports the patched rects.
            let mut rects = [0u16; 4 * 8];
            let n = crate::ov_screen_last_dirty_rects(screen, rects.as_mut_ptr(), 8);
            assert!(n >= 1);
            let (rx, ry, rw, rh) = (
                rects[0] as usize,
                rects[1] as usize,
                rects[2] as usize,
                rects[3] as usize,
            );
            let (ax, ay) = ((at / 4) % w as usize, (at / 4) / w as usize);
            assert!(n > 1 || (rx <= ax && ax < rx + rw && ry <= ay && ay < ry + rh));
        }

        // Lose an update: the next delta's base is unknown -> -1, the sink
        // does not fire, and a KEYFRAME_REQUEST is owed.
        px[0] ^= 1;
        enc.submit(&px, w, h, None, &mb);
        let _lost = mb.take().unwrap();
        px[4 * 150] ^= 1;
        enc.submit(&px, w, h, None, &mb);
        let frames_before = FRAMES.lock().unwrap().len();
        assert_eq!(stream(screen, &mb.take().unwrap()), -1);
        assert_eq!(FRAMES.lock().unwrap().len(), frames_before);
        let kf = take_reply(screen).expect("keyframe request owed");
        assert!(matches!(
            ovapp::terminal::decode(&kf).unwrap(),
            Command::KeyframeRequest { .. }
        ));
        // The host answers with a full frame: back in sync.
        enc.request_keyframe();
        enc.submit(&px, w, h, None, &mb);
        assert_eq!(stream(screen, &mb.take().unwrap()), 0);
        assert_eq!(FRAMES.lock().unwrap().last().unwrap().5, px);

        assert_eq!(
            crate::ov_screen_last_dirty_rects(std::ptr::null_mut(), std::ptr::null_mut(), 0),
            -1
        );
        crate::ov_screen_free(screen);
    }
}

/// Zero-copy frames: a shell that mirrors the framebuffer by copying
/// only the visited dirty rects (what the iOS Metal uploader does) stays
/// identical to the host's pixels; a full visit reports no rects.
#[test]
fn screen_visit_framebuffer_dirty_rects_rebuild_the_frame() {
    use ovapp::sendq::{FrameMailbox, UpdateEncoder};
    use ovapp::terminal::{encode, Command, LOCAL_CAPS};

    /// The shell's "texture" + how many bytes the last visit copied.
    struct Mirror {
        px: Vec<u8>,
        copied: usize,
        rects_seen: usize,
    }
    extern "C" fn on_frame(
        _u: *mut c_void,
        _s: u64,
        _w: u32,
        _h: u32,
        _st: u32,
        _f: u8,
        _p: *const u8,
        _n: usize,
    ) {
    }
    extern "C" fn visit(
        user: *mut c_void,
        px: *const u8,
        len: usize,
        w: u32,
        h: u32,
        stride: u32,
        format: u8,
        _seq: u64,
        rects: *const u16,
        n: usize,
    ) {
        let m = unsafe { &mut *(user as *mut Mirror) };
        let src = unsafe { std::slice::from_raw_parts(px, len) };
        assert_eq!(format, 0);
        let stride = stride as usize;
        m.px.resize(w as usize * h as usize * 4, 0);
        m.copied = 0;
        m.rects_seen = n;
        let list: Vec<[u16; 4]> = if n == 0 {
            vec![[0, 0, w as u16, h as u16]]
        } else {
            let r = unsafe { std::slice::from_raw_parts(rects, n * 4) };
            // A trailing partial quad (none for `n * 4` u16s) is ignored.
            r.as_chunks::<4>().0.to_vec()
        };
        for [x, y, rw, rh] in list {
            let (x, y, rw, rh) = (x as usize, y as usize, rw as usize, rh as usize);
            for row in y..y + rh {
                let s = row * stride + x * 4;
                let d = row * w as usize * 4 + x * 4;
                m.px[d..d + rw * 4].copy_from_slice(&src[s..s + rw * 4]);
                m.copied += rw * 4;
            }
        }
    }
    fn stream(screen: *mut crate::OvScreen, f: &ovapp::sendq::EncodedFrame) {
        for cmd in std::iter::once(f.begin())
            .chain(f.data.chunks(1000).map(|c| Command::FrameData(c.to_vec())))
            .chain(std::iter::once(Command::FrameEnd(f.seq)))
        {
            let wire = encode(&cmd);
            assert_eq!(
                unsafe { crate::ov_screen_feed(screen, wire.as_ptr(), wire.len()) },
                0
            );
        }
    }

    let (w, h) = (320u32, 200u32);
    let mut px: Vec<u8> = (0..w * h * 4).map(|i| (i % 251) as u8 | 1).collect();
    let mb = FrameMailbox::default();
    let mut enc = UpdateEncoder::new();
    enc.set_peer_caps(LOCAL_CAPS);
    let mut m = Mirror {
        px: Vec::new(),
        copied: 0,
        rects_seen: 0,
    };
    let user = &mut m as *mut Mirror as *mut c_void;
    unsafe {
        let screen = crate::ov_screen_new();
        assert_eq!(
            crate::ov_screen_set_frame_sink(screen, std::ptr::null_mut(), on_frame),
            0
        );
        // Nothing to visit before the first frame; bad args.
        assert_eq!(
            crate::ov_screen_visit_framebuffer(screen, user, Some(visit), 0),
            -2
        );
        assert_eq!(
            crate::ov_screen_visit_framebuffer(screen, user, None, 0),
            -1
        );
        assert_eq!(
            crate::ov_screen_visit_framebuffer(std::ptr::null_mut(), user, Some(visit), 0),
            -1
        );

        enc.submit(&px, w, h, None, &mb);
        stream(screen, &mb.take().unwrap());
        assert_eq!(
            crate::ov_screen_visit_framebuffer(screen, user, Some(visit), 0),
            0
        );
        assert_eq!(m.px, px, "keyframe mirrored");
        for i in 0..6u32 {
            let at = ((i * 41 % h) * w + i * 67 % w) as usize * 4;
            px[at] ^= 0xFF;
            enc.submit(&px, w, h, None, &mb);
            stream(screen, &mb.take().unwrap());
            assert_eq!(
                crate::ov_screen_visit_framebuffer(screen, user, Some(visit), 0),
                0
            );
            assert!(m.rects_seen >= 1);
            assert!(
                m.copied < px.len() / 4,
                "delta {i}: only dirty rects copied ({} B)",
                m.copied
            );
            assert_eq!(m.px, px, "delta {i}: mirror in sync");
        }
        // A full visit hands over everything with no rect list.
        assert_eq!(
            crate::ov_screen_visit_framebuffer(screen, user, Some(visit), 1),
            0
        );
        assert_eq!(m.rects_seen, 0);
        assert_eq!(m.copied, px.len());
        crate::ov_screen_free(screen);
    }
}

/// A shell that mirrors only the visited dirty rects (the iOS
/// Metal uploader) stays identical to the assembler's framebuffer through
/// lossy JPEG tiles, their refinement, and cache REFs (window switching),
/// and ends on the host's exact pixels.
#[test]
fn screen_dirty_rects_cover_lossy_refined_and_cached_tiles() {
    use ovapp::sendq::{FrameMailbox, SubmitOutcome, UpdateEncoder};
    use ovapp::terminal::{encode, Command, LOCAL_CAPS, VIEWER_W2_CAPS};
    use std::time::{Duration, Instant};

    struct Mirror {
        px: Vec<u8>,
        full: Vec<u8>,
        copied: usize,
    }
    extern "C" fn on_frame(
        _u: *mut c_void,
        _s: u64,
        _w: u32,
        _h: u32,
        _st: u32,
        _f: u8,
        _p: *const u8,
        _n: usize,
    ) {
    }
    extern "C" fn visit(
        user: *mut c_void,
        px: *const u8,
        len: usize,
        w: u32,
        h: u32,
        stride: u32,
        _format: u8,
        _seq: u64,
        rects: *const u16,
        n: usize,
    ) {
        let m = unsafe { &mut *(user as *mut Mirror) };
        let src = unsafe { std::slice::from_raw_parts(px, len) };
        m.full = src.to_vec();
        m.px.resize(w as usize * h as usize * 4, 0);
        m.copied = 0;
        let list: Vec<[u16; 4]> = if n == 0 {
            vec![[0, 0, w as u16, h as u16]]
        } else {
            let r = unsafe { std::slice::from_raw_parts(rects, n * 4) };
            // A trailing partial quad (none for `n * 4` u16s) is ignored.
            r.as_chunks::<4>().0.to_vec()
        };
        for [x, y, rw, rh] in list {
            let (x, y, rw, rh) = (x as usize, y as usize, rw as usize, rh as usize);
            for row in y..y + rh {
                let s = row * stride as usize + x * 4;
                let d = row * w as usize * 4 + x * 4;
                m.px[d..d + rw * 4].copy_from_slice(&src[s..s + rw * 4]);
                m.copied += rw * 4;
            }
        }
    }
    fn stream(screen: *mut crate::OvScreen, f: &ovapp::sendq::EncodedFrame) {
        for cmd in std::iter::once(f.begin())
            .chain(f.data.chunks(1000).map(|c| Command::FrameData(c.to_vec())))
            .chain(std::iter::once(Command::FrameEnd(f.seq)))
        {
            let wire = encode(&cmd);
            assert_eq!(
                unsafe { crate::ov_screen_feed(screen, wire.as_ptr(), wire.len()) },
                0
            );
        }
    }
    let (w, h) = (448u32, 320u32);
    // Window A: light "text" (sparse dark pixels) with a photo; window B:
    // other text.
    let text = |seed: u32| -> Vec<u8> {
        (0..w * h)
            .flat_map(|i| {
                let v = i
                    .wrapping_mul(2_654_435_761)
                    .wrapping_add(seed.wrapping_mul(97));
                if (v >> 24) % 9 == 0 {
                    [0x30, 0x30, 0x30, 0xFF]
                } else {
                    [0xF0, 0xF0, 0xF0, 0xFF]
                }
            })
            .collect()
    };
    let mut a = text(1);
    let mut s = 0x1234_5678u64;
    for y in 64..256usize {
        for x in 64..320usize {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            let o = (y * w as usize + x) * 4;
            let n = (s >> 59) as u8;
            a[o..o + 3].copy_from_slice(&[
                (x / 2) as u8 + n,
                (y / 2) as u8 + n,
                ((x + y) / 4) as u8 ^ n,
            ]);
        }
    }
    let b = text(2);
    let mb = FrameMailbox::default();
    let mut enc = UpdateEncoder::new();
    enc.set_peer_caps(LOCAL_CAPS | VIEWER_W2_CAPS);
    let t0 = Instant::now();
    enc.set_clock(Some(t0));
    let mut m = Mirror {
        px: Vec::new(),
        full: Vec::new(),
        copied: 0,
    };
    let user = &mut m as *mut Mirror as *mut c_void;
    unsafe {
        let screen = crate::ov_screen_new();
        assert_eq!(
            crate::ov_screen_set_frame_sink(screen, std::ptr::null_mut(), on_frame),
            0
        );
        enc.submit(&a, w, h, None, &mb);
        stream(screen, &mb.take().unwrap());
        assert_eq!(
            crate::ov_screen_visit_framebuffer(screen, user, Some(visit), 0),
            0
        );
        assert_eq!(m.px, m.full);
        assert_ne!(m.px, a, "photo tiles arrive lossy first");
        assert!(enc.stats().lossy_tiles >= 12, "{:?}", enc.stats());
        // Refinement: each chunk's rects are reported; ends exact.
        enc.set_clock(Some(t0 + Duration::from_millis(400)));
        let mut chunks = 0;
        while enc.submit_refine(&mb) != SubmitOutcome::Unchanged {
            stream(screen, &mb.take().unwrap());
            assert_eq!(
                crate::ov_screen_visit_framebuffer(screen, user, Some(visit), 0),
                0
            );
            assert_eq!(m.px, m.full, "refine chunk {chunks}: mirror in sync");
            assert!(m.copied < a.len(), "only the refined tiles copied");
            chunks += 1;
        }
        assert!(chunks >= 1);
        assert_eq!(m.px, a, "refined to exact pixels");
        // Window switch B, then back to A: A comes back as cache REFs.
        for (i, px) in [&b, &a].into_iter().enumerate() {
            enc.set_clock(Some(t0 + Duration::from_secs(2 + i as u64)));
            enc.submit(px, w, h, None, &mb);
            let f = mb.take().unwrap();
            if i == 1 {
                assert!(f.data.len() < 4096, "switch back: {} B", f.data.len());
            }
            stream(screen, &f);
            assert_eq!(
                crate::ov_screen_visit_framebuffer(screen, user, Some(visit), 0),
                0
            );
            assert_eq!(&m.px, px, "switch {i}: mirror exact");
        }
        assert!(enc.stats().cache_refs > 0);
        crate::ov_screen_free(screen);
    }
}

/// With an Opus sink installed the assembler advertises CAP_OPUS
/// and passes Opus AUDIO packets through untouched (iOS decodes them with
/// AVAudioConverter); raw PCM from an old host still reaches the PCM sink.
/// Without an Opus sink the CAPS stay the base bits (the host keeps
/// sending raw PCM).
#[test]
fn screen_passes_opus_through_and_advertises_cap_opus() {
    use ovapp::terminal::{encode, Command, CAP_OPUS, LOCAL_CAPS};
    use std::sync::Mutex;

    static OPUS: Mutex<Vec<(Vec<u8>, u32)>> = Mutex::new(Vec::new());
    static PCM: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());
    extern "C" fn on_opus(_u: *mut c_void, p: *const u8, len: usize, samples: u32) {
        // NULL / 0 = a loss signal.
        let b = if p.is_null() {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(p, len) }.to_vec()
        };
        OPUS.lock().unwrap().push((b, samples));
    }
    extern "C" fn on_pcm(_u: *mut c_void, p: *const u8, len: usize) {
        let b = unsafe { std::slice::from_raw_parts(p, len) }.to_vec();
        PCM.lock().unwrap().push(b);
    }
    fn feed(screen: *mut crate::OvScreen, cmd: &Command) -> i32 {
        let wire = encode(cmd);
        unsafe { crate::ov_screen_feed(screen, wire.as_ptr(), wire.len()) }
    }
    fn caps_reply(screen: *mut crate::OvScreen) -> u32 {
        // Any screen command queues our CAPS once.
        assert_eq!(feed(screen, &Command::MonitorsInfo { monitors: vec![] }), 0);
        let mut out = [0u8; 9];
        assert_eq!(
            unsafe { crate::ov_screen_take_pong_reply(screen, out.as_mut_ptr(), 9) },
            1
        );
        match ovapp::terminal::decode(&out).unwrap() {
            Command::Caps { flags } => flags,
            other => panic!("expected CAPS, got {other:?}"),
        }
    }

    unsafe {
        // Old shell (no Opus sink): base caps only.
        let old = crate::ov_screen_new();
        assert_eq!(
            caps_reply(old),
            LOCAL_CAPS | ovapp::terminal::CAP_LZ4 | ovapp::terminal::VIEWER_W2_CAPS
        );
        crate::ov_screen_free(old);

        let screen = crate::ov_screen_new();
        assert_eq!(
            crate::ov_screen_set_audio_sink(screen, std::ptr::null_mut(), Some(on_pcm)),
            0
        );
        assert_eq!(
            crate::ov_screen_set_opus_sink(screen, std::ptr::null_mut(), Some(on_opus)),
            0
        );
        assert_eq!(
            caps_reply(screen),
            LOCAL_CAPS
                | ovapp::terminal::CAP_LZ4
                | ovapp::terminal::VIEWER_W2_CAPS
                | CAP_OPUS
                | ovapp::terminal::CAP_AUDIO10
        );

        let packet = vec![0x78u8, 0x01, 0x02, 0x03, 0xFE];
        let opus = ovapp::audio::encode_opus_frame(&packet, 960, 0);
        assert_eq!(feed(screen, &Command::Audio(opus)), 0);
        assert_eq!(OPUS.lock().unwrap().as_slice(), &[(packet, 960)]);
        assert!(
            PCM.lock().unwrap().is_empty(),
            "Opus never reaches the PCM sink"
        );

        let raw = ovapp::audio::encode_audio_frame(&[1, -1], 0);
        assert_eq!(feed(screen, &Command::Audio(raw)), 0);
        assert_eq!(PCM.lock().unwrap().as_slice(), &[vec![1u8, 0, 0xFF, 0xFF]]);
        // Malformed Opus is dropped, never a crash.
        assert_eq!(feed(screen, &Command::Audio(vec![1, 0, 0, 0, 3, 0xC0])), 0);
        assert_eq!(OPUS.lock().unwrap().len(), 1);
        // Audio lost on the media channel -> NULL packet + samples.
        let gap = Command::MediaGap {
            kind: ovapp::terminal::GAP_AUDIO,
            packets: 3,
            samples: 1440,
        };
        assert_eq!(feed(screen, &gap), 0);
        assert_eq!(OPUS.lock().unwrap().last(), Some(&(Vec::new(), 1440)));
        crate::ov_screen_free(screen);
    }
}

/// The iOS datagram transport speaks the reliable UDP framing. A
/// peer that switched (as a desktop host does after our CAPS advertised
/// CAP_RUDP) sends framed, fragmented records over a lossy, reordering
/// wire; the FFI carrier auto-switches, delivers every record intact and in
/// order through the record sink, and its own records go back framed.
#[test]
fn datagram_transport_recovers_loss_with_reliable_framing() {
    use std::collections::VecDeque;

    struct Wire {
        /// (to_server, datagram)
        q: VecDeque<(bool, Vec<u8>)>,
        records: Vec<(bool, u8, Vec<u8>)>,
        established: u32,
        rng: u64,
        lossy: bool,
        dropped: u32,
        framed_seen: u32,
    }
    static W: Mutex<Option<Wire>> = Mutex::new(None);
    fn w<R>(f: impl FnOnce(&mut Wire) -> R) -> R {
        let mut g = W.lock().unwrap();
        f(g.as_mut().unwrap())
    }
    extern "C" fn dsink(user: *mut c_void, data: *const u8, len: usize) {
        let from_server = !user.is_null();
        let d = unsafe { std::slice::from_raw_parts(data, len) }.to_vec();
        w(|w| {
            if ovtransport::rudp::is_framed(&d) {
                w.framed_seen += 1;
                assert!(d.len() <= 1214, "datagram {} B", d.len());
            }
            w.rng ^= w.rng << 13;
            w.rng ^= w.rng >> 7;
            w.rng ^= w.rng << 17;
            if w.lossy && w.rng % 10 == 0 {
                w.dropped += 1;
                return;
            }
            // Reorder: every 7th datagram jumps the queue.
            if w.lossy && w.rng % 7 == 0 {
                w.q.push_front((!from_server, d));
            } else {
                w.q.push_back((!from_server, d));
            }
        });
    }
    extern "C" fn rsink(user: *mut c_void, ch: u8, data: *const u8, len: usize) {
        let at_server = !user.is_null();
        let d = unsafe { std::slice::from_raw_parts(data, len) }.to_vec();
        w(|w| w.records.push((at_server, ch, d)));
    }
    extern "C" fn ssink(_u: *mut c_void, state: i32, _fp: *const u8, _n: usize) {
        if state == 1 {
            w(|w| w.established += 1);
        }
    }
    *W.lock().unwrap() = Some(Wire {
        q: VecDeque::new(),
        records: Vec::new(),
        established: 0,
        rng: 0x1234_5678_9ABC_DEF1,
        lossy: false,
        dropped: 0,
        framed_seen: 0,
    });
    let code = std::ffi::CString::new("654321").unwrap();
    let client = unsafe { ov_session_create(0, code.as_ptr()) };
    let server = unsafe { ov_session_create(1, code.as_ptr()) };
    let srv_tag = std::ptr::dangling_mut::<c_void>();
    unsafe {
        ov_session_set_datagram_sink(client, std::ptr::null_mut(), dsink);
        ov_session_set_datagram_sink(server, srv_tag, dsink);
        ov_session_set_record_sink(client, std::ptr::null_mut(), rsink);
        ov_session_set_record_sink(server, srv_tag, rsink);
        ov_session_set_state_sink(client, std::ptr::null_mut(), ssink);
        ov_session_set_state_sink(server, srv_tag, ssink);
        assert_eq!(ov_session_begin(client, 0), 0);
        assert_eq!(ov_session_begin(server, 0), 0);
    }
    // Deliver queued datagrams + pump both sides.
    let step = || {
        while let Some((to_server, d)) = w(|w| w.q.pop_front()) {
            let s = if to_server { server } else { client };
            unsafe { ov_session_on_datagram(s, d.as_ptr(), d.len()) };
        }
        unsafe {
            ov_session_pump(client);
            ov_session_pump(server);
        }
    };
    for _ in 0..20 {
        step();
        if w(|w| w.established) == 2 {
            break;
        }
    }
    assert_eq!(
        w(|w| w.established),
        2,
        "handshake (raw, lossless) completes"
    );
    assert_eq!(w(|w| w.framed_seen), 0, "nothing framed before the switch");

    // The server's peer advertised CAP_RUDP: switch it (TerminalSession
    // does this on CAPS); from now on the wire is lossy.
    unsafe {
        let mut g = (*server).inner.lock().unwrap();
        match &mut g.transport {
            crate::Transport::Datagram(c) => {
                assert!(ovtransport::Carrier::enable_reliable(c.as_mut()))
            }
            _ => panic!("datagram transport expected"),
        }
    }
    w(|w| w.lossy = true);
    let recs: Vec<Vec<u8>> = (0..40)
        .map(|i| (0..(i * 997 % 9000 + 9)).map(|j| (i + j) as u8).collect())
        .collect();
    for r in &recs {
        assert_eq!(
            unsafe { ov_session_send_record(server, 0, r.as_ptr(), r.len()) },
            0
        );
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while w(|w| w.records.iter().filter(|r| !r.0).count()) < recs.len()
        && std::time::Instant::now() < deadline
    {
        step();
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let got: Vec<Vec<u8>> = w(|w| {
        w.records
            .iter()
            .filter(|r| !r.0)
            .map(|r| r.2.clone())
            .collect()
    });
    assert_eq!(got.len(), recs.len(), "dropped {}", w(|w| w.dropped));
    assert!(got == recs, "every record intact and in order");
    assert!(w(|w| w.dropped) > 0, "the wire really was lossy");

    // The client auto-switched: its record goes back framed and arrives.
    let before = w(|w| w.framed_seen);
    let msg = b"reply from ios";
    assert_eq!(
        unsafe { ov_session_send_record(client, 1, msg.as_ptr(), msg.len()) },
        0
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !w(|w| w.records.iter().any(|r| r.0 && r.2 == msg))
        && std::time::Instant::now() < deadline
    {
        step();
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(w(|w| w
        .records
        .iter()
        .any(|r| r.0 && r.1 == 1 && r.2 == msg)));
    assert!(
        w(|w| w.framed_seen) > before,
        "client sends framed after the switch"
    );
    unsafe {
        ov_session_free(client);
        ov_session_free(server);
    }
}

/// With a video sink installed the assembler advertises CAP_H264
/// and hands each H.264 access unit to the shell split for VideoToolbox
/// (SPS/PPS out of band on keyframes, AVCC body); a sequence gap drops
/// P-frames until the next keyframe and queues a VIDEO_IDR_REQUEST on the
/// 9-byte reply path; VIDEO_END arrives as a flagged empty packet. Without a
/// sink the CAPS never include CAP_H264 and nothing is delivered.
#[test]
fn screen_passes_h264_through_and_advertises_cap_h264() {
    use ovapp::terminal::{encode, Command, CAP_H264, LOCAL_CAPS, SMALL_CONTROL_LEN};
    use ovapp::tiles::TileRect;
    use ovapp::video::{VideoHeader, CODEC_H264, VIDEO_END, VIDEO_KEYFRAME};

    #[derive(Debug, Clone, PartialEq)]
    struct Got {
        seq: u64,
        flags: u32,
        rect: (u32, u32, u32, u32, u32, u32),
        pts: u64,
        sps: Vec<u8>,
        pps: Vec<u8>,
        data: Vec<u8>,
    }
    static GOT: Mutex<Vec<Got>> = Mutex::new(Vec::new());
    extern "C" fn on_video(_u: *mut c_void, p: *const crate::OvVideoPacket) {
        let p = unsafe { &*p };
        // BT.709 limited range is what the host always signals.
        assert_eq!(
            (p.colour_primaries, p.transfer, p.matrix, p.full_range),
            (1, 1, 1, 0)
        );
        let bytes = |ptr: *const u8, len: usize| {
            if ptr.is_null() {
                assert_eq!(len, 0);
                Vec::new()
            } else {
                unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec()
            }
        };
        GOT.lock().unwrap().push(Got {
            seq: p.seq,
            flags: p.flags,
            rect: (p.x, p.y, p.w, p.h, p.screen_w, p.screen_h),
            pts: p.pts_us,
            sps: bytes(p.sps, p.sps_len),
            pps: bytes(p.pps, p.pps_len),
            data: bytes(p.data, p.data_len),
        });
    }
    fn feed(screen: *mut crate::OvScreen, cmd: &Command) -> i32 {
        let wire = encode(cmd);
        unsafe { crate::ov_screen_feed(screen, wire.as_ptr(), wire.len()) }
    }
    fn reply(screen: *mut crate::OvScreen) -> Option<Command> {
        let mut out = [0u8; SMALL_CONTROL_LEN];
        match unsafe { crate::ov_screen_take_pong_reply(screen, out.as_mut_ptr(), out.len()) } {
            1 => Some(ovapp::terminal::decode(&out).unwrap()),
            _ => None,
        }
    }
    let header = |seq: u64, flags: u8| VideoHeader {
        seq,
        flags,
        codec: CODEC_H264,
        rect: TileRect {
            x: 640,
            y: 320,
            w: 640,
            h: 384,
        },
        screen_w: 1920,
        screen_h: 1080,
        pts_us: 1_000 + seq,
    };
    // Sent in small chunks like the host does.
    fn unit(screen: *mut crate::OvScreen, h: VideoHeader, annexb: &[u8]) -> i32 {
        assert_eq!(
            feed(
                screen,
                &Command::VideoBegin {
                    header: h,
                    compressed_len: annexb.len() as u32
                }
            ),
            0
        );
        for c in annexb.chunks(3) {
            assert_eq!(feed(screen, &Command::FrameData(c.to_vec())), 0);
        }
        feed(screen, &Command::FrameEnd(h.seq))
    }
    let idr: Vec<u8> = [
        &[0, 0, 0, 1, 0x09, 0xF0][..],
        &[0, 0, 0, 1, 0x67, 0x4D, 0x00, 0x28, 0xAB][..],
        &[0, 0, 0, 1, 0x68, 0xEE, 0x3C, 0x80][..],
        &[0, 0, 1, 0x65, 0x88, 0x80, 0x10, 0x00, 0x2F][..],
    ]
    .concat();
    let p_frame = [0u8, 0, 0, 1, 0x41, 0x9A, 0x20, 0x07];

    unsafe {
        // Old shell: no CAP_H264, video ignored.
        let old = crate::ov_screen_new();
        assert_eq!(unit(old, header(0, VIDEO_KEYFRAME), &idr), 0);
        match reply(old) {
            Some(Command::Caps { flags }) => assert_eq!(flags & CAP_H264, 0),
            other => panic!("expected CAPS, got {other:?}"),
        }
        assert!(GOT.lock().unwrap().is_empty());
        crate::ov_screen_free(old);

        let screen = crate::ov_screen_new();
        assert_eq!(
            crate::ov_screen_set_video_sink(screen, std::ptr::null_mut(), Some(on_video)),
            0
        );
        // A P-frame before any keyframe: dropped, IDR requested (after CAPS).
        assert_eq!(unit(screen, header(0, 0), &p_frame), 0);
        assert_eq!(
            reply(screen),
            Some(Command::Caps {
                flags: LOCAL_CAPS
                    | ovapp::terminal::CAP_LZ4
                    | ovapp::terminal::VIEWER_W2_CAPS
                    | CAP_H264
            })
        );
        assert_eq!(
            reply(screen),
            Some(Command::VideoIdrRequest { have_seq: 0 })
        );
        assert_eq!(reply(screen), None);
        assert!(GOT.lock().unwrap().is_empty());

        // Keyframe: parameter sets out of band, AVCC body (AUD dropped).
        assert_eq!(unit(screen, header(1, VIDEO_KEYFRAME), &idr), 0);
        assert_eq!(unit(screen, header(2, 0), &p_frame), 0);
        {
            let got = GOT.lock().unwrap();
            assert_eq!(got.len(), 2);
            assert_eq!(got[0].flags, crate::OV_VIDEO_KEYFRAME);
            assert_eq!(got[0].rect, (640, 320, 640, 384, 1920, 1080));
            assert_eq!(got[0].pts, 1_001);
            assert_eq!(got[0].sps, [0x67, 0x4D, 0x00, 0x28, 0xAB]);
            assert_eq!(got[0].pps, [0x68, 0xEE, 0x3C, 0x80]);
            assert_eq!(
                got[0].data,
                [0, 0, 0, 6, 0x65, 0x88, 0x80, 0x10, 0x00, 0x2F]
            );
            assert_eq!(got[1].flags, 0);
            assert!(got[1].sps.is_empty() && got[1].pps.is_empty());
            assert_eq!(got[1].data, [0, 0, 0, 4, 0x41, 0x9A, 0x20, 0x07]);
        }
        // A gap (seq 3 lost): dropped until the next keyframe; one IDR
        // request (rate limited even though two units were dropped).
        std::thread::sleep(ovapp::terminal::KEYFRAME_RETRY);
        assert_eq!(unit(screen, header(4, 0), &p_frame), 0);
        assert_eq!(unit(screen, header(5, 0), &p_frame), 0);
        assert_eq!(
            reply(screen),
            Some(Command::VideoIdrRequest { have_seq: 4 })
        );
        assert_eq!(reply(screen), None);
        assert_eq!(GOT.lock().unwrap().len(), 2);
        assert_eq!(unit(screen, header(6, VIDEO_KEYFRAME), &idr), 0);
        assert_eq!(GOT.lock().unwrap().len(), 3);
        // Torn unit (FRAME_END before all bytes): an error, chain broken.
        assert_eq!(
            feed(
                screen,
                &Command::VideoBegin {
                    header: header(7, 0),
                    compressed_len: 100
                }
            ),
            0
        );
        assert_eq!(feed(screen, &Command::FrameData(vec![0, 0, 0, 1])), 0);
        assert_eq!(feed(screen, &Command::FrameEnd(7)), -1);
        assert_eq!(unit(screen, header(8, 0), &p_frame), 0);
        assert_eq!(
            GOT.lock().unwrap().len(),
            3,
            "P-frame after a torn unit dropped"
        );
        // VIDEO_END: a flagged, empty packet.
        assert_eq!(unit(screen, header(9, VIDEO_END), &[]), 0);
        {
            let got = GOT.lock().unwrap();
            let end = got.last().unwrap();
            assert_eq!(end.flags, crate::OV_VIDEO_END);
            assert!(end.data.is_empty() && end.sps.is_empty());
            assert_eq!(end.rect, (640, 320, 640, 384, 1920, 1080));
        }
        // The shell's own decoder failed: P-frames dropped, IDR requested.
        while reply(screen).is_some() {}
        std::thread::sleep(ovapp::terminal::KEYFRAME_RETRY);
        assert_eq!(unit(screen, header(10, VIDEO_KEYFRAME), &idr), 0);
        assert_eq!(crate::ov_screen_request_video_idr(screen), 0);
        assert_eq!(
            reply(screen),
            Some(Command::VideoIdrRequest { have_seq: 10 })
        );
        let before = GOT.lock().unwrap().len();
        assert_eq!(unit(screen, header(11, 0), &p_frame), 0);
        assert_eq!(GOT.lock().unwrap().len(), before);
        // A whole unit from the media channel, in one command.
        assert_eq!(
            feed(
                screen,
                &Command::VideoUnit {
                    header: header(12, VIDEO_KEYFRAME),
                    data: idr.clone()
                }
            ),
            0
        );
        assert_eq!(GOT.lock().unwrap().len(), before + 1);
        // A stale unit (it lost the race to a newer one) is ignored.
        assert_eq!(
            feed(
                screen,
                &Command::VideoUnit {
                    header: header(9, VIDEO_KEYFRAME),
                    data: idr.clone()
                }
            ),
            0
        );
        assert_eq!(GOT.lock().unwrap().len(), before + 1);
        // A media gap breaks the chain: IDR requested, P-frames dropped.
        while reply(screen).is_some() {}
        std::thread::sleep(ovapp::terminal::KEYFRAME_RETRY);
        let gap = Command::MediaGap {
            kind: ovapp::terminal::GAP_VIDEO,
            packets: 1,
            samples: 0,
        };
        assert_eq!(feed(screen, &gap), 0);
        assert_eq!(
            reply(screen),
            Some(Command::VideoIdrRequest { have_seq: 12 })
        );
        assert_eq!(
            feed(
                screen,
                &Command::VideoUnit {
                    header: header(13, 0),
                    data: p_frame.to_vec()
                }
            ),
            0
        );
        assert_eq!(GOT.lock().unwrap().len(), before + 1);
        assert_eq!(crate::ov_screen_request_video_idr(std::ptr::null_mut()), -1);
        crate::ov_screen_free(screen);
    }
    // Layout: u64 x2, u32 x8, 6 words, then four
    // colour bytes padded to the struct alignment.
    assert_eq!(
        std::mem::size_of::<crate::OvVideoPacket>(),
        16 + 32 + 6 * std::mem::size_of::<usize>() + std::mem::size_of::<usize>()
    );
    assert_eq!(
        std::mem::offset_of!(crate::OvVideoPacket, colour_primaries),
        16 + 32 + 6 * std::mem::size_of::<usize>()
    );
}

/// The assembler advertises CAP_LZ4 and applies LZ4-block updates
/// (a host may pick LZ4 for low-ratio content); a corrupt block is refused.
#[test]
fn screen_decodes_lz4_frames() {
    use ovapp::terminal::{encode, Command, FrameCodec, CAP_LZ4};
    use std::sync::Mutex;
    static LAST: Mutex<Vec<u8>> = Mutex::new(Vec::new());
    extern "C" fn on_frame(
        _u: *mut c_void,
        _s: u64,
        _w: u32,
        _h: u32,
        _st: u32,
        _f: u8,
        p: *const u8,
        n: usize,
    ) {
        *LAST.lock().unwrap() = unsafe { std::slice::from_raw_parts(p, n) }.to_vec();
    }
    fn feed(screen: *mut crate::OvScreen, cmd: &Command) -> i32 {
        let wire = encode(cmd);
        unsafe { crate::ov_screen_feed(screen, wire.as_ptr(), wire.len()) }
    }
    let (w, h) = (64u32, 48u32);
    let px: Vec<u8> = (0..w * h * 4).map(|i| ((i / 7) % 251) as u8).collect();
    let (used, block) = ovapp::codec::Compressor::new().compress(FrameCodec::Lz4, &px);
    assert_eq!(used, FrameCodec::Lz4);
    let begin = |seq: u64, len: usize| Command::FrameBegin {
        seq,
        width: w,
        height: h,
        stride: w * 4,
        format: ovapp::screen::PixelFormat::Bgra,
        total: px.len() as u32,
        codec: FrameCodec::Lz4,
        compressed_len: len as u32,
    };
    unsafe {
        let screen = crate::ov_screen_new();
        assert_eq!(
            crate::ov_screen_set_frame_sink(screen, std::ptr::null_mut(), on_frame),
            0
        );
        assert_eq!(feed(screen, &begin(1, block.len())), 0);
        assert_eq!(feed(screen, &Command::FrameData(block.clone())), 0);
        assert_eq!(feed(screen, &Command::FrameEnd(1)), 0);
        assert_eq!(*LAST.lock().unwrap(), px);
        let mut out = [0u8; 9];
        assert_eq!(
            crate::ov_screen_take_pong_reply(screen, out.as_mut_ptr(), 9),
            1
        );
        match ovapp::terminal::decode(&out).unwrap() {
            Command::Caps { flags } => assert!(flags & CAP_LZ4 != 0),
            other => panic!("expected CAPS, got {other:?}"),
        }
        // Truncated block: refused, framebuffer untouched.
        assert_eq!(feed(screen, &begin(2, block.len() - 5)), 0);
        assert_eq!(
            feed(
                screen,
                &Command::FrameData(block[..block.len() - 5].to_vec())
            ),
            0
        );
        assert_eq!(feed(screen, &Command::FrameEnd(2)), -1);
        crate::ov_screen_free(screen);
    }
}
