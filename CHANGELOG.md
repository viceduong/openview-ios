# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Input wakes the host's capture: every injected input event pulls the
  next capture in to the active frame interval and keeps the full capture
  rate for 750 ms (the idle governor no longer lets a reaction wait up to
  250 ms for its next tick). Input-to-photon probe (`--test-pattern video`,
  loopback relay, 200 presses): p95 137-141 ms -> 38-67 ms, p99 143-174 ms
  -> 97-155 ms, p50 6-34 ms -> 12-15 ms.
- Palette bands (`CAP_PALETTE`, bit 21): band codec 4 sends few-colour
  chunks (<= 16 colours per 64 pixels: text, UI) as a palette plus 1/2/4-bit
  indices before zstd; the host falls back to plain zstd when that does not
  halve the band. Synthetic 1080p screens vs plain zstd: anti-aliased text
  2.3x smaller, a dark code editor 1.9x, flat UI / text pages 39-115x;
  photos are unaffected. Decoded in pure Rust (desktop viewer and iOS).
- Viewport priority (`CAP_VIEWPORT`, bit 22): the viewer reports what it
  shows (`VIEWPORT`, `0x3D`: rect in remote pixels + zoom); the host sends
  the visible changed tiles first and defers off-screen ones (sent once
  settled, at most 500 ms later). The iOS app reports on pan / zoom
  (throttled to 100 ms, `ov_viewport_record`); the desktop viewer reports
  its whole-frame view and scale.
- Video loss recovery without an IDR (`CAP_LTR`, bit 20). The host's
  encoder keeps 2 long-term references (NVENC and the Microsoft MFT both
  support it) and marks one picture per second (`VIDEO_LTR` flag); the
  viewer acknowledges decoded ones (`VIDEO_LTR_ACK`, `0x28`). After a loss
  the viewer sends `VIDEO_RECOVER` (`0x29`) naming the newest reference it
  holds, and the host answers with a P-frame predicted only from it
  (`VIDEO_RECOVERY`) instead of a keyframe; an IDR stays the fallback (no
  confirmed reference, decoder error, encoder without LTR). At 1080p,
  CBR 8 Mbps, the resync picture shrinks from 146 KB to 63 KB (H.264) and
  from 196 KB to 19 KB (HEVC): about 80 ms (H.264) to 180 ms (HEVC) less
  queueing on an 8 Mbit/s link. Desktop viewer, iOS assembler and host all
  speak it; `ov_screen_set_video_caps` lets the iOS shell turn it off.
- HEVC video (`CAP_HEVC`, bit 19). The `VIDEO_BEGIN` codec byte (unchanged
  38-byte header) now also takes `2` = HEVC: Annex-B, Main profile, VPS +
  SPS + PPS on every keyframe, hardware MFT only, same low-latency settings.
  The host streams HEVC only to a viewer that advertises it (desktop: an HEVC
  decoder MFT is present; iOS: `ov_screen_set_video_caps(OV_VIDEO_CAP_HEVC)`,
  `ov_video_packet` gains `codec` and `vps`/`vps_len`), and falls back to
  H.264 when the HEVC MFT does not open. Shared `video::VideoCodec`. On the
  synthetic 1080p bench NVENC HEVC is not consistently better than H.264
  High (83-113 % of the bytes, -5.7..+1.5 dB), so it stays an opt-in.
- Stable encode surface during motion: a motion region that grows past
  its encoder re-opens it once at the full screen for the rest of the
  motion period, and a growth that fits no longer forces a keyframe. On a
  steadily growing region (7 grows in 8 s): 2 encoders / 2 keyframes
  instead of 8 / 8.
- Windows viewer: hardware video decode with zero-copy present. With the
  Direct2D renderer, the Media Foundation decoder runs on the renderer's
  D3D11 device (DXVA). The D3D11 video processor converts each picture
  from NV12 to BGRA, and the renderer draws it straight from the GPU over
  the frame at the video rect. No NV12 reaches the CPU, and no framebuffer
  snapshot is made per video picture. On a 1920x1080 test pattern the
  `dec` stage went from 3.2/3.9 ms (p50/p95) to 0.2/0.8 ms, and viewer CPU
  from 8.9 % to 6.2 % of a core. The software path stays as the fallback
  (GDI renderer, no D3D11 or DXVA, recording, `--latency-probe`, GPU
  errors). The viewer decodes HEVC when an HEVC decoder MFT can be created,
  and only then advertises `CAP_HEVC`.
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
- iOS: hardware video is decoded with a `VTDecompressionSession` (real
  time) instead of an `AVSampleBufferDisplayLayer` overlay, and composited
  into the Metal frame in the same pass as the tiles (BT.709 / BT.601 /
  BT.2020 YCbCr conversion from the stream's VUI), so video is scaled and
  sharpened like the rest of the screen and the end-of-motion stale flash
  and its hide timer are gone. Decoded pictures are paced by the display
  link (up to 120 Hz on ProMotion), newest wins, never more than one
  queued. HEVC streams are decoded too (`CAP_HEVC`, advertised only with
  VideoToolbox hardware HEVC decode). Every decode failure asks the host
  for recovery instead of leaving the region black. Latency telemetry now
  ends video's decode stage at the VideoToolbox callback
  (`ov_screen_latency_video_decoded`) and its present stage at the Metal
  presented time of the drawable showing it. Without the Metal compositor
  the 11.2 `AVSampleBufferDisplayLayer` overlay is used.
- iOS: Settings > Diagnostics A/B switches against build 11.2 (legacy
  video layer, no viewport reports, classic caps via the new
  `ov_screen_mask_caps`), and a 5-second render / network pacing summary
  in the diagnostic log. Owed control replies (PONG, recovery requests,
  LTR acks) are all sent at once instead of one per pump.

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
