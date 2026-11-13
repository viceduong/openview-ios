//! "Computers on this network": OpenView hosts found by mDNS
//! ([`ovdiscovery::mdns::Browser`]), shown on the Remote page and dialed
//! directly over the LAN (`ovview --id <id> --lan-addr <addrs>`), no relay.
//!
//! The browser runs only while the window is visible; a 1 s UI-thread timer
//! ([`tick`]) starts / stops it and copies its snapshot into the in-place
//! model.

use std::net::SocketAddr;

use ovdiscovery::mdns::{Browser, DiscoveredHost};
use slint::{ComponentHandle, Model, SharedString};

use crate::env::DEMO_LAN_PREFIX;
use crate::generated::{App, LanHost as UiLanHost, MainWindow};
use crate::{log, shell, util, State};

/// LAN discovery state kept in [`State`].
#[derive(Default)]
pub(crate) struct Lan {
    browser: Option<Browser>,
    /// The hosts on screen (our own host excluded).
    pub(crate) hosts: Vec<DiscoveredHost>,
    /// The browser failed to start once (logged; retried on the next show).
    failed: bool,
}

impl Lan {
    /// The addresses of `raw_id` if it is on this network right now.
    pub(crate) fn addrs_of(&self, raw_id: &str) -> Option<Vec<SocketAddr>> {
        let id: u64 = raw_id.parse().ok()?;
        self.hosts
            .iter()
            .find(|h| h.device_id == id && !h.addrs.is_empty())
            .map(|h| h.addrs.clone())
    }

    /// Display name of `raw_id` on this network, if listed.
    pub(crate) fn name_of(&self, raw_id: &str) -> Option<String> {
        let id: u64 = raw_id.parse().ok()?;
        self.hosts
            .iter()
            .find(|h| h.device_id == id)
            .map(|h| h.name.clone())
    }

    fn stop(&mut self) {
        if let Some(mut b) = self.browser.take() {
            b.stop();
        }
        self.hosts.clear();
    }
}

/// Once a second: run the browser while the window is visible and refresh
/// the list.
pub(crate) fn tick(s: &mut State, ui: &MainWindow) {
    let visible = shell::is_window_visible();
    if !visible {
        if s.lan.browser.is_some() {
            s.lan.stop();
            set_model(s, ui);
        }
        s.lan.failed = false;
        return;
    }
    if s.lan.browser.is_none() && !s.lan.failed {
        match Browser::start() {
            Ok(b) => s.lan.browser = Some(b),
            Err(e) => {
                log::line(&format!("LAN discovery unavailable: {e}"));
                s.lan.failed = true;
            }
        }
    }
    ui.global::<App>().set_lan_browsing(s.lan.browser.is_some());
    let Some(b) = &s.lan.browser else { return };
    let own: Option<u64> = s.settings.last_device_id.parse().ok();
    // Screenshot mode lists only the demo host (never a real machine).
    let shot = s.env.screenshot.is_some();
    let hosts: Vec<DiscoveredHost> = b
        .hosts()
        .into_iter()
        .filter(|h| Some(h.device_id) != own)
        .filter(|h| !shot || h.name.starts_with(DEMO_LAN_PREFIX))
        .collect();
    let same = hosts.len() == s.lan.hosts.len()
        && hosts.iter().zip(&s.lan.hosts).all(|(a, b)| {
            a.device_id == b.device_id
                && a.name == b.name
                && a.addrs == b.addrs
                && a.version == b.version
        });
    if !same {
        s.lan.hosts = hosts;
        set_model(s, ui);
    }
}

/// Mirror `s.lan.hosts` into the in-place model.
fn set_model(s: &State, ui: &MainWindow) {
    let rows: Vec<UiLanHost> = s.lan.hosts.iter().map(row).collect();
    let unchanged = s.lan_model.row_count() == rows.len()
        && rows
            .iter()
            .enumerate()
            .all(|(i, r)| s.lan_model.row_data(i).as_ref() == Some(r));
    if !unchanged {
        s.lan_model.set_vec(rows);
    }
    let _ = ui;
}

fn row(h: &DiscoveredHost) -> UiLanHost {
    let raw = h.device_id.to_string();
    UiLanHost {
        id: util::group_id(&raw).into(),
        raw_id: raw.into(),
        name: h.name.clone().into(),
        hint: if h.is_compatible() {
            SharedString::new()
        } else {
            "different OpenView version".into()
        },
    }
}

/// `ovview` arguments for a LAN dial of `id` at `addrs`.
pub(crate) fn viewer_args(id: &str, addrs: &[SocketAddr]) -> Vec<String> {
    let list = addrs
        .iter()
        .map(SocketAddr::to_string)
        .collect::<Vec<_>>()
        .join(",");
    vec![
        "--id".to_owned(),
        id.to_owned(),
        "--lan-addr".to_owned(),
        list,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewer_args_list_every_address() {
        let a: SocketAddr = "192.168.1.5:4571".parse().unwrap();
        let b: SocketAddr = "10.0.0.2:4571".parse().unwrap();
        assert_eq!(
            viewer_args("42", &[a, b]),
            vec!["--id", "42", "--lan-addr", "192.168.1.5:4571,10.0.0.2:4571"]
        );
    }
}
