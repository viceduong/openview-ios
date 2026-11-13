//! `ovhost` — the OpenView host: makes this computer reachable through an
//! OpenView relay (and upgrades to a direct UDP path when the network
//! allows), then streams the screen and injects the viewer's input.
//!
//! - `ovhost --unattended --service --screen` (PIN in `OPENVIEW_PIN`): what
//!   the GUI runs — a stable device ID + PIN authentication, serving
//!   sessions forever.
//! - `ovhost --screen`: attended mode — prints a one-time pairing code and
//!   serves exactly one session.
//!
//! Run `ovhost --help` for every option. Output contract:
//!
//! - **stdout** carries machine-readable status events, one JSON object per
//!   line ([`ovcommon::status`]); the GUI parses these.
//! - **stderr** carries human-readable logs (`tracing`): lifecycle lines at
//!   the default `info` level, periodic statistics with `-v`.
//!
//! The relay session id is the device ID's low 32 bits (unattended) or the
//! pairing code's first 4 bytes (matches what `ovview` dials).
//!
//! Network: the relay connection is outbound TCP (`--relay`); the direct
//! path binds UDP `0.0.0.0:<--direct-port>` (+ `[::]` when available) —
//! the only inbound port the host needs from the Internet (UPnP maps it on
//! the router when allowed). The host also accepts sessions from its own
//! network without the relay on UDP `<--lan-port>` (default 4571); in
//! unattended mode it announces itself there by mDNS (UDP 5353,
//! `_openview._udp`); see [`lan`].
//!
//! Module map:
//!
//! - [`cli`] / [`settings`]: the command line and the process-wide switches
//!   it sets;
//! - [`session`]: one relay park + handshake + session, in terminal or
//!   screen mode (capture thread, mic thread, input hooks);
//! - [`capture`] / `dxgi`: the screen source (DXGI Desktop Duplication or
//!   GDI); [`cursor`]: the cursor overlay; [`input`]: input injection;
//! - [`mic`] / [`speaker`] / `sysaudio`: audio in both directions;
//! - [`clipboard_os`], [`desktop_fx`], [`direct_net`], [`lan`], [`cleanup`],
//!   [`unattended`]: OS integration and networking helpers.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(clippy::undocumented_unsafe_blocks)]

use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use ovcommon::status::{self, DirectState, StatusEvent};
use tracing::{error, info, warn};

mod capture;
mod cleanup;
mod cli;
mod clipboard_os;
mod cursor;
mod desktop_fx;
mod direct_net;
#[cfg(windows)]
mod dxgi;
mod input;
mod lan;
mod mic;
mod session;
mod settings;
mod speaker;
#[cfg(windows)]
mod sysaudio;
mod unattended;

use cli::{Cli, ScreenArea};
use mic::MicKind;
use session::fx::{fx_backend, FxCfg};
use session::SessionOpts;

fn main() -> ExitCode {
    let cli = Cli::parse();
    ovcommon::logging::init(cli.verbose, cli.log_level.as_deref());
    let rc = match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e}");
            status::emit(&StatusEvent::Error { message: e });
            ExitCode::FAILURE
        }
    };
    // Delete the UPnP mapping (and anything else registered).
    cleanup::run_all();
    rc
}

fn run(cli: Cli) -> Result<(), String> {
    info!("OpenView host {} starting", env!("CARGO_PKG_VERSION"));
    settings::apply(&cli);
    // 1 ms waits for the direct path's packet pacer.
    ovapp::netwait::high_res_timers();

    let pin = cli.pin.filter(|p| !p.is_empty());
    if cli.unattended && pin.is_none() {
        return Err("--unattended needs a PIN (OPENVIEW_PIN or --pin)".into());
    }
    let data_dir = ovcommon::paths::ensure_data_dir(cli.data_dir.as_deref())
        .map_err(|e| format!("cannot create the data directory: {e}"))?;
    let opts = SessionOpts {
        relay: cli.relay,
        screen: cli.screen.map(ScreenArea::region),
        fps: cli.fps,
        mic: if cli.system_audio {
            Some(MicKind::System)
        } else if cli.mic {
            Some(MicKind::Real)
        } else if cli.mic_synth {
            Some(MicKind::Synth)
        } else {
            None
        },
        audio_playback: cli.audio,
        clipboard: !cli.no_clipboard,
        pin,
        data_dir,
        send_file: cli.send_file,
    };
    let fx = FxCfg {
        hide_wallpaper: !cli.keep_wallpaper,
        no_animations: !cli.keep_animations,
        flat_color: cli.desktop_color,
        dry_run: cli.fx_dry_run,
    };

    // Restore-on-exit (Ctrl-C, console close, panic) for desktop effects
    // and the UPnP mapping; then undo a crashed run's leftover effects.
    cleanup::install();
    desktop_fx::set_marker_dir(&opts.data_dir);
    for marker in std::iter::once(desktop_fx::default_marker()).chain(desktop_fx::legacy_marker()) {
        let mut be = fx_backend(fx.dry_run);
        if desktop_fx::DesktopFx::recover_stale(&mut *be, &marker) {
            info!("restored desktop effects left over by a previous run");
        }
    }
    let _ = settings::FX_CFG.set(fx);
    if cli.no_direct {
        status::emit(&StatusEvent::DirectPath {
            state: DirectState::Disabled,
            port: None,
            addr: None,
            detail: Some("--no-direct".into()),
        });
    } else {
        let d = direct_net::DirectOpts {
            port: cli.direct_port,
            upnp: !cli.no_upnp,
            stun: (!cli.no_stun).then_some(cli.stun),
            advertise: (!cli.direct_advertise.is_empty()).then_some(cli.direct_advertise),
        };
        match direct_net::DirectNet::start(&d) {
            Ok(n) => {
                let _ = settings::DIRECT.set(n);
            }
            Err(e) => {
                warn!("direct path disabled (UDP {}: {e})", d.port);
                status::emit(&StatusEvent::DirectPath {
                    state: DirectState::Disabled,
                    port: Some(d.port),
                    addr: None,
                    detail: Some(e.to_string()),
                });
            }
        }
    }

    // Accept viewers on this network without the relay; unattended hosts
    // are also announced by mDNS (goodbye sent at exit). Attended hosts are
    // dialed by address + pairing code (their identity changes every run).
    let mut lan = if cli.no_lan {
        None
    } else {
        start_lan(&opts, cli.lan_port, opts.pin.is_some() && !cli.no_mdns)
    };

    if !cli.service {
        return session::listen_once(&opts, lan.as_mut());
    }
    // Serve forever; back off after errors so a wedged relay or bad peer
    // cannot spin the CPU. An idle relay park (`XT`, nobody dialed within
    // the relay's window) is not an error: `listen_once` re-parks
    // internally without touching the backoff.
    let mut backoff = Duration::from_secs(1);
    loop {
        match session::listen_once(&opts, lan.as_mut()) {
            Ok(()) => backoff = Duration::from_secs(1),
            Err(e) => {
                warn!("session failed: {e}; retrying in {backoff:?}");
                status::emit(&StatusEvent::SessionFailed {
                    message: e,
                    retry_in_secs: backoff.as_secs(),
                });
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

/// Bind the LAN session port and start the mDNS advertisement (kept for the
/// process lifetime; the cleanup registry sends the goodbye at exit).
fn start_lan(opts: &SessionOpts, port: u16, mdns: bool) -> Option<lan::LanListener> {
    let listener = match lan::LanListener::bind(port) {
        Ok(l) => l,
        Err(e) => {
            warn!("LAN sessions disabled (UDP {port}: {e})");
            status::emit(&StatusEvent::Lan {
                port: None,
                mdns: false,
                detail: Some(format!("UDP {port}: {e}")),
            });
            return None;
        }
    };
    let port = listener.port();
    let mut detail = None;
    let advertised = mdns
        && match unattended::load_or_create_identity(&opts.data_dir) {
            Ok(me) => {
                let device_id = unattended::device_id(&me);
                let mut cfg = ovdiscovery::mdns::AdvertiseConfig::new(device_id, port);
                cfg.version = ovcore::handshake::PROTOCOL_VERSION;
                match ovdiscovery::mdns::Advertiser::start(cfg) {
                    Ok(adv) => {
                        info!("mDNS: announced as {}", adv.instance_name());
                        let adv = std::sync::Mutex::new(Some(adv));
                        cleanup::register(move || {
                            if let Some(mut a) = adv.lock().ok().and_then(|mut g| g.take()) {
                                a.stop();
                            }
                        });
                        true
                    }
                    Err(e) => {
                        warn!("mDNS announcement unavailable ({e})");
                        detail = Some(format!("mDNS: {e}"));
                        false
                    }
                }
            }
            Err(e) => {
                detail = Some(e);
                false
            }
        };
    if !mdns && opts.pin.is_some() {
        detail = Some("--no-mdns".into());
    }
    info!("LAN sessions on UDP {port}");
    status::emit(&StatusEvent::Lan {
        port: Some(port),
        mdns: advertised,
        detail,
    });
    Some(listener)
}
