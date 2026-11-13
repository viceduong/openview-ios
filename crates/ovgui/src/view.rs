//! Rendering [`State`] into the Slint window: settings, the connection list,
//! the host status card and the tray tooltip/menu.

use std::path::PathBuf;
use std::time::Duration;

use slint::{ComponentHandle, SharedString};

use crate::events::{post, Event};
use crate::generated::{App, Connection as UiConnection, HostStatus, MainWindow};
use crate::host::{HostArgs, Phase};
use crate::{log, shell, util, win, State, RUN_VALUE};

pub(crate) fn cred_target(s: &State, id: &str) -> String {
    format!("{}:{id}", s.env.cred_prefix)
}

/// The relay for outgoing connections (and for our host when the bundled
/// relay is off): the setting, else (test mode only) the test relay.
pub(crate) fn outgoing_relay(s: &State) -> Option<String> {
    let r = s.settings.relay_address.trim();
    if r.is_empty() {
        s.env.dev_outgoing_relay.clone()
    } else {
        Some(r.to_owned())
    }
}

pub(crate) fn host_args(s: &State) -> HostArgs {
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

pub(crate) fn exe_command_line() -> String {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("OpenView.exe"));
    format!("\"{}\" --minimized", exe.display())
}

fn start_at_login_enabled(s: &State) -> bool {
    win::reg_get_string(s.env.run_key, RUN_VALUE).is_some()
}

pub(crate) fn load_settings_into_ui(s: &State, ui: &MainWindow) {
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

pub(crate) fn apply_theme(ui: &MainWindow) {
    let dark = win::system_dark_mode();
    ui.global::<App>().set_dark_mode(dark);
    shell::set_dark_title(dark);
}

pub(crate) fn toast(s: &mut State, ui: &MainWindow, msg: &str) {
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

pub(crate) fn refresh_connections(s: &State, ui: &MainWindow) {
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
pub(crate) fn sync_ui(s: &mut State, ui: &MainWindow) {
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
