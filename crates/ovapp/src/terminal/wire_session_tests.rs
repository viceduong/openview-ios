//! Wire round trips for the frame, input, cursor, quality, monitor and
//! clipboard commands, the DEFLATE helpers, and end-to-end
//! [`TerminalSession`] behaviour over an in-memory carrier.

use ovtransport::{Carrier, TransportError};

use super::*;
use crate::screen::PixelFormat;

/// Two symmetrically-established [`Session`]s sharing derived keys, produced by
/// running an in-memory handshake (no real sockets). Returns (client, server).
fn established_pair(
    client_id: &ovcore::identity::Identity,
    server_id: &ovcore::identity::Identity,
) -> (ovcore::session::Session, ovcore::session::Session) {
    use ovcore::handshake::{Auth, Handshake, HandshakeEvent, Role};
    use ovtransport::InMemoryCarrier;

    let (mut cc, mut sc) = InMemoryCarrier::pair();
    let code = server_id.pairing_code();
    let mut client = Handshake::new(
        Role::Client,
        0x11,
        client_id,
        Auth::PairingCode(code.clone()),
    );
    client.expect_fingerprint(server_id.fingerprint());
    let mut server = Handshake::new(Role::Server, 0x22, server_id, Auth::PairingCode(code));
    server.expect_device(0x11);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let _ = cc.poll_recv(&mut |d| {
            let _ = client.feed(d);
        });
        let _ = sc.poll_recv(&mut |d| {
            let _ = server.feed(d);
        });
        for ev in client.take_events() {
            let HandshakeEvent::Sent(dg) = ev;
            let _ = cc.send(&dg);
        }
        for ev in server.take_events() {
            let HandshakeEvent::Sent(dg) = ev;
            let _ = sc.send(&dg);
        }
        if client.is_established() && server.is_established() {
            let sa = client.take_session().expect("client session");
            let sb = server.take_session().expect("server session");
            return (sa, sb);
        }
        if std::time::Instant::now() > deadline {
            panic!(
                "handshake over inmemory did not complete: {:?} / {:?}",
                client.failed(),
                server.failed()
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

#[test]
fn frame_command_wire_roundtrip() {
    // FRAME_BEGIN carries all geometry + codec; verify every field survives
    // for both codecs (raw 0 and deflate 1).
    for (codec, compressed_len) in [
        (FrameCodec::Raw, 1024u32 * 128),
        (FrameCodec::Deflate, 4_321),
    ] {
        let begin = Command::FrameBegin {
            seq: 0x0102_0304_0506_0708,
            width: 256,
            height: 128,
            stride: 1024,
            format: PixelFormat::Bgra,
            total: 1024 * 128,
            codec,
            compressed_len,
        };
        let wire = encode(&begin);
        assert_eq!(wire[0], 0x20, "opcode must be FRAME_BEGIN");
        assert_eq!(wire.len(), 1 + 30, "payload is exactly 30 bytes");
        assert_eq!(wire[1 + 25], codec as u8, "codec byte at [26]");
        assert_eq!(
            &wire[1 + 26..1 + 30],
            &compressed_len.to_be_bytes(),
            "compressed_len u32 BE at [27..31]"
        );
        let back = decode(&wire).expect("decode FRAME_BEGIN");
        assert_eq!(back, begin);
    }

    // A 30-byte payload is the minimum; 29 must be rejected.
    let begin = Command::FrameBegin {
        seq: 1,
        width: 4,
        height: 4,
        stride: 16,
        format: PixelFormat::Bgra,
        total: 64,
        codec: FrameCodec::Raw,
        compressed_len: 64,
    };
    let wire = encode(&begin);
    let short = &wire[..1 + 29];
    assert!(decode(short).is_err(), "short FRAME_BEGIN must error");
    // An unknown codec byte must be rejected.
    let mut bad_codec = wire.clone();
    bad_codec[1 + 25] = 9;
    assert!(decode(&bad_codec).is_err(), "codec 9 must error");

    let chunk = vec![0xAAu8; 7];
    let dw = encode(&Command::FrameData(chunk.clone()));
    assert_eq!(dw[0], 0x21);
    assert_eq!(decode(&dw).unwrap(), Command::FrameData(chunk));

    let ew = encode(&Command::FrameEnd(42));
    assert_eq!(ew[0], 0x22);
    assert_eq!(decode(&ew).unwrap(), Command::FrameEnd(42));
    // FRAME_END must carry exactly 8 bytes.
    assert!(decode(&[0x22, 0, 0, 0, 0, 0, 0, 0]).is_err());
}

#[test]
fn compress_decompress_roundtrip_is_lossless() {
    // A synthetic frame (patterned pixels) survives a compress/decompress
    // roundtrip byte-exactly.
    let frame = crate::screen::synthetic_frame(7, 320, 240);
    let compressed = compress_frame(&frame.pixels);
    let back = decompress(&compressed, frame.pixels.len()).expect("decompress");
    assert_eq!(back, frame.pixels, "roundtrip must be byte-exact");
}

#[test]
fn decompress_bounds_the_declared_length() {
    let compressed = compress_frame(&[7u8; 1000]);
    // A peer-declared size past the frame limit is refused before any
    // allocation; one the stream cannot fill is refused after decoding.
    let err = decompress(&compressed, crate::bands::MAX_RAW_TOTAL + 1).unwrap_err();
    assert!(err.to_string().contains("frame limit"), "{err}");
    assert!(decompress(&compressed, 300 * 1024 * 1024).is_err());
    let mut out = Vec::new();
    assert!(super::decompress_into(&compressed, 64 * 1024 * 1024, &mut out).is_err());
    assert!(
        out.capacity() < 2 * 1024 * 1024,
        "reservation bounded by the input size, got {}",
        out.capacity()
    );
    for codec in [
        FrameCodec::Raw,
        FrameCodec::Zstd,
        FrameCodec::Lz4,
        FrameCodec::Deflate,
    ] {
        assert!(crate::codec::Decompressor::new()
            .decompress_into(codec, &compressed, usize::MAX, &mut out)
            .is_err());
    }
}

#[test]
fn deflate_shrinks_a_realistic_frame() {
    // A 1920x1080 BGRA frame of flat regions + a gradient (like a real
    // desktop) must compress to a fraction of its raw size.
    let (w, h) = (1920usize, 1080usize);
    let mut pixels = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;
            // Flat dark background with a horizontal gradient band.
            if y < 64 {
                pixels[i] = (x % 256) as u8;
                pixels[i + 1] = (x % 256) as u8;
                pixels[i + 2] = 0x40;
            }
            pixels[i + 3] = 0xFF;
        }
    }
    let compressed = compress_frame(&pixels);
    assert!(compressed.len() < pixels.len(), "must shrink");
    let ratio = pixels.len() as f64 / compressed.len() as f64;
    eprintln!(
        "1920x1080 gradient frame: {} bytes -> {} bytes (ratio {:.1}:1)",
        pixels.len(),
        compressed.len(),
        ratio
    );
    assert!(decompress(&compressed, pixels.len()).is_ok());
}

#[test]
fn corrupt_or_short_deflate_is_rejected() {
    let frame = crate::screen::synthetic_frame(1, 64, 32);
    let compressed = compress_frame(&frame.pixels);

    // Truncated stream must error.
    assert!(decompress(&compressed[..compressed.len() / 2], frame.pixels.len()).is_err());
    // Corrupted bytes must error (or produce the wrong length).
    let mut corrupt = compressed.clone();
    corrupt[4] ^= 0xFF;
    let bad = decompress(&corrupt, frame.pixels.len());
    assert!(
        bad.is_err() || bad.unwrap() != frame.pixels,
        "corrupt stream must not yield the original pixels"
    );
    // Right stream but wrong declared length must error.
    assert!(decompress(&compressed, frame.pixels.len() - 1).is_err());
}

#[test]
fn input_v2_wire_roundtrip() {
    // POINTER_EX: opcode + 11 payload bytes, exact layout.
    let p = Command::PointerEx {
        action: PointerAction::MoveRel,
        button: MouseButton::Left,
        flags: POINTER_RAW,
        x: -3,
        y: 0x0102_0304,
    };
    let w = encode(&p);
    assert_eq!(w[0], 0x34);
    assert_eq!(w.len(), 12);
    assert_eq!(&w[1..4], &[3, 0, POINTER_RAW]);
    assert_eq!(&w[4..8], &(-3i32).to_be_bytes());
    assert_eq!(&w[8..12], &[1, 2, 3, 4]);
    assert_eq!(decode(&w).unwrap(), p);
    for action in [
        PointerAction::MoveAbs,
        PointerAction::Down,
        PointerAction::Up,
        PointerAction::MoveRel,
    ] {
        for button in MouseButton::ALL {
            let c = Command::PointerEx {
                action,
                button,
                flags: POINTER_HAS_POS,
                x: 65_535,
                y: 0,
            };
            assert_eq!(decode(&encode(&c)).unwrap(), c);
        }
    }
    let mut bad = w.clone();
    bad[1] = 4;
    assert!(decode(&bad).is_err(), "unknown action");
    let mut bad = w.clone();
    bad[2] = 5;
    assert!(decode(&bad).is_err(), "unknown button");
    assert!(decode(&w[..11]).is_err(), "short POINTER_EX");

    // WHEEL: dy then dx, i16 BE.
    let wh = Command::Wheel { dy: -240, dx: 120 };
    let ww = encode(&wh);
    assert_eq!(ww, vec![0x35, 0xFF, 0x10, 0x00, 0x78]);
    assert_eq!(decode(&ww).unwrap(), wh);
    assert!(decode(&ww[..4]).is_err());

    // KEY_EX: vk, scan, flags.
    let k = Command::KeyEx {
        vk: 0x25,
        scan: 0x4B,
        flags: KEY_DOWN | KEY_EXTENDED,
    };
    let kw = encode(&k);
    assert_eq!(kw, vec![0x39, 0x00, 0x25, 0x00, 0x4B, 0x03]);
    assert_eq!(decode(&kw).unwrap(), k);
    assert!(decode(&kw[..5]).is_err());

    // TEXT: UTF-8 (incl. astral plane), invalid UTF-8 rejected.
    let t = Command::Text("héllo 😀".into());
    let tw = encode(&t);
    assert_eq!(tw[0], 0x3A);
    assert_eq!(decode(&tw).unwrap(), t);
    assert!(decode(&[0x3A, 0xFF, 0xFE]).is_err());

    // RELEASE_ALL / SAS: bare opcodes.
    assert_eq!(encode(&Command::ReleaseAll), vec![0x3B]);
    assert_eq!(decode(&[0x3B]).unwrap(), Command::ReleaseAll);
    assert_eq!(encode(&Command::Sas), vec![0x3C]);
    assert_eq!(decode(&[0x3C]).unwrap(), Command::Sas);
}

#[test]
fn wheel_accumulator_carries_fractions() {
    let mut acc = WheelAccumulator::default();
    acc.add(0.4, 0.0);
    assert_eq!(acc.take(), None, "under one unit stays pending");
    acc.add(0.7, -2.5);
    assert_eq!(acc.take(), Some(Command::Wheel { dy: 1, dx: -2 }));
    acc.add(0.0, -0.6);
    // 0.1 + 0.0 dy, -0.5 - 0.6 = -1.1 dx
    assert_eq!(acc.take(), Some(Command::Wheel { dy: 0, dx: -1 }));
    acc.add(1e9, 0.0);
    assert_eq!(
        acc.take(),
        Some(Command::Wheel {
            dy: i16::MAX,
            dx: 0
        }),
        "clamped"
    );
    acc.add(f64::NAN, f64::INFINITY);
    // The NaN/inf were ignored; the clamped remainder still drains.
    assert!(acc.take().is_some());
}

#[test]
fn caps_advertise_input2() {
    const { assert!(LOCAL_CAPS & CAP_INPUT2 != 0) };
    assert_eq!(CAP_INPUT2, 0x40);
    // An old peer's CAPS (pre-input2 bits only) does not carry it.
    let old = CAP_TILES | CAP_ZSTD | CAP_KEYFRAME | CAP_RUDP;
    assert_eq!(old & CAP_INPUT2, 0);
}

#[test]
fn pointer_and_key_wire_roundtrip() {
    // POINTER: opcode, 1B kind, 4B x BE, 4B y BE — exactly 9 payload bytes.
    let pointer = Command::Pointer {
        kind: PointerKind::Down,
        x: 0x0001_0203,
        y: 0x0004_0506,
    };
    let wire = encode(&pointer);
    assert_eq!(wire[0], 0x30, "opcode must be POINTER");
    assert_eq!(wire.len(), 1 + 9, "payload is exactly 9 bytes");
    assert_eq!(wire[1], 1, "kind = Down");
    assert_eq!(&wire[2..6], &[0x00, 0x01, 0x02, 0x03], "x big-endian");
    assert_eq!(&wire[6..10], &[0x00, 0x04, 0x05, 0x06], "y big-endian");
    assert_eq!(decode(&wire).expect("decode POINTER"), pointer);

    // Every kind round-trips; an unknown kind byte is rejected.
    for kind in [PointerKind::Move, PointerKind::Down, PointerKind::Up] {
        let cmd = Command::Pointer {
            kind,
            x: 0,
            y: 65_535,
        };
        assert_eq!(decode(&encode(&cmd)).unwrap(), cmd);
        let mut bad = encode(&cmd);
        bad[1] = 3;
        assert!(decode(&bad).is_err(), "kind 3 must error");
    }

    // Truncated / over-long payloads are exact-length errors.
    assert!(decode(&wire[..1 + 8]).is_err(), "8-byte POINTER must error");
    assert!(decode(&wire[..1 + 4]).is_err(), "4-byte POINTER must error");
    let mut long = wire.clone();
    long.push(0);
    assert!(decode(&long).is_err(), "10-byte POINTER must error");

    // KEY: opcode, 2B vk BE, 1B down — exactly 3 payload bytes.
    let key = Command::Key {
        vk: 0x0102,
        down: true,
    };
    let kw = encode(&key);
    assert_eq!(kw[0], 0x31, "opcode must be KEY");
    assert_eq!(kw.len(), 1 + 3, "payload is exactly 3 bytes");
    assert_eq!(&kw[1..3], &[0x01, 0x02], "vk big-endian");
    assert_eq!(kw[3], 1, "down = 1");
    assert_eq!(decode(&kw).expect("decode KEY"), key);
    assert_eq!(
        decode(&encode(&Command::Key {
            vk: 0x0D,
            down: false
        }))
        .unwrap(),
        Command::Key {
            vk: 0x0D,
            down: false
        }
    );
    assert!(decode(&kw[..1 + 2]).is_err(), "2-byte KEY must error");
    let mut klong = kw.clone();
    klong.push(0);
    assert!(decode(&klong).is_err(), "4-byte KEY must error");
}

#[test]
fn cursor_wire_roundtrip() {
    // CURSOR: opcode, 4B x i32-LE, 4B y i32-LE — exactly 8 payload bytes.
    // i32 (not u32) so multi-monitor left/bottom coordinates stay negative.
    for (x, y) in [(0, 0), (-1920, -1080), (3839, 2159), (i32::MIN, i32::MAX)] {
        let cmd = Command::Cursor { x, y };
        let wire = encode(&cmd);
        assert_eq!(wire[0], 0x32, "opcode must be CURSOR");
        assert_eq!(wire.len(), 1 + 8, "payload is exactly 8 bytes");
        assert_eq!(&wire[1..5], &x.to_le_bytes(), "x little-endian");
        assert_eq!(&wire[5..9], &y.to_le_bytes(), "y little-endian");
        assert_eq!(decode(&wire).expect("decode CURSOR"), cmd);
    }
    // Exact-length errors.
    let wire = encode(&Command::Cursor { x: 1, y: 2 });
    assert!(decode(&wire[..1 + 7]).is_err(), "7-byte CURSOR must error");
    let mut long = wire.clone();
    long.push(0);
    assert!(decode(&long).is_err(), "9-byte CURSOR must error");
}

#[test]
fn cursor_shape_wire_roundtrip() {
    // CURSOR_SHAPE: 8 bytes of u16-LE header + the BGRA bitmap tail.
    let shape = Command::CursorShape {
        hotspot_x: 2,
        hotspot_y: 3,
        w: 4,
        h: 4,
        bgra: (0..64u8).collect(),
    };
    let wire = encode(&shape);
    assert_eq!(wire[0], 0x33, "opcode must be CURSOR_SHAPE");
    assert_eq!(wire.len(), 1 + 8 + 64, "header + w*h*4 bitmap");
    assert_eq!(&wire[1..3], &2u16.to_le_bytes(), "hotspot_x LE");
    assert_eq!(&wire[3..5], &3u16.to_le_bytes(), "hotspot_y LE");
    assert_eq!(&wire[5..7], &4u16.to_le_bytes(), "w LE");
    assert_eq!(&wire[7..9], &4u16.to_le_bytes(), "h LE");
    assert_eq!(decode(&wire).expect("decode CURSOR_SHAPE"), shape);

    // A 1x1 cursor (4 bitmap bytes) round-trips too.
    let tiny = Command::CursorShape {
        hotspot_x: 0,
        hotspot_y: 0,
        w: 1,
        h: 1,
        bgra: vec![1, 2, 3, 4],
    };
    assert_eq!(decode(&encode(&tiny)).unwrap(), tiny);

    // Bitmap length must match w*h*4 exactly: short and over-long error.
    assert!(
        decode(&wire[..1 + 8 + 63]).is_err(),
        "short bitmap must error"
    );
    let mut long = wire.clone();
    long.push(0);
    assert!(decode(&long).is_err(), "over-long bitmap must error");
    // Truncated header errors.
    assert!(decode(&wire[..1 + 7]).is_err(), "7-byte header must error");
}

#[test]
fn quality_set_wire_roundtrip() {
    // QUALITY_SET: opcode 0x36, a 2-byte u16-BE target fps;
    // 0 means AUTO.
    for target in [0u16, 1, 10, 30, 60, u16::MAX] {
        let cmd = Command::QualitySet { target_fps: target };
        let wire = encode(&cmd);
        assert_eq!(wire[0], 0x36, "opcode must be QUALITY_SET");
        assert_eq!(wire.len(), 3, "payload is exactly 2 bytes");
        assert_eq!(&wire[1..3], &target.to_be_bytes(), "fps big-endian");
        assert_eq!(decode(&wire).expect("decode QUALITY_SET"), cmd);
    }
    // Exact-length errors: short and over-long payloads are protocol
    // errors, never silently accepted.
    let wire = encode(&Command::QualitySet { target_fps: 30 });
    assert!(decode(&wire[..2]).is_err(), "1-byte payload must error");
    let mut long = wire.clone();
    long.push(0);
    assert!(decode(&long).is_err(), "3-byte payload must error");
    assert!(decode(&wire[..1]).is_err(), "empty payload must error");
}

#[test]
fn capture_region_wire_roundtrip() {
    // CAPTURE_REGION: opcode 0x37, 16-byte BE payload (i32 x,
    // i32 y, u32 w, u32 h) — negative origins are legal (multi-monitor).
    for (x, y, w, h) in [
        (0i32, 0i32, 1920u32, 1080u32),
        (-1920, 0, 1920, 1080),
        (i32::MIN, i32::MAX, u32::MAX, 1),
    ] {
        let cmd = Command::CaptureRegion { x, y, w, h };
        let wire = encode(&cmd);
        assert_eq!(wire[0], 0x37, "opcode must be CAPTURE_REGION");
        assert_eq!(wire.len(), 17, "payload is exactly 16 bytes (4x4)");
        assert_eq!(&wire[1..5], &x.to_be_bytes(), "x big-endian");
        assert_eq!(&wire[5..9], &y.to_be_bytes(), "y big-endian");
        assert_eq!(&wire[9..13], &w.to_be_bytes(), "w big-endian");
        assert_eq!(&wire[13..17], &h.to_be_bytes(), "h big-endian");
        assert_eq!(decode(&wire).expect("decode CAPTURE_REGION"), cmd);
    }
    // Exact-length errors: short and over-long payloads are protocol
    // errors, never silently accepted.
    let wire = encode(&Command::CaptureRegion {
        x: 1,
        y: 2,
        w: 3,
        h: 4,
    });
    assert!(
        decode(&wire[..1 + 11]).is_err(),
        "11-byte payload must error"
    );
    assert!(decode(&wire[..1 + 8]).is_err(), "8-byte payload must error");
    let mut long = wire.clone();
    long.push(0);
    assert!(decode(&long).is_err(), "13-byte payload must error");
}

#[test]
fn monitors_info_wire_roundtrip() {
    // MONITORS_INFO: opcode 0x38, u8 count + 17 bytes per monitor
    // (i32 x, i32 y, u32 w, u32 h, u8 is_primary), all big-endian.
    let mons = vec![
        MonitorInfo {
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
            is_primary: true,
        },
        MonitorInfo {
            x: -1920,
            y: -360,
            w: 1920,
            h: 1200,
            is_primary: false,
        },
    ];
    let cmd = Command::MonitorsInfo {
        monitors: mons.clone(),
    };
    let wire = encode(&cmd);
    assert_eq!(wire[0], 0x38, "opcode must be MONITORS_INFO");
    assert_eq!(wire.len(), 1 + 1 + 2 * 17, "count + 17 bytes per monitor");
    assert_eq!(wire[1], 2, "count byte");
    // First monitor: x, y, w, h BE + primary flag.
    assert_eq!(&wire[2..6], &0i32.to_be_bytes());
    assert_eq!(&wire[6..10], &0i32.to_be_bytes());
    assert_eq!(&wire[10..14], &1920u32.to_be_bytes());
    assert_eq!(&wire[14..18], &1080u32.to_be_bytes());
    assert_eq!(wire[18], 1);
    assert_eq!(decode(&wire).expect("decode MONITORS_INFO"), cmd);

    // Zero monitors round-trips too (an empty host is a valid answer).
    let empty = Command::MonitorsInfo {
        monitors: Vec::new(),
    };
    let ew = encode(&empty);
    assert_eq!(ew.len(), 2);
    assert_eq!(decode(&ew).unwrap(), empty);

    // Truncation anywhere in the rows must error, not panic.
    for cut in [2usize, 1 + 17, 1 + 2 * 17 - 1] {
        assert!(
            decode(&wire[..cut]).is_err(),
            "truncated at {cut} must error"
        );
    }
    // An over-long payload must error.
    let mut long = wire.clone();
    long.push(0);
    assert!(decode(&long).is_err());
    // A count byte larger than the payload must error.
    let mut bad = encode(&empty);
    bad[1] = 3;
    assert!(decode(&bad).is_err());
    // 255 monitors is the wire cap; encoding clamps over-long lists.
    let many: Vec<MonitorInfo> = (0..300)
        .map(|i| MonitorInfo {
            x: i,
            y: 0,
            w: 100,
            h: 100,
            is_primary: false,
        })
        .collect();
    let mw = encode(&Command::MonitorsInfo { monitors: many });
    assert_eq!(mw[1], 255);
    let back = decode(&mw).unwrap();
    let Command::MonitorsInfo { monitors } = back else {
        panic!("expected MonitorsInfo");
    };
    assert_eq!(monitors.len(), 255);
}

#[test]
fn clipboard_wire_roundtrip_and_malformed() {
    // CLIPBOARD: opcode 0x50, 1B kind + UTF-8 text. Empty text is
    // a valid "clear" command; any nonzero kind is a protocol error.
    let cmd = Command::Clipboard {
        kind: 0,
        text: "héllo ✓".as_bytes().to_vec(),
    };
    let wire = encode(&cmd);
    assert_eq!(wire[0], 0x50, "opcode must be CLIPBOARD");
    assert_eq!(wire[1], 0, "kind byte is text");
    assert_eq!(&wire[2..], "héllo ✓".as_bytes());
    assert_eq!(decode(&wire).expect("decode CLIPBOARD"), cmd);

    // Empty text (clear) round-trips.
    let clear = Command::Clipboard {
        kind: 0,
        text: Vec::new(),
    };
    let cw = encode(&clear);
    assert_eq!(cw.len(), 2);
    assert_eq!(decode(&cw).unwrap(), clear);

    // A payload with only the kind byte is the empty-text clear; still
    // valid. But a missing kind byte must error.
    assert!(decode(&[0x50]).is_err(), "bare opcode must error");

    // Unknown kinds must error, never be misinterpreted.
    for kind in [1u8, 2, 0xFF] {
        let mut bad = encode(&cmd);
        bad[1] = kind;
        assert!(decode(&bad).is_err(), "kind {kind} must error");
    }
}

#[test]
fn reassembles_frames_over_inmemory() {
    use crate::screen::synthetic_frame;
    use ovtransport::InMemoryCarrier;

    // Build a directly-established session pair (no handshake needed for
    // the reassembly logic): use a symmetric in-memory session.
    let (id_a, id_b) = (
        ovcore::identity::Identity::generate(),
        ovcore::identity::Identity::generate(),
    );
    let (sess_a, sess_b) = established_pair(&id_a, &id_b);
    let (ca, cb) = InMemoryCarrier::pair();

    let mut host = TerminalSession::new(sess_a, ca);
    let mut view = TerminalSession::new(sess_b, cb);

    let seqs = [0u64, 1, 2];
    for &seq in &seqs {
        let frame = synthetic_frame(seq, 200, 64);
        host.send_frame(&frame, 4096).expect("send frame");
    }

    let mut got = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while got.len() < seqs.len() && std::time::Instant::now() < deadline {
        view.pump().expect("view pump");
        if let Some(f) = view.take_frame() {
            got.push(f);
        }
    }
    assert_eq!(got.len(), seqs.len(), "all frames must arrive");
    for (g, &seq) in got.iter().zip(&seqs) {
        let expect = synthetic_frame(seq, 200, 64);
        assert_eq!(g.seq, seq);
        assert_eq!(g.width, 200);
        assert_eq!(g.height, 64);
        assert!(g.is_complete());
        assert_eq!(g.pixels, expect.pixels, "frame {seq} must be byte-exact");
    }
}

fn inmem_pair() -> (TerminalSession, TerminalSession) {
    let (id_a, id_b) = (
        ovcore::identity::Identity::generate(),
        ovcore::identity::Identity::generate(),
    );
    let (sess_a, sess_b) = established_pair(&id_a, &id_b);
    let (ca, cb) = ovtransport::InMemoryCarrier::pair();
    (
        TerminalSession::new(sess_a, ca),
        TerminalSession::new(sess_b, cb),
    )
}

#[test]
fn oversized_command_is_rejected_without_desyncing_the_session() {
    let (mut host, mut view) = inmem_pair();
    let got = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<u8>>::new()));
    let sink = got.clone();
    view.on_cmd(Some(Box::new(move |c: &Command| {
        if let Command::Clipboard { text, .. } = c {
            sink.lock().unwrap().push(text.clone());
        }
    })));
    // 70 KB clipboard: one byte over is enough, but go well past.
    let huge = Command::Clipboard {
        kind: 0,
        text: vec![b'x'; 70_000],
    };
    assert!(matches!(
        host.send(&huge),
        Err(TransportError::FrameTooLarge(_))
    ));
    // Exactly at the limit still goes out.
    let max = Command::Clipboard {
        kind: 0,
        text: vec![b'y'; MAX_COMMAND_LEN - 2],
    };
    assert_eq!(encode(&max).len(), MAX_COMMAND_LEN);
    host.send(&max).expect("max-size command fits one record");
    // ...and the session is still in sync for the next record.
    host.send(&Command::Clipboard {
        kind: 0,
        text: b"after".to_vec(),
    })
    .unwrap();
    for _ in 0..5 {
        view.pump().unwrap();
    }
    let got = got.lock().unwrap();
    assert_eq!(got.len(), 2, "both in-limit records decrypt");
    assert_eq!(got[1], b"after");
    assert_eq!(view.stats().decrypt_errors, 0);
}

#[test]
fn utf8_truncate_and_chunk_respect_char_boundaries() {
    let s = "aé✓b"; // 1 + 2 + 3 + 1 bytes
    assert_eq!(truncate_utf8(s, 100), s);
    assert_eq!(truncate_utf8(s, 2), "a");
    assert_eq!(truncate_utf8(s, 3), "aé");
    assert_eq!(truncate_utf8(s, 5), "aé");
    let long = "✓".repeat(30_000); // 90 000 bytes
    let pieces = chunk_utf8(&long, MAX_TEXT_PAYLOAD);
    assert!(pieces.len() >= 2);
    assert!(pieces.iter().all(|p| p.len() <= MAX_TEXT_PAYLOAD));
    assert_eq!(pieces.concat(), long);
    assert_eq!(chunk_utf8("", 10), vec![""]);
}

#[test]
fn large_cursor_shapes_are_downscaled_to_fit() {
    let big = Command::CursorShape {
        hotspot_x: 127,
        hotspot_y: 64,
        w: 128,
        h: 128,
        bgra: vec![0xAB; 128 * 128 * 4],
    };
    assert!(
        encode(&big).len() > MAX_COMMAND_LEN,
        "precondition: too big for one record"
    );
    let capped = cap_cursor_shape(big, MAX_CURSOR_DIM);
    match &capped {
        Command::CursorShape {
            hotspot_x,
            hotspot_y,
            w,
            h,
            bgra,
        } => {
            assert_eq!((*w, *h), (64, 64));
            assert_eq!((*hotspot_x, *hotspot_y), (63, 32));
            assert_eq!(bgra.len(), 64 * 64 * 4);
            assert!(bgra.iter().all(|&b| b == 0xAB));
        }
        other => panic!("unexpected {other:?}"),
    }
    // Wire-valid after capping, and small shapes pass through untouched.
    assert_eq!(decode(&encode(&capped)).unwrap(), capped);
    let small = Command::CursorShape {
        hotspot_x: 1,
        hotspot_y: 2,
        w: 32,
        h: 16,
        bgra: vec![1; 32 * 16 * 4],
    };
    assert_eq!(cap_cursor_shape(small.clone(), MAX_CURSOR_DIM), small);
    // Non-square: aspect kept.
    let wide = Command::CursorShape {
        hotspot_x: 0,
        hotspot_y: 0,
        w: 256,
        h: 64,
        bgra: vec![0; 256 * 64 * 4],
    };
    let Command::CursorShape { w, h, .. } = cap_cursor_shape(wide, MAX_CURSOR_DIM) else {
        unreachable!()
    };
    assert_eq!((w, h), (64, 16));
}

#[test]
fn take_latest_frame_skips_stale_frames_and_queue_is_capped() {
    use crate::screen::synthetic_frame;
    let (mut host, mut view) = inmem_pair();
    for seq in 0..10u64 {
        host.send_frame(&synthetic_frame(seq, 64, 32), 4096)
            .unwrap();
    }
    for _ in 0..5 {
        view.pump().unwrap();
    }
    assert!(
        view.finished_frames.len() <= MAX_QUEUED_FRAMES,
        "queue is bounded"
    );
    let f = view.take_latest_frame().expect("newest frame");
    assert_eq!(f.seq, 9);
    assert_eq!(f.pixels, synthetic_frame(9, 64, 32).pixels);
    assert!(
        view.take_latest_frame().is_none(),
        "older frames were discarded"
    );
    assert!(view.take_frame().is_none());
    // A recycled buffer is reused for the next decode.
    view.recycle_frame(f);
    host.send_frame(&synthetic_frame(10, 64, 32), 4096).unwrap();
    view.pump().unwrap();
    let g = view.take_frame().expect("next frame");
    assert_eq!(g.seq, 10);
    assert_eq!(g.pixels, synthetic_frame(10, 64, 32).pixels);
}
