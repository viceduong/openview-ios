//! User actions (buttons, settings changes, connecting) and the wiring of
//! the Slint callbacks to them.

use ovcommon::relay_addr::{RelayAddr, RELAY_HINT};
use slint::{ComponentHandle, SharedString};

use crate::events::{post, Event};
use crate::generated::{App, MainWindow, Page};
use crate::host::Phase;
use crate::store::FpsMode;
use crate::view::{
    cred_target, exe_command_line, host_args, outgoing_relay, refresh_connections, sync_ui, toast,
};
use crate::{env, host, lan, log, procs, util, win, with, State, Viewer, RUN_VALUE};

pub(crate) fn copy_id(s: &mut State, ui: &MainWindow) {
    let id = util::group_id(&s.settings.last_device_id);
    if !id.is_empty() && win::set_clipboard(&id) {
        toast(s, ui, "ID copied to the clipboard");
    }
}

pub(crate) fn toggle_pause(s: &mut State) {
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
    let relay = preset_relay
        .filter(|r| !r.is_empty())
        .or_else(|| outgoing_relay(s));
    // On this network right now: dial it directly (no relay needed), with
    // the relay as the fallback when one is configured.
    let lan_addrs = s.lan.addrs_of(&id);
    let (args, relay) = match (&lan_addrs, relay) {
        (Some(addrs), relay) => {
            let mut args = lan::viewer_args(&id, addrs);
            if let Some(r) = &relay {
                args.extend([
                    "--relay-fallback".to_owned(),
                    "--relay".to_owned(),
                    r.clone(),
                ]);
            }
            log::line(&format!(
                "launching viewer for {id} on the LAN ({})",
                args[3]
            ));
            (args, relay.unwrap_or_default())
        }
        (None, Some(relay)) => {
            log::line(&format!("launching viewer for {id} via {relay}"));
            let args = vec![
                "--connect".to_owned(),
                id.clone(),
                "--relay".to_owned(),
                relay.clone(),
            ];
            (args, relay)
        }
        (None, None) => {
            a.set_partner_error(
                "No relay configured. Enter the relay address in Settings first.".into(),
            );
            a.set_page(Page::Settings);
            return;
        }
    };
    s.viewer_seq += 1;
    let seq = s.viewer_seq;
    // The PIN goes through the environment, never the command line.
    let logs = s.env.logs_dir();
    let log = logs.join(format!("ovview-{id}.log"));
    let status_file = logs.join(format!("ovview-{id}.status.jsonl"));
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
            if !r.is_empty() && !RelayAddr::is_valid(r) {
                toast(s, ui, &format!("Relay must be an {RELAY_HINT}"));
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

pub(crate) fn wire_callbacks(ui: &MainWindow) {
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
    on!(on_lan_select, |s, ui, id| select_connection(s, ui, &id));
    on!(on_lan_connect, |s, ui, id| {
        let id = id.to_string();
        select_connection(s, ui, &id);
        if let Some(name) = s.lan.name_of(&id) {
            log::line(&format!("LAN connect to {name} ({id})"));
        }
        connect(s, ui, None);
    });
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
