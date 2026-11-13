//! Wire round trips for tile updates, CAPS, H.264 and the local media
//! commands, plus rejection of malformed / unknown commands.

use super::*;
use crate::screen::PixelFormat;
use crate::tiles::TileRect;

#[test]
fn local_media_commands_round_trip() {
    let header = crate::video::VideoHeader {
        seq: 77,
        flags: crate::video::VIDEO_KEYFRAME,
        codec: crate::video::CODEC_H264,
        rect: TileRect {
            x: 64,
            y: 0,
            w: 128,
            h: 64,
        },
        screen_w: 640,
        screen_h: 480,
        pts_us: 123_456,
    };
    for cmd in [
        Command::VideoUnit {
            header,
            data: vec![0, 0, 0, 1, 0x65, 9, 9],
        },
        Command::MediaGap {
            kind: GAP_AUDIO,
            packets: 3,
            samples: 1440,
        },
        Command::MediaGap {
            kind: GAP_VIDEO,
            packets: 1,
            samples: 0,
        },
    ] {
        assert_eq!(decode(&encode(&cmd)).unwrap(), cmd);
    }
    // A VIDEO_UNIT whose declared length disagrees with its body.
    let mut bad = encode(&Command::VideoUnit {
        header,
        data: vec![1; 10],
    });
    bad.pop();
    assert!(decode(&bad).is_err());
    assert!(decode(&[OpCode::MediaGap as u8, 1, 0]).is_err());
    assert_eq!(CAP_MEDIA, 1 << 15);
    assert_eq!(
        CAP_MEDIA & (LOCAL_CAPS | VIEWER_W2_CAPS | CAP_CACHE),
        0,
        "a fresh bit"
    );
}

#[test]
fn new_commands_round_trip() {
    let cmds = [
        Command::TilesBegin {
            seq: 9,
            base_seq: 8,
            width: 1920,
            height: 1080,
            format: PixelFormat::Bgra,
            codec: FrameCodec::Zstd,
            total: 64 * 64 * 4 * 2,
            compressed_len: 1234,
            rects: vec![
                TileRect {
                    x: 0,
                    y: 0,
                    w: 64,
                    h: 64,
                },
                TileRect {
                    x: 1856,
                    y: 1024,
                    w: 64,
                    h: 56,
                },
            ],
        },
        Command::TilesBegin {
            seq: 1,
            base_seq: 0,
            width: 10,
            height: 10,
            format: PixelFormat::Bgra,
            codec: FrameCodec::Deflate,
            total: 0,
            compressed_len: 0,
            rects: vec![],
        },
        Command::KeyframeRequest { have_seq: 77 },
        Command::Caps { flags: LOCAL_CAPS },
        Command::Caps {
            flags: LOCAL_CAPS | CAP_H264,
        },
        Command::VideoIdrRequest { have_seq: 12 },
        Command::VideoBegin {
            header: crate::video::VideoHeader {
                seq: 5,
                flags: crate::video::VIDEO_KEYFRAME,
                codec: crate::video::CODEC_H264,
                rect: TileRect {
                    x: 640,
                    y: 320,
                    w: 640,
                    h: 360,
                },
                screen_w: 1920,
                screen_h: 1080,
                pts_us: 123_456_789,
            },
            compressed_len: 40_000,
        },
        Command::VideoBegin {
            header: crate::video::VideoHeader {
                seq: 6,
                flags: crate::video::VIDEO_END,
                codec: crate::video::CODEC_H264,
                rect: TileRect {
                    x: 0,
                    y: 0,
                    w: 1920,
                    h: 1080,
                },
                screen_w: 1920,
                screen_h: 1080,
                pts_us: 1,
            },
            compressed_len: 0,
        },
        Command::FrameBegin {
            seq: 3,
            width: 2,
            height: 2,
            stride: 8,
            format: PixelFormat::Bgra,
            total: 16,
            codec: FrameCodec::Zstd,
            compressed_len: 20,
        },
    ];
    for c in cmds {
        assert_eq!(decode(&encode(&c)).unwrap(), c);
    }
    // CAPS / KEYFRAME_REQUEST fit the 9-byte reply path (iOS).
    assert_eq!(encode(&Command::Caps { flags: 7 }).len(), SMALL_CONTROL_LEN);
    assert_eq!(
        encode(&Command::KeyframeRequest { have_seq: 1 }).len(),
        SMALL_CONTROL_LEN
    );
    // Forward compatibility: unknown CAPS bits / extra bytes are kept /
    // ignored, an empty KEYFRAME_REQUEST is accepted.
    assert_eq!(
        decode(&[0x60, 0, 0, 1, 0xFF, 1, 2, 3, 4, 5]).unwrap(),
        Command::Caps { flags: 0x1FF }
    );
    assert_eq!(
        decode(&[0x24]).unwrap(),
        Command::KeyframeRequest { have_seq: 0 }
    );
}

#[test]
fn malformed_tiles_begin_is_rejected() {
    let good = encode(&Command::TilesBegin {
        seq: 1,
        base_seq: 0,
        width: 100,
        height: 100,
        format: PixelFormat::Bgra,
        codec: FrameCodec::Raw,
        total: 4,
        compressed_len: 4,
        rects: vec![TileRect {
            x: 99,
            y: 99,
            w: 1,
            h: 1,
        }],
    });
    assert!(decode(&good).is_ok());
    // Truncated rect list / header.
    assert!(decode(&good[..good.len() - 1]).is_err());
    assert!(decode(&good[..20]).is_err());
    // A rect outside the frame, or empty.
    let mut bad = good.clone();
    bad[1 + 36 + 4..1 + 36 + 6].copy_from_slice(&2u16.to_be_bytes()); // w = 2
    assert!(decode(&bad).is_err());
    let mut bad = good.clone();
    bad[1 + 36 + 6..1 + 36 + 8].copy_from_slice(&0u16.to_be_bytes()); // h = 0
    assert!(decode(&bad).is_err());
    // Unknown codec.
    let mut bad = good.clone();
    bad[1 + 25] = 9;
    assert!(decode(&bad).is_err());
    assert!(decode(&[0x60, 0, 0]).is_err(), "short CAPS");
}

/// A viewer that predates tile updates sees TILES_BEGIN / CAPS as
/// unknown opcodes; the session counts a decode error and carries on —
/// which is why the host never sends tiles without CAP_TILES.
#[test]
fn unknown_opcodes_do_not_break_a_session() {
    assert!(decode(&[0x7E, 1, 2]).is_err());
}
