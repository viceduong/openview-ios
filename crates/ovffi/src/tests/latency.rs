//! Latency telemetry through the C entry points: clock probes out of the
//! keepalive tick, timed PONGs, a TIMING-stamped update, the present call
//! and `ov_screen_latency_stats`.

use std::ffi::c_void;

use ovapp::latency::{is_clock_probe, FrameTiming, HostStamps, KIND_SCREEN};
use ovapp::sendq::{FrameMailbox, UpdateEncoder};
use ovapp::terminal::{encode, Command, CAP_TIMING, LOCAL_CAPS};
use ovcore::clocksync::mono_us;

extern "C" fn on_frame(
    _user: *mut c_void,
    _seq: u64,
    _w: u32,
    _h: u32,
    _stride: u32,
    _format: u8,
    _px: *const u8,
    _len: usize,
) {
}

fn feed(screen: *mut crate::OvScreen, cmd: &Command) -> i32 {
    let wire = encode(cmd);
    unsafe { crate::ov_screen_feed(screen, wire.as_ptr(), wire.len()) }
}

fn stats(screen: *mut crate::OvScreen) -> crate::OvLatencyStats {
    let mut out = crate::OvLatencyStats::default();
    assert_eq!(
        unsafe { crate::ov_screen_latency_stats(screen, &mut out) },
        0
    );
    out
}

#[test]
fn screen_measures_latency_against_a_timing_host() {
    unsafe {
        let screen = crate::ov_screen_new();
        assert_eq!(
            crate::ov_screen_set_frame_sink(screen, std::ptr::null_mut(), on_frame),
            0
        );
        // An old host (no CAP_TIMING): the tick never yields a probe.
        assert_eq!(
            feed(
                screen,
                &Command::Caps {
                    flags: LOCAL_CAPS & !CAP_TIMING
                }
            ),
            0
        );
        assert_eq!(crate::ov_screen_keepalive_tick(screen, 100), 0);
        // A timing host: the tick hands out tagged probe sequences.
        assert_eq!(feed(screen, &Command::Caps { flags: LOCAL_CAPS }), 0);
        for i in 0..3 {
            let seq = crate::ov_screen_keepalive_tick(screen, 200 + i * 100);
            assert!(seq > 0, "probe {i} due");
            let nonce = seq as u64;
            assert!(is_clock_probe(nonce));
            // The host answers 1 s ahead of our clock (zero path delay).
            let t = mono_us() + 1_000_000;
            let pong = Command::PongTimed {
                nonce,
                rx_us: t,
                tx_us: t,
            };
            assert_eq!(feed(screen, &pong), 0);
        }
        let s = stats(screen);
        assert_eq!(s.clock_valid, 1);
        assert!(
            (s.clock_offset_us - 1_000_000).abs() < 50_000,
            "offset {}",
            s.clock_offset_us
        );
        assert_eq!(s.frames, 0);

        // One stamped full frame, as the host streams it.
        let (w, h) = (96u32, 64u32);
        let px = vec![7u8; (w * h * 4) as usize];
        let mb = FrameMailbox::default();
        let mut enc = UpdateEncoder::new();
        enc.set_peer_caps(LOCAL_CAPS);
        enc.submit(&px, w, h, None, &mb);
        let f = mb.take().unwrap();
        let host_now = mono_us() + 1_000_000;
        let stamps = HostStamps {
            capture_us: host_now - 6_000,
            grabbed_us: host_now - 4_000,
            encoded_us: host_now - 1_000,
        };
        let timing = FrameTiming::from_stamps(KIND_SCREEN, f.seq, &stamps, host_now);
        assert_eq!(feed(screen, &Command::Timing(timing)), 0);
        assert_eq!(feed(screen, &f.begin()), 0);
        assert_eq!(feed(screen, &Command::FrameData(f.data.clone())), 0);
        assert_eq!(feed(screen, &Command::FrameEnd(f.seq)), 0);
        // Drawn and presented right now.
        assert_eq!(crate::ov_screen_latency_presented(screen, 0, 0), 1);
        let s = stats(screen);
        assert_eq!(s.frames, 1);
        assert_eq!((s.capture.p50_us, s.capture.samples), (2_000, 1));
        assert_eq!(s.encode.p50_us, 3_000);
        assert_eq!(s.send.p50_us, 1_000);
        assert_eq!(s.total.samples, 1);
        // The offset is off by at most half the probe's RTT (a loaded test
        // machine delays the "zero-delay" echoes).
        assert!(
            u64::from(s.total.p50_us) + s.clock_rtt_us / 2 + 500 >= 6_000,
            "total {} µs, clock rtt {} µs: {s:?}",
            s.total.p50_us,
            s.clock_rtt_us
        );
        // Nothing new: a second present measures nothing.
        assert_eq!(crate::ov_screen_latency_presented(screen, 0, 0), 0);

        assert_eq!(
            crate::ov_screen_latency_stats(std::ptr::null_mut(), std::ptr::null_mut()),
            -1
        );
        assert_eq!(
            crate::ov_screen_latency_presented(std::ptr::null_mut(), 0, 0),
            -1
        );
        crate::ov_screen_free(screen);
    }
}

static RTT: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-2);
extern "C" fn on_liveness(_user: *mut c_void, _alive: i32, rtt_ms: i32, _fps: f32) {
    RTT.store(rtt_ms, std::sync::atomic::Ordering::Relaxed);
}

#[test]
fn probe_echoes_do_not_disturb_the_keepalive_rtt() {
    unsafe {
        let screen = crate::ov_screen_new();
        assert_eq!(feed(screen, &Command::Caps { flags: LOCAL_CAPS }), 0);
        let probe = crate::ov_screen_keepalive_tick(screen, 0) as u64;
        assert!(is_clock_probe(probe));
        // The keepalive PING is due at 5 s; its timed echo measures the RTT
        // even after an unrelated probe echo arrived first.
        let ka = crate::ov_screen_keepalive_tick(screen, 5_000) as u64;
        assert!(ka > 0 && !is_clock_probe(ka));
        let t = mono_us();
        assert_eq!(
            feed(
                screen,
                &Command::PongTimed {
                    nonce: probe,
                    rx_us: t,
                    tx_us: t
                }
            ),
            0
        );
        assert_eq!(
            feed(
                screen,
                &Command::PongTimed {
                    nonce: ka,
                    rx_us: t,
                    tx_us: t
                }
            ),
            0
        );
        assert_eq!(
            crate::ov_screen_set_liveness_sink(screen, std::ptr::null_mut(), Some(on_liveness)),
            0
        );
        assert_eq!(crate::ov_screen_liveness_tick(screen, 5_010), 0);
        assert!(
            RTT.load(std::sync::atomic::Ordering::Relaxed) >= 0,
            "keepalive RTT measured"
        );
        crate::ov_screen_free(screen);
    }
}
