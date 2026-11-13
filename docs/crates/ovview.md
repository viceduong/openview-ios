# `ovview` — the OpenView desktop viewer

`ovview` connects to an `ovhost`, shows the remote screen in a native Win32
window, forwards local mouse and keyboard input, plays and captures audio,
syncs the clipboard, and can record sessions and play them back. Everything
below comes from `crates/ovview/`. Paths are relative to the repository root.

| File | Role |
|---|---|
| `crates/ovview/src/main.rs` | Entry point: logging setup, dispatch of the parsed command line |
| `crates/ovview/src/cli.rs` | `clap` options (`--relay` parses into `ovcommon::relay_addr::RelayAddr`) and their translation into what to run |
| `crates/ovview/src/connect.rs` | The three dial paths (`run_relay`, `run_unattended`, `lan_connect` with the optional relay fallback), `dial_relay` / `dial` (TCP or WebSocket relay), `handshake_over` |
| `crates/ovview/src/connect/tests.rs` | Relay dialing and handshake tests (in-process relay) |
| `crates/ovview/src/session/mod.rs` | `viewer_loop`: the post-handshake session, liveness, clipboard, frame presentation, waits, `finish` |
| `crates/ovview/src/session/hooks.rs` | Inbound command hook: monitors, audio (with loss concealment), clipboard, cursor, chat |
| `crates/ovview/src/session/input.rs` | Local input → remote commands (input v2 or legacy), stdin lines as `TYPE` |
| `crates/ovview/src/session/keys.rs` | Local hotkeys (F1–F8, H, M), recording toggle |
| `crates/ovview/src/session/stats.rs` | End-of-session summary, `wave2:` and `metrics:` lines |
| `crates/ovview/src/window/mod.rs` | `RenderWindow`: a plain Win32 window through the `windows` crate, frame / cursor / HUD state |
| `crates/ovview/src/window/wndproc.rs` | Window class and creation, message pump, the `WndProc` that paints and queues input |
| `crates/ovview/src/window/paint.rs` | `WM_PAINT`: Direct2D, or the GDI `StretchDIBits` fallback (CPU-scaled downscale, `SRCCOPY`) |
| `crates/ovview/src/window/overlay.rs` | GDI HUD overlay and dead-peer banner (`TextOutW`) |
| `crates/ovview/src/window/cursor.rs` (+ `cursor/tests.rs`) | GDI-path cursor compositing into a cached frame copy (pure) |
| `crates/ovview/src/window/d2d.rs` (+ `d2d/tests.rs`) | Direct2D presenter: flip-model swap chain, high-quality cubic scaling, cursor layer, DirectWrite HUD |
| `crates/ovview/src/localinput.rs` (+ `localinput/tests.rs`) | Pure math: client pixels → letterboxed content rect → 0..=65535; the `LocalEvent` type |
| `crates/ovview/src/hud.rs` (+ `hud/tests.rs`) | HUD line formatting (pure) |
| `crates/ovview/src/audio.rs` | Speaker playback thread over a jitter buffer |
| `crates/ovview/src/mic.rs` | Microphone capture (re-exports the shared `ovapp` sources) |
| `crates/ovview/src/clipboard_os.rs` | Win32 clipboard I/O |
| `crates/ovview/src/record.rs` | F6 recorder and the offline `--info`, `--export`, `--play` tools |
| `crates/ovview/src/ctrlc.rs` | Ctrl-C handler (`SetConsoleCtrlHandler`) raising a process-wide flag |
| `crates/ovview/examples/input_smoke.rs` | Scripted input-v2 test client |
| `crates/ovview/build.rs` | Includes `assets/winres.rs`: icon (resource ID 1 = `APP_ICON_ID`, used as the window icon), application manifest (asInvoker, Windows 10/11, PerMonitorV2 DPI) and VERSIONINFO; links `/DELAYLOAD:mfplat.dll` + `delayimp.lib` |

The manifest description reads: *"OpenView desktop viewer/terminal client:
connect by pairing code (relay) or fingerprint (LAN mDNS)"*. Its dependencies
are `ovcore`, `ovtransport` (feature `ws-client`), `ovapp` (with the `opus`
and `mf` features), `ovdiscovery`, `ovcommon` (feature `logging`), `hex`,
`clap`, `tracing`, and, on Windows only, `windows` for the window, GDI,
Direct2D/D3D11/DirectWrite/DXGI, clipboard and console. `ovrelay` is a
dev-dependency (in-process relay for the unit tests).

For every flag, see [`../USAGE.md`](../USAGE.md#ovview).

---

## 1. Connecting

`main()` parses the flags with `clap` (`Cli` in `cli.rs`; `--help`, `--version`;
development switches are `hide = true`) and calls
`ovcommon::logging::init` (stderr; `-v`, `-vv`, `--log-level`,
`OPENVIEW_LOG`). There is no runtime DPI call: the embedded manifest declares
PerMonitorV2, so the window works in physical pixels. `clap` requires exactly
one of `--connect`, `--code`, `--id`, `--info`, `--export`, `--play`. The
offline modes `--info`, `--export` and `--play` return before any network or
identity code runs (`--info` / `--export` print plain-text reports on
stdout). Otherwise `connect::run` dials one of these targets:

| Target | Flags | Path |
|---|---|---|
| `Relay` | `--code <code>` | `run_relay`: relay session ID = the code's first 4 bytes as a big-endian `u32`; handshake `Auth::PairingCode(code)` |
| `Unattended` | `--connect <device_id>` + PIN (`OPENVIEW_PIN` or `--pin`) | `run_unattended`: session ID = the low 32 bits of the device ID; `Auth::Pin(pin_psk(pin))`; `hs.expect_device(device_id)` pins the host's device ID |
| `Lan` | `--id <device_id>` + PIN, optional `--lan-addr IP:PORT[,…]`, `--relay-fallback` | `lan_connect`: `ovdiscovery::mdns::resolve(id, 3 s)` (skipped with `--lan-addr`), then for each address in order a `UdpP2pCarrier` and the client handshake `Auth::Pin(pin_psk(pin))` with `hs.expect_device(device_id)`, 4 s per address; the session runs on the first address that completes |

`--connect` / `--id` without a PIN fail with `--connect needs the host's PIN
(OPENVIEW_PIN or --pin)` (or `--id needs ...`). `--pin` is visible to other processes; the GUI
always passes the PIN in `OPENVIEW_PIN`.

The relay address is `--relay <ADDR>`, default `127.0.0.1:4569` (no
environment variable is read). `clap` parses it into
`ovcommon::relay_addr::RelayAddr`, so it accepts an IP, `hostname[:port]`
(default port 4569, bare or bracketed IPv6 too) or a `ws://` / `wss://` URL
(see [ovcommon.md](ovcommon.md) §3).

### Relay dialing

`dial()` (`connect.rs`) opens the relay carrier: a `Tcp` address goes to
`RelayTunnelCarrier::connect_host` (hostnames resolved, every address tried),
a `WebSocket` address to `RelayTunnelCarrier::connect_ws` (TLS for `wss://`
through SChannel; see [ovtransport.md](ovtransport.md) §6.4).
`dial_relay()` connects as the tunnel `Initiator` and waits up to
`ATTACH_WAIT` = **35 s** (the relay's 30 s park window plus slack) for the host
to attach. A host that is between parks therefore still gets attached.

| Error | Meaning |
|---|---|
| `host did not attach within 35s` | The host never attached in the window |
| `host not reachable (relay park timed out with no host)` | The relay timed the park out (`XT`) |
| `relay <addr>: connect failed: ...` | The relay could not be reached (TCP connect, name resolution, TLS or WebSocket upgrade) |
| `relay attach failed: ...` | Any other error while waiting for the attach |

`handshake_over()` then pumps the handshake for up to 3 s, polling every
10 ms. Datagrams that arrive in the same batch as the final handshake message
are kept as a backlog for the session.

### LAN (`--id`)

The host's LAN session port (UDP 4571 by default, the SRV port of its mDNS
record) accepts a HELLO from any viewer on its network (see
[ovhost.md](ovhost.md) §1). `lan_connect` emits a `connecting` status event
per address (`relay` = `lan <addr>`). Outcomes:

| Result | Meaning |
|---|---|
| `device N was not found on this network; dial it through the relay instead (--connect N)` | No mDNS answer within 3 s |
| `LAN <addr>: handshake timed out` (last address) | No address answered (firewall, wrong port, host busy with another session) |
| `handshake failed: authentication failed ...` / rate limited / incompatible version | The host answered and refused; no further address is tried and `--relay-fallback` does not apply |

With `--relay-fallback`, the two "nobody answered" cases continue as
`--connect <id>` through `--relay`. The session itself is the same as a
relayed one, including the direct-path upgrade (§6): the LAN carrier plays
the relay's part.

---

## 2. The viewer loop (`viewer_loop` in `session/mod.rs`)

After the handshake:

1. It builds a `TerminalSession` and queues the handshake backlog.
2. **Direct path**: unless `--no-direct` is set, it binds
   `DirectSockets::bind(0)` and calls `term.enable_direct(..)`. The host offers
   candidates and the relay stays the fallback.
3. **H.264** (Windows): unless `--no-h264` is set, it installs
   `ovapp::mfcodec::H264Decoder`, which adds `CAP_H264` to the CAPS. On
   failure, including a missing Media Foundation (`mfcodec::available()`
   false on Windows N/KN without the Media Feature Pack; `mfplat.dll` is
   delay-loaded so the exe still starts), it logs `no H.264 decoder (...);
   lossless tiles only`.
4. `announce_caps()`: advertises tile/zstd decoding, keyframe requests and the
   other capabilities.
5. Optional (hidden) `--send-file`.
6. Installs the Ctrl-C handler (`SetConsoleCtrlHandler`, which sets a flag).
7. Creates the render window (`open_window` → `RenderWindow::new`) unless
   `--no-window` is set (then no window exists at all). If creation fails, the
   viewer warns and continues headless. The renderer is chosen by
   `--renderer d2d|gdi`.
8. Starts the speaker thread. It always runs, so F2 can enable playback later.
   It also starts the mic thread if `--mic` or `--audio-synth` was given.
9. Installs the command hook (`session/hooks.rs`). The hook handles `MONITORS_INFO`, `AUDIO` (with
   loss concealment), `MEDIA_GAP`, `CLIPBOARD`, `CURSOR` and `CURSOR_SHAPE`.

Each loop turn does the following, in order:

1. Pump window messages.
2. Turn queued local input into commands (§4).
3. `term.pump()`, which reassembles frames and auto-answers PINGs.
4. Liveness and keepalive. The PING interval is `KEEPALIVE_INTERVAL` = 5 s, or
   `--ping-ms`. RTT comes from `take_pong`, and `--rtt-log` prints each sample
   with its path (`relay` or `direct <addr>`).
5. Send mic ticks.
6. Poll the clipboard.
7. Save received files to `Downloads\OpenView` (§6).
8. Present the **latest** complete frame. Intermediate frames are skipped, so
   a slow paint never falls behind.
9. Update the HUD.
10. Check the exit conditions.
11. Block in `Wakeup::wait_or_message`, which wakes on socket data, a window
    message, a mic tick or a deadline. The deadline is capped at
    `IDLE_WAIT` = 50 ms, or `UNWATCHED_WAIT` = 2 ms if the socket cannot be
    watched, and at 4 ms while a vsync-refused present is pending.

**Liveness**: the peer is "dead" after `DEAD_AFTER` = 15 s with no inbound
traffic. The frame is then no longer updated and the HUD shows a banner. After
`SESSION_TIMEOUT` = 30 s of silence the session ends.

**Exit** happens on any of these:

* the window is closed;
* Ctrl-C;
* the hidden `--duration <secs>` elapses;
* the peer is silent for 30 s;
* a transport error.

`finish()` sends `RELEASE_ALL` (to an input-v2 host), drains for 500 ms, and
closes. Before that the viewer logs, at `debug` (`-v`, `session/stats.rs`),
and emits a `peer_disconnected` status event:

* a **session summary**: frames, resolution, commands received, KB/s,
  desyncs, keyframe requests, peer CAPS, last RTT;
* a `wave2:` line (lossy tiles on screen, tile-cache slots);
* an `H.264:` line (units, decode time, capture-to-paint latency);
* a single `metrics:` line that benchmarks parse: video latency
  p50/p95/p99, stalls, RTT p50/p95, audio gaps, receive Mbps, media summary;
* audio receive and playback statistics.

---

## 3. Window and rendering (`window/`)

* The window is a plain top-level Win32 window (`WS_OVERLAPPEDWINDOW`),
  bound through the `windows` crate with no GUI framework (`window/wndproc.rs`).
  Its title is always **"OpenView — Remote"**, including in playback.
* Window state is an `Arc<WindowState>` stored in `GWLP_USERDATA`. The
  `WndProc` queues input events, paints on `WM_PAINT`, and turns `WM_CLOSE`
  into a quit flag.
* **Letterboxing**: `localinput::letterbox_rect` keeps the frame's aspect
  ratio inside the client area. The black bars are not part of the remote
  screen.

### Presenters

| `RendererPref` | How |
|---|---|
| `Direct2D` (default) | A D3D11 device (hardware, then WARP) with a flip-model DXGI swap chain. The frame is drawn with `D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC`. The remote cursor is a separate premultiplied layer, not baked into the frame. The HUD is drawn with DirectWrite. `Present(1, DXGI_PRESENT_DO_NOT_WAIT)`: a refused present is retried on the next loop turn (`repaint_pending`). **Any failure** (no device, device removed) switches the window to GDI permanently. |
| `Gdi` | `StretchDIBits` with `SRCCOPY` (`window/paint.rs`; the unit test `gdi_blit_copies_pixels` checks the blit really copies pixels). A real downscale first goes through the CPU scaler `ovapp::scale` (area + sharpen 0.5) and is then blitted 1:1. Other cases use `COLORONCOLOR`, or `HALFTONE` for a strong downscale. On this path the cursor is composited into a cached copy of the frame: a cursor move restores only the old cursor rect and blends the new one (`window/cursor.rs`). The HUD is drawn with `TextOutW` (`window/overlay.rs`). |

The active renderer is logged as `renderer: <name>`. `ovgui` decides that a
connection succeeded from the `peer_connected` status event on stdout, and
reports the `error` event's message when the viewer exits before that.

### HUD (`hud.rs`)

The HUD has six lines, toggled with **H** or **F3**:

* `RTT <ms>`
* `FPS <n>`
* `RES <w>x<h>`
* `BW <MB/s>`
* `Q AUTO|<n> fps`
* `MON <i>/<n> <w>x<h>`, replaced by `REC ● <s>s <MiB>M <frames>f` while
  recording.

FPS and bandwidth are measured over a 2 s window (`STATS_WINDOW`). When the
peer is dead, a red banner is shown.

---

## 4. Input capture and forwarding

`WndProc` (`window/wndproc.rs`) queues `LocalEvent`s:

* `WM_MOUSEMOVE`, plus button down/up for left, right, middle, X1 and X2. The
  window calls `SetCapture` while any button is held, so the release still
  arrives if it happens outside the window.
* `WM_MOUSEWHEEL` and `WM_MOUSEHWHEEL`, as signed 1/120 deltas.
* `WM_KEYDOWN/UP` **and `WM_SYSKEYDOWN/UP`**, so Alt combinations (Alt+Tab
  inside the remote, Alt+F4, F10) go to the host. Each event carries the VK,
  the set-1 scancode and the extended bit.

The viewer loop maps these into commands (`session/input.rs`):

* **Pointer**: a press is sent only if it starts inside the content rect
  (`client_to_normalized`). While a button is held, moves and the release are
  **clamped** to the rect instead of dropped (`client_to_normalized_clamped`),
  so a lost button-up cannot leave a button stuck.
* **Input v2** is used if the host advertised `CAP_INPUT2`:
  * `POINTER_EX` with `MoveAbs`, or `Down`/`Up` plus `POINTER_HAS_POS`;
  * `WHEEL`;
  * `KEY_EX` (VK + scan + `KEY_DOWN`/`KEY_EXTENDED`).

  For an older host, the viewer falls back to `POINTER` (moves and the left
  button only), `KEY` (VK only), and no wheel.
* **Session end**: `RELEASE_ALL` is sent so nothing stays held on the host.

### Hotkeys

Hotkeys are checked in `session/keys.rs`:

| Key | Action |
|---|---|
| **F1** | Cycle quality AUTO → 10 → 30 → 60 fps → AUTO (sends `QUALITY_SET`, where 0 means AUTO) |
| **F2** | Toggle speaker playback |
| **M** | Mute or unmute. Incoming ticks are dropped and the device keeps running. |
| **H** / **F3** | Toggle the HUD |
| **F4** | Next monitor (`CAPTURE_REGION` from `MONITORS_INFO`; needs at least 2 monitors) |
| **F6** | Start or stop recording to `.ovrec` |
| **F8** | Toggle the remote wallpaper (`SESSION_OPTION OPT_HIDE_WALLPAPER`; only if the host has `CAP_DESKTOP_FX`) |

> **Behaviour to be aware of** (from the code): only **F8** is consumed by the
> viewer. F1, F2, F3, F4, F6, **H** and **M** are handled locally *and still
> sent to the host as keystrokes*. Typing "h" or "m" into a remote
> application therefore also toggles the HUD or mute.

### stdin

A background thread (`spawn_stdin_reader`) reads stdin lines.
`handle_local_input` forwards each line as `TYPE` (chat) text, split if it is
very long; `exit` or `quit` closes the session.

### `--no-window`

`--no-window` creates no window at all; the session runs headless and stdin
lines are still sent as text.

---

## 5. Audio

| Part | Details |
|---|---|
| Playback (`audio.rs`) | `AudioBus::with_ms(30, 90)` (`AUDIO_PREFILL_MS`, `AUDIO_CEILING_MS`): a jitter buffer with a 30 ms prefill and a 90 ms ceiling, sized for the 10 ms Opus packets the host sends to a `CAP_AUDIO10` viewer. Drained into `ovapp::winaudio::WinmmSpeaker` (three rotating 20 ms `waveOut` headers refilled on the completion event). The thread always runs; playback starts **off** unless `--audio` is given, and F2 toggles it. |
| Decode | `ovapp::audio::AudioDecoder`: Opus or raw PCM. When the media channel reports a `MEDIA_GAP` (`GAP_AUDIO`), the lost packets are concealed with Opus PLC, and the last lost packet is rebuilt from the in-band FEC of the next packet. |
| Gap metric | An inter-arrival gap longer than `AUDIO_GAP_US` = 40 ms counts as an audible gap in the `metrics:` line |
| Mic (`mic.rs`) | `--mic`: WinMM `waveIn`, 48 kHz mono, 20 ms ticks, event-driven ring of `MIC_BUFFERS` buffers. `--audio-synth`: a 440 Hz tone. Encoded as Opus for a `CAP_OPUS` host (raw otherwise) and sent with `send_audio`, over the media channel when it is open. If the mic fails to open, capture is disabled with a log line. |

Audio is the host's microphone, the host's system loopback, or a synthetic
tone, depending on the host's flags (see `ovhost.md` §7). The host plays the
viewer's mic only when it runs with `--audio`.

---

## 6. Clipboard, files, direct path

* **Clipboard**: on by default; `--no-clipboard` disables it. The viewer polls
  the OS sequence number every loop turn and uses the `ClipboardSync` echo
  guard. Received text is written to the local clipboard.
* **Files**: the hidden `--send-file <path>` sends one file after the
  handshake. Received files are saved to `Downloads\OpenView\<sanitized>`
  (`ovcommon::paths::received_files_dir`), never overwriting an existing file
  (`name (2).ext`). Apart from recordings, the viewer stores nothing on disk.
* **Direct path**: events are logged as `direct: <event>` and emitted as
  `direct_path` status events (`active` / `fallback`).
  `--no-direct` keeps the session on the relay.

---

## 7. Recording and playback (`.ovrec`)

* **F6** toggles recording. The default directory is
  `%USERPROFILE%\ovrecord` (or `<temp>\ovrecord`); `--record-dir <dir>`
  overrides it.
  * File name: `ovview-YYYYMMDD-HHMMSS-mmm.ovrec`. Despite the "local time"
    comment, the timestamp is computed from Unix time with no timezone offset,
    so it is UTC.
  * Header: codec `CODEC_DEFLATE`, BGRA, initial size, quality mode, and the
    peer fingerprint hex stored as `host_name`. `app_version` is hard-coded to
    `"0.23.0"`.
  * Only presented **frames** are recorded; audio is not.
  * The file is finalised when recording is toggled off, and on every loop
    exit.
* `--info <file>`: validates the file and prints the header and statistics.
* `--export <file> --out <dir> [--frame N]`: writes `frame-<seq>.ppm`. Without
  `--frame`, every frame is exported, with a warning above 500 frames
  (`EXPORT_WARN_FRAMES`).
* `--play <file> [--loop]`: plays at the recorded pace (from
  `ovapp::image::frame_pacing`) through the same present path. It stops on
  window close or Ctrl-C.
  * The startup message says "Escape or Ctrl-C stops", but no Escape handling
    exists in the code.
  * The playback window title is still "OpenView — Remote".

---

## 8. `examples/input_smoke.rs`

This is an input-v2 smoke client. Usage:

```text
cargo run -p ovview --example input_smoke -- 127.0.0.1:PORT CODE
```

It dials the relay by pairing code (10 s attach wait), handshakes, waits for
the host's CAPS, then sends a scripted sequence and exits:

* right click;
* wheel;
* Unicode text;
* relative moves;
* a held key followed by `RELEASE_ALL`;
* SAS.

Run the host with `--dry-input -v` so nothing is really injected.

---

## 9. Output and environment

stdout carries JSON-lines status events (`connecting`, `peer_connected`,
`peer_disconnected`, `direct_path`, `error`; schema in
[ovcommon.md](ovcommon.md) §4), except for the plain-text reports of
`--info` / `--export`. Human logs go to stderr; at `info` only lifecycle
lines appear, the session summary / metrics need `-v`, per-event lines `-vv`.

| Variable | Effect |
|---|---|
| `OPENVIEW_PIN` | The PIN for `--connect` (preferred over `--pin`) |
| `OPENVIEW_LOG` | Default log filter (overridden by `-v` / `--log-level`) |
| `USERPROFILE` | Base of the default recording directory |

The former `OV_RELAY`, `OVVIEW_NO_H264` and `OVVIEW_RENDERER` variables are
no longer read; use `--relay`, `--no-h264`, `--renderer`.

## 10. Constants

| Constant | Value | File |
|---|---|---|
| `DEFAULT_RELAY` | `127.0.0.1:4569` | `cli.rs` |
| `ATTACH_WAIT` | 35 s | `connect.rs` |
| `IDLE_WAIT` | 50 ms | `session/mod.rs` |
| `UNWATCHED_WAIT` | 2 ms | `session/mod.rs` |
| `REPAINT_RETRY_WAIT` | 4 ms | `session/mod.rs` |
| `AUDIO_PREFILL_MS` / `AUDIO_CEILING_MS` | 30 / 90 ms | `session/mod.rs` |
| `AUDIO_GAP_US` | 40 000 µs | `session/hooks.rs` |
| `EXPORT_WARN_FRAMES` | 500 | `record.rs` |
| `QUALITY_STEPS` | AUTO, 10, 30, 60 | `session/keys.rs` |
| `STATS_WINDOW` | 2 s | `hud.rs` |
| `--ping-ms` minimum | 10 ms | `cli.rs` |
