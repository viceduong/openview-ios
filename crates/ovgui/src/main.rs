//! OpenView for Windows: a desktop front end for OpenView.
//!
//! It does not link the networking stack. It supervises `ovrelay.exe` +
//! `ovhost.exe` as children (this computer can be controlled) and launches
//! `ovview.exe` (control another computer). See `host.rs` / `env.rs`.
//!
//! Children report their state as JSON status events on stdout
//! ([`ovcommon::status`]); their stderr logs go to `logs\*.log`. The host
//! PIN reaches `ovhost` / `ovview` through the `OPENVIEW_PIN` environment
//! variable, never the command line.
//!
//! Threading: Slint state is touched only on the UI thread. Child readers,
//! timers and the tray window procedure `events::post` plain data; one
//! event-loop callback per burst drains it (`drain`).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![deny(unsafe_op_in_unsafe_fn)]
#![warn(clippy::undocumented_unsafe_blocks)]

mod actions;
mod env;
mod events;
mod host;
mod instance;
mod lan;
mod log;
mod platform;
mod procs;
mod shell;
mod store;
mod util;
mod view;
mod win;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::Receiver;

use slint::{ComponentHandle, ModelRc, VecModel, Weak};

#[allow(
    unused_qualifications,
    missing_debug_implementations,
    unreachable_pub,
    clippy::all,
    clippy::pedantic
)]
mod generated {
    slint::include_modules!();
}
use generated::{App, Connection as UiConnection, LanHost as UiLanHost, MainWindow};

use actions::{copy_id, toggle_pause, wire_callbacks};
use env::Env;
use events::{post, Event, TrayCmd};
use host::{Host, Notice};
use ovcommon::status::StatusEvent;
use procs::Output;
use store::{Connections, Settings, Store};
use view::{apply_theme, host_args, load_settings_into_ui, refresh_connections, sync_ui, toast};

const APP_NAME: &str = "OpenView";
const RUN_VALUE: &str = "OpenView";

struct Viewer {
    seq: u64,
    id: String,
    connected: bool,
    /// The viewer's last `error` status event (shown if it exits early).
    last_error: String,
}

struct State {
    env: Env,
    store: Store,
    settings: Settings,
    conns: Connections,
    host: Host,
    pin: String,
    ui: Weak<MainWindow>,
    conn_model: Rc<VecModel<UiConnection>>,
    /// "Computers on this network" (mDNS), see [`lan`].
    lan: lan::Lan,
    lan_model: Rc<VecModel<UiLanHost>>,
    viewer_seq: u64,
    viewer: Option<Viewer>,
    toast_seq: u64,
    /// Last status line written to the log (log only changes).
    logged_status: String,
}

thread_local! {
    static STATE: RefCell<Option<Rc<RefCell<State>>>> = const { RefCell::new(None) };
    static RX: RefCell<Option<Receiver<Event>>> = const { RefCell::new(None) };
}

fn state() -> Option<Rc<RefCell<State>>> {
    STATE.with(|s| s.borrow().clone())
}

/// Run `f` with the state + window (UI thread only).
fn with<R>(f: impl FnOnce(&mut State, &MainWindow) -> R) -> Option<R> {
    let st = state()?;
    let ui = st.borrow().ui.upgrade()?;
    // A native call (e.g. ShowWindow) can re-enter Slint while we hold the
    // state; skip rather than panic on a double borrow.
    let Ok(mut s) = st.try_borrow_mut() else {
        log::line("state busy; UI callback skipped");
        return None;
    };
    Some(f(&mut s, &ui))
}

fn main() -> Result<(), slint::PlatformError> {
    let env = Env::detect();
    log::init(&env.logs_dir());
    log::line(&format!(
        "OpenView {} starting (dev={}, minimized={})",
        env!("CARGO_PKG_VERSION"),
        env.dev,
        env.minimized
    ));

    let instance_name = if env.dev { "OpenView-dev" } else { "OpenView" };
    let quit = std::env::args().any(|a| a == "--quit");
    let Some(_guard) = instance::acquire(instance_name, quit) else {
        log::line(if quit {
            "asked the running instance to quit"
        } else {
            "another instance is running; asked it to show itself"
        });
        return Ok(());
    };

    // Platform BEFORE the first window and before any thread that touches
    // the environment.
    platform::init(&platform::WindowSpec {
        // Screenshot mode shows the normal title (no test-mode marker).
        title: if env.dev && env.screenshot.is_none() {
            "OpenView (test mode)".into()
        } else {
            APP_NAME.into()
        },
        size: (1000, 700),
        min_size: (860, 600),
    });

    let ui = MainWindow::new()?;
    let hwnd = platform::main_hwnd_raw();
    if hwnd != 0 {
        let h = windows::Win32::Foundation::HWND(hwnd as *mut _);
        if env.minimized {
            shell::hide_window();
        }
        shell::install(h, APP_NAME);
    }

    let rx = events::init(drain);
    RX.with(|r| *r.borrow_mut() = Some(rx));

    env.migrate_legacy_data();
    let store = Store::new(env.data_dir.clone());
    let settings = store.load_settings();
    let conns = store.load_connections();
    let pin_target = format!("{}:host-pin", env.cred_prefix);
    let pin = win::cred_read(&pin_target).unwrap_or_else(|| {
        let p = util::generate_pin(8);
        if let Err(e) = win::cred_write(&pin_target, &p) {
            log::line(&format!("cannot store host PIN: {e}"));
        }
        p
    });

    let conn_model: Rc<VecModel<UiConnection>> = Rc::new(VecModel::default());
    let lan_model: Rc<VecModel<UiLanHost>> = Rc::new(VecModel::default());
    let app = ui.global::<App>();
    app.set_connections(ModelRc::from(conn_model.clone()));
    app.set_lan_hosts(ModelRc::from(lan_model.clone()));
    app.set_version(env!("CARGO_PKG_VERSION").into());
    app.set_dev_mode(env.dev && env.screenshot.is_none());
    app.set_default_relay(
        env.dev_outgoing_relay
            .clone()
            .filter(|_| env.screenshot.is_none())
            .unwrap_or_else(|| "relay.example.com:4569".to_owned())
            .into(),
    );
    if let Some(shot) = env.screenshot {
        app.set_page(match shot.page {
            env::ShotPage::Remote => generated::Page::Remote,
            env::ShotPage::Settings => generated::Page::Settings,
            env::ShotPage::About => generated::Page::About,
        });
    }

    let host = Host::new(env.clone());
    let st = Rc::new(RefCell::new(State {
        env,
        store,
        settings,
        conns,
        host,
        pin,
        ui: ui.as_weak(),
        conn_model,
        lan: lan::Lan::default(),
        lan_model,
        viewer_seq: 0,
        viewer: None,
        toast_seq: 0,
        logged_status: String::new(),
    }));
    STATE.with(|s| *s.borrow_mut() = Some(st.clone()));

    {
        let mut s = st.borrow_mut();
        load_settings_into_ui(&s, &ui);
        apply_theme(&s, &ui);
        let args = host_args(&s);
        let paused = s.settings.paused;
        s.host.boot(args, paused);
        refresh_connections(&s, &ui);
        sync_ui(&mut s, &ui);
    }
    wire_callbacks(&ui);

    // LAN discovery: browse while the window is visible, refresh the list
    // once a second (UI thread; the browser itself runs in the background).
    let lan_timer = slint::Timer::default();
    lan_timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_secs(1),
        || {
            let _ = with(lan::tick);
        },
    );

    ui.show()?;
    if st.borrow().env.minimized && hwnd != 0 {
        shell::hide_window();
    }
    slint::run_event_loop_until_quit()?;
    drop(lan_timer);

    // Quit: stop what we own, drop the tray icon.
    log::line("shutting down");
    st.borrow_mut().host.shutdown();
    shell::remove();
    Ok(())
}

// ------------------------------------------------------------------- events

fn drain() {
    let Some(st) = state() else { return };
    let Some(ui) = st.borrow().ui.upgrade() else {
        return;
    };
    let events: Vec<Event> = RX.with(|r| {
        r.borrow()
            .as_ref()
            .map(|rx| rx.try_iter().collect())
            .unwrap_or_default()
    });
    if events.is_empty() {
        return;
    }
    let Ok(mut s) = st.try_borrow_mut() else {
        // Re-entered from a native call; retry on the next wake.
        for ev in events {
            post(ev);
        }
        return;
    };
    for ev in events {
        match ev {
            Event::Host { gen, out } => {
                if let Some(n) = s.host.on_host(gen, out) {
                    match n {
                        Notice::Connected(fp) => shell::balloon(
                            "Remote session started",
                            &format!("A device ({fp}) is now controlling this computer."),
                        ),
                        Notice::Disconnected => {
                            shell::balloon(
                                "Remote session ended",
                                "The remote device disconnected.",
                            );
                        }
                    }
                }
            }
            Event::Relay { gen, out } => s.host.on_relay(gen, out),
            Event::RelayRetry { gen } => s.host.on_relay_retry(gen),
            Event::HostStopped { gen } => s.host.on_stopped(gen),
            Event::RetryDue { gen } => s.host.on_retry_due(gen),
            Event::ExternalGone => s.host.on_external_gone(),
            Event::TakeOverDone { err } => s.host.on_take_over_done(err),
            Event::Viewer { seq, out } => on_viewer(&mut s, &ui, seq, out),
            Event::Tray(cmd) => on_tray(&mut s, &ui, cmd),
            Event::ThemeChanged => apply_theme(&s, &ui),
            Event::Shown => ui.window().request_redraw(),
            Event::ToastExpired(seq) => {
                if seq == s.toast_seq {
                    ui.global::<App>().set_toast_shown(false);
                }
            }
        }
    }
    sync_ui(&mut s, &ui);
}

fn on_tray(s: &mut State, ui: &MainWindow, cmd: TrayCmd) {
    match cmd {
        // Already shown by the tray procedure itself.
        TrayCmd::Open => {}
        TrayCmd::CopyId => copy_id(s, ui),
        TrayCmd::TogglePause => toggle_pause(s),
        TrayCmd::Quit => {
            log::line("quit requested from the tray");
            shell::quit();
        }
    }
}

fn on_viewer(s: &mut State, ui: &MainWindow, seq: u64, out: Output) {
    let Some(v) = s.viewer.as_mut().filter(|v| v.seq == seq) else {
        return;
    };
    let a = ui.global::<App>();
    match out {
        Output::Status(StatusEvent::PeerConnected { .. }) => {
            if !v.connected {
                v.connected = true;
                let id = util::group_id(&v.id);
                a.set_connecting(false);
                toast(s, ui, &format!("Connected to {id}"));
            }
        }
        Output::Status(StatusEvent::Error { message }) => v.last_error = message,
        Output::Status(_) | Output::Log(_) => {}
        Output::Exited(code) => {
            let v = s.viewer.take().unwrap_or(Viewer {
                seq,
                id: String::new(),
                connected: true,
                last_error: String::new(),
            });
            a.set_connecting(false);
            if !v.connected && code != Some(0) {
                let msg = v.last_error.trim().to_owned();
                let msg = if msg.is_empty() {
                    format!("the viewer exited (code {})", code.unwrap_or(-1))
                } else {
                    msg
                };
                a.set_partner_error(format!("Couldn't connect: {msg}").into());
            }
        }
    }
}
