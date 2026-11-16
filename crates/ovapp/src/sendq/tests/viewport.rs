//! Viewport priority (`CAP_VIEWPORT`): visible tiles first, off-screen
//! changes deferred into the backlog and still delivered.

use super::*;

use super::super::viewport::OFFVIEW_MAX_DEFER;
use crate::terminal::CAP_VIEWPORT;

const W: u32 = 512;
const H: u32 = 256;

/// The left half of the screen.
const LEFT: TileRect = TileRect {
    x: 0,
    y: 0,
    w: 256,
    h: 256,
};

/// Bump one pixel inside tile `(c, r)`.
fn touch(px: &mut [u8], c: u32, r: u32, v: u8) {
    let o = ((r * 64 + 10) * W + c * 64 + 10) as usize * 4;
    px[o] = v;
}

/// An encoder + decoder holding the first full frame of a text screen,
/// clock pinned at the returned instant.
fn setup(caps: u32) -> (UpdateEncoder, FrameMailbox, ScreenDecoder, Vec<u8>, Instant) {
    let t0 = Instant::now();
    let mut enc = UpdateEncoder::new();
    enc.set_peer_caps(caps);
    enc.set_clock(Some(t0));
    let mb = FrameMailbox::new(None);
    let mut dec = ScreenDecoder::new();
    let px = crate::lossy::tests::text(W, H, 9);
    assert!(matches!(
        enc.submit(&px, W, H, None, &mb),
        SubmitOutcome::Full { .. }
    ));
    apply(&mut dec, &mb.take().unwrap());
    (enc, mb, dec, px, t0)
}

fn rects_touch_col(f: &EncodedFrame, min_x: u16) -> bool {
    f.delta
        .as_ref()
        .is_some_and(|d| d.rects.iter().any(|r| r.x + r.w > min_x))
}

#[test]
fn off_viewport_changes_wait_for_the_refine_lane() {
    let (mut enc, mb, mut dec, mut px, t0) = setup(LOCAL_CAPS);
    assert_ne!(LOCAL_CAPS & CAP_VIEWPORT, 0);
    enc.set_viewport(Some(LEFT));
    // One change inside the viewport, one outside (tile col 6).
    touch(&mut px, 1, 1, 0x11);
    touch(&mut px, 6, 2, 0x22);
    let out = enc.submit(&px, W, H, None, &mb);
    assert!(
        matches!(out, SubmitOutcome::Delta { tiles: 1, .. }),
        "{out:?}"
    );
    let f = mb.take().unwrap();
    assert!(!rects_touch_col(&f, 256), "only the visible tile goes now");
    apply(&mut dec, &f);
    assert_ne!(
        dec.frame().unwrap().pixels,
        px,
        "off-screen tile still stale"
    );
    assert_eq!(enc.stats().deferred_tiles, 1);
    // Off-screen only: nothing to send right now.
    touch(&mut px, 7, 3, 0x33);
    assert_eq!(enc.submit(&px, W, H, None, &mb), SubmitOutcome::Unchanged);
    assert!(!enc.refine_due(), "deferred tiles wait until they settle");
    // Settled: the refine lane delivers them.
    enc.set_clock(Some(t0 + REFINE_DELAY + Duration::from_millis(10)));
    assert!(enc.refine_due());
    while enc.submit_refine(&mb) != SubmitOutcome::Unchanged {
        apply(&mut dec, &mb.take().unwrap());
    }
    assert_eq!(dec.frame().unwrap().pixels, px);
}

#[test]
fn off_viewport_tiles_are_never_held_past_the_max_defer() {
    let (mut enc, mb, mut dec, mut px, t0) = setup(LOCAL_CAPS);
    enc.set_viewport(Some(LEFT));
    // A clock ticking off-screen every 100 ms, plus typing in view: the
    // ticking tile never settles, yet an update carries it within
    // OFFVIEW_MAX_DEFER.
    let mut carried_at = None;
    for i in 0..12u32 {
        let t = t0 + Duration::from_millis(100) * i;
        enc.set_clock(Some(t));
        touch(&mut px, 7, 0, i as u8);
        touch(&mut px, 0, 0, i as u8);
        if enc.submit(&px, W, H, None, &mb) == SubmitOutcome::Unchanged {
            continue;
        }
        let f = mb.take().unwrap();
        if rects_touch_col(&f, 256) && carried_at.is_none() {
            carried_at = Some(t - t0);
        }
        apply(&mut dec, &f);
    }
    let at = carried_at.expect("the off-screen tile was sent");
    assert!(at >= OFFVIEW_MAX_DEFER && at <= OFFVIEW_MAX_DEFER + Duration::from_millis(100));
}

#[test]
fn no_viewport_or_no_cap_means_no_deferral() {
    for (caps, vp) in [(LOCAL_CAPS & !CAP_VIEWPORT, Some(LEFT)), (LOCAL_CAPS, None)] {
        let (mut enc, mb, mut dec, mut px, _) = setup(caps);
        enc.set_viewport(vp);
        touch(&mut px, 1, 1, 0x11);
        touch(&mut px, 6, 2, 0x22);
        assert!(matches!(
            enc.submit(&px, W, H, None, &mb),
            SubmitOutcome::Delta { tiles: 2, .. }
        ));
        apply(&mut dec, &mb.take().unwrap());
        assert_eq!(dec.frame().unwrap().pixels, px);
        assert_eq!(enc.stats().deferred_tiles, 0);
    }
    // A viewport covering every tile defers nothing either.
    let (mut enc, mb, _, mut px, _) = setup(LOCAL_CAPS);
    enc.set_viewport(Some(TileRect {
        x: 0,
        y: 0,
        w: W as u16,
        h: H as u16,
    }));
    touch(&mut px, 6, 2, 0x22);
    assert!(matches!(
        enc.submit(&px, W, H, None, &mb),
        SubmitOutcome::Delta { .. }
    ));
    assert_eq!(enc.stats().deferred_tiles, 0);
}

#[test]
fn panning_brings_deferred_tiles_in_at_once() {
    let (mut enc, mb, mut dec, mut px, _) = setup(LOCAL_CAPS);
    enc.set_viewport(Some(LEFT));
    touch(&mut px, 6, 2, 0x22);
    assert_eq!(enc.submit(&px, W, H, None, &mb), SubmitOutcome::Unchanged);
    assert!(!enc.refine_due());
    // The viewer pans right: the deferred tile is visible now and due
    // immediately (no settle wait).
    enc.set_viewport(Some(TileRect { x: 256, ..LEFT }));
    assert!(enc.refine_due());
    assert!(matches!(
        enc.submit_refine(&mb),
        SubmitOutcome::Delta { .. }
    ));
    apply(&mut dec, &mb.take().unwrap());
    assert_eq!(dec.frame().unwrap().pixels, px);
}

#[test]
fn random_edits_with_a_moving_viewport_converge_exactly() {
    let (mut enc, mb, mut dec, mut px, t0) = setup(LOCAL_CAPS);
    let mut rng = Rng(77);
    for i in 0..300u32 {
        enc.set_clock(Some(t0 + Duration::from_millis(40) * i));
        if i % 25 == 0 {
            let x = rng.below(W - 64) as u16;
            let y = rng.below(H - 64) as u16;
            enc.set_viewport(Some(TileRect {
                x,
                y,
                w: 128,
                h: 96,
            }));
        }
        mutate(&mut px, W, H, &mut rng);
        enc.submit(&px, W, H, None, &mb);
        // The sender keeps up only some of the time.
        if rng.below(3) != 0 {
            if let Some(f) = mb.take() {
                apply(&mut dec, &f);
            }
        }
    }
    if let Some(f) = mb.take() {
        apply(&mut dec, &f);
    }
    enc.set_clock(Some(t0 + Duration::from_secs(60)));
    let mut n = 0;
    while enc.submit_refine(&mb) != SubmitOutcome::Unchanged {
        apply(&mut dec, &mb.take().unwrap());
        n += 1;
        assert!(n < 5000);
    }
    assert_eq!(dec.frame().unwrap().pixels, px);
}

#[test]
fn viewport_command_round_trips_on_the_wire() {
    use crate::terminal::{decode, encode};
    let c = Command::Viewport {
        x: 10,
        y: 20,
        w: 1280,
        h: 720,
        scale_milli: 333,
    };
    assert_eq!(decode(&encode(&c)).unwrap(), c);
    let mut bad = encode(&c);
    bad.pop();
    assert!(decode(&bad).is_err(), "VIEWPORT is exactly 10 bytes");
}
