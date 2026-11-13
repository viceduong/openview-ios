# Contributing to OpenView

Thanks for your interest in improving OpenView! Bug reports, documentation
fixes, tests and features are all welcome. By participating you agree to
follow the [Code of Conduct](CODE_OF_CONDUCT.md).

- **Bugs and ideas:** open an issue using the templates. For larger
  changes, please discuss the approach in an issue first.
- **Security problems:** do not open an issue; see [SECURITY.md](SECURITY.md).

## Development setup

Follow [docs/BUILDING.md](docs/BUILDING.md). In short, on Windows:

```powershell
winget install Rustlang.Rustup Kitware.CMake Git.Git
# plus Visual Studio 2022 Build Tools with "Desktop development with C++"
git clone https://github.com/OWNER/openview
cd openview
cargo build
cargo test --workspace
```

The iOS app needs a Mac with Xcode 15+ and XcodeGen (`ios/build_ios.sh`).

To run the GUI without disturbing an installed OpenView, use test mode:
`$env:OPENVIEW_DEV_PORTS = "14600"; cargo run -p ovgui` (loopback relay,
simulated input, separate data and credentials).

## Workspace layout

| Path | What it is |
|---|---|
| `crates/ovcore` | Identities (Ed25519 + X25519), handshake (SPAKE2 PIN mode), AEAD record layer |
| `crates/ovtransport` | Transports (`Carrier`): UDP + reliable UDP, TCP, relay tunnel (TCP / WebSocket), in-memory; direct-path upgrade, congestion control, FEC media channel |
| `crates/ovrelay` | The relay server (`ovrelay`), std-only |
| `crates/ovdiscovery` | STUN, UPnP IGD, mDNS |
| `crates/ovapp` | Application layer: command bus and capabilities, screen codecs, pacing, audio, clipboard, file transfer, recording |
| `crates/ovhost` | Windows host (`ovhost.exe`): capture, input injection, audio, direct networking |
| `crates/ovview` | Windows viewer (`ovview.exe`): Direct2D window, input, audio |
| `crates/ovgui` | Windows app (`OpenView.exe`, Slint): supervises host and relay, launches the viewer |
| `crates/ovcommon` | Shared by the Windows binaries: data directories, JSON status events, logging |
| `crates/ovffi` | C ABI static library for the iOS app |
| `ios/` | SwiftUI app (`RemoteView`), build script, C header |
| `fuzz/` | cargo-fuzz targets (nightly) |
| `packaging/` | Installer script, portable-zip files, license templates |
| `scripts/` | `package.ps1` (release artifacts), `e2e_relay.sh` (3-process end-to-end test), `e2e_lan.sh` (LAN session without a relay) |
| `docs/` | User and developer documentation ([index](docs/README.md)) |

Start with [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md); each crate has a
reference page in [docs/crates/](docs/crates/).

## Checks

CI runs these on every pull request; please run them locally first:

```powershell
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
bash scripts/e2e_relay.sh
bash scripts/e2e_lan.sh
```

If you touch `ovffi`, `ovcore`, `ovtransport`, `ovapp` or `ios/`, the iOS
workflow also runs `cargo test -p ovffi`, `ios/build_ios.sh --sim` and
`ios/build_ios.sh --test` on macOS. Dependency changes must pass
`cargo deny check` (licenses: permissive only, see `deny.toml`).

Guidelines:

- Keep the protocol compatible: new wire formats are negotiated through
  capability bits (`CAPS`), and peers ignore what they do not know.
- No `unsafe` without a `// SAFETY:` comment explaining why it is sound.
- Network input is untrusted: bound every allocation and length, return
  errors instead of panicking, and add a test (or a fuzz target) for
  malformed input.
- Never log or print secrets (PINs, keys); never put a PIN on a command
  line in code or docs (use `OPENVIEW_PIN`).
- Update the docs with the code: flag tables in `docs/USAGE.md`, the
  per-crate page, and `CHANGELOG.md` (under *Unreleased*).

## Commit messages

We use [Conventional Commits](https://www.conventionalcommits.org/):

```
<type>(<optional scope>): <summary in the imperative, lower case>

<optional body: what and why>
```

Types: `feat`, `fix`, `perf`, `refactor`, `docs`, `test`, `build`, `ci`,
`chore`. Scopes are usually a crate (`ovhost`, `ovgui`, …) or `ios`.
Examples:

```
feat(ovview): add a host-key chord for local hotkeys
fix(ovrelay): close parked sessions that never send CONNECT
docs: explain portable mode in INSTALL.md
```

Mark breaking changes with `!` (`feat(ovcore)!: ...`) and describe them in
the body.

## Pull request checklist

- [ ] One logical change per pull request, rebased on `main`
- [ ] The checks above pass locally
- [ ] Tests added or updated for the change
- [ ] Docs and `CHANGELOG.md` updated where behaviour, flags or UI changed
- [ ] No secrets or personal data (PINs, device IDs, IP addresses,
      screenshots with real IDs) in code, tests, logs or docs

## Releases

Maintainers bump `version` in `[workspace.package]` of `Cargo.toml`, move
the *Unreleased* changelog entries under the new version with its date,
and push a `vX.Y.Z` tag. The release workflow builds the Windows zip and
installer and the iOS `.ipa` and publishes them with the changelog section
as release notes.

## License

By contributing, you agree that your contributions are licensed under the
[MIT License](LICENSE) of this project.
