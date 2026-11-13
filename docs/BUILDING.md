# Building OpenView

OpenView is a Cargo workspace (Rust) plus a SwiftUI app for iOS. The Windows
binaries build on Windows only; the relay (`ovrelay`) and the protocol
crates also build on Linux and macOS; the iOS app needs a Mac.

| Output | Crate | Platform |
|---|---|---|
| `OpenView.exe` | `crates/ovgui` | Windows |
| `ovhost.exe` | `crates/ovhost` | Windows |
| `ovview.exe` | `crates/ovview` | Windows |
| `ovrelay(.exe)` | `crates/ovrelay` | Windows, Linux, macOS |
| `OvFFI.xcframework` + iOS app | `crates/ovffi` + `ios/` | macOS (Xcode) |

## Windows

### Prerequisites

1. **Rust** via [rustup](https://rustup.rs). The repository pins the
   toolchain in `rust-toolchain.toml` (stable, with `rustfmt`, `clippy` and
   the iOS targets); rustup installs it automatically on the first `cargo`
   command, or run `rustup toolchain install` in the repository. The
   minimum supported Rust version is `rust-version` in `Cargo.toml`.
2. **Visual Studio 2022 Build Tools** (or Visual Studio) with the
   *Desktop development with C++* workload: the MSVC compiler and linker
   and a Windows SDK.
3. **CMake** on `PATH` (libopus is compiled from source):
   `winget install Kitware.CMake`.
4. Optional: **Git for Windows** (provides `bash` for
   `scripts/e2e_relay.sh`) and **Inno Setup 6**
   (`winget install JRSoftware.InnoSetup`) to build the installer.
   `scripts/package.ps1` runs on the built-in Windows PowerShell 5.1 or on
   PowerShell 7.

### Build

```powershell
cargo build --release                 # all workspace binaries
# or only the Windows app and its helpers:
cargo build --release --locked -p ovgui -p ovhost -p ovview -p ovrelay
```

The executables land in `target\release\`. `OpenView.exe` expects
`ovhost.exe`, `ovrelay.exe` and `ovview.exe` in its own folder, which is
the case there. The release profile uses thin LTO and strips symbols;
`.cargo/config.toml` links the C runtime statically, so the binaries run on
a clean Windows 10/11 without the Visual C++ redistributable.

Debug builds (`cargo build`) open a console window for `OpenView.exe`, which
is handy for logs.

### Running a development build safely

To try the GUI without touching an installed OpenView (its relay port,
data, credentials, sign-in entry or running processes), use **test mode**:

```powershell
$env:OPENVIEW_DEV_PORTS = "14600"     # any free port
target\release\OpenView.exe
```

Everything then runs on `127.0.0.1:14600` with simulated input and no
desktop, UPnP or direct-path side effects; data goes to
`%LOCALAPPDATA%\OpenView-dev`. Details: [USAGE.md](USAGE.md#openviewexe-ovgui).

## Tests

```powershell
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
bash scripts/e2e_relay.sh            # relay + host + viewer as 3 processes, port 14569
bash scripts/e2e_lan.sh              # LAN: unattended host + ovview --id, no relay, ports 24600-24609
```

`scripts/e2e_relay.sh [PORT]` builds the release binaries, starts `ovrelay`,
an attended `ovhost` and an `ovview` that joins with the host's pairing
code, sends a chat line and checks that it arrives and is echoed, and that
both sides exit cleanly. It runs under Git Bash on Windows and natively on
Linux/macOS.

`scripts/e2e_lan.sh [PORT] [--no-mdns]` starts an unattended `ovhost` whose
relay address points at a closed port (scratch direct port `PORT`, LAN port
`PORT+1`), finds it with `ovview --id <device id>` by mDNS and checks the
PIN handshake and the chat echo over the LAN path. `--no-mdns` dials
`127.0.0.1` with `--lan-addr` instead (machines where multicast is
filtered).

Fuzzing targets for the parsers live in `fuzz/` (nightly + `cargo-fuzz`,
see [`fuzz/README.md`](../fuzz/README.md)).

Supply-chain checks use [cargo-deny](https://github.com/EmbarkStudios/cargo-deny)
with `deny.toml`:

```bash
cargo install cargo-deny --locked
cargo deny check
```

These are the same checks CI runs (`.github/workflows/ci.yml`).

## Packaging (Windows)

```powershell
powershell -ExecutionPolicy Bypass -File scripts\package.ps1               # build + package
powershell -ExecutionPolicy Bypass -File scripts\package.ps1 -SkipBuild    # package the existing target\release binaries
powershell -ExecutionPolicy Bypass -File scripts\package.ps1 -NoInstaller  # zip only
```

It writes to `dist\`:

| File | Contents |
|---|---|
| `OpenView-<ver>-windows-x64-portable.zip` | Folder `OpenView-<ver>\` with the four exes, `LICENSE`, `THIRD_PARTY_LICENSES.txt`, `README.txt`, `firewall.cmd` and `portable.txt` |
| `OpenView-<ver>-setup.exe` | Per-user installer built from `packaging/windows/openview.iss` (only when Inno Setup 6 is installed; skipped with a warning otherwise) |
| `SHA256SUMS` | `sha256sum`-compatible checksums |

The version is read from `[workspace.package]` in `Cargo.toml`.
`THIRD_PARTY_LICENSES.txt` is generated with
[cargo-about](https://github.com/EmbarkStudios/cargo-about) when it is
installed (config `packaging/about.toml` / `about.hbs`), otherwise from
`cargo metadata`: every crate linked into the four binaries with its
license, the license texts shipped in each crate (identical texts printed
once), and the notices of the bundled C libraries libopus and zstd.

Releases are built by `.github/workflows/release.yml` when a `v*` tag is
pushed. It checks that the tag matches the versions in `Cargo.toml` and
`ios/OpenView/project.yml` (and `Info.plist`), runs the whole CI workflow,
then runs `package.ps1 -RequireCargoAbout` on Windows, builds static
`ovrelay` binaries for Linux x86_64 / aarch64 (musl, `cargo zigbuild`) and
the unsigned iOS `.ipa` on macOS, and publishes everything with
`SHA256SUMS`, build provenance attestations and the matching
`CHANGELOG.md` section as release notes (rename `## [Unreleased]` to
`## [x.y.z] - date` before tagging). A tag containing `-` (for example
`v0.2.0-rc.1`) is published as a pre-release.

## iOS app

### Prerequisites

- macOS with **Xcode 15** or newer (and its command line tools)
- [XcodeGen](https://github.com/yonaskolb/XcodeGen): `brew install xcodegen`
- Rust via rustup with the iOS targets (already listed in
  `rust-toolchain.toml`; otherwise
  `rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios`)

The iOS library `ovffi` is pure Rust: no C toolchain beyond Xcode is needed.

### Build

```bash
ios/build_ios.sh --sim          # simulator .app, no signing
ios/build_ios.sh --test         # unit tests on an iOS simulator
ios/build_ios.sh --device       # unsigned device build: ios/build/OpenView-unsigned.ipa
ios/build_ios.sh --ipa          # signed archive + .ipa (needs a development team)
ios/build_ios.sh --xcframework  # only ios/build/OvFFI.xcframework
cargo test -p ovffi             # the Rust side of the C ABI, on the Mac itself
```

Signing team, bundle id prefix and a baked-in default relay are set in
`ios/OpenView/Config/Local.xcconfig` (git-ignored). See
[ios/README.md](../ios/README.md) and [IOS_APP.md](IOS_APP.md).

CI builds and tests the app on every change to the shared crates or `ios/`
(`.github/workflows/ios.yml`).
