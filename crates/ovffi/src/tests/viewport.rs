//! `ov_viewport_record`: the VIEWPORT payload the iOS shell sends.

use ovapp::terminal::{decode, Command, CAP_VIEWPORT, LOCAL_CAPS};

use crate::session::ov_viewport_record;

#[test]
fn viewport_record_encodes_a_viewport_command() {
    let mut out = [0u8; 32];
    // SAFETY: `out` is writable for its length.
    let n = unsafe { ov_viewport_record(10, 20, 640, 360, 1500, out.as_mut_ptr(), out.len()) };
    assert_eq!(n, 11);
    assert_eq!(
        decode(&out[..n as usize]).unwrap(),
        Command::Viewport {
            x: 10,
            y: 20,
            w: 640,
            h: 360,
            scale_milli: 1500
        }
    );
    // Too small / null buffers are refused.
    // SAFETY: as above; null is allowed.
    unsafe {
        assert_eq!(
            ov_viewport_record(0, 0, 1, 1, 1000, out.as_mut_ptr(), 10),
            -1
        );
        assert_eq!(
            ov_viewport_record(0, 0, 1, 1, 1000, std::ptr::null_mut(), 64),
            -1
        );
    }
    assert_eq!(CAP_VIEWPORT, 1 << 22);
    assert_ne!(LOCAL_CAPS & CAP_VIEWPORT, 0, "every host advertises it");
}
