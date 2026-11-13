# `ovhost` — the OpenView host (listener service)

`ovhost` is the binary that runs on the computer being controlled. It parks on
an `ovrelay` relay, completes the encrypted handshake with a viewer, then
serves either a **terminal session** (chat echo, files, clipboard, audio
playback) or a **screen session** (capture + encode + input injection +
cursor + audio + clipboard + desktop effects).

Everything below is taken from the source in `crates/ovhost/`. File paths are
relative to the repository root.

| File | Role |
|---|---|
| `crates/ovhost/src/main.rs` | Process lifecycle: logging, data dir, cleanup and desktop-effects recovery, direct-path start, the one-shot or `--service` loop |
| `crates/ovhost/src/cli.rs` | `clap` options (`--relay` parses into `ovcommon::relay_addr::RelayAddr`) |
| `crates/ovhost/src/settings.rs` | Process-wide switches set once from the command line |
| `crates/ovhost/src/session/mod.rs` | `listen_once`: identity, waiting for a viewer (`acquire`: relay park with `dial` for TCP or WebSocket relays, and the LAN listener), handshake, choice of serve loop; clipboard poll |
| `crates/ovhost/src/lan.rs` | `LanListener`: the LAN session port (UDP, default 4571) that takes a viewer's HELLO without the relay |
| `crates/ovhost/src/session/screen.rs` | `serve_screen` / `screen_session_loop`: capture and mic threads, the session thread that owns the wire |
| `crates/ovhost/src/session/capture_loop.rs` (+ `capture_loop/tests.rs`) | The capture thread: grab, tile / H.264 encode, rate and quality control, metrics |
| `crates/ovhost/src/session/terminal.rs` | `serve_terminal`: the terminal-mode loop |
| `crates/ovhost/src/session/hooks.rs` (+ `hooks/tests.rs`) | `HostHooks`: the inbound-command hook shared by both loops (input, CAPS, quality, clipboard, audio, files, chat) |
| `crates/ovhost/src/session/liveness.rs` | Keepalive PONG bookkeeping and the dead / gone verdicts |
| `crates/ovhost/src/session/files.rs` | Saving received files and iOS diagnostic log uploads |
| `crates/ovhost/src/session/fx.rs` | Per-session desktop effects (`SessionFx`) and their exit guarantee |
| `crates/ovhost/src/session/direct.rs` | Direct-path event reporting (log and status events) |
| `crates/ovhost/src/capture.rs` | `ScreenSource`: DXGI-or-GDI capture with fallback, plus the synthetic `--test-pattern` screens |
| `crates/ovhost/src/dxgi.rs` (+ `dxgi/tests.rs`) | DXGI Desktop Duplication backend (Windows only) |
| `crates/ovhost/src/cursor/mod.rs` | Cursor position/shape tracking for the cursor overlay channel |
| `crates/ovhost/src/cursor/shape.rs`, `cursor/win32.rs`, `cursor/tests.rs` | Pure shape composition; Win32 polling and shape rendering; tests |
| `crates/ovhost/src/input/mod.rs` | Wire input commands into `SendInput` (plus a dry-run backend) |
| `crates/ovhost/src/input/geometry.rs`, `input/controller.rs`, `input/backend.rs`, `input/win32.rs`, `input/tests.rs` | Pure coordinate mapping; `InputController`; the backends and `sas_capability`; Win32 calls; tests |
| `crates/ovhost/src/unattended.rs` | The persisted unattended identity and the device ID |
| `crates/ovhost/src/mic.rs` | Microphone capture (re-exports the shared `ovapp` sources) |
| `crates/ovhost/src/sysaudio.rs` | `--system-audio`: WASAPI loopback capture (Windows only) |
| `crates/ovhost/src/speaker.rs` | `--audio`: plays the viewer's microphone |
| `crates/ovhost/src/clipboard_os.rs` | Win32 clipboard read/write and sequence number |
| `crates/ovhost/src/desktop_fx.rs` (+ `desktop_fx/tests.rs`) | Hide wallpaper / disable animations during a session, with crash recovery |
| `crates/ovhost/src/cleanup.rs` | Cleanup actions that run on exit, Ctrl-C, console close or panic |
| `crates/ovhost/src/direct_net.rs` | Direct UDP path: socket, candidate gathering, STUN, UPnP |
| `crates/ovhost/build.rs` | Includes `assets/winres.rs`: embeds the icon, an application manifest (asInvoker, supportedOS Windows 10/11, PerMonitorV2 DPI with the `true/pm` fallback) and VERSIONINFO (CompanyName "OpenView contributors", ProductName "OpenView", FileVersion = `CARGO_PKG_VERSION`); links `/DELAYLOAD:mfplat.dll` + `delayimp.lib` |

The package is described in its manifest as *"OpenView host: makes this PC
reachable through a relay (and a direct UDP path) and streams its screen"*. It
depends on `ovcore`, `ovtransport` (feature `ws-client`: `ws://` / `wss://`
relays), `ovapp` (with the `zstd-encode`, `opus` and `mf` features: C zstd
encoding, libopus and Media Foundation are host-only), `ovdiscovery`,
`ovcommon` (feature `logging`: data dir, relay addresses, status events,
logging), `hex`, `clap`, `tracing` and, on Windows, the `windows` crate for
DXGI/D3D11, IP Helper, WASAPI and COM. `ovrelay` is a dev-dependency used only
by tests (`crates/ovhost/Cargo.toml`).

For every flag, see [`../USAGE.md`](../USAGE.md#ovhost).

---

## 1. Process lifecycle

`main()` and `run()` (`src/main.rs`) run in this order:

1. It parses the command line with `clap` (`Cli`; `--help`, `--version`;
   development switches are `hide = true`). Usage errors exit with code 2.
2. `ovcommon::logging::init(-v count, --log-level)`: `tracing` to stderr
   (see [ovcommon.md](ovcommon.md) §5). There is no runtime DPI call: the
   embedded manifest declares PerMonitorV2, so capture, cursor, monitor and
   input coordinates are physical pixels.
3. It stores the global switches (`--dry-input`, `--gdi`, `--h264`,
   `--test-pattern`, `--lossless`, `--lossy`, `--no-tile-cache`,
   `--max-fps`, `--video-bpp`, `--no-media`, `--no-pacing`,
   `--no-gpu-encode`). If `--h264` is not `off` and
   `ovapp::mfcodec::available()` is false (no `mfplat.dll`: Windows N/KN
   without the Media Feature Pack), it logs one info line and switches H.264
   off; lossless tiles are used. `mfplat.dll` is delay-loaded, so the exe
   starts without it, and no Media Foundation function runs before that
   probe.
4. `ovapp::netwait::high_res_timers()`: 1 ms timer resolution for the packet
   pacer.
5. The PIN comes from `--pin` or the `OPENVIEW_PIN` environment variable
   (clap `env`); an empty PIN counts as none. A PIN implies `--unattended`;
   `--unattended` without one fails with `--unattended needs a PIN
   (OPENVIEW_PIN or --pin)`. The relay is `--relay <ADDR>`, default
   `127.0.0.1:4569` (`DEFAULT_RELAY`); no environment variable is read for
   it. `clap` parses it into `ovcommon::relay_addr::RelayAddr`, so it accepts
   an IP, `hostname[:port]` (default port 4569, bare or bracketed IPv6 too) or
   a `ws://` / `wss://` URL (see [ovcommon.md](ovcommon.md) §3).
6. It creates the data dir (`--data-dir`, else the `ovcommon::paths` rule:
   `%LOCALAPPDATA%\OpenView`, or `<exe dir>\data` in portable mode).
7. `cleanup::install()`, then `DesktopFx::recover_stale()` for the marker in
   the data dir and for a legacy marker next to the exe. This undoes desktop
   effects left behind by a crashed earlier run (see §8).
8. `DirectNet::start()`, unless `--no-direct` is set. It binds the direct UDP
   port, gathers candidates, runs STUN and starts the UPnP thread (see §9).
   If this fails, ovhost warns `direct path disabled (UDP <port>: <err>)`,
   emits a `direct_path` / `disabled` status event and continues on the relay
   only.
9. `start_lan()`, unless `--no-lan` is set: binds the **LAN session port**
   (`--lan-port`, default 4571; `0` = any free port). In unattended mode it
   also starts an `ovdiscovery::mdns::Advertiser` for the process lifetime
   (unless `--no-mdns`): instance `<device-id>._openview._udp.local`, SRV
   port = the bound LAN port, TXT `id`, `v` (handshake version), `name`
   (computer name). Its stop (the mDNS goodbye) is registered with the exit
   cleanup. A `lan` status event reports `port` and `mdns`; if the port
   cannot be bound, LAN sessions are off (`lan` without `port`, with
   `detail`) and the host continues relay-only.
10. It enters one of two loops:
   * **default**: one call to `listen_once()`, then exit. (`--listen` is a
     hidden no-op kept for compatibility.)
   * **`--service`**: calls `listen_once()` forever. After an error it sleeps
     for a backoff that starts at 1 s, doubles each time and is capped at
     30 s (warning `session failed: <e>; retrying in <d>` plus a
     `session_failed` status event). A successful session resets the backoff
     to 1 s.
11. On exit `cleanup::run_all()` deletes the UPnP mapping and sends the mDNS
    goodbye; a fatal error is
    logged and emitted as an `error` status event, and the exit code is
    non-zero.

### Output: status events and logs

stdout carries only JSON-lines status events (`ovcommon::status`, schema in
[ovcommon.md](ovcommon.md) §4): `device_id` / `pairing_code`, `waiting`,
`ready`, `peer_connected`, `peer_disconnected`, `direct_path`, `upnp`, `lan`,
`session_failed`, `error`. Human logs go to stderr. At the default `info`
level only lifecycle lines appear (startup, device ID or pairing code,
waiting / `ready: parked on relay`, peer connected / disconnected, direct
path active / fallback, UPnP mapped / unavailable, warnings, errors).
Periodic statistics (capture, link pacing, H.264, copy-rect, wave2, audio
kbps, media summary) are `debug` (`-v`); per-input-event lines are `trace`
(`-vv`).

### `listen_once()`: one session

1. **Identity and authentication** (see §10).
2. It logs the device ID (unattended) or the fingerprint and pairing code
   (attended), and emits the matching `device_id` / `pairing_code` status
   event.
3. **Wait for a viewer** (`acquire`): the relay park below and, when the LAN
   listener runs, a viewer's HELLO on the LAN port (`LanListener::poll`,
   checked every 200 ms while parked). Datagrams that queued up on the LAN
   port during the previous session are discarded first. A LAN viewer wins
   over an idle park (the relay slot is closed). With a LAN listener a relay
   that cannot be reached is **not** an error: the host reports
   `session_failed`, keeps serving the LAN port and retries the relay with a
   backoff of 1 s doubling to 30 s.

   **LAN viewer:** only a datagram starting with a handshake HELLO tag
   (`0x11`, or the v1 `0x01`, which is answered with REJECT(incompatible))
   is taken, bare or as the first packet of a reliable-UDP link (DATA,
   sequence 0); it is only peeked, so it stays queued. The listener socket
   is handed to the session as a `UdpP2pCarrier` connected to the viewer,
   with the reliable-UDP framing switched on before anything is sent: the
   handshake datagrams are retransmitted when lost, and screen records are
   fragmented to <= 1.2 KB without waiting for the viewer's CAPS (the iOS
   app sends its CAPS only after the first screen update). A viewer that
   starts with bare datagrams switches on the host's first framed packet.
   `set_auth_source(viewer IP)` enables the per-source PIN attempt limit. After the session the port is bound again.
   From here on the session is identical to a relayed one, including the
   direct-path upgrade (the LAN carrier plays the relay's part).

   **Relay park.** `dial()` opens the relay carrier:
   a `Tcp` address through `RelayTunnelCarrier::connect_host` (hostnames
   resolved, every address tried), a `WebSocket` address through
   `RelayTunnelCarrier::connect_ws` (TLS for `wss://` through SChannel; see
   [ovtransport.md](ovtransport.md) §6.4). It connects as the tunnel
   `Responder` and waits up to `PARK_WAIT` = 35 s to be attached. The relay
   drops an unclaimed park after its own 30 s window (the `XT` reply). That
   counts as normal idle time: the host parks again at once with the same
   session ID and does not apply the service backoff. If a park ends after
   less than `MIN_PARK` = 2 s, the relay refused it (for example, it still
   holds the old slot), so the host sleeps 1 s before parking again. Only a
   failure to reach the relay at all is an error.
4. **Handshake** (`handshake_over`). The host runs as `Role::Server` and has
   `HANDSHAKE_AFTER_ATTACH` = 10 s to finish, polling every 10 ms. Datagrams
   that arrive in the same batch as the final handshake message are handed to
   the session as a backlog, so they are not lost.
5. It logs `peer connected, fingerprint <hex>` and emits a `peer_connected`
   status event, which `ovgui` parses (see `ovgui.md`).
6. It builds a `TerminalSession` and, if the direct path is up, calls
   `term.enable_direct(sockets, candidates)` (see §9).
7. With the hidden `--send-file <path>`, the file is sent through `send_file_xfer`
   straight after the handshake.
8. Without `--screen`, it runs `serve_terminal`. With `--screen`, it starts the
   session desktop effects (`SessionFx`), adds `CAP_DESKTOP_FX` to the local
   CAPS and runs `serve_screen`.

---

## 2. Threads

| Thread | Created in | Owns / does |
|---|---|---|
| main / **session thread** | `main` → `session::listen_once` → `serve_screen` / `serve_terminal` | Owns the `TerminalSession`. Pumps inbound traffic (the command hook injects input as soon as it is decoded), sends with priority, runs keepalive and liveness checks, polls the clipboard and cursor, encodes audio. |
| `ovhost-capture` | `serve_screen` | Owns the `ScreenSource`, since GDI DCs and the DXGI device must stay on the thread that created them. Runs `capture_loop`: grab → `UpdateEncoder` (tiles) and `VideoSession` (H.264) → single-slot `FrameMailbox`. Registered with MMCSS under the task name `"Capture"` (`AvSetMmThreadCharacteristicsW`). |
| mic thread | `spawn_mic` (only with `--mic`, `--mic-synth` or `--system-audio`) | Runs `ovapp::audio::run_mic_loop`. Sends PCM ticks over an `mpsc` channel and wakes the session thread. |
| speaker thread | `speaker::spawn` (only with `--audio`) | Drains the `AudioBus` jitter buffer into WinMM `waveOut`. It is joined when `HostHooks` is dropped, so it does not outlive the session. |
| `ovhost-upnp` | `direct_net::DirectNet::start` | Creates the UPnP mapping, refreshes it every 60 s and registers its removal with the cleanup registry. |

The session thread does not sleep on a fixed timer. It blocks in
`ovapp::netwait::Wakeup::wait`, which wakes on socket readability, a mailbox
wake (new frame), a mic tick, or a computed deadline (cursor poll, pacer,
transport retransmit timer). The constants (in `src/session/`) are:

| Constant | File | Value | Meaning |
|---|---|---|---|
| `CLIPBOARD_POLL` | `mod.rs` | 50 ms | OS clipboard sequence-number poll |
| `CURSOR_POLL` | `screen.rs` | 8 ms | Cursor poll while the cursor is idle |
| `CURSOR_POLL_ACTIVE` | `screen.rs` | 4 ms | Cursor poll while it is moving (keeps the 120 Hz cap reachable) |
| `CURSOR_ACTIVE_FOR` | `screen.rs` | 250 ms | How long after the last move the fast poll continues |
| `UNWATCHED_WAIT` | `mod.rs` | 2 ms | Longest wait when the socket cannot be watched |
| `TERMINAL_IDLE_WAIT` | `terminal.rs` | 50 ms | Longest idle wait in terminal mode |
| (screen mode cap) | `screen.rs` | 50 ms | Longest idle wait in screen mode when the socket is watched |
| `METRICS_EVERY` | `capture_loop.rs` | 30 s | Interval between capture, pacing and media metric lines |
| `IDLE_FPS` | `capture_loop.rs` | 4 | Governor rate for a static screen |
| `VIDEO_FPS` | `capture_loop.rs` | 30 | Minimum capture rate while a region streams as H.264 (AUTO only) |
| `FRAME_CHUNK` | `screen.rs` | `ovapp::sendq::DEFAULT_CHUNK` = 4096 B | FRAME_DATA chunk size |

### Send priority (screen mode)

`screen_session_loop` (`session/screen.rs`) uses `ovapp::sendq::PrioritySender`. **Control traffic
is always sent first**: PONG, PING, CURSOR/CURSOR_SHAPE, AUDIO (when the media
channel is closed), CLIPBOARD, chat echo and MONITORS_INFO. After that it sends
**one** frame record per loop turn, with at most one frame in flight, and goes
back to the socket before the next chunk. Input therefore never waits behind a
frame. The capture thread times its next grab with `capture_hold()` so the new
frame is ready just as the link finishes draining the current one (sleeps are
capped at 20 ms). If a frame is already waiting in the mailbox, it holds off
1 ms instead of compressing a frame that would be thrown away.

Two hidden A/B switches exist for benchmarks only:

* `--no-media`: clears `CAP_MEDIA`, so there is no media channel and no v2
  congestion control.
* `--no-pacing`: disables the link pacer, so frames are sent as fast as the
  transport accepts them.

---

## 3. Screen capture backends

`capture::ScreenSource` (`src/capture.rs`) wraps a GDI source, which also
defines the capture region, and tries DXGI Desktop Duplication on the same
region.

### Selection and fallback rules

| Situation | Behaviour |
|---|---|
| Start, or a retarget (monitor switch) | `try_dxgi(DXGI_INITIAL_WAIT_MS = 1000)`. Uses DXGI if a desktop image arrives within 1 s. |
| Duplication created but no image yet (TV off, display asleep, output without an active monitor) | The duplication is kept **pending**. GDI serves the grabs, and every grab polls the pending duplication without blocking. Its first image switches capture to DXGI. Logged once: `DXGI delivers no desktop image (no active monitor / display off?); using GDI until it does`. |
| Creating the duplication fails (RDP session, no output fully containing the region, rotated output, secure desktop) | Falls back to GDI and logs `DXGI capture unavailable (<e>); using GDI`. Retries every `DXGI_RETRY` = 2 s. Periodic retries never wait for an image, so they cannot stall a grab. A regression test covers this: `grabs_never_stall_on_a_duplication_without_frames`. |
| An active duplication is lost (`ACCESS_LOST`: desktop switch, mode change, UAC secure desktop) | Re-created at once (`<e>; re-creating`). If that fails, the GDI and retry rules above apply. |
| `--gdi` | DXGI is never tried (`prefer_dxgi = false`). |
| `--test-pattern video\|scroll\|photo` | A synthetic screen replaces capture entirely (`Backend::Pattern`). |

Each backend switch is logged once: `screen capture via <backend>`.
Off Windows only GDI exists, and the `windows`-gated paths compile out.

### DXGI details (`src/dxgi.rs`)

* `open_region` enumerates adapters and outputs and picks the output whose
  desktop rect **fully contains** the region and that is attached to the
  desktop. A rotated output returns an error: `rotated output (not supported
  by DXGI path)`.
* Only the OS's dirty and move rects are copied. A GPU `CopySubresourceRegion`
  copies them into one of **two alternating** staging textures. The texture
  is then mapped and the rows are copied into a persistent CPU mirror, with
  alpha forced opaque (`copy_opaque`). Nothing is allocated per frame.
* The rects are passed to the tile encoder as *hints*. The encoder still
  `memcmp`s them, so a spurious OS rect costs a compare but never wire bytes.
* `MAX_RECT_COPIES` = 32: above this many rects, the whole region is copied in
  one GPU call.
* `FULL_REFRESH_EVERY` = 300 damaged frames: the whole region is re-copied and
  hinted as fully dirty. This repairs the mirror if the OS ever under-reports
  damage.
* Move rects are passed on as copy-rect (scroll) candidates, which the encoder
  verifies.
* `wait(timeout_ms)` gives **event-driven capture**: the capture thread blocks
  on `AcquireNextFrame` instead of sleeping. A pointer-only update does not
  count as a change.
* The capture-phase latency probe is the QPC `LastPresentTime` to grab time,
  reported every 30 s as `capture latency (present -> grab)`.
* The mouse cursor is **not** in the duplicated image. It is sent separately
  (see §5).
* **GPU encode input**: `dxgi::GPU_ENCODE` defaults to `true`. A GPU-resident
  copy of the region is kept so the H.264 encoder can take D3D11 surfaces
  directly. The hidden `--no-gpu-encode` switches to system-memory NV12. A device
  without video support, or an encoder that rejects the textures, falls back
  to the CPU mirror automatically.

### GDI

`ovapp::gdisrc::GdiScreen` (`BitBlt` into a DIB). The source is created as
`GdiScreen::full_primary()`, or as `GdiScreen::new(0, 0, w, h)` for
`--screen WxH`. It produces no diff hints, so the encoder compares everything.

### Test patterns

`--test-pattern` (`capture::TestPattern`) draws a synthetic desktop:

| Kind | Moving region | Timing |
|---|---|---|
| `video` | 640x360 centred (fixed). | Moves for 5 s (`MOVE`), rests for 2 s (`REST`) |
| `scroll` | 1280x800 scrolling page | Same 5 s / 2 s cycle |
| `photo` | Cycles three "windows" (desktop + two photo windows, 1100x700) | Switches every 1.5 s (`SWITCH`, fixed). |

### Capture-region retarget (monitor switch)

The viewer sends `CAPTURE_REGION`. The command hook checks it against the
virtual screen with `ovapp::monitors::validate_region` and parks it in
`pending_region`. The hook never touches the capture source. The capture thread
then does the following:

1. Calls `ScreenSource::retarget`.
2. Updates the cursor origin.
3. Re-maps input (`set_region` and `set_virtual_screen`).
4. Resets the encoder, so the next update is a full frame.
5. Stops any H.264 region.
6. Sets `monitors_dirty`, so the session re-sends `MONITORS_INFO`.

At session start the host sends `MONITORS_INFO` from `enumerate_monitors()`
before the first frame.

---

## 4. Encoding and quality control

This is driven by `capture_loop` (`src/session/capture_loop.rs`). The codecs themselves live
in `ovapp`.

* **Tile updates**: `ovapp::sendq::UpdateEncoder` diffs each capture against
  what the viewer already holds and queues a full frame, a tile delta, or
  nothing. The viewer's CAPS select what is allowed (tiles, zstd, LZ4 bands,
  copy-rect moves). A viewer that never sent CAPS (peer caps 0) gets full
  DEFLATE frames only.
* **Lossy-first tiles**: image-like tiles go out as JPEG at quality
  `--lossy Q` (default `ovapp::lossy::DEFAULT_QUALITY` = 75), but only to a
  viewer with `CAP_LOSSY`. Settled lossy tiles are then *refined* to exact
  pixels by a "refinement lane" that sends small exact chunks whenever the
  mailbox is free, without needing a capture. `--lossless` disables lossy
  tiles. A tile cache (`CACHE REF`/`STORE`, `CAP_CACHE`) is on by default;
  `--no-tile-cache` disables it.
* **H.264 for motion**: `ovapp::video::VideoSession` finds
  high-motion regions and streams them as H.264 through Media Foundation
  (`ovapp::mfcodec::MfVideoEncoder`). Conditions: the viewer advertised
  `CAP_H264` and `--h264` is not `off`. Encoder preference: `auto` (hardware
  first), `hw` or `sw`. The video rect is masked out of tile updates. When
  motion stops, `VIDEO_END` is queued, followed by a lossless refresh of the
  region, which drains in pieces of about 64 KB. If the encoder fails, ovhost
  logs `H.264 encoder failed (<e>); staying on lossless tiles`.
  The hidden `--video-bpp <f64>` sets the area-based bitrate budget
  (benchmarks). Without Media Foundation (`mfcodec::available()` false)
  H.264 is off from startup (§1).
* **Rate control**:
  * `RateGovernor::new(--fps, IDLE_FPS=4)`: when nothing changes, the capture
    rate eases down to 4 fps.
  * `QUALITY_SET` from the viewer: 0 means AUTO. Any other value is a fixed
    rate that overrides AUTO.
  * AUTO uses `ovapp::adapt::AutoQuality`, which follows the pacer's excess
    **queueing delay** rather than raw RTT. It lowers the H.264 bitrate scale
    before it lowers fps.
  * `link_base_fps()` lets AUTO go above `--fps`, to 30 or to `--max-fps`
    (default 60, clamped to 240), but only when about half of the measured
    delivery rate can carry that many updates of the smoothed update size.
    It never drops below `--fps`.
  * While H.264 is active, the AUTO base is at least `VIDEO_FPS` = 30.
  * Link rate source: the delay-based controller's target, net of FEC, on the
    direct v2 path. Otherwise the pacer's delivery-rate estimate.
* **Logs** (every 30 s while streaming): `screen capture — sent N, skipped N
  unchanged`, `capture <backend>: x ms/grab ...`, `copy-rect: ...`, `wave2:
  ...`, `H.264: ...`, `link pacing: ...`, `N stale frame(s) superseded before
  sending`. On a change: `capture rate -> N fps (AUTO|manual, queue delay ...,
  H.264 bitrate xF, rtt N us)` and `link-limited: ~N fps (a frame drains in N
  ms)`.

The host also handles `KEYFRAME_REQUEST` (the next update is a full frame and
the next picture an IDR) and `VIDEO_IDR_REQUEST` (the next H.264 picture is an
IDR).

---

## 5. Cursor overlay (`src/cursor/`)

The cursor is tracked separately from frames. The idea is that cheap `CURSOR`
(position) and `CURSOR_SHAPE` (bitmap) commands let the viewer draw the cursor
on top of frames that may arrive at only 4 fps when the screen is idle.

* **Poll**: `GetCursorInfo` returns the position, the `hCursor` handle and the
  `CURSOR_SHOWING` flag. A hidden cursor polls as `None`.
* **Coordinates**: positions are **relative to the captured region's origin**,
  so the viewer can map them through the frame scale directly.
* **Throttle**: `CURSOR_MAX_HZ` = 120 (`CURSOR_MIN_INTERVAL` ≈ 8.33 ms).
  Positions are sent only when the cursor has moved. A move inside the
  throttle window is held and sent by the first later tick.
* **Shape**: rendered only when the handle changes (`render_shape`), as
  premultiplied BGRA plus the hotspot.
  * Colour cursors with alpha: colours are premultiplied, because GDI stores
    straight alpha.
  * Colour cursors without alpha: the AND mask decides between opaque,
    transparent and inverted.
  * Monochrome cursors (I-beam, cross): the AND rows and XOR rows of the
    double-height mask are combined. *Inverting* pixels become opaque black
    with a 1 px white 8-connected outline, because an alpha sprite cannot
    invert what is underneath.
  * Bitmaps up to `MAX_GDI_CURSOR_DIM` = 1024 are read, then downscaled to
    `ovapp::terminal::MAX_CURSOR_DIM` for the wire.
* Cursor sends pause while the peer is classified dead.

---

## 6. Input injection (`src/input/`)

Input is injected **only in screen mode**. In terminal mode the input hook has
no controller, so pointer and key commands are only logged (at `trace`, `-vv`).

The work is split into three layers:

1. **Pure mapping** (`input/geometry.rs`, unit-tested):
   * `map_to_region` turns a normalised 0..=65535 point into a pixel inside the
     captured `Region`.
   * `to_virtual_desk` turns that pixel into
     `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK` coordinates across the
     whole virtual screen, including negative origins.
   * `VirtualScreen::query()` uses `GetSystemMetrics(SM_*VIRTUALSCREEN)` and
     falls back to 1x1 when the metrics are unusable.
2. **`InputController`** (`input/controller.rs`) translates wire commands into `InputOp`s:
   * Legacy commands: `POINTER` (left button only) and `KEY` (VK only).
   * Input v2 (`CAP_INPUT2`, always advertised by this host):
     * `POINTER_EX`: absolute or relative moves. A relative move is clamped to
       the captured region by default, or sent as raw motion with
       `POINTER_RAW`.
     * Buttons: left, right, middle, X1 and X2.
     * `WHEEL`: vertical and horizontal, in 1/120 units.
     * `KEY_EX`: VK + scancode + extended flag. For a scancode-less event, the
       extended bit is derived from the VK.
     * `TEXT`: UTF-16 `KEYEVENTF_UNICODE` keystrokes. Surrogate pairs are
       supported, and `\n`/`\r` become Enter, `\t` Tab.
     * `RELEASE_ALL` and `SAS`.
   * Every key and button the controller presses is recorded. They are all
     released (buttons first, then keys in reverse press order) in four cases:
     * on `RELEASE_ALL`;
     * at session end (`session ended; released N held key(s)/button(s)`);
     * when the controller is dropped;
     * after **`STUCK_RELEASE_AFTER` = 30 s** with no input while something is
       still held (`released N stuck key(s)/button(s) after 30s without input`).
3. **Backends** (`input/backend.rs`, Win32 calls in `input/win32.rs`):
   * `SendInputBackend` performs the real `SendInput`, `GetCursorPos` and
     `SendSAS` calls. If the OS refuses a batch, the hook logs `input injection
     refused (blocked desktop / UIPI?)`.
   * `DryRunBackend` (hidden `--dry-input`) only logs, while
     tracking a virtual cursor.
   * Off Windows the real backend does nothing.

**Ctrl+Alt+Del (SAS)**: `sas_capability()` allows a real `SendSAS(FALSE)` only
when all of these hold:

* `sas.dll` exports `SendSAS`;
* the process runs in **session 0** (that is, as a service);
* `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System\SoftwareSASGeneration`
  is 1 or 3.

Otherwise ovhost injects the plain Ctrl, Alt, Del keystrokes and logs that
Winlogon ignores them. Note that the `--service` flag only loops; it does
**not** make ovhost a Windows service (see §10).

---

## 7. Audio

| Direction | Flag | Implementation |
|---|---|---|
| Host microphone → viewer | `--mic` | WinMM `waveIn`, default input device, 48 kHz mono s16le, ring of `ovapp::winaudio::MIC_BUFFERS` × 20 ms buffers drained on the driver's completion event (`src/mic.rs`, shared `ovapp::winaudio`) |
| Synthetic tone → viewer | `--mic-synth` | 440 Hz sine, paced by a drift-free `TickClock` |
| Host speakers (loopback) → viewer | `--system-audio` | WASAPI loopback of the default **render** endpoint, event-driven shared mode, 10 ms period (`PERIOD_HNS` = 100 000). Down-mixed to mono, converted to s16, linearly resampled to 48 kHz if needed. Emits silent ticks at real-time pace while nothing plays (`src/sysaudio.rs`). |
| Viewer microphone → host speakers | `--audio` | `speaker::AudioBus` = `ovapp::audio::JitterBuffer` (60 ms prefill, 120 ms ceiling) drained into `ovapp::winaudio::WinmmSpeaker` (three rotating 20 ms `waveOut` headers) on a dedicated thread (`src/speaker.rs`) |

* `--mic`, `--mic-synth` and `--system-audio` are mutually exclusive. The last
  one on the command line wins.
* **Encoding** (`ovapp::audio::AudioEncoder::encode_ticks`):
  * Opus to a viewer with `CAP_OPUS`, raw s16le PCM otherwise.
  * A `CAP_AUDIO10` viewer gets 10 ms Opus packets with in-band FEC
    (`audio streaming on (10 ms Opus packets to a CAP_AUDIO10 viewer, 20 ms
    otherwise)`).
  * Once the media channel is open (`term.media_ready()`), audio travels as
    unreliable datagrams (`send_audio`), so a lost packet is concealed instead
    of stalling.
  * Audio is sent only while the peer is alive.
  * A bitrate line is logged every 3000 ticks.
* **Decoding** of received audio uses `ovapp::audio::AudioDecoder`. Malformed
  payloads are dropped and never panic.
* The two ends play only what they receive, so there is no local feedback loop.
* If the device fails to open, audio is disabled for the session with a log
  line (`mic unavailable, audio disabled: ...`); it never crashes.

---

## 8. Desktop effects and cleanup guarantees

### Desktop effects (`src/desktop_fx.rs`)

These apply in screen sessions only, and are on by default:

* **Hide the wallpaper**, optionally replacing it with a flat colour
  (`--desktop-color RRGGBB`, applied through `SetSysColors(COLOR_DESKTOP)`).
  Disable with `--keep-wallpaper`.
* **Turn off UI animations**: `SPI_SETCLIENTAREAANIMATION` and
  `SPI_SETANIMATION` (minimize/maximize). Disable with `--keep-animations`.
* The viewer can toggle these during a session through `SESSION_OPTION`
  (`OPT_HIDE_WALLPAPER`, `OPT_NO_ANIMATIONS`). The host advertises
  `CAP_DESKTOP_FX` in screen mode.
* **Nothing is written to the user profile.** Changes use `SPIF_SENDCHANGE`
  only, never `SPIF_UPDATEINIFILE`, and `SetSysColors` is session-only, so
  even a lost marker is undone at the next logon.
* `--fx-dry-run` uses the logging `DryRunFx` backend instead of `Win32Fx`.

### Restore guarantees

| Exit path | Mechanism |
|---|---|
| Normal session end | `SessionFx::drop` calls `restore_all()` (`desktop effects restored`) |
| Ctrl-C, Ctrl-Break, console close, logoff, shutdown | `cleanup::install()` registers a `SetConsoleCtrlHandler` that runs every registered action, then lets the default handler terminate the process |
| Panic | A panic hook runs every action before the previous hook |
| Hard crash, kill or power loss | The original values are saved in the marker file **`<data dir>\ovhost-desktopfx.state`** (`default_marker()`; a marker left next to the exe by an older release is also recovered, `legacy_marker()`). At the next start, `DesktopFx::recover_stale` restores them (`restored desktop effects left over by a previous run`). |

`cleanup.rs` actions run **at most once**, newest first, each inside
`catch_unwind`. Owners that clean up normally unregister their action. The two
users are the desktop-effects guard and the UPnP mapping deletion. `ovgui`
stops hosts with Ctrl-Break for exactly this reason (see `ovgui.md`).

---

## 9. Direct networking (`src/direct_net.rs`)

A session always **starts on the relay**. The host then offers a direct UDP
path (`DIRECT_CANDIDATES`), and the relay remains the fallback. Promotion,
demotion and probe events are logged as `direct: <event>`.

* **Port**: `DEFAULT_DIRECT_PORT` = **4570/udp**, set with `--direct-port N`.
  The socket(s) are bound once per process (`DirectSockets::bind`, IPv4 and
  IPv6 when available) and cloned for each session.
* **Candidates**, de-duplicated:
  1. Every usable LAN IPv4 address (`ovdiscovery::netif`, built on
     `GetAdaptersAddresses`), the default-route address toward 8.8.8.8 first.
  2. `127.0.0.1:<port>`.
  3. A global IPv6 address (2000::/3) toward `2001:4860:4860::8888`, if an IPv6
     socket exists.
  4. The **STUN** reflexive address. The query uses the direct IPv4 socket
     with a 1.5 s timeout and runs synchronously at start-up, before any
     session traffic. The default server is `ovdiscovery::stun::DEFAULT_STUN`
     = `stun.cloudflare.com:3478`. Change it with `--stun H:P`, or skip it
     with `--no-stun`.
  5. The **UPnP** external address. It is added asynchronously by the
     `ovhost-upnp` thread:
     * `PortMapping::create(port, UPNP_LEASE = 3600 s, 3 s timeout)`;
     * the mapping is refreshed every 60 s; a failing refresh warns once
       (`UPnP refresh failed (<e>); retrying every minute`), later failures
       are `debug`, and `UPnP refresh works again` is logged on recovery;
     * it is deleted at exit through the cleanup registry
       (`direct: UPnP mapping removed`).

     `--no-upnp` disables this.
* The hidden `--direct-advertise A:P[,A:P]` **replaces** the gathered list and also
  disables UPnP. It is meant for benchmarks and network emulators.
* `--no-direct` keeps the session on the relay only.
* **LAN addresses** (candidate 1) come from `ovdiscovery::netif::lan_ipv4s()`:
  loopback, link-local, tunnel and host-only virtual adapters are left out.

The log line `direct path on UDP <port> (candidates: ...)` shows what
will be offered; the socket state is also emitted as a `direct_path` status
event (`listening` with `port`, or `disabled`), and the UPnP result as an
`upnp` event.

---

## 10. Security model: pairing code vs unattended PIN

The handshake lives in `ovcore` (the GUI's About page describes sessions as
*X25519 + ChaCha20-Poly1305, PIN-authenticated*). The host chooses one of two
auth modes in `listen_once()`:

| | Attended (default) | Unattended (a PIN via `OPENVIEW_PIN` or `--pin`; `--unattended` is implied) |
|---|---|---|
| Identity | `Identity::generate()`: a **fresh identity on every `listen_once`**, so `--service` without a PIN gets a new code for each session | Loaded from or created in **`<data dir>\ovhost-unattended.id`** (`unattended::load_or_create_identity`; default `%LOCALAPPDATA%\OpenView`), so it stays the same across runs. A file left next to the exe by an older release is copied over once (`ovcommon::paths::migrate_file`), so the device ID survives the upgrade |
| What the operator shares | The **pairing code** (log `pairing code <code>`, status event `pairing_code`): a random one-time 10-digit code from `ovcore::pairing::generate`, new for every `listen_once` — so a code is burned after any attempt | The **device ID** (log `device id <n> (unattended)`, status event `device_id`) and the PIN |
| Relay session ID | The code's 4 public rendezvous digits as a big-endian `u32` (`ovcore::pairing::relay_session_id`) | The low 32 bits of the device ID |
| Handshake auth | `Auth::PairingCode(code)`: SPAKE2 with the pairing identities | `Auth::Pin(psk)`, where `psk = ovcore::pin::pin_psk(pin)`: PBKDF2-HMAC-SHA256, 10 000 iterations, salt `openview-pin-v1`; SPAKE2 with the PIN identities |

* **Device ID** = the first 5 fingerprint bytes as a big-endian `u64` (40
  bits). The same value is used as the handshake `my_dev` in both modes.
* **Credentials** ([ovcore.md](ovcore.md) §4.1): both modes run SPAKE2 with
  key confirmation in both directions (handshake v3), so neither the PIN nor
  any part of the code crosses the wire, a wrong credential fails cleanly on
  both sides, and each attempt tests one guess. Attempts are throttled
  host-wide (`AuthThrottle`; per source address on the LAN port). The
  ephemeral X25519 keys keep forward secrecy.
* A viewer dialing by device ID pins the expected device
  (`hs.expect_device`), so it cannot be steered to another host that shares
  the 32-bit relay session ID.
* `--unattended` without a PIN fails with `--unattended needs a PIN
  (OPENVIEW_PIN or --pin)`.
* **PIN visibility**: `--pin` puts the PIN on the command line, which other
  local processes can read; `OPENVIEW_PIN` does not (clap `env`,
  `hide_env_values`). `ovgui` always uses the environment variable and never
  reads another process's command line (KNOWN_ISSUES SEC-2, fixed).

### "Service" / unattended operation

There is **no Windows Service (SCM) integration** in `ovhost`. `--service` is a
loop with backoff ("serve sessions forever (back off after errors) instead of
exactly one", `cli.rs`).
Unattended start-up is done in one of two ways:

* starting `ovrelay` and `ovhost --service` with `OPENVIEW_PIN` set
  yourself, e.g. from a logon script (see `USAGE.md` §4);
* `ovgui` (`OpenView.exe`), which supervises `ovrelay` and `ovhost --unattended
  --service --data-dir <dir> ... --screen` (PIN in `OPENVIEW_PIN`) and can
  start at sign-in through the HKCU `Run` key.

---

## 11. Clipboard and file transfer

* **Clipboard** (`CLIPBOARD` 0x50, text only, `kind 0`) is on by default and
  disabled with `--no-clipboard`.
  * The session loop polls `GetClipboardSequenceNumber` every 50 ms and reads
    the text only when the number changes.
  * `ovapp::clipboard::ClipboardSync` guards against echo loops: text that
    came from the peer is not sent back.
  * Received text is written to the OS clipboard from the command hook.
* **Files**:
  * The hidden `--send-file <path>` sends one file straight after the
    handshake.
  * Received files (file-ID-keyed transfers, `session/files.rs`) are saved to
    **`Downloads\OpenView\<sanitized name>`** (`ovcommon::paths::received_files_dir`,
    the Downloads known folder), never overwriting an existing file
    (`unique_path`: `name (2).ext`, ...).
  * Diagnostic logs uploaded by the iOS app (`ovlog-ios-*.log`) go to
    `<data dir>\logs\phone` instead.
  * Legacy `FILE_BEGIN` transfers are only reported.
* **Chat**: a `TYPE` command with `key == 0 && mods == 0` is a chat line. It is
  logged as `chat: <line>` and echoed back to the peer.

---

## 12. Liveness

These constants come from `ovapp::liveness` (`crates/ovapp/src/liveness.rs`):

| Constant | Value |
|---|---|
| `KEEPALIVE_INTERVAL` | 5 s (host PING cadence; the PONG gives the RTT) |
| `DEAD_AFTER` | 15 s without any inbound record: the peer is **dead**. Capture pauses and no media is produced, but the session is kept. |
| `SESSION_TIMEOUT` | 30 s without any inbound record: the session ends (`peer silent for 30s; ending session`) |

Every decoded inbound record, of any kind, refreshes liveness.

---

## 13. Other constants

| Constant | Where | Value |
|---|---|---|
| `DEFAULT_RELAY` | `cli.rs` | `127.0.0.1:4569` |
| `PARK_WAIT` | `session/mod.rs` | 35 s |
| `HANDSHAKE_AFTER_ATTACH` | `session/mod.rs` | 10 s |
| `MIN_PARK` | `session/mod.rs` | 2 s |
| `MAX_AUTO_FPS` | `settings.rs` | 60 by default (`--max-fps`, clamped to 240) |
| `--fps` default | `cli.rs` | 10 |
| `DXGI_RETRY` | `capture.rs` | 2 s |
| `DXGI_INITIAL_WAIT_MS` | `capture.rs` | 1000 |
| `MAX_RECT_COPIES` | `dxgi.rs` | 32 |
| `FULL_REFRESH_EVERY` | `dxgi.rs` | 300 |
| `CURSOR_MAX_HZ` | `cursor/mod.rs` | 120 |
| `MAX_GDI_CURSOR_DIM` | `cursor/win32.rs` | 1024 |
| `STUCK_RELEASE_AFTER` | `input/mod.rs` | 30 s |
| `PERIOD_HNS` | `sysaudio.rs` | 100 000 (10 ms) |
| `DEFAULT_DIRECT_PORT` | `direct_net.rs` | 4570 |
| `UPNP_LEASE` | `direct_net.rs` | 3600 s, refreshed every 60 s |

## 14. Files and network

Files (the data dir is `--data-dir`, else `%LOCALAPPDATA%\OpenView`, or
`<exe dir>\data` when `portable.txt` sits next to the exe; see
[ovcommon.md](ovcommon.md) §2):

| Path | Written by |
|---|---|
| `<data dir>\ovhost-unattended.id` | Unattended identity (persisted; migrated once from `<exe dir>`) |
| `<data dir>\ovhost-desktopfx.state` | Desktop-effects marker; exists only while effects are active (a legacy `<exe dir>` marker is still recovered) |
| `<data dir>\logs\phone\` | iOS diagnostic log uploads |
| `Downloads\OpenView\` | Received files |

`ovhost` writes no log file itself (stderr only; the GUI redirects it to
`logs\ovhost.log`).

Network:

| Direction | Port | Purpose |
|---|---|---|
| Outbound TCP | `--relay` (default `127.0.0.1:4569`; a `ws://` / `wss://` URL uses its port, 80 / 443 by default) | Park on the relay |
| Inbound UDP | `0.0.0.0:4570` and `[::]:4570` (`--direct-port`) | Direct path; the only inbound port the host needs from the Internet. UPnP maps it on the router unless `--no-upnp`. |
| Inbound UDP | `0.0.0.0:4571` (`--lan-port`; `--no-lan` turns it off) | LAN sessions without the relay (viewers on the same network) |
| UDP 5353 (shared) | `224.0.0.251:5353` | mDNS announcements and answers (unattended mode; `--no-mdns` turns it off) |
| Outbound UDP | `--stun` (default `stun.cloudflare.com:3478`) | Public address discovery |

## 15. Tests in the crate

Session unit tests (`src/session/`) cover:

* the AUTO fps rule, `capture_hold`, the static-screen idle behaviour and the
  metrics line format (`capture_loop/tests.rs`);
* the peer-state classification (`liveness.rs`);
* re-parking after a relay `XT` against a real in-process `ovrelay::Relay`
  (`mod.rs`);
* the LAN path end to end: mDNS advertise of a scratch device ID → resolve →
  PIN handshake and datagram exchange over the LAN carrier, with an
  unreachable relay (`mod.rs`); the LAN listener's HELLO filter, hand-over
  and re-bind (`lan.rs`);
* CAPS, keyframe and IDR propagation, input v1 and v2 reaching the
  controller, `QUALITY_SET` and chat payloads (`hooks/tests.rs`);
* received-file handling (`files.rs`).

`capture.rs`, `dxgi/tests.rs`, `cursor/tests.rs`, `input/tests.rs`,
`desktop_fx/tests.rs`, `cleanup.rs`, `direct_net.rs`, `mic.rs`, `speaker.rs`
and `sysaudio.rs` have their own tests. The DXGI and WASAPI
tests are CI-safe: they degrade gracefully when no device exists.
