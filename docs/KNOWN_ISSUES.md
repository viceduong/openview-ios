# Known issues

Open bugs and limitations of the current release, plus security items that
were fixed (kept for reference). Each entry cites where in the code it
lives, so it can be confirmed and picked up quickly; contributions are
welcome (see [CONTRIBUTING.md](../CONTRIBUTING.md)). Report new problems on
the issue tracker, and security problems privately
([SECURITY.md](../SECURITY.md)).

Severity: **S1** security / data loss · **S2** broken feature · **S3** wrong
behaviour with a workaround · **S4** docs/cosmetic.

## Security

| # | Sev | Where | Issue |
|---|---|---|---|
| SEC-0 | fixed | `crates/ovcore` handshake | ~~v1 PIN proof `HMAC(PBKDF2(PIN, fixed salt), …)` sent in the host's HELLO let anyone who reached the host brute-force the PIN offline.~~ Fixed in handshake v2: SPAKE2 with key confirmation in both directions (no offline guessing; one guess per attempt) plus host-side attempt throttling. |
| SEC-8 | fixed | `crates/ovcore` handshake, `ovhost`, `ovview`, `ovffi` | ~~Pairing-code mode was not a PAKE: the 6-digit code, derived from the host's public fingerprint, travelled in the client HELLO, and the relay session ID was its first 4 digits.~~ Fixed in handshake v3: random one-time 10-digit codes (4 public rendezvous digits for the relay + 6 secret digits), authenticated with SPAKE2 under pairing-specific identities with key confirmation both ways; the code is burned after one attempt and attempts are throttled. |
| SEC-1 | fixed | `crates/ovapp/src/file_transfer.rs` | ~~Drive-relative / ADS / reserved-name escape in `sanitize_name`.~~ Fixed: `:` and Windows-invalid characters are replaced, trailing dots/spaces trimmed, reserved device names prefixed, length capped; every result is a single `Normal` path component (tested). `unique_name` resolves collisions with ` (n)` suffixes. |
| SEC-2 | fixed | `crates/ovhost` / `crates/ovview` arg parsing, `crates/ovgui/src/host.rs` | ~~The unattended PIN is passed as `--pin <PIN>` on the command line, so it is visible to every local user via the process list; the GUI even reads it back from an external host's command line.~~ Fixed: the PIN travels in the `OPENVIEW_PIN` environment variable (the GUI always uses it for `ovhost` and `ovview`); `--pin` remains for manual use, with a visibility warning in its help; the GUI no longer reads other processes' command lines (an external host is only offered "Stop it and take over"). |
| SEC-3 | fixed | `crates/ovffi/src/session.rs` (`ov_session_pump`), `crates/ovffi/src/relay.rs` (`pump_relay`) + iOS `SessionController+Transport.swift` | ~~Every pump failure was collapsed to `-3`, which Swift treated as *authentication failed*, deleting the stored Keychain PIN on a transient network error.~~ Fixed: the pump flushes pending handshake output (REJECT / CONFIRM) before returning and reports distinct codes: `-3` only for a real credential failure (`HandshakeError::is_auth_failure`), `-7` rate limited (`ov_session_retry_after_secs`), `-8` incompatible version, `-9` other handshake errors, transport errors stay `-4`. The app discards a saved PIN only on `-3`. Covered by `crates/ovffi/src/tests/pin.rs`. |
| SEC-4 | fixed | `crates/ovrelay` | ~~No cap on parked sessions; `'O'` before capacity checks.~~ Fixed: total / per-IP connection caps at accept, pending-session cap checked before `'O'` (`XT` instead), overall pre-session deadline; all exposed as CLI flags. |
| SEC-5 | fixed | `crates/ovcore` handshake | ~~Static X25519 key not mixed into the key schedule.~~ Fixed in handshake v2: IKM = ephemeral DH ‖ static DH [‖ PAKE key], salted with the transcript hash. |
| SEC-6 | fixed | `crates/ovcore` record layer | ~~No replay/reorder window; records had to arrive exactly once and in order.~~ Fixed in record format v3: a 64-bit sequence number in the authenticated header (and nonce) and a 2048-record IPsec-style anti-replay window; duplicates / too-old records are dropped silently and counted, reordering on datagram paths is accepted. (The AAD channel byte is still not on the wire, by design.) |
| SEC-7 | fixed | `crates/ovrelay/src/ws.rs` | ~~WebSocket server ignored the request path and `Sec-WebSocket-Version`, and accepted unmasked client frames.~~ Fixed: upgrades only on `/relay` (`--ws-path` to change), version 13 required (`426` + `Sec-WebSocket-Version: 13` otherwise), `404` for other paths, `400` for a malformed upgrade; client frames must be masked. |

## Broken or unreachable features

| # | Sev | Where | Issue |
|---|---|---|---|
| BUG-1 | fixed | `crates/ovrelay/src/main.rs` | ~~`ovrelay` defaulted to port 4647 while clients dial 4569.~~ The relay now defaults to `0.0.0.0:4569`. |
| BUG-2 | fixed | `crates/ovview/src/main.rs` | ~~Viewer stdin lines are pushed into a channel nothing drains; `--no-window` still opens a window~~ — fixed: the viewer loop drains stdin lines and `--no-window` skips the window. Still open: playback prints "Escape stops" but Escape isn't handled. |
| BUG-3 | S3 | `crates/ovview` input forwarding | Only F8 is kept local. F1–F6, `H` and `M` are both handled locally **and** forwarded, so typing "h"/"m" in a remote app toggles the HUD / mutes audio. Fix: modifier-gated hotkeys or a host-key chord. |
| BUG-4 | fixed | `crates/ovview` `--id` mode, `crates/ovhost`, `crates/ovdiscovery` | ~~`--id` (LAN) only dialed `127.0.0.1`; no binary started an mDNS `Advertiser`; browse results carried no IP and the A record was hard-coded to 127.0.0.1; one malformed mDNS packet aborted the browse.~~ Fixed: unattended hosts accept LAN sessions on UDP 4571 and announce `<device-id>._openview._udp.local` with their real LAN addresses; `ovview --id <device id>` resolves by mDNS and dials directly with the PIN; the GUI and the iOS app list hosts on the network; the browser skips malformed packets. Still open: `punch.rs` `HolePunch` / `detect_symmetric` are test-only (superseded by `direct.rs`). |
| BUG-5 | S3 | `crates/ovhost` `--service` | `--service` is a reconnect loop, not a Windows service; secure-desktop input (real Ctrl+Alt+Del / UAC) needs SYSTEM-session plumbing that does not exist yet. |
| BUG-6 | S3 | iOS `SessionSheets.swift`, `FloatingToolbar.swift` | Chat sheet is never presented (incoming chat only bumps `unreadChat`). `sendSAS`, `sendCombo`, `sendClipboardNow`, `typeClipboardText` have no UI; the menu never emits `.disconnect` / `.showKeyBar`. |
| BUG-7 | S3 | iOS `RemoteVideo.swift` / ovffi Opus sink | The Opus sink's "packet lost" signal (NULL packet) is filtered by a Swift guard, so no packet-loss concealment runs. |
| BUG-8 | S4 | iOS home screen | "Your pairing code" / fingerprint come from a throwaway identity generated per launch; each session makes its own identity. The shown code cannot reach the phone (the app is viewer-only anyway). |
| BUG-9 | S4 | `crates/ovapp` direct-path candidates | `CAND_UPNP` is defined but `candidate_kind()` never returns it (UPnP candidates are reported as another kind). |
| BUG-10 | fixed | `crates/ovcommon/src/relay_addr.rs`, `crates/ovtransport/src/relay/ws_client.rs` | ~~`ovview` / `ovhost` parsed `--relay` as a numeric socket address, so host names and WebSocket URLs failed with `bad relay ...`.~~ Fixed: one shared parser (`RelayAddr`) accepts an IP, `hostname[:port]` (resolved, every address tried) and `ws://` / `wss://` URLs; the Windows apps reach WebSocket relays through `RelayTunnelCarrier::connect_ws` (TLS via SChannel). The GUI validates the setting with the same parser. |
| BUG-11 | fixed | `crates/ovview/src/window/paint.rs` | ~~The GDI renderer (`--renderer gdi`, and the fallback when Direct2D fails) passed raster operation 0 (`BLACKNESS`) to `StretchDIBits`, painting the frame black.~~ Fixed: `SRCCOPY`, covered by a GDI unit test. |

## Stale or contradicting code comments (S4)

A contributor backlog from the documentation pass; some may already be
fixed in the source.

- `pacing::RATE_WINDOW` says "max-filter" — code uses the median after startup.
- `scroll::MIN_ROWS` says "tile rows" — compared in pixels.
- `VideoSession` usage notes recommend `submit_masked` + forced refresh; `ovhost` uses `queue_refresh`.
- `OPUS_FEC_LOSS_PERC` comment says audio is "reliable today" — audio can ride the unreliable media channel.
- `VideoRx::synced` comment describes the inverse state.
- Chat docs say NULs are trimmed "on both ends" — receiver truncates at the first NUL.
- `ov_identity_fingerprint` / `ov_identity_free` are exported but unused by Swift.
- `ovcore`: identity-blob signature documented as 32 bytes (is 64); "HELLO+READY in one datagram" is two datagrams; mutual-TLS auth mode 2 rejected as not implemented; `commands.rs`, `clocksync.rs`, `framing::Recorder`, `clock_offset_ms` unused.
- `ovtransport` direct-path module doc omits the REBIND probes.

## Not yet implemented (not bugs)

- iOS: no microphone uplink, no hosting (ReplayKit).
- Recordings (`.ovrec`) store video frames only, no audio.
- No acoustic echo cancellation for full-duplex audio.
- One session per host process; no multi-viewer.
- KEX is X25519-only (no P-256 or post-quantum hybrid key exchange).
