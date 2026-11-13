# OpenView documentation

## Using OpenView

| Page | What it covers |
|---|---|
| [INSTALL.md](INSTALL.md) | Installer and portable zip, upgrade and uninstall, data locations, firewall, start at sign-in |
| [USAGE.md](USAGE.md) | The Windows app, every command-line flag of `ovhost` / `ovview` / `ovrelay`, recipes (LAN, internet, WebSocket relay, unattended host, recording, audio, files) |
| [TROUBLESHOOTING.md](TROUBLESHOOTING.md) | Logs, common problems, every error message with its cause and fix |
| [KNOWN_ISSUES.md](KNOWN_ISSUES.md) | Open bugs and limitations |
| [../ios/README.md](../ios/README.md) | The iOS app: features, relay configuration, building |
| [../SECURITY.md](../SECURITY.md) | Threat model, known limitations, reporting vulnerabilities |

## Developing OpenView

| Page | What it covers |
|---|---|
| [BUILDING.md](BUILDING.md) | Windows and iOS builds, tests, packaging, release workflow |
| [../CONTRIBUTING.md](../CONTRIBUTING.md) | Workspace layout, checks, commit style, pull requests |
| [ARCHITECTURE.md](ARCHITECTURE.md) | How the crates and apps fit together |
| [WIRE_PROTOCOL.md](WIRE_PROTOCOL.md) | Handshake, record layer, relay and command formats; with the command table in [crates/ovapp.md](crates/ovapp.md), enough to implement a compatible peer |
| [MEDIA_PIPELINE.md](MEDIA_PIPELINE.md) | Screen capture, codecs, pacing and audio, end to end |
| [IOS_APP.md](IOS_APP.md) | iOS app architecture, flows, rendering and input; C ABI in [crates/ovffi.md](crates/ovffi.md) |
| [../CHANGELOG.md](../CHANGELOG.md) | Release history |

### Per-crate references

| Crate | Role | Reference |
|---|---|---|
| `ovcore` | Identities, handshake, record layer | [crates/ovcore.md](crates/ovcore.md) |
| `ovtransport` | Carriers, direct path, congestion control, media channel | [crates/ovtransport.md](crates/ovtransport.md) |
| `ovrelay` | Relay server | [crates/ovrelay.md](crates/ovrelay.md) |
| `ovdiscovery` | STUN, UPnP, mDNS | [crates/ovdiscovery.md](crates/ovdiscovery.md) |
| `ovapp` | Application layer and codecs | [crates/ovapp.md](crates/ovapp.md) |
| `ovhost` | Windows host | [crates/ovhost.md](crates/ovhost.md) |
| `ovview` | Windows viewer | [crates/ovview.md](crates/ovview.md) |
| `ovgui` | Windows app (`OpenView.exe`) | [crates/ovgui.md](crates/ovgui.md) |
| `ovffi` | C ABI for iOS | [crates/ovffi.md](crates/ovffi.md) |
| `ovcommon` | Data dirs, status events, logging | [crates/ovcommon.md](crates/ovcommon.md) |

## Glossary

| Term | Meaning |
|---|---|
| **Host** | The computer being controlled. It runs `ovhost` (started by the Windows app) and shares its screen, sound and input. Windows 10/11 only. |
| **Viewer** | The side that watches and controls: `ovview` on Windows (opened by the Windows app) or the iOS app. |
| **Relay** | `ovrelay`, a small rendezvous server that pipes the encrypted traffic of a host and a viewer that cannot reach each other directly. It only ever sees ciphertext. Self-hosted: run it on a PC, a Linux server or in Docker. Default TCP port 4569. |
| **Device ID** | A host's stable public number, derived from its identity key and shown as digit groups (for example `123 456 789 012`). Viewers dial it; it is not a secret. |
| **PIN** | The password a host owner sets for **unattended access**. Verified with SPAKE2, so it never crosses the network and cannot be guessed offline. |
| **Pairing code** | A random **one-time** 10-digit code (`1234 567890`) the host shows for **attended access**. The first four digits are public (they name the relay slot), the other six are the secret; the host shows a new code after every attempt. |
| **Short code** | Six digits derived from a device's public fingerprint (`Identity::short_code`). Public, never a credential. |
| **Direct path** | After a session starts on the relay, the peers try to reach each other over UDP (LAN, STUN or UPnP addresses, default port 4570) and move the session there when it works, falling back to the relay if it stops. |
| **LAN session** | A session with no relay at all: an unattended host announces itself on the local network over mDNS / Bonjour, and viewers on the same network dial it directly on UDP 4571. |

## Conventions

- Reference pages cite source paths relative to the repository root. If
  code and documentation disagree, the code wins; please fix the page.
- Endianness differs by layer (for example the record header is
  little-endian, most command payloads big-endian); always check the table
  for the field.
