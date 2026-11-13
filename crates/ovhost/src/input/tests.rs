use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ovapp::terminal::{
    Command, MouseButton, PointerAction, PointerKind, KEY_DOWN, KEY_EXTENDED, POINTER_HAS_POS,
    POINTER_RAW,
};

use super::controller::text_ops;
use super::geometry::{from_virtual_desk, map_to_absolute, map_to_region, to_virtual_desk};
use super::*;

const FULL: Region = Region {
    x: 0,
    y: 0,
    w: 1920,
    h: 1080,
};
const VS: VirtualScreen = VirtualScreen {
    x: 0,
    y: 0,
    w: 1920,
    h: 1080,
};

/// Records every op; the cursor follows absolute moves.
#[derive(Clone)]
struct Rec {
    ops: Arc<Mutex<Vec<InputOp>>>,
    vs: VirtualScreen,
    cursor: Arc<Mutex<Option<(i32, i32)>>>,
    sas_ok: bool,
}

impl Rec {
    fn new(vs: VirtualScreen) -> Self {
        Self {
            ops: Arc::default(),
            vs,
            cursor: Arc::new(Mutex::new(Some((100, 100)))),
            sas_ok: false,
        }
    }
    fn take(&self) -> Vec<InputOp> {
        std::mem::take(&mut *self.ops.lock().unwrap())
    }
}

impl InputBackend for Rec {
    fn send(&mut self, ops: &[InputOp]) -> bool {
        for op in ops {
            if let InputOp::MoveAbs { nx, ny } = op {
                *self.cursor.lock().unwrap() = Some(from_virtual_desk(*nx, *ny, &self.vs));
            }
        }
        self.ops.lock().unwrap().extend_from_slice(ops);
        true
    }
    fn cursor_pos(&mut self) -> Option<(i32, i32)> {
        *self.cursor.lock().unwrap()
    }
    fn send_sas(&mut self) -> Result<&'static str, String> {
        if self.sas_ok {
            Ok("test SAS")
        } else {
            Err("test: no SAS".into())
        }
    }
}

fn controller(region: Region, vs: VirtualScreen) -> (InputController, Rec) {
    let rec = Rec::new(vs);
    (InputController::new(Box::new(rec.clone()), region, vs), rec)
}

#[test]
fn region_mapping_hits_first_and_last_pixel() {
    let r = Region {
        x: 10,
        y: 20,
        w: 100,
        h: 50,
    };
    assert_eq!(map_to_region(0, 0, &r), (10, 20));
    assert_eq!(
        map_to_region(65_535, 65_535, &r),
        (109, 69),
        "65535 = last pixel"
    );
    assert_eq!(map_to_region(u32::MAX, u32::MAX, &r), (109, 69), "clamped");
    // The viewer's px * 65535 / (w - 1) round-trips every pixel.
    for px in 0..100u32 {
        let n = (px * 65_535 + 49) / 99;
        assert_eq!(map_to_region(n, 0, &r).0, 10 + px as i32, "px {px}");
    }
}

#[test]
fn virtual_desk_round_trips_every_pixel_incl_negative_origins() {
    for vs in [
        VS,
        // Secondary monitor left of the primary: origin -1920.
        VirtualScreen {
            x: -1920,
            y: 0,
            w: 3840,
            h: 1080,
        },
        // Monitor above-left, odd sizes.
        VirtualScreen {
            x: -1280,
            y: -1024,
            w: 3200,
            h: 2104,
        },
        VirtualScreen {
            x: -2560,
            y: -300,
            w: 7680,
            h: 1740,
        },
    ] {
        for px in (vs.x..vs.x + vs.w as i32)
            .step_by(7)
            .chain([vs.x + vs.w as i32 - 1])
        {
            let (nx, _) = to_virtual_desk(px, vs.y, &vs);
            assert!((0..=65_535).contains(&nx));
            assert_eq!(from_virtual_desk(nx, 0, &vs).0, px, "vs {vs:?} px {px}");
        }
        for py in (vs.y..vs.y + vs.h as i32)
            .step_by(5)
            .chain([vs.y + vs.h as i32 - 1])
        {
            let (_, ny) = to_virtual_desk(vs.x, py, &vs);
            assert_eq!(from_virtual_desk(0, ny, &vs).1, py, "vs {vs:?} py {py}");
        }
        // Off-screen pixels clamp to the edge.
        assert_eq!(to_virtual_desk(vs.x - 50, vs.y - 50, &vs), (0, 0));
    }
}

#[test]
fn selected_region_on_a_negative_monitor_maps_correctly() {
    // The viewer picked the left monitor (-1920..-1) of a 2-monitor desk.
    let vs = VirtualScreen {
        x: -1920,
        y: -200,
        w: 3840,
        h: 1280,
    };
    let left = Region {
        x: -1920,
        y: -200,
        w: 1920,
        h: 1080,
    };
    let (nx, ny) = map_to_absolute(0, 0, &left, &vs);
    assert_eq!(from_virtual_desk(nx, ny, &vs), (-1920, -200));
    let (nx, ny) = map_to_absolute(65_535, 65_535, &left, &vs);
    assert_eq!(from_virtual_desk(nx, ny, &vs), (-1, 879));
    // Center of the left monitor.
    let (nx, ny) = map_to_absolute(32_768, 32_768, &left, &vs);
    assert_eq!(from_virtual_desk(nx, ny, &vs), (-960, 340));
    // The primary (right) monitor region.
    let right = Region {
        x: 0,
        y: 0,
        w: 1920,
        h: 1080,
    };
    let (nx, ny) = map_to_absolute(0, 0, &right, &vs);
    assert_eq!(from_virtual_desk(nx, ny, &vs), (0, 0));
    let (nx, ny) = map_to_absolute(65_535, 65_535, &right, &vs);
    assert_eq!(from_virtual_desk(nx, ny, &vs), (1919, 1079));
}

#[test]
fn full_screen_region_is_near_identity() {
    for (x, y) in [(0u32, 0u32), (32_767, 32_767), (65_535, 65_535)] {
        let (ax, ay) = map_to_absolute(x, y, &FULL, &VS);
        assert!((i64::from(ax) - i64::from(x)).abs() <= 40, "x {x} -> {ax}");
        assert!((i64::from(ay) - i64::from(y)).abs() <= 70, "y {y} -> {ay}");
    }
}

#[test]
fn legacy_pointer_and_key_keep_working() {
    let (mut c, rec) = controller(FULL, VS);
    let now = Instant::now();
    c.handle(
        &Command::Pointer {
            kind: PointerKind::Down,
            x: 0,
            y: 0,
        },
        now,
    );
    assert_eq!(
        rec.take(),
        vec![
            InputOp::MoveAbs { nx: 0, ny: 0 },
            InputOp::Button {
                button: MouseButton::Left,
                down: true
            }
        ]
    );
    assert!(c.holding());
    c.handle(
        &Command::Pointer {
            kind: PointerKind::Up,
            x: 0,
            y: 0,
        },
        now,
    );
    assert!(!c.holding());
    rec.take();
    // Legacy KEY on an arrow gets the extended flag (NumLock-proof).
    c.handle(
        &Command::Key {
            vk: 0x25,
            down: true,
        },
        now,
    );
    assert_eq!(
        rec.take(),
        vec![InputOp::Key {
            vk: 0x25,
            scan: 0,
            flags: keyflags::EXTENDEDKEY
        }]
    );
    c.handle(
        &Command::Key {
            vk: 0x41,
            down: false,
        },
        now,
    );
    assert_eq!(
        rec.take(),
        vec![InputOp::Key {
            vk: 0x41,
            scan: 0,
            flags: keyflags::KEYUP
        }]
    );
    // Non-input commands are not ours.
    assert_eq!(c.handle(&Command::Ping(1), now), None);
}

#[test]
fn buttons_wheel_and_relative_moves() {
    let (mut c, rec) = controller(FULL, VS);
    let now = Instant::now();
    let pex = |action, button, flags, x, y| Command::PointerEx {
        action,
        button,
        flags,
        x,
        y,
    };
    c.handle(&pex(PointerAction::Down, MouseButton::Right, 0, 0, 0), now);
    c.handle(&pex(PointerAction::Up, MouseButton::Right, 0, 0, 0), now);
    assert_eq!(
        rec.take(),
        vec![
            InputOp::Button {
                button: MouseButton::Right,
                down: true
            },
            InputOp::Button {
                button: MouseButton::Right,
                down: false
            }
        ]
    );
    c.handle(
        &pex(
            PointerAction::Down,
            MouseButton::Middle,
            POINTER_HAS_POS,
            65_535,
            0,
        ),
        now,
    );
    let ops = rec.take();
    assert!(matches!(ops[0], InputOp::MoveAbs { .. }));
    assert_eq!(
        ops[1],
        InputOp::Button {
            button: MouseButton::Middle,
            down: true
        }
    );
    c.handle(&pex(PointerAction::Up, MouseButton::Middle, 0, 0, 0), now);
    rec.take();
    // Relative: exact pixels from the cursor (100,100) -> (90,130).
    *rec.cursor.lock().unwrap() = Some((100, 100));
    c.handle(
        &pex(PointerAction::MoveRel, MouseButton::Left, 0, -10, 30),
        now,
    );
    let ops = rec.take();
    let InputOp::MoveAbs { nx, ny } = ops[0] else {
        panic!("{ops:?}")
    };
    assert_eq!(from_virtual_desk(nx, ny, &VS), (90, 130));
    // Clamped to the region.
    c.handle(
        &pex(PointerAction::MoveRel, MouseButton::Left, 0, -5000, 5000),
        now,
    );
    let InputOp::MoveAbs { nx, ny } = rec.take()[0] else {
        panic!()
    };
    assert_eq!(from_virtual_desk(nx, ny, &VS), (0, 1079));
    // RAW: real relative motion.
    c.handle(
        &pex(
            PointerAction::MoveRel,
            MouseButton::Left,
            POINTER_RAW,
            3,
            -4,
        ),
        now,
    );
    assert_eq!(rec.take(), vec![InputOp::MoveRaw { dx: 3, dy: -4 }]);
    // Wheel: vertical then horizontal.
    c.handle(&Command::Wheel { dy: -240, dx: 60 }, now);
    assert_eq!(rec.take(), vec![InputOp::Wheel(-240), InputOp::HWheel(60)]);
    c.handle(&Command::Wheel { dy: 0, dx: 0 }, now);
    assert!(rec.take().is_empty());
}

#[test]
fn key_ex_extended_and_scancode_only() {
    let (mut c, rec) = controller(FULL, VS);
    let now = Instant::now();
    // Right Ctrl: vk A3, scan 1D, extended per the viewer.
    c.handle(
        &Command::KeyEx {
            vk: 0xA3,
            scan: 0x1D,
            flags: KEY_DOWN | KEY_EXTENDED,
        },
        now,
    );
    // Numpad Enter: vk 0D + extended scan 1C.
    c.handle(
        &Command::KeyEx {
            vk: 0x0D,
            scan: 0x1C,
            flags: KEY_DOWN | KEY_EXTENDED,
        },
        now,
    );
    // Scancode-only key.
    c.handle(
        &Command::KeyEx {
            vk: 0,
            scan: 0x1E,
            flags: KEY_DOWN,
        },
        now,
    );
    // Scan given, no ext flag: the viewer is authoritative (main Enter).
    c.handle(
        &Command::KeyEx {
            vk: 0x2E,
            scan: 0x53,
            flags: KEY_DOWN,
        },
        now,
    );
    // Scan absent: ext derived from vk (Delete).
    c.handle(
        &Command::KeyEx {
            vk: 0x2E,
            scan: 0,
            flags: 0,
        },
        now,
    );
    assert_eq!(
        rec.take(),
        vec![
            InputOp::Key {
                vk: 0xA3,
                scan: 0x1D,
                flags: keyflags::EXTENDEDKEY
            },
            InputOp::Key {
                vk: 0x0D,
                scan: 0x1C,
                flags: keyflags::EXTENDEDKEY
            },
            InputOp::Key {
                vk: 0,
                scan: 0x1E,
                flags: keyflags::SCANCODE
            },
            InputOp::Key {
                vk: 0x2E,
                scan: 0x53,
                flags: 0
            },
            InputOp::Key {
                vk: 0x2E,
                scan: 0,
                flags: keyflags::EXTENDEDKEY | keyflags::KEYUP
            },
        ]
    );
    // Delete was released by the last event; A3, 0D, scan 1E remain.
    assert_eq!(c.held_key_count(), 3);
}

#[test]
fn text_is_unicode_with_surrogate_pairs() {
    let ops = text_ops("hé😀\r\nx\t");
    let units: Vec<(u16, bool)> = ops
        .iter()
        .filter_map(|o| match o {
            InputOp::Unicode { unit, up } => Some((*unit, *up)),
            _ => None,
        })
        .collect();
    assert_eq!(
        units,
        vec![
            (0x68, false),
            (0x68, true),
            (0xE9, false),
            (0xE9, true),
            (0xD83D, false),
            (0xD83D, true),
            (0xDE00, false),
            (0xDE00, true),
            (0x78, false),
            (0x78, true),
        ]
    );
    // "\r\n" is ONE Enter, "\t" a Tab.
    let keys: Vec<&InputOp> = ops
        .iter()
        .filter(|o| matches!(o, InputOp::Key { .. }))
        .collect();
    assert_eq!(keys.len(), 4);
    assert!(matches!(keys[0], InputOp::Key { vk: vk::RETURN, .. }));
    assert!(matches!(keys[2], InputOp::Key { vk: vk::TAB, .. }));
    // Text never leaves anything held.
    let (mut c, _rec) = controller(FULL, VS);
    c.handle(&Command::Text("héllo 😀".into()), Instant::now());
    assert!(!c.holding());
}

#[test]
fn release_all_releases_everything_it_pressed() {
    let (mut c, rec) = controller(FULL, VS);
    let now = Instant::now();
    c.handle(
        &Command::KeyEx {
            vk: 0xA2,
            scan: 0x1D,
            flags: KEY_DOWN,
        },
        now,
    );
    c.handle(
        &Command::KeyEx {
            vk: 0x10,
            scan: 0x2A,
            flags: KEY_DOWN,
        },
        now,
    );
    c.handle(
        &Command::PointerEx {
            action: PointerAction::Down,
            button: MouseButton::Left,
            flags: 0,
            x: 0,
            y: 0,
        },
        now,
    );
    // A repeated down (auto-repeat) is tracked once.
    c.handle(
        &Command::KeyEx {
            vk: 0x10,
            scan: 0x2A,
            flags: KEY_DOWN,
        },
        now,
    );
    rec.take();
    assert_eq!(c.handle(&Command::ReleaseAll, now), Some(true));
    assert_eq!(
        rec.take(),
        vec![
            InputOp::Button {
                button: MouseButton::Left,
                down: false
            },
            InputOp::Key {
                vk: 0x10,
                scan: 0x2A,
                flags: keyflags::KEYUP
            },
            InputOp::Key {
                vk: 0xA2,
                scan: 0x1D,
                flags: keyflags::KEYUP
            },
        ]
    );
    assert!(!c.holding());
    // Nothing held: RELEASE_ALL is a no-op.
    c.handle(&Command::ReleaseAll, now);
    assert!(rec.take().is_empty());
}

#[test]
fn stuck_input_is_released_after_silence_and_on_drop() {
    let (mut c, rec) = controller(FULL, VS);
    let t0 = Instant::now();
    c.handle(
        &Command::Key {
            vk: 0x11,
            down: true,
        },
        t0,
    );
    rec.take();
    assert_eq!(
        c.tick(t0 + Duration::from_secs(29)),
        0,
        "still within the window"
    );
    assert!(c.holding());
    assert_eq!(c.tick(t0 + STUCK_RELEASE_AFTER), 1);
    assert_eq!(
        rec.take(),
        vec![InputOp::Key {
            vk: 0x11,
            scan: 0,
            flags: keyflags::KEYUP
        }]
    );
    // Activity resets the clock.
    c.handle(
        &Command::Key {
            vk: 0x12,
            down: true,
        },
        t0 + Duration::from_secs(40),
    );
    assert_eq!(c.tick(t0 + Duration::from_secs(60)), 0);
    rec.take();
    // Session end: dropping the controller releases.
    drop(c);
    assert_eq!(
        rec.take(),
        vec![InputOp::Key {
            vk: 0x12,
            scan: 0,
            flags: keyflags::KEYUP
        }]
    );
}

#[test]
fn sas_falls_back_to_keystrokes() {
    let (mut c, rec) = controller(FULL, VS);
    assert_eq!(c.handle(&Command::Sas, Instant::now()), Some(true));
    let ops = rec.take();
    assert_eq!(ops.len(), 6, "ctrl alt del down + up");
    assert!(!c.holding(), "the fallback leaves nothing held");
    let mut rec2 = Rec::new(VS);
    rec2.sas_ok = true;
    let mut c2 = InputController::new(Box::new(rec2.clone()), FULL, VS);
    c2.handle(&Command::Sas, Instant::now());
    assert!(rec2.take().is_empty(), "real SAS: no keystrokes");
}

#[test]
fn extended_vk_table() {
    for v in [
        0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x2D, 0x2E, 0x5B, 0x5C, 0xA3, 0xA5,
    ] {
        assert!(is_extended_vk(v), "{v:#x}");
    }
    for v in [0x0D, 0x41, 0x11, 0xA2, 0xA4, 0x60, 0x70] {
        assert!(!is_extended_vk(v), "{v:#x}");
    }
}

#[test]
fn dry_run_backend_tracks_the_cursor() {
    let vs = VirtualScreen {
        x: -100,
        y: 0,
        w: 300,
        h: 100,
    };
    let mut b = DryRunBackend::new(vs);
    let (nx, ny) = to_virtual_desk(-50, 20, &vs);
    assert!(b.send(&[
        InputOp::MoveAbs { nx, ny },
        InputOp::MoveRaw { dx: 5, dy: 5 }
    ]));
    assert_eq!(b.cursor_pos(), Some((-45, 25)));
}
