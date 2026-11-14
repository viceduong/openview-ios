import Combine
import SwiftUI
import UIKit
import os

/// The session engine behind the immersive session (`SessionContainer`).
///
/// Owns the `OvSessionBox` wiring, the screen assembler, the transports and
/// the pump, and publishes only finished, UI-ready state to the main thread.
/// The code is split across files by concern:
///
/// - `SessionController.swift`: published state, the attempt / reconnect
///   state machine and app-lifecycle handling (main thread).
/// - `SessionController+Transport.swift`: dialing, relay / UDP / direct-path
///   wiring, pump loops, error routing, teardown (session queue).
/// - `SessionController+Screen.swift`: screen-assembler sinks and the
///   coalesced main-thread screen flush.
/// - `SessionController+Commands.swift`: outbound commands, chat, clipboard
///   and file transfer.
///
/// ## State machine
///
/// The controller is built from a `ConnectRequest` and dials it itself,
/// driving `phase`:
///
///     dialing(transport) -> authenticating -> waitingForFirstFrame -> live
///     live --(transport lost)--> reconnecting(n) -> live | failed(error)
///     failed(authFailed | rateLimited | incompatibleVersion |
///            hostUnreachable (15 s) | disconnected | transport)
///
/// Up to `maxReconnectAttempts` reconnects rebuild the session from the same
/// request. Every attempt has a generation number; late events of an older
/// attempt are ignored on main.
///
/// ## Threading model
///
/// - **Session queue** (`OvQueues.session`, one background serial queue):
///   every `OvSessionBox` / `OvScreenBox` / transport call. The WebSocket,
///   UDP and direct-path sockets deliver on this queue too, so
///   feed -> pump -> sink ordering is serialized without locks. Members
///   documented as "session queue" are only touched here.
/// - **Rust callbacks**: session sinks fire synchronously inside the pump /
///   send call that triggered them, on the session queue, while Rust holds
///   the session mutex. A sink must never call back into the session; work
///   that needs to (PONG replies, IDR requests) is deferred until the pump
///   returns. The relay wake sink is the one exception: it fires on a Rust
///   watcher thread and only dispatches a pump onto the session queue.
/// - **Main thread**: all `@Published` state, the attempt state machine,
///   input, clipboard polling and UI callbacks. Results hop from the session
///   queue to main with `onMain` / `DispatchQueue.main.async`; decode work
///   (frames, audio, file writes) stays off main.
/// - `screenLock` guards the one structure written from the session queue
///   and drained on main (`pendingScreen`).
///
/// The class is not `@MainActor`: half of it runs on the session queue, so
/// isolation is by convention, as documented per member.
///
/// ## Lifetime
///
/// Owned by `AppModel` for the session's duration. Every closure handed to
/// the session, screen, transports and timers captures `self` weakly, and
/// `stop()` / `deinit` clear them all and close the transports, so dropping
/// the controller frees the `OvSessionBox` (`ov_session_free` closes the
/// Rust relay socket).
final class SessionController: ObservableObject {
    static let maxReconnectAttempts = 3
    /// Watchdog: inbound silence (ms) after which an established session is
    /// declared dead and reconnected (the host keepalive is 5 s).
    static let watchdogMs: Int64 = 8_000
    /// No secure session after this long = host not responding.
    static let handshakeTimeout: TimeInterval = 15

    // ---- published UI state (main thread only) ----
    @Published private(set) var phase: SessionPhase
    /// Session log (status / errors; chat lines live in `chat`).
    @Published private(set) var lines: [LogLine] = []
    @Published private(set) var chat: [ChatMessage] = []
    @Published var unreadChat = 0
    @Published private(set) var peerPine: String = ""
    @Published private(set) var receivedFiles: [ReceivedFile] = []
    /// The host's monitor list from MONITORS_INFO; the picker sends
    /// CAPTURE_REGION for the chosen monitor.
    @Published private(set) var monitors: [MonitorInfo] = []
    @Published private(set) var monitorIndex = 0
    /// Requested capture rate (QUALITY_SET), 0 = Auto.
    @Published private(set) var targetFps: UInt16
    /// Short transient message (file received, clipboard sent, ...).
    @Published var toast: String?
    /// TOFU: set while the host's identity differs from the pinned one.
    @Published var identityMismatch: IdentityMismatch?
    /// The host announced CAP_DESKTOP_FX (wallpaper / animation switches in
    /// the session menu).
    @Published private(set) var hostSupportsDesktopFx = false
    /// The transport the current attempt dials ("LAN 192.168.1.5:4571",
    /// "Relay wss://…"): a LAN connection may fall back to the relay.
    @Published private(set) var transportDetail: String

    let screenModel = ScreenModel()
    let livenessModel = LivenessModel()
    /// Remote input: gestures / keyboard -> channel-0 commands. Main thread
    /// only.
    let input = RemoteInputController()

    /// The request being (re)dialed. Main thread only.
    private(set) var request: ConnectRequest
    /// The chat sheet is visible (no unread counting). Main thread only.
    var chatVisible = false
    /// Established with an accepted identity (pine string). Main thread.
    var onEstablished: ((String) -> Void)?
    /// The PIN / pairing code was rejected. Main thread.
    var onAuthFailed: (() -> Void)?

    // The stored state below is internal (not private) only so the
    // SessionController+*.swift extensions can reach it; treat it as private
    // to the controller.

    /// Log / chat caps; older lines are dropped.
    static let maxLines = 300
    static let maxChat = 500
    var nextLineID = 0 // main thread only

    // ---- main-thread attempt state ----
    var generation = 0
    var reconnectAttempts = 0
    var everLive = false
    var attemptEstablished = false
    /// Monitor to restore when the host announces its monitors.
    var pendingMonitor: Int?
    /// The attempt's legs (LAN first and / or relay, `TransportPlan`) and
    /// the one being dialed; re-planned on every attempt / reconnect.
    var plan = TransportPlan(routes: [.relay])
    var routeIndex = 0
    /// The LAN leg heard from the host (a datagram arrived), so the short
    /// LAN probe timeout no longer applies.
    var lanProgress = false
    var started = false
    var stopped = false
    var observers: [NSObjectProtocol] = []
    var backgroundTask: UIBackgroundTaskIdentifier = .invalid
    /// Echo-guarded pasteboard bridge, polled on main.
    let clipboard = ClipboardBridge()
    var clipboardTimer: DispatchSourceTimer?

    // ---- session-queue state ----
    var session: OvSessionBox?
    let queue = OvQueues.session
    /// The Rust frame assembler (replaced for every attempt).
    var screen = OvScreenBox()
    /// Remote PC audio playback via AVAudioEngine.
    let audioPlayer = RemoteAudioPlayer()
    var pumpTimer: DispatchSourceTimer?
    var udpBridge: NwUdpBridge?
    /// WebSocket relay mode: the socket the dialer opened.
    var relaySocket: RelayWebSocket?
    /// Direct (UDP) mode: inbound arrives via the bridge, no relay pump.
    var isDirect = false
    /// Direct mode: datagrams queued into the session since the last pump,
    /// and whether a pump is already scheduled. With the reliable UDP
    /// framing a frame arrives as ~1.2 KB fragments, so pumping once per
    /// datagram would run the Rust pump ~1000x per MB; instead datagrams are
    /// queued and one pump drains a batch (at most ~1 ms later, or at once
    /// when `directBatchMax` are waiting).
    var directQueued = 0
    var directPumpScheduled = false
    static let directBatchMax = 64
    /// Set once the handshake completes (picks the -3 error wording).
    var established = false
    /// Direct mode: the first datagram from the host was reported to main
    /// (`noteLanProgress`).
    var lanHeard = false
    /// Set on a fatal error / teardown: the pump is stopped and inbound
    /// traffic is ignored until the next attempt.
    var halted = false
    /// Set by `stop()`: no further attempts.
    var tornDown = false
    /// The generation of the attempt running on the queue.
    var queueGeneration = 0
    /// The last error line logged, so a repeating error is shown once.
    var lastErrorLine: String?
    /// Monotonic ms epoch for the keepalive / liveness ticks.
    let pumpEpochNs = DispatchTime.now().uptimeNanoseconds
    /// Last liveness values published, so the HUD only re-renders on change.
    var lastLiveness: (alive: Bool, rtt: Int32, fpsTenths: Int) = (true, -1, 0)
    /// H.264 overlay state: CMSampleBuffer builder, whether a motion period
    /// is running, its generation, and the generation whose END is waiting
    /// for the refresh frame.
    let videoSamples = H264SampleBuilder()
    var videoActive = false
    var videoGen = 0
    var pendingHideGen: Int?
    /// Last host CAPS seen (CAP_INPUT2 picks the input commands).
    var lastPeerCaps: UInt32 = 0
    /// The desktop-effect wishes went out to this host.
    var sentDesktopFx = false
    static let capDesktopFx: UInt32 = 1 << 8
    // Direct UDP path next to a relay session (session queue).
    /// This attempt runs over a relay (TCP or WebSocket).
    var relayMode = false
    /// The direct sinks are installed (relay mode + setting on).
    var p2pEnabled = false
    /// One UDP NWConnection per host candidate.
    var p2pPaths: DirectPathSet?
    /// One-shot timer driving pumpRelay at the engine deadline (<= 20 ms)
    /// while probing / direct, and when it is due.
    var p2pTimer: DispatchSourceTimer?
    var p2pTimerDue: DispatchTime?
    /// Direct datagrams queued since the last pump / a pump is scheduled.
    var p2pQueued = 0
    var p2pPumpScheduled = false
    /// Last direct state / path published (UI + log only on change).
    var lastDirectState: Int32 = -1
    var lastDirectPath: Int64 = -1
    /// Watchdog: when it was armed (first tick after the secure session came
    /// up), so a slow first frame never counts as silence.
    var watchdogArmedMs: Int64?
    static let p2pMaxIntervalMs: Int64 = 20

    /// Screen updates coalesced across threads (guarded by `screenLock`).
    let screenLock = NSLock()
    var pendingScreen = PendingScreen()
    var screenFlushScheduled = false

    init(request: ConnectRequest) {
        self.request = request
        self.phase = .dialing(request.transport.label)
        self.transportDetail = request.transport.detail
        self.targetFps = request.defaultFps
        self.pendingMonitor = request.lastMonitor > 0 ? request.lastMonitor : nil
    }

    deinit {
        // stop() normally ran already; this is the backstop. Capture the
        // parts, not self, and tear them down on the queue that owns them.
        // Everything here is idempotent.
        clipboardTimer?.cancel()
        let nc = NotificationCenter.default
        for o in observers {
            nc.removeObserver(o)
        }
        let session = self.session
        let screen = self.screen
        let audio = self.audioPlayer
        let timer = self.pumpTimer
        let ws = self.relaySocket
        let udp = self.udpBridge
        let p2pTimer = self.p2pTimer
        let p2pPaths = self.p2pPaths
        queue.async {
            timer?.cancel()
            p2pTimer?.cancel()
            p2pPaths?.closeAll()
            ws?.onMessage = nil
            ws?.onClose = nil
            ws?.close()
            udp?.close()
            session?.clearCallbacks()
            screen.clearCallbacks()
            audio.stop()
        }
    }

    // MARK: Lifecycle (main thread)

    func start() {
        guard !started, !stopped else { return }
        started = true
        clipboard.send = { [weak self] text in
            self?.sendClipboardPayload(text)
        }
        startClipboardPolling()
        input.send = { [weak self] payload in
            self?.sendCommand(payload)
        }
        // The overlay's decoder failed (main): ask the host for an IDR from
        // the session queue.
        screenModel.video.onFailure = { [weak self] in
            self?.queue.async { [weak self] in
                guard let self, !self.halted else { return }
                self.screen.requestVideoIdr()
            }
        }
        // A new Metal surface (or lost staging) needs the whole frame again;
        // re-stage it from the Rust framebuffer.
        screenModel.uploads?.requestFull = { [weak self] in
            self?.queue.async { [weak self] in
                self?.restageFullFrame()
            }
        }
        observeAppLifecycle()
        beginAttempt()
    }

    func stop() {
        guard !stopped else { return }
        stopped = true
        generation += 1
        removeLifecycleObservers()
        endBackgroundTask()
        clipboardTimer?.cancel()
        clipboardTimer = nil
        clipboard.send = nil
        // Release every key / button held on the host before teardown (the
        // records queue ahead of the teardown below), then unhook input.
        input.releaseAll()
        input.shutdown()
        // Strong capture on purpose: the teardown must run even if the
        // owner releases the controller right after this.
        queue.async { self.teardownOnQueue() }
    }

    /// Try again from scratch (error view Retry, back from background,
    /// re-entered PIN). `pin` replaces the device PIN; `remember` updates
    /// whether it is stored once established.
    func retry(pin: String? = nil, remember: Bool? = nil) {
        guard started, !stopped else { return }
        if let pin, case .device(let id, _) = request.target {
            request.target = .device(id: id, pin: pin)
            request.pinIsStored = false
        }
        if let remember {
            request.rememberPIN = remember
        }
        reconnectAttempts = 0
        identityMismatch = nil
        phase = .dialing(request.transport.label)
        beginAttempt()
    }

    /// TOFU: the user accepted the host's new identity — re-pin it.
    func acceptNewIdentity() {
        guard let mismatch = identityMismatch else { return }
        identityMismatch = nil
        request.pinnedFingerprint = mismatch.got
        log("Host identity accepted and re-pinned: \(mismatch.got)")
        onEstablished?(mismatch.got)
    }

    /// Start (or restart) one connection attempt: plan its legs from the
    /// current network (a LAN connection with a device ID tries LAN first
    /// on Wi-Fi / Ethernet, the relay directly on cellular) and dial the
    /// first one.
    func beginAttempt() {
        guard !stopped else { return }
        plan = Self.makePlan(for: request, network: NetworkPathWatcher.shared.current)
        routeIndex = 0
        launchAttempt()
    }

    /// The legs for `req` on `network`.
    static func makePlan(for req: ConnectRequest, network: NetworkReach) -> TransportPlan {
        TransportPlan.make(isLan: req.transport.kind == .lan,
                           hasDeviceID: req.deviceID != nil,
                           network: network)
    }

    /// The leg being dialed.
    var route: DialRoute {
        plan.routes.indices.contains(routeIndex) ? plan.routes[routeIndex] : plan.first
    }

    /// The request as dialed on `route`: a LAN connection's relay leg
    /// reaches the same device ID through the default relay. Target, PIN
    /// and pinned fingerprint are unchanged, so TOFU applies on both legs.
    func dialRequest(on route: DialRoute) -> ConnectRequest {
        var req = request
        if route == .relay, req.transport.kind == .lan {
            req.transport = .relay(AppSettings.defaultRelayURL)
        }
        return req
    }

    /// Dial the current leg with a new generation.
    func launchAttempt() {
        guard !stopped else { return }
        generation += 1
        let gen = generation
        attemptEstablished = false
        lanProgress = false
        let req = dialRequest(on: route)
        transportDetail = req.transport.detail
        if case .reconnecting = phase {
            // keep the capsule
        } else {
            phase = .dialing(req.transport.label)
        }
        // Nothing stays pressed; the next host's CAPS decide the commands.
        input.releaseAll()
        input.input2 = false
        livenessModel.peerAlive = true
        livenessModel.rttMs = -1
        livenessModel.fps = 0
        livenessModel.path = nil
        hostSupportsDesktopFx = false
        screenModel.video.hide(generation: screenModel.video.generation)
        log("Connecting to \(req.displayName) via \(req.transport.detail)")
        DispatchQueue.main.asyncAfter(deadline: .now() + Self.handshakeTimeout) { [weak self] in
            self?.handshakeTimedOut(generation: gen)
        }
        // A LAN leg with a relay behind it gets a short probe: a host that
        // has not answered at all by then is not on this network.
        if route == .lan, plan.hasFallback(after: routeIndex) {
            DispatchQueue.main.asyncAfter(deadline: .now() + TransportPlan.lanProbeTimeout) { [weak self] in
                self?.lanProbeTimedOut(generation: gen)
            }
        }
        queue.async { [weak self] in
            self?.startOnQueue(request: req, generation: gen)
        }
    }

    func handshakeTimedOut(generation gen: Int) {
        guard gen == generation, !stopped, !attemptEstablished else { return }
        log("No secure session after \(Int(Self.handshakeTimeout)) s")
        queue.async { [weak self] in
            self?.haltOnQueue()
        }
        handleFailure(.hostUnreachable, generation: gen)
    }

    /// The LAN leg heard nothing from the host within the probe timeout:
    /// end it and let `handleFailure` move on to the relay.
    func lanProbeTimedOut(generation gen: Int) {
        guard gen == generation, !stopped, !attemptEstablished, !lanProgress else { return }
        debug("LAN: no answer from the host after \(Int(TransportPlan.lanProbeTimeout)) s")
        queue.async { [weak self] in
            self?.haltOnQueue()
        }
        handleFailure(.hostUnreachable, generation: gen)
    }

    /// Main: the LAN leg received its first datagram from the host.
    func noteLanProgress(generation gen: Int) {
        guard gen == generation, !stopped else { return }
        lanProgress = true
    }

    /// An attempt ended with `error`: fall back from LAN to the relay (a
    /// LAN leg that never reached a secure session, unless the host itself
    /// refused: wrong PIN, rate limited, incompatible version), else
    /// auto-reconnect (an established session that dropped, up to 3
    /// attempts) or show the error.
    func handleFailure(_ error: SessionError, generation gen: Int) {
        guard gen == generation, !stopped else { return }
        // Invalidate late events of the failed attempt.
        generation += 1
        var error = error
        if route == .lan, !attemptEstablished,
           let next = plan.fallback(after: routeIndex, failure: error.legFailure) {
            routeIndex += 1
            if next == .relay, AppSettings.defaultRelayURL.isEmpty {
                // Nothing to fall back to: say how to reach this PC away
                // from its network.
                log("LAN unreachable — no relay server is configured")
                error = .transport(ConnectionTransport.ConfigError.noRelay.message)
            } else {
                log("LAN unreachable — connecting via relay")
                launchAttempt()
                return
            }
        }
        let inBackground = UIApplication.shared.applicationState == .background
        if error.canAutoReconnect, everLive, !inBackground, AppSettings.autoReconnect,
           reconnectAttempts < Self.maxReconnectAttempts {
            reconnectAttempts += 1
            let attempt = reconnectAttempts
            phase = .reconnecting(attempt)
            log("Reconnecting (attempt \(attempt) of \(Self.maxReconnectAttempts))…")
            let g = generation
            let delay = Double(attempt)
            DispatchQueue.main.asyncAfter(deadline: .now() + delay) { [weak self] in
                guard let self, !self.stopped, self.generation == g else { return }
                self.beginAttempt()
            }
            return
        }
        phase = .failed(error)
        if !inBackground {
            Haptics.error()
        }
        if case .authFailed = error {
            onAuthFailed?()
        }
    }

    /// Main: the secure session is up (once per attempt).
    func didEstablish(pine: String, generation gen: Int) {
        guard gen == generation, !stopped else { return }
        attemptEstablished = true
        peerPine = pine
        log("Secure session established — host \(pine)")
        switch phase {
        case .live, .reconnecting:
            break // a reconnect keeps its capsule until the first frame
        default:
            phase = .waitingForFirstFrame
        }
        // Re-apply this session's quality (a reconnected host starts at
        // its default) — capped while the device is hot / in Low Power Mode.
        let fps = effectiveFps
        if fps != 0 {
            sendQuality(targetFps: fps)
        }
        if let expected = request.pinnedFingerprint, !expected.isEmpty, expected != pine {
            log("Host identity changed: expected \(expected), got \(pine)")
            identityMismatch = IdentityMismatch(expected: expected, got: pine)
        } else {
            onEstablished?(pine)
        }
    }

    /// Main: a frame arrived — the session is live.
    func markLiveIfNeeded() {
        guard attemptEstablished else { return }
        switch phase {
        case .dialing, .authenticating, .waitingForFirstFrame, .reconnecting:
            phase = .live
            reconnectAttempts = 0
            everLive = true
            Haptics.success()
        case .live, .failed:
            break
        }
    }

    /// Main: the transport is up and the handshake is running.
    func didDial(generation gen: Int) {
        guard gen == generation, !stopped, !attemptEstablished else { return }
        if case .dialing = phase {
            phase = .authenticating
        }
    }

    // MARK: App lifecycle (main thread)

    func observeAppLifecycle() {
        let nc = NotificationCenter.default
        observers.append(nc.addObserver(forName: UIApplication.didEnterBackgroundNotification,
                                        object: nil, queue: .main) { [weak self] _ in
            self?.didEnterBackground()
        })
        observers.append(nc.addObserver(forName: UIApplication.willEnterForegroundNotification,
                                        object: nil, queue: .main) { [weak self] _ in
            self?.willEnterForeground()
        })
        // Thermal state / Low Power Mode changes re-apply the requested host
        // frame rate.
        observers.append(nc.addObserver(forName: PowerGovernor.didChange,
                                        object: nil, queue: .main) { [weak self] _ in
            self?.powerStateChanged()
        })
        if PowerGovernor.shared.constrained {
            debug("Power: \(PowerGovernor.shared.reason), limiting to 60 Hz / \(PowerGovernor.constrainedHostFps) fps")
        }
    }

    /// The frame rate actually requested from the host: the user's choice
    /// (0 = Auto), capped at `PowerGovernor.constrainedHostFps` while the
    /// device is hot or in Low Power Mode (Auto then means the cap).
    var effectiveFps: UInt16 {
        guard PowerGovernor.shared.constrained else { return targetFps }
        let cap = PowerGovernor.constrainedHostFps
        return targetFps == 0 ? cap : min(targetFps, cap)
    }

    /// Main: the device got hot / cooled down / Low Power Mode toggled.
    func powerStateChanged() {
        if PowerGovernor.shared.constrained {
            debug("Power: \(PowerGovernor.shared.reason), limiting to 60 Hz / \(PowerGovernor.constrainedHostFps) fps")
        } else {
            debug("Power: back to normal, full frame rate restored")
        }
        guard attemptEstablished, !stopped else { return }
        // 0 restores the host's Auto governor.
        sendQuality(targetFps: effectiveFps)
    }

    func removeLifecycleObservers() {
        let nc = NotificationCenter.default
        for o in observers {
            nc.removeObserver(o)
        }
        observers.removeAll()
    }

    /// Nothing stays pressed on the host while we are away, and ask iOS
    /// for a little time so a short app switch keeps the session.
    func didEnterBackground() {
        input.releaseAll()
        guard backgroundTask == .invalid else { return }
        backgroundTask = UIApplication.shared.beginBackgroundTask(withName: "OpenView session") { [weak self] in
            self?.endBackgroundTask()
        }
    }

    /// Back in the foreground: a session that dropped meanwhile reconnects.
    func willEnterForeground() {
        endBackgroundTask()
        guard started, !stopped else { return }
        if case .failed(let error) = phase, error.canAutoReconnect, everLive {
            log("Back in the foreground — reconnecting")
            retry()
        }
    }

    func endBackgroundTask() {
        guard backgroundTask != .invalid else { return }
        UIApplication.shared.endBackgroundTask(backgroundTask)
        backgroundTask = .invalid
    }

    // MARK: Published-state setters (main thread)

    // The extensions update `private(set)` published state through these,
    // so the setters stay in this file.

    /// The host's monitor list arrived (also after a reconnect): restore the
    /// saved / chosen monitor.
    func applyMonitors(_ mons: [MonitorInfo]) {
        monitors = mons
        let want = pendingMonitor ?? monitorIndex
        pendingMonitor = nil
        if want > 0 && want < mons.count {
            monitorIndex = want
            sendCaptureRegion(monitorAt: want)
        } else {
            monitorIndex = 0
        }
    }

    /// Switch the captured monitor (session menu).
    func selectMonitor(_ index: Int) {
        guard monitors.indices.contains(index), index != monitorIndex else { return }
        monitorIndex = index
        sendCaptureRegion(monitorAt: index)
    }

    /// Change the requested frame rate (0 = Auto).
    func setQuality(_ fps: UInt16) {
        guard fps != targetFps else { return }
        targetFps = fps
        sendQuality(targetFps: effectiveFps)
    }

    /// The host's CAPS changed: pick the input command set and show / hide
    /// the desktop-effect switches.
    func applyPeerCaps(input2: Bool, desktopFx: Bool) {
        input.input2 = input2
        if hostSupportsDesktopFx != desktopFx {
            hostSupportsDesktopFx = desktopFx
        }
    }

    /// A received file was written to the app container.
    func noteReceivedFile(_ file: ReceivedFile) {
        receivedFiles.append(file)
        toast = "Received \(file.name)"
    }

    // MARK: Helpers

    /// Run `work` on main with the controller, if it is still alive.
    func onMain(_ work: @escaping (SessionController) -> Void) {
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            work(self)
        }
    }

    /// A user-meaningful session event (connecting, connected, auth failed,
    /// reconnecting, direct path active / lost, file sent / received,
    /// errors): shown in the in-app session log, written to the unified log
    /// and to the persistent diagnostics log. Callable from any thread.
    func log(_ text: String) {
        Log.session.notice("\(text, privacy: .public)")
        // Persistent copy: written from its own queue, so it survives a
        // blocked main thread / session queue.
        DiagLog.shared.log(text)
        onMain { $0.appendLine(text) }
    }

    /// Developer detail (transport events, retries, power state): unified
    /// log only, at debug level (not persisted, not shown in the app).
    func debug(_ text: String) {
        Log.transport.debug("\(text, privacy: .public)")
    }

    /// Main thread: append and cap the log at `maxLines`.
    func appendLine(_ text: String) {
        nextLineID += 1
        lines.append(LogLine(id: nextLineID, text: text, date: Date()))
        if lines.count > Self.maxLines {
            lines.removeFirst(lines.count - Self.maxLines)
        }
    }

    /// Main thread: a chat line from the peer.
    func receiveChat(_ text: String) {
        appendChat(text, fromMe: false)
        if !chatVisible {
            unreadChat += 1
        }
    }

    /// Main thread: append and cap the chat.
    func appendChat(_ text: String, fromMe: Bool) {
        nextLineID += 1
        chat.append(ChatMessage(id: nextLineID, text: text, fromMe: fromMe, date: Date()))
        if chat.count > Self.maxChat {
            chat.removeFirst(chat.count - Self.maxChat)
        }
    }

    /// "A1B2C3D4 E5F60718 293A4B5C D6E7F809"-style display. Static (not on
    /// a View: View members are main-actor-isolated; this runs on the
    /// session queue).
    static func pineString(_ fp: [UInt8]) -> String {
        let hex = fp.map { String(format: "%02X", $0) }.joined()
        return stride(from: 0, to: hex.count, by: 8).map {
            String(hex.dropFirst($0).prefix(8))
        }.joined(separator: " ")
    }
}
