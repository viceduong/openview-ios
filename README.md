# OpenView iOS — CI build mirror

Public mirror of the **OpenView** iOS app (a from-scratch reimplementation of
TeamViewer, driven by wire-format reverse engineering). The private monorepo
cannot run GitHub Actions without billing, so the iOS tree and a **prebuilt
Rust FFI staticlib** (`libs/libovffi.a`, built for `aarch64-apple-ios-sim`) are
mirrored here and built on a macOS runner.

## What this is

OpenView pairs a Rust core (X25519 + ChaCha20-Poly1305, LAN + hole-punch relay
transport, screen/audio/clipboard/file channels) with a thin SwiftUI shell that
bridges the Rust FFI via a C shim (`RemoteView/include/shim.h`). Deployment
target is **iOS 15.6+**.

## Building locally (on a Mac)

```
brew install xcodegen
bash build_ios.sh --sim     # simulator .app, no signing needed
```

Or a device build (requires an Apple development account):

```
bash build_ios.sh           # archive + export .ipa
```

## CI

Pushing to `master`/`main` (or a `v*` tag) triggers a `macos-latest` build that
runs `build_ios.sh --sim` and uploads the `OpenView.app` as an artifact.
No Rust toolchain is needed here — `libs/libovffi.a` is prebuilt and committed.
When the Rust FFI changes, rebuild the staticlib and re-mirror it.

## Re-mirroring from the private repo

Copy `openview/ios/RemoteView/project.yml`, the `RemoteView/` sources tree, and
the freshly built `libs/libovffi.a` (for `aarch64-apple-ios-sim`) into this
repo, then `git push`. See the private repo's `openview/ios/build_ios.sh` for
the canonical build recipe.