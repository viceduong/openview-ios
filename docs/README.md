# OpenView documentation

Every page was written from the
source and cites file paths; if code and doc disagree, the code wins — please
fix the doc.

## Start here

| If you want to… | Read |
|---|---|
| Know what works and what doesn't | [`KNOWN_ISSUES.md`](KNOWN_ISSUES.md) |
| Run it | [`USAGE.md`](USAGE.md) — all flags, recipes, troubleshooting |
| Understand the system | [`ARCHITECTURE.md`](ARCHITECTURE.md) |
| Implement a compatible peer | [`WIRE_PROTOCOL.md`](WIRE_PROTOCOL.md) + command table in [`crates/ovapp.md`](crates/ovapp.md) |
| Tune or extend screen / audio | [`MEDIA_PIPELINE.md`](MEDIA_PIPELINE.md) |
| Work on the iPhone app | [`IOS_APP.md`](IOS_APP.md), [`crates/ovffi.md`](crates/ovffi.md), [`../ios/README.md`](../ios/README.md) |
| See how we got here | [`../CHANGELOG.md`](../CHANGELOG.md) |

## Per-crate references

| Crate | Doc |
|---|---|
| `ovcore` | [crates/ovcore.md](crates/ovcore.md) |
| `ovtransport` | [crates/ovtransport.md](crates/ovtransport.md) |
| `ovrelay` | [crates/ovrelay.md](crates/ovrelay.md) |
| `ovdiscovery` | [crates/ovdiscovery.md](crates/ovdiscovery.md) |
| `ovapp` | [crates/ovapp.md](crates/ovapp.md) |
| `ovhost` | [crates/ovhost.md](crates/ovhost.md) |
| `ovview` | [crates/ovview.md](crates/ovview.md) |
| `ovgui` | [crates/ovgui.md](crates/ovgui.md) |
| `ovffi` | [crates/ovffi.md](crates/ovffi.md) |
| `ovcommon` | [crates/ovcommon.md](crates/ovcommon.md) |

## Conventions

- Paths are relative to the repository root unless stated.
- "Phase N" / "Wave N" / "Section N" labels match the module headers in the
  code and the CHANGELOG workstreams.
- Endianness differs by layer (e.g. record header is little-endian, most
  command payloads big-endian) — always check the table for the field.
