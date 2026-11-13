# `ovcommon` — data directories, status events, logging setup

> Source: `crates/ovcommon/`
> Dependencies: `serde`, `serde_json`, `tracing`; `tracing-subscriber` (optional, feature `logging`); `windows` (Win32 Shell / COM, for the Downloads known folder). No networking, codec or crypto crates, so the GUI can depend on it cheaply.

## 1. Overview

Shared plumbing of the OpenView Windows binaries:

| Module | Purpose | Used by |
|---|---|---|
| `paths` | Where data, logs and received files live: one rule for every binary, including portable mode and one-time migrations | `ovhost`, `ovview`, `ovgui` |
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

## 3. `status` — JSON-lines status events

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
| `connecting` | `relay`, `target` | viewer | Dialing the relay for `target` |
| `peer_connected` | `fingerprint` | both | Handshake done, session running |
| `peer_disconnected` | `reason` | both | Session over |
| `direct_path` | `state`, `port`?, `addr`?, `detail`? | both | `listening` / `disabled` (host socket), `active` / `fallback` (session path) |
| `upnp` | `mapped`, `addr`? | host | Router port mapping created (`addr` = public address) or unavailable |
| `session_failed` | `message`, `retry_in_secs` | host (`--service`) | A session attempt failed; the host retries |
| `error` | `message` | both | Fatal error; the process exits non-zero right after |

Fields marked `?` are omitted when absent. Examples:

```json
{"event":"device_id","id":"309731599215"}
{"event":"direct_path","state":"listening","port":4570}
{"event":"peer_connected","fingerprint":"a1b2..."}
```

API: `StatusEvent` (serde, `#[serde(tag = "event", rename_all = "snake_case")]`),
`DirectState { Listening, Disabled, Active, Fallback }`, `emit(&ev)` (one
line + flush; errors such as a closed stdout are ignored so status output
never takes the process down), and `parse(line) -> Option<StatusEvent>`.
Consumers must ignore unknown events and unknown fields (new ones may be
added); `parse` does exactly that.

## 4. `logging` — the CLI subscriber (feature `logging`)

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

## 5. Tests

`cargo test -p ovcommon --all-features`: data-dir precedence (override >
portable > LocalAppData), migration copies once and never overwrites,
`unique_path` numbering, the status wire format (tagged snake_case, optional
fields skipped), `parse` ignoring unknown events / fields / non-JSON, and the
log-filter precedence.
