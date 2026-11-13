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

## Conventions

- Reference pages cite source paths relative to the repository root. If
  code and documentation disagree, the code wins; please fix the page.
- Endianness differs by layer (for example the record header is
  little-endian, most command payloads big-endian); always check the table
  for the field.
