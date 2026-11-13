//! Tests for the outbound path: shared fixtures; the cases live in the submodules.

use super::*;

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::bands::MoveRect;
use crate::screen::{Frame, PixelFormat};
use crate::terminal::{Command, FrameCodec, TerminalSession, CAP_BANDS, CAP_TILES};
use crate::tiles::TileRect;
use crate::video::VideoHeader;

fn frame(seq: u64, bytes: usize) -> EncodedFrame {
    EncodedFrame {
        seq,
        width: 1,
        height: 1,
        stride: 4,
        format: PixelFormat::Bgra,
        total: 4,
        codec: FrameCodec::Deflate,
        data: vec![seq as u8; bytes],
        delta: None,
        video: None,
        barrier: 0,
    }
}

fn is_frame_end(o: &Outgoing, seq: u64) -> bool {
    matches!(o, Outgoing::Frame(Command::FrameEnd(s)) if *s == seq)
}

// ---------------- UpdateEncoder helpers ----------------

use crate::terminal::{CAP_KEYFRAME, LOCAL_CAPS};
use crate::tiles::ScreenDecoder;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u32) -> u32 {
        (self.next() % u64::from(n.max(1))) as u32
    }
}

fn noise(w: u32, h: u32, seed: u64) -> Vec<u8> {
    let mut r = Rng(seed | 1);
    (0..w * h * 4).map(|_| r.next() as u8).collect()
}

/// Mutate a frame like a desktop does: small edits (typing, a blinking
/// caret), a block fill (a window/video region), or a vertical scroll.
fn mutate(px: &mut [u8], w: u32, h: u32, rng: &mut Rng) {
    let stride = (w * 4) as usize;
    match rng.below(10) {
        0..=5 => {
            for _ in 0..1 + rng.below(6) {
                let i = rng.below(w * h * 4) as usize;
                px[i] = px[i].wrapping_add(1 + rng.below(200) as u8);
            }
        }
        6..=8 => {
            let (bx, by) = (rng.below(w), rng.below(h));
            let (bw, bh) = (1 + rng.below(w - bx), 1 + rng.below(h - by));
            let v = rng.next() as u8;
            for y in by..by + bh {
                let o = y as usize * stride + bx as usize * 4;
                px[o..o + bw as usize * 4].iter_mut().for_each(|b| *b = v);
            }
        }
        _ => {
            let dy = 1 + rng.below(h / 2) as usize;
            px.copy_within(dy * stride.., 0);
        }
    }
}

fn apply(dec: &mut ScreenDecoder, f: &EncodedFrame) {
    dec.apply(&f.meta(), f.seq, &f.data).unwrap_or_else(|e| {
        panic!(
            "update {} ({:?}) failed: {e}",
            f.seq,
            f.delta.as_ref().map(|d| d.base_seq)
        )
    });
}

/// Two terminal sessions over an in-memory carrier pair: (host, view).
fn session_pair() -> (TerminalSession, TerminalSession) {
    use ovcore::handshake::{Auth, Handshake, HandshakeEvent, Role};
    use ovcore::identity::Identity;
    use ovtransport::{Carrier, InMemoryCarrier};
    let (id_v, id_h) = (Identity::generate(), Identity::generate());
    let (mut vc, mut hc) = InMemoryCarrier::pair();
    let code = id_h.pairing_code();
    let mut client = Handshake::new(Role::Client, 0x11, &id_v, Auth::PairingCode(code.clone()));
    client.expect_fingerprint(id_h.fingerprint());
    let mut server = Handshake::new(Role::Server, 0x22, &id_h, Auth::PairingCode(code));
    for _ in 0..200 {
        let _ = vc.poll_recv(&mut |d| {
            let _ = client.feed(d);
        });
        let _ = hc.poll_recv(&mut |d| {
            let _ = server.feed(d);
        });
        for ev in client.take_events() {
            let HandshakeEvent::Sent(d) = ev;
            let _ = vc.send(&d);
        }
        for ev in server.take_events() {
            let HandshakeEvent::Sent(d) = ev;
            let _ = hc.send(&d);
        }
        if client.is_established() && server.is_established() {
            break;
        }
    }
    let view = TerminalSession::new(client.take_session().expect("client"), vc);
    let host = TerminalSession::new(server.take_session().expect("server"), hc);
    (host, view)
}

mod encoder;
mod lossy;
mod moves;
mod sender;
mod video;
