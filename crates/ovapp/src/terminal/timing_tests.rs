//! Latency telemetry on the wire ([`CAP_TIMING`]): timed PONGs, TIMING
//! records, and the viewer joining them end to end over an in-memory
//! carrier.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::wire_session_tests::inmem_pair;
use super::*;
use crate::latency::{is_clock_probe, FrameTiming, HostStamps, Stage, KIND_SCREEN};
use crate::screen::synthetic_frame;
use crate::sendq::{EncodedFrame, FrameMailbox, PrioritySender};
use ovcore::clocksync::mono_us;

#[test]
fn timed_pong_and_timing_wire_roundtrip() {
    let pong = Command::PongTimed {
        nonce: 0x0102_0304_0506_0708,
        rx_us: 11,
        tx_us: 22,
    };
    let wire = encode(&pong);
    assert_eq!(wire[0], OpCode::Pong as u8, "a timed PONG is still a PONG");
    assert_eq!(wire.len(), 25);
    assert_eq!(decode(&wire).unwrap(), pong);
    // The plain 8-byte PONG still decodes as before.
    assert_eq!(
        decode(&encode(&Command::Pong(9))).unwrap(),
        Command::Pong(9)
    );
    // Anything else is malformed.
    assert!(decode(&wire[..17]).is_err());

    let timing = Command::Timing(FrameTiming {
        kind: KIND_SCREEN,
        seq: 77,
        capture_us: 1 << 40,
        grab_us: 1,
        encode_us: 2,
        queue_us: 3,
    });
    let wire = encode(&timing);
    assert_eq!(wire[0], 0x65);
    assert_eq!(wire.len(), 1 + FrameTiming::LEN);
    assert_eq!(decode(&wire).unwrap(), timing);
    assert!(decode(&wire[..20]).is_err());
}

#[test]
fn timing_is_part_of_the_local_caps() {
    assert_ne!(LOCAL_CAPS & CAP_TIMING, 0);
    assert_eq!(CAP_TIMING, 1 << 17);
}

/// Pump both sides until `done` or a deadline.
fn pump_until(
    host: &mut TerminalSession,
    view: &mut TerminalSession,
    mut done: impl FnMut(&mut TerminalSession, &mut TerminalSession) -> bool,
) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let _ = host.pump();
        let _ = view.pump();
        if done(host, view) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    false
}

#[test]
fn a_peer_without_timing_gets_plain_pongs() {
    let (mut host, mut view) = inmem_pair();
    view.set_local_caps(LOCAL_CAPS & !CAP_TIMING);
    view.announce_caps().unwrap();
    assert!(pump_until(&mut host, &mut view, |h, _| h
        .peer_caps()
        .is_some()));
    assert!(!host.timing_enabled() && !view.timing_enabled());
    let got = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&got);
    view.on_cmd(Some(Box::new(move |c: &Command| {
        sink.lock().unwrap().push(c.clone());
    })));
    view.send(&Command::Ping(5)).unwrap();
    assert!(pump_until(&mut host, &mut view, |_, v| v.last_pong() == Some(5)));
    assert_eq!(got.lock().unwrap().as_slice(), &[Command::Pong(5)]);
    assert!(view.clock().is_empty(), "plain PONGs carry no clock sample");
}

#[test]
fn viewer_measures_every_stage_end_to_end() {
    let (mut host, mut view) = inmem_pair();
    view.set_latency_tracking(true);
    // The viewer's hook must see no probe echo and no TIMING record.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    view.on_cmd(Some(Box::new(move |c: &Command| {
        sink.lock().unwrap().push(c.clone());
    })));
    view.announce_caps().unwrap();
    assert!(pump_until(&mut host, &mut view, |h, v| {
        h.timing_enabled() && v.timing_enabled()
    }));
    // Clock probes go out by themselves and come back timed.
    assert!(
        pump_until(&mut host, &mut view, |_, v| v.clock().len() >= 3),
        "clock probes answered"
    );
    let offset = view.clock().offset_us().unwrap();
    assert!(offset.abs() < 50_000, "one process, one clock: {offset} µs");
    // A keepalive PING still reaches the loop as a plain PONG.
    view.send(&Command::Ping(42)).unwrap();
    assert!(pump_until(&mut host, &mut view, |_, v| v.last_pong() == Some(42)));

    // The host stamps a capture and sends one update through the sender.
    let mailbox = FrameMailbox::default();
    let now = mono_us();
    mailbox.set_capture_stamp(Some(HostStamps {
        capture_us: now - 5_000,
        grabbed_us: now - 3_000,
        encoded_us: 0,
    }));
    mailbox.put(EncodedFrame::compress(&synthetic_frame(1, 128, 64)));
    mailbox.set_capture_stamp(None);
    let mut sender = PrioritySender::new(0);
    while sender.has_work(&mailbox) {
        sender.flush(&mut host, &mailbox, 64).unwrap();
    }
    let mut frame = None;
    assert!(pump_until(&mut host, &mut view, |_, v| {
        frame = v.take_latest_frame();
        frame.is_some()
    }));
    let presented = Instant::now();
    assert_eq!(view.note_presented(presented, presented), 1);
    let snap = view.latency_snapshot();
    assert_eq!(snap.frames, 1);
    let capture = snap.stage(Stage::Capture);
    assert_eq!((capture.n, capture.p50_us), (1, 2_000));
    assert!(snap.stage(Stage::Encode).p50_us >= 3_000);
    let total = snap.stage(Stage::Total);
    assert_eq!(total.n, 1);
    // The offset is off by at most half the probe RTT.
    let slack = snap.clock_rtt_us.unwrap_or(0) / 2 + 500;
    assert!(
        u64::from(total.p50_us) + slack >= 5_000,
        "total {} µs, slack {slack} µs",
        total.p50_us
    );
    assert!(snap.hud_line().unwrap().contains(" = "));

    let seen = seen.lock().unwrap();
    assert!(seen.iter().all(|c| match c {
        Command::Pong(n) => !is_clock_probe(*n),
        Command::PongTimed { .. } | Command::Timing(_) => false,
        _ => true,
    }));
}

#[test]
fn updates_without_stamps_send_no_timing() {
    let (mut host, mut view) = inmem_pair();
    view.set_latency_tracking(true);
    view.announce_caps().unwrap();
    assert!(pump_until(&mut host, &mut view, |h, _| h.timing_enabled()));
    let mailbox = FrameMailbox::default();
    mailbox.put(EncodedFrame::compress(&synthetic_frame(1, 64, 64)));
    let mut sender = PrioritySender::new(0);
    while sender.has_work(&mailbox) {
        sender.flush(&mut host, &mailbox, 64).unwrap();
    }
    assert!(pump_until(&mut host, &mut view, |_, v| v
        .take_latest_frame()
        .is_some()));
    let t = Instant::now();
    view.note_presented(t, t);
    let snap = view.latency_snapshot();
    assert_eq!(snap.frames, 1, "viewer stages still measured");
    assert_eq!(snap.stage(Stage::Capture).n, 0, "no host stamps");
    assert_eq!(snap.stage(Stage::Decode).n, 1);
}
