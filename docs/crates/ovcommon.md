# `ovcommon` — data directories, relay addresses, status events, logging setup

> Source: `crates/ovcommon/`
> Dependencies: `serde`, `serde_json`, `tracing`; `tracing-subscriber` (optional, feature `logging`); `windows` (Win32 Shell / COM, for the Downloads known folder). No networking, codec or crypto crates, so the GUI can depend on it cheaply.

## 1. Overview

Shared plumbing of the OpenView Windows binaries:

| Module | Purpose | Used by |
|---|---|---|
| `paths` | Where data, logs and received files live: one rule for every binary, including portable mode and one-time migrations | `ovhost`, `ovview`, `ovgui` |
| `relay_addr` | The relay address syntax shared by every `--relay` input and the GUI relay setting: IP, `hostname[:port]`, `ws://` / `wss://` URL | `ovhost`, `ovview` (clap parses `--relay` into `RelayAddr`), `ovgui` (setting validation) |
| `status` | The machine-readable status events `ovhost` / `ovview` print as JSON lines on **stdout** (human logs go to stderr) | emitted by `ovhost`, `ovview`; parsed by `ovgui` |
| `logging` (feature `logging`) | The `tracing` subscriber of the CLI binaries (stderr, `-v` / `--log-level` / `OPENVIEW_LOG`) | `ovhost`, `ovview`, `ovrelay` |

## 2. `paths` — data locations

| What | Location |
|------|----------|
| Data root (identity, settings, logs) | `%LOCALAPPDATA%\OpenView` |
| Portable mode (`portable.txt` next to the exe) | `<exe dir>\data` |
| Explicit override (`--data-dir <path>` for `ovhost`, `OPENVIEW_DATA_DIR` for the GUI) | `<path>` |
| Files received from a peer | `Downloads\OpenView` (the user's Downloads known folder) |

Resolution order (`resolve_data_dir`, unit-tested): the override wins, then
portable mode, then `%LOCALAPPDATA%\OpenView`. Everything is per machine and
per user, so it lives under *Local* AppData, not the roaming profile. PINs
are never stored here (the GUI keeps them in Windows Credential Manager).

Contents of the data root:

| File | Written by |
|---|---|
| `ovhost-unattended.id` | `ovhost` (unattended identity = the stable device ID) |
| `ovhost-desktopfx.state` | `ovhost` crash-recovery marker for wallpaper / animations (only while a screen session runs) |
| `settings.json`, `connections.json` | GUI |
| `logs\openview.log`, `logs\ovhost.log`, `logs\ovrelay.log`, `logs\ovview-<id>.log`, `logs\ovview-<id>.status.jsonl` | GUI (child stderr / stdout redirected there) |
| `logs\phone\ovlog-ios-*.log` | `ovhost` (diagnostic logs uploaded by the iOS app) |

Public API:

| Item | Meaning |
|---|---|
| `APP_DIR_NAME` (`"OpenView"`), `PORTABLE_MARKER` (`"portable.txt"`), `PORTABLE_DATA_DIR` (`"data"`) | Constants |
| `exe_dir()`, `local_app_data()`, `roaming_app_data()`, `is_portable()` | Environment helpers (`local_app_data` falls back to the temp dir) |
| `data_dir(override)`, `ensure_data_dir(override)`, `resolve_data_dir(...)` | The data root (not created / created / pure rule) |
| `logs_dir(data)` | `<data>\logs` |
| `received_files_dir()` | `FOLDERID_Downloads\OpenView`, else `%USERPROFILE%\Downloads\OpenView`, else the temp dir (not created) |
| `unique_path(dir, name)` | `name`, or `name (2).ext`, `(3)`, ... : a received file never overwrites an existing one |
| `migrate_file(old, new)` | One-time copy when `new` is missing and `old` exists; never overwrites |

One-time migrations from releases before 0.2 (all copies, never overwrites):

* `ovhost` copies `ovhost-unattended.id` from next to `ovhost.exe` into the
  data dir, so the device ID survives the upgrade.
* `ovhost` still recovers an `ovhost-desktopfx.state` marker left next to
  the exe by an old release.
* The GUI copies `settings.json` / `connections.json` from
  `%APPDATA%\OpenView` once (normal mode only, not in portable or test mode).

## 3. `relay_addr` — relay addresses

`RelayAddr` (`src/relay_addr.rs`) is what users type for a relay. It matches
the iOS app's `RelayAddress`.

```rust
pub enum RelayAddr {
    Tcp { host: String, port: u16 },                       // raw relay protocol over TCP
    WebSocket { url: String, tls: bool, host: String, port: u16, path: String },
}
```

| Input | Result |
|---|---|
| `203.0.113.5`, `relay.example.com` | `Tcp`, port `DEFAULT_PORT` (4569) |
| `relay.example.com:5000`, `[2001:db8::1]:5000` | `Tcp` on that port |
| `2001:db8::1` (bare IPv6, several colons) | `Tcp`, port 4569 |
| `ws://host[:port]/path`, `wss://host[:port]/path` | `WebSocket`; default port 80 / 443, `tls` for `wss`, path `/` when empty (a bare `?query` becomes `/?query`), the fragment dropped |

Rejected: empty input, whitespace or control characters, credentials in a
URL (`user@`), other schemes, a path without a `ws://` / `wss://` scheme, port
0 or out of range, and host characters other than letters, digits, `.`, `-`,
`_` and `:` (so no IPv6 zone IDs). Hostnames are resolved when dialing, not
when parsing; every resolved address is tried.

| Item | Meaning |
|---|---|
| `RelayAddr::parse(text)` / `FromStr` | Parse (surrounding whitespace ignored). The error (`RelayAddrError`) names the input and ends with the hint |
| `RelayAddr::is_valid(text)` | `parse(text).is_ok()`, used by the GUI to validate the relay setting |
| `host()`, `port()` | Host (IPv6 without brackets) and the TCP port dialed |
| `Display` | `host:port`, `[v6]:port`, or the original URL for WebSocket |
| `DEFAULT_PORT` | `4569` |
| `RELAY_HINT` | `"IP, hostname[:port] or wss://host/path"`, shown in errors and help text |

The binaries dial `Tcp` with `ovtransport::RelayTunnelCarrier::connect_host`
and `WebSocket` with `connect_ws` (see [ovtransport.md](ovtransport.md)).

## 4. `status` — JSON-lines status events

`ovhost` and `ovview` print one JSON object per line on **stdout**;
human-readable logs go to **stderr**. The GUI (`OpenView.exe`) parses stdout
and never scrapes log text. `ovview --info` / `--export` are plain-text
reports on stdout instead.

Every object carries an `"event"` tag; the other fields depend on it:

| `event` | Fields | Emitted by | Meaning |
|---------|--------|------------|---------|
| `device_id` | `id` | host | Unattended device ID (decimal string, stable across runs) |
| `pairing_code` | `code`, `fingerprint` | host | Attended mode: the one-time code to share |
| `waiting` | `relay` | host | Connecting / re-parking on the relay |
| `ready` | `relay` | host | Parked on the relay: reachable, waiting for a viewer |
| `connecting` | `relay`, `target` | viewer | Dialing the relay for `target` (`relay` = `lan <addr>` for a LAN dial) |
| `peer_connected` | `fingerprint` | both | Handshake done, session running |
| `peer_disconnected` | `reason` | both | Session over |
| `direct_path` | `state`, `port`?, `addr`?, `detail`? | both | `listening` / `disabled` (host socket), `active` / `fallback` (session path) |
| `upnp` | `mapped`, `addr`? | host | Router port mapping created (`addr` = public address) or unavailable |
| `lan` | `port`?, `mdns`, `detail`? | host | LAN sessions: `port` = the UDP port viewers on this network dial (absent = LAN listener off), `mdns` = announced as `_openview._udp` (unattended only) |
| `session_failed` | `message`, `retry_in_secs` | host (`--service`; also while the relay is unreachable but LAN sessions keep the host up) | A session attempt failed; the host retries |
| `error` | `message` | both | Fatal error; the process exits non-zero right after |

Fields marked `?` are omitted when absent. Examples:

```json
{"event":"device_id","id":"123456789012"}
{"event":"direct_path","state":"listening","port":4570}
{"event":"peer_connected","fingerprint":"a1b2..."}
```

API: `StatusEvent` (serde, `#[serde(tag = "event", rename_all = "snake_case")]`),
`DirectState { Listening, Disabled, Active, Fallback }`, `emit(&ev)` (one
line + flush; errors such as a closed stdout are ignored so status output
never takes the process down), and `parse(line) -> Option<StatusEvent>`.
Consumers must ignore unknown events and unknown fields (new ones may be
added); `parse` does exactly that.

## 5. `logging` — the CLI subscriber (feature `logging`)

`logging::init(verbose, level)` installs a `tracing-subscriber` `fmt`
subscriber writing to **stderr**, with ANSI colours only when stderr is a
terminal and module targets only when the filter is not plain `info`. The
filter is chosen, first match wins, from:

1. `--log-level <filter>` (any `EnvFilter` directive, e.g. `debug` or
   `info,ovtransport=trace`);
2. `-v` (debug) / `-vv` (trace);
3. the `OPENVIEW_LOG` environment variable;
4. `info`.

An invalid filter falls back to `info` with a warning. At `info` only
lifecycle lines appear (startup, device ID, relay wait / ready, peer
connected / disconnected, direct path active / fallback, UPnP result,
warnings and errors). Periodic statistics are `debug`, per-event noise
(every input event) `trace`.

## 6. Tests

`cargo test -p ovcommon --all-features`: data-dir precedence (override >
portable > LocalAppData), migration copies once and never overwrites,
`unique_path` numbering, relay address forms (TCP and WebSocket forms,
garbage rejected, `Display` round trips), the status wire format (tagged snake_case, optional
fields skipped), `parse` ignoring unknown events / fields / non-JSON, and the
log-filter precedence.
