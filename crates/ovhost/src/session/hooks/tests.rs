use std::time::Duration;

use ovcore::handshake::{Auth, Handshake, HandshakeEvent, Role};
use ovcore::identity::Identity;
use ovtransport::{Carrier, InMemoryCarrier};

use super::*;
use crate::input::Region;

/// (viewer, host) terminal sessions over an in-memory carrier pair.
fn term_pair() -> (TerminalSession, TerminalSession) {
    let (id_a, id_b) = (Identity::generate(), Identity::generate());
    let (mut cc, mut sc) = InMemoryCarrier::pair();
    let code = id_b.short_code();
    let mut client = Handshake::new(Role::Client, 0x11, &id_a, Auth::PairingCode(code.clone()));
    client.expect_fingerprint(id_b.fingerprint());
    client.expect_device(0x22);
    let mut server = Handshake::new(Role::Server, 0x22, &id_b, Auth::PairingCode(code));
    for _ in 0..200 {
        let _ = cc.poll_recv(&mut |d| {
            let _ = client.feed(d);
        });
        let _ = sc.poll_recv(&mut |d| {
            let _ = server.feed(d);
        });
        for ev in client.take_events() {
            let HandshakeEvent::Sent(d) = ev;
            let _ = cc.send(&d);
        }
        for ev in server.take_events() {
            let HandshakeEvent::Sent(d) = ev;
            let _ = sc.send(&d);
        }
        if client.is_established() && server.is_established() {
            break;
        }
    }
    assert!(
        client.is_established() && server.is_established(),
        "handshake: client {:?}, server {:?}",
        client.failed(),
        server.failed()
    );
    (
        TerminalSession::new(client.take_session().expect("client"), cc),
        TerminalSession::new(server.take_session().expect("server"), sc),
    )
}

/// A viewer's CAPS reaches the capture thread's atomic (and the pump
/// answers with the host's CAPS); a KEYFRAME_REQUEST raises the keyframe
/// flag.
#[test]
fn caps_and_keyframe_requests_reach_the_capture_thread() {
    use ovapp::terminal::LOCAL_CAPS;
    let (mut view, mut host) = term_pair();
    let hooks = HostHooks::new();
    hooks.register(&mut host);
    view.announce_caps().unwrap();
    view.send(&Command::KeyframeRequest { have_seq: 5 })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while (view.peer_caps().is_none() || !hooks.keyframe.load(Ordering::Relaxed))
        && Instant::now() < deadline
    {
        host.pump().unwrap();
        view.pump().unwrap();
    }
    // Desktop builds also advertise CAP_OPUS (libopus), CAP_AUDIO10 (10 ms
    // Opus packets decode fine) and the viewer-side lossy / tile-cache
    // decode bits.
    let ours = LOCAL_CAPS
        | ovapp::terminal::CAP_OPUS
        | ovapp::terminal::CAP_AUDIO10
        | ovapp::terminal::VIEWER_W2_CAPS;
    assert_eq!(hooks.peer_caps.load(Ordering::Relaxed), ours);
    assert!(hooks.keyframe.load(Ordering::Relaxed));
    assert_eq!(view.peer_caps(), Some(ours), "host answered with its CAPS");
}

/// A viewer with an H.264 decoder advertises CAP_H264 (the capture thread
/// then may stream video), and its VIDEO_IDR_REQUEST reaches the capture
/// thread.
#[test]
fn h264_caps_and_idr_requests_reach_the_capture_thread() {
    struct NoDecode;
    impl ovapp::video::VideoDecode for NoDecode {
        fn decode(
            &mut self,
            _a: &[u8],
            _w: u32,
            _h: u32,
            _o: &mut Vec<u8>,
        ) -> Result<bool, String> {
            Ok(false)
        }
    }
    let (mut view, mut host) = term_pair();
    let hooks = HostHooks::new();
    hooks.register(&mut host);
    view.set_video_decoder(Box::new(NoDecode));
    view.announce_caps().unwrap();
    view.send(&Command::VideoIdrRequest { have_seq: 9 })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !hooks.video_idr.load(Ordering::Relaxed) && Instant::now() < deadline {
        host.pump().unwrap();
        view.pump().unwrap();
    }
    assert!(hooks.peer_caps.load(Ordering::Relaxed) & ovapp::terminal::CAP_H264 != 0);
    assert!(hooks.video_idr.load(Ordering::Relaxed));
    assert!(
        !hooks.keyframe.load(Ordering::Relaxed),
        "an IDR request is not a tile keyframe"
    );
}

/// Input v2 end to end over a real session: the host answers CAPS with
/// CAP_INPUT2, and both a legacy viewer's POINTER/KEY and the new
/// messages reach the controller; RELEASE_ALL releases what was held.
#[test]
fn input_commands_reach_the_controller_old_and_new() {
    use ovapp::terminal::{MouseButton, PointerAction, PointerKind, CAP_INPUT2, KEY_DOWN};
    struct Rec(Arc<Mutex<Vec<input::InputOp>>>);
    impl input::InputBackend for Rec {
        fn send(&mut self, ops: &[input::InputOp]) -> bool {
            self.0.lock().unwrap().extend_from_slice(ops);
            true
        }
        fn cursor_pos(&mut self) -> Option<(i32, i32)> {
            Some((10, 10))
        }
        fn send_sas(&mut self) -> Result<&'static str, String> {
            Err("test".into())
        }
    }
    let (mut view, mut host) = term_pair();
    let ops: Arc<Mutex<Vec<input::InputOp>>> = Arc::default();
    let mut hooks = HostHooks::new();
    let region = Region {
        x: 0,
        y: 0,
        w: 1920,
        h: 1080,
    };
    let vs = VirtualScreen {
        x: 0,
        y: 0,
        w: 1920,
        h: 1080,
    };
    hooks.input = Some(Arc::new(Mutex::new(input::InputController::new(
        Box::new(Rec(Arc::clone(&ops))),
        region,
        vs,
    ))));
    hooks.register(&mut host);
    view.announce_caps().unwrap();
    let cmds = [
        Command::Pointer {
            kind: PointerKind::Down,
            x: 0,
            y: 0,
        },
        Command::Pointer {
            kind: PointerKind::Up,
            x: 0,
            y: 0,
        },
        Command::Key {
            vk: 0x41,
            down: true,
        },
        Command::Key {
            vk: 0x41,
            down: false,
        },
        Command::PointerEx {
            action: PointerAction::Down,
            button: MouseButton::Right,
            flags: 0,
            x: 0,
            y: 0,
        },
        Command::Wheel { dy: -120, dx: 0 },
        Command::Text("é".into()),
        Command::KeyEx {
            vk: 0xA2,
            scan: 0x1D,
            flags: KEY_DOWN,
        },
        Command::ReleaseAll,
    ];
    for c in &cmds {
        view.send(c).unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while (view.peer_caps().is_none() || ops.lock().unwrap().len() < 13)
        && Instant::now() < deadline
    {
        host.pump().unwrap();
        view.pump().unwrap();
    }
    assert!(
        view.peer_caps().unwrap() & CAP_INPUT2 != 0,
        "host advertises input v2"
    );
    let ops = ops.lock().unwrap().clone();
    use input::InputOp as O;
    assert!(ops.contains(&O::Button {
        button: MouseButton::Left,
        down: true
    }));
    assert!(ops.contains(&O::Button {
        button: MouseButton::Left,
        down: false
    }));
    assert!(ops.contains(&O::Button {
        button: MouseButton::Right,
        down: true
    }));
    assert!(ops.contains(&O::Wheel(-120)));
    assert!(ops.contains(&O::Unicode {
        unit: 0xE9,
        up: false
    }));
    // RELEASE_ALL: right button up, then left ctrl up — the last two.
    assert_eq!(
        &ops[ops.len() - 2..],
        &[
            O::Button {
                button: MouseButton::Right,
                down: false
            },
            O::Key {
                vk: 0xA2,
                scan: 0x1D,
                flags: input::keyflags::KEYUP
            }
        ]
    );
}

/// A QUALITY_SET command sent by a viewer over a real (in-memory) session
/// reaches the host hook and updates the shared target-fps atomic.
#[test]
fn quality_set_command_updates_the_target_atomic() {
    let (mut view, mut host) = term_pair();
    let hooks = HostHooks::new();
    hooks.register(&mut host);

    // AUTO first, then a manual 30 fps: the atomic must track both.
    view.send(&Command::QualitySet { target_fps: 0 })
        .expect("send auto");
    view.send(&Command::QualitySet { target_fps: 30 })
        .expect("send 30");
    let deadline = Instant::now() + Duration::from_secs(5);
    while hooks.target_fps.load(Ordering::Relaxed) != 30 && Instant::now() < deadline {
        host.pump().expect("host pump");
    }
    assert_eq!(
        hooks.target_fps.load(Ordering::Relaxed),
        30,
        "the last QUALITY_SET wins"
    );
}

#[test]
fn chat_payload_is_nul_trimmed_and_lossy() {
    assert_eq!(line_str(b"hi\0\0junk"), "hi");
    assert_eq!(line_str(b"a\xffb"), "a\u{fffd}b");
}
