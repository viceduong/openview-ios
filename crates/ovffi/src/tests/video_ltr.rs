//! The iOS assembler's long-term-reference recovery and HEVC pass-through:
//! `VIDEO_LTR` units are acknowledged, a gap asks for `VIDEO_RECOVER`
//! naming the newest LTR (with a `CAP_LTR` host), the recovery unit is
//! delivered without a keyframe, a shell decoder failure falls back to an
//! IDR request, and an opted-in shell gets HEVC packets (codec 2, VPS).

use std::ffi::c_void;
use std::sync::Mutex;

use ovapp::terminal::{encode, Command, CAP_H264, CAP_HEVC, CAP_LTR, SMALL_CONTROL_LEN};
use ovapp::tiles::TileRect;
use ovapp::video::{
    VideoHeader, CODEC_H264, CODEC_HEVC, VIDEO_KEYFRAME, VIDEO_LTR, VIDEO_RECOVERY,
};

#[derive(Debug, Clone, PartialEq)]
struct Got {
    seq: u64,
    codec: u32,
    vps: Vec<u8>,
    sps: Vec<u8>,
    data: Vec<u8>,
}

static GOT: Mutex<Vec<Got>> = Mutex::new(Vec::new());
/// The two tests share `GOT`: one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

extern "C" fn on_video(_u: *mut c_void, p: *const crate::OvVideoPacket) {
    // SAFETY: the assembler hands a valid packet for the callback's duration.
    let p = unsafe { &*p };
    let bytes = |ptr: *const u8, len: usize| {
        if ptr.is_null() {
            Vec::new()
        } else {
            // SAFETY: non-null pointer + length from the packet.
            unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec()
        }
    };
    GOT.lock().unwrap().push(Got {
        seq: p.seq,
        codec: p.codec,
        vps: bytes(p.vps, p.vps_len),
        sps: bytes(p.sps, p.sps_len),
        data: bytes(p.data, p.data_len),
    });
}

fn feed(screen: *mut crate::OvScreen, cmd: &Command) -> i32 {
    let wire = encode(cmd);
    // SAFETY: a live screen and a valid buffer.
    unsafe { crate::ov_screen_feed(screen, wire.as_ptr(), wire.len()) }
}

fn replies(screen: *mut crate::OvScreen) -> Vec<Command> {
    let mut v = Vec::new();
    loop {
        let mut out = [0u8; SMALL_CONTROL_LEN];
        // SAFETY: a live screen and a buffer of the right size.
        match unsafe { crate::ov_screen_take_pong_reply(screen, out.as_mut_ptr(), out.len()) } {
            1 => v.push(ovapp::terminal::decode(&out).unwrap()),
            _ => return v,
        }
    }
}

fn header(seq: u64, flags: u8, codec: u8) -> VideoHeader {
    VideoHeader {
        seq,
        flags,
        codec,
        rect: TileRect {
            x: 0,
            y: 0,
            w: 64,
            h: 64,
        },
        screen_w: 64,
        screen_h: 64,
        pts_us: 1,
    }
}

/// The reliable path (VIDEO_BEGIN + FRAME_DATA + FRAME_END), which also
/// makes the assembler queue its CAPS on the first unit.
fn unit(screen: *mut crate::OvScreen, h: VideoHeader, data: &[u8]) {
    let begin = Command::VideoBegin {
        header: h,
        compressed_len: data.len() as u32,
    };
    assert_eq!(feed(screen, &begin), 0);
    assert_eq!(feed(screen, &Command::FrameData(data.to_vec())), 0);
    assert_eq!(feed(screen, &Command::FrameEnd(h.seq)), 0);
}

const H264_IDR: [u8; 18] = [
    0, 0, 0, 1, 0x67, 0x4D, 0x00, 0x28, 0, 0, 0, 1, 0x68, 0xEE, 0, 0, 1, 0x65,
];
const H264_P: [u8; 6] = [0, 0, 0, 1, 0x41, 0x9A];

#[test]
fn screen_acks_ltrs_and_recovers_without_a_keyframe() {
    let _one = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    GOT.lock().unwrap().clear();
    // SAFETY: a fresh screen, freed at the end, used from this thread only.
    unsafe {
        let screen = crate::ov_screen_new();
        assert_eq!(
            crate::ov_screen_set_video_sink(screen, std::ptr::null_mut(), Some(on_video)),
            0
        );
        // The host speaks CAP_LTR.
        assert_eq!(
            feed(
                screen,
                &Command::Caps {
                    flags: CAP_LTR | ovapp::terminal::CAP_TILES
                }
            ),
            0
        );
        unit(
            screen,
            header(0, VIDEO_KEYFRAME | VIDEO_LTR, CODEC_H264),
            &H264_IDR,
        );
        unit(screen, header(1, VIDEO_LTR, CODEC_H264), &H264_P);
        unit(screen, header(2, 0, CODEC_H264), &H264_P);
        let r = replies(screen);
        assert!(
            matches!(r.first(), Some(Command::Caps { flags }) if flags & CAP_LTR != 0 && flags & CAP_H264 != 0 && flags & CAP_HEVC == 0),
            "{r:?}"
        );
        assert_eq!(
            r[1..],
            [
                Command::VideoLtrAck { seq: 0 },
                Command::VideoLtrAck { seq: 1 }
            ]
        );
        // Seq 3 lost: VIDEO_RECOVER from the newest LTR (1), not an IDR.
        unit(screen, header(4, 0, CODEC_H264), &H264_P);
        assert_eq!(replies(screen), [Command::VideoRecover { ltr_seq: 1 }]);
        unit(screen, header(5, VIDEO_RECOVERY, CODEC_H264), &H264_P);
        unit(screen, header(6, 0, CODEC_H264), &H264_P);
        let seqs: Vec<u64> = GOT.lock().unwrap().iter().map(|g| g.seq).collect();
        assert_eq!(seqs, [0, 1, 2, 5, 6], "recovered at 5 without a keyframe");
        // The shell's decoder fails: LTRs are forgotten, an IDR is asked.
        std::thread::sleep(ovapp::terminal::KEYFRAME_RETRY);
        assert_eq!(crate::ov_screen_request_video_idr(screen), 0);
        assert_eq!(replies(screen), [Command::VideoIdrRequest { have_seq: 6 }]);
        crate::ov_screen_free(screen);
    }
}

#[test]
fn screen_passes_hevc_through_when_opted_in() {
    let _one = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: fresh screens, freed at the end, used from this thread only.
    unsafe {
        // Opted out of LTR, into HEVC.
        let screen = crate::ov_screen_new();
        crate::ov_screen_set_video_sink(screen, std::ptr::null_mut(), Some(on_video));
        assert_eq!(
            crate::ov_screen_set_video_caps(screen, crate::OV_VIDEO_CAP_HEVC),
            0
        );
        assert_eq!(crate::ov_screen_set_video_caps(std::ptr::null_mut(), 0), -1);
        let nal = |t: u8, b: u8| [0u8, 0, 0, 1, t << 1, 1, b];
        let key: Vec<u8> = [nal(32, 1), nal(33, 2), nal(34, 3), nal(19, 4)].concat();
        let before = GOT.lock().unwrap().len();
        unit(screen, header(0, VIDEO_KEYFRAME, CODEC_HEVC), &key);
        let r = replies(screen);
        assert!(
            matches!(r.first(), Some(Command::Caps { flags }) if flags & CAP_HEVC != 0 && flags & CAP_LTR == 0),
            "{r:?}"
        );
        {
            let got = GOT.lock().unwrap();
            let g = got
                .iter()
                .skip(before)
                .find(|g| g.codec == 2)
                .expect("HEVC packet");
            assert_eq!(g.vps, [32 << 1, 1, 1]);
            assert_eq!(g.sps, [33 << 1, 1, 2]);
            assert_eq!(g.data, [0, 0, 0, 3, 19 << 1, 1, 4]);
        }
        // A keyframe without VPS cannot start VideoToolbox: IDR request.
        let bad: Vec<u8> = [nal(33, 2), nal(34, 3), nal(19, 4)].concat();
        std::thread::sleep(ovapp::terminal::KEYFRAME_RETRY);
        unit(screen, header(1, VIDEO_KEYFRAME, CODEC_HEVC), &bad);
        assert_eq!(replies(screen), [Command::VideoIdrRequest { have_seq: 1 }]);
        crate::ov_screen_free(screen);
    }
}
