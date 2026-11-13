//! Integration test: two real `TerminalSession`s that start on a relay-like
//! in-memory carrier, upgrade to a loopback direct UDP path, negotiate
//! `CAP_MEDIA`, and then move H.264 units (through the host's
//! `PrioritySender`) and audio over the unreliable media channel while
//! commands keep flowing on the reliable one. A peer without `CAP_MEDIA`
//! keeps everything on the reliable path.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ovapp::sendq::{EncodedFrame, FrameMailbox, PrioritySender};
use ovapp::terminal::{Command, TerminalSession, CAP_MEDIA};
use ovapp::tiles::TileRect;
use ovapp::video::{VideoDecode, VideoHeader, CODEC_H264, VIDEO_KEYFRAME};
use ovcore::handshake::{Auth, Handshake, HandshakeEvent, Role};
use ovcore::identity::Identity;
use ovtransport::{Carrier, DirectSockets, InMemoryCarrier};

fn flush(hs: &mut Handshake, c: &mut InMemoryCarrier) {
    for ev in hs.take_events() {
        let HandshakeEvent::Sent(d) = ev;
        c.send(&d).unwrap();
    }
}

/// (viewer, host), both direct-capable on loopback sockets.
fn sessions() -> (TerminalSession, TerminalSession) {
    let (mut vc, mut hc) = InMemoryCarrier::pair();
    let (vid, hid) = (Identity::generate(), Identity::generate());
    let code = hid.pairing_code();
    let mut v = Handshake::new(Role::Client, 0x11, &vid, Auth::PairingCode(code.clone()));
    v.expect_fingerprint(hid.fingerprint());
    let mut h = Handshake::new(Role::Server, 0x22, &hid, Auth::PairingCode(code));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !(v.is_established() && h.is_established()) {
        assert!(Instant::now() < deadline, "handshake timed out");
        flush(&mut v, &mut vc);
        flush(&mut h, &mut hc);
        hc.poll_recv(&mut |d| {
            let _ = h.feed(d);
        })
        .unwrap();
        vc.poll_recv(&mut |d| {
            let _ = v.feed(d);
        })
        .unwrap();
    }
    flush(&mut h, &mut hc);
    let mut view = TerminalSession::new(v.take_session().unwrap(), vc);
    let mut host = TerminalSession::new(h.take_session().unwrap(), hc);
    let hs = DirectSockets::bind_loopback().unwrap();
    let addr: SocketAddr = format!("127.0.0.1:{}", hs.port()).parse().unwrap();
    host.enable_direct(hs, vec![addr]);
    view.enable_direct(DirectSockets::bind_loopback().unwrap(), Vec::new());
    (view, host)
}

/// Accepts every unit (the test checks delivery, not pixels).
struct NullDecoder;
impl VideoDecode for NullDecoder {
    fn decode(
        &mut self,
        _annexb: &[u8],
        w: u32,
        h: u32,
        out: &mut Vec<u8>,
    ) -> Result<bool, String> {
        out.clear();
        out.resize((w * h * 4) as usize, 0x80);
        Ok(true)
    }
}

fn pump_both(view: &mut TerminalSession, host: &mut TerminalSession) {
    view.pump().unwrap();
    host.pump().unwrap();
}

fn until(mut f: impl FnMut() -> bool, secs: u64) -> bool {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_micros(500));
    }
    false
}

fn unit(seq: u64, key: bool) -> EncodedFrame {
    let header = VideoHeader {
        seq,
        flags: if key { VIDEO_KEYFRAME } else { 0 },
        codec: CODEC_H264,
        rect: TileRect {
            x: 0,
            y: 0,
            w: 320,
            h: 192,
        },
        screen_w: 640,
        screen_h: 384,
        pts_us: ovapp::video::unix_micros(),
    };
    let data: Vec<u8> = (0..20_000u32).map(|i| (i as u64 * 7 + seq) as u8).collect();
    EncodedFrame::video(header, data)
}

#[test]
fn video_and_audio_ride_the_media_channel() {
    let (mut view, mut host) = sessions();
    view.set_video_decoder(Box::new(NullDecoder));
    let audio: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
    let a = Arc::clone(&audio);
    view.on_cmd(Some(Box::new(move |c| {
        if let Command::Audio(p) = c {
            a.lock().unwrap().push(p.clone());
        }
    })));
    assert_ne!(view.local_caps() & CAP_MEDIA, 0);
    view.announce_caps().unwrap();
    assert!(
        until(
            || {
                pump_both(&mut view, &mut host);
                view.media_ready() && host.media_ready()
            },
            10
        ),
        "media channel never opened"
    );
    assert!(
        host.transport_rate().is_some(),
        "the controller governs the direct path"
    );
    let mailbox = FrameMailbox::new(None);
    let mut sender = PrioritySender::new(0);
    for seq in 0..20u64 {
        mailbox.push_video(unit(seq, seq == 0));
        sender.push_control(Command::Ping(seq));
        host.send_audio(vec![1, 0, 0, 0, 1, 224, 9, 9, 9]).unwrap();
        let t = Instant::now() + Duration::from_millis(20);
        while Instant::now() < t {
            sender.flush(&mut host, &mailbox, 4).unwrap();
            pump_both(&mut view, &mut host);
            std::thread::sleep(Duration::from_micros(500));
        }
    }
    assert!(
        until(
            || {
                // Units the transport's backpressure held in the mailbox.
                sender.flush(&mut host, &mailbox, 4).unwrap();
                pump_both(&mut view, &mut host);
                view.video_stats().units == 20
            },
            5
        ),
        "units {:?}",
        view.video_stats()
    );
    let st = host.media_stats().unwrap();
    assert_eq!(st.frames_tx, 20, "every unit went on the media channel");
    assert!(st.parity_tx > 0);
    assert_eq!(view.video_stats().dropped, 0);
    assert_eq!(audio.lock().unwrap().len(), 20);
    assert!(
        view.media_summary().contains("rx 20 unit(s)"),
        "{}",
        view.media_summary()
    );
    assert!(view.last_pong().is_none() || view.stats().cmds_rx > 0);
}

#[test]
fn without_cap_media_everything_stays_reliable() {
    let (mut view, mut host) = sessions();
    view.set_video_decoder(Box::new(NullDecoder));
    view.set_local_caps(view.local_caps() & !CAP_MEDIA);
    view.announce_caps().unwrap();
    assert!(until(
        || {
            pump_both(&mut view, &mut host);
            view.direct_state() == Some(ovtransport::DirectState::Direct)
                && host.direct_state() == Some(ovtransport::DirectState::Direct)
        },
        10
    ));
    assert!(!host.media_ready() && !view.media_ready());
    assert!(
        host.transport_rate().is_none(),
        "relay-style pacing stays in charge"
    );
    let mailbox = FrameMailbox::new(None);
    let mut sender = PrioritySender::new(0);
    for seq in 0..5u64 {
        mailbox.push_video(unit(seq, seq == 0));
    }
    assert!(
        until(
            || {
                let _ = sender.flush(&mut host, &mailbox, 64);
                pump_both(&mut view, &mut host);
                view.video_stats().units == 5
            },
            10
        ),
        "{:?}",
        view.video_stats()
    );
    assert!(host.media_stats().is_none());
}
