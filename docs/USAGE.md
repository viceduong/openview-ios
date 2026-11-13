# OpenView — Operator Guide

This guide covers how to run the OpenView desktop binaries. Every flag,
default and message below comes from the argument parsers, the help text and
the log lines in the source:

* `crates/ovhost/src/main.rs`
* `crates/ovview/src/main.rs`
* `crates/ovrelay/src/main.rs`
* `crates/ovgui/src/{main,env}.rs`

For architecture, see [`crates/ovhost.md`](crates/ovhost.md),
[`crates/ovview.md`](crates/ovview.md) and [`crates/ovgui.md`](crates/ovgui.md).

| Binary | Crate | Role |
|---|---|---|
| `ovrelay` | `crates/ovrelay` | Rendezvous relay: pairs a parked host with a dialing viewer and pipes bytes. Accepts raw TCP and WebSocket clients on the same port. |
| `ovhost` | `crates/ovhost` | The computer being controlled |
| `ovview` | `crates/ovview` | The controlling computer: native viewer window |
| `OpenView.exe` | `crates/ovgui` | Windows GUI that supervises `ovrelay` + `ovhost` and launches `ovview` |

The command-line binaries parse flags with `clap`: `--help` / `-h` lists the
flags, `--version` / `-V` prints the version, and an unknown flag or a missing
value prints an error and exits with code 2. Development and test switches
are accepted but hidden from `--help` (listed below as "hidden").

**Output.** `ovhost` and `ovview` print machine-readable status events as
JSON lines on **stdout** (schema: [`crates/ovcommon.md`](crates/ovcommon.md)
§3) and human-readable logs on **stderr** (`tracing`, no colours when stderr
is not a terminal). The default `info` level shows only lifecycle lines
(startup, device ID, waiting / ready on the relay, peer connected /
disconnected, direct path active / fallback, UPnP mapped / unavailable,
warnings and errors). Periodic statistics (capture, link pacing, H.264,
audio, session summaries) need `-v` (debug); every input event needs `-vv`
(trace). `--log-level FILTER` or the `OPENVIEW_LOG` variable set any
`tracing` filter.

**PIN.** `--pin` works but other processes can read a command line; prefer
the `OPENVIEW_PIN` environment variable (both binaries read it, and the GUI
always hands the PIN over this way).

> **Port.** `ovrelay` binds `0.0.0.0:4569` by default, the port `ovhost`,
> `ovview`, the GUI and the iOS app dial by default.

---

## 1. Flag reference

### `ovrelay`

`ovrelay [--bind HOST:PORT] [--timeout SECS] [--max-connections N] [--max-per-ip N] [--max-pending N] [-v|-vv] [--log-level FILTER]`

| Flag | Default | Meaning |
|---|---|---|
| `--bind HOST:PORT` | `0.0.0.0:4569` | Listen address (TCP). The same port also accepts WebSocket upgrades at `GET /relay`, plus plain HTTP requests (health checks, answered and closed). |
| `--timeout SECS` | `30` | How long an unmatched (parked) session waits before the relay sends `XT` |
| `--max-connections N` | `1024` | Simultaneous client connections; more are closed at accept |
| `--max-per-ip N` | `32` | Simultaneous connections from one IP (loopback — a local reverse proxy or tunnel — is exempt) |
| `--max-pending N` | `512` | Parked (unpaired) sessions; a CONNECT beyond it gets `XT` |
| `-v`, `-vv` | | Debug / trace logging |
| `--log-level FILTER` | | `tracing` filter (e.g. `debug`, `info,ovrelay=trace`); overrides `-v` and `OPENVIEW_LOG` |
| `--help`, `-h` / `--version`, `-V` | | Print usage / version |

Logs go to stderr. At the default `info` level the relay prints its listen
address and one line per paired and per closed session; per-direction byte
counts and WebSocket close diagnostics need `-v`. A client has 15 s per read
and 20 s in total to send its HTTP upgrade and CONNECT line.

### `ovhost`

```text
ovhost [--relay ADDR]                                   # attended: one session, pairing code
ovhost --screen [WxH] [--fps N] [--relay ADDR]
set OPENVIEW_PIN=<pin> & ovhost --service --screen      # unattended, serve forever
```

**Session mode and connectivity**

| Flag | Default | Meaning |
|---|---|---|
| `--service` | off (one session, then exit) | Serve sessions forever, with an error backoff of 1 s doubling to 30 s. It is a loop, not a Windows SCM service. |
| `--relay ADDR` | `127.0.0.1:4569` | Relay to park on: an IP or host name with an optional port (default 4569), or a `ws://` / `wss://` URL. The host connects **out** to it (TCP, or WebSocket over TCP/TLS). |
| `--unattended` | off | Unattended access: stable device ID and PIN authentication. Needs a PIN (`--unattended needs a PIN (OPENVIEW_PIN or --pin)`). |
| `--pin <pw>` / env `OPENVIEW_PIN` | — | The access PIN. Implies `--unattended`. Prefer the environment variable: a command line is visible to other processes. |
| `--data-dir <dir>` | `%LOCALAPPDATA%\OpenView` (`<exe dir>\data` when `portable.txt` sits next to the exe) | Where the unattended identity `ovhost-unattended.id` (= the device ID) and the desktop-effects crash marker live. An identity left next to the exe by an older release is copied here once. |
| `--no-clipboard` | sync on | Disable bidirectional text clipboard sync |
| `-v`, `-vv` | | Debug / trace logging (stderr) |
| `--log-level FILTER` | `info` (env `OPENVIEW_LOG`) | `tracing` filter; overrides `-v` and `OPENVIEW_LOG` |
| `--help`, `-h` / `--version`, `-V` | | Print usage / version |
| `--listen` | hidden | Accepted for compatibility and ignored: one session is the default |

**Screen streaming**

| Flag | Default | Meaning |
|---|---|---|
| `--screen [WxH]` | off (terminal mode) | Stream the screen. Default: the full primary monitor. An optional `WxH` (e.g. `640x360`) captures that region at (0,0) of the primary monitor. **Put `--screen` last** so the next token is not taken as its size. |
| `--fps N` | `10` | Base capture rate (1..240). AUTO may go above it (see `--max-fps`). |
| `--max-fps N` | `60` (1..240) | Highest AUTO rate, used only when the link can carry it |
| `--gdi` | off | Capture with GDI BitBlt instead of DXGI Desktop Duplication. DXGI also falls back to GDI on its own. |
| `--h264 auto\|hw\|sw\|off` | `auto` | H.264 for high-motion regions: hardware first, hardware only, software only, or never. Without Media Foundation (Windows N/KN lacking the Media Feature Pack) the host logs one line and streams lossless tiles. |

**Audio**

| Flag | Default | Meaning |
|---|---|---|
| `--mic` | off | Stream the host microphone (48 kHz mono) |
| `--system-audio` | off | Stream what the host's speakers play (WASAPI loopback, 10 ms). Conflicts with `--mic`. |
| `--audio` | off | Play the viewer's microphone on the host speakers (full duplex) |

**Direct path (UDP)**

| Flag | Default | Meaning |
|---|---|---|
| `--direct-port N` | `4570` | UDP port of the direct path, bound on `0.0.0.0` (and `[::]`). The only **inbound** port the host needs; allow it in the firewall. |
| `--no-direct` | direct on | Relay only, with no direct-path upgrade |
| `--lan-port N` | `4571` | UDP port for **LAN sessions without the relay**: a viewer on the same network sends its handshake here (`0` = any free port; unattended hosts announce the bound port by mDNS). Allow it in the firewall. |
| `--no-lan` | LAN on | Do not accept LAN sessions (implies `--no-mdns`) |
| `--no-mdns` | mDNS on | Unattended mode: do not announce the host on the local network (mDNS / Bonjour `_openview._udp`, UDP 5353). LAN viewers then need its address (`ovview --lan-addr`). |
| `--no-upnp` | UPnP on | Do not map the direct port on the router (UPnP IGD) |
| `--stun H:P` | `stun.cloudflare.com:3478` | STUN server used to learn the public address |
| `--no-stun` | STUN on | Skip the STUN query |

**Desktop effects** (screen sessions only)

| Flag | Default | Meaning |
|---|---|---|
| `--keep-wallpaper` | wallpaper hidden | Do not hide the wallpaper |
| `--keep-animations` | animations off | Do not disable UI animations |
| `--desktop-color RRGGBB` | unchanged | Flat desktop colour while the wallpaper is hidden |

**Hidden development / test switches** (not in `--help`; they replace the
former `OV_*` / `OVHOST_*` environment variables, which are no longer read)

| Flag | Meaning |
|---|---|
| `--dry-input` | Log every `SendInput` call instead of injecting it |
| `--fx-dry-run` | Log desktop-effect changes instead of applying them |
| `--mic-synth` | Stream a synthetic 440 Hz tone (conflicts with `--mic` / `--system-audio`) |
| `--test-pattern video\|scroll\|photo` | Stream a synthetic moving screen: 5 s of motion then 2 s of rest (`video` = a 640x360 region), or for `photo`, a window switch every 1.5 s |
| `--lossless` | Never send lossy JPEG tiles |
| `--lossy Q` | JPEG quality (1..100) of lossy image tiles (default 75) |
| `--no-tile-cache` | Disable the tile cache |
| `--direct-advertise A:P[,A:P]` | Offer exactly these direct candidates (network emulators; also disables UPnP) |
| `--no-gpu-encode` | Feed the H.264 encoder system-memory NV12 instead of D3D11 surfaces |
| `--video-bpp <float>` | H.264 area-based bitrate budget (benchmarks) |
| `--no-media` | A/B: disable the unreliable media channel |
| `--no-pacing` | A/B: disable link pacing |
| `--send-file <path>` | Send this file to the viewer straight after the handshake |

### `ovview`

```text
ovview --code <10-digit code> [--relay ADDR]
ovview --id <device_id> [--lan-addr IP:PORT] [--relay-fallback --relay ADDR]   # LAN, PIN as below
ovview --connect <device_id> [--relay ADDR]           # PIN from OPENVIEW_PIN (or --pin)
ovview --info <file.ovrec>
ovview --export <file.ovrec> --out <dir> [--frame N]
ovview --play <file.ovrec> [--loop]
```

**Choose a target** (exactly one of `--connect`, `--code`, `--id`, or an offline
`--info` / `--export` / `--play`):

| Flag | Meaning |
|---|---|
| `--code <code>` | Join a host by its one-time 10-digit pairing code through the relay (spaces / dashes allowed, e.g. `1234 567890`). The first four digits name the relay slot; the rest is the secret, never sent |
| `--id <device_id>` | LAN access without the relay: look the unattended host up on the local network by mDNS (up to 3 s), then run the PIN handshake and the session over direct UDP to its LAN port. Spaces and dashes in the ID are ignored. Needs the PIN like `--connect`. |
| `--lan-addr IP:PORT[,IP:PORT]` | With `--id`: dial these host addresses (its LAN port, default 4571) instead of the mDNS lookup |
| `--relay-fallback` | With `--id`: when nobody answers on the LAN, dial the host through `--relay` like `--connect` (a wrong PIN is not retried) |
| `--connect <device_id>` | Unattended access: dial by the host's numeric device ID and authenticate with its PIN (`OPENVIEW_PIN`, or `--pin <pw>`, which other processes can see) |

**Options**

| Flag | Default | Meaning |
|---|---|---|
| `--relay ADDR` | `127.0.0.1:4569` | Relay address: IP or host name with an optional port (default 4569), or a `ws://` / `wss://` URL |
| `--no-window` | off | Run headless: no window is created (stdin lines are still sent as text) |
| `--no-h264` | H.264 on | Never accept H.264, so the host sends lossless tiles only. Without Media Foundation (Windows N/KN lacking the Media Feature Pack) H.264 is off automatically. |
| `--no-direct` | direct on | Stay on the relay |
| `--renderer d2d\|gdi` | `d2d` | Direct2D high-quality cubic, or GDI with the CPU area/sharpen scaler |
| `--mic` | off | Stream our microphone to the host (full duplex) |
| `--audio` | off | Start with speaker playback on (F2 toggles it) |
| `--no-clipboard` | sync on | Disable clipboard sync |
| `--record-dir <dir>` | `%USERPROFILE%\ovrecord` | Where F6 writes `.ovrec` recordings |
| `-v`, `-vv` / `--log-level FILTER` | `info` (env `OPENVIEW_LOG`) | Logging on stderr, as for `ovhost` |
| `--help`, `-h` / `--version`, `-V` | | Print usage / version |

Received files are saved to `Downloads\OpenView` (never overwriting an
existing file). Apart from recordings, the viewer stores nothing on disk.

**Hidden development / test switches** (not in `--help`)

| Flag | Meaning |
|---|---|
| `--audio-synth` | Stream a synthetic 440 Hz tone instead of the mic |
| `--rtt-log` | Print every keepalive RTT sample with its path (relay or direct) |
| `--ping-ms N` | Keepalive PING interval in ms (>= 10; default 5000) |
| `--duration <secs>` | End the session cleanly after this long (headless runs) |
| `--send-file <path>` | Send this file to the host after the handshake |

**Offline recording tools** (no network; `--info` / `--export` print plain-text reports on stdout):

| Flag | Meaning |
|---|---|
| `--info <file.ovrec>` | Print the header summary and statistics |
| `--export <file.ovrec> --out <dir> [--frame N]` | Write frames as `frame-<seq>.ppm`. Without `--frame`, every frame is written (with a warning above 500 frames). |
| `--play <file.ovrec> [--loop]` | Play the recording at the recorded pace. Close the window or press Ctrl-C to stop. |

**Hotkeys in the viewer window**

| Key | Action |
|---|---|
| F1 | Quality: AUTO → 10 → 30 → 60 fps |
| F2 | Speaker playback on/off |
| M | Mute/unmute |
| H / F3 | HUD on/off |
| F4 | Next host monitor |
| F6 | Start/stop recording |
| F8 | Show/hide the host wallpaper |

Only F8 is kept by the viewer. The other hotkeys (including the letters H and
M) are also forwarded to the host as keystrokes.

### `OpenView.exe` (`ovgui`)

| Flag / variable | Meaning |
|---|---|
| `--minimized` | Start hidden in the tray (used by the sign-in Run key) |
| `--quit` | Tell the running instance to quit (it stops its host and relay) |
| `OPENVIEW_DEV_PORTS=<port>` | Isolated **test mode** (development): relay on `127.0.0.1:<port>`, simulated input, no desktop/UPnP/direct side effects, data in `%LOCALAPPDATA%\OpenView-dev`, separate credentials and Run key; never adopts or stops processes started outside it |
| `OPENVIEW_DATA_DIR` | Data directory (default `%LOCALAPPDATA%\OpenView`, or `<exe dir>\data` when `portable.txt` sits next to `OpenView.exe`) |
| `OPENVIEW_BIN_DIR` | Development: folder with `ovhost.exe`, `ovrelay.exe` and `ovview.exe` (default: the folder of `OpenView.exe`) |
| `SLINT_BACKEND`, `SLINT_DISABLE_D3D11`, `SLINT_USE_SOFTWARE` | Renderer overrides |

---

## 2. Recipes

All commands assume release binaries in `target\release` (for example
`cargo build --release`), or `cargo run -p <crate> -- <args>`.

### 2.1 Quick start: attended session through a relay

```powershell
# Relay (anywhere both sides can reach over TCP)
ovrelay --bind 0.0.0.0:4569

# Host: logs "pairing code NNNNNNNNNN" (stderr) and prints {"event":"pairing_code",...} (stdout)
ovhost --relay 192.168.1.10:4569 --screen

# Viewer: use the code the host printed
ovview --code NNNNNNNNNN --relay 192.168.1.10:4569
```

Without `--screen` the host runs the terminal loop: chat echo, files,
clipboard, and audio playback with `--audio`, but no screen. The pairing code
changes every time `ovhost` starts, and with `--service` it also changes for
every session.

### 2.2 LAN connection (no relay needed)

An **unattended** host (`--unattended`, which the Windows app runs) accepts
sessions from its own network on its **LAN port** (UDP 4571, `--lan-port`)
and announces itself by mDNS / Bonjour as `<device-id>._openview._udp.local`
(TXT `id`, `v`, `name`). A viewer on the same network finds it by device ID
and dials it directly; no relay is involved:

```powershell
$env:OPENVIEW_PIN = "<host PIN>"
ovhost --unattended --screen                       # LAN sessions on UDP 4571 + mDNS
ovview --id 123456789012                            # mDNS lookup, then direct UDP
ovview --id 123456789012 --lan-addr 192.168.1.20:4571   # no mDNS (e.g. multicast filtered)
```

* The handshake is the normal PIN handshake (SPAKE2); `ovview` pins the
  device ID, so another machine answering the mDNS query fails the
  handshake. A wrong PIN counts against the host's attempt limit, per source
  address.
* The Windows app lists the hosts it finds under **Computers on this
  network**; connecting to one (or to its ID from the Connect card) uses the
  LAN path, with the relay as the fallback.
* The iPhone / iPad app lists them under **On this network** (Bonjour).
* The host keeps serving the LAN when its relay is unreachable (it reports
  `session_failed` and retries the relay in the background).
* An attended host (`ovhost` without a PIN) also listens on the LAN port; it
  is not announced (its identity changes every run), so dial its address
  with the pairing code (the iOS app's "Pair with code → LAN").
* Allow UDP 4571 (and the shared mDNS port UDP 5353) inbound for
  `ovhost.exe`; `firewall.cmd add` allows all UDP for the program.
* Turn it off with `--no-lan` (or only the announcement with `--no-mdns`).

**Direct upgrade of a relayed session.** A session that starts on the relay
(or on the LAN path) is upgraded to a direct UDP path when one works: the
host offers candidates (its LAN IPv4 addresses, loopback, the STUN and UPnP
public addresses, a global IPv6 address) on **UDP 4570**, and the viewer
switches to the best working path. Look for `direct: ...` log lines (or
`direct_path` status events on stdout). With `--rtt-log`, each RTT sample
shows `via direct <addr>` or `via relay`.

```powershell
ovrelay --bind 0.0.0.0:4569
ovhost  --relay 192.168.1.10:4569 --screen          # direct path on UDP 4570
ovview  --code NNNNNNNNNN --relay 192.168.1.10:4569 --rtt-log
```

* Allow UDP 4570 inbound on the host's firewall, or choose another port with
  `--direct-port`.
* Force relay-only with `--no-direct` on either side.

### 2.3 Through the internet via a relay

Run `ovrelay` on a publicly reachable machine (TCP port open) and point both
sides at it with `--relay <address>`: an IP or host name with an optional
port (default 4569), e.g. `--relay relay.example.com`. Host names are
resolved and every returned address is tried. The direct upgrade is still
attempted through UPnP and STUN. If it fails, the session stays on the relay.

### 2.4 WebSocket relay (e.g. behind a Cloudflare tunnel)

`ovrelay` accepts WebSocket clients on the **same port** as raw TCP. A client
sends `GET /relay` with an upgrade; the relay protocol bytes then travel
inside binary messages, and WebSocket and TCP clients pair with each other.
This lets an HTTP-only front such as a Cloudflare tunnel expose the relay as
`wss://<host>/relay`.

```powershell
ovrelay --bind 127.0.0.1:4569      # put the HTTPS/WSS front (tunnel) in front of this port
```

* `ovhost` and `ovview` dial it with `--relay wss://<host>/relay` (TLS by
  the Windows TLS stack, SChannel; `ws://` for a plain-HTTP front). The
  WebSocket runs on a helper thread inside the process; everything else
  behaves as with a TCP relay.
* The **iOS app** dials over WebSocket too (`RelayWebSocket.swift`, using
  `RelayTunnelCarrier::connect_bridge`).
* A host on the relay machine can keep using the raw TCP address
  (`127.0.0.1:4569`); WebSocket and TCP clients pair with each other.
* A peer that arrives over WebSocket has no relay-observed address, so the
  direct UDP upgrade relies on the other candidates (local, UPnP, STUN).
* The relay sends WebSocket PINGs to keep proxies and tunnels open.

### 2.5 Unattended PIN host (always reachable)

```powershell
ovrelay --bind 0.0.0.0:4569
$env:OPENVIEW_PIN = "my-secret"
ovhost --service --relay 127.0.0.1:4569 --fps 30 --screen
# logs: device id 123456789012 (unattended); stdout: {"event":"device_id","id":"123456789012"}

# From another computer:
$env:OPENVIEW_PIN = "my-secret"
ovview --connect 123456789012 --relay <relay-ip>:4569
```

* A PIN implies `--unattended`. `--pin "my-secret"` also works, but the
  command line is visible to other local users (process list); the
  environment variable is not.
* The device ID is stable because the identity is persisted in
  `%LOCALAPPDATA%\OpenView\ovhost-unattended.id` (or `--data-dir`).
  Deleting that file creates a new ID. A file left next to `ovhost.exe` by an
  older release is copied there once, so the ID survives the upgrade.
* `--service` re-parks forever. Idle relay timeouts (every ~30 s with no
  viewer) are normal and do not trigger the backoff.
* For a real Ctrl+Alt+Del (SAS), ovhost must run in session 0 (as a service)
  with the policy `SoftwareSASGeneration` = 1 or 3. Otherwise the keystrokes
  are injected, and Winlogon ignores them.
* Easiest setup: use `OpenView.exe` (§3), or start the processes yourself
  (§4).

### 2.6 Recording and playback

```powershell
ovview --code NNNNNNNNNN --relay 1.2.3.4:4569 --record-dir C:\rec   # press F6 to start/stop
ovview --info   C:\rec\ovview-20261112-101500-123.ovrec
ovview --export C:\rec\ovview-....ovrec --out C:\frames               # all frames
ovview --export C:\rec\ovview-....ovrec --out C:\frames --frame 42    # one frame
ovview --play   C:\rec\ovview-....ovrec --loop
```

* Recordings hold video frames only: BGRA, DEFLATE, with timestamps.
* The file name timestamp is UTC.
* The HUD shows `REC ● <s>s <MiB>M <frames>f` while recording.

### 2.7 Audio

| Goal | Host | Viewer |
|---|---|---|
| Hear the host's speakers (system sound) | `--system-audio` | `--audio`, or press F2 |
| Hear the host's microphone | `--mic` | `--audio`, or F2 |
| Talk to the host (viewer mic → host speakers) | `--audio` | `--mic` |
| Hardware-free test | `--mic-synth` | `--audio-synth` / `--audio` |

* Codec: Opus is used when the peer advertises it, otherwise raw PCM. A
  modern host sends 10 ms Opus packets with FEC to the viewer.
* The viewer's playback starts **off**: press F2 or pass `--audio`. M mutes.
* Host speaker playback (`--audio`) starts immediately.
* If a device cannot be opened, the session continues without audio (look for
  `mic unavailable` or `system audio unavailable`).

### 2.8 Clipboard

Clipboard sync is on by default in both directions, for **text only**. Turn
it off on either side with `--no-clipboard`. A failed local write is logged
as `clipboard write failed (peer text dropped)`.

### 2.9 File transfer

```powershell
ovhost --relay 1.2.3.4:4569 --screen --send-file C:\notes.txt     # host -> viewer
ovview --code NNNNNNNNNN --relay 1.2.3.4:4569 --send-file C:\report.pdf # viewer -> host
```

* `--send-file` is a hidden test switch on both binaries; it sends one file
  right after the handshake.
* The receiver (host or viewer) saves it to **`Downloads\OpenView\<sanitized
  name>`** (the user's Downloads known folder), never overwriting an existing
  file (`name (2).ext`, ...), and logs `received `name` -> path (N bytes)`.

### 2.10 Testing without touching the desktop

```powershell
ovhost --relay 127.0.0.1:4569 --dry-input --fx-dry-run -vv --test-pattern video --screen
cargo run -p ovview --example input_smoke -- 127.0.0.1:4569 NNNNNN
```

The `input_smoke` example sends a scripted input-v2 sequence. Run the host
with `--dry-input -vv` (per-input-event lines are trace level) to watch it without anything being injected. For
an end-to-end test across three processes, see `scripts/e2e_relay.sh`
([BUILDING.md](BUILDING.md#tests)).

---

## 3. `OpenView.exe` (GUI)

1. `ovhost.exe`, `ovrelay.exe` and `ovview.exe` must sit in the same folder
   as `OpenView.exe` (`OPENVIEW_BIN_DIR` overrides this for development).
   A missing binary shows a clear error ("... is missing. It must be in the
   same folder as OpenView.exe").
2. Enter the relay in **Settings → Relay** (the first card). **Relay
   address** is empty by default and must be entered before connecting to
   another computer: an IP or host name with an optional port (default
   4569), or a `ws://` / `wss://` URL (the field is validated with the same
   parser `ovhost` / `ovview` use); a notice shows while it is missing, and the Remote page
   links to Settings. **Run a relay on this PC** (default on) runs the
   bundled `ovrelay --bind 0.0.0.0:4569` (or uses an existing relay on that
   port) and parks this PC's host on `127.0.0.1:4569`; when off, the host
   parks on the relay address instead.
3. On start, the GUI runs `ovhost --unattended --service --relay <relay>
   --data-dir <data dir> --fps 30 ... --screen` with the PIN in the
   `OPENVIEW_PIN` environment variable (never on the command line), and shows
   **YOUR ID** (grouped in threes) and the **PIN**. It follows the host
   through its JSON status events on stdout.

   The PIN is generated on first run and kept in Windows Credential Manager.
   Change it with **Change**; saving restarts the host and ends any open
   session.
4. To control another PC, enter its **Partner ID** and **PIN** under "Connect
   to a computer". This launches `ovview --connect <id> --relay <relay
   address>`, again with the PIN in `OPENVIEW_PIN`.
5. Closing the window hides it to the tray, and the PC stays reachable. Quit
   from the tray menu or with `OpenView.exe --quit`.
6. **Start OpenView at sign-in** writes the HKCU `Run` value `OpenView` =
   `"<exe>" --minimized`.
7. If an `ovhost.exe` started outside the GUI (§4) runs from the install
   folder, the GUI shows "Another host is running" with **Stop it and take
   over**, which stops it and starts the GUI's own host with the GUI's PIN.
8. Logs: **Settings → Troubleshooting → Open logs folder**, which is
   `%LOCALAPPDATA%\OpenView\logs`: `openview.log` (GUI, including status
   changes), `ovhost.log` / `ovrelay.log` (their stderr), `ovview-<id>.log`
   and `ovview-<id>.status.jsonl` (a viewer's stderr and stdout status
   events), `panics.log`.
9. **About** shows Slint's "Made with Slint" panel, as required by the Slint
   royalty-free license.

Data lives in `%LOCALAPPDATA%\OpenView` (or `<exe dir>\data` with
`portable.txt` next to `OpenView.exe`; `OPENVIEW_DATA_DIR` overrides).
Settings from older releases in `%APPDATA%\OpenView` are copied over once.

Safe testing (a development feature): set `OPENVIEW_DEV_PORTS=14600` (any
port). Everything then runs on loopback with simulated input and separate
data (`%LOCALAPPDATA%\OpenView-dev`), and processes started outside this
instance are never adopted or stopped, so the real install is not touched.

**Firewall.** The host connects **out** to the relay over TCP. Inbound it
uses **UDP 4570** for the direct path (`--direct-port`; UPnP maps it unless
`--no-upnp`), **UDP 4571** for LAN sessions (`--lan-port`) and the shared
mDNS port **UDP 5353** on the local network. The bundled relay listens on **TCP 4569** on all
interfaces (`127.0.0.1:<port>` in test mode); allow it inbound when other
devices use this PC's relay.

---

## 4. Running the host without the GUI

To keep a PC reachable without `OpenView.exe`, start the relay and an
unattended host yourself (for example from a logon script or task):

```powershell
$env:OPENVIEW_PIN = "<your-pin>"
ovrelay.exe --bind 0.0.0.0:4569
ovhost.exe --service --fps 30 --audio --system-audio --screen
```

Things to note:

* Choose a strong PIN; anyone who can reach the relay and knows the PIN can
  control the PC. `--pin <your-pin>` also works but is visible to other
  processes.
* The host uses the default relay `127.0.0.1:4569` (no `--relay`), which
  matches the relay above.
* If this host runs from the OpenView install folder, `OpenView.exe` shows
  it as "Another host is running"; **Stop it and take over** replaces it
  with the GUI's own host (the GUI does not read its PIN).
* To stop, end `ovhost.exe` and `ovrelay.exe`, or use the GUI's take-over.
  Ending the host with Ctrl-Break or a console close restores desktop
  effects. A hard kill is repaired at the next start (see [TROUBLESHOOTING.md](TROUBLESHOOTING.md)).

---

## 5. Troubleshooting

Error messages, their causes and fixes, and where the logs are:
[TROUBLESHOOTING.md](TROUBLESHOOTING.md).
