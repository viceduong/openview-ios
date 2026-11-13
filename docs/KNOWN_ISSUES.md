# Known issues

Open bugs and limitations of the current release. Each entry cites where in
the code it lives, so it can be confirmed and picked up quickly;
contributions are welcome (see [CONTRIBUTING.md](../CONTRIBUTING.md)).
Report new problems on the issue tracker, and security problems privately
([SECURITY.md](../SECURITY.md)).

Severity: **S2** broken feature · **S3** wrong behaviour with a workaround ·
**S4** cosmetic.

## Open issues

| # | Sev | Where | Issue |
|---|---|---|---|
| BUG-3 | S3 | `crates/ovview/src/session/keys.rs` | Only F8 is kept local. F1–F6, `H` and `M` are both handled locally **and** forwarded, so typing "h"/"m" in a remote app toggles the HUD / mutes audio. Fix: modifier-gated hotkeys or a host-key chord. |
| BUG-5 | S3 | `crates/ovhost` `--service` | `--service` is a reconnect loop, not a Windows service. Secure-desktop input (the real Ctrl+Alt+Del, UAC prompts, the lock screen) needs SYSTEM-session plumbing that does not exist yet. |
| BUG-6 | S3 | iOS `SessionContainer.swift`, `FloatingToolbar.swift` | The chat sheet exists but nothing opens it (incoming chat only bumps `unreadChat`). `sendSAS`, `sendCombo`, `sendClipboardNow` and `typeClipboardText` have no UI yet. |
| BUG-7 | S3 | iOS `RemoteAudioPlayer.swift` | The Opus sink's "packet lost" signal (empty packet) is dropped by a guard, so no packet-loss concealment runs. |
| BUG-8 | S4 | iOS `SessionDialer.swift` (`PairingState`), Settings | The "This device's short code" row (Settings) shows the short code of a throwaway identity generated per launch; each session makes its own identity. The app is viewer-only, so the value has no use. |
| BUG-9 | S4 | `crates/ovapp/src/terminal/command.rs` | `CAND_UPNP` is defined but `candidate_kind()` never returns it: UPnP-mapped candidates are reported as STUN candidates. |

## Not yet implemented (not bugs)

- Hosting on macOS or Linux (the host is Windows-only).
- iOS: no microphone uplink, no hosting (ReplayKit).
- Recordings (`.ovrec`) store video frames only, no audio.
- No acoustic echo cancellation for full-duplex audio.
- One session per host process; no multi-viewer.
- KEX is X25519-only (no P-256 or post-quantum hybrid key exchange).
