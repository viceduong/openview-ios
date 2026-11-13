# OpenView iOS app

The OpenView iOS app is a viewer-only SwiftUI client. It controls a remote
desktop running the OpenView host (`ovhost`). Every protocol detail lives in
the Rust core, which is linked as `OvFFI.xcframework` (the `ovffi` staticlib)
and documented in
[docs/crates/ovffi.md](crates/ovffi.md). The Swift side does four things:

- owns the sockets (UDP, WebSocket),
- renders with Metal or VideoToolbox,
- turns touch, pointer and keyboard input into wire commands,
- provides the UI.

Sources live in `ios/OpenView/OpenView/`, grouped in folders. All
paths below are relative to that folder unless noted.

| | |
|---|---|
| Targets | `OpenView` (app) and `OpenViewTests` (unit tests), XcodeGen spec `ios/OpenView/project.yml` |
| Bundle id | `$(OV_BUNDLE_ID_PREFIX).openview`, prefix `org.openview` by default (`ios/OpenView/Config/Base.xcconfig`) |
| Deployment target | iOS **15.6**, iPhone and iPad (`TARGETED_DEVICE_FAMILY 1,2`); device arm64, simulator arm64 + x86_64 |
| Swift | 5.9 |
| Rust link | `ios/build/OvFFI.xcframework` (static library + `shim.h` + module map, built by `ios/build_ios.sh`); Swift uses `import OvFFI` |
| C header | `ios/OvFFI/include/shim.h`, kept in sync with `crates/ovffi` by `crates/ovffi/src/tests/header.rs` |
| Role | Client only: every session is created with `is_server = 0` (`SessionDialer.dial`) |

---

## 1. Source map

| Folder / file | Contents |
|---|---|
| **App/** | |
| `OpenViewApp.swift` | `@main OpenViewApp`, `RootView` (Home plus Face ID app lock plus appearance), `AppLockView` |
| `AppModel.swift` | `AppModel`: the single active session, full-screen presentation, remember-on-connect (save device, TOFU pin, Keychain PIN), the "no relay configured" check |
| `HomeView.swift` | Home screen, `PinPromptView`, `PairView`, `ConnectionEditView`, `ReconnectCard`, rows, empty state |
| `RelaySetupView.swift` | First-run "Relay server" sheet |
| `ScreenshotMode.swift` | Debug-only `-OVScreenshotMode` launch mode for `scripts/ios_screenshots.sh`: in-memory demo devices, no relay sheet / app lock / LAN browse; `-OVScreenshotSettings` opens Settings |
| `Info.plist`, `PrivacyInfo.xcprivacy`, `Assets.xcassets` | Bundle metadata, privacy manifest, icon and accent colour |
| **Session/** | |
| `SessionController.swift` | Session engine: published state, phases and reconnects, app lifecycle; the threading model is documented at the top |
| `SessionController+Transport.swift` | Dialing, relay / UDP / direct-path wiring, pumps, watchdog, error routing, teardown |
| `SessionController+Screen.swift` | Screen-assembler sinks, H.264 hand-off, coalesced screen flush to main |
| `SessionController+Commands.swift` | Outbound commands: input, quality, monitor, desktop effects, clipboard, chat |
| `SessionController+Files.swift` | Sending and receiving files |
| `SessionModels.swift` | `SessionPhase`, `SessionError`, `LogLine`, `ChatMessage`, `ReceivedFile`, `IdentityMismatch` |
| `SessionDialer.swift` | `ConnectMode`, `ConnectRequest`, `SessionDialer.dial`, `PairingState` |
| `RemoteAudioPlayer.swift` | PC audio playback (PCM / Opus, jitter cushion) |
| `ClipboardBridge.swift` | Pasteboard sync with echo guard |
| `SessionHost.swift` | `SessionChrome`, `SessionHostingController` (full-screen UIKit host), `SessionPresenter` |
| `SessionContainer.swift` | The immersive session view: phase overlays, stats chip, reconnect capsule, toasts, error card, gesture help, sheet routing |
| `SessionSheets.swift` | Session menu, chat, files, session log, `DocumentPicker`, `ShareSheet` |
| `FloatingToolbar.swift` | Collapsible toolbar pill and drag handle |
| **Screen/** | |
| `RemoteScreenSurface.swift` | `RemoteScreenSurface` (SwiftUI wrapper) and `RemoteSurfaceView`: layers, rendering, cursor, keyboard |
| `RemoteSurfaceView+Viewport.swift` | Safe area / keyboard geometry, zoom presets, cursor-following viewport |
| `RemoteSurfaceView+Gestures.swift` | Trackpad model, taps, scroll, pinch, recognizer delegate |
| `RemoteSurfaceView+Pointer.swift` | iPad hover, pointer clicks, pointer lock |
| `ViewportMath.swift` | Pure zoom-scale math (fit / fill / 1:1 / open default, clamp, snap) |
| `ScreenModel.swift` | `ScreenModel`, `LivenessModel`, `PendingScreen` |
| `FrameImage.swift` | CGImage builders (fallback path, cursor sprites) |
| `MetalScreenRenderer.swift`, `RemoteScreen.metal` | `MetalContext`, `FrameUploadQueue`, `MetalScreenRenderer`; scaling and sharpening shaders |
| `RemoteVideo.swift` | `RemoteVideoPacket`, `H264SampleBuilder`, `VideoOverlay` |
| **Input/** | |
| `RemoteInput.swift` | `InputWire` encoders, `KeyStroke`, `ZoomPreset`, `RemoteInputController`, `PowerGovernor`, `FrameTicker` |
| `TrackpadTouchRecognizer.swift`, `FlingTracker.swift` | One-finger touch source; trackpad scroll inertia |
| `RemoteKeyboard.swift` | `HIDKeyMap`, `HardwareKeyRouter` (Cmd→Ctrl), `RemoteKeyInputView` (soft keyboard responder) |
| `KeyAccessoryBar.swift` | Special-keys ribbon (input accessory view) |
| `PointerSupport.swift` | `PointerHider`, `GCMouseBridge`, `PointerLockHostingController` |
| **Transport/** | |
| `OvFFI.swift` | `OvQueues`, `OvWakeTarget`, `OvSessionBox`, `OvScreenBox` (RAII wrappers; callback ownership contract at the top) |
| `RelayWebSocket.swift` | WebSocket relay transport |
| `NwUdpBridge.swift` | LAN UDP transport |
| `LanDiscovery.swift` | "On this network": `NWBrowser` for `_openview._udp` (Bonjour, TXT included), `DiscoveredPC`, and resolving a host to IPv4 + port with a short-lived UDP `NWConnection` |
| `LanTxtRecord.swift` | Parsing the host's TXT record (`id`, `v`, `name`); pure, unit-tested |
| `DirectPaths.swift` | `DirectPathSet`: per-candidate UDP paths for the direct upgrade |
| **Settings/** | |
| `AppSettings.swift` | `SettingsKey` (UserDefaults keys), `AppSettings` typed readers (incl. `bundledRelayURL`), `QualityChoice`, `DeviceID`, `DisplayTime`, `AppearanceStyle`, `Haptics` |
| `ConnectionStore.swift` | `ConnectionTransport`, `SavedConnection`, `ConnectionStore` (JSON persistence), `KeychainPIN` |
| `RelayAddress.swift` | Relay address parsing / validation |
| `SettingsView.swift` | Settings screen |
| **Support/** | |
| `Log.swift` | `os.Logger` categories |
| `DiagLog.swift` | Persistent diagnostics log, `HangMonitor`, `Breadcrumb` (debug builds) |

Outside the sources: `ios/OvFFI/include/` (C header + module map),
`ios/OpenView/Config/` (xcconfig), `ios/OpenView/OpenViewTests/`.

---

## 2. Architecture

```
OpenViewApp ─ RootView ─ HomeView ──(ConnectRequest)──► AppModel.connect
                                                           │
             SessionHostingController (UIKit, full screen) ◄┘ SessionPresenter.present
                └─ SessionContainer (SwiftUI)
                     ├─ RemoteScreenView ─ RemoteScreenSurface (UIViewRepresentable → RemoteSurfaceView)
                     ├─ FloatingToolbar / StatsChip / ReconnectCapsule / overlays
                     └─ sheets (menu, files, picker, PIN)
                                 │ observes
                     SessionController ── ScreenModel, LivenessModel, RemoteInputController
                                 │ (OvQueues.session)
               OvSessionBox ◄── SessionDialer ── RelayWebSocket | NwUdpBridge | Rust TCP relay
               OvScreenBox                        DirectPathSet (direct upgrade)
```

### Threading model

The authoritative description is the doc comment at the top of
`SessionController.swift`; in short:

- **Main thread.** All SwiftUI views, `AppModel`, `ConnectionStore`,
  `SessionChrome`, `RemoteInputController`, the published state of
  `SessionController`, `ScreenModel` and `LivenessModel`, and the renderer.
- **`OvQueues.session`.** A serial queue with `.userInteractive` QoS
  (`OvFFI.swift`). It owns all session, screen and transport work: dialing,
  pumping, sinks, `NWConnection` callbacks, WebSocket deliveries, frame
  staging, audio scheduling and file writes. Results reach main through
  `DispatchQueue.main.async`. Screen updates are coalesced under
  `screenLock` (`postScreen` / `flushScreen`, where the latest value wins).
- **Rust watcher thread.** Fires only the TCP relay wake sink. That sink
  just hops to the session queue (`OvWakeTarget`).
- **Rust callbacks** fire synchronously inside the FFI call that triggered
  them (on the session queue, under the Rust session lock), so a sink never
  calls back into the session. Callback `user` pointers are unretained; see
  the ownership notes at the top of `OvFFI.swift`.

`ScreenModel`, `LivenessModel` and `SessionController` are not marked
`@MainActor`: `SessionController` runs half its code on the session queue
and creates the models from a nonisolated initializer. Main-thread-only
members are documented as such instead.

### Ownership

- `AppModel` owns the single `SessionController` and the hosting controller.
- `SessionController` owns the `OvSessionBox`, the `OvScreenBox` (replaced
  on every attempt), the transports and the timers.
- Every closure handed down captures `self` weakly. `stop()` and `deinit`
  clear the callbacks and close the transports, so dropping the controller
  frees the Rust session.

---

## 3. Session lifecycle

`SessionController` drives a `SessionPhase` (`SessionController.swift`):

```
dialing(transport) ─► authenticating ─► waitingForFirstFrame ─► live
        │                   │                    │                │
        └──────── failed(SessionError) ◄─────────┴── transport lost
                              ▲                                   │
                              └── reconnecting(n) ◄──────────────┘  (n ≤ 3)
```

| Trigger | Effect |
|---|---|
| `start()` (from `SessionContainer.onAppear`) | Wires clipboard, input and video failure, then `beginAttempt()` |
| `beginAttempt()` | Increments the `generation`, resets per-attempt UI, arms the **15 s handshake timeout**, runs `startOnQueue` |
| `startOnQueue` | Resets attempt state, installs screen sinks on a fresh `OvScreenBox`, calls `SessionDialer.dial`, wires the transport, first `pump()`, starts a 50 ms `DispatchSourceTimer` (`tick`) |
| Transport up | `didDial` → `.authenticating` |
| State sink, or `peerFingerprint()` after the first pump | `noteEstablished` → `didEstablish` → `.waitingForFirstFrame`. Re-sends quality. Checks the TOFU pin. |
| First frame flushed to main | `markLiveIfNeeded` → `.live`, success haptic, reconnect counter reset |
| Error | `handle(error)` → `failOnQueue` → `handleFailure` |

**Error mapping** (`handle`):

| FFI error | Before establishment | After establishment |
|---|---|---|
| `-3` auth failed (wrong PIN / code) | `.authFailed` (deletes a stored PIN) | `.disconnected("secure channel error")` |
| `-7` rate limited | `.rateLimited(retryAfter:)`: "Too many attempts — try again in N s" | same |
| `-8` incompatible version | `.incompatibleVersion`: "This PC runs an incompatible OpenView version — update both apps" | same |
| `-9` other handshake error | `.transport("The secure handshake failed.")` | `.disconnected("protocol error")` |
| `-5` (closed) | `.disconnected` | `.disconnected` |
| `-4` (transport) | `.transport` | `.disconnected("network error")` |
| `-6` (internal error: Rust caught a panic) | `.disconnected("internal error")` | `.disconnected("internal error")` |
| `-1` / `-2` | logged only (deduplicated) | logged only (deduplicated) |

Only `.authFailed` counts as a credential failure; network, protocol and
rate-limit errors never touch the saved PIN. `.authFailed`, `.rateLimited`
and `.incompatibleVersion` are not auto-reconnected.

**Failures and reconnects.**

- The handshake timeout produces `.hostUnreachable`.
- **Auto-reconnect** (`handleFailure`) runs only when all of these hold:
  - the error is not `.authFailed`, `.rateLimited` or `.incompatibleVersion`,
  - the session was live at least once,
  - the app is not in the background,
  - the "Reconnect automatically" setting is on,
  - fewer than 3 attempts have been made.

  The delays are 1 s, 2 s and 3 s.
- `.authFailed` calls `onAuthFailed`. `AppModel` then deletes the stored
  PIN, and the container opens the "Re-enter PIN" sheet.
- **Watchdog** (`tick`): once established, more than 8 s without inbound
  records (`ov_screen_inbound_age_ms`) ends the attempt. This catches dead
  paths that no transport reported, such as a VPN drop.
- **Background.**
  - Entering the background releases all held input and starts a
    `beginBackgroundTask`.
  - Returning to the foreground retries a failed session that had been live.
  - The app lock re-engages on background only when no session is active
    (`RootView`).
- **Generations**: every attempt has a generation number, and late events
  from older attempts are ignored on main.
- **Stop** (`AppModel.endSession`):
  1. Persists the per-device fps and monitor.
  2. Copies the log to Settings > "Last session log".
  3. Calls `controller.stop()`, which runs `input.releaseAll()` then queues
     `teardownOnQueue`.
  4. Dismisses the session.

**Thermal and Low Power governor** (`PowerGovernor` in `RemoteInput.swift`):

- Triggers when the thermal state is `.serious`/`.critical` or Low Power
  Mode is on.
- While triggered, display links are capped at 60 Hz and the host is asked
  for at most 30 fps (`QUALITY_SET`). "Auto" then means 30.
- Both restore live when the condition clears.

---

## 4. Connection flows

`ConnectRequest` (`SessionDialer.swift`) carries:

- a **target**:
  - `.device(id, pin)`: unattended access, PIN authentication.
  - `.pairing(code)`: one-off pairing-code session.
- a `ConnectionTransport`.

`ConnectionTransport.connectMode()` picks the transport:

| Transport setting | `ConnectMode` | Who owns the socket |
|---|---|---|
| Relay URL starting `wss://` / `ws://` | `.relayWebSocket(url)` | Swift, `RelayWebSocket` (`URLSessionWebSocketTask`) |
| Relay `host` or `host:port` (default port **4569**) | `.relay(host, port)` | Rust (blocking TCP inside `ov_session_relay_connect`) |
| LAN `host` + UDP port (default **4571**, the host's LAN session port) | `.direct(host, port)` | Swift, `NwUdpBridge` (`NWConnection` UDP) |

**Relay configuration.** No relay is bundled. The relay comes from, in
order: the "Default relay URL" setting (`ov.defaultRelayURL`), else the
build-time `OVDefaultRelayURL` Info.plist key (`OV_DEFAULT_RELAY_URL` in
`Config/*.xcconfig`, empty in the repository), else nothing.

- On first launch, Home shows `RelaySetupView` unless a relay is already
  configured: enter `wss://host/relay`, `ws://host/relay` or `host:port`
  (validated by `RelayAddress.parse`), or "Skip — LAN / direct only". The
  choice is stored in the same setting; `ov.relaySetupDone` records that the
  sheet was answered.
- A connection that needs a relay while none is configured is refused before
  the session opens (`AppModel.relayMissing`, `ConnectionTransport.isMissingRelay`):
  Home shows "No relay server" with an **Open Settings** button. The dialer
  also rejects it (`ConnectionTransport.ConfigError.noRelay`) as a backstop.

**Session id and auth** (`SessionDialer.dial`):

| Target | Pairing code | Relay session id | Begin call |
|---|---|---|---|
| Device | placeholder `"000000"` | `UInt32(truncatingIfNeeded: deviceID)` (low 32 bits, matching `ovhost --unattended`) | `beginPin(pin)` |
| Pairing | the host's 10-digit one-time code (`PairView` accepts exactly 10 digits) | `ov_relay_session_id(code)` (its 4 public rendezvous digits) | `begin(authMode: 0)` (SPAKE2; the code never crosses the wire) |

### 4.1 Relay over TCP

1. `relayConnect` blocks on the session queue.
2. `setWakeHandler` installs the event-driven receive. If it returns false,
   the log says "polling every 50 ms".
3. The pump runs from three places:
   - the 50 ms tick, which calls `pump()` then `pumpRelay()`,
   - each wake, via `relayWakePump`, which calls `p2pPump`,
   - the direct-engine deadline timer.

### 4.2 Relay over WebSocket

- `RelayWebSocket` sends each Rust chunk as one binary message and delivers
  inbound messages on the session queue. It buffers until `onMessage` is
  set, keeps ordering, and delivers the close after all messages.
- A heartbeat sends a WebSocket PING every 5 s. If no PONG has arrived by
  the next PING, the socket is closed as dead.
- On each message the controller calls `relayFeed`, `pumpRelay` and
  `sendPongIfOwed`.
- On close it logs the close code, then calls `relayEOF` and `pumpRelay`.
  If the direct path is active, the session continues on UDP; otherwise it
  fails with `.disconnected`.

### 4.3 LAN direct UDP

The host's LAN session port (`ovhost --lan-port`, default 4571) accepts the
handshake HELLO from any viewer on its network, no relay involved: PIN auth
for an unattended host (device ID), pairing code for an attended one. The
session then runs on that UDP path.

**Finding hosts.** `HomeView` shows **On this network** while it is visible
and no session runs: `LanDiscovery` browses `_openview._udp` with
`NWBrowser` (`.bonjourWithTXTRecord`, no peer-to-peer). Each result's
instance name is the device ID and its TXT record carries `id`, `v` and
`name` (`LanTxtRecord`); one row per device ID, sorted by name, with a note
when `v` is not this app's protocol version. Tapping a row resolves the
service: a UDP `NWConnection` to the service endpoint, forced to IPv4, whose
`currentPath.remoteEndpoint` gives the address and the SRV port (5 s
timeout). A saved device connects with its stored PIN, name and pinned
fingerprint, using `.lan(ip, port)` for this attempt only (its saved
transport is unchanged). A new device asks for the PIN, and after the
session is established it is saved as a LAN connection.

1. `NwUdpBridge` is created **before** the first pump, because the client
   HELLO goes out during that pump. The bridge buffers up to 256 datagrams
   until the connection is `.ready`.
2. Inbound datagrams go to `onDatagram`. The pump is batched: it runs at
   once when 64 datagrams are waiting, otherwise about 1 ms later
   (`scheduleDirectPump`).
3. Each pump's outbound datagrams go out as one `NWConnection.batch`.
4. The reliable-UDP framing is done in Rust, from the first packet (the
   handshake is retransmitted when a datagram is lost).
5. A receive error other than cancellation (e.g. ECONNREFUSED after an ICMP
   port-unreachable) does not stop the receive loop; it re-arms after
   50 ms.

### 4.4 Direct upgrade next to a relay

This applies to relay sessions when the "Direct connection" setting is on
(default on). `enableDirectPath`:

1. Creates a `DirectPathSet`.
2. Installs `onDirectOpen` / `onDirectSend` and calls `setDirectSinks()`.
3. Calls `screen.setDirectCapable(true)`, so CAPS advertise
   `CAP_DIRECT | CAP_MEDIA`.
4. Arms a one-shot timer.

Behaviour once enabled:

- Rust decides everything: which candidates to probe, promotion, fallback
  and rebinds.
- `DirectPathSet` opens one UDP `NWConnection` per candidate (at most 32).
  It labels a path LAN or Internet: 10/8, 172.16/12, 192.168/16,
  169.254/16, 100.64/10 (CGNAT/Tailscale), loopback and IPv6 ULA or
  link-local count as LAN.
- A path that fails or errors on receive gets a new socket (up to 40
  times, with a backoff of 0.25 s × n capped at 2 s). The engine's REBIND
  then moves the host to the new port.
- While probing or direct, pumps run at the engine deadline, at most 20 ms
  apart (`afterRelayPump`).
- The stats chip shows "Relay", "Probing…" or "Direct (LAN|Internet)"
  (`LivenessModel.path`). Promotions and fallbacks go to the session log;
  probing and socket details go to the unified log at debug level.
- If the relay dies while the direct path is active, the session continues
  on UDP (§4.2).

### 4.5 Identity (TOFU)

- After establishment, the host's fingerprint is formatted as a "pine"
  string (`pineString`) and compared with `SavedConnection.pinnedFingerprint`.
- On a mismatch, the "Host identity changed" alert offers Disconnect or
  Continue. Continue calls `acceptNewIdentity`, which re-pins.
- Pairing sessions are never saved or pinned (`AppModel.sessionEstablished`
  returns early for `.pairing`).

---

## 5. Rendering

### 5.1 Frame path

`installScreenSinks` chooses between two paths.

**Metal (normal path).** Used when `MetalContext.shared != nil`. Frames are
zero-copy:

1. The frame sink only sets a flag.
2. After `ov_screen_feed` returns, `OvScreenBox.visitFramebuffer` hands a
   pointer to the Rust framebuffer and its dirty rects to
   `FrameUploadQueue.stage`.
3. `stage` copies only the dirty rects into pooled shared staging
   `MTLBuffer`s. It converts BGR to BGRA when needed. It uploads the full
   frame instead when there are no rects, the size changed, or more than
   24 MB is pending.
4. On its next redraw, the renderer encodes blits from staging into a
   persistent GPU-private `BGRA8Unorm` texture, in the same command buffer
   as the draw. There is no CPU wait.
5. A new surface or lost staging triggers `requestFull`, which re-stages the
   whole framebuffer (`restageFullFrame`).

**CGImage fallback** (no Metal). `FrameImage.make` builds a `CGImage` on the
session queue. BGRA is wrapped without conversion as
`noneSkipFirst | 32Little`; BGR is expanded to BGRX. The image is published
on `ScreenModel.image`.

### 5.2 Metal pipeline

`MetalScreenRenderer` and `RemoteScreen.metal` mirror
`crates/ovapp/src/scale.rs`:

- **Downscale** (`scale < 1`): a separable area filter (`ov_area_x` then
  `ov_area_y`, coverage-weighted means).
- **Upscale** (`scale ≥ 1`): centre-aligned bilinear (`ov_bilinear`).
- **Final pass** `ov_sharpen`: a 5-tap unsharp mask,
  `C + k·(C − avg(N,S,E,W))`, with `k = 0.5`. The "Sharpen" setting
  (default on) sets `k = 0` when off.
- Only the visible part of the frame is processed, at device-pixel
  resolution. There is no colour management.
- `CAMetalLayer` uses `BGRA8Unorm`, 2 drawables and one command buffer in
  flight. If the GPU is busy, the redraw is skipped and retried on the next
  display refresh, so main never blocks.
- Redraws happen only on change and are paced by `FrameTicker`, a
  `CADisplayLink` running at up to 120 Hz on ProMotion
  (`CADisableMinimumFrameDurationOnPhone`), or 60 Hz when the governor is
  constrained.

### 5.3 H.264 overlay (VideoToolbox)

This is enabled only if `VTIsHardwareDecodeSupported(kCMVideoCodecType_H264)`
(`H264SampleBuilder.available`). When enabled, `enableVideo()` advertises
`CAP_H264`, and the host streams high-motion regions as H.264.

1. `OvScreenBox` copies the `ov_video_packet` into a `RemoteVideoPacket`.
2. `H264SampleBuilder` builds a `CMVideoFormatDescription` from SPS and PPS.
   It re-creates it with colour tags (YCbCr matrix, primaries, transfer,
   full range) from the packet's VUI code points.
3. It wraps the AVCC data in a `CMSampleBuffer` marked `DisplayImmediately`
   (P-frames are also marked `NotSync`).
4. `VideoOverlay` enqueues the sample on an `AVSampleBufferDisplayLayer`
   positioned at the region's rect over the frame. Decoding happens inside
   the layer.
5. If a unit cannot be built, or the layer reports `.failed`, the app calls
   `ov_screen_request_video_idr`, deferred to the session queue.
6. `OV_VIDEO_END` marks the current video generation. The overlay is hidden
   when the next (lossless refresh) frame is **presented**
   (`requestHide` / `takePendingHide`), with a 0.5 s safety timer. This
   avoids a stale flash.

### 5.4 Cursor

- The host's CURSOR and CURSOR_SHAPE commands drive a locally drawn cursor
  sprite (premultiplied BGRA → `CGImage`, `FrameImage.makeCursor`).
- `RemoteInputController` predicts the cursor locally and ignores host
  echoes while fingers are down, then resyncs to the host position.

---

## 6. Input model

Everything goes through `RemoteInputController` (main thread), which emits
channel-0 commands via `SessionController.sendCommand`. Wire format
(`RemoteInput.swift` header):

| Op | Command | Availability |
|---|---|---|
| `0x30` | POINTER (move / left down / left up, coordinates normalized to 0..65535) | always |
| `0x31` | KEY (vk, down) | always |
| `0x34` | POINTER_EX (action, button, flags, x, y) | host `CAP_INPUT2` |
| `0x35` | WHEEL (dy, dx in 1/120 notches) | host `CAP_INPUT2` |
| `0x39` | KEY_EX (vk, set-1 scancode, down/E0 flags) | host `CAP_INPUT2` |
| `0x3A` | TEXT (UTF-8, ≤ 60 KiB chunks) | host `CAP_INPUT2` |
| `0x3B` | RELEASE_ALL | host `CAP_INPUT2` |
| `0x3C` | SAS | host `CAP_INPUT2` |

`input2` is set from `ov_screen_peer_caps` on each tick. Without it, only
the left button, legacy KEY and per-character key taps are available, and
the wheel is dropped.

Pointer moves are coalesced to the display rate (`FrameTicker`). Every DOWN
is tracked so that `releaseAll()` can undo it on background, disconnect or
re-attempt.

### 6.1 Touch gestures (`RemoteSurfaceView+Gestures.swift`)

Touch always behaves as a trackpad:

| Gesture | Action |
|---|---|
| 1-finger pan | Moves the cursor. `pct += travel / displayed image size`, sent as **absolute** moves. No acceleration. When zoomed in, the viewport keeps the cursor centred, clamped at the image edges. |
| tap | Left click at the cursor, decided 0.3 s after lift. Two taps give a double click. |
| double-tap and drag | Left drag, with a haptic and a ring indicator |
| long press 0.30 s | Right click |
| 2-finger tap / 3-finger tap | Right click / middle click |
| 2-finger pan | Wheel: 120 per 10 pt, natural direction, both axes, no inertia |
| pinch | Local zoom around the cursor. On release it snaps to fit, fill or 1:1 within ±20%. Range: fit … max(2 pt/px, fit). |
| (session start) | The viewport opens at `ZoomPreset.openDefault`: the remote screen's height fills 2/5 of the view, clamped to the zoom range (`ViewportMath`). |
| "Lock local zoom" on | Pinch becomes Ctrl+wheel on the host |

- The soft keyboard does not resize the image. Instead the viewport scrolls
  so the cursor stays at least 24 pt above the keyboard.
- The surface never takes touches inside the chrome frames
  (`chromeExclusionRects`).
- First-session gesture help is in `GestureHelp.items`.

### 6.2 iPad pointer and trackpad (`RemoteSurfaceView+Pointer.swift`, `PointerSupport.swift`)

- Hovering moves the cursor (absolute moves). Clicks and secondary clicks
  arrive as indirect pointer touches; this needs
  `UIApplicationSupportsIndirectInputEvents`.
- Trackpad scrolling produces WHEEL commands, with fling inertia from
  `FlingTracker`.
- The system pointer is hidden over the remote screen (`PointerHider`).
- **Pointer lock** (setting, or **Cmd+Shift+P**):
  `PointerLockHostingController.wantsPointerLock`. While the pointer is
  locked, raw `GCMouse` deltas and buttons are used (`GCMouseBridge`).

### 6.3 Hardware keyboard (`RemoteKeyboard.swift`)

- `HardwareKeyRouter` maps each HID usage to a Windows vk plus set-1
  scancode (`HIDKeyMap`) and sends KEY_EX down/up. Auto-repeat is left to
  the host.
- **Cmd acts as Ctrl** (setting `ov.cmdAsCtrl`, default on). While Cmd is
  held:
  - C V X Z A F S Y W T R P N O are sent as Ctrl+key.
  - `.` sends Esc.
  - Any other key is sent as Win+key.
  - Cmd pressed and released alone sends a Win tap.

  With the setting off, Cmd is simply the Windows key.
- Cmd+Shift+P toggles pointer lock and Cmd+Shift+Space toggles the toolbar;
  both stay local (`SessionHostingController.keyCommands`).

### 6.4 Soft keyboard and key accessory bar

- `RemoteKeyInputView` is an invisible `UIKeyInput` responder. Typed text
  goes to `typeText`:
  - `"\n"` becomes Enter.
  - With a sticky non-Shift modifier active, a single ASCII letter or digit
    is sent as that key, so Ctrl+C works.
  - Otherwise text is sent as TEXT, or as per-character KEYs for old hosts.
- `KeyAccessoryBar` is a horizontally scrolling ribbon:
  ⊞ Shift Ctrl Alt | ScrLk Fn | Esc Tab PrtScn Pause Ins Del Home End PgUp
  PgDn | ← ↑ → ↓.
  - **Fn** swaps in F1–F12.
  - Modifiers are sticky: one tap is one-shot, a double tap within 0.35 s
    locks.
  - Win: a tap sends Win, a double tap holds it.
  - A trailing button hides the keyboard.

---

## 7. Session chrome, toolbar and sheets

**`SessionHostingController`** (`SessionHost.swift`):

- Presented full screen with a dark style.
- Hidden status bar, auto-hidden home indicator, and all screen-edge system
  gestures deferred to the app.
- Orientation follows the setting:
  - "Lock to landscape" allows landscape only.
  - Otherwise the iPhone allows all orientations except upside-down, and the
    iPad allows all.

**`FloatingToolbar`**:

- A vertical pill with **Keyboard**, **Session menu**, **Disconnect** and
  **Collapse**.
- It collapses to a handle that can be dragged anywhere. The handle snaps to
  the nearest side edge and its position is persisted.
- With "Fade toolbar when idle" on, it fades to 70% after 3 s.
- It auto-collapses while the keyboard is up.

**`SessionContainer`** overlays, by phase:

- **Connecting**: dimmed last frame, spinner, step text, Cancel.
- **Live**:
  - Stats chip (RTT colour dot plus "Relay/Direct"; tap to expand).
  - "Reconnecting…" capsule.
  - Toasts.
  - Gesture help on first use.
- **Failed**: error card with Re-enter PIN (device sessions), Retry and
  Close.

Disconnect asks for confirmation when that setting is on. While live, the
idle timer is disabled when "Keep screen awake" is on.

**Session menu** (`SessionMenuSheet`):

| Section | Items |
|---|---|
| Transfer | Send file… (document picker), Received files (count) |
| Display | Monitor picker (more than one monitor: `CAPTURE_REGION 0x37`); Fit / Fill / Actual size (1:1); Lock local zoom; with host `CAP_DESKTOP_FX`: Hide remote wallpaper, Disable animations (`SESSION_OPTION`) |
| Quality | Frame rate segmented control: Auto / 10 / 30 / 60 fps (`QUALITY_SET 0x36`, 0 = Auto) |
| Session | Host fingerprint (copy), connection detail, Gesture help, Session log |

On establishment the saved desktop-effect wishes are sent once, and only
the "on" ones (`sendDesktopWishes`).

---

## 8. Clipboard, files, audio, chat

**Clipboard** (`ClipboardBridge`):

- The local pasteboard is polled every 500 ms. `changeCount` is checked
  first, so the text is read (which may trigger the iOS paste prompt) only
  after a change.
- New text is sent as `CLIPBOARD 0x50` with kind 0 = text.
- Peer clipboard text is written to `UIPasteboard`. An echo guard stops the
  app from sending it straight back.

**Files**:

- *Send*: a document picker selects the file. It is read with
  security-scoped access on the session queue, then sent through
  `ov_file_send`. The whole file is held in memory.
- *Receive*: the file sink writes into the app's **Documents** directory and
  never overwrites (`"a (1).txt"` etc.). Received files appear in the Files
  sheet with Quick Look and share.

**Audio** (`RemoteAudioPlayer`) plays the PC's sound:

- If `AVAudioConverter` can decode Opus (`opusDecodeAvailable`), the app
  advertises `CAP_OPUS` and decodes Opus packets. The packet duration comes
  from the TOC byte. Otherwise it receives raw s16le PCM ticks.
- Playback uses `AVAudioEngine` + `AVAudioPlayerNode` with audio session
  category `.playback` and `.mixWithOthers`, and a 10 ms I/O buffer.
- The jitter cushion has a 30 ms prefill and a 30 ms headroom. Each underrun
  adds 10 ms of prefill, up to 50 ms; 10 s without an underrun takes 10 ms
  back.
- With "Play PC sound" off, audio is dropped and the engine stopped.
- There is **no** microphone uplink on iOS. No capture code and no
  `NSMicrophoneUsageDescription` exist.

**Chat** (channel 1): `SessionController.sendChat` / `receiveChat` and
`ChatSheet` exist, but **nothing presents the chat sheet**. No code sets
`sheet = .chat`, so inbound chat only increments `unreadChat`.

---

## 9. Settings and persistence

Settings live in `UserDefaults` (`SettingsKey`, `AppSettings.swift`) and are
bound with `@AppStorage` in `SettingsView`.

| Section | Setting (key) | Default |
|---|---|---|
| Connection | Default relay URL (`ov.defaultRelayURL`) | empty (no bundled relay) |
| | Default quality (`ov.defaultQuality`) | Auto (0) |
| | Reconnect automatically (`ov.autoReconnect`) | on |
| | Connect to last device on launch (`ov.autoConnectOnLaunch`) | off |
| | Confirm before disconnecting (`ov.confirmDisconnect`) | on |
| Direct Connection | Direct connection (`ov.directConnection`) | on |
| Display | Sharpen (`ov.sharpen`) | on |
| Input | Cmd acts as Ctrl (`ov.cmdAsCtrl`) | on |
| | Pointer lock (iPad) (`ov.pointerLock`) | off |
| | Lock local zoom (`ov.lockLocalZoom`, in the session menu) | off |
| Session | Keep screen awake (`ov.keepAwake`) | on |
| | Play PC sound (`ov.playPCSound`) | on |
| | Orientation (`ov.orientation`: auto / landscape) | auto |
| | Show connection stats (`ov.showStats`) | on |
| | Fade toolbar when idle (`ov.toolbarFadeIdle`) | off |
| Security | Require Face ID to open (`ov.appLock`) | off |
| | Require Face ID for saved PINs (`ov.pinBiometric`) | off |
| Appearance | System / Light / Dark (`ov.appearance`) | system |
| About | This device's short code and fingerprint, Last session log, Share diagnostic logs | — |

Other persisted UI state:

- `ov.relaySetupDone` (first-run relay sheet answered)
- toolbar collapsed / handle edge and Y (`ov.toolbarCollapsed`,
  `ov.toolbarHandleEdge`, `ov.toolbarHandleY`)
- `ov.didShowGestureHelp`
- `ov.keyBarHintShown`
- desktop-effect wishes (`ov.hideWallpaper` default on,
  `ov.disableAnimations` default off)

**Saved connections** (`ConnectionStore`):

- Stored as JSON in
  `Application Support/OpenView/connections.json` (ISO-8601 dates, atomic
  writes).
- Decoding is tolerant: unknown keys are ignored and missing fields get
  defaults.
- Only device-ID (PIN) connections are saved. A connection is saved on first
  establishment along with its TOFU fingerprint, last fps and last monitor.

**PINs** (`KeychainPIN`):

- Stored as generic passwords: service `com.openview.pin`, account =
  connection UUID, `WhenUnlockedThisDeviceOnly`.
- With "Require Face ID for saved PINs" on, new items get `.userPresence`.
  They are read off main, so the biometric prompt never blocks the UI.
- A wrong PIN deletes the stored one.

**Home** (`HomeView`) shows:

- a one-tap Reconnect card (the most recent connection with a PIN),
- Quick Connect by device ID, then a PIN prompt,
- "Pair with code instead" (`PairView`, Relay or LAN),
- Favorites and Recent with swipe Edit / Delete, plus New connection,
- the first-run relay sheet, or else auto-connect on launch, if enabled.

### Logging and diagnostics

- **Session log** (session menu > Session log; Settings > Last session log):
  user-meaningful events only. `SessionController.log` covers connecting,
  connected, auth failed, reconnecting, direct path active / lost, files sent
  and received, and errors.
- **Unified log** (`Support/Log.swift`): `os.Logger` with the bundle id as
  subsystem and the categories `session`, `transport`, `input`, `render`,
  `diag`. Session-log lines are mirrored at notice level; transport detail
  (socket closes, probing, retries, power state) is logged only here, at
  debug level (`SessionController.debug`).
- **Diagnostics files** (`Support/DiagLog.swift`): one file per launch in
  Application Support/logs, at most 1 MB each, the newest 5 kept. They hold
  session-log events, app lifecycle, network path changes and hangs
  (`HangMonitor` pings the main thread and the session queue every second;
  in debug builds `Breadcrumb` adds the FFI call in progress). Nothing is
  uploaded automatically; Settings > About > **Share diagnostic logs** opens
  the share sheet.

---

## 10. Info.plist and capabilities

The properties come from `project.yml` (`targets.OpenView.info.properties`).
XcodeGen writes them to `OpenView/App/Info.plist`.

| Key | Purpose |
|---|---|
| `NSLocalNetworkUsageDescription` | LAN peers and the relay server |
| `NSBonjourServices` = `[_openview._udp]` | Browsing for OpenView PCs ("On this network") |
| `NSAppTransportSecurity.NSAllowsLocalNetworking` | Plain connections on the local network |
| `NSFaceIDUsageDescription` | App lock and protected saved PINs |
| `UIApplicationSupportsIndirectInputEvents` | iPad mouse/trackpad secondary clicks and scroll |
| `UISupportedInterfaceOrientations` (`~ipad`) | Portrait and landscape; the iPad also allows upside-down |
| `UIViewControllerBasedStatusBarAppearance` | The session hides the status bar |
| `CADisableMinimumFrameDurationOnPhone` | Display link above 60 Hz on ProMotion iPhones |
| `UIRequiredDeviceCapabilities: arm64` | |
| `UIApplicationSceneManifest.UIApplicationSupportsMultipleScenes = false` | Single scene |
| `OVDefaultRelayURL` | Optional relay baked into the build, from the `OV_DEFAULT_RELAY_URL` build setting (empty by default) |

`App/PrivacyInfo.xcprivacy` is the privacy manifest: no tracking, no
collected data, and the one required-reason API in use (`UserDefaults`,
reason `CA92.1`).

`ITSAppUsesNonExemptEncryption` is deliberately not set. The app uses
encryption (TLS for `wss://` relays and its own end-to-end session
encryption), so whoever distributes a build must answer App Store Connect's
export-compliance questions for it; see `ios/README.md`.

There are no microphone, camera or photo-library usage keys. LAN hosts are
found by Bonjour (above) or entered by IP address.

---

## 11. Build

### Prerequisites

A Mac with Xcode 15 or newer, XcodeGen (`brew install xcodegen`) and Rust
(rustup) with the iOS targets:

```bash
rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios
```

### `ios/build_ios.sh`

The script:

1. Builds `ovffi` (release) for `aarch64-apple-ios`, `aarch64-apple-ios-sim`
   and `x86_64-apple-ios`, with `IPHONEOS_DEPLOYMENT_TARGET=15.6`.
2. Merges the two simulator slices with `lipo`.
3. Creates `ios/build/OvFFI.xcframework` with `xcodebuild -create-xcframework`
   (device + simulator libraries, each with `ios/OvFFI/include`: `shim.h` and
   the `OvFFI` module map).
4. Runs `xcodegen generate` in `ios/OpenView`.
5. Builds with a fixed derived-data folder (`ios/build/DerivedData`).

```bash
ios/build_ios.sh --sim          # simulator .app, no signing (Apple silicon and Intel)
ios/build_ios.sh --device       # unsigned device .ipa -> ios/build/OpenView-unsigned.ipa
ios/build_ios.sh --ipa          # signed archive + export (DEVELOPMENT_TEAM in Config/Local.xcconfig)
ios/build_ios.sh --test         # unit tests on a simulator (DESTINATION=... to pick one)
ios/build_ios.sh --xcframework  # only the XCFramework
```

- The unsigned `.ipa` is meant for **Sideloadly / AltStore**, which re-sign
  it with a free Apple ID (7-day validity).
- Everything under `ios/build/` is a build output and is git-ignored.
- `Config/Base.xcconfig` holds `OV_BUNDLE_ID_PREFIX` (default
  `org.openview`) and `OV_DEFAULT_RELAY_URL`; an optional, git-ignored
  `Config/Local.xcconfig` overrides them (and sets `DEVELOPMENT_TEAM`).

### Screenshots (`scripts/ios_screenshots.sh`)

On a Mac, `scripts/ios_screenshots.sh` writes `docs/images/ios-home.png`,
`ios-settings.png` and `ios-home-dark.png`. It builds the XCFramework
(`build_ios.sh --xcframework`; `--skip-rust` reuses an existing one), runs
XcodeGen, builds **Debug** for the simulator into
`ios/build/ScreenshotsDerivedData`, picks an available iPhone simulator
(`--device NAME`, default "iPhone 15"), boots it, overrides the status bar
(9:41, full battery), and launches the app with `-OVScreenshotMode`
(plus `-OVScreenshotSettings` for the Settings shot).

Screenshot mode (`App/ScreenshotMode.swift`) is compiled into Debug builds
only. It shows demo devices from an in-memory `ConnectionStore` and a demo
relay from the in-memory registration / argument domains of
`UserDefaults`, so the real saved devices and settings are never read for
display or overwritten. It skips the first-run relay sheet, the app lock,
auto-connect and the Bonjour browse.

### Tests

- `OpenViewTests` (XCTest) covers pure logic: relay address parsing
  (`RelayAddress`), direct-path candidate parsing and LAN classification
  (`DirectPathSet.parse` / `classify`), the zoom math (`ViewportMath`) and
  the hosts' TXT records (`LanTxtRecord`).
  These source files are compiled into the test bundle directly, so the
  tests need neither the app host nor the Rust library.
- The Rust side is covered by `cargo test -p ovffi`, including the check that
  `shim.h` declares exactly the exported symbols.

---

## 12. Limitations

Open items are also tracked in [KNOWN_ISSUES.md](KNOWN_ISSUES.md).

**Features without UI yet**

- Chat UI is unreachable (§8).
- Implemented but not reachable from any UI:
  - `RemoteInputController.sendSAS` (Ctrl+Alt+Del)
  - `sendCombo`
  - `SessionController.sendClipboardNow`
  - `typeClipboardText`
- `SessionMenuAction.disconnect` and `.showKeyBar` are handled but never
  emitted by the menu.

**Client-only app**

- The app never acts as a host (the pairing sheet therefore no longer
  shows "Your pairing code"). Settings' "This device's short code" and
  fingerprint come from a throwaway per-launch identity
  (`PairingStateBox`, `ov_identity_create`). Each session generates its own
  identity in `ov_session_create`. The code shown is therefore not usable
  by a partner to connect to the phone.

**Media and files**

- The NULL-packet "audio lost" signal from the Opus sink is dropped by the
  Swift trampoline. There is no concealment.
- Received files are held fully in memory (Rust file sink), as are sent
  files.

**Version numbers**

- `project.yml` `CFBundleShortVersionString` and `Info.plist` must be kept
  in sync with the workspace `Cargo.toml` version by hand.

**Missing features**

- No microphone uplink and no host discovery on iOS.
