//! Input-v2 smoke viewer: dial a host by pairing code through a relay,
//! wait for its CAPS, then send a scripted input sequence (right click,
//! wheel, Unicode text, relative moves, a held key + RELEASE_ALL, SAS) and
//! exit. Run the host with `--dry-input --verbose` so nothing is injected:
//!
//! ```text
//! cargo run -p ovview --example input_smoke -- 127.0.0.1:PORT CODE
//! ```

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use ovapp::terminal::{
    Command, MouseButton, PointerAction, TerminalSession, WheelAccumulator, CAP_INPUT2, KEY_DOWN,
    POINTER_HAS_POS,
};
use ovcore::handshake::{Auth, Handshake, HandshakeEvent, Role};
use ovcore::identity::Identity;
use ovtransport::{Carrier, RelayTunnelCarrier, TunnelRole};

fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let relay: SocketAddr = args
        .next()
        .ok_or("usage: input_smoke RELAY_ADDR CODE")?
        .parse()
        .map_err(|e| format!("bad relay: {e}"))?;
    let code = args.next().ok_or("usage: input_smoke RELAY_ADDR CODE")?;
    let sid = u32::from_be_bytes(code.as_bytes()[..4].try_into().map_err(|_| "short code")?);
    let mut carrier = RelayTunnelCarrier::connect(relay, sid, TunnelRole::Initiator)
        .map_err(|e| format!("relay connect: {e}"))?;
    if !carrier
        .wait_attached(Duration::from_secs(10))
        .map_err(|e| e.to_string())?
    {
        return Err("host did not attach".into());
    }
    let me = Identity::generate();
    let mut hs = Handshake::new(Role::Client, 0x5170_4E00, &me, Auth::PairingCode(code));
    let mut backlog = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !hs.is_established() {
        if Instant::now() > deadline {
            return Err("handshake timed out".into());
        }
        if let Some(e) = hs.failed() {
            return Err(format!("handshake failed: {e}"));
        }
        for ev in hs.take_events() {
            let HandshakeEvent::Sent(d) = ev;
            let _ = carrier.send(&d);
        }
        carrier
            .poll_recv(&mut |d| {
                if hs.is_established() {
                    backlog.push(d.to_vec());
                } else {
                    let _ = hs.feed(d);
                }
            })
            .map_err(|e| e.to_string())?;
        for ev in hs.take_events() {
            let HandshakeEvent::Sent(d) = ev;
            let _ = carrier.send(&d);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let mut term = TerminalSession::new(hs.take_session().ok_or("no session")?, carrier);
    term.queue_inbound(backlog);
    term.announce_caps().map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while term.peer_caps().is_none() && Instant::now() < deadline {
        term.pump().map_err(|e| e.to_string())?;
        std::thread::sleep(Duration::from_millis(5));
    }
    let caps = term.peer_caps().ok_or("host never sent CAPS")?;
    println!(
        "smoke: host caps {caps:#x} (input2 {})",
        caps & CAP_INPUT2 != 0
    );
    if caps & CAP_INPUT2 == 0 {
        return Err("host lacks CAP_INPUT2".into());
    }

    let pex = |action, button, flags, x, y| Command::PointerEx {
        action,
        button,
        flags,
        x,
        y,
    };
    // A trackpad-style wheel: 0.3 + 0.3 + 0.5 notches -> one WHEEL of 132
    // units, the remainder carried.
    let mut wheel = WheelAccumulator::default();
    let mut script = vec![
        // Right click at the frame center.
        pex(
            PointerAction::Down,
            MouseButton::Right,
            POINTER_HAS_POS,
            32_768,
            32_768,
        ),
        pex(
            PointerAction::Up,
            MouseButton::Right,
            POINTER_HAS_POS,
            32_768,
            32_768,
        ),
        // Middle click at the cursor.
        pex(PointerAction::Down, MouseButton::Middle, 0, 0, 0),
        pex(PointerAction::Up, MouseButton::Middle, 0, 0, 0),
        // Relative moves (exact pixels) and one raw one.
        pex(PointerAction::MoveRel, MouseButton::Left, 0, 25, -10),
        pex(PointerAction::MoveRel, MouseButton::Left, 0, -5, 40),
        pex(
            PointerAction::MoveRel,
            MouseButton::Left,
            ovapp::terminal::POINTER_RAW,
            3,
            3,
        ),
        // Unicode text incl. an astral emoji (surrogate pair) and a newline.
        Command::Text("héllo 😀\n".into()),
        // Extended key (Right arrow) tap.
        Command::KeyEx {
            vk: 0x27,
            scan: 0x4D,
            flags: KEY_DOWN | ovapp::terminal::KEY_EXTENDED,
        },
        Command::KeyEx {
            vk: 0x27,
            scan: 0x4D,
            flags: ovapp::terminal::KEY_EXTENDED,
        },
        // Held Ctrl + held left button, then RELEASE_ALL.
        Command::KeyEx {
            vk: 0xA2,
            scan: 0x1D,
            flags: KEY_DOWN,
        },
        pex(PointerAction::Down, MouseButton::Left, 0, 0, 0),
        Command::ReleaseAll,
        // Ctrl+Alt+Del.
        Command::Sas,
    ];
    for notch in [0.3, 0.3, 0.5] {
        wheel.add(notch * 120.0, 0.0);
    }
    if let Some(w) = wheel.take() {
        script.insert(4, w);
    }
    script.insert(5, Command::Wheel { dy: 0, dx: -120 });
    for cmd in &script {
        term.send(cmd).map_err(|e| format!("send {cmd:?}: {e}"))?;
        term.pump().map_err(|e| e.to_string())?;
        std::thread::sleep(Duration::from_millis(30));
    }
    println!("smoke: sent {} input commands", script.len());
    let until = Instant::now() + Duration::from_millis(800);
    while Instant::now() < until {
        let _ = term.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = term.close();
    Ok(())
}
