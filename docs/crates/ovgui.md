# `ovgui` — OpenView for Windows (desktop GUI)

`ovgui` builds **`OpenView.exe`** (`[[bin]] name = "OpenView"`), a
desktop front end. The manifest describes it as *"desktop GUI that
supervises ovhost/ovrelay and launches ovview"*.

It does **not** link the OpenView networking stack. It drives the other
binaries as child processes:

* **This computer can be controlled**: it runs `ovrelay.exe` and
  `ovhost.exe --unattended ... --service ... --screen` as supervised children,
  parses the host's JSON-lines status events (stdout, `ovcommon::status`)
  into a status, and restarts them with backoff.
* **Control another computer**: it launches `ovview.exe --connect <id>
  --relay <addr>` as a detached process, or `ovview.exe --id <id> --lan-addr
  <addrs> [--relay-fallback --relay <addr>]` when that computer was found on
  this network.

The PIN is always handed over in the `OPENVIEW_PIN` environment variable,
never on a command line (which other processes can read). The OpenView
crates it links are `ovcommon` (data dir, status-event schema) and
`ovdiscovery` (the mDNS browser behind "Computers on this network").

Everything below comes from `crates/ovgui/`. Paths are relative to
the repository root.

---

## 1. UI toolkit

* **Slint `~1.14`** (`default-features = false`, features `std`,
  `backend-winit`, `renderer-software`, `compat-1-2`). The minor version is
  pinned because the vendored D3D11 platform is verified on 1.14.
* The UI is declarative `.slint`, compiled by `slint-build` in `build.rs` with
  the `native` style. A `.slint` error fails the build.
  * `ui/main.slint`: the window shell and the three pages.
  * `ui/adapters.slint`: the `App` global, the `Page` / `HostStatus` enums and
    the `Connection` struct.
  * `ui/theme.slint`: theming.
  * `ui/pages/{remote,settings,about}.slint`.
  * `ui/components/*.slint`: button, input, scroll view, icon, widgets,
    click-away.
  * `ui/icons/*.svg`.
* **Renderer** (`src/platform/mod.rs`):
  * The default on Windows, behind the default `d3d11` feature, is a
    **D3D11 WARP platform**: Slint's software renderer presented through a
    DXGI swap chain. This keeps text crisp and works on headless, RDP and VM
    hosts where GDI presentation blanks the window.
  * If D3D11 initialisation fails, it falls back to the plain software
    renderer.
  * The roughly 900-line `src/platform/d3d11/` module is vendored ("from
    mbfs-face-core-rust / vd-gui"). Local changes are marked `ovgui patch`;
    they are an idle-CPU fix that keeps a tray app from spinning.
  * Environment overrides: `SLINT_BACKEND` (wins), `SLINT_DISABLE_D3D11`,
    `SLINT_USE_SOFTWARE`. The code also sets
    `SLINT_DEBUG_PERFORMANCE=refresh_full_speed` when it is unset, which
    disables partial rendering.
* Main window: title "OpenView", or "OpenView (test mode)" in test mode; size
  1000x700, minimum 860x600.
* `build.rs` also includes `assets/winres.rs`: the icon (resource ID 1, used
  for the exe, the window and the tray), an application manifest (asInvoker,
  supportedOS Windows 10/11, PerMonitorV2 DPI with the `true/pm` fallback,
  Common Controls v6) and VERSIONINFO (CompanyName "OpenView contributors",
  ProductName "OpenView", FileVersion = `CARGO_PKG_VERSION`). For Windows
  binaries it reserves an **8 MiB main-thread stack** (`/STACK` on MSVC, `--stack` on GNU),
  because debug builds construct the window on the main stack.
* Release builds use `windows_subsystem = "windows"`, so there is no console.

## 2. Modules

| File | Role |
|---|---|
| `src/main.rs` | Start-up, the state struct, the event drain |
| `src/actions.rs` | User actions and the Slint callback wiring: connect, PIN, settings changes (the relay setting is validated with `ovcommon::relay_addr::RelayAddr::is_valid`), pause, take-over |
| `src/view.rs` | Rendering the state into the window: settings, connection list, host status card, tray tooltip and menu |
| `src/env.rs` | Normal vs **test mode**, data dir (`ovcommon::paths`) and one-time settings migration, credential and Run-key namespaces, binary paths and the missing-binary error |
| `src/host.rs` | `Host` supervisor: the ovhost command line and environment (`OPENVIEW_PIN`), relay and host lifecycle, status events → `Phase`, retries, detect / take over an external host |
| `src/procs.rs` | Spawning children hidden: stdout parsed as status events, stderr copied to a log file; a kill-on-close **job object**; graceful stop via **Ctrl-Break**; process lookup by image path |
| `src/events.rs` | Hand-off from worker threads to the UI thread (`post`, then one `invoke_from_event_loop` drain per burst) |
| `src/shell.rs` | Window subclass: icon, dark title bar, close-to-tray, tray icon and menu, balloons, theme-change and show requests |
| `src/win.rs` | Credential Manager, registry (Run key, app theme), clipboard, shell open |
| `src/store.rs` | `settings.json`, `connections.json` (atomic temp file + rename) |
| `src/instance.rs` | Single instance: a named mutex plus `Show` / `Quit` named events |
| `src/log.rs` | `logs\openview.log` (start-up, status changes, child exits; truncated at start-up when over 4 MiB) and `logs\panics.log` (panic hook with forced backtrace) |
| `src/util.rs` | ID grouping, PIN generation and validation, "x minutes ago" formatting |
| `src/lan.rs` | "Computers on this network": an `ovdiscovery::mdns::Browser` that runs while the window is visible, a 1 s UI-thread timer copying its snapshot (own device excluded) into an in-place model, and the `ovview --id … --lan-addr …` arguments |
| `src/platform/mod.rs`, `platform/config.rs`, `platform/d3d11/` (`mod.rs`, `platform.rs`, `warp_presenter.rs`, `window_adapter.rs`) | Rendering platform selection and the vendored D3D11 WARP platform |

**Threading**: Slint state is touched only on the UI thread. Child output
readers, timers and the tray window procedure `post` plain `Event` values, and
one event-loop callback (`drain`) applies them.

---

## 3. Start-up (`main.rs::main`)

1. `Env::detect()` (§6) and `log::init(<data>\logs)`.
2. **Single instance**: `Local\OpenView.Instance`, or `OpenView-dev` in test
   mode. A second launch signals `Local\<name>.Show` (the running instance
   shows its window), or `Local\<name>.Quit` when started with `--quit`, and
   then exits.
3. Initialise the platform, create the `MainWindow`, install the tray. With
   `--minimized` the window starts hidden.
4. One-time migration of `settings.json` / `connections.json` from
   `%APPDATA%\OpenView` (normal, non-portable mode only, never over existing
   files), then load both.
5. **Host PIN**: read from Windows Credential Manager under the target
   `<prefix>:host-pin`. If it is missing, an 8-character PIN is generated from
   `abcdefghjkmnpqrstuvwxyz23456789` (no 0/O/1/l/I) and stored there.
6. `Host::boot`. If an `ovhost.exe` started outside the GUI is running from
   the install folder (`procs::find_by_exe`, matched by image path), the
   phase becomes `External` ("Another host is running") instead of starting
   a duplicate; the GUI starts its own host once that process exits.
   Otherwise the relay and host start, unless the user has paused remote
   access. Test mode never looks for external hosts.
7. Run the Slint event loop. On quit, `host.shutdown()` stops the children and
   the tray icon is removed.

**Closing the window hides it to the tray** and shows a balloon: "OpenView is
still running — This computer stays reachable." Only the tray's **Quit
OpenView** item, or `OpenView.exe --quit`, exits. The tray menu has four items:
Open OpenView, Copy ID, Pause remote access, Quit OpenView.

---

## 4. How the GUI drives the host (`host.rs`)

### The ovhost command line (`HostArgs::to_argv`)

```text
OPENVIEW_PIN=<PIN> (environment)
ovhost.exe --unattended --service --relay <host relay> --data-dir <data dir>
           --fps <F> [--max-fps <M>] [--audio] [--system-audio]
           [--keep-wallpaper] [<test-mode flags> | --no-direct | --no-upnp]
           --screen
```

| Setting | Flags |
|---|---|
| "Run a relay on this PC" on / off | `--relay 127.0.0.1:4569` (the bundled relay) / `--relay <Relay address>` |
| Frame rate Auto / 30 / 60 | `--fps 30` / `--fps 30 --max-fps 30` / `--fps 60 --max-fps 60` |
| "Play the remote microphone on this PC" | `--audio` |
| "Send PC audio to the remote device" | `--system-audio` |
| "Hide wallpaper during sessions" off | `--keep-wallpaper` |
| "Direct connections" off | `--no-direct` |
| "Open the router port automatically (UPnP)" off | `--no-upnp` (only when direct is on) |
| Test mode | `--dry-input --fx-dry-run --no-upnp --no-direct --lan-port 0` (LAN listener on any free port; the test identity is announced by mDNS) |

`--screen` is always **last**, because it takes an optional `WxH`.
`--data-dir` makes the host keep its identity in the GUI's data dir, so both
always agree. With "Run a relay on this PC" off and no relay address, the host
is not started and the status shows "No relay configured. Enter a relay
address in Settings, or turn on "Run a relay on this PC"." A missing
`ovhost.exe` is reported the same way ("ovhost.exe is missing. It must be in
the same folder as OpenView.exe (...). Reinstall OpenView.").

### The relay (`ensure_relay`)

Only with "Run a relay on this PC" on: `ovrelay.exe --bind <relay_bind>`,
which is `0.0.0.0:4569` normally (TCP, all interfaces: other devices dial it,
so allow it in the firewall), or `127.0.0.1:<port>` in test mode. Its stderr
goes to `logs\ovrelay.log`.

* If the port is already taken (for example by a relay started by hand), the
  GUI uses that relay instead of starting its own.
* If the relay process exits, it is restarted after 3 s.
* When the host is stopped or restarted, **the GUI also restarts its own
  relay**. The reason given in the code: the relay keeps a stopped host's park
  slot until its 30 s timeout ("session already has a responder"), so a
  restarted host would otherwise be unreachable for that time.

### Status from stdout (`Host::on_status`)

ovhost's stdout carries JSON-lines status events (`ovcommon::status`, schema
in [ovcommon.md](ovcommon.md) §3); its stderr (human logs) only goes to
`logs\ovhost.log`. The GUI never scrapes log text, except that the last
`ERROR` line (or a clap `error:` line) explains an unexpected exit.

| ovhost event | GUI phase |
|---|---|
| `device_id` | Records the device ID. It is shown grouped in threes ("123 456 789 012") and saved as `last_device_id`. |
| `ready` | **Ready**: "Ready — waiting for a connection" |
| `peer_connected` | **Connected** (first 8 hex chars of the fingerprint, local HH:MM) plus a tray balloon |
| `peer_disconnected` | Back to Ready plus a balloon |
| `session_failed` | **Error**, with a friendly message: "Can't reach the relay at ... (connection refused)"; for a failed incoming handshake one text per cause: "A device tried to connect with a wrong PIN." (authentication failed only), "Too many wrong PINs: …", "A device started to connect but the connection did not complete (network problem?)." (timeout), a version mismatch, or "… the secure connection could not be set up."; otherwise the raw reason |
| `error` | Remembered as the reason shown if the host exits |
| `direct_path` (`listening` / `disabled` / `active` / `fallback`), `upnp` | Status detail, e.g. "Relay 127.0.0.1:4569 · direct UDP 4570 · router port open (UPnP)" |
| `lan` | Logged to `openview.log` (LAN port, mDNS on/off) |

Every status change is also written to `logs\openview.log` (`status: <text>
| <detail> | id <id>`).

* **Retries**: if ovhost exits unexpectedly, the phase becomes Error and a
  restart is scheduled after `min(2 << min(failures, 4), 30)` s (4, 8, 16, 30
  s...). The UI shows "Trying again in N s."
* **Settings changes** restart the host immediately. During an active session
  the restart waits until the session ends ("Changes will apply when the
  current session ends."). **A PIN change** restarts at once, ending any open
  session.
* **Stop** (`procs::Proc::stop`): the GUI sends **Ctrl-Break** to the child's
  console process group and waits a grace period (5 s for the host, 2 s for
  the relay) before `TerminateProcess`. Ctrl-Break lets ovhost restore the
  wallpaper and remove its UPnP mapping (see `ovhost.md` §8).
* **Kill on close**: supervised children are placed in a job object with
  `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so the host and relay never outlive a
  crashed GUI.
* **Stop it and take over** (external host): the GUI stops that process
  (Ctrl-Break, then terminate), then starts its own host with the GUI's own
  PIN. It never reads the other process's command line or PIN. If the
  external host runs elevated and cannot be stopped, the GUI says so ("Can't
  stop the other host (it may run as administrator). End ovhost.exe in Task
  Manager, then try again.").

## 5. How the GUI drives the viewer (`actions.rs::connect`)

1. The partner ID (digits only, at least 6) and PIN are entered by the user,
   or the PIN is taken from Credential Manager under `<prefix>:<id>`.
2. The GUI checks that `ovview.exe` exists next to `OpenView.exe` (else the
   "... is missing" error).
3. Relay: the per-connection relay if one was saved, then the "Relay address"
   setting (test mode: the test relay).
4. **On this network?** If the ID is in the "Computers on this network" list,
   it spawns `ovview.exe --id <id> --lan-addr <addr,addr…>` (plus
   `--relay-fallback --relay <relay>` when a relay is known): a direct LAN
   session, no relay needed. Otherwise, without a relay, the GUI shows "No
   relay configured. Enter the relay address in Settings first." and opens
   Settings; with one it spawns `ovview.exe --connect <id> --relay <relay>`.
   Either way the PIN goes in `OPENVIEW_PIN`, via `procs::spawn_detached`. The viewer is **not** in the
   job and does not write into the GUI's pipes, so it survives OpenView
   quitting. Its stderr goes to `logs\ovview-<id>.log`, its stdout (status
   events) to `logs\ovview-<id>.status.jsonl`, which a tail thread reads.
5. **Connected** is detected from the viewer's `peer_connected` status event.
   A toast then shows "Connected to <id>".
6. If the viewer exits with a non-zero code before connecting, the message of
   its last `error` event is shown as "Couldn't connect: ...".
7. "Remember PIN on this PC" stores or deletes the credential. The connection
   is recorded in "Recent & favorites" (`connections.json`).

## 6. Environment and test mode (`env.rs`)

Test mode (`OPENVIEW_DEV_PORTS=<port>`) is a documented development feature
that never touches the real install.

| | Normal | Test mode (`OPENVIEW_DEV_PORTS=<port>`) |
|---|---|---|
| Relay bind | `0.0.0.0:4569` (`PROD_RELAY_PORT`) | `127.0.0.1:<port>` |
| Host parks on (bundled relay) | `127.0.0.1:4569` | `127.0.0.1:<port>` |
| Default outgoing relay | none: the user must enter one | `127.0.0.1:<port>` |
| Data dir | `%LOCALAPPDATA%\OpenView` (`<exe dir>\data` when `portable.txt` sits next to `OpenView.exe`) | `%LOCALAPPDATA%\OpenView-dev` |
| Credential prefix | `OpenView` | `OpenView-dev` |
| Run key | `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, value `OpenView` = `"<exe>" --minimized` | `HKCU\Software\OpenView-dev\Run` |
| External host lookup | `ovhost.exe` running from the binaries folder | Never (no external process is adopted or stopped) |
| Host flags | — | `--dry-input --fx-dry-run --no-upnp --no-direct`; the status shows "Test mode · input is simulated" |

Other environment variables:

| Variable | Effect |
|---|---|
| `OPENVIEW_DATA_DIR` | Override the data directory (both modes) |
| `OPENVIEW_BIN_DIR` | Development: folder holding `ovhost.exe`, `ovrelay.exe`, `ovview.exe` |
| `SLINT_BACKEND`, `SLINT_DISABLE_D3D11`, `SLINT_USE_SOFTWARE` | Renderer selection (§1) |

The theme follows the Windows app theme (`AppsUseLightTheme`) and reacts to
`ImmersiveColorSet` changes.

**Binaries**: `ovhost.exe`, `ovrelay.exe` and `ovview.exe` must sit in the
folder of `OpenView.exe` (`OPENVIEW_BIN_DIR` overrides it for development).
There is no setting for it and no `target\release` fallback; a missing binary
shows "<name> is missing. It must be in the same folder as OpenView.exe
(<dir>). Reinstall OpenView."

**Network** (for firewall rules): the bundled relay listens on TCP
`0.0.0.0:4569` (inbound from other devices); the host connects out to the
relay over TCP and binds UDP `0.0.0.0:4570` (and `[::]:4570`) for the direct
path, the only inbound port it needs (UPnP maps it unless turned off).

## 7. Persistent state

| Location | Content |
|---|---|
| `<data>\settings.json` | `send_audio` (default on), `play_mic` (on), `hide_wallpaper` (on), `fps` (`auto`/`fps30`/`fps60`, default auto), `direct` (on), `upnp` (on), `relay_address` (default empty), `run_relay` (on; older files without it keep the bundled relay on), `paused`, `last_device_id` |
| `<data>\connections.json` | Recent partners: ID, name, relay, `last_connected`, favourite |
| Credential Manager `<prefix>:host-pin` | This computer's access PIN |
| Credential Manager `<prefix>:<partner id>` | Remembered partner PINs. PINs are never stored in the JSON files. |
| `<data>\logs\` | `openview.log`, `panics.log`, `ovhost.log` / `ovrelay.log` (child stderr), `ovview-<id>.log` (viewer stderr), `ovview-<id>.status.jsonl` (viewer stdout status events), `phone\` (iOS log uploads, written by ovhost) |
| `<data>\ovhost-unattended.id`, `<data>\ovhost-desktopfx.state` | Written by ovhost (`--data-dir`) |

**PIN rules** (`util::check_pin`): at least 6 characters, at most 64 bytes, no
whitespace. The "Generate" button proposes an 8-character PIN.

## 8. Pages

* **Remote Control**
  * "YOUR ID" and PIN, with copy, change and generate actions.
  * Host status: Starting, Ready, Connected, Paused, Error ("Not ready"), or
    "Another host is running" with a **Stop it and take over** button.
  * A notice linking to Settings while no relay address is configured.
  * "End session" and pause/resume.
  * "Connect to a computer" (Partner ID, PIN, Remember PIN).
  * "Computers on this network": OpenView hosts found by mDNS (name, grouped
    ID, "different OpenView version" when the TXT `v` differs). Click fills
    the Connect card (ID and saved PIN); double-click or **Connect** dials
    it over the LAN. While empty it says "Looking for OpenView computers on
    this network…" (or "Network discovery is not available." when the
    browser cannot start). Browsing stops while the window is hidden.
  * "Recent & favorites".
* **Settings**
  * Relay (first card): "Relay address (IP, hostname[:port] or wss://…)"
    (empty by default; a notice shows while it is missing). A non-empty value
    must pass `RelayAddr::is_valid` (see [ovcommon.md](ovcommon.md) §3);
    otherwise a toast says "Relay must be an IP, hostname[:port] or
    wss://host/path" and the previous value is restored and "Run a relay on this PC" (default
    on: the bundled relay on TCP 4569, and this PC's host parks on it; off:
    the host parks on the relay address).
  * General: "Start OpenView at sign-in".
  * Incoming sessions: send PC audio, play remote mic, hide wallpaper, frame
    rate.
  * Direct connections: direct path, UPnP.
  * Troubleshooting: "Open logs folder".
* **About**: version and a short description ("End-to-end encrypted sessions
  (X25519 + ChaCha20-Poly1305), PIN-authenticated.", "Runs ovhost, ovrelay and
  ovview under the hood."), and Slint's `AboutSlint` widget ("Made with
  Slint"), which the Slint royalty-free license requires.

## 9. Command-line flags

| Flag | Effect |
|---|---|
| `--minimized` | Start hidden in the tray (the Run key passes this) |
| `--quit` | Ask the running instance to quit (it stops its host and relay), then exit |

Any other argument is ignored.
