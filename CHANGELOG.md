# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - Unreleased

First public release.

### Added

#### Security and sessions
- Ed25519 device identities with fingerprints and pairing codes; X25519
  handshake with signed key exchange; ChaCha20-Poly1305 AEAD record layer
  with per-direction keys and sequence checks (`ovcore`).
- Unattended access with a PIN, authenticated with SPAKE2 (handshake v2):
  no offline PIN guessing, key confirmation in both directions, the PAKE key
  and the full transcript mixed into the key schedule; host-side attempt
  throttling (lockout from 30 s doubling to 15 min) with a distinct "too
  many attempts" error; explicit incompatible-version error for v1 peers.
- Typed `HandshakeError` / `CoreError`; session keys and identity secrets
  zeroized on drop with redacting `Debug`.
- Hostile-input hardening: bounded file-transfer reassembly (concurrency,
  in-flight bytes, overlaps) and a path-safe received-file-name sanitizer;
  bounded band-container decoding; relay connection / per-IP / pending caps
  and client-frame masking; cargo-fuzz targets in `fuzz/`.
- Trust-on-first-use host pinning on iOS.

#### Transport and connectivity
- `Carrier` abstraction over UDP, TCP, in-memory and relay-tunnel transports
  (`ovtransport`).
- Reliable ordered UDP (`rudp`) with delay-based pacing.
- Direct-path upgrade: a relayed session moves to direct UDP via candidate
  exchange (LAN, STUN, UPnP IGD port mapping) and hole punching, with relay
  fallback.
- Congestion control and an unreliable media channel with Reed-Solomon FEC
  on the direct path.
- `ovrelay`: std-only rendezvous relay over TCP and WebSocket (`ws(s)://`).
- `ovdiscovery`: STUN binding client, UPnP IGD, mDNS/DNS-SD.

#### Screen streaming
- DXGI Desktop Duplication capture with dirty/move rects and GDI fallback.
- Capability-negotiated payload codecs: dirty tiles, band containers with
  DEFLATE / zstd / LZ4, scroll detection (copy-rect moves), content-aware
  JPEG tiles with lossless refinement, tile cache, and Media Foundation
  H.264 for high-motion regions (VideoToolbox on iOS).
- Event-driven send loop with prioritised control traffic and latest-wins
  frame mailbox; RTT-aware automatic quality plus manual quality selection.
- Area + sharpen scaling on CPU, Direct2D and Metal; separate cursor overlay
  channel.
- Session desktop effects (hide wallpaper, disable animations) restored on
  exit, including after a crash.

#### Interaction
- Mouse, keyboard, wheel, key combos and secure-attention requests.
- Opus audio (system loopback and microphone, full duplex) with PCM
  fallback.
- Bidirectional clipboard sync, chat and file transfer.
- Session recording (`.ovrec`) with playback and frame export.
- Multi-monitor selection.

#### Applications
- `ovhost`: Windows host with unattended service loop.
- `ovview`: Windows viewer with Direct2D presentation, HUD and hotkeys.
- `OpenView.exe` (`ovgui`): Slint desktop app that supervises the host and
  relay, shows ID and PIN, keeps a partner list, stores PINs in Windows
  Credential Manager and lives in the tray.
- iOS app (SwiftUI + Metal) linked against the `ovffi` C ABI staticlib, with
  trackpad-style touch input, iPad pointer support, special-key bar and
  session sheets for files and logs.
- `ovcommon` crate: shared data-dir rule, JSON-lines status-event schema and
  `tracing` logging setup for the Windows binaries.
- Status channel: `ovhost` / `ovview` print machine-readable status events as
  JSON lines on stdout (human logs on stderr); the GUI parses them instead of
  log text and writes child logs to `logs\ovhost.log`, `logs\ovrelay.log`,
  `logs\ovview-<id>.log` (+ `ovview-<id>.status.jsonl`).
- Portable mode: `portable.txt` next to the exe keeps data in
  `<exe dir>\data`; `ovhost --data-dir`, GUI `OPENVIEW_DATA_DIR`.
- Windows resources from `assets/winres.rs`: icon, application manifest
  (asInvoker, Windows 10/11, PerMonitorV2 DPI, Common Controls v6 for the
  GUI) and VERSIONINFO for `ovhost`, `ovview` and `OpenView.exe`.
- GUI: Relay settings card (relay address, "Run a relay on this PC"),
  Troubleshooting card with "Open logs folder", "Made with Slint" on the
  About page.

### Changed

- Data moves to `%LOCALAPPDATA%\OpenView` (host identity, GUI settings,
  logs, desktop-effects crash marker). One-time migrations: the device
  identity next to `ovhost.exe` and GUI settings in `%APPDATA%\OpenView` are
  copied over; an old crash marker next to the exe is still recovered.
- Received files go to `Downloads\OpenView` (host and viewer), never
  overwriting an existing file.
- `ovhost` / `ovview` use `clap` (`--help`, `--version`). The PIN is read
  from `OPENVIEW_PIN` (preferred; `--pin` still works but is visible to
  other processes); a PIN implies `--unattended`. Development / test
  switches are hidden from `--help`.
- Logging via `tracing` on stderr: `info` shows lifecycle only; periodic
  statistics are `debug` (`-v`), per-input-event lines `trace` (`-vv`);
  `--log-level` / `OPENVIEW_LOG` set any filter.
- `ovview --no-window` now really runs without a window.
- Media Foundation (`mfplat.dll`) is delay-loaded: on Windows N/KN without
  the Media Feature Pack, `ovhost` / `ovview` start and use lossless tiles.
  The binaries link the C runtime statically (no VC++ redistributable).
- GUI: helper binaries must sit next to `OpenView.exe` (`OPENVIEW_BIN_DIR`
  for development; a missing binary shows a clear error); the PIN is handed
  to `ovhost` / `ovview` via `OPENVIEW_PIN`, never argv; the relay address
  is empty by default and must be entered; an external host is offered
  "Stop it and take over" (its PIN is no longer read).

### Removed

- All `OV_*` / `OVHOST_*` / `OVVIEW_*` environment hooks (`OV_RELAY`,
  `OV_VERBOSE`, `OV_DRY_INPUT`, `OV_CAPTURE`, `OVHOST_H264`, `OV_VIDEO_BPP`,
  `OV_NO_MEDIA`, `OVHOST_NO_PACING`, `OV_PATTERN_*`, `OVVIEW_NO_H264`,
  `OVVIEW_RENDERER`); use the flags (`--video-bpp`, `--no-media`,
  `--no-pacing` are hidden). The default relay is `127.0.0.1:4569`.
- `--clipboard` (`ovhost`, `ovview`) and `ovhost --gpu-encode` (both are
  the default); `ovhost --listen` is a hidden no-op.
- GUI: "Program folder" setting and `target\release` fallback, legacy
  "OpenView Host" scheduled-task detection, PowerShell command-line reading
  of external hosts, `OPENVIEW_THEME`, `OPENVIEW_DEV_FAKE_TASK`,
  `OPENVIEW_DEV_EXTERNAL_IMAGE`.
- `assets/openview.rc` and the runtime DPI-awareness calls in `ovhost` /
  `ovview` (the manifest covers it).

### Security

- SEC-2: the PIN no longer has to appear on a command line (see
  `docs/KNOWN_ISSUES.md`).

[0.1.0]: https://github.com/OWNER/openview/releases/tag/v0.1.0
