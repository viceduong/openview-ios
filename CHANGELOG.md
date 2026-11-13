# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

#### Streaming and interaction
- DXGI Desktop Duplication capture; lossless dirty tiles with scroll
  detection, compressed bands and Media Foundation H.264 for high-motion
  regions; RTT-aware automatic quality.
- Opus audio (system sound and microphone, full duplex), bidirectional
  clipboard, chat, file transfer, session recording and playback.

#### Packaging
- Per-user Windows installer (no administrator rights), portable zip,
  Linux relay archives, an unsigned iOS `.ipa`, `SHA256SUMS`, build
  provenance attestations and `THIRD_PARTY_LICENSES.txt`.
- Docker image definition and a hardened systemd unit for the relay.

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

[Unreleased]: https://github.com/viceduong/openview/commits/main
