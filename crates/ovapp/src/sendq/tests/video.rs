//! Video access units through the mailbox and H.264 compositing.

use super::*;

use crate::terminal::CAP_H264;
use crate::video::{
    EncodedAu, EncoderFactory, MotionConfig, VideoDecode, VideoEncode, VideoSession, CODEC_H264,
    VIDEO_END, VIDEO_KEYFRAME,
};

fn vunit(seq: u64, flags: u8) -> EncodedFrame {
    EncodedFrame::video(
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
            screen_w: 128,
            screen_h: 128,
            pts_us: 0,
        },
        vec![0, 0, 0, 1, 0x41, seq as u8],
    )
}

/// Video units are FIFO (never replaced), a screen update queued after
/// a video unit waits for it, and when both are eligible they alternate.
#[test]
fn mailbox_keeps_video_fifo_and_orders_updates_after_it() {
    let mb = FrameMailbox::default();
    mb.push_video(vunit(0, VIDEO_KEYFRAME));
    mb.push_video(vunit(1, 0));
    mb.put(frame(10, 4)); // barrier = 2
    mb.push_video(vunit(2, 0));
    assert_eq!(mb.video_queued(), 3);
    assert!(mb.is_pending());
    let order: Vec<(bool, u64)> =
        std::iter::from_fn(|| mb.take().map(|f| (f.video.is_some(), f.seq))).collect();
    assert_eq!(order, [(true, 0), (true, 1), (false, 10), (true, 2)]);
    assert_eq!(
        mb.last_taken(),
        Some(10),
        "only screen updates are delta bases"
    );
    // A newer update still replaces an unsent one; video is never replaced.
    mb.push_video(vunit(3, VIDEO_END));
    mb.put(frame(11, 4));
    assert!(mb.put(frame(12, 4)));
    let order: Vec<(bool, u64)> =
        std::iter::from_fn(|| mb.take().map(|f| (f.video.is_some(), f.seq))).collect();
    assert_eq!(
        order,
        [(true, 3), (false, 12)],
        "END goes before the refresh queued after it"
    );
    // Streaming a video unit: VIDEO_BEGIN + chunks + FRAME_END(seq).
    mb.push_video(vunit(4, 0));
    let mut tx = PrioritySender::new(4);
    let now = Instant::now();
    let recs: Vec<_> = std::iter::from_fn(|| tx.next(&mb, now)).collect();
    assert!(
        matches!(&recs[0], Outgoing::Frame(Command::VideoBegin { header, compressed_len: 6 }) if header.seq == 4)
    );
    assert!(matches!(
        recs.last(),
        Some(Outgoing::Frame(Command::FrameEnd(4)))
    ));
}

/// Masked tiles never go out as tiles; the refresh sends the whole
/// region losslessly even where nothing changed since.
#[test]
fn masked_region_is_left_to_video_and_refreshed_afterwards() {
    let (w, h) = (256, 256);
    let mb = FrameMailbox::default();
    let mut enc = UpdateEncoder::new();
    enc.set_peer_caps(LOCAL_CAPS);
    let mut dec = ScreenDecoder::new();
    let mut px = noise(w, h, 21);
    enc.submit(&px, w, h, None, &mb);
    apply(&mut dec, &mb.take().unwrap());
    let mask = TileRect {
        x: 64,
        y: 64,
        w: 128,
        h: 128,
    };
    // Changes only inside the mask: no tile update at all, but the
    // change is visible to the motion detector.
    px[(100 * 256 + 100) * 4] ^= 0xFF;
    assert_eq!(
        enc.submit_masked(&px, w, h, None, Some(mask), None, &mb),
        SubmitOutcome::Unchanged
    );
    assert!(enc.fresh_valid());
    assert_eq!(enc.fresh().count(), 1);
    assert!(!mb.is_pending());
    // Inside + outside: only the outside tile is sent.
    px[(100 * 256 + 101) * 4] ^= 0xFF;
    px[(10 * 256 + 10) * 4] ^= 0xFF;
    assert!(matches!(
        enc.submit_masked(&px, w, h, None, Some(mask), None, &mb),
        SubmitOutcome::Delta { tiles: 1, .. }
    ));
    let f = mb.take().unwrap();
    // (Trimmed to the one changed row of that tile.)
    assert_eq!(
        f.delta.as_ref().unwrap().rects,
        [TileRect {
            x: 0,
            y: 10,
            w: 64,
            h: 1
        }]
    );
    apply(&mut dec, &f);
    assert_ne!(
        dec.frame().unwrap().pixels,
        px,
        "the masked change is not in the tile framebuffer"
    );
    // Motion over: refresh the region with nothing changed since.
    assert_eq!(
        enc.submit(&px, w, h, Some(&[]), &mb),
        SubmitOutcome::Unchanged
    );
    assert!(matches!(
        enc.submit_masked(&px, w, h, Some(&[]), None, Some(mask), &mb),
        SubmitOutcome::Delta { tiles: 4, .. }
    ));
    apply(&mut dec, &mb.take().unwrap());
    assert_eq!(dec.frame().unwrap().pixels, px, "lossless again");
}

/// "Encodes" a picture as its first BGRA pixel's blue byte; the fake
/// decoder paints a picture of that colour.
struct FakeEnc {
    first: bool,
}
impl VideoEncode for FakeEnc {
    fn encode(
        &mut self,
        bgra: &[u8],
        _s: usize,
        force_idr: bool,
        pts_us: u64,
    ) -> Result<Vec<EncodedAu>, String> {
        let key = force_idr || self.first;
        self.first = false;
        Ok(vec![EncodedAu {
            data: vec![0, 0, 0, 1, if key { 0x65 } else { 0x41 }, bgra[0]],
            keyframe: key,
            pts_us,
        }])
    }
    fn set_bitrate(&mut self, _bps: u32) {}
    fn name(&self) -> String {
        "fake".into()
    }
}
struct FakeDec {
    decoded: std::rc::Rc<std::cell::Cell<u32>>,
}
impl VideoDecode for FakeDec {
    fn decode(&mut self, annexb: &[u8], w: u32, h: u32, out: &mut Vec<u8>) -> Result<bool, String> {
        let v = *annexb.last().ok_or("empty unit")?;
        out.clear();
        out.resize((w * h * 4) as usize, v);
        self.decoded.set(self.decoded.get() + 1);
        Ok(true)
    }
}
fn fake_factory() -> EncoderFactory {
    Box::new(|_, _, _, _| Ok(Box::new(FakeEnc { first: true }) as Box<dyn VideoEncode>))
}

/// CAPS negotiation: only a viewer with a decoder advertises CAP_H264,
/// and the host's VideoSession only streams to it. End to end over a
/// session: motion becomes video painted over the tile framebuffer,
/// rest brings back the exact lossless pixels.
#[test]
fn h264_only_to_capable_viewers_and_composited_over_tiles() {
    let caps_seen = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let run = |with_decoder: bool| -> (u32, u64, u32) {
        let (mut host, mut view) = session_pair();
        let decoded = std::rc::Rc::new(std::cell::Cell::new(0));
        if with_decoder {
            view.set_video_decoder(Box::new(FakeDec {
                decoded: decoded.clone(),
            }));
        }
        let seen = std::sync::Arc::clone(&caps_seen);
        host.on_cmd(Some(Box::new(move |cmd| {
            if let Command::Caps { flags } = cmd {
                seen.store(*flags, Ordering::Relaxed);
            }
        })));
        view.announce_caps().unwrap();
        let (w, h) = (512u32, 384u32);
        let mb = FrameMailbox::default();
        let mut tx = PrioritySender::new(4096);
        let mut enc = UpdateEncoder::new();
        let cfg = MotionConfig {
            enter_after: Duration::from_millis(100),
            ..MotionConfig::default()
        };
        let mut video = VideoSession::with_config(fake_factory(), 30, cfg);
        let mut px = noise(w, h, 31);
        let region = TileRect {
            x: 128,
            y: 64,
            w: 256,
            h: 256,
        };
        let t0 = Instant::now();
        let mut shown: Option<Frame> = None;
        let mut caps = 0;
        for step in 0..120u64 {
            for _ in 0..3 {
                host.pump().unwrap();
                view.pump().unwrap();
            }
            caps = caps_seen.load(Ordering::Relaxed);
            enc.set_peer_caps(caps);
            let now = t0 + Duration::from_millis(step * 33);
            // 60 captures of motion in the region, then rest.
            if step < 60 {
                let v = (step as u8).wrapping_mul(7) | 1;
                for y in region.y..region.y + region.h {
                    let o = (usize::from(y) * w as usize + usize::from(region.x)) * 4;
                    px[o..o + usize::from(region.w) * 4]
                        .iter_mut()
                        .for_each(|b| *b = v);
                }
            }
            // The viewer advertises CAP_LOSSY (noise is
            // "image"): lossy first, refined on the encoder's clock.
            enc.set_clock(Some(now));
            let out = enc.submit_masked(&px, w, h, None, video.active_rect(), None, &mb);
            let _ = out;
            video.on_capture(
                now,
                &px,
                w,
                h,
                w as usize * 4,
                enc.fresh(),
                enc.fresh_valid(),
                caps & CAP_H264 != 0,
                &mb,
            );
            if let Some(r) = video.take_refresh() {
                // The post-motion refresh rides the refine lane.
                enc.queue_refresh(r);
                enc.submit_masked(&px, w, h, Some(&[]), None, None, &mb);
            }
            while tx.has_work(&mb) {
                tx.flush(&mut host, &mb, 64).unwrap();
            }
            // Refine lane: whenever the link is free.
            while enc.submit_refine(&mb) != SubmitOutcome::Unchanged {
                while tx.has_work(&mb) {
                    tx.flush(&mut host, &mb, 64).unwrap();
                }
            }
            for _ in 0..3 {
                view.pump().unwrap();
            }
            if let Some(f) = view.take_latest_frame() {
                shown = Some(f);
            }
            if step == 40 && with_decoder {
                // Mid-motion: the region shows the decoded picture, the
                // rest the lossless tiles.
                let f = shown.as_ref().expect("frame");
                let o = (usize::from(region.y + 10) * w as usize + usize::from(region.x + 10)) * 4;
                assert_eq!(f.pixels[o], px[o], "video painted over the region");
                assert!(video.active_rect().is_some());
            }
        }
        let f = shown.expect("frames shown");
        assert_eq!(f.pixels, px, "lossless after the motion ended");
        assert_eq!(video.active_rect(), None);
        (caps, video.stats().frames, decoded.get())
    };
    let (caps, frames, decoded) = run(true);
    assert!(
        caps & CAP_H264 != 0,
        "a viewer with a decoder advertises H.264"
    );
    assert!(
        frames > 20 && decoded as u64 >= frames - 1,
        "frames {frames} decoded {decoded}"
    );
    let (caps, frames, decoded) = run(false);
    assert_eq!(caps & CAP_H264, 0, "no decoder, no CAP_H264");
    assert_eq!(
        (frames, decoded),
        (0, 0),
        "never any video to an incapable viewer"
    );
}
