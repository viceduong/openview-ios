//! Codec identity (HEVC), long-term-reference recovery (viewer `VideoRx`,
//! host `LtrTracker` / `VideoSession`) and the stable encode surface.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use super::*;
use crate::sendq::FrameMailbox;
use crate::tiles::{TileMap, TileRect};

fn hdr(seq: u64, flags: u8) -> VideoHeader {
    VideoHeader {
        seq,
        flags,
        codec: CODEC_H264,
        rect: TileRect {
            x: 0,
            y: 0,
            w: 64,
            h: 64,
        },
        screen_w: 64,
        screen_h: 64,
        pts_us: 0,
    }
}

// ---------------------------------------------------------------------------
// Codec
// ---------------------------------------------------------------------------

/// HEVC NAL unit with a 2-byte header of type `t`.
fn hevc_nal(t: u8, body: &[u8]) -> Vec<u8> {
    let mut v = vec![0, 0, 0, 1, t << 1, 1];
    v.extend_from_slice(body);
    v
}

#[test]
fn codec_byte_round_trips_and_unknown_codecs_are_rejected() {
    assert_eq!(VideoCodec::from_wire(1), Some(VideoCodec::H264));
    assert_eq!(VideoCodec::from_wire(2), Some(VideoCodec::Hevc));
    assert_eq!(VideoCodec::from_wire(3), None);
    for codec in [VideoCodec::H264, VideoCodec::Hevc] {
        let h = VideoHeader {
            codec: codec.wire(),
            flags: VIDEO_KEYFRAME | VIDEO_LTR | VIDEO_RECOVERY,
            ..hdr(7, 0)
        };
        let mut b = Vec::new();
        h.encode(5, &mut b);
        assert_eq!(b.len(), VIDEO_HEADER_LEN, "header length unchanged");
        let (back, len) = VideoHeader::decode(&b).unwrap();
        assert_eq!((back, len), (h, 5));
        assert_eq!(back.video_codec(), codec);
        assert!(back.is_ltr() && back.is_recovery() && back.is_keyframe());
    }
    let mut b = Vec::new();
    VideoHeader {
        codec: 3,
        ..hdr(1, 0)
    }
    .encode(0, &mut b);
    assert!(VideoHeader::decode(&b).is_err());
}

#[test]
fn hevc_nal_types_and_keyframes() {
    let c = VideoCodec::Hevc;
    assert_eq!(c.nal_type(&[19 << 1, 1]), 19);
    assert!(c.is_keyframe_nal(19) && c.is_keyframe_nal(21) && !c.is_keyframe_nal(1));
    assert_eq!(c.param_set(32), Some(ParamSet::Vps));
    assert_eq!(c.param_set(33), Some(ParamSet::Sps));
    assert_eq!(c.param_set(34), Some(ParamSet::Pps));
    // An H.264 IDR byte (0x65) is HEVC type 50 (unspecified): not a key.
    assert!(!c.annexb_is_keyframe(&[0, 0, 0, 1, 0x65, 0]));
    assert!(VideoCodec::H264.annexb_is_keyframe(&[0, 0, 0, 1, 0x65, 0]));
    let idr = hevc_nal(19, &[9, 9]);
    assert!(c.annexb_is_keyframe(&idr));
}

/// HEVC keyframes become self-contained (VPS + SPS + PPS prepended) and
/// split into out-of-band parameter sets + HVCC for VideoToolbox.
#[test]
fn hevc_param_sets_cached_prepended_and_split() {
    let mut cache = ParamSetCache::new(VideoCodec::Hevc);
    let mut first = Vec::new();
    for (t, b) in [
        (35u8, &[0x50][..]),
        (32, &[1]),
        (33, &[2]),
        (34, &[3]),
        (19, &[4, 4]),
    ] {
        first.extend(hevc_nal(t, b));
    }
    assert_eq!(
        cache.fix(first.clone()),
        first,
        "complete keyframe untouched"
    );
    // A later bare IDR gets VPS/SPS/PPS in front, a P-frame stays bare.
    let bare = hevc_nal(19, &[5]);
    let fixed = cache.fix(bare.clone());
    let au = AccessUnit::from_annexb_codec(VideoCodec::Hevc, &fixed);
    assert!(au.idr && au.has_param_sets(VideoCodec::Hevc));
    assert_eq!(au.vps.as_deref(), Some(&[32 << 1, 1, 1][..]));
    assert_eq!(au.sps.as_deref(), Some(&[33 << 1, 1, 2][..]));
    assert_eq!(au.pps.as_deref(), Some(&[34 << 1, 1, 3][..]));
    assert_eq!(au.avcc, [&[0, 0, 0, 3][..], &[19 << 1, 1, 5]].concat());
    let p = hevc_nal(1, &[6]);
    assert_eq!(cache.fix(p.clone()), p);
    // Without VPS an HEVC keyframe is not decodable out of band.
    let no_vps = AccessUnit::from_annexb_codec(
        VideoCodec::Hevc,
        &[hevc_nal(33, &[2]), hevc_nal(34, &[3]), hevc_nal(19, &[4])].concat(),
    );
    assert!(!no_vps.has_param_sets(VideoCodec::Hevc));
    assert!(no_vps.has_param_sets(VideoCodec::H264));
}

// ---------------------------------------------------------------------------
// Viewer: VideoRx
// ---------------------------------------------------------------------------

#[test]
fn video_rx_recovers_from_an_ltr_after_a_loss() {
    let mut rx = VideoRx::new();
    assert_eq!(
        rx.accept(&hdr(0, VIDEO_KEYFRAME | VIDEO_LTR)),
        VideoVerdict::Deliver
    );
    assert_eq!(rx.decoded(&hdr(0, VIDEO_KEYFRAME | VIDEO_LTR)), Some(0));
    assert_eq!(rx.accept(&hdr(1, 0)), VideoVerdict::Deliver);
    assert_eq!(rx.decoded(&hdr(1, 0)), None, "only LTR pictures are acked");
    assert_eq!(rx.accept(&hdr(2, VIDEO_LTR)), VideoVerdict::Deliver);
    assert_eq!(rx.decoded(&hdr(2, VIDEO_LTR)), Some(2));
    // Unit 3 lost: 4 is undecodable.
    assert_eq!(rx.accept(&hdr(4, 0)), VideoVerdict::Drop);
    assert_eq!(rx.request(true), VideoRequest::Recover { ltr_seq: 2 });
    assert_eq!(rx.accept(&hdr(5, 0)), VideoVerdict::Drop);
    // A recovery picture resyncs without a keyframe.
    assert_eq!(rx.accept(&hdr(6, VIDEO_RECOVERY)), VideoVerdict::Deliver);
    assert_eq!(rx.accept(&hdr(7, 0)), VideoVerdict::Deliver);
    assert_eq!(rx.recovered, 1);
    // Without CAP_LTR on both sides: always an IDR.
    let mut rx2 = rx.clone();
    rx2.mark_broken();
    assert_eq!(rx2.request(false), VideoRequest::Idr);
}

#[test]
fn video_rx_escalates_to_an_idr() {
    let mut rx = VideoRx::new();
    rx.accept(&hdr(0, VIDEO_KEYFRAME | VIDEO_LTR));
    rx.decoded(&hdr(0, VIDEO_KEYFRAME | VIDEO_LTR));
    rx.mark_broken();
    assert_eq!(rx.request(true), VideoRequest::Recover { ltr_seq: 0 });
    // The recovery did not arrive by the next (rate-limited) request,
    // nor by the one after.
    assert_eq!(rx.request(true), VideoRequest::Idr);
    assert_eq!(rx.request(true), VideoRequest::Idr);
    // The late recovery picture is still decodable (the LTR is held).
    assert_eq!(rx.accept(&hdr(3, VIDEO_RECOVERY)), VideoVerdict::Deliver);
    assert_eq!(rx.accept(&hdr(4, VIDEO_KEYFRAME)), VideoVerdict::Deliver);
    // A decoder failure forgets every LTR: IDR only.
    rx.accept(&hdr(5, VIDEO_LTR));
    rx.decoded(&hdr(5, VIDEO_LTR));
    rx.mark_corrupt();
    assert_eq!(rx.request(true), VideoRequest::Idr);
    // END forgets them too.
    let mut rx = VideoRx::new();
    rx.accept(&hdr(0, VIDEO_KEYFRAME | VIDEO_LTR));
    rx.decoded(&hdr(0, VIDEO_KEYFRAME | VIDEO_LTR));
    rx.accept(&hdr(1, VIDEO_END));
    assert_eq!(rx.ltr_held(), None);
}

/// A second recovery picture after a gap (stale P-frames purged by the
/// host between two answers) resyncs again instead of freezing.
#[test]
fn video_rx_takes_a_recovery_picture_after_a_gap() {
    let mut rx = VideoRx::new();
    rx.accept(&hdr(0, VIDEO_KEYFRAME | VIDEO_LTR));
    rx.decoded(&hdr(0, VIDEO_KEYFRAME | VIDEO_LTR));
    assert_eq!(rx.accept(&hdr(2, 0)), VideoVerdict::Drop);
    assert_eq!(rx.request(true), VideoRequest::Recover { ltr_seq: 0 });
    assert_eq!(rx.accept(&hdr(3, VIDEO_RECOVERY)), VideoVerdict::Deliver);
    // 4..=6 purged: the answer to the retry follows with a gap.
    assert_eq!(rx.accept(&hdr(7, VIDEO_RECOVERY)), VideoVerdict::Deliver);
    assert_eq!(rx.accept(&hdr(8, 0)), VideoVerdict::Deliver);
    assert_eq!(rx.recovered, 2);
    // No LTR held (decoder reset): a recovery picture is undecodable.
    rx.mark_corrupt();
    assert_eq!(rx.accept(&hdr(10, VIDEO_RECOVERY)), VideoVerdict::Drop);
}

// ---------------------------------------------------------------------------
// Host: LtrTracker
// ---------------------------------------------------------------------------

fn au(pts: u64, keyframe: bool, marked: Option<u8>) -> EncodedAu {
    EncodedAu {
        data: Vec::new(),
        keyframe,
        pts_us: pts,
        ltr: LtrInfo { marked, used: 0 },
    }
}

#[test]
fn ltr_tracker_marks_protects_and_recovers() {
    let mut t = LtrTracker::new(2, 3);
    // Keyframe + next picture auto-marked (as the MF H.264 MFTs do).
    assert_eq!(
        t.plan(0, true),
        FramePlan {
            force_idr: true,
            ..FramePlan::default()
        }
    );
    assert_eq!(t.on_output(10, &au(0, true, Some(0))), VIDEO_LTR);
    t.plan(1, false);
    assert_eq!(t.on_output(11, &au(1, false, Some(1))), VIDEO_LTR);
    t.ack(11);
    // Every 3rd picture a mark — never into the slot of the newest
    // confirmed reference (slot 1 = seq 11).
    let mut marks = Vec::new();
    for i in 2..8u64 {
        let p = t.plan(i, false);
        if let Some(k) = p.refs.mark_ltr {
            marks.push(k);
        }
        t.on_output(10 + i, &au(i, false, p.refs.mark_ltr));
    }
    assert_eq!(
        marks,
        vec![0, 0],
        "slot 1 stays protected while unconfirmed marks rotate"
    );
    // Recover from 11: the next picture predicts from slot 1 only.
    t.request_recover(11);
    let p = t.plan(100, false);
    assert_eq!(p.refs.use_ltr, 1 << 1);
    assert!(!p.force_idr);
    assert_eq!(t.on_output(200, &au(100, false, None)), VIDEO_RECOVERY);
    // An LTR no slot holds any more: IDR.
    t.request_recover(3);
    assert!(t.plan(101, false).force_idr);
    assert_eq!((t.recoveries, t.recover_idrs), (1, 1));
    // A keyframe clears every slot.
    t.on_output(300, &au(101, true, None));
    t.request_recover(11);
    assert!(t.plan(102, false).force_idr);
}

#[test]
fn ltr_tracker_marks_at_once_when_the_keyframe_was_not_marked() {
    // HEVC MFTs do not auto-mark the keyframe.
    let mut t = LtrTracker::new(2, 30);
    t.plan(0, true);
    assert_eq!(t.on_output(0, &au(0, true, None)), 0);
    assert_eq!(t.plan(1, false).refs.mark_ltr, Some(0));
    // LTR off: nothing at all.
    let mut off = LtrTracker::new(0, 30);
    assert_eq!(off.plan(0, false), FramePlan::default());
    assert_eq!(off.on_output(0, &au(0, false, Some(0))), 0);
}

#[test]
fn video_feedback_collects_across_threads() {
    let fb = std::sync::Arc::new(VideoFeedback::new());
    let f2 = std::sync::Arc::clone(&fb);
    std::thread::spawn(move || {
        f2.ack(4);
        f2.recover(4);
    })
    .join()
    .unwrap();
    let s = fb.take();
    assert_eq!((s.idr, s.recover, s.acks), (false, Some(4), vec![4]));
    assert_eq!(fb.take(), FeedbackState::default());
}

// ---------------------------------------------------------------------------
// Host: VideoSession codec choice and recovery
// ---------------------------------------------------------------------------

/// A fake encoder of a given codec that honours LTR refs (H.264-style auto
/// marks); every picture is logged.
struct Fake2 {
    codec: VideoCodec,
    ltr: u8,
    first: bool,
    auto: u8,
    refs: FrameRefs,
    log: Rc<RefCell<Vec<String>>>,
}

impl VideoEncode for Fake2 {
    fn encode(
        &mut self,
        _bgra: &[u8],
        _stride: usize,
        force_idr: bool,
        pts_us: u64,
    ) -> Result<Vec<EncodedAu>, String> {
        let key = force_idr || self.first;
        self.first = false;
        let refs = std::mem::take(&mut self.refs);
        if key {
            self.auto = 0;
        }
        let mut marked = refs.mark_ltr;
        if marked.is_none() && self.ltr > 0 && self.auto < self.ltr {
            marked = Some(self.auto);
            self.auto += 1;
        }
        self.log.borrow_mut().push(match (key, refs.use_ltr) {
            (true, _) => "I".into(),
            (false, 0) => "P".into(),
            (false, m) => format!("R{m}"),
        });
        Ok(vec![EncodedAu {
            data: vec![0, 0, 0, 1, 1],
            keyframe: key,
            pts_us,
            ltr: LtrInfo {
                marked: marked.filter(|_| self.ltr > 0),
                used: refs.use_ltr,
            },
        }])
    }
    fn set_bitrate(&mut self, _bps: u32) {}
    fn name(&self) -> String {
        format!("fake {}", self.codec)
    }
    fn codec(&self) -> VideoCodec {
        self.codec
    }
    fn ltr_slots(&self) -> u8 {
        self.ltr
    }
    fn set_frame_refs(&mut self, refs: FrameRefs) {
        self.refs = refs;
    }
}

/// `hevc_ok` = the factory can open HEVC encoders.
fn factory2(log: Rc<RefCell<Vec<String>>>, hevc_ok: bool) -> EncoderFactory {
    Box::new(move |s: &EncoderSpec| {
        log.borrow_mut().push(format!(
            "new {}x{} {} ltr{}",
            s.w, s.h, s.codec, s.ltr_slots
        ));
        if s.codec == VideoCodec::Hevc && !hevc_ok {
            return Err("no HEVC MFT".into());
        }
        Ok(Box::new(Fake2 {
            codec: s.codec,
            ltr: s.ltr_slots,
            first: true,
            auto: 0,
            refs: FrameRefs::default(),
            log: log.clone(),
        }) as Box<dyn VideoEncode>)
    })
}

fn full_map(w: u32, h: u32) -> TileMap {
    let mut m = TileMap::new(w, h);
    m.mark_rect(&TileRect {
        x: 0,
        y: 0,
        w: w as u16,
        h: h as u16,
    });
    m
}

fn instant_cfg() -> MotionConfig {
    MotionConfig {
        enter_after: Duration::ZERO,
        ..MotionConfig::default()
    }
}

/// Drive `n` captures of full-screen motion; returns the units queued.
fn drive(
    vs: &mut VideoSession,
    mb: &FrameMailbox,
    t0: Instant,
    from: u64,
    n: u64,
) -> Vec<VideoHeader> {
    let (w, h) = (640u32, 384u32);
    let px = vec![0u8; (w * h * 4) as usize];
    let map = full_map(w, h);
    let mut out = Vec::new();
    for i in from..from + n {
        let now = t0 + Duration::from_millis(i * 33);
        vs.on_capture(now, &px, w, h, w as usize * 4, &map, true, true, mb);
        while let Some(f) = mb.take() {
            out.extend(f.video);
        }
    }
    out
}

#[test]
fn session_streams_hevc_to_a_capable_viewer_and_falls_back_to_h264() {
    for hevc_ok in [true, false] {
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut vs = VideoSession::with_config(factory2(log.clone(), hevc_ok), 30, instant_cfg());
        vs.set_peer_caps(true, false);
        let mb = FrameMailbox::default();
        let units = drive(&mut vs, &mb, Instant::now(), 0, 5);
        let want = if hevc_ok { CODEC_HEVC } else { CODEC_H264 };
        assert!(
            !units.is_empty() && units.iter().all(|u| u.codec == want),
            "{hevc_ok}"
        );
        assert!(units[0].is_keyframe());
        let l = log.borrow();
        assert_eq!(l[0], "new 640x384 HEVC ltr0");
        if !hevc_ok {
            assert_eq!(l[1], "new 640x384 H.264 ltr0", "{l:?}");
            assert!(vs.take_error().is_some_and(|e| e.contains("HEVC")));
            assert_eq!(
                vs.wanted_codec(),
                VideoCodec::H264,
                "for the rest of the session"
            );
        }
    }
    // A viewer without CAP_HEVC: H.264.
    let log = Rc::new(RefCell::new(Vec::new()));
    let mut vs = VideoSession::with_config(factory2(log.clone(), true), 30, instant_cfg());
    let units = drive(&mut vs, &FrameMailbox::default(), Instant::now(), 0, 3);
    assert!(units.iter().all(|u| u.codec == CODEC_H264));
}

#[test]
fn session_answers_recover_with_a_recovery_frame_or_an_idr() {
    let log = Rc::new(RefCell::new(Vec::new()));
    let mut vs = VideoSession::with_config(factory2(log.clone(), true), 30, instant_cfg());
    vs.set_peer_caps(false, true);
    let mb = FrameMailbox::default();
    let t0 = Instant::now();
    let units = drive(&mut vs, &mb, t0, 0, 40);
    assert!(units[0].is_keyframe() && units[0].is_ltr() && units[1].is_ltr());
    let marks: Vec<u64> = units.iter().filter(|u| u.is_ltr()).map(|u| u.seq).collect();
    assert!(marks.len() >= 3, "a mark about every second: {marks:?}");
    // The viewer acked every LTR; it lost pictures and recovers from the
    // newest one.
    vs.apply_feedback(FeedbackState {
        acks: marks.clone(),
        ..FeedbackState::default()
    });
    vs.apply_feedback(FeedbackState {
        recover: marks.last().copied(),
        ..FeedbackState::default()
    });
    let after = drive(&mut vs, &mb, t0, 40, 2);
    assert!(
        after[0].is_recovery() && !after[0].is_keyframe(),
        "{after:?}"
    );
    assert!(!after[1].is_recovery());
    assert_eq!(log.borrow().iter().filter(|s| s.as_str() == "I").count(), 1);
    // Recovering from an LTR long overwritten: IDR.
    vs.request_recover(units[2].seq + 1_000);
    let after = drive(&mut vs, &mb, t0, 42, 1);
    assert!(after[0].is_keyframe());
    let st = vs.stats();
    assert_eq!((st.recoveries, st.recover_idrs), (1, 1));
    // Without CAP_LTR every recover is an IDR (and nothing is marked).
    let log = Rc::new(RefCell::new(Vec::new()));
    let mut vs = VideoSession::with_config(factory2(log, true), 30, instant_cfg());
    let units = drive(&mut vs, &mb, t0, 0, 5);
    assert!(units.iter().all(|u| !u.is_ltr()));
    vs.request_recover(units[0].seq);
    assert!(drive(&mut vs, &mb, t0, 5, 1)[0].is_keyframe());
}

// ---------------------------------------------------------------------------
// Stable encode surface (M5)
// ---------------------------------------------------------------------------

/// A motion region that keeps growing (a window being resized / a video
/// expanding): encoder re-opens, keyframes and coded-rect changes with and
/// without the stable surface. Prints the comparison.
#[test]
fn stable_surface_avoids_reopens_while_motion_grows() {
    let (w, h) = (1920u32, 1080u32);
    let px = vec![0u8; (w * h * 4) as usize];
    let run = |fixed: bool| {
        let log = Rc::new(RefCell::new(Vec::new()));
        let cfg = MotionConfig {
            enter_after: Duration::ZERO,
            ..MotionConfig::default()
        };
        let mut vs = VideoSession::with_config(factory2(log.clone(), true), 30, cfg);
        vs.set_fixed_surface(fixed);
        let mb = FrameMailbox::default();
        let t0 = Instant::now();
        let mut rects = Vec::new();
        // 8 s of motion growing from 384x256 by 128x64 px every 1.1 s
        // (the detector grows at most once per second).
        for i in 0..240u64 {
            let g = (i / 33) as u16;
            let r = TileRect {
                x: 256,
                y: 192,
                w: 384 + 128 * g,
                h: 256 + 64 * g,
            };
            let mut map = TileMap::new(w, h);
            map.mark_rect(&r);
            vs.on_capture(
                t0 + Duration::from_millis(i * 33),
                &px,
                w,
                h,
                w as usize * 4,
                &map,
                true,
                true,
                &mb,
            );
            while mb.take().is_some() {}
            if let Some(a) = vs.active_rect() {
                if rects.last() != Some(&a) {
                    rects.push(a);
                }
            }
        }
        let st = vs.stats();
        (st.encoders, st.keyframes, st.grows, rects.len())
    };
    let (enc_old, key_old, grows, rects_old) = run(false);
    let (enc_new, key_new, _, rects_new) = run(true);
    eprintln!(
        "growing motion ({grows} grows): 128 px buckets {enc_old} encoders / {key_old} keyframes / \
         {rects_old} coded rects; stable surface {enc_new} encoders / {key_new} keyframes / \
         {rects_new} coded rects"
    );
    assert!(grows >= 5);
    assert!(enc_new <= 2, "at most one re-open per motion period");
    assert_eq!(
        key_new, enc_new,
        "a keyframe per encoder only, none per grow"
    );
    assert!(enc_new < enc_old && key_new < key_old);
}
