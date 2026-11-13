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

mod env;
mod events;
mod host;
mod instance;
mod log;
mod platform;
mod procs;
mod shell;
mod store;
mod util;
mod win;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel, Weak};

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
use generated::{App, Connection as UiConnection, HostStatus, MainWindow, Page};

use env::Env;
use events::{post, Event, TrayCmd};
use host::{Host, HostArgs, Notice, Phase};
use ovcommon::status::StatusEvent;
use procs::Output;
use store::{Connections, FpsMode, Settings, Store};

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
        title: if env.dev {
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
    let app = ui.global::<App>();
    app.set_connections(ModelRc::from(conn_model.clone()));
    app.set_version(env!("CARGO_PKG_VERSION").into());
    app.set_dev_mode(env.dev);
    app.set_default_relay(
        env.dev_outgoing_relay
            .clone()
            .unwrap_or_else(|| "relay.example.com:4569".to_owned())
            .into(),
    );

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
        viewer_seq: 0,
        viewer: None,
        toast_seq: 0,
        logged_status: String::new(),
    }));
    STATE.with(|s| *s.borrow_mut() = Some(st.clone()));

    {
        let mut s = st.borrow_mut();
        load_settings_into_ui(&s, &ui);
        apply_theme(&ui);
        let args = host_args(&s);
        let paused = s.settings.paused;
        s.host.boot(args, paused);
        refresh_connections(&s, &ui);
        sync_ui(&mut s, &ui);
    }
    wire_callbacks(&ui);

    ui.show()?;
    if st.borrow().env.minimized && hwnd != 0 {
        shell::hide_window();
    }
    slint::run_event_loop_until_quit()?;

    // Quit: stop what we own, drop the tray icon.
    log::line("shutting down");
    st.borrow_mut().host.shutdown();
    shell::remove();
    Ok(())
}

// ------------------------------------------------------------------ helpers

fn cred_target(s: &State, id: &str) -> String {
    format!("{}:{id}", s.env.cred_prefix)
}

/// The relay for outgoing connections (and for our host when the bundled
/// relay is off): the setting, else (test mode only) the test relay.
fn outgoing_relay(s: &State) -> Option<String> {
    let r = s.settings.relay_address.trim();
    if r.is_empty() {
        s.env.dev_outgoing_relay.clone()
    } else {
        Some(r.to_owned())
    }
}

fn host_args(s: &State) -> HostArgs {
    HostArgs {
        pin: s.pin.clone(),
        relay: if s.settings.run_relay {
            Some(s.env.local_relay.clone())
        } else {
            outgoing_relay(s)
        },
        run_relay: s.settings.run_relay,
        send_audio: s.settings.send_audio,
        play_mic: s.settings.play_mic,
        hide_wallpaper: s.settings.hide_wallpaper,
        fps: s.settings.fps,
        direct: s.settings.direct,
        upnp: s.settings.upnp,
    }
}

fn exe_command_line() -> String {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("OpenView.exe"));
    format!("\"{}\" --minimized", exe.display())
}

fn start_at_login_enabled(s: &State) -> bool {
    win::reg_get_string(s.env.run_key, RUN_VALUE).is_some()
}

fn load_settings_into_ui(s: &State, ui: &MainWindow) {
    let a = ui.global::<App>();
    let st = &s.settings;
    a.set_send_audio(st.send_audio);
    a.set_play_mic(st.play_mic);
    a.set_hide_wallpaper(st.hide_wallpaper);
    a.set_fps_mode(st.fps.index());
    a.set_direct(st.direct);
    a.set_upnp(st.upnp);
    a.set_relay_address(st.relay_address.clone().into());
    a.set_run_relay(st.run_relay);
    a.set_start_at_login(start_at_login_enabled(s));
}

fn apply_theme(ui: &MainWindow) {
    let dark = win::system_dark_mode();
    ui.global::<App>().set_dark_mode(dark);
    shell::set_dark_title(dark);
}

fn toast(s: &mut State, ui: &MainWindow, msg: &str) {
    s.toast_seq += 1;
    let seq = s.toast_seq;
    let a = ui.global::<App>();
    a.set_toast_text(msg.into());
    a.set_toast_shown(true);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(2200));
        post(Event::ToastExpired(seq));
    });
}

fn refresh_connections(s: &State, ui: &MainWindow) {
    let a = ui.global::<App>();
    let filter = a.get_list_filter();
    let now = util::now_secs();
    let rows: Vec<UiConnection> = s
        .conns
        .items
        .iter()
        .filter(|c| filter == 0 || c.favorite)
        .map(|c| UiConnection {
            id: util::group_id(&c.id).into(),
            raw_id: c.id.clone().into(),
            name: c.name.clone().into(),
            subtitle: format!("Last connected {}", util::ago(c.last_connected, now)).into(),
            favorite: c.favorite,
            has_pin: win::cred_read(&cred_target(s, &c.id)).is_some(),
        })
        .collect();
    // Mutate the long-lived model in place (never swap it under the `for`).
    s.conn_model.set_vec(rows);
    a.set_recent_count(i32::try_from(s.conns.items.len()).unwrap_or(0));
    a.set_favorite_count(
        i32::try_from(s.conns.items.iter().filter(|c| c.favorite).count()).unwrap_or(0),
    );
}

fn direct_detail(s: &State) -> String {
    let relay = s.host.relay().unwrap_or("?");
    if s.env.dev {
        return format!("Test mode · relay {relay} · input is simulated");
    }
    let mut parts = vec![format!("Relay {relay}")];
    if s.host.direct.active {
        parts.push("direct path active".into());
    } else if !s.settings.direct || s.host.direct.disabled {
        parts.push("relay only".into());
    } else if let Some(p) = s.host.direct.port {
        parts.push(format!("direct UDP {p}"));
        match s.host.direct.upnp {
            Some(true) => parts.push("router port open (UPnP)".into()),
            Some(false) if s.settings.upnp => parts.push("UPnP unavailable".into()),
            _ => {}
        }
    }
    parts.join(" · ")
}

/// Push supervisor state into the UI + tray. Cheap; called after every
/// drained batch and user action.
fn sync_ui(s: &mut State, ui: &MainWindow) {
    if let Some(id) = s.host.device_id.clone() {
        if s.settings.last_device_id != id {
            s.settings.last_device_id = id;
            s.store.save_settings(&s.settings);
        }
    }
    let a = ui.global::<App>();
    let id = s.settings.last_device_id.clone();
    a.set_my_id(util::group_id(&id).into());
    a.set_pin(s.pin.clone().into());
    let (status, text, detail, busy) = match &s.host.phase {
        Phase::Starting => (
            HostStatus::Starting,
            "Starting…".to_owned(),
            "Launching the relay and the host.".to_owned(),
            false,
        ),
        Phase::Stopping => (
            HostStatus::Starting,
            "Restarting…".to_owned(),
            "Applying changes.".to_owned(),
            true,
        ),
        Phase::Ready => (
            HostStatus::Ready,
            "Ready — waiting for a connection".to_owned(),
            direct_detail(s),
            false,
        ),
        Phase::Connected { peer, since } => (
            HostStatus::Connected,
            format!("Connected: {peer} since {since}"),
            if s.host.direct.active {
                "A remote device is controlling this computer (direct connection).".to_owned()
            } else {
                "A remote device is controlling this computer.".to_owned()
            },
            false,
        ),
        Phase::Paused => (
            HostStatus::Paused,
            "Remote access paused".to_owned(),
            "Nobody can connect until you resume.".to_owned(),
            false,
        ),
        Phase::Error(e) => {
            let mut d = e.clone();
            if let Some(n) = s.host.retry_in() {
                d.push_str(&format!(" Trying again in {n} s."));
            }
            (HostStatus::Error, "Not ready".to_owned(), d, false)
        }
        Phase::External => (
            HostStatus::External,
            "Another host is running".to_owned(),
            "An ovhost started outside OpenView is already running on this PC. Stop it and \
             take over to manage remote access here."
                .to_owned(),
            false,
        ),
    };
    let line = format!("status: {text} | {detail} | id {id}");
    if line != s.logged_status {
        log::line(&line);
        s.logged_status = line;
    }
    a.set_host_status(status);
    a.set_status_text(text.clone().into());
    a.set_status_detail(detail.into());
    a.set_busy(busy);
    a.set_session_active(s.host.session_active());
    let relay = outgoing_relay(s);
    a.set_relay_missing(relay.is_none());
    a.set_outgoing_relay(relay.unwrap_or_default().into());
    a.set_settings_note(if s.host.restart_pending() {
        SharedString::from("Changes will apply when the current session ends.")
    } else {
        SharedString::new()
    });
    let tip = if id.is_empty() {
        format!("OpenView — {text}")
    } else {
        format!("OpenView — {text}\nID {}", util::group_id(&id))
    };
    shell::set_tooltip(&tip);
    shell::set_menu_state(s.host.phase == Phase::Paused, !id.is_empty());
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
            Event::ThemeChanged => apply_theme(&ui),
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

// ---------------------------------------------------------------- actions

fn copy_id(s: &mut State, ui: &MainWindow) {
    let id = util::group_id(&s.settings.last_device_id);
    if !id.is_empty() && win::set_clipboard(&id) {
        toast(s, ui, "ID copied to the clipboard");
    }
}

fn toggle_pause(s: &mut State) {
    if s.host.phase == Phase::External {
        return;
    }
    s.settings.paused = s.host.phase != Phase::Paused;
    s.store.save_settings(&s.settings);
    if s.settings.paused {
        s.host.pause();
    } else {
        s.host.resume();
    }
}

fn connect(s: &mut State, ui: &MainWindow, preset_relay: Option<String>) {
    let a = ui.global::<App>();
    a.set_partner_error(SharedString::new());
    let id = util::digits(&a.get_partner_id());
    if id.len() < 6 {
        a.set_partner_error(
            "Enter the partner's ID (the number shown on the other device).".into(),
        );
        return;
    }
    let mut pin = a.get_partner_pin().to_string();
    if pin.is_empty() {
        if let Some(saved) = win::cred_read(&cred_target(s, &id)) {
            pin = saved;
        }
    }
    if pin.is_empty() {
        a.set_partner_error("Enter the partner's PIN.".into());
        return;
    }
    if let Some(e) = s.env.missing_binary(env::VIEWER_EXE) {
        a.set_partner_error(e.into());
        return;
    }
    let Some(relay) = preset_relay
        .filter(|r| !r.is_empty())
        .or_else(|| outgoing_relay(s))
    else {
        a.set_partner_error(
            "No relay configured. Enter the relay address in Settings first.".into(),
        );
        a.set_page(Page::Settings);
        return;
    };
    s.viewer_seq += 1;
    let seq = s.viewer_seq;
    // The PIN goes through the environment, never the command line.
    let args = vec![
        "--connect".to_owned(),
        id.clone(),
        "--relay".to_owned(),
        relay.clone(),
    ];
    let logs = s.env.logs_dir();
    let log = logs.join(format!("ovview-{id}.log"));
    let status_file = logs.join(format!("ovview-{id}.status.jsonl"));
    log::line(&format!("launching viewer for {id} via {relay}"));
    let spec = procs::SpawnSpec {
        exe: &s.env.exe(env::VIEWER_EXE),
        args: &args,
        envs: &[(host::PIN_ENV, pin.as_str())],
        cwd: &s.env.bin_dir,
        in_job: false,
        log_path: Some(&log),
        status_stdout: true,
    };
    match procs::spawn_detached(&spec, &status_file, move |out| {
        post(Event::Viewer { seq, out })
    }) {
        Ok(_p) => {
            s.viewer = Some(Viewer {
                seq,
                id: id.clone(),
                connected: false,
                last_error: String::new(),
            });
            a.set_connecting(true);
            let target = cred_target(s, &id);
            if a.get_remember_pin() {
                if let Err(e) = win::cred_write(&target, &pin) {
                    log::line(&format!("cannot save PIN: {e}"));
                }
            } else {
                win::cred_delete(&target);
            }
            s.conns.touch(&id, &relay, util::now_secs());
            s.store.save_connections(&s.conns);
            refresh_connections(s, ui);
        }
        Err(e) => a.set_partner_error(e.into()),
    }
}

fn select_connection(s: &State, ui: &MainWindow, id: &str) {
    let a = ui.global::<App>();
    a.set_partner_id(util::group_id(id).into());
    a.set_partner_pin(
        win::cred_read(&cred_target(s, id))
            .unwrap_or_default()
            .into(),
    );
    a.set_partner_error(SharedString::new());
}

fn on_setting(s: &mut State, ui: &MainWindow, key: &str) {
    let a = ui.global::<App>();
    let before = s.settings.clone();
    s.settings.send_audio = a.get_send_audio();
    s.settings.play_mic = a.get_play_mic();
    s.settings.hide_wallpaper = a.get_hide_wallpaper();
    s.settings.fps = FpsMode::from_index(a.get_fps_mode());
    s.settings.direct = a.get_direct();
    s.settings.upnp = a.get_upnp();
    s.settings.relay_address = a.get_relay_address().trim().to_owned();
    s.settings.run_relay = a.get_run_relay();
    match key {
        "start-at-login" => {
            if a.get_start_at_login() {
                match win::reg_set_string(s.env.run_key, RUN_VALUE, &exe_command_line()) {
                    Ok(()) => toast(s, ui, "OpenView will start when you sign in"),
                    Err(e) => {
                        a.set_start_at_login(false);
                        toast(s, ui, &e);
                    }
                }
            } else {
                win::reg_delete_value(s.env.run_key, RUN_VALUE);
            }
        }
        "relay" => {
            let r = &s.settings.relay_address;
            if !r.is_empty() && !valid_host_port(r) {
                toast(s, ui, "Relay must look like host:port");
                s.settings.relay_address = before.relay_address.clone();
                a.set_relay_address(before.relay_address.clone().into());
            }
        }
        _ => {}
    }
    if s.settings != before {
        s.store.save_settings(&s.settings);
    }
    let args = host_args(s);
    s.host.apply(args);
}

fn valid_host_port(s: &str) -> bool {
    match s.rsplit_once(':') {
        Some((h, p)) => !h.is_empty() && p.parse::<u16>().is_ok_and(|p| p > 0),
        None => false,
    }
}

fn save_pin(s: &mut State, ui: &MainWindow) {
    let a = ui.global::<App>();
    let p = a.get_new_pin().to_string();
    let c = a.get_confirm_pin().to_string();
    if let Err(e) = util::check_pin(&p) {
        a.set_pin_dialog_error(e.into());
        return;
    }
    if p != c {
        a.set_pin_dialog_error("The two PINs don't match.".into());
        return;
    }
    let target = format!("{}:host-pin", s.env.cred_prefix);
    if let Err(e) = win::cred_write(&target, &p) {
        a.set_pin_dialog_error(format!("Could not save: {e}").into());
        return;
    }
    s.pin = p;
    a.set_pin_dialog_open(false);
    a.set_new_pin(SharedString::new());
    a.set_confirm_pin(SharedString::new());
    let args = host_args(s);
    if s.host.session_active() {
        // A PIN change must not wait: drop the session now.
        s.host.apply_args_silently(args);
        s.host.restart();
    } else {
        s.host.apply(args);
    }
    toast(s, ui, "PIN changed — the host restarted");
}

// ---------------------------------------------------------------- wiring

fn wire_callbacks(ui: &MainWindow) {
    let a = ui.global::<App>();

    a.on_format_id(|t| util::group_id(&t).into());

    macro_rules! on {
        ($setter:ident, |$s:ident, $ui:ident| $body:expr) => {
            a.$setter(move || {
                let _ = with(|$s, $ui| {
                    $body;
                    sync_ui($s, $ui);
                });
            });
        };
        ($setter:ident, |$s:ident, $ui:ident, $arg:ident| $body:expr) => {
            a.$setter(move |$arg| {
                let _ = with(|$s, $ui| {
                    $body;
                    sync_ui($s, $ui);
                });
            });
        };
    }

    on!(on_copy_id, |s, ui| copy_id(s, ui));
    on!(on_copy_pin, |s, ui| {
        let pin = s.pin.clone();
        if win::set_clipboard(&pin) {
            toast(s, ui, "PIN copied to the clipboard");
        }
    });
    on!(on_open_pin_dialog, |s, ui| {
        let _ = &s;
        let a = ui.global::<App>();
        a.set_new_pin(SharedString::new());
        a.set_confirm_pin(SharedString::new());
        a.set_pin_dialog_error(SharedString::new());
        a.set_pin_dialog_info(SharedString::new());
        a.set_pin_dialog_open(true);
    });
    on!(on_generate_pin, |s, ui| {
        let _ = &s;
        let p = util::generate_pin(8);
        let a = ui.global::<App>();
        a.set_new_pin(p.clone().into());
        a.set_confirm_pin(p.clone().into());
        a.set_pin_dialog_error(SharedString::new());
        a.set_pin_dialog_info(format!("New PIN: {p}  — click Save PIN to use it.").into());
    });
    on!(on_save_pin, |s, ui| save_pin(s, ui));
    on!(on_end_session, |s, ui| {
        let _ = &ui;
        s.host.restart();
    });
    on!(on_toggle_pause, |s, ui| {
        let _ = &ui;
        toggle_pause(s);
    });
    on!(on_take_over, |s, ui| {
        let _ = &ui;
        s.host.take_over();
    });
    on!(on_connect, |s, ui| connect(s, ui, None));
    on!(on_connection_connect, |s, ui, id| {
        let id = id.to_string();
        select_connection(s, ui, &id);
        let relay = s.conns.get(&id).map(|c| c.relay.clone());
        connect(s, ui, relay);
    });
    on!(on_connection_select, |s, ui, id| select_connection(
        s, ui, &id
    ));
    on!(on_connection_toggle_favorite, |s, ui, id| {
        s.conns.toggle_favorite(&id);
        s.store.save_connections(&s.conns);
        refresh_connections(s, ui);
    });
    on!(on_connection_remove, |s, ui, id| {
        s.conns.remove(&id);
        win::cred_delete(&cred_target(s, &id));
        s.store.save_connections(&s.conns);
        refresh_connections(s, ui);
    });
    on!(on_list_filter_changed, |s, ui| refresh_connections(s, ui));
    on!(on_setting_changed, |s, ui, key| on_setting(s, ui, &key));
    on!(on_open_logs, |s, ui| {
        let _ = &ui;
        let dir = s.env.logs_dir();
        let _ = std::fs::create_dir_all(&dir);
        win::shell_open(&dir.display().to_string());
    });

    // Navigation needs no Rust; keep Page referenced for the generated API.
    let _ = Page::Remote;
}
