//! One inbound session: park on the relay until a viewer attaches (or, in
//! unattended mode, take a viewer's HELLO on the LAN port — [`crate::lan`]),
//! run the server-side handshake, then serve the session in terminal mode
//! ([`terminal`]) or screen mode ([`screen`]).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ovapp::terminal::TerminalSession;
use ovcommon::relay_addr::RelayAddr;
use ovcommon::status::{self, StatusEvent};
use ovcore::handshake::{Auth, Handshake, HandshakeEvent, Role};
use ovcore::identity::Identity;
use ovtransport::{Carrier, RelayTunnelCarrier, TunnelRole, UdpP2pCarrier, WsEndpoint};
use tracing::{info, trace, warn};

use crate::lan::{LanListener, LanPending};
use crate::mic::MicKind;
use crate::{clipboard_os, settings, unattended};

mod capture_loop;
mod direct;
mod files;
pub(crate) mod fx;
mod hooks;
mod liveness;
mod screen;
mod terminal;

/// Everything a session needs from the command line.
pub(crate) struct SessionOpts {
    pub(crate) relay: RelayAddr,
    /// `None` = terminal-only session; `Some(None)` = full primary monitor.
    pub(crate) screen: Option<Option<(u32, u32)>>,
    pub(crate) fps: u32,
    pub(crate) mic: Option<MicKind>,
    pub(crate) audio_playback: bool,
    pub(crate) clipboard: bool,
    /// `Some` = unattended mode with this PIN.
    pub(crate) pin: Option<String>,
    pub(crate) data_dir: PathBuf,
    pub(crate) send_file: Option<PathBuf>,
}

/// How long the relay may take to answer a park before we re-park (its own
/// unclaimed-park timeout is 30 s; this only bounds a relay that went
/// silent).
const PARK_WAIT: Duration = Duration::from_secs(35);
/// Handshake budget once the relay attached a viewer.
const HANDSHAKE_AFTER_ATTACH: Duration = Duration::from_secs(10);
/// A park that ends sooner than this was refused rather than timed out
/// (e.g. the relay still holds our previous slot); pause before retrying so
/// a refusing relay is not hammered.
const MIN_PARK: Duration = Duration::from_secs(2);
/// While parked, the LAN listener is checked this often.
const LAN_POLL: Duration = Duration::from_millis(200);
/// Longest pause between relay connection attempts while LAN sessions keep
/// the host useful.
const MAX_RELAY_RETRY: Duration = Duration::from_secs(30);

/// Where the session's viewer came from.
enum Inbound {
    /// A viewer attached through the relay.
    Relay(RelayTunnelCarrier),
    /// A viewer's HELLO arrived on the LAN port.
    Lan(LanPending),
}

/// How often the session loop polls the OS clipboard sequence number.
const CLIPBOARD_POLL: Duration = Duration::from_millis(50);
/// Longest idle wait when the socket cannot be watched (no readiness
/// registration): bounds inbound latency on such carriers.
const UNWATCHED_WAIT: Duration = Duration::from_millis(2);

/// Serve exactly one inbound session (blocks until the peer disconnects).
/// `lan` (unattended mode) also accepts a viewer on the LAN port.
pub(crate) fn listen_once(opts: &SessionOpts, lan: Option<&mut LanListener>) -> Result<(), String> {
    let relay = &opts.relay;
    // Unattended mode: a STABLE identity persisted in the data dir (the
    // device id must not change between runs — viewers dial it by ID), and
    // PIN auth instead of the pairing code.
    let (me, auth, session_id) = if let Some(pin) = opts.pin.as_deref() {
        let me = unattended::load_or_create_identity(&opts.data_dir)?;
        let psk = ovcore::pin::pin_psk(pin.as_bytes());
        let my_dev = unattended::device_id(&me);
        let session_id = (my_dev & 0xFFFF_FFFF) as u32;
        info!("device id {my_dev} (unattended)");
        status::emit(&StatusEvent::DeviceId {
            id: my_dev.to_string(),
        });
        (me, Auth::Pin(psk), session_id)
    } else {
        let me = Identity::generate();
        let code = me.pairing_code();
        info!("fingerprint  {}", me.fingerprint_pine());
        info!("pairing code {code}");
        status::emit(&StatusEvent::PairingCode {
            code: code.to_string(),
            fingerprint: hex::encode(me.fingerprint()),
        });
        let session_id = u32::from_be_bytes(
            code.as_bytes()[..4]
                .try_into()
                .map_err(|_| "pairing code shorter than 4 bytes".to_owned())?,
        );
        (me, Auth::PairingCode(code), session_id)
    };
    let addr = relay;
    info!("waiting on relay {addr} (session {session_id:08x})");
    status::emit(&StatusEvent::Waiting {
        relay: addr.to_string(),
    });
    // Hold the relay slot until a viewer attaches: the relay times an
    // unclaimed park out after its ~30 s window, and that is normal idle —
    // re-park immediately with the same identity/code (no backoff), so an
    // attended host stays reachable for as long as it runs and a dialing
    // viewer never falls into a gap.
    let my_dev = unattended::device_id(&me);
    let mut hs = Handshake::new(Role::Server, my_dev, &me, auth);
    match acquire(addr, session_id, lan)? {
        Inbound::Relay(carrier) => serve(carrier, hs, opts),
        Inbound::Lan(p) => {
            info!("LAN viewer {}", p.peer);
            // Per-source PIN attempt limiting (the viewer's address is real
            // here, unlike behind the relay).
            hs.set_auth_source(p.peer.ip());
            let carrier = UdpP2pCarrier::from_socket(p.socket, p.peer);
            // The HELLO the listener already read.
            let _ = hs.feed(&p.first);
            serve(carrier, hs, opts)
        }
    }
}

/// Run the handshake on `carrier` and serve the session.
fn serve<C: Carrier + 'static>(
    mut carrier: C,
    mut hs: Handshake,
    opts: &SessionOpts,
) -> Result<(), String> {
    let SessionOpts {
        screen,
        fps,
        mic,
        audio_playback,
        clipboard,
        ..
    } = *opts;
    // The viewer starts its handshake the moment the relay attaches it (or
    // already sent it, on the LAN), so the handshake timer only starts now.
    let backlog = handshake_over(&mut hs, &mut carrier, HANDSHAKE_AFTER_ATTACH)?;
    let peer_fp = hs
        .peer()
        .ok_or("handshake established without peer info")?
        .fingerprint;
    info!("peer connected, fingerprint {}", hex::encode(peer_fp));
    status::emit(&StatusEvent::PeerConnected {
        fingerprint: hex::encode(peer_fp),
    });

    let session = hs.take_session().ok_or("no session after handshake")?;
    let mut term = TerminalSession::new(session, carrier);
    // Datagrams polled in the batch that completed the handshake.
    term.queue_inbound(backlog);
    // Offer a direct UDP path (the relay stays the fallback).
    if let Some((sockets, cands)) = settings::DIRECT.get().and_then(|d| d.for_session()) {
        term.enable_direct(sockets, cands);
    }
    // `--send-file`: push the file to the peer right after the handshake.
    if let Some(path) = opts.send_file.as_deref() {
        let data =
            std::fs::read(path).map_err(|e| format!("--send-file {}: {e}", path.display()))?;
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_owned();
        info!("sending {name} ({} bytes)", data.len());
        term.send_file_xfer(&name, &data, 0)
            .map_err(|e| format!("send {name}: {e}"))?;
    }
    let result = match screen {
        // Terminal mode; with `--audio` a playback bus for the viewer's mic
        // rides along (full-duplex audio).
        None => terminal::serve_terminal(term, clipboard, audio_playback, &opts.data_dir),
        Some(region) => {
            // Hide the wallpaper / stop animations while the viewer
            // watches; restored when `fx` drops.
            let cfg = settings::FX_CFG.get().copied().unwrap_or(fx::FxCfg::NONE);
            let fx = fx::SessionFx::start(cfg);
            term.set_local_caps(term.local_caps() | ovapp::terminal::CAP_DESKTOP_FX);
            let r = screen::serve_screen(
                term,
                screen::ScreenOpts {
                    region,
                    fps,
                    mic,
                    audio_playback,
                    clipboard,
                    fx: Some(Arc::clone(&fx.fx)),
                },
                &opts.data_dir,
            );
            drop(fx);
            r
        }
    };
    if let Err(e) = &result {
        info!("session ended: {e}");
        status::emit(&StatusEvent::PeerDisconnected { reason: e.clone() });
    }
    result
}

/// Park on the relay as the RESPONDER until a viewer attaches. A park that
/// times out (`XT`, the relay's normal "nobody came" verdict) re-parks
/// immediately with the same session id; only a failure to reach the relay
/// at all is an error (the service loop backs off on those).
#[cfg(test)]
fn park_until_attached(addr: &RelayAddr, session_id: u32) -> Result<RelayTunnelCarrier, String> {
    match acquire(addr, session_id, None)? {
        Inbound::Relay(c) => Ok(c),
        Inbound::Lan(_) => Err("no LAN listener was given".into()),
    }
}

/// Wait for the next viewer: park on the relay (re-parking after idle
/// timeouts) and, with a LAN listener, accept a LAN HELLO meanwhile.
///
/// Without a LAN listener a relay that cannot be reached is an error (the
/// service loop backs off). With one the host stays reachable on the LAN:
/// relay failures are reported (`session_failed`) and retried with backoff
/// while the LAN port keeps being served.
fn acquire(
    addr: &RelayAddr,
    session_id: u32,
    mut lan: Option<&mut LanListener>,
) -> Result<Inbound, String> {
    if let Some(l) = lan.as_deref_mut() {
        l.discard_stale();
    }
    let mut announced = false;
    let mut retry = Duration::from_secs(1);
    loop {
        if let Some(p) = lan.as_deref_mut().and_then(LanListener::poll) {
            return Ok(Inbound::Lan(p));
        }
        let parked_at = Instant::now();
        let mut carrier = match dial(addr, session_id, TunnelRole::Responder) {
            Ok(c) => {
                retry = Duration::from_secs(1);
                c
            }
            Err(e) => {
                let message = format!("relay {addr}: connect failed: {e}");
                let Some(l) = lan.as_deref_mut() else {
                    return Err(message);
                };
                warn!("{message}; LAN viewers are still accepted, retrying the relay in {retry:?}");
                status::emit(&StatusEvent::SessionFailed {
                    message,
                    retry_in_secs: retry.as_secs(),
                });
                announced = false;
                if let Some(p) = l.wait(retry) {
                    return Ok(Inbound::Lan(p));
                }
                retry = (retry * 2).min(MAX_RELAY_RETRY);
                continue;
            }
        };
        if !std::mem::replace(&mut announced, true) {
            info!("ready: parked on relay {addr}");
            status::emit(&StatusEvent::Ready {
                relay: addr.to_string(),
            });
        }
        // Wait for the attach in slices so a LAN viewer is not kept waiting
        // for the whole park window.
        let deadline = parked_at + PARK_WAIT;
        let outcome = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let slice = if lan.is_some() {
                left.min(LAN_POLL)
            } else {
                left
            };
            match carrier.wait_attached(slice) {
                Ok(true) => return Ok(Inbound::Relay(carrier)),
                Ok(false) if Instant::now() < deadline => {
                    if let Some(p) = lan.as_deref_mut().and_then(LanListener::poll) {
                        // Give the relay slot up; the LAN viewer wins.
                        let _ = carrier.close();
                        return Ok(Inbound::Lan(p));
                    }
                }
                other => break other,
            }
        };
        match outcome {
            Ok(true) => return Ok(Inbound::Relay(carrier)),
            // Relay silent for the whole window, or it timed the park out:
            // both are idle, not failure.
            Ok(false) | Err(ovtransport::TransportError::Closed) => {
                let _ = carrier.close();
                trace!("relay park expired; re-parking");
                if parked_at.elapsed() < MIN_PARK {
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
            Err(e) => return Err(format!("relay park failed: {e}")),
        }
    }
}

/// Dial the relay at `addr` — raw TCP (hostnames resolved, every address
/// tried) or WebSocket (`ws://` / `wss://`) — and send CONNECT.
fn dial(
    addr: &RelayAddr,
    session_id: u32,
    role: TunnelRole,
) -> Result<RelayTunnelCarrier, ovtransport::TransportError> {
    match addr {
        RelayAddr::Tcp { host, port } => {
            RelayTunnelCarrier::connect_host(host, *port, session_id, role)
        }
        RelayAddr::WebSocket {
            tls,
            host,
            port,
            path,
            ..
        } => RelayTunnelCarrier::connect_ws(
            &WsEndpoint {
                host: host.clone(),
                port: *port,
                path: path.clone(),
                tls: *tls,
            },
            session_id,
            role,
        ),
    }
}

/// Pump the server-side handshake to completion. Returns any datagrams that
/// arrived after establishment within the same poll batch (they are session
/// records — feeding them to the established handshake would drop them).
fn handshake_over(
    hs: &mut Handshake,
    carrier: &mut dyn Carrier,
    timeout: Duration,
) -> Result<Vec<Vec<u8>>, String> {
    let iterations = timeout.as_millis().div_ceil(10) as usize;
    let mut backlog = Vec::new();
    for _ in 0..iterations {
        if hs.is_established() {
            return Ok(backlog);
        }
        if let Some(e) = hs.failed() {
            return Err(format!("handshake failed: {e}"));
        }
        carrier
            .poll_recv(&mut |dgram| {
                if hs.is_established() {
                    backlog.push(dgram.to_vec());
                } else {
                    let _ = hs.feed(dgram);
                }
            })
            .map_err(|e| format!("handshake recv: {e}"))?;
        for ev in hs.take_events() {
            let HandshakeEvent::Sent(d) = ev;
            let _ = carrier.send(&d);
        }
        if hs.is_established() {
            return Ok(backlog);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Err("handshake timed out".into())
}

/// The shared clipboard echo-loop guard (`None` = `--no-clipboard`).
type SharedClipboard = Option<Arc<Mutex<ovapp::clipboard::ClipboardSync>>>;

/// Clipboard poll: cheap sequence-number check each loop; on a change, read
/// the text and feed the echo-loop guard. Returns the text to send to the
/// peer, if any. `None` when clipboard sync is off, nothing changed, or the
/// read failed (transient lock contention — retried next poll because the
/// sequence number has not been consumed).
fn clipboard_poll(sync: &SharedClipboard, last_seq: &mut u32) -> Option<String> {
    let sync = sync.as_ref()?;
    let seq = clipboard_os::clip_sequence();
    if seq == *last_seq {
        return None;
    }
    *last_seq = seq;
    let text = clipboard_os::clip_read_text()?;
    let mut s = sync.lock().ok()?;
    s.note_local(&text)
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use super::*;

    /// LAN path end to end without a relay: the host advertises a scratch
    /// device id by mDNS with its LAN port, a viewer resolves it the way
    /// `ovview --id` does, sends its HELLO, and both sides complete the PIN
    /// handshake over the point-to-point UDP carrier and exchange records.
    /// The relay address is unreachable on purpose (LAN keeps working).
    #[test]
    fn lan_viewer_resolves_by_mdns_and_completes_the_pin_handshake() {
        use ovdiscovery::mdns::{self, AdvertiseConfig, Advertiser};

        let host_id = Identity::generate();
        let dev = unattended::device_id(&host_id);
        let mut lan = LanListener::bind(0).expect("LAN listener");
        let port = lan.port();
        let mut cfg = AdvertiseConfig::new(dev, port);
        cfg.name = format!("ovhost-test-{dev}");
        let adv = Advertiser::start(cfg).ok();

        // Viewer (what ovview --id does): resolve, else fall back to the
        // loopback address so the LAN session itself is still tested.
        let viewer = std::thread::spawn(move || {
            let found = mdns::resolve(dev, Duration::from_secs(6)).expect("browser");
            let addr = match &found {
                Some(h) => {
                    assert_eq!(h.name, format!("ovhost-test-{dev}"));
                    assert!(h.addrs.iter().all(|a| a.port() == port), "{h:?}");
                    h.addrs[0]
                }
                None => {
                    eprintln!("note: mDNS lookup unavailable here; dialing loopback");
                    SocketAddr::from(([127, 0, 0, 1], port))
                }
            };
            let me = Identity::generate();
            let mut carrier =
                UdpP2pCarrier::new(SocketAddr::from(([0, 0, 0, 0], 0)), addr).expect("udp");
            let psk = ovcore::pin::pin_psk(b"2468");
            let mut hs = Handshake::new(Role::Client, 77, &me, Auth::Pin(psk));
            hs.expect_device(dev);
            for ev in hs.take_events() {
                let HandshakeEvent::Sent(d) = ev;
                carrier.send(&d).unwrap();
            }
            let backlog = handshake_over(&mut hs, &mut carrier, Duration::from_secs(10))
                .expect("viewer handshake");
            assert!(backlog.is_empty());
            // Raw datagrams flow both ways on the established carrier.
            carrier.send(b"ping").unwrap();
            let mut got = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(5);
            while got.is_empty() && Instant::now() < deadline {
                carrier.poll_recv(&mut |d| got.push(d.to_vec())).unwrap();
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(got, vec![b"pong".to_vec()]);
            found.is_some()
        });

        let relay = RelayAddr::parse("127.0.0.1:1").unwrap();
        let inbound = acquire(&relay, (dev & 0xFFFF_FFFF) as u32, Some(&mut lan)).expect("acquire");
        let Inbound::Lan(p) = inbound else {
            panic!("expected a LAN viewer")
        };
        let psk = ovcore::pin::pin_psk(b"2468");
        let mut hs = Handshake::new(Role::Server, dev, &host_id, Auth::Pin(psk));
        hs.set_auth_source(p.peer.ip());
        let mut carrier = UdpP2pCarrier::from_socket(p.socket, p.peer);
        let _ = hs.feed(&p.first);
        // The viewer's first record may ride in the handshake's last batch.
        let mut got =
            handshake_over(&mut hs, &mut carrier, Duration::from_secs(10)).expect("host handshake");
        assert_eq!(hs.peer().map(|p| p.device_id), Some(77));
        let deadline = Instant::now() + Duration::from_secs(5);
        while got.is_empty() && Instant::now() < deadline {
            carrier.poll_recv(&mut |d| got.push(d.to_vec())).unwrap();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(got, vec![b"ping".to_vec()]);
        carrier.send(b"pong").unwrap();
        let via_mdns = viewer.join().expect("viewer thread");
        if adv.is_some() && !via_mdns {
            eprintln!("note: advertised, but the lookup did not see it (multicast filtered?)");
        }
        drop(adv);
    }

    /// The relay times an unclaimed park out (`XT`) — the host must re-park
    /// at once (not fail, not back off) so a viewer dialing later still
    /// finds it; here the park window is 300 ms and the viewer shows up
    /// after ~1 s, i.e. after several XT cycles.
    #[test]
    fn host_reparks_after_relay_park_timeout() {
        let mut relay = ovrelay::Relay::bind("127.0.0.1:0".parse().unwrap()).expect("bind");
        relay.set_timeout(Duration::from_millis(300));
        let addr = relay.local_addr();
        std::thread::spawn(move || relay.serve(None));
        let sid = 0x5EED_0001;
        let relay_addr = RelayAddr::parse(&addr.to_string()).unwrap();
        let host = std::thread::spawn(move || park_until_attached(&relay_addr, sid));
        std::thread::sleep(Duration::from_millis(1100));
        let mut viewer =
            RelayTunnelCarrier::connect(addr, sid, TunnelRole::Initiator).expect("viewer connect");
        assert!(viewer
            .wait_attached(Duration::from_secs(10))
            .expect("viewer attach"));
        let mut hc = host.join().expect("host thread").expect("host parked");
        assert!(hc.is_attached());
        // The tunnel is live end to end.
        viewer.send(b"hello").unwrap();
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while got.is_empty() && Instant::now() < deadline {
            hc.poll_recv(&mut |d| got.push(d.to_vec())).unwrap();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(got, vec![b"hello".to_vec()]);
    }
}
