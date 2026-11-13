//! Host-side supervision: runs `ovrelay` + `ovhost` as children, turns the
//! host's status events (JSON lines on its stdout, see
//! [`ovcommon::status`]) into a [`Phase`], restarts with backoff, and
//! offers to replace a host started outside OpenView.
//!
//! Lives on the UI thread; child output arrives as [`Event`]s. The host's
//! stderr (human-readable logs) only goes to `logs\ovhost.log`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ovcommon::status::{DirectState, StatusEvent};

use crate::env::{Env, HOST_EXE, RELAY_EXE};
use crate::events::{post, Event};
use crate::procs::{self, Output, Proc, SpawnSpec};
use crate::store::FpsMode;

/// Environment variable that hands the PIN to `ovhost` (never argv).
pub const PIN_ENV: &str = "OPENVIEW_PIN";

/// Everything that shapes the ovhost command line (and its environment).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostArgs {
    pub pin: String,
    /// The relay the host parks on; `None` = none configured yet.
    pub relay: Option<String>,
    /// Run the bundled relay on this PC (and park the host on it).
    pub run_relay: bool,
    pub send_audio: bool,
    pub play_mic: bool,
    pub hide_wallpaper: bool,
    pub fps: FpsMode,
    pub direct: bool,
    pub upnp: bool,
}

impl HostArgs {
    /// The ovhost argv. The PIN is NOT here (see [`Self::envs`]); `--screen`
    /// stays LAST: it takes an optional WxH.
    pub fn to_argv(&self, env: &Env) -> Vec<String> {
        let mut a: Vec<String> = vec![
            "--unattended".into(),
            "--service".into(),
            "--relay".into(),
            self.relay.clone().unwrap_or_default(),
            "--data-dir".into(),
            env.data_dir.display().to_string(),
        ];
        let (fps, max) = match self.fps {
            FpsMode::Auto => (30, None),
            FpsMode::Fps30 => (30, Some(30)),
            FpsMode::Fps60 => (60, Some(60)),
        };
        a.extend(["--fps".into(), fps.to_string()]);
        if let Some(m) = max {
            a.extend(["--max-fps".into(), m.to_string()]);
        }
        if self.play_mic {
            a.push("--audio".into());
        }
        if self.send_audio {
            a.push("--system-audio".into());
        }
        if !self.hide_wallpaper {
            a.push("--keep-wallpaper".into());
        }
        if env.dev {
            // Isolated test mode: no real input, no desktop changes, no
            // router/UDP side effects.
            a.extend([
                "--dry-input".into(),
                "--fx-dry-run".into(),
                "--no-upnp".into(),
                "--no-direct".into(),
            ]);
        } else if !self.direct {
            a.push("--no-direct".into());
        } else if !self.upnp {
            a.push("--no-upnp".into());
        }
        a.push("--screen".into());
        a
    }

    /// Environment for ovhost: the PIN.
    pub fn envs(&self) -> [(&'static str, &str); 1] {
        [(PIN_ENV, self.pin.as_str())]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Paused,
    Starting,
    Ready,
    Connected {
        peer: String,
        since: String,
    },
    Error(String),
    /// An ovhost we did not start is running from our install folder.
    External,
    /// A stop is in flight (restart / pause / take-over).
    Stopping,
}

/// What happened, for tray balloons.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    Connected(String),
    Disconnected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum After {
    Start,
    Pause,
}

#[derive(Debug, Default, Clone)]
pub struct DirectInfo {
    pub port: Option<u16>,
    pub upnp: Option<bool>,
    pub disabled: bool,
    /// The current session runs on a direct UDP path.
    pub active: bool,
}

/// Shown when no relay is configured and the bundled one is off.
pub const NO_RELAY: &str = "No relay configured. Enter a relay address in Settings, \
                            or turn on \u{201c}Run a relay on this PC\u{201d}.";

pub struct Host {
    env: Env,
    args: Option<HostArgs>,
    gen: u64,
    host: Option<Arc<Proc>>,
    relay: Option<Arc<Proc>>,
    relay_gen: u64,
    relay_external: bool,
    external: Vec<u32>,
    pub phase: Phase,
    pub device_id: Option<String>,
    pub direct: DirectInfo,
    failures: u32,
    retry_in: Option<u64>,
    last_error: String,
    after_stop: After,
    pending_restart: bool,
}

impl Host {
    pub fn new(env: Env) -> Self {
        Self {
            env,
            args: None,
            gen: 0,
            host: None,
            relay: None,
            relay_gen: 0,
            relay_external: false,
            external: Vec::new(),
            phase: Phase::Starting,
            device_id: None,
            direct: DirectInfo::default(),
            failures: 0,
            retry_in: None,
            last_error: String::new(),
            after_stop: After::Start,
            pending_restart: false,
        }
    }

    pub fn retry_in(&self) -> Option<u64> {
        self.retry_in
    }

    pub fn session_active(&self) -> bool {
        matches!(self.phase, Phase::Connected { .. })
    }

    pub fn restart_pending(&self) -> bool {
        self.pending_restart
    }

    /// The relay the host parks on (for the status line).
    pub fn relay(&self) -> Option<&str> {
        self.args.as_ref().and_then(|a| a.relay.as_deref())
    }

    /// First call at startup. In normal mode an ovhost already running from
    /// our install folder (started by hand or by an old setup) is reported
    /// as External instead of starting a duplicate.
    pub fn boot(&mut self, args: HostArgs, paused: bool) {
        self.args = Some(args);
        let pids = self.external_hosts();
        if !pids.is_empty() {
            self.adopt_external(pids);
            return;
        }
        if paused {
            self.phase = Phase::Paused;
        } else {
            self.start();
        }
    }

    fn adopt_external(&mut self, pids: Vec<u32>) {
        crate::log::line(&format!("external ovhost running (pids {pids:?})"));
        self.external.clone_from(&pids);
        self.phase = Phase::External;
        // Watch them: once they are gone, start our own.
        std::thread::spawn(move || {
            for pid in pids {
                if let Some(p) = Proc::open(pid) {
                    while !p.wait(Duration::from_secs(3600)) {}
                }
            }
            post(Event::ExternalGone);
        });
    }

    /// Hosts started outside OpenView from our ovhost binary. Test mode
    /// never looks (a live host must never be adopted or stopped by a test).
    fn external_hosts(&self) -> Vec<u32> {
        if self.env.dev {
            return Vec::new();
        }
        procs::find_by_exe(&self.env.exe(HOST_EXE))
    }

    fn logs(&self) -> PathBuf {
        self.env.logs_dir()
    }

    /// Start relay (if needed) and host.
    pub fn start(&mut self) {
        if !self.external.is_empty() || self.host.is_some() {
            return;
        }
        let Some(args) = self.args.clone() else {
            return;
        };
        let pids = self.external_hosts();
        if !pids.is_empty() {
            self.adopt_external(pids);
            return;
        }
        self.retry_in = None;
        if args.relay.is_none() {
            self.phase = Phase::Error(NO_RELAY.to_owned());
            return;
        }
        if let Some(e) = self.env.missing_binary(HOST_EXE) {
            self.phase = Phase::Error(e);
            return;
        }
        if args.run_relay {
            self.ensure_relay();
        }
        self.gen += 1;
        let gen = self.gen;
        let argv = args.to_argv(&self.env);
        crate::log::line(&format!("starting ovhost (gen {gen}): {}", argv.join(" ")));
        let log = self.logs().join("ovhost.log");
        let spec = SpawnSpec {
            exe: &self.env.exe(HOST_EXE),
            args: &argv,
            envs: &args.envs(),
            cwd: &self.env.bin_dir,
            in_job: true,
            log_path: Some(&log),
            status_stdout: true,
        };
        match procs::spawn(&spec, move |out| post(Event::Host { gen, out })) {
            Ok(p) => {
                self.host = Some(Arc::new(p));
                self.phase = Phase::Starting;
                self.last_error.clear();
                self.direct = DirectInfo::default();
            }
            Err(e) => {
                self.phase = Phase::Error(e);
                self.schedule_retry();
            }
        }
    }

    fn ensure_relay(&mut self) {
        if self.relay.as_ref().is_some_and(|r| r.alive()) {
            return;
        }
        // Someone (another relay) already serves the port: use it instead
        // of failing to bind a duplicate. Re-checked on every start, so we
        // take over once that relay goes away.
        if std::net::TcpListener::bind(&self.env.relay_bind).is_err() {
            if !self.relay_external {
                crate::log::line(&format!(
                    "relay port {} busy; using the existing relay",
                    self.env.relay_bind
                ));
            }
            self.relay_external = true;
            return;
        }
        self.relay_external = false;
        if let Some(e) = self.env.missing_binary(RELAY_EXE) {
            crate::log::line(&e);
            return;
        }
        self.relay_gen += 1;
        let gen = self.relay_gen;
        let argv = vec!["--bind".to_owned(), self.env.relay_bind.clone()];
        let log = self.logs().join("ovrelay.log");
        let spec = SpawnSpec {
            exe: &self.env.exe(RELAY_EXE),
            args: &argv,
            envs: &[],
            cwd: &self.env.bin_dir,
            in_job: true,
            log_path: Some(&log),
            status_stdout: false,
        };
        match procs::spawn(&spec, move |out| post(Event::Relay { gen, out })) {
            Ok(p) => self.relay = Some(Arc::new(p)),
            Err(e) => crate::log::line(&format!("relay start failed: {e}")),
        }
    }

    fn schedule_retry(&mut self) {
        self.failures = self.failures.saturating_add(1);
        let secs = (2u64 << self.failures.min(4)).min(30);
        self.retry_in = Some(secs);
        let gen = self.gen;
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(secs));
            post(Event::RetryDue { gen });
        });
    }

    pub fn on_retry_due(&mut self, gen: u64) {
        if gen == self.gen && self.host.is_none() && matches!(self.phase, Phase::Error(_)) {
            self.start();
        }
    }

    /// Stop the host on a worker thread, then continue with `after`.
    fn stop_then(&mut self, after: After) {
        self.after_stop = after;
        self.pending_restart = false;
        self.gen += 1; // ignore the old child's exit event
        let gen = self.gen;
        // The relay keeps a stopped host's park slot until its ~30 s timeout
        // ("session already has a responder"), so a restarted host would be
        // unreachable meanwhile. Restart our own relay with the host.
        let relay = self.relay.take();
        if relay.is_some() {
            self.relay_gen += 1;
        }
        let host = self.host.take();
        if host.is_none() && relay.is_none() {
            self.finish_stop();
            return;
        }
        self.phase = Phase::Stopping;
        std::thread::spawn(move || {
            if let Some(p) = host {
                p.stop(Duration::from_secs(5));
            }
            if let Some(r) = relay {
                r.stop(Duration::from_secs(2));
            }
            post(Event::HostStopped { gen });
        });
    }

    pub fn on_stopped(&mut self, gen: u64) {
        if gen == self.gen {
            self.finish_stop();
        }
    }

    fn finish_stop(&mut self) {
        match self.after_stop {
            After::Start => {
                self.phase = Phase::Starting;
                self.start();
            }
            After::Pause => self.phase = Phase::Paused,
        }
    }

    /// Apply new args: restart now, or after the current session ends.
    pub fn apply(&mut self, args: HostArgs) {
        if self.args.as_ref() == Some(&args) {
            return;
        }
        self.args = Some(args);
        match self.phase {
            Phase::Paused | Phase::External => {}
            Phase::Connected { .. } => self.pending_restart = true,
            _ => self.restart(),
        }
    }

    /// Replace the args without restarting (the caller restarts).
    pub fn apply_args_silently(&mut self, args: HostArgs) {
        self.args = Some(args);
    }

    /// End the session / apply settings: restart the host.
    pub fn restart(&mut self) {
        if self.phase == Phase::External {
            return;
        }
        self.failures = 0;
        self.stop_then(After::Start);
    }

    pub fn pause(&mut self) {
        if self.phase != Phase::External {
            self.stop_then(After::Pause);
        }
    }

    pub fn resume(&mut self) {
        if self.phase == Phase::Paused {
            self.failures = 0;
            self.phase = Phase::Starting;
            self.start();
        }
    }

    /// Stop the external host and start ours (with our PIN). Runs the stop
    /// on a worker; the result comes back in `TakeOverDone`.
    pub fn take_over(&mut self) {
        if self.external.is_empty() {
            return;
        }
        let pids = std::mem::take(&mut self.external);
        self.phase = Phase::Stopping;
        std::thread::spawn(move || {
            let mut err = None;
            for pid in pids {
                match Proc::open(pid) {
                    Some(p) => {
                        p.stop(Duration::from_secs(5));
                        if p.alive() {
                            err = Some("The other host did not stop.".to_owned());
                        }
                    }
                    None => {
                        err = Some(
                            "Can't stop the other host (it may run as administrator). \
                             End ovhost.exe in Task Manager, then try again."
                                .to_owned(),
                        );
                    }
                }
            }
            post(Event::TakeOverDone { err });
        });
    }

    pub fn on_take_over_done(&mut self, err: Option<String>) {
        if let Some(e) = err {
            // Still running? Back to External with the reason.
            let pids = self.external_hosts();
            if pids.is_empty() {
                self.phase = Phase::Starting;
                self.start();
            } else {
                self.adopt_external(pids);
                self.phase = Phase::Error(e);
            }
            return;
        }
        self.failures = 0;
        self.phase = Phase::Starting;
        self.start();
    }

    pub fn on_external_gone(&mut self) {
        if self.phase == Phase::External || !self.external.is_empty() {
            self.external.clear();
            self.phase = Phase::Starting;
            self.start();
        }
    }

    /// Stop everything we own (Quit). Blocking.
    pub fn shutdown(&mut self) {
        self.gen += 1;
        self.relay_gen += 1;
        let host = self.host.take();
        let relay = self.relay.take();
        let h = std::thread::spawn(move || {
            if let Some(p) = host {
                p.stop(Duration::from_secs(5));
            }
        });
        let _ = h.join();
        if let Some(r) = relay {
            r.stop(Duration::from_secs(2));
        }
    }

    pub fn on_relay(&mut self, gen: u64, out: Output) {
        if gen != self.relay_gen {
            return;
        }
        match out {
            Output::Status(_) | Output::Log(_) => {}
            Output::Exited(code) => {
                crate::log::line(&format!("relay exited ({code:?})"));
                self.relay = None;
                let g = self.relay_gen;
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_secs(3));
                    post(Event::RelayRetry { gen: g });
                });
            }
        }
    }

    pub fn on_relay_retry(&mut self, gen: u64) {
        let wanted = self.args.as_ref().is_some_and(|a| a.run_relay);
        if gen == self.relay_gen && self.relay.is_none() && wanted {
            self.ensure_relay();
        }
    }

    /// Host output. Returns a notice for the tray when a viewer comes/goes.
    pub fn on_host(&mut self, gen: u64, out: Output) -> Option<Notice> {
        if gen != self.gen {
            return None;
        }
        match out {
            Output::Status(ev) => self.on_status(ev),
            Output::Log(line) => {
                // Remember the last error-level log line (or a clap usage
                // error) to explain an unexpected exit.
                if let Some((_, msg)) = line.split_once(" ERROR ") {
                    msg.trim().clone_into(&mut self.last_error);
                } else if let Some(msg) = line.strip_prefix("error: ") {
                    msg.trim().clone_into(&mut self.last_error);
                }
                None
            }
            Output::Exited(code) => {
                crate::log::line(&format!("ovhost exited ({code:?})"));
                self.host = None;
                let was_connected = self.session_active();
                let why = if self.last_error.is_empty() {
                    format!(
                        "The host stopped unexpectedly (exit code {}).",
                        code.unwrap_or(-1)
                    )
                } else {
                    self.last_error.clone()
                };
                self.phase = Phase::Error(why);
                // Drop the dead host's stale park slot (see `stop_then`): the
                // retry re-creates our relay via `ensure_relay`.
                if let Some(r) = self.relay.take() {
                    self.relay_gen += 1;
                    std::thread::spawn(move || r.stop(Duration::from_secs(2)));
                }
                self.schedule_retry();
                was_connected.then_some(Notice::Disconnected)
            }
        }
    }

    fn on_status(&mut self, ev: StatusEvent) -> Option<Notice> {
        match ev {
            StatusEvent::DeviceId { id } => {
                if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) {
                    self.device_id = Some(id);
                }
                None
            }
            StatusEvent::Ready { .. } => {
                let was = self.session_active();
                self.phase = Phase::Ready;
                self.failures = 0;
                self.retry_in = None;
                was.then(|| self.session_over())
            }
            StatusEvent::PeerConnected { fingerprint } => {
                let short: String = fingerprint.chars().take(8).collect();
                self.phase = Phase::Connected {
                    peer: short.clone(),
                    since: crate::util::local_hhmm(),
                };
                Some(Notice::Connected(short))
            }
            StatusEvent::PeerDisconnected { reason } => {
                crate::log::line(&format!("session ended: {reason}"));
                if !self.session_active() {
                    return None;
                }
                self.phase = Phase::Ready;
                Some(self.session_over())
            }
            StatusEvent::SessionFailed { message, .. } => {
                let relay = self.relay().unwrap_or_default().to_owned();
                let was = self.session_active();
                self.phase = Phase::Error(friendly_error(&message, &relay));
                was.then(|| self.session_over())
            }
            StatusEvent::DirectPath {
                state, port, addr, ..
            } => {
                match state {
                    DirectState::Listening => {
                        self.direct.port = port;
                        self.direct.disabled = false;
                    }
                    DirectState::Disabled => self.direct.disabled = true,
                    DirectState::Active => self.direct.active = true,
                    DirectState::Fallback => self.direct.active = false,
                }
                let _ = addr;
                None
            }
            StatusEvent::Upnp { mapped, .. } => {
                self.direct.upnp = Some(mapped);
                None
            }
            StatusEvent::Error { message } => {
                self.last_error = message;
                None
            }
            StatusEvent::Waiting { .. }
            | StatusEvent::PairingCode { .. }
            | StatusEvent::Connecting { .. } => None,
        }
    }

    /// A session just ended: reset its path state and apply a deferred
    /// restart.
    fn session_over(&mut self) -> Notice {
        self.direct.active = false;
        if self.pending_restart {
            self.restart();
        }
        Notice::Disconnected
    }
}

/// Turn raw ovhost errors into something a person can act on.
fn friendly_error(reason: &str, relay: &str) -> String {
    let r = reason.to_ascii_lowercase();
    if r.contains("refused") || r.contains("10061") {
        format!("Can't reach the relay at {relay} (connection refused). Retrying…")
    } else if r.contains("handshake") {
        "A device tried to connect but the handshake failed (wrong PIN?).".to_owned()
    } else {
        format!("{reason}. Retrying…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(dev: bool) -> Env {
        let mut e = Env::detect();
        e.dev = dev;
        e.data_dir = PathBuf::from(r"C:\data");
        e
    }

    fn args() -> HostArgs {
        HostArgs {
            pin: "secret1".into(),
            relay: Some("127.0.0.1:4569".into()),
            run_relay: true,
            send_audio: true,
            play_mic: true,
            hide_wallpaper: true,
            fps: FpsMode::Auto,
            direct: true,
            upnp: true,
        }
    }

    #[test]
    fn argv_defaults_and_screen_last() {
        let a = args().to_argv(&env(false));
        assert_eq!(a.last().map(String::as_str), Some("--screen"));
        let j = a.join(" ");
        assert!(j.starts_with(
            r"--unattended --service --relay 127.0.0.1:4569 --data-dir C:\data --fps 30"
        ));
        assert!(j.contains("--audio") && j.contains("--system-audio"));
        assert!(
            !j.contains("--keep-wallpaper")
                && !j.contains("--no-direct")
                && !j.contains("--max-fps")
        );
    }

    #[test]
    fn the_pin_travels_in_the_environment_only() {
        let a = args();
        assert!(!a.to_argv(&env(false)).iter().any(|x| x.contains("secret1")));
        assert!(!a.to_argv(&env(false)).iter().any(|x| x == "--pin"));
        assert_eq!(a.envs(), [(PIN_ENV, "secret1")]);
    }

    #[test]
    fn argv_options() {
        let mut h = args();
        h.fps = FpsMode::Fps60;
        h.hide_wallpaper = false;
        h.upnp = false;
        h.send_audio = false;
        let j = h.to_argv(&env(false)).join(" ");
        assert!(j.contains("--fps 60 --max-fps 60"));
        assert!(j.contains("--keep-wallpaper") && j.contains("--no-upnp"));
        assert!(!j.contains("--system-audio"));
        h.direct = false;
        let j = h.to_argv(&env(false)).join(" ");
        assert!(j.contains("--no-direct") && !j.contains("--no-upnp"));
        let d = args().to_argv(&env(true)).join(" ");
        assert!(d.contains("--dry-input --fx-dry-run --no-upnp --no-direct --screen"));
    }

    fn status(h: &mut Host, json: &str) -> Option<Notice> {
        let ev = ovcommon::status::parse(json).expect("a status event");
        h.on_host(1, Output::Status(ev))
    }

    #[test]
    fn follows_host_status_events() {
        let mut h = Host::new(env(true));
        h.gen = 1;
        h.args = Some(args());
        assert_eq!(
            status(&mut h, r#"{"event":"device_id","id":"309731599215"}"#),
            None
        );
        assert_eq!(h.device_id.as_deref(), Some("309731599215"));
        status(&mut h, r#"{"event":"waiting","relay":"127.0.0.1:4569"}"#);
        assert_eq!(h.phase, Phase::Starting, "waiting is not ready yet");
        status(&mut h, r#"{"event":"ready","relay":"127.0.0.1:4569"}"#);
        assert_eq!(h.phase, Phase::Ready);
        let n = status(
            &mut h,
            r#"{"event":"peer_connected","fingerprint":"a1b2c3d4e5f6a7b8c9"}"#,
        );
        assert_eq!(n, Some(Notice::Connected("a1b2c3d4".into())));
        assert!(h.session_active());
        status(
            &mut h,
            r#"{"event":"direct_path","state":"active","detail":"x"}"#,
        );
        assert!(h.direct.active);
        let n = status(&mut h, r#"{"event":"peer_disconnected","reason":"closed"}"#);
        assert_eq!(n, Some(Notice::Disconnected));
        assert_eq!(h.phase, Phase::Ready);
        assert!(!h.direct.active);
        // Stale generation is ignored.
        let ev =
            ovcommon::status::parse(r#"{"event":"peer_connected","fingerprint":"ff"}"#).unwrap();
        assert_eq!(h.on_host(0, Output::Status(ev)), None);
        status(
            &mut h,
            r#"{"event":"session_failed","message":"relay connect failed: connection refused (os error 10061)","retry_in_secs":1}"#,
        );
        assert!(matches!(&h.phase, Phase::Error(e) if e.contains("connection refused")));
        status(
            &mut h,
            r#"{"event":"direct_path","state":"listening","port":4570}"#,
        );
        assert_eq!(h.direct.port, Some(4570));
        status(&mut h, r#"{"event":"upnp","mapped":false}"#);
        assert_eq!(h.direct.upnp, Some(false));
    }

    #[test]
    fn an_exit_is_explained_by_the_last_error() {
        let mut h = Host::new(env(true));
        h.gen = 1;
        status(&mut h, r#"{"event":"error","message":"bad relay `x`"}"#);
        h.on_host(1, Output::Exited(Some(1)));
        assert!(matches!(&h.phase, Phase::Error(e) if e == "bad relay `x`"));
    }

    #[test]
    fn no_relay_is_an_actionable_error() {
        let mut h = Host::new(env(true));
        let mut a = args();
        a.relay = None;
        a.run_relay = false;
        h.boot(a, false);
        assert_eq!(h.phase, Phase::Error(NO_RELAY.to_owned()));
    }
}
