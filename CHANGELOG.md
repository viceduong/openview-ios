# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Stereo system audio. With `--system-audio` the host captures the
  loopback in stereo (surround folded down) and sends 2-channel Opus
  (128 kbit/s, 10 ms frames, in-band FEC) to viewers that advertise the new
  `CAP_STEREO` (bit 18): the Windows viewer (stereo `waveOut`) and the iOS
  app (when AVAudioConverter decodes stereo Opus). Older viewers and the mic
  paths keep the mono stream. Below a 1 Mbit/s link rate the stream drops to
  mono at 64 kbit/s, and stereo returns after 5 s at 1.5 Mbit/s or more.
- Per-stage latency telemetry (`CAP_TIMING`, bit 17). The host stamps every
  screen update and H.264 unit (capture, encode, first byte sent) in a small
  TIMING record and answers PINGs with timed PONGs; the viewer measures the
  clock offset NTP-style (minimum-RTT probe) and its own reassembly, decode
  and present, and keeps rolling p50/p95 per stage. The Windows HUD shows
  `cap · enc · send · net · dec · pres = total` on its first line, the
  `metrics:` line adds session percentiles, and the iOS stats chip shows the
  end-to-end latency with the breakdown in the session sheet
  (`ov_screen_latency_stats`). Older peers are unaffected.
- Input-to-photon probe: `ovhost --latency-probe` flips a 64x64 patch on
  every input press and `ovview --latency-probe N` times N presses to the
  presented flip (p50/p95/p99). See the benchmarks section of
  `docs/USAGE.md`.

### Changed
- iOS: smooth two-finger scrolling. The pan streams high-resolution wheel
  deltas (12 units per point instead of a whole 120-unit notch per 10 pt),
  at most about 120 per second, and glides on after the lift with
  exponential-decay inertia that stops when a finger lands. The natural
  direction is unchanged. iPad trackpad scrolling uses the same paced
  sender.

## [0.1.0] - 2026-11-14

First public release (0.1.0).

### Added

#### Remote access
- Windows host (`ovhost`) with unattended access (device ID + PIN) and
  attended access (random one-time 10-digit pairing code).
- Windows desktop app (`OpenView.exe`): shows your ID and PIN, partner list,
  PINs stored in Windows Credential Manager, tray icon, light and dark theme,
  optional built-in relay.
- Windows viewer (`ovview`) with Direct2D presentation, HUD, hotkeys and
  multi-monitor selection.
- iOS viewer app (SwiftUI + Metal) with trackpad-style touch input, iPad
  pointer support, a special-key bar, file transfer and LAN discovery.

#### Connectivity
- Self-hostable relay (`ovrelay`) over TCP and WebSocket (`ws://`, `wss://`
  behind a reverse proxy); runs on Windows, Linux (static musl builds) and
  in Docker.
- Automatic upgrade to a direct UDP path (LAN, STUN, UPnP IGD candidates)
  with relay fallback.
- LAN sessions without any relay: hosts announce themselves over mDNS /
  Bonjour and are listed as **Computers on this network**.
- iOS: LAN connections with a device ID fall back to the relay. On Wi-Fi the
  app tries the LAN address first (3 s), on cellular it dials the relay
  directly; saved LAN addresses follow the PC when Bonjour sees it at a new
  address.

#### Streaming and interaction
- DXGI Desktop Duplication capture; lossless dirty tiles with scroll
  detection, compressed bands and Media Foundation H.264 for high-motion
  regions; RTT-aware automatic quality.
- Opus audio (system sound and microphone, full duplex), bidirectional
  clipboard, chat, file transfer, session recording and playback.
- H.264 is encoded in High profile (Main when the encoder refuses it); the
  encoder's long-term-reference and intra-refresh support is probed and
  logged at start-up.
- Lower display latency on Windows: the viewer presents immediately, with
  tearing allowed where supported; `ovview --vsync` restores
  vsync-locked presentation.
- Adaptive video FEC on the direct path: parity drops from 10 % to 2 % on
  a loss-free link whose round trip lets a NACK repair a frame in time, and
  returns to 10-30 % as soon as loss is measured.

#### Packaging
- Per-user Windows installer (no administrator rights), portable zip,
  Linux relay archives, an unsigned iOS `.ipa`, `SHA256SUMS`, build
  provenance attestations and `THIRD_PARTY_LICENSES.txt`.
- Docker image definition and a hardened systemd unit for the relay.

### Fixed
- iOS: a PC added from **On this network** could not be reached away from
  home: the app kept dialing its private LAN address and never tried the
  relay. Existing LAN connections get the relay fallback automatically.
- Reconnecting right after the app was closed or sent to the background
  failed ("the relay closed the connection", "error: wrongState") until the
  host gave the old session up:
  - the app says goodbye (new `BYE` command, `CAP_BYE`; `ov_session_goodbye`)
    on disconnect, termination, reconnect and after 10 s in the background
    (it reconnects on return), so the host frees its session at once — also
    when the session ran on the direct path;
  - the relay reads parked clients: one that left is never paired (the host
    was paired with the app's abandoned attempt and dropped within 0.2 s),
    WebSocket PINGs are answered (a parked app gave up after ~10 s), and a
    newer viewer replaces a parked one instead of being refused;
  - an unattended host keeps a standby park while serving and makes way for
    a waiting viewer once the current one has been silent for 2.5 s;
  - iOS no longer sends keepalives or input before the secure session is
    up, and never shows FFI usage errors in the session log.

### Security
- End-to-end encryption: X25519 key exchange with Ed25519 identities and a
  ChaCha20-Poly1305 record layer with an anti-replay window; the relay only
  sees ciphertext.
- PINs and pairing codes are verified with SPAKE2 (no offline guessing, key
  confirmation in both directions) and host-side attempt throttling.
- The PIN never appears on a command line; received file names are
  sanitized; all peer-supplied sizes are bounded.
- The relay caps total, per-IP and pending connections; cargo-fuzz targets
  cover the parsers.

[Unreleased]: https://github.com/viceduong/openview/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/viceduong/openview/releases/tag/v0.1.0
