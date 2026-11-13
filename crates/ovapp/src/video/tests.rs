use std::time::{Duration, Instant};

use super::convert::{bgra_to_nv12_band, nv12_to_bgra_band};
use super::*;
use crate::sendq::FrameMailbox;
use crate::tiles::{TileMap, TileRect};

/// A Main-profile SPS (1280x720 in MBs) written field by field, with an
/// optional VUI (`full_range`, timing, bitstream restriction).
fn make_sps(vui: Option<(bool, u8)>) -> Vec<u8> {
    let mut w = sps::Writer::default();
    w.bits(0x67, 8); // NAL header: SPS
    w.bits(77, 8); // profile_idc Main
    w.bits(0, 8); // constraint flags
    w.bits(31, 8); // level 3.1
    w.ue(0); // sps id
    w.ue(0); // log2_max_frame_num_minus4
    w.ue(0); // poc type 0
    w.ue(2); // log2_max_poc_lsb_minus4
    w.ue(1); // max_num_ref_frames
    w.bit(0); // gaps
    w.ue(79); // width 80 MBs
    w.ue(44); // height 45 map units
    w.bit(1); // frame_mbs_only
    w.bit(1); // direct_8x8
    w.bit(0); // no cropping
    match vui {
        None => w.bit(0),
        Some((full, matrix)) => {
            w.bit(1);
            w.bit(1); // aspect ratio present
            w.bits(1, 8); // 1:1
            w.bit(0); // overscan
            w.bit(1); // video signal type
            w.bits(5, 3);
            w.bit(full as u32);
            w.bit(1);
            w.bits(2, 8);
            w.bits(2, 8);
            w.bits(u32::from(matrix), 8);
            w.bit(0); // chroma loc
            w.bit(1); // timing
            w.bits(1, 32);
            w.bits(60, 32);
            w.bit(0);
            w.bit(1); // nal hrd
            w.ue(0);
            w.bits(4, 4);
            w.bits(6, 4);
            w.ue(4999);
            w.ue(9999);
            w.bit(1);
            w.bits(23, 5);
            w.bits(23, 5);
            w.bits(23, 5);
            w.bits(24, 5);
            w.bit(0); // vcl hrd
            w.bit(1); // low_delay_hrd
            w.bit(0); // pic struct
            w.bit(1); // bitstream restriction
            w.bit(1);
            w.ue(0);
            w.ue(0);
            w.ue(16);
            w.ue(16);
            w.ue(0); // max_num_reorder_frames
            w.ue(1);
        }
    }
    sps::escape(&w.finish())
}

#[test]
fn sps_without_vui_gains_bt709_limited() {
    let raw = make_sps(None);
    let before = sps::summary(&raw).unwrap();
    assert!(!before.colour.vui_present);
    let fixed = sps::force_bt709_limited(&raw).unwrap();
    let after = sps::summary(&fixed).unwrap();
    assert!(after.colour.is_bt709_limited(), "{:?}", after.colour);
    assert_eq!((after.width_mbs, after.height_map_units), (80, 45));
    assert_eq!(after.profile_idc, 77);
    assert_eq!(after.max_num_reorder_frames, Some(0));
    // Idempotent.
    assert!(sps::force_bt709_limited(&fixed).is_none());
}

#[test]
fn sps_vui_colour_is_replaced_and_the_rest_kept() {
    let raw = make_sps(Some((true, 6)));
    let before = sps::summary(&raw).unwrap();
    assert!(before.colour.full_range && before.colour.matrix == 6);
    let fixed = sps::force_bt709_limited(&raw).unwrap();
    let after = sps::summary(&fixed).unwrap();
    assert!(after.colour.is_bt709_limited());
    assert_eq!(after.timing, Some((1, 60)), "timing info preserved");
    assert_eq!(
        after.max_num_reorder_frames,
        Some(0),
        "bitstream restriction preserved"
    );
    assert_eq!((after.width_mbs, after.height_map_units), (80, 45));
}

#[test]
fn sps_emulation_prevention_round_trips() {
    let rbsp = [0x67u8, 0, 0, 1, 0, 0, 0, 2, 0, 0, 3, 7];
    let esc = sps::escape(&rbsp);
    assert_eq!(esc, vec![0x67, 0, 0, 3, 1, 0, 0, 3, 0, 2, 0, 0, 3, 3, 7]);
    assert_eq!(sps::unescape(&esc), rbsp);
}

#[test]
fn param_set_cache_rewrites_sps_colour() {
    let mut c = ParamSetCache::default();
    let mut au = vec![0, 0, 0, 1];
    au.extend_from_slice(&make_sps(None));
    au.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xCE, 0x3C, 0x80, 0, 0, 0, 1, 0x65, 0x88]);
    let out = c.fix(au);
    let parsed = AccessUnit::from_annexb(&out);
    assert!(sps::colour(parsed.sps.as_deref().unwrap())
        .unwrap()
        .is_bt709_limited());
    assert_eq!(parsed.pps.as_deref(), Some(&[0x68, 0xCE, 0x3C, 0x80][..]));
    assert!(parsed.idr);
}

fn map_with(w: u32, h: u32, rect: TileRect) -> TileMap {
    let mut m = TileMap::new(w, h);
    m.mark_rect(&rect);
    m
}

const VID: TileRect = TileRect {
    x: 640,
    y: 320,
    w: 640,
    h: 360,
};

#[test]
fn video_header_round_trips_and_validates() {
    let h = VideoHeader {
        seq: 77,
        flags: VIDEO_KEYFRAME,
        codec: CODEC_H264,
        rect: TileRect {
            x: 64,
            y: 128,
            w: 640,
            h: 360,
        },
        screen_w: 1920,
        screen_h: 1080,
        pts_us: 1_700_000_000_123_456,
    };
    let mut b = Vec::new();
    h.encode(4321, &mut b);
    assert_eq!(b.len(), VIDEO_HEADER_LEN);
    assert_eq!(VideoHeader::decode(&b).unwrap(), (h, 4321));
    // Rect outside the screen / unknown codec / bad length: refused.
    let mut bad = h;
    bad.rect.x = 1800;
    let mut b2 = Vec::new();
    bad.encode(0, &mut b2);
    assert!(VideoHeader::decode(&b2).is_err());
    let mut b3 = b.clone();
    b3[9] = 9;
    assert!(VideoHeader::decode(&b3).is_err());
    assert!(VideoHeader::decode(&b[..20]).is_err());
}

#[test]
fn annexb_split_and_avcc() {
    // AUD, SPS, PPS (3-byte start code), IDR with an emulation-free tail.
    let mut s = vec![0, 0, 0, 1, 0x09, 0xF0];
    s.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1F]);
    s.extend_from_slice(&[0, 0, 1, 0x68, 0xCE, 0x3C, 0x80]);
    s.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x10]);
    let nals = annexb_nals(&s);
    assert_eq!(nals.len(), 4);
    assert_eq!(
        nals.iter().map(|n| nal_type(n)).collect::<Vec<_>>(),
        [9, 7, 8, 5]
    );
    assert!(annexb_is_idr(&s));
    let au = AccessUnit::from_annexb(&s);
    assert_eq!(au.sps.as_deref(), Some(&[0x67, 0x42, 0x00, 0x1F][..]));
    assert_eq!(au.pps.as_deref(), Some(&[0x68, 0xCE, 0x3C, 0x80][..]));
    assert!(au.idr);
    assert_eq!(au.avcc, [0, 0, 0, 5, 0x65, 0x88, 0x84, 0x00, 0x10]);
    // A P-frame: no parameter sets.
    let p = [0, 0, 0, 1, 0x41, 0x9A, 0x02];
    let au = AccessUnit::from_annexb(&p);
    assert!(au.sps.is_none() && !au.idr);
    assert_eq!(au.avcc, [0, 0, 0, 3, 0x41, 0x9A, 0x02]);
}

#[test]
fn param_set_cache_completes_a_bare_idr() {
    let mut c = ParamSetCache::default();
    let first = [
        0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9,
    ]
    .to_vec();
    assert_eq!(c.fix(first.clone()), first);
    let p = [0, 0, 0, 1, 0x41, 7].to_vec();
    assert_eq!(c.fix(p.clone()), p);
    let bare = [0, 0, 0, 1, 0x65, 8].to_vec();
    let fixed = c.fix(bare);
    let au = AccessUnit::from_annexb(&fixed);
    assert_eq!(au.sps.as_deref(), Some(&[0x67, 1, 2][..]));
    assert_eq!(au.pps.as_deref(), Some(&[0x68, 3][..]));
    assert!(au.idr);
}

#[test]
fn colour_round_trip_is_close() {
    let (w, h) = (64u32, 32u32);
    let mut src = vec![0u8; (w * h * 4) as usize];
    synth_video(
        &mut src,
        w,
        TileRect {
            x: 0,
            y: 0,
            w: 64,
            h: 32,
        },
        5,
    );
    let mut nv = Vec::new();
    bgra_to_nv12(&src, w as usize * 4, w, h, &mut nv);
    assert_eq!(nv.len(), nv12_len(w, h));
    let mut back = Vec::new();
    nv12_to_bgra(&nv, w as usize, (w * h) as usize, w, h, &mut back).unwrap();
    // 4:2:0 subsampling of the grain costs a few dB, nothing more.
    let p = psnr_bgra(&src, &back);
    assert!(p > 30.0, "psnr {p}");
    // Flat colours survive almost exactly.
    let flat: Vec<u8> = [30u8, 140, 220, 255].repeat((w * h) as usize);
    bgra_to_nv12(&flat, w as usize * 4, w, h, &mut nv);
    nv12_to_bgra(&nv, w as usize, (w * h) as usize, w, h, &mut back).unwrap();
    assert!(psnr_bgra(&flat, &back) > 40.0);
    // Too small a buffer is an error, never a panic.
    assert!(nv12_to_bgra(&nv[..100], w as usize, (w * h) as usize, w, h, &mut back).is_err());
}

#[test]
fn threaded_conversions_match_single_band() {
    let (w, h) = (1280u32, 720u32);
    let mut src = vec![0u8; (w * h * 4) as usize];
    synth_video(
        &mut src,
        w,
        TileRect {
            x: 0,
            y: 0,
            w: 1280,
            h: 720,
        },
        9,
    );
    let mut threaded = Vec::new();
    bgra_to_nv12(&src, w as usize * 4, w, h, &mut threaded);
    let mut single = vec![0u8; nv12_len(w, h)];
    let (yp, uvp) = single.split_at_mut((w * h) as usize);
    bgra_to_nv12_band(&src, w as usize * 4, w as usize, 0, yp, uvp);
    assert_eq!(threaded, single);
    let mut a = Vec::new();
    nv12_to_bgra(&single, w as usize, (w * h) as usize, w, h, &mut a).unwrap();
    let mut b = vec![0u8; (w * h * 4) as usize];
    nv12_to_bgra_band(&single, w as usize, (w * h) as usize, w as usize, 0, &mut b);
    assert_eq!(a, b);
}

#[test]
fn motion_enters_after_sustained_large_change_and_exits_after_rest() {
    let (w, h) = (1920, 1080);
    let mut d = MotionDetector::default();
    let t0 = Instant::now();
    let ms = |n: u64| t0 + Duration::from_millis(n);
    let video = map_with(w, h, VID);
    let quiet = TileMap::new(w, h);
    // Two samples of 30 fps change: not yet.
    let mut t = 0;
    while t < 66 {
        assert_eq!(
            d.observe(ms(t), &video, w, h),
            MotionEvent::None,
            "at {t} ms"
        );
        t += 33;
    }
    // Crossing 65 ms: enter, with the tile-aligned rect.
    let mut entered = None;
    while t < 400 && entered.is_none() {
        if let MotionEvent::Enter(r) = d.observe(ms(t), &video, w, h) {
            entered = Some((t, r));
        }
        t += 33;
    }
    let (te, r) = entered.expect("entered motion");
    assert!((60..=100).contains(&te), "entered at {te} ms");
    assert_eq!(
        r,
        TileRect {
            x: 640,
            y: 320,
            w: 640,
            h: 384
        }
    );
    assert_eq!(d.motion_rect(), Some(r));
    // Keeps playing: stays in motion.
    for _ in 0..30 {
        assert_eq!(d.observe(ms(t), &video, w, h), MotionEvent::None);
        t += 33;
    }
    // Paused: exit only after ~1 s of rest (hysteresis).
    let pause = t;
    let mut exited = None;
    while t < pause + 2000 && exited.is_none() {
        if let MotionEvent::Exit(x) = d.observe(ms(t), &quiet, w, h) {
            exited = Some((t, x));
        }
        t += 33;
    }
    let (tx, x) = exited.expect("exited");
    assert!(
        tx - pause >= 990 && tx - pause <= 1100,
        "exit after {} ms",
        tx - pause
    );
    assert_eq!(x, r);
    assert_eq!(d.motion_rect(), None);
}

#[test]
fn small_or_scattered_changes_never_enter() {
    let (w, h) = (1920, 1080);
    let mut d = MotionDetector::default();
    let t0 = Instant::now();
    // Typing: one tile, for ages.
    let caret = map_with(
        w,
        h,
        TileRect {
            x: 100,
            y: 100,
            w: 10,
            h: 16,
        },
    );
    // A clock + a caret at opposite corners: huge bbox, 2 tiles.
    let mut scattered = map_with(
        w,
        h,
        TileRect {
            x: 0,
            y: 0,
            w: 8,
            h: 8,
        },
    );
    scattered.mark_rect(&TileRect {
        x: 1900,
        y: 1060,
        w: 8,
        h: 8,
    });
    for i in 0..200 {
        let now = t0 + Duration::from_millis(i * 33);
        let m = if i % 2 == 0 { &caret } else { &scattered };
        assert_eq!(d.observe(now, m, w, h), MotionEvent::None);
    }
    // A short burst (two samples, < 65 ms) does not enter either.
    let big = map_with(
        w,
        h,
        TileRect {
            x: 0,
            y: 0,
            w: 1200,
            h: 800,
        },
    );
    for i in 0..2 {
        assert_eq!(
            d.observe(t0 + Duration::from_millis(10_000 + i * 33), &big, w, h),
            MotionEvent::None
        );
    }
    let quiet = TileMap::new(w, h);
    for i in 0..20 {
        assert_eq!(
            d.observe(t0 + Duration::from_millis(11_000 + i * 33), &quiet, w, h),
            MotionEvent::None
        );
    }
}

#[test]
fn short_gaps_keep_the_run_and_a_big_area_snaps_to_full_screen() {
    let (w, h) = (1920, 1080);
    let mut d = MotionDetector::default();
    let t0 = Instant::now();
    // 24 fps content sampled at 30 fps: every 5th sample repeats.
    let page = map_with(
        w,
        h,
        TileRect {
            x: 0,
            y: 0,
            w: 1600,
            h: 1000,
        },
    );
    let quiet = TileMap::new(w, h);
    let mut ev = MotionEvent::None;
    for i in 0..30u64 {
        let m = if i % 5 == 4 { &quiet } else { &page };
        let e = d.observe(t0 + Duration::from_millis(i * 33), m, w, h);
        if e != MotionEvent::None {
            ev = e;
            break;
        }
    }
    assert_eq!(
        ev,
        MotionEvent::Enter(TileRect {
            x: 0,
            y: 0,
            w: 1920,
            h: 1080
        })
    );
}

#[test]
fn motion_rect_grows_at_most_once_per_interval() {
    let (w, h) = (1920, 1080);
    let mut d = MotionDetector::default();
    let t0 = Instant::now();
    let a = map_with(
        w,
        h,
        TileRect {
            x: 0,
            y: 0,
            w: 640,
            h: 384,
        },
    );
    let mut t = 0u64;
    loop {
        if let MotionEvent::Enter(_) = d.observe(t0 + Duration::from_millis(t), &a, w, h) {
            break;
        }
        t += 33;
    }
    let b = map_with(
        w,
        h,
        TileRect {
            x: 0,
            y: 0,
            w: 640,
            h: 640,
        },
    );
    let mut grows = Vec::new();
    for _ in 0..90 {
        t += 33;
        if let MotionEvent::Grow(r) = d.observe(t0 + Duration::from_millis(t), &b, w, h) {
            grows.push(r);
        }
    }
    assert_eq!(
        grows,
        [TileRect {
            x: 0,
            y: 0,
            w: 640,
            h: 640
        }],
        "grew once, then contained"
    );
}

#[test]
fn video_rx_drops_until_keyframe_after_a_gap() {
    let mk = |seq, flags| VideoHeader {
        seq,
        flags,
        codec: CODEC_H264,
        ..Default::default()
    };
    let mut rx = VideoRx::new();
    assert_eq!(
        rx.accept(&mk(0, 0)),
        VideoVerdict::Drop,
        "P-frame before any keyframe"
    );
    assert_eq!(rx.accept(&mk(1, VIDEO_KEYFRAME)), VideoVerdict::Deliver);
    assert_eq!(rx.accept(&mk(2, 0)), VideoVerdict::Deliver);
    assert_eq!(rx.accept(&mk(4, 0)), VideoVerdict::Drop, "gap");
    assert_eq!(rx.accept(&mk(5, 0)), VideoVerdict::Drop, "still broken");
    assert_eq!(rx.accept(&mk(6, VIDEO_KEYFRAME)), VideoVerdict::Deliver);
    assert_eq!(rx.accept(&mk(7, 0)), VideoVerdict::Deliver);
    assert_eq!(rx.accept(&mk(8, VIDEO_END)), VideoVerdict::End);
    assert_eq!(
        rx.accept(&mk(9, 0)),
        VideoVerdict::Drop,
        "a new period starts with an IDR"
    );
    assert_eq!(rx.accept(&mk(10, VIDEO_KEYFRAME)), VideoVerdict::Deliver);
    rx.mark_broken();
    assert_eq!(rx.accept(&mk(11, 0)), VideoVerdict::Drop);
    assert_eq!(rx.dropped, 5);
    // A unit older than what was already seen is stale (no IDR
    // request, no paint, state untouched).
    assert_eq!(rx.accept(&mk(12, VIDEO_KEYFRAME)), VideoVerdict::Deliver);
    assert_eq!(rx.accept(&mk(9, VIDEO_KEYFRAME)), VideoVerdict::Stale);
    assert_eq!(rx.accept(&mk(13, 0)), VideoVerdict::Deliver);
}

#[test]
fn bitrate_follows_area_and_link() {
    assert_eq!(target_bitrate(1920, 1080, 30, None), 12_441_600);
    assert_eq!(target_bitrate(1920, 1080, 30, Some(1_000_000.0)), 5_600_000);
    assert_eq!(target_bitrate(64, 64, 30, Some(1.0)), 500_000);
    assert_eq!(target_bitrate(3840, 2160, 60, None), 40_000_000);
}

/// A fake encoder: "IDR" units start with 0x65, P units with 0x41; it
/// records what it was asked to do.
struct Fake {
    log: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
    first: bool,
}
impl VideoEncode for Fake {
    fn encode(
        &mut self,
        _bgra: &[u8],
        _stride: usize,
        force_idr: bool,
        pts_us: u64,
    ) -> Result<Vec<EncodedAu>, String> {
        let key = force_idr || self.first;
        self.first = false;
        self.log
            .borrow_mut()
            .push(if key { "I".into() } else { "P".into() });
        Ok(vec![EncodedAu {
            data: vec![0, 0, 0, 1, if key { 0x65 } else { 0x41 }, 1],
            keyframe: key,
            pts_us,
        }])
    }
    fn set_bitrate(&mut self, bps: u32) {
        self.log.borrow_mut().push(format!("rate {bps}"));
    }
    fn name(&self) -> String {
        "fake".into()
    }
}

fn fake_factory(log: std::rc::Rc<std::cell::RefCell<Vec<String>>>) -> EncoderFactory {
    Box::new(move |w, h, _fps, _b| {
        log.borrow_mut().push(format!("new {w}x{h}"));
        Ok(Box::new(Fake {
            log: log.clone(),
            first: true,
        }) as Box<dyn VideoEncode>)
    })
}

/// Encoder sizes come in 128 px buckets and survive growth
/// inside them; the coded rect always contains the motion rect.
#[test]
fn encoder_rect_buckets_and_reuses_the_encoder_size() {
    let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let mut vs = VideoSession::new(fake_factory(log.clone()), 30);
    let (sw, sh) = (1920u32, 1080u32);
    let need = TileRect {
        x: 640,
        y: 320,
        w: 576,
        h: 320,
    };
    let r = vs.encoder_rect(need, sw, sh);
    assert_eq!((r.w, r.h), (640, 384));
    let contains = |r: TileRect, n: TileRect| {
        r.x <= n.x && r.y <= n.y && r.x + r.w >= n.x + n.w && r.y + r.h >= n.y + n.h
    };
    assert!(contains(r, need) && u32::from(r.x) + u32::from(r.w) <= sw);
    // An encoder of that size exists: a grown rect that still fits keeps it.
    vs.enc = Some((640, 384, (vs.factory)(640, 384, 30, 1).unwrap()));
    let grown = TileRect {
        x: 576,
        y: 320,
        w: 640,
        h: 384,
    };
    let r2 = vs.encoder_rect(grown, sw, sh);
    assert_eq!((r2.w, r2.h), (640, 384));
    assert!(contains(r2, grown));
    // Growing past it: the next bucket.
    let big = TileRect {
        x: 576,
        y: 256,
        w: 704,
        h: 448,
    };
    let r3 = vs.encoder_rect(big, sw, sh);
    assert_eq!((r3.w, r3.h), (768, 512));
    assert!(contains(r3, big));
    // At the screen edge the rect is pinned inside the screen.
    let edge = TileRect {
        x: 1856,
        y: 1024,
        w: 64,
        h: 56,
    };
    let r4 = vs.encoder_rect(edge, sw, sh);
    assert!(
        contains(r4, edge) && u32::from(r4.x + r4.w) <= sw && u32::from(r4.y + r4.h) <= sh,
        "{r4:?}"
    );
}

#[test]
fn video_session_enters_streams_and_ends_with_a_refresh() {
    let (w, h) = (1920u32, 1080u32);
    let px = vec![0u8; (w * h * 4) as usize];
    let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let mut vs = VideoSession::new(fake_factory(log.clone()), 30);
    let mb = FrameMailbox::default();
    let t0 = Instant::now();
    let video = map_with(w, h, VID);
    let quiet = TileMap::new(w, h);
    let mut t = 0u64;
    let mut first_video_at = None;
    while t < 1500 {
        let now = t0 + Duration::from_millis(t);
        if vs.on_capture(now, &px, w, h, w as usize * 4, &video, true, true, &mb)
            && first_video_at.is_none()
        {
            first_video_at = Some(t);
        }
        while mb.take().is_some() {} // the link drains everything
        t += 33;
    }
    assert!(
        first_video_at.is_some_and(|t| (60..=100).contains(&t)),
        "{first_video_at:?}"
    );
    assert_eq!(
        vs.active_rect(),
        Some(TileRect {
            x: 640,
            y: 320,
            w: 640,
            h: 384
        })
    );
    assert_eq!(log.borrow()[0], "new 640x384");
    assert_eq!(log.borrow()[1], "I");
    assert!(log.borrow()[2..].iter().all(|s| s == "P"));
    // A keyframe request -> the next picture is an IDR.
    vs.request_idr();
    vs.on_capture(
        t0 + Duration::from_millis(t),
        &px,
        w,
        h,
        w as usize * 4,
        &video,
        true,
        true,
        &mb,
    );
    let f = mb.take().expect("video unit");
    assert!(f.video.as_ref().is_some_and(|v| v.is_keyframe()));
    // Rest: END queued, then a refresh of the whole region.
    let mut end = None;
    for _ in 0..40 {
        t += 33;
        vs.on_capture(
            t0 + Duration::from_millis(t),
            &px,
            w,
            h,
            w as usize * 4,
            &quiet,
            true,
            true,
            &mb,
        );
        if let Some(f) = mb.take() {
            end = f.video;
            break;
        }
    }
    let end = end.expect("END marker");
    assert!(end.is_end());
    assert_eq!(
        end.rect,
        TileRect {
            x: 640,
            y: 320,
            w: 640,
            h: 384
        }
    );
    assert_eq!(vs.take_refresh(), Some(end.rect));
    assert_eq!(vs.active_rect(), None);
    let st = vs.stats();
    assert_eq!((st.enters, st.exits), (1, 1));
    assert!(st.frames >= 25, "{st:?}");
}

#[test]
fn video_session_bitrate_follows_the_link() {
    let (w, h) = (1280u32, 720u32);
    let px = vec![0u8; (w * h * 4) as usize];
    let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let cfg = MotionConfig {
        enter_after: Duration::ZERO,
        ..MotionConfig::default()
    };
    let mut vs = VideoSession::with_config(fake_factory(log.clone()), 30, cfg);
    let mb = FrameMailbox::default();
    let all = map_with(
        w,
        h,
        TileRect {
            x: 0,
            y: 0,
            w: 1280,
            h: 720,
        },
    );
    let t0 = Instant::now();
    let step = |vs: &mut VideoSession, i: u64| {
        vs.on_capture(
            t0 + Duration::from_millis(i * 33),
            &px,
            w,
            h,
            w as usize * 4,
            &all,
            true,
            true,
            &mb,
        );
        while mb.take().is_some() {}
    };
    for i in 0..4 {
        step(&mut vs, i);
    }
    assert_eq!(vs.bitrate(), target_bitrate(1280, 720, 30, None));
    // The pacer measures a 500 KB/s link: 70% of it, retargeted live.
    vs.set_link_rate(Some(500_000.0));
    step(&mut vs, 4);
    assert_eq!(vs.bitrate(), 2_800_000);
    assert!(log.borrow().iter().any(|l| l == "rate 2800000"));
    // Small wobble (< 15%): no churn.
    vs.set_link_rate(Some(520_000.0));
    step(&mut vs, 5);
    assert_eq!(vs.bitrate(), 2_800_000);
}

#[test]
fn video_session_backs_off_when_the_queue_is_full_and_stops_without_h264() {
    let (w, h) = (1280u32, 720u32);
    let px = vec![0u8; (w * h * 4) as usize];
    let log = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let cfg = MotionConfig {
        enter_after: Duration::ZERO,
        ..MotionConfig::default()
    };
    let mut vs = VideoSession::with_config(fake_factory(log.clone()), 30, cfg);
    let mb = FrameMailbox::default();
    let all = map_with(
        w,
        h,
        TileRect {
            x: 0,
            y: 0,
            w: 1280,
            h: 720,
        },
    );
    let t0 = Instant::now();
    for i in 0..10u64 {
        vs.on_capture(
            t0 + Duration::from_millis(i * 33),
            &px,
            w,
            h,
            w as usize * 4,
            &all,
            true,
            true,
            &mb,
        );
    }
    // Nothing drained: the queue caps at MAX_VIDEO_QUEUE, never dropped.
    assert_eq!(mb.video_queued(), MAX_VIDEO_QUEUE);
    assert!(vs.stats().skipped > 0);
    // The viewer turns out not to take H.264 (or a caps change): END.
    vs.on_capture(
        t0 + Duration::from_millis(400),
        &px,
        w,
        h,
        w as usize * 4,
        &all,
        true,
        false,
        &mb,
    );
    assert_eq!(mb.video_queued(), MAX_VIDEO_QUEUE + 1);
    assert_eq!(vs.active_rect(), None);
    assert!(vs.take_refresh().is_some());
}

#[test]
fn encoder_failure_falls_back_to_tiles() {
    let (w, h) = (1280u32, 720u32);
    let px = vec![0u8; (w * h * 4) as usize];
    let cfg = MotionConfig {
        enter_after: Duration::ZERO,
        ..MotionConfig::default()
    };
    let mut vs =
        VideoSession::with_config(Box::new(|_, _, _, _| Err("no encoder".to_owned())), 30, cfg);
    let mb = FrameMailbox::default();
    let all = map_with(
        w,
        h,
        TileRect {
            x: 0,
            y: 0,
            w: 1280,
            h: 720,
        },
    );
    let t0 = Instant::now();
    for i in 0..5u64 {
        assert!(!vs.on_capture(
            t0 + Duration::from_millis(i * 33),
            &px,
            w,
            h,
            w as usize * 4,
            &all,
            true,
            true,
            &mb
        ));
    }
    assert_eq!(vs.active_rect(), None);
    assert_eq!(vs.stats().encoder_failures, 1, "retry is rate-limited");
    assert_eq!(vs.take_error().as_deref(), Some("no encoder"));
}
