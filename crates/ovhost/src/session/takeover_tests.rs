//! End to end through a real relay: an unattended host serving one viewer
//! makes way for the next one — at once after a goodbye, within the
//! takeover grace when the first viewer vanished without one.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::thread::JoinHandle;

use ovapp::terminal::{Command, TerminalSession};

use super::*;

const PIN: &[u8] = b"2468";

/// A relay with the production park window on an ephemeral port.
fn start_relay() -> SocketAddr {
    let relay = ovrelay::Relay::bind("127.0.0.1:0".parse().unwrap()).expect("bind relay");
    let addr = relay.local_addr();
    std::thread::spawn(move || relay.serve(None));
    addr
}

/// A fresh data dir (the host's persisted unattended identity).
fn scratch_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dir = std::env::temp_dir().join(format!(
        "ovhost-takeover-{tag}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Run `sessions` unattended terminal sessions back to back, as the
/// service loop does (standby park on).
fn spawn_host(relay: SocketAddr, data_dir: PathBuf, sessions: usize) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let opts = SessionOpts {
            relay: RelayAddr::parse(&relay.to_string()).unwrap(),
            screen: None,
            fps: 30,
            mic: None,
            audio_playback: false,
            clipboard: false,
            pin: Some(String::from_utf8(PIN.to_vec()).unwrap()),
            data_dir,
            send_file: None,
            standby: true,
        };
        let mut carry = None;
        for _ in 0..sessions {
            listen_once(&opts, None, &mut carry).expect("host session");
        }
    })
}

/// Dial the host through the relay and complete the PIN handshake.
fn connect_viewer(relay: SocketAddr, dev: u64) -> TerminalSession {
    let sid = (dev & 0xFFFF_FFFF) as u32;
    let me = Identity::generate();
    let mut carrier =
        RelayTunnelCarrier::connect(relay, sid, TunnelRole::Initiator).expect("viewer connect");
    assert!(carrier
        .wait_attached(Duration::from_secs(20))
        .expect("viewer attach"));
    let mut hs = Handshake::new(Role::Client, 77, &me, Auth::Pin(ovcore::pin::pin_psk(PIN)));
    hs.expect_device(dev);
    for ev in hs.take_events() {
        let HandshakeEvent::Sent(d) = ev;
        carrier.send(&d).unwrap();
    }
    let backlog = handshake_over(&mut hs, &mut carrier, Duration::from_secs(10), Vec::new())
        .expect("viewer handshake");
    let mut term = TerminalSession::new(hs.take_session().expect("session"), carrier);
    term.queue_inbound(backlog);
    // CAPS both ways, as every viewer does (the goodbye's BYE needs the
    // host's CAP_BYE).
    term.announce_caps().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while term.peer_caps().is_none() {
        assert!(Instant::now() < deadline, "no CAPS from the host");
        term.pump().expect("viewer pump");
        std::thread::sleep(Duration::from_millis(5));
    }
    term
}

/// Pump `term` for `d` (answers the host's keepalive PINGs).
fn pump_for(term: &mut TerminalSession, d: Duration) {
    let end = Instant::now() + d;
    while Instant::now() < end {
        term.pump().expect("viewer pump");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn host_device(dir: &std::path::Path) -> u64 {
    unattended::device_id(&unattended::load_or_create_identity(dir).unwrap())
}

#[test]
fn a_goodbye_frees_the_host_for_the_next_viewer_at_once() {
    let relay = start_relay();
    let dir = scratch_dir("bye");
    let dev = host_device(&dir);
    let host = spawn_host(relay, dir.clone(), 2);
    let mut a = connect_viewer(relay, dev);
    pump_for(&mut a, Duration::from_millis(300));
    a.goodbye().unwrap();
    let t = Instant::now();
    let mut b = connect_viewer(relay, dev);
    let took = t.elapsed();
    assert!(took < Duration::from_secs(2), "B took {took:?}");
    b.send(&Command::Echo(b"hi".to_vec())).unwrap();
    pump_for(&mut b, Duration::from_millis(200));
    b.goodbye().unwrap();
    host.join().expect("host thread");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_vanished_viewer_makes_way_for_the_next_one_within_the_grace() {
    let relay = start_relay();
    let dir = scratch_dir("vanish");
    let dev = host_device(&dir);
    let host = spawn_host(relay, dir.clone(), 2);
    let mut a = connect_viewer(relay, dev);
    pump_for(&mut a, Duration::from_millis(300));
    // A goes silent without a goodbye: its relay socket stays open (as
    // behind a VPN that keeps the local TCP connection), nothing answers.
    let t = Instant::now();
    let mut b = connect_viewer(relay, dev);
    let took = t.elapsed();
    assert!(
        took < Duration::from_secs(5),
        "B must be served once A is silent for the grace, took {took:?}"
    );
    // A's session is over on the host: its carrier was told.
    let deadline = Instant::now() + Duration::from_secs(5);
    while a.pump().is_ok() {
        assert!(Instant::now() < deadline, "A was never disconnected");
        std::thread::sleep(Duration::from_millis(10));
    }
    pump_for(&mut b, Duration::from_millis(200));
    b.goodbye().unwrap();
    host.join().expect("host thread");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_live_viewer_keeps_its_session_while_another_waits() {
    let relay = start_relay();
    let dir = scratch_dir("live");
    let dev = host_device(&dir);
    let host = spawn_host(relay, dir.clone(), 2);
    let mut a = connect_viewer(relay, dev);
    // B dials while A is alive and answering: B waits, A stays.
    let b = std::thread::spawn(move || {
        let mut b = connect_viewer(relay, dev);
        let served = Instant::now();
        pump_for(&mut b, Duration::from_millis(200));
        b.goodbye().unwrap();
        served
    });
    pump_for(&mut a, standby::TAKEOVER_GRACE * 2);
    assert!(!b.is_finished(), "B must wait while A is alive");
    // A leaves properly: B gets the host.
    let left = Instant::now();
    a.goodbye().unwrap();
    let served = b.join().expect("viewer B");
    assert!(served >= left, "B was served before A left");
    host.join().expect("host thread");
    let _ = std::fs::remove_dir_all(dir);
}
