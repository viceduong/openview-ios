# `ovview` — the OpenView desktop viewer

`ovview` connects to an `ovhost`, shows the remote screen in a native Win32
window, forwards local mouse and keyboard input, plays and captures audio,
syncs the clipboard, and can record sessions and play them back. Everything
below comes from `crates/ovview/`. Paths are relative to the repository root.

| File | Role |
|---|---|
| `crates/ovview/src/main.rs` | Flag parsing, the three connect paths, the viewer loop, hotkeys, recording/export/playback, session summary |
| `crates/ovview/src/window.rs` | `RenderWindow`: the raw-FFI Win32 window, GDI presentation, cursor compositing, HUD drawing, input queueing |
| `crates/ovview/src/window/d2d.rs` | Direct2D presenter: flip-model swap chain, high-quality cubic scaling, cursor layer, DirectWrite HUD |
| `crates/ovview/src/localinput.rs` | Pure math: client pixels → letterboxed content rect → 0..=65535; the `LocalEvent` type |
| `crates/ovview/src/hud.rs` | HUD line formatting (pure) |
| `crates/ovview/src/audio.rs` | Speaker playback thread over a jitter buffer |
| `crates/ovview/src/mic.rs` | Microphone capture (re-exports the shared `ovapp` sources) |
| `crates/ovview/src/clipboard_os.rs` | Win32 clipboard I/O |
| `crates/ovview/examples/input_smoke.rs` | Scripted input-v2 test client |
| `crates/ovview/build.rs` | Includes `assets/winres.rs`: icon (resource ID 1 = `APP_ICON_ID`, used as the window icon), application manifest (asInvoker, Windows 10/11, PerMonitorV2 DPI) and VERSIONINFO; links `/DELAYLOAD:mfplat.dll` + `delayimp.lib` |

The manifest description reads: *"OpenView desktop viewer/terminal client:
connect by pairing code (relay) or fingerprint (LAN mDNS)"*. Its dependencies
are `ovcore`, `ovtransport`, `ovapp` (with the `opus` and `mf` features),
`ovdiscovery`, `ovrelay`, `ovcommon` (feature `logging`), `clap`, `tracing`,
and, on Windows only, `windows` 0.58 for
Direct2D/D3D11/DirectWrite/DXGI.

For every flag, see [`../USAGE.md`](../USAGE.md#ovview).

---

## 1. Connecting

`main()` parses the flags with `clap` (`Cli`; `--help`, `--version`;
development switches are `hide = true`) and calls
`ovcommon::logging::init` (stderr; `-v`, `-vv`, `--log-level`,
`OPENVIEW_LOG`). There is no runtime DPI call: the embedded manifest declares
PerMonitorV2, so the window works in physical pixels. `clap` requires exactly
one of `--connect`, `--code`, `--id`, `--info`, `--export`, `--play`. The
offline modes `--info`, `--export` and `--play` return before any network or
identity code runs (`--info` / `--export` print plain-text reports on
stdout). Otherwise `high_res_timers()` runs and the target is one of:

| Target | Flags | Path |
|---|---|---|
| `Relay` | `--code <code>` | `run_relay`: relay session ID = the code's first 4 bytes as a big-endian `u32`; handshake `Auth::PairingCode(code)` |
| `Unattended` | `--connect <device_id>` + PIN (`OPENVIEW_PIN` or `--pin`) | `run_unattended`: session ID = the low 32 bits of the device ID; `Auth::Pin(pin_psk(pin))`; `hs.expect_device(device_id)` pins the host's device ID |
| `Lan` | `--id <32 hex chars>` | `run_lan`: mDNS browse for up to 5 s (`lan_find`), then a direct UDP handshake with `Auth::DirectDevice` and `expect_fingerprint` |

`--connect` without a PIN fails with `--connect needs the host's PIN
(OPENVIEW_PIN or --pin)`. `--pin` is visible to other processes; the GUI
always passes the PIN in `OPENVIEW_PIN`.

The relay address is `--relay`, default `127.0.0.1:4569` (no environment
variable is read). It must parse as a `SocketAddr` (IP:port). `ovview` has
**no WebSocket client**: the relay's WebSocket front (`GET /relay`) is used by
the iOS app, not by `ovview`.

### Relay dialing

`dial_relay()` connects as the tunnel `Initiator` and waits up to
`ATTACH_WAIT` = **35 s** (the relay's 30 s park window plus slack) for the host
to attach. A host that is between parks therefore still gets attached.

| Error | Meaning |
|---|---|
| `host did not attach within 35s` | The host never attached in the window |
| `host not reachable (relay park timed out with no host)` | The relay timed the park out (`XT`) |
| `relay connect failed: ...` | TCP connect to the relay failed |

`handshake_over()` then pumps the handshake: 200 iterations of 10 ms, about
2 s. Datagrams that arrive in the same batch as the final handshake message are
kept as a backlog for the session.

### LAN (`--id`) limitations

These are visible in the code:

* `run_lan` always dials **`127.0.0.1:<udp_port>`** of the discovered peer, so
  as written it only reaches a peer on the same machine.
* `ovhost` does not advertise mDNS (it only prints a name), so `--id` finds a
  peer only if something else advertises one.

For real LAN use, connect through a relay. The session then upgrades itself to
a direct UDP path (§6).

---

## 2. The viewer loop (`viewer_loop` in `main.rs`)

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
7. Creates the render window (`RenderWindow::new`) unless `--no-window` is
   set (then no window exists at all). If creation fails, the viewer warns
   and continues headless. The renderer is chosen by `--renderer d2d|gdi`.
8. Starts the speaker thread. It always runs, so F2 can enable playback later.
   It also starts the mic thread if `--mic` or `--audio-synth` was given.
9. Installs the command hook. The hook handles `MONITORS_INFO`, `AUDIO` (with
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
closes. Before that the viewer logs, at `debug` (`-v`), and emits a
`peer_disconnected` status event:

* a **session summary**: frames, resolution, commands received, KB/s,
  desyncs, keyframe requests, peer CAPS, last RTT;
* a `wave2:` line (lossy tiles on screen, tile-cache slots);
* an `H.264:` line (units, decode time, capture-to-paint latency);
* a single `metrics:` line that benchmarks parse: video latency
  p50/p95/p99, stalls, RTT p50/p95, audio gaps, receive Mbps, media summary;
* audio receive and playback statistics.

---

## 3. Window and rendering (`window.rs`, `window/d2d.rs`)

* The window is a plain top-level Win32 window (`WS_OVERLAPPEDWINDOW`),
  declared with raw `extern "system"` FFI and no GUI framework. Its title is
  always **"OpenView — Remote"**: the `title` argument is ignored, including in
  playback.
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
| `Gdi` | `StretchDIBits`. A real downscale first goes through the CPU scaler `ovapp::scale` (area + sharpen 0.5) and is then blitted 1:1. Other cases use `COLORONCOLOR`, or `HALFTONE` for a strong downscale. On this path the cursor is composited into a cached copy of the frame: a cursor move restores only the old cursor rect and blends the new one (`compose`, `blend_cursor`, `restore_cursor_rect`). |

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

`WndProc` (`window.rs`) queues `LocalEvent`s:

* `WM_MOUSEMOVE`, plus button down/up for left, right, middle, X1 and X2. The
  window calls `SetCapture` while any button is held, so the release still
  arrives if it happens outside the window.
* `WM_MOUSEWHEEL` and `WM_MOUSEHWHEEL`, as signed 1/120 deltas.
* `WM_KEYDOWN/UP` **and `WM_SYSKEYDOWN/UP`**, so Alt combinations (Alt+Tab
  inside the remote, Alt+F4, F10) go to the host. Each event carries the VK,
  the set-1 scancode and the extended bit.

The viewer loop maps these into commands (`main.rs`):

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

Hotkeys are checked in the key handler of `main.rs`:

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

The module documentation says stdin lines are forwarded as `TYPE` (chat)
commands, and that `exit` or `quit` closes the session. In the current
`viewer_loop`, however, the stdin reader thread sends lines into an `mpsc`
channel whose receiver (`_rx`) is never read. Only window events are drained.
**As written, stdin input has no effect**, and the `LocalEvent::Line` branch is
unreachable from stdin.

### `--no-window`

`--no-window` only suppresses the "no window available; running headless"
message. `viewer_loop` still calls `RenderWindow::new` unconditionally, so a
window is still created on Windows.

---

## 5. Audio

| Part | Details |
|---|---|
| Playback (`audio.rs`) | `AudioBus::with_ms(30, 90)`: a jitter buffer with a 30 ms prefill and a 90 ms ceiling (Wave 1, since the host sends 10 ms Opus packets to a `CAP_AUDIO10` viewer). Drained into `ovapp::winaudio::WinmmSpeaker` (three rotating 20 ms `waveOut` headers refilled on the completion event). The thread always runs; playback starts **off** unless `--audio` is given, and F2 toggles it. |
| Decode | `ovapp::audio::AudioDecoder`: Opus or raw PCM. Wave 3: when the media channel reports a `MEDIA_GAP` (`GAP_AUDIO`), the lost packets are concealed with Opus PLC, and the last lost packet is rebuilt from the in-band FEC of the next packet. |
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
[ovcommon.md](ovcommon.md) §3), except for the plain-text reports of
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

| Constant | Value |
|---|---|
| `DEFAULT_RELAY` | `127.0.0.1:4569` |
| `ATTACH_WAIT` | 35 s |
| `IDLE_WAIT` | 50 ms |
| `UNWATCHED_WAIT` | 2 ms |
| `AUDIO_GAP_US` | 40 000 µs |
| `EXPORT_WARN_FRAMES` | 500 |
| `QUALITY_STEPS` | AUTO, 10, 30, 60 |
| `hud::STATS_WINDOW` | 2 s |
| `--ping-ms` minimum | 10 ms |
