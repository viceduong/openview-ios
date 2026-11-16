//! Integration test: video loss recovery end to end over two real
//! `TerminalSession`s. The host's `VideoSession` streams a motion region
//! through the mailbox / `PrioritySender`; one access unit is lost on the
//! way. With `CAP_LTR` on both sides the viewer answers with
//! `VIDEO_RECOVER`, the host with a `VIDEO_RECOVERY` P-frame predicted from
//! a confirmed long-term reference — no second keyframe — and the viewer's
//! (reference-checking) decoder accepts it. Without `CAP_LTR` the same loss
//! costs an IDR.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ovapp::sendq::{FrameMailbox, PrioritySender};
use ovapp::terminal::{Command, TerminalSession, CAP_H264, CAP_LTR};
use ovapp::tiles::TileMap;
use ovapp::video::{
    EncodedAu, EncoderFactory, EncoderSpec, FrameRefs, LtrInfo, MotionConfig, VideoDecode,
    VideoEncode, VideoFeedback, VideoSession,
};
use ovcore::handshake::{Auth, Handshake, HandshakeEvent, Role};
use ovcore::identity::Identity;
use ovtransport::{Carrier, InMemoryCarrier};

fn flush(hs: &mut Handshake, c: &mut InMemoryCarrier) {
    for ev in hs.take_events() {
        let HandshakeEvent::Sent(d) = ev;
        c.send(&d).unwrap();
    }
}

/// (viewer, host) over an in-memory (reliable) carrier.
fn sessions() -> (TerminalSession, TerminalSession) {
    let (mut vc, mut hc) = InMemoryCarrier::pair();
    let (vid, hid) = (Identity::generate(), Identity::generate());
    let code = hid.short_code();
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
    let view = TerminalSession::new(v.take_session().unwrap(), vc);
    let host = TerminalSession::new(h.take_session().unwrap(), hc);
    (view, host)
}

/// A model of an LTR-capable encoder: each unit carries its picture id
/// and the id of the picture it predicts from; LTR slots behave like the
/// MF H.264 MFTs (a keyframe and the next picture are auto-marked into
/// slots 0 and 1; explicit marks; `use_ltr` predicts from a slot).
struct LtrModel {
    next_id: u32,
    prev: Option<u32>,
    slots: [Option<u32>; 2],
    auto: u8,
    refs: FrameRefs,
    ltr: bool,
}

impl VideoEncode for LtrModel {
    fn encode(
        &mut self,
        _bgra: &[u8],
        _stride: usize,
        force_idr: bool,
        pts_us: u64,
    ) -> Result<Vec<EncodedAu>, String> {
        let id = self.next_id;
        self.next_id += 1;
        let refs = std::mem::take(&mut self.refs);
        let key = force_idr || self.prev.is_none();
        let reference = if key {
            self.slots = [None; 2];
            self.auto = 0;
            u32::MAX
        } else if refs.use_ltr != 0 {
            let k = refs.use_ltr.trailing_zeros() as usize;
            self.slots[k].ok_or("use of an empty LTR slot")?
        } else {
            self.prev.unwrap_or(u32::MAX)
        };
        let mut marked = refs.mark_ltr;
        if marked.is_none() && self.ltr && self.auto < 2 {
            marked = Some(self.auto);
        }
        if let Some(k) = marked {
            self.slots[usize::from(k)] = Some(id);
            self.auto = self.auto.max(k + 1);
        }
        self.prev = Some(id);
        let mut data = vec![0, 0, 0, 1, if key { 0x65 } else { 0x41 }];
        data.extend_from_slice(&id.to_be_bytes());
        data.extend_from_slice(&reference.to_be_bytes());
        // Keyframes are big, P-frames small (the byte counts are printed).
        data.resize(if key { 40_000 } else { 4_000 }, 0x11);
        Ok(vec![EncodedAu {
            data,
            keyframe: key,
            pts_us,
            ltr: LtrInfo {
                marked: marked.filter(|_| self.ltr),
                used: refs.use_ltr,
            },
        }])
    }
    fn set_bitrate(&mut self, _bps: u32) {}
    fn name(&self) -> String {
        "ltr model".into()
    }
    fn ltr_slots(&self) -> u8 {
        if self.ltr {
            2
        } else {
            0
        }
    }
    fn set_frame_refs(&mut self, refs: FrameRefs) {
        self.refs = refs;
    }
}

/// Decodes a unit only when its reference picture was decoded (exactly
/// what a real decoder needs to produce a correct picture).
struct RefChecker {
    have: Rc<RefCell<HashSet<u32>>>,
    errors: Rc<RefCell<u32>>,
}

impl VideoDecode for RefChecker {
    fn decode(&mut self, annexb: &[u8], w: u32, h: u32, out: &mut Vec<u8>) -> Result<bool, String> {
        let id = u32::from_be_bytes(annexb[5..9].try_into().unwrap());
        let reference = u32::from_be_bytes(annexb[9..13].try_into().unwrap());
        let mut have = self.have.borrow_mut();
        if reference == u32::MAX {
            have.clear();
        } else if !have.contains(&reference) {
            *self.errors.borrow_mut() += 1;
            return Err(format!("picture {id}: reference {reference} missing"));
        }
        have.insert(id);
        out.clear();
        out.resize((w * h * 4) as usize, 0x80);
        Ok(true)
    }
}

struct Outcome {
    host_keyframes: u64,
    host_recoveries: u64,
    host_bytes_after_loss: u64,
    recover_requests: u64,
    idr_requests: u64,
    decode_errors: u32,
    decoded_after_loss: usize,
}

fn run(viewer_ltr: bool) -> Outcome {
    let (mut view, mut host) = sessions();
    let have = Rc::new(RefCell::new(HashSet::new()));
    let errors = Rc::new(RefCell::new(0));
    view.set_video_decoder(Box::new(RefChecker {
        have: have.clone(),
        errors: errors.clone(),
    }));
    if !viewer_ltr {
        view.set_local_caps(view.local_caps() & !CAP_LTR);
    }
    // Host: the hook hands feedback to the capture side, as ovhost does.
    let fb = Arc::new(VideoFeedback::new());
    let caps = Arc::new(Mutex::new(0u32));
    let (f2, c2) = (Arc::clone(&fb), Arc::clone(&caps));
    host.on_cmd(Some(Box::new(move |cmd| match cmd {
        Command::Caps { flags } => *c2.lock().unwrap() = *flags,
        Command::VideoIdrRequest { .. } => f2.idr(),
        Command::VideoRecover { ltr_seq } => f2.recover(*ltr_seq),
        Command::VideoLtrAck { seq } => f2.ack(*seq),
        _ => {}
    })));
    view.announce_caps().unwrap();
    host.announce_caps().unwrap();
    let factory: EncoderFactory = Box::new(|s: &EncoderSpec| {
        Ok(Box::new(LtrModel {
            next_id: 0,
            prev: None,
            slots: [None; 2],
            auto: 0,
            refs: FrameRefs::default(),
            ltr: s.ltr_slots > 0,
        }) as Box<dyn VideoEncode>)
    });
    let cfg = MotionConfig {
        enter_after: Duration::ZERO,
        ..MotionConfig::default()
    };
    let mut video = VideoSession::with_config(factory, 30, cfg);
    let (w, h) = (640u32, 384u32);
    let px = vec![0u8; (w * h * 4) as usize];
    let mut moving = TileMap::new(w, h);
    for row in 0..moving.rows() {
        for col in 0..w.div_ceil(64) {
            moving.set(col, row);
        }
    }
    let mb = FrameMailbox::default();
    let mut tx = PrioritySender::new(4096);
    let t0 = Instant::now();
    let (mut keyframes_before, mut bytes_before, mut have_before) = (0, 0, 0);
    let lose_at = 50u64;
    for step in 0..100u64 {
        for _ in 0..3 {
            host.pump().unwrap();
            view.pump().unwrap();
        }
        let c = *caps.lock().unwrap();
        video.set_peer_caps(false, c & CAP_LTR != 0 && host.local_caps() & CAP_LTR != 0);
        video.apply_feedback(fb.take());
        let now = t0 + Duration::from_millis(step * 33);
        video.on_capture(
            now,
            &px,
            w,
            h,
            w as usize * 4,
            &moving,
            true,
            c & CAP_H264 != 0,
            &mb,
        );
        if step == lose_at {
            // The link loses this access unit for good.
            keyframes_before = video.stats().keyframes;
            bytes_before = video.stats().bytes;
            have_before = have.borrow().len();
            assert!(mb.take().is_some_and(|f| f.video.is_some()));
        }
        while tx.has_work(&mb) {
            tx.flush(&mut host, &mb, 64).unwrap();
        }
        for _ in 0..3 {
            view.pump().unwrap();
            let _ = view.take_latest_frame();
        }
    }
    let have_after = have.borrow().len();
    let decode_errors = *errors.borrow();
    let st = video.stats();
    let vs = view.video_stats();
    eprintln!(
        "viewer LTR {viewer_ltr}: host {st:?}\n  viewer {vs:?}, decoder errors {}",
        errors.borrow()
    );
    Outcome {
        host_keyframes: st.keyframes - keyframes_before,
        host_recoveries: st.recoveries,
        host_bytes_after_loss: st.bytes - bytes_before,
        recover_requests: vs.recover_requests,
        idr_requests: vs.idr_requests,
        decode_errors,
        decoded_after_loss: have_after.saturating_sub(have_before),
    }
}

#[test]
fn a_lost_unit_is_recovered_from_a_long_term_reference() {
    let ltr = run(true);
    assert_eq!(ltr.recover_requests, 1, "one VIDEO_RECOVER");
    assert_eq!(ltr.idr_requests, 0);
    assert_eq!(ltr.host_recoveries, 1, "answered with a recovery frame");
    assert_eq!(ltr.host_keyframes, 0, "no keyframe after the loss");
    assert_eq!(
        ltr.decode_errors, 0,
        "every decoded picture had its reference"
    );
    assert!(ltr.decoded_after_loss >= 40, "{}", ltr.decoded_after_loss);

    let idr = run(false);
    assert_eq!(idr.recover_requests, 0);
    assert_eq!(idr.idr_requests, 1);
    assert_eq!(idr.host_keyframes, 1, "the loss costs a keyframe");
    assert_eq!(idr.decode_errors, 0);
    eprintln!(
        "bytes after the loss: LTR {} vs IDR {}",
        ltr.host_bytes_after_loss, idr.host_bytes_after_loss
    );
    assert!(ltr.host_bytes_after_loss < idr.host_bytes_after_loss);
}
