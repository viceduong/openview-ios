# OpenView iOS app

The SwiftUI viewer for OpenView: it controls a remote desktop running
`ovhost`, on iPhone and iPad (iOS 15.6+). All protocol logic lives in the
Rust library `crates/ovffi` (identity, handshake, AEAD records, relay
tunnel, reliable UDP, direct-path engine, tile and H.264 demux, liveness,
file transfer), shipped to Xcode as `OvFFI.xcframework`. Swift owns the
sockets, renders with Metal and VideoToolbox, maps input and provides the UI.

- **App architecture, flows, rendering, input, settings:**
  [../docs/IOS_APP.md](../docs/IOS_APP.md)
- **C ABI reference (`shim.h` / ovffi):**
  [../docs/crates/ovffi.md](../docs/crates/ovffi.md)

## Features

**Connecting**

- Device ID + access PIN (unattended). Saved devices with a TOFU host
  fingerprint and Keychain PINs, optionally protected by Face ID.
- One-off pairing-code sessions.
- Three transports:
  - relay over WebSocket (`wss://host/relay`),
  - relay over TCP (`host[:4569]`),
  - LAN UDP (`host:4570`).
- Relay sessions upgrade automatically to a direct UDP path when the host
  is reachable.

**Display**

- Zero-copy dirty-rect uploads into a Metal texture, with area / bilinear
  scaling and sharpening.
- H.264 overlay for high-motion regions (VideoToolbox).
- Local cursor and multi-monitor picker.

**Input**

- Trackpad-style touch gestures.
- iPad pointer, scroll and pointer lock.
- Hardware keyboard with Cmd→Ctrl.
- Soft keyboard with a special-keys bar.

**Other**

- PC audio (Opus or PCM), clipboard sync, file send and receive.
- Quality control, auto-reconnect and a watchdog.

## Building

### Prerequisites

- macOS with **Xcode 15** or newer
- [XcodeGen](https://github.com/yonaskolb/XcodeGen): `brew install xcodegen`
- Rust via [rustup](https://rustup.rs) with the iOS targets:

  ```bash
  rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios
  ```

### Build

From the repository root:

```bash
ios/build_ios.sh --sim          # simulator .app, no signing (Apple silicon and Intel Macs)
ios/build_ios.sh --device       # unsigned device .ipa -> ios/build/OpenView-unsigned.ipa
ios/build_ios.sh --ipa          # signed archive + .ipa export (needs a development team)
ios/build_ios.sh --test         # unit tests on an iOS simulator
ios/build_ios.sh --xcframework  # only ios/build/OvFFI.xcframework
```

The script builds `ovffi` for the device and both simulator architectures,
merges the simulator slices with `lipo`, packs them with the C header and
module map into `ios/build/OvFFI.xcframework`, generates
`ios/OpenView/OpenView.xcodeproj` with XcodeGen and runs `xcodebuild`.
Everything it produces lives in `ios/build/` (git-ignored). After the first
run you can also open `ios/OpenView/OpenView.xcodeproj` in Xcode; re-run
`ios/build_ios.sh --xcframework` whenever the Rust code changes.

The unsigned `.ipa` is meant for Sideloadly or AltStore, which re-sign it
with a free Apple ID.

### Screenshots

`scripts/ios_screenshots.sh` (macOS) builds a Debug simulator app, boots an
iPhone simulator and writes `docs/images/ios-home.png`, `ios-settings.png`
and `ios-home-dark.png`, using the app's Debug-only `-OVScreenshotMode`
launch argument (in-memory demo devices; your saved devices are untouched).
`--skip-rust` reuses an existing `ios/build/OvFFI.xcframework`, `--no-dark`
skips the dark shot, `--device "iPhone 15 Pro"` picks a simulator.

### Bundle id, signing team

Build settings live in `OpenView/Config/Base.xcconfig`. Put personal
overrides in `OpenView/Config/Local.xcconfig` (git-ignored), for example:

```
OV_BUNDLE_ID_PREFIX = com.example
DEVELOPMENT_TEAM = ABCDE12345
```

The app's bundle id is `$(OV_BUNDLE_ID_PREFIX).openview`
(default `org.openview.openview`).

## Relay configuration

The app needs a relay that it and the PC can both reach. `OpenView.exe`
runs one on the PC by default (TCP 4569), which works on your LAN; to reach
the PC from elsewhere, expose that relay through an HTTPS reverse proxy or
tunnel, or run `ovrelay` on a server (see
[Reaching the PC from anywhere](../README.md#reaching-the-pc-from-anywhere)).
Then point the app at it:

- **First launch:** the app asks for a "Relay server". Enter
  `wss://relay.example.com/relay` (WebSocket, works behind an HTTPS reverse
  proxy or tunnel), `ws://host/relay`, or `host:port` (plain TCP, default
  port 4569). Choose **Skip — LAN / direct only** to use LAN connections only.
- **Later:** Settings > Connection > relay field. Connections that need a
  relay while none is set are refused with a message that links to Settings.
- **Bake it into your build** (no code changes): set `OV_DEFAULT_RELAY_URL`
  in `OpenView/Config/Local.xcconfig`. It becomes the `OVDefaultRelayURL`
  Info.plist key, which the app uses whenever the user has not entered a
  relay, and the first-run sheet is skipped. `//` starts a comment in
  `.xcconfig` files, so write the URL with an empty `$()` in between:

  ```
  OV_DEFAULT_RELAY_URL = wss:/$()/relay.example.com/relay
  ```

## Layout

```
ios/
  build_ios.sh                 build script (XCFramework + XcodeGen + xcodebuild)
  OvFFI/include/               shim.h (C declarations of ovffi) + module.modulemap (module OvFFI)
  build/                       build outputs (git-ignored): OvFFI.xcframework, DerivedData, .ipa
  OpenView/
    project.yml                XcodeGen spec (targets OpenView and OpenViewTests)
    Config/Base.xcconfig       bundle id prefix, baked-in relay; Local.xcconfig overrides
    OpenView/
      App/                     app entry, Home, first-run relay sheet, Info.plist, privacy manifest
      Session/                 SessionController (+Transport, +Screen, +Commands, +Files), dialer, session UI
      Screen/                  remote screen view, viewport math, Metal renderer, H.264 overlay
      Input/                   input encoding, gestures, keyboards, pointer
      Transport/               OvFFI.swift (Rust wrappers), WebSocket / UDP / direct-path sockets
      Settings/                settings, saved connections, relay address parsing
      Support/                 os.Logger categories, diagnostics log, hang monitor
    OpenViewTests/             XCTest unit tests (pure logic)
```

See the source map in [IOS_APP.md §1](../docs/IOS_APP.md#1-source-map).

## Privacy, logs and export compliance

- `App/PrivacyInfo.xcprivacy` declares no tracking and no collected data;
  the only required-reason API used is `UserDefaults` (reason `CA92.1`).
- Diagnostic logs (errors, connection events, network changes, hangs; at
  most 5 files of 1 MB) stay on the device. Nothing is uploaded
  automatically; Settings > About > Share diagnostic logs hands them to the
  share sheet. Developer detail goes to the unified log (`os.Logger`,
  subsystem = the bundle id).
- The app uses encryption (TLS for `wss://` relays and its own end-to-end
  session encryption). `ITSAppUsesNonExemptEncryption` is intentionally not
  set in Info.plist: if you distribute a build through the App Store or
  TestFlight, answer the export-compliance questions in App Store Connect
  for your build (and add the key yourself once you know the answer).

## Protocol quick notes

- Channel 0 of the AEAD record layer carries terminal commands: screen,
  input and control. Channel 1 carries chat text.
- Relay session id:
  - device sessions use the low 32 bits of the device ID,
  - pairing sessions use the first 4 bytes of the code, big-endian
    (`ov_relay_session_id`).
- All session, screen and transport work runs on one serial queue,
  `OvQueues.session`. FFI sinks fire there under the Rust session lock, so
  they must never call back into the session (details at the top of
  `Session/SessionController.swift` and `Transport/OvFFI.swift`).
