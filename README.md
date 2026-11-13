# OpenView

A self-hosted remote desktop stack in Rust, built from the wire up:
handshake → AEAD record layer → transports (UDP / TCP / relay / in-memory) →
relay server → LAN discovery → hole punching → application shells.

> Start with [`docs/`](docs/README.md). The demo sections further down
> describe features as they were introduced and are kept for history; flags
> and behaviour may have changed since — the authoritative flag reference is
> [`docs/USAGE.md`](docs/USAGE.md), open bugs are in
> [`docs/KNOWN_ISSUES.md`](docs/KNOWN_ISSUES.md).

Workspace layout (`crates/`):

| crate        | role                                                                 |
|--------------|----------------------------------------------------------------------|
| `ovcore`     | identity (Ed25519+X25519, OVID1), handshake (+PIN PSK), AEAD record `Session` |
| `ovtransport`| `Carrier` trait: UDP (+reliable RUDP) / TCP / in-memory / relay tunnel (TCP or WebSocket); direct-path upgrade, congestion control, FEC media channel |
| `ovrelay`    | std-only rendezvous relay, TCP + WebSocket (`ovrelay` binary)         |
| `ovdiscovery`| mDNS/DNS-SD, STUN binding client, UPnP IGD port mapping               |
| `ovapp`      | application layer: command bus + CAPS, tiles/bands/MOVE/JPEG/H.264 screen codecs, send queue + pacing, Opus audio, clipboard, chat, file transfer, recording |
| `ovview`     | desktop viewer binary (Direct2D)                                     |
| `ovhost`     | desktop host binary (DXGI capture, input, audio, direct networking)  |
| `ovgui`      | `OpenView.exe` — Slint Windows GUI supervising host/relay, launching viewer |
| `ovffi`      | C ABI staticlib for the iOS app (`ios/RemoteView`)                    |
| `ovcommon`   | shared plumbing of the Windows binaries: data dirs, JSON-lines status events, logging setup |

Docs: [architecture](docs/ARCHITECTURE.md) · [usage](docs/USAGE.md) ·
[wire protocol](docs/WIRE_PROTOCOL.md) · [media pipeline](docs/MEDIA_PIPELINE.md) ·
[iOS app](docs/IOS_APP.md) · [known issues](docs/KNOWN_ISSUES.md) ·
per-crate references in [`docs/crates/`](docs/crates/).

Protocol details: [`docs/WIRE_PROTOCOL.md`](docs/WIRE_PROTOCOL.md).

## Build

```
cargo build --workspace
cargo test --workspace
```

`ovhost` / `ovview` take `--help` and `--version`. They print JSON-lines
status events on stdout and human logs on stderr (`-v` debug, `-vv` trace,
`--log-level`, `OPENVIEW_LOG`). Data (host identity, GUI settings, logs) lives
in `%LOCALAPPDATA%\OpenView` (`<exe dir>\data` when `portable.txt` sits next
to the exe); received files go to `Downloads\OpenView`. See
[`docs/USAGE.md`](docs/USAGE.md) and
[`docs/crates/ovcommon.md`](docs/crates/ovcommon.md).

## Demo: chat + file transfer through the relay

Terminal 1 — relay:

```
cargo run -p ovrelay -- --bind 127.0.0.1:4569
```

Terminal 2 — host (prints its pairing code):

```
cargo run -p ovhost
```

Terminal 3 — view (use the code the host printed):

```
cargo run -p ovview -- --code 123456 --relay 127.0.0.1:4569
```

Type a line in the view — the host decrypts it, logs the content, and echoes
it back, so the view prints `[peer] <line>`. `exit`/`quit` ends the session.

### File transfer (v0.24, file-id-keyed, both directions)

Either end can push a file to the other right after the handshake:

```
cargo run -p ovhost -- --send-file C:/tmp/report.pdf
cargo run -p ovview -- --code 123456 --relay 127.0.0.1:4569 --send-file notes.txt
```

(`--send-file` is a hidden test switch.) Each side saves what it receives
into `Downloads\OpenView` (the user's Downloads folder; an existing file is
never overwritten, the new one becomes `name (2).ext`; the peer-supplied name is sanitized to a bare file name — path
components, drive prefixes and control characters are stripped). On the wire
the transfer rides the v0.24 `FILE_XFER_DATA (0x14)` / `FILE_XFER_COMPLETE
(0x18)` commands: every chunk carries a `file_id` + `offset`, so multiple
transfers can interleave on one session and each is reassembled
independently, order-tolerantly, with the declared total verified before the
file is accepted. The legacy `FILE_BEGIN/FILE_DATA/FILE_END` path still works
for the demo chat mode.

## Unattended access (device ID + PIN)

For a headless machine nobody is sitting at, the host publishes a **stable
device ID + access password (PIN)**; anyone with both can connect — no
pairing, no QR, no pre-shared keys. The PIN is stretched to a PSK
(PBKDF2-HMAC-SHA256, 10 000 iterations) that is mixed into the handshake key
schedule and proven via an HMAC bound to the ephemeral KEX key — the PIN
itself never crosses the wire, and forward secrecy is preserved (the
ephemeral X25519 ECDH is still the base secret).

Terminal 1 — relay (as before):

```
cargo run -p ovrelay -- --bind 127.0.0.1:4569
```

Terminal 2 — host, unattended (prints its stable device id):

```
$env:OPENVIEW_PIN = "hunter2"   # or --pin hunter2 (visible to other processes)
cargo run -p ovhost -- --unattended
```

Terminal 3 — viewer, connect by device id + PIN:

```
$env:OPENVIEW_PIN = "hunter2"
cargo run -p ovview -- --connect <device_id>
```

Auth modes: **identity-trust** (`--code` pairing code / `--id` fingerprint,
requires pre-sharing the peer identity) vs **PIN** (`--unattended --pin` /
`--connect --pin`, requires only the shared password). A wrong PIN is
rejected explicitly during the handshake (`PIN proof mismatch`), never as a
silent mid-stream AEAD failure.

## Automated end-to-end validation (3 processes)

`scripts/e2e_relay.sh` drives the whole stack as **three separate OS
processes** over real sockets — the strongest proof the protocol works
end-to-end, not just in-crate:

```
./scripts/e2e_relay.sh            # uses port 14569
./scripts/e2e_relay.sh 5000       # or a custom relay port
```

It builds the release binaries, starts `ovrelay`, starts `ovhost --listen`
(capturing the freshly generated 6-digit pairing code), dials `ovview --code
<code>`, sends `hello-e2e`, and asserts that:

- the view completes the handshake and observes the host's echo (`[peer]
  hello-e2e`);
- the host completes the handshake and logs the decrypted line (`chat:
  hello-e2e`);
- both processes exit 0 on a clean disconnect.

This exercises relay rendezvous by pairing code, the full X25519 + Ed25519 +
ChaCha20-Poly1305 handshake, AEAD records in **both directions**, and teardown
across process boundaries. Deterministic (verified 5/5 runs).

## Demo: direct LAN (no relay)

```
cargo run -p ovhost                   # advertises over mDNS
cargo run -p ovview  -- --id           # browses mDNS, dials UDP directly
```

Both binaries must run on the same machine / LAN segment (mDNS group
224.0.0.251:5353).

## Demo: screen streaming + remote control

```
cargo run -p ovhost -- --screen        # stream the full primary monitor
cargo run -p ovhost -- --fps 15 --screen 640x360   # or a WxH region
```

The host streams real GDI-captured BGRA frames to the connected view. Since
v0.11 the viewer is a **controller**, not just a display: POINTER/KEY commands
from the viewer are injected on the host as real `SendInput` events (mouse
move/left-down/left-up mapped from normalized 0..=65535 coordinates through
the captured region into `MOUSEEVENTF_ABSOLUTE` virtual-screen space, and
Windows virtual-key presses). The iOS RemoteView app provides the touch
surface (drag on the screen image) plus a key row; see spec §11.4 for the
wire details.

## Session recording (v0.19)
The desktop viewer can record the received screen session: press **F6** to
start, **F6** again to stop. Files land in `%USERPROFILE%\ovrecord\`
(override with `--record-dir <path>`) named
`ovview-YYYYMMDD-HHMMSS-mmm.ovrec`. While recording the HUD's bottom line
shows `REC ● <elapsed>s <MiB> <frames>f`; the file is always finalized with
its trailer, including on window close / Ctrl-C / peer loss.

The container (`ovapp::record`) is self-describing and versioned: an
`OVREC001` header carrying the full capture config (app version, pixel
format, codec, initial resolution, quality mode, peer identity, wall-clock
start time), one record per frame with its sequence number, wall-clock
ms-since-start, and **per-frame geometry (width/height)** — the stream is
change-detected and variable-rate so timestamps drive playback pacing, and
per-frame geometry means a recording that switches capture target mid-session
(v0.18) still decodes correctly — plus an `OVRECEOF` trailer with the frame
count and total duration. Pixels are raw-DEFLATE compressed (same codec as
the wire). `read_header` / `read_frames` / `validate` re-parse any file
without panicking on malformed bytes. Playback/decode-to-screen is v0.20;
the codec module is pure Rust so iOS recording can reuse it via FFI.

### Playback & export (v0.20)

A recording plays back offline — no host, no relay, no network:

- **Play:** `ovview --play <file.ovrec>` opens the render window and shows
  the frames at the **recorded pace** (each frame waits the gap to the next
  timestamp, so the original variable-rate timing is reproduced). Add
  `--loop` to restart from the beginning on EOF; Escape or Ctrl-C stops.
  The window letterboxes exactly like a live session — playback reuses the
  same present path, adapting to each frame's stored geometry.
- **Inspect:** `ovview --info <file.ovrec>` prints the header summary
  (app version, codec, initial resolution, quality mode, host, start time,
  frame count, duration, bytes) — a quick well-formedness check.
- **Export frames:** `ovview --export <file.ovrec> --out <dir> [--frame N]`
  writes `frame-<seq>.ppm` files (8-bit RGB, P6). Without `--frame N` every
  frame is exported (a warning prints above 500 frames). PPM was chosen over
  PNG/JPEG because it needs no encoder dependency and is byte-exact; convert
  with any image tool (`magick frame-3.ppm frame-3.png`).

All three run before any network code, so they work on a machine that never
connected to a host; a malformed file reports an error and exits non-zero.

## Audio streaming (v0.21)
The host's microphone is streamed to the viewer in real time: `ovhost --screen ...
--mic` captures the default input device via WinMM `waveIn` and sends one `AUDIO`
record per 20 ms tick (raw s16le 48 kHz mono, 960 samples — opcode `0x40`, spec
§11.4). Use `--mic-synth` to stream a synthetic 440 Hz tone instead (no microphone
needed; useful for testing). A failed device open disables audio with a log line and
the session continues video-only.

On the viewer, audio starts muted-off: press **F2** to enable playback, **M** to
mute/unmute (mute keeps the pipeline running so unmute resumes in sync). Incoming
ticks land in a ~400 ms jitter buffer (`ovapp::audio::AudioRingBuffer`) that a
playback thread drains into WinMM `waveOut`; underruns zero-fill and are counted.
The HUD shows the audio state alongside the other status lines.

Audio rides the same command channel as the screen frames, so the two media types
coexist without a second connection — the `audio_over_relay` integration test
proves both cross a live relay byte-exact. The iOS viewer receives the same ticks
via `ov_screen_set_audio_sink` and plays them through `AVAudioEngine`.

## Full-duplex audio (v0.23)

Audio is **two-way**: the `AUDIO (0x40)` opcode is direction-agnostic — the
sender of a record is always "my mic", the receiver plays it as "the remote
peer's mic". In a 1:1 session that is unambiguous, and each end plays ONLY what
it receives, so there is no local feedback loop.

- Host: `--mic` (or `--mic-synth`) captures and streams its mic as before;
  add `--audio` to ALSO play the viewer's mic on the host speakers (a
  `waveOut` playback thread draining a jitter bus, mirroring the viewer's
  v0.21 path).
- Viewer: playback of incoming audio is unchanged (F2/M); add `--mic` (or
  `--audio-synth`) to capture the local mic and send 0x40 ticks to the host,
  paced one tick per 20 ms inside the viewer loop.
- `audio_duplex_over_relay` proves both directions concurrently over a live
  relay: each endpoint streams a distinct sine tone and receives the OTHER's,
  byte-exact in order.

Known limitation: there is no acoustic echo cancellation — with speakers and
mic on in the same room, the remote side hears itself. Both-ends AEC is a
future enhancement.

## Clipboard sync (v0.26)

Clipboard *text* syncs both ways automatically while a session is up — copy on
one end, paste on the other:

```
cargo run -p ovhost                     # clipboard sync on by default
cargo run -p ovview -- --code 123456 --relay 127.0.1:4569 --no-clipboard
```

On the wire it rides the `CLIPBOARD (0x50)` command (`1B kind` + payload;
kind `0` = UTF-8 text). Each end keeps an echo guard (`ovapp::clipboard::ClipboardSync`):
text just applied from the peer is never re-sent, so copy/paste does not
oscillate. The host and viewer poll the Win32 clipboard via
`GetClipboardSequenceNumber` (cheap, no open) inside their existing loops;
iOS applies peer text to `UIPasteboard` and polls `changeCount` per pump tick.
Pass `--no-clipboard` to disable.

## File transfer on iOS (v0.27)

The v0.24 file-id-keyed transfer (`FILE_XFER_DATA 0x14` / `FILE_XFER_COMPLETE
0x18` on channel 0) is surfaced on the phone both ways:

- **Receive**: `ov_screen_set_file_sink` fires when the peer's transfer
  completes; `SessionView` writes the sanitized file into the app container
  and lists it (tap → share sheet).
- **Send**: the `Send file…` button presents a document picker and ships the
  picked file via `ov_file_send` (begin-less 16 KiB chunks + completion, the
  same `ovapp::file_transfer::FileTransfer` chunking the desktop shells use).

Large files are held in memory on the FFI boundary (name + bytes); a
streaming-to-disk receive is a future refinement.

## iOS app (RemoteView)

The same Rust protocol core ships to iPhone as a staticlib (`ovffi`), wrapped by
a small SwiftUI shell under `ios/RemoteView/`. The app is a **controller** (it
dials a host); the host is run as `ovhost` on the desktop.

- **Build:** on a Mac, `ios/build_ios.sh --sim` builds `ovffi` for the
  device and simulator targets (pure-Rust deps, no C toolchain needed),
  packs it with `ios/OvFFI/include/shim.h` into `ios/build/OvFFI.xcframework`
  and builds the app with XcodeGen + Xcode. See [ios/README.md](ios/README.md).
- **Two transport modes** (chosen on the connect screen):
  - **Relay** — the app dials a relay (`ovrelay`) over TCP; the Rust side owns
    the socket and a timer pump drives it. Works across NAT with no config
    beyond the relay host:port.
  - **Direct / LAN** — `NwUdpBridge` (Network.framework `NWConnection`, UDP)
    dials the peer's IP:port directly; inbound datagrams are fed to
    `ov_session_on_datagram`, outbound ones come from the ovffi datagram sink
    and are sent over the NWConnection. Both the session and the bridge are
    confined to the main queue to avoid re-entrancy with the pump.
- The pairing code, device identity (Ed25519/X25519 seed) and READY binding are
  identical to the desktop path — same `ovcore` handshake and ChaCha20-Poly1305
  record layer, so a phone and a desktop host interoperate transparently.
- The session also decodes **screen frames** (`FRAME_BEGIN/DATA/END`) from the
  host and hands each reassembled `BGR8` frame to a `FrameHook`, ready to feed a
  render surface (e.g. a `CAGradientLayer`/`MTKView` or a `CGImage`) on iOS.

## Version history

- **v0.27** — **iOS file-transfer surfacing**: `ov_screen_set_file_sink`
  (fired from `ov_screen_feed` on completed peer transfers; the `OvScreen`
  box owns the same `ovapp::file_transfer::FileTransfer` reassembler the
  desktop pump uses) + `ov_file_send` outbound (shared chunking, thread-safe
  via the session mutex). Swift: `onFile` sink, `sendFile(name:data:)`, a
  `Send file…` document picker and a received-files list written to the app
  container.
- **v0.26** — **bidirectional clipboard sync**: new `CLIPBOARD (0x50)` command
  (`1B kind` + payload, kind 0 = UTF-8 text) with an echo-loop-safe
  `ovapp::clipboard::ClipboardSync` (6 unit tests: send-once, echo broken,
  clear propagates, two-endpoint convergence). Win32 text read/write via raw
  FFI in `ovhost`/`ovview` (`clipboard_os`, `CF_UNICODETEXT`, change polling
  via `GetClipboardSequenceNumber`), on by default with `--no-clipboard` to
  disable. iOS gets `ov_screen_set_clipboard_sink` + a Swift `ClipboardBridge`
  (UIPasteboard apply/poll with its own echo guard).
- **v0.23** — **bidirectional (full-duplex) audio**: the `AUDIO (0x40)` opcode
  is now direction-agnostic — the sender of a record is always "my mic", the
  receiver plays it as "the remote peer's mic". `ovhost` gains `--audio` (a
  `ovhost::speaker` playback path mirroring the viewer's `waveOut` thread, so
  the host plays the viewer's mic) and `ovview` gains `--mic` / `--audio-synth`
  (`ovview::mic` mirroring `ovhost::mic`, so the viewer streams its mic to the
  host). Each end plays only what it receives — no local feedback loop.
  Verified by the `audio_duplex_over_relay` integration test: two endpoints
  over a live relay each stream a distinct sine tone concurrently and each
  receives the OTHER's byte-exact in order. Known limitation: no acoustic echo
  cancellation (both-ends AEC is a future enhancement).
- **v0.21** — **real-time audio streaming**: new `AUDIO (0x40)` command opcode
  carrying one 20 ms tick of raw s16le 48 kHz mono PCM per record (6-byte header:
  codec, flags, u32-BE sample count). `ovapp::audio` holds the pure helpers
  (encode/decode with `Err`-not-panic validation, `AudioRingBuffer` jitter buffer
  with underrun counting, `SineGen` synthetic mic). `ovhost` gains `--mic` (WinMM
  `waveIn` capture, raw-FFI like `cursor.rs`) and `--mic-synth`, paced by a pure
  `MicPacer` inside the existing `serve_screen` loop (one tick per 20 ms, catch-up
  capped at one tick). `ovview` gains a `waveOut` playback thread draining a ~400 ms
  `AudioBus` jitter buffer, **F2** toggles playback and **M** mutes; the `on_cmd`
  hook is now installed unconditionally. iOS gets `ov_screen_set_audio_sink` and
  plays the ticks through `AVAudioEngine` + `AVAudioPlayerNode`. Verified by the
  `audio_over_relay` integration test: 10 sine ticks + a screen frame cross a live
  relay, audio byte-exact in order, video reassembles byte-exact alongside.
- **v0.17** — **RTT-aware auto quality + manual QUALITY_SET**: the host reads its
  own keepalive-measured RTT (v0.16 liveness) and, in auto mode, throttles the
  effective capture rate on a congested link (`ovapp::adapt::auto_target_fps`:
  <=150 us full, <=1 ms half, >1 ms 8 fps; monotonic) via the new
  `RateGovernor::set_base_fps` (retargets without resetting the idle state).
  New viewer→host command `QUALITY_SET` (0x36, 2-byte u16-BE target fps, 0 =
  AUTO) pins a manual rate. The desktop HUD gains a `Q AUTO/N fps` line and an
  **F1** keybinding cycling AUTO → 10 → 30 → 60 fps; the iOS `SessionView`
  gains a segmented quality picker sending the same command.
- **v0.7** — `ovffi` staticlib + SwiftUI RemoteView shell + macOS CI pipeline;
  the phone dials a desktop host over the relay.
- **v0.8** — `NwUdpBridge` (Network.framework `NWConnection`, UDP) adds the
  **direct / LAN** transport mode on iOS, so the phone can bypass the relay and
  punch straight to a host on the same LAN.
- **v0.9** — **screen streaming**: `ovapp` `FRAME_BEGIN/FRAME_DATA/FRAME_END`
  opcodes, `TerminalSession` frame reassembly (`on_frame`/`take_frame`; pixel chunks
  rejoin byte-exact), the `screen` data model (`Frame`/`PixelFormat`), and the
  `OvScreen` C-FFI reassembler on the iOS side; `TerminalSession` drives the
  `ovhost` send side and the view/phone receive side, wired through the existing
  `ov_session` record sink. Also raised the
  relay tunnel `MAX_FRAME` from 16 384 to 65 532 (one full AEAD record always
  fits in a single TCP-carrier frame) so full-size screen frames flow through
  the relay unfragmented. Verified end-to-end: a 256×128 (128 KiB) synthetic
  frame crosses a live relay byte-exact.
- **v0.11** — **remote input injection**: new command-channel opcodes
  `POINTER (0x30)` and `KEY (0x31)` (see spec §11.4). `ovhost --screen` now
  streams real pixels **and** applies remote input: normalized 0..=65535
  pointer coordinates map into the captured region, then into
  `MOUSEEVENTF_ABSOLUTE` virtual-screen space, and inject via Win32
  `SendInput` (`ovhost::input`); keys inject as VK press/release. The iOS
  viewer renders the screen in a `GeometryReader` (letterbox-mapped touch →
  POINTER down/move/up, throttled to ~30 Hz) and adds a key row (⌫ ↵ esc tab
  ←↑→↓ ctrl alt) whose taps send KEY down+up pairs — all on channel 0,
  leaving chat on channel 1 untouched.
- **v0.13** — **lossless frame compression**: `send_frame` now deflates the full frame
  with **raw DEFLATE** (`flate2`, pure-Rust `miniz_oxide` backend — no C dependency, so
  the iOS `aarch64-apple-ios` staticlib is unchanged and still builds) before streaming;
  `FRAME_BEGIN` is extended from 25 to 30 bytes with a `codec` byte (`0` raw, `1` deflate)
  and a `compressed_len` u32 (`total` still means the uncompressed length). The viewer
  (`TerminalSession::take_frame` and the iOS `OvScreen` reassembler) accumulates
  `compressed_len` wire bytes, inflates to exactly `total`, and drops corrupt/short
  streams instead of presenting torn frames; codec 0 (raw) stays valid. Desktop screens
  are mostly flat regions, so this cuts wire bandwidth ~5–15x (raw 1080p BGRA ≈ 240 Mbps
  at 30 fps → tens of Mbps). Screen streaming now uses the compressed path end to end.

### Cursor overlay channel (v0.15)

At idle the adaptive rate would freeze the mouse pointer between frames. The host now
tracks the real cursor separately (`ovhost::cursor`: `GetCursorInfo` polling +
`GetIconInfo`/`GetDIBits` rendering) and streams two cheap host→viewer commands over the
command channel (spec §11.4): `CURSOR` (0x32) position updates throttled to 30/s in
frame pixel space, and `CURSOR_SHAPE` (0x33) premultiplied-BGRA bitmaps with hotspot,
sent when the cursor shape changes. The desktop viewer composites the cursor over the
presented frame (cached display buffer, one O(cursor-area) blend per move); iOS gets
`ov_screen_set_cursor_sink` / `ov_screen_set_cursor_shape_sink` and renders a SwiftUI
overlay positioned through the same letterbox mapping.

### Change detection + adaptive capture rate (v0.14)

A static desktop no longer costs anything. The host capture loop keeps the
pixels of the last **sent** frame and, per capture, applies a cheap pre-filter
(`ovapp::adapt::frame_changed_fast`, one byte in 128 sampled) plus an exact
full compare on a "maybe" verdict: an unchanged screen sends **no frame at
all**. A pure `RateGovernor` eases the capture rate from the configured
`--fps` down to 4 fps while idle (interval x1.5 per idle capture, capped at
the idle rate) and snaps back to full rate on the first change, so motion is
picked up at base fps. Metrics (`sent` / `skipped unchanged` / current fps)
are logged every 30 s and on disconnect. Receivers (ovview, iOS) are
unchanged: they simply receive fewer frames.

### RTT-aware auto quality + QUALITY_SET (v0.17)

The capture rate now adapts to the **link**, not just to screen activity:

- **Auto mode (default)** — the host reads its own measured RTT (the v0.16
  keepalive PING/PONG exchange, smoothed EWMA in `ovapp::liveness`) every
  capture tick and applies a monotonic rule (`ovapp::adapt::auto_target_fps`):
  RTT <= 150 us → full base fps; 150 us–1 ms → half base fps; > 1 ms → 8 fps.
  The result retargets the governor via `RateGovernor::set_base_fps` without
  disturbing the idle ease-out, so a congested link degrades gracefully
  instead of lagging.
- **Manual override** — a new viewer→host command `QUALITY_SET` (0x36,
  2-byte u16-BE target fps; `0` = AUTO) pins the capture rate to a fixed
  value, bypassing the RTT rule. The last QUALITY_SET wins.
- **Desktop HUD** — the v0.16 health panel gains a fifth line, `Q AUTO` or
  `Q N fps`, and **F1** cycles AUTO → 10 → 30 → 60 fps (each step sends a
  QUALITY_SET).
- **iOS** — `SessionView` gains a segmented quality picker (Auto / 10 / 30 /
  60) that sends QUALITY_SET over the command channel; no new FFI surface is
  needed (the shell already builds raw command payloads).

## Security model

- Identity: Ed25519 signing key + X25519 static key derived from one 32-byte
  master seed (`OVID1` format); fingerprint = SHA-256(ed pubkey)[..16];
  6-digit pairing code derived from the fingerprint.
- Handshake: X25519 ephemeral + static, HKDF-SHA256 key schedule
  (`ov-client-key` / `ov-server-key`), READY signed with the peer's Ed25519 key,
  pairing code carried in the HELLO auth blob and checked by the server.
- Record layer: ChaCha20-Poly1305 per-direction keys, 12-byte header
  (sender id LE + 32-bit sequence LE) as AAD, independent send/recv sequence
  counters (anti-desync / anti-replay).
