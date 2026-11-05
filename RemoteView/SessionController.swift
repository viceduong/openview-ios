import SwiftUI
import Combine
import AVFoundation
import UIKit

// Session engine behind the immersive session (SessionContainer). Owns the
// OvSessionBox wiring, the screen assembler, the transports and the pump;
// publishes only finished, UI-ready state to the main thread.
//
// Lifecycle (section 3): the controller is built from a `ConnectRequest`
// and dials it itself, on the session queue, driving `phase`:
//   dialing(transport) -> authenticating -> waitingForFirstFrame -> live
//   live --(transport lost)--> reconnecting(n) (up to 3 attempts, the
//   session rebuilt from the same request) -> live | failed(error)
//   failed(authFailed / hostUnreachable (15 s) / disconnected / transport)
// Every attempt has a generation number; late events of an older attempt
// are ignored on main.
//
// Threading: every session / screen / transport call runs on
// `OvQueues.session` (a background serial queue). Sinks fire there, so all
// decode work (frame -> CGImage, audio conversion, file writes) happens off
// main; only the results hop to main. Main-thread-only members are marked.
//
// Lifetime: owned by AppModel for the session's duration. Every closure
// handed to the session, screen, transports and timers captures `self`
// weakly, and `stop()` / `deinit` clear them all and close the transports,
// so dropping the controller frees the OvSessionBox (ov_session_free ->
// the Rust relay socket closes).

// MARK: - Log lines

/// One session-log line (status / errors); a stable id so lists diff cheaply.
struct LogLine: Identifiable {
    let id: Int
    let text: String
    let date: Date
}

/// One chat message (channel 1).
struct ChatMessage: Identifiable {
    let id: Int
    let text: String
    let fromMe: Bool
    let date: Date
}

// A file received from the peer (v0.27): already written to the app
// container; `url` opens it (Quick Look / share sheet).
struct ReceivedFile: Identifiable {
    let id = UUID()
    let name: String
    let url: URL
    let size: Int
    let date: Date
}

/// One host monitor from MONITORS_INFO (v0.18).
typealias MonitorInfo = (x: Int32, y: Int32, w: UInt32, h: UInt32, primary: Bool)

/// TOFU: the host presented a different identity than the pinned one.
struct IdentityMismatch: Identifiable {
    let id = UUID()
    let expected: String
    let got: String
}

// MARK: - Session phase

enum SessionError: Equatable {
    /// PIN / pairing code rejected (handshake crypto failure).
    case authFailed
    /// No secure session within the handshake timeout.
    case hostUnreachable
    /// An established session ended.
    case disconnected(String)
    /// The transport could not be set up / failed.
    case transport(String)

    var canAutoReconnect: Bool {
        if case .authFailed = self { return false }
        return true
    }

    /// One line for the session log.
    var logText: String {
        switch self {
        case .authFailed: return "Authentication failed"
        case .hostUnreachable: return "Host not responding"
        case .disconnected(let reason): return "Disconnected — \(reason)"
        case .transport(let reason): return "Connection failed — \(reason)"
        }
    }
}

enum SessionPhase: Equatable {
    case dialing(String)
    case authenticating
    case waitingForFirstFrame
    case live
    case reconnecting(Int)
    case failed(SessionError)

    /// The full-screen connecting overlay is shown.
    var isConnecting: Bool {
        switch self {
        case .dialing, .authenticating, .waitingForFirstFrame: return true
        default: return false
        }
    }

    var stepText: String {
        switch self {
        case .dialing(let transport): return "Contacting the host via \(transport)…"
        case .authenticating: return "Verifying identity…"
        case .waitingForFirstFrame: return "Waiting for the remote screen…"
        case .live: return "Connected"
        case .reconnecting(let n): return "Reconnecting… (\(n)/\(SessionController.maxReconnectAttempts))"
        case .failed(let error): return error.logText
        }
    }
}

// MARK: - Screen + liveness models

/// Remote screen state, observed only by the screen view so frame / cursor
/// updates never re-evaluate the whole session body. Main thread only.
final class ScreenModel: ObservableObject {
    // Reassembled remote screen (nil until the first frame completes; kept
    // across reconnects so overlays dim the last frame).
    @Published var image: CGImage?
    // The last frame's geometry — the space pointer coordinates normalize to.
    @Published var frameSize: CGSize = .zero
    // Remote cursor overlay (v0.15): position in frame pixel space, and the
    // shape bitmap with its hotspot. nil shape = position-only (hidden).
    @Published var cursorPos: CGPoint?
    @Published var cursorImage: CGImage?
    @Published var cursorHotspot: CGSize = .zero
    @Published var cursorSize: CGSize = .zero
    // Section 4: the latest frame's BGRA bytes + the rects changed since
    // the Metal renderer last uploaded (not published: the surface pulls
    // them whenever `image` changes).
    let pixels = FramePixelStore()
    // Phase 5: the H.264 overlay for high-motion regions (not published —
    // the frame view hosts its layer; samples are enqueued directly).
    let video = VideoOverlay()
}

/// Liveness state (v0.16), observed only by the stats chip and the
/// reconnecting capsule. Main thread only.
final class LivenessModel: ObservableObject {
    @Published var peerAlive = true
    @Published var rttMs: Int32 = -1
    @Published var fps: Float = 0
    /// Section 5: "Relay" / "Probing…" / "Direct (LAN)" / "Direct
    /// (Internet)" for relay sessions; nil for direct UDP sessions.
    @Published var path: String?
}

/// Screen updates accumulated on the session queue and applied to
/// `ScreenModel` in one main-thread hop (latest wins).
private struct PendingScreen {
    var frame: (image: CGImage, size: CGSize, pixels: FramePixels)?
    var cursorPos: CGPoint?
    var cursorShape: (image: CGImage?, hotspot: CGSize, size: CGSize)?
    /// Phase 5: this frame is the lossless refresh after VIDEO_END of that
    /// video generation — hide the overlay together with showing it.
    var hideVideoGen: Int?

    var isEmpty: Bool { frame == nil && cursorPos == nil && cursorShape == nil && hideVideoGen == nil }
}

// MARK: - Controller

final class SessionController: ObservableObject {
    static let maxReconnectAttempts = 3
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
    // Monitors (v0.18): the host's monitor list from MONITORS_INFO; the
    // picker sends CAPTURE_REGION for the chosen monitor.
    @Published private(set) var monitors: [MonitorInfo] = []
    @Published private(set) var monitorIndex = 0
    /// Requested capture rate (QUALITY_SET), 0 = Auto.
    @Published private(set) var targetFps: UInt16
    /// Short transient message (file received, clipboard sent, ...).
    @Published var toast: String?
    /// TOFU: set while the host's identity differs from the pinned one.
    @Published var identityMismatch: IdentityMismatch?
    /// Section 4: the host announced CAP_DESKTOP_FX (wallpaper / animation
    /// switches in the session menu).
    @Published private(set) var hostSupportsDesktopFx = false

    let screenModel = ScreenModel()
    let livenessModel = LivenessModel()
    /// Remote input (section 2): gestures / keyboard -> channel-0 commands.
    /// Main thread only.
    let input = RemoteInputController()

    /// The request being (re)dialed. Main thread only.
    private(set) var request: ConnectRequest
    /// The chat sheet is visible (no unread counting). Main thread only.
    var chatVisible = false
    /// Established with an accepted identity (pine string). Main thread.
    var onEstablished: ((String) -> Void)?
    /// The PIN / pairing code was rejected. Main thread.
    var onAuthFailed: (() -> Void)?

    /// Log / chat caps — older lines are dropped.
    private static let maxLines = 300
    private static let maxChat = 500
    private var nextLineID = 0 // main thread only

    // ---- main-thread attempt state ----
    private var generation = 0
    private var reconnectAttempts = 0
    private var everLive = false
    private var attemptEstablished = false
    /// Monitor to restore when the host announces its monitors.
    private var pendingMonitor: Int?
    private var started = false
    private var stopped = false
    private var observers: [NSObjectProtocol] = []
    private var backgroundTask: UIBackgroundTaskIdentifier = .invalid
    // Clipboard sync (v0.26): echo-guarded pasteboard bridge, polled on main.
    private let clipboard = ClipboardBridge()
    private var clipboardTimer: DispatchSourceTimer?

    // ---- session-queue state ----
    private var session: OvSessionBox?
    private let queue = OvQueues.session
    // The Rust frame assembler (replaced for every attempt).
    private var screen = OvScreenBox()
    // Audio (v0.21): remote mic playback via AVAudioEngine.
    private let audioPlayer = RemoteAudioPlayer()
    private var pumpTimer: DispatchSourceTimer?
    private var udpBridge: NwUdpBridge?
    /// WebSocket relay mode: the socket the dialer opened.
    private var relaySocket: RelayWebSocket?
    /// Direct (UDP) mode: inbound arrives via the bridge, no relay pump.
    private var isDirect = false
    /// Direct mode (Phase 4): datagrams queued into the session since the
    /// last pump, and whether a pump is already scheduled. With the reliable
    /// UDP framing a frame arrives as ~1.2 KB fragments, so pumping once per
    /// datagram would run the Rust pump ~1000x per MB; instead datagrams are
    /// queued and one pump drains a batch (at most ~1 ms later, or at once
    /// when `directBatchMax` are waiting).
    private var directQueued = 0
    private var directPumpScheduled = false
    private static let directBatchMax = 64
    /// Set once the handshake completes (picks the -3 error wording).
    private var established = false
    /// Set on a fatal error / teardown: the pump is stopped and inbound
    /// traffic is ignored until the next attempt.
    private var halted = false
    /// Set by `stop()`: no further attempts.
    private var tornDown = false
    /// The generation of the attempt running on the queue.
    private var queueGeneration = 0
    /// The last error line logged, so a repeating error is shown once.
    private var lastErrorLine: String?
    // Liveness (v0.16): shell monotonic ms epoch for the keepalive ticks.
    private let pumpEpochNs = DispatchTime.now().uptimeNanoseconds
    // Last liveness values published, so the HUD only re-renders on change.
    private var lastLiveness: (alive: Bool, rtt: Int32, fpsTenths: Int) = (true, -1, 0)
    // Phase 5 H.264 overlay state (session queue): CMSampleBuffer builder,
    // whether a motion period is running, its generation, and the
    // generation whose END is waiting for the refresh frame.
    private let videoSamples = H264SampleBuilder()
    private var videoActive = false
    private var videoGen = 0
    private var pendingHideGen: Int?
    /// Last host CAPS seen (section 2: CAP_INPUT2 picks the input commands).
    private var lastPeerCaps: UInt32 = 0
    /// Section 4: the desktop-effect wishes went out to this host.
    private var sentDesktopFx = false
    static let capDesktopFx: UInt32 = 1 << 8
    // Section 5: direct UDP path next to a relay session (session queue).
    /// This attempt runs over a relay (TCP or WebSocket).
    private var relayMode = false
    /// The direct sinks are installed (relay mode + setting on).
    private var p2pEnabled = false
    /// One UDP NWConnection per host candidate.
    private var p2pPaths: DirectPathSet?
    /// One-shot timer driving pumpRelay at the engine deadline (<= 20 ms)
    /// while probing / direct, and when it is due.
    private var p2pTimer: DispatchSourceTimer?
    private var p2pTimerDue: DispatchTime?
    /// Direct datagrams queued since the last pump / a pump is scheduled.
    private var p2pQueued = 0
    private var p2pPumpScheduled = false
    /// Last direct state / path published (UI + log only on change).
    private var lastDirectState: Int32 = -1
    private var lastDirectPath: Int64 = -1
    private static let p2pMaxIntervalMs: Int64 = 20

    // Screen updates coalesced across threads (guarded by `screenLock`).
    private let screenLock = NSLock()
    private var pendingScreen = PendingScreen()
    private var screenFlushScheduled = false

    init(request: ConnectRequest) {
        self.request = request
        self.phase = .dialing(request.transport.label)
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
        // Phase 5: the overlay's decoder failed (main) -> ask the host for an
        // IDR from the session queue.
        screenModel.video.onFailure = { [weak self] in
            self?.queue.async { [weak self] in
                guard let self, !self.halted else { return }
                self.screen.requestVideoIdr()
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

    /// Start (or restart) one connection attempt with a new generation.
    private func beginAttempt() {
        guard !stopped else { return }
        generation += 1
        let gen = generation
        attemptEstablished = false
        if case .reconnecting = phase {
            // keep the capsule
        } else {
            phase = .dialing(request.transport.label)
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
        let req = request
        log("Connecting to \(req.displayName) via \(req.transport.detail)")
        DispatchQueue.main.asyncAfter(deadline: .now() + Self.handshakeTimeout) { [weak self] in
            self?.handshakeTimedOut(generation: gen)
        }
        queue.async { [weak self] in
            self?.startOnQueue(request: req, generation: gen)
        }
    }

    private func handshakeTimedOut(generation gen: Int) {
        guard gen == generation, !stopped, !attemptEstablished else { return }
        log("No secure session after \(Int(Self.handshakeTimeout)) s")
        queue.async { [weak self] in
            self?.haltOnQueue()
        }
        handleFailure(.hostUnreachable, generation: gen)
    }

    /// An attempt ended with `error`: auto-reconnect (an established
    /// session that dropped, up to 3 attempts) or show the error.
    private func handleFailure(_ error: SessionError, generation gen: Int) {
        guard gen == generation, !stopped else { return }
        // Invalidate late events of the failed attempt.
        generation += 1
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
    private func didEstablish(pine: String, generation gen: Int) {
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
        // its default).
        if targetFps != 0 {
            sendQuality(targetFps: targetFps)
        }
        if let expected = request.pinnedFingerprint, !expected.isEmpty, expected != pine {
            log("Host identity changed: expected \(expected), got \(pine)")
            identityMismatch = IdentityMismatch(expected: expected, got: pine)
        } else {
            onEstablished?(pine)
        }
    }

    /// Main: a frame arrived — the session is live.
    private func markLiveIfNeeded() {
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
    private func didDial(generation gen: Int) {
        guard gen == generation, !stopped, !attemptEstablished else { return }
        if case .dialing = phase {
            phase = .authenticating
        }
    }

    // MARK: App lifecycle (main thread)

    private func observeAppLifecycle() {
        let nc = NotificationCenter.default
        observers.append(nc.addObserver(forName: UIApplication.didEnterBackgroundNotification,
                                        object: nil, queue: .main) { [weak self] _ in
            self?.didEnterBackground()
        })
        observers.append(nc.addObserver(forName: UIApplication.willEnterForegroundNotification,
                                        object: nil, queue: .main) { [weak self] _ in
            self?.willEnterForeground()
        })
    }

    private func removeLifecycleObservers() {
        let nc = NotificationCenter.default
        for o in observers {
            nc.removeObserver(o)
        }
        observers.removeAll()
    }

    /// Nothing stays pressed on the host while we are away, and ask iOS
    /// for a little time so a short app switch keeps the session.
    private func didEnterBackground() {
        input.releaseAll()
        guard backgroundTask == .invalid else { return }
        backgroundTask = UIApplication.shared.beginBackgroundTask(withName: "OpenView session") { [weak self] in
            self?.endBackgroundTask()
        }
    }

    /// Back in the foreground: a session that dropped meanwhile reconnects.
    private func willEnterForeground() {
        endBackgroundTask()
        guard started, !stopped else { return }
        if case .failed(let error) = phase, error.canAutoReconnect, everLive {
            log("Back in the foreground — reconnecting")
            retry()
        }
    }

    private func endBackgroundTask() {
        guard backgroundTask != .invalid else { return }
        UIApplication.shared.endBackgroundTask(backgroundTask)
        backgroundTask = .invalid
    }

    // MARK: Session-queue setup

    /// Reset the per-attempt state, dial, wire the sinks and start pumping.
    private func startOnQueue(request req: ConnectRequest, generation gen: Int) {
        guard !tornDown else { return }
        resetAttemptOnQueue()
        queueGeneration = gen
        installScreenSinks()

        let dialed: SessionDialer.Result
        do {
            dialed = try SessionDialer.dial(req)
        } catch {
            failOnQueue(.transport(SessionDialer.describe(error)))
            return
        }
        guard !tornDown, !halted else {
            // Stopped / timed out while the (blocking) dial ran.
            dialed.webSocket?.close()
            return
        }
        let session = dialed.session
        self.session = session
        let mode = dialed.mode
        let ws = dialed.webSocket

        session.onEstablished = { [weak self] fp in
            self?.noteEstablished(fp)
        }
        session.onRecord = { [weak self] channel, payload in
            guard let self else { return }
            switch channel {
            case 0:
                // Terminal command channel: screen frames reassemble here.
                // (Fires under the Rust session lock — never send from
                // here; PONG replies go out after the pump returns.)
                self.screen.feed(payload)
            case 1:
                if let text = String(data: payload, encoding: .utf8) {
                    self.onMain { $0.receiveChat(text) }
                }
            default:
                break
            }
        }
        session.onDatagramOut = { [weak self] data in
            // Direct mode: forward to the UDP bridge (fires on this queue
            // from pump()/sendRecord()). Relay mode: the Rust-owned relay
            // carrier sends from inside pumpRelay(), so nothing to do here.
            self?.udpBridge?.send(data)
        }

        // Direct/LAN mode: dial the peer over UDP. The bridge MUST exist
        // before the first pump() — the client's first HELLO fires
        // `onDatagramOut` during that pump, and a nil bridge would drop the
        // packet so the handshake never starts. Inbound datagrams arrive on
        // this queue via the bridge and are processed right away.
        if case .direct(let host, let port) = mode {
            isDirect = true
            udpBridge = NwUdpBridge(host: host, port: port, queue: queue,
                                    onReceive: { [weak self] data in
                guard let self, !self.halted, let session = self.session else { return }
                do {
                    try session.onDatagram(data)
                } catch {
                    self.handle(error)
                    return
                }
                self.directQueued += 1
                if self.directQueued >= Self.directBatchMax {
                    self.directPump() // a big burst: drain now
                } else {
                    self.scheduleDirectPump()
                }
            }, onState: { [weak self] state in
                if case .failed(let error) = state {
                    self?.log("udp: \(error)")
                }
            })
        }

        // WebSocket relay mode: each received message feeds the Rust relay
        // carrier and is processed right away; outbound bytes already flow
        // through the sink installed by relayConnectStream. Handlers are set
        // here, on the socket's delivery queue; buffered messages flush now.
        if let ws {
            relaySocket = ws
            ws.onMessage = { [weak self] data in
                guard let self, !self.halted, let session = self.session else { return }
                do {
                    try session.relayFeed(data)
                    try session.pumpRelay()
                    try self.sendPongIfOwed()
                } catch {
                    self.handle(error)
                }
                self.afterRelayPump()
            }
            ws.onClose = { [weak self] closeError in
                guard let self, !self.halted, let session = self.session else { return }
                session.relayEOF()
                // Drain whatever arrived before the close; the relay then
                // reports closed (-5), which ends the session below —
                // unless the direct path carries the session (Section 5:
                // Rust keeps it alive while the path holds, and reports -5
                // from a later pump once it is gone too).
                do {
                    try session.pumpRelay()
                } catch {
                    self.handle(error)
                }
                if !self.halted, self.p2pEnabled, session.directState == 2 {
                    self.relaySocket = nil
                    self.log("Relay connection closed — continuing on the direct path")
                    self.afterRelayPump()
                    return
                }
                if !self.halted {
                    let reason = closeError.map { "relay: \($0.localizedDescription)" } ?? "the relay connection closed"
                    self.failOnQueue(.disconnected(reason))
                }
            }
        }

        // Section 5: relay sessions may move onto a direct UDP path (the
        // host offers its candidates once our CAPS say CAP_DIRECT).
        switch mode {
        case .relay, .relayWebSocket:
            relayMode = true
            if AppSettings.directConnection {
                enableDirectPath(session)
            }
            publishDirectState()
        case .direct:
            break
        }

        do {
            try session.pump()
            if let fp = session.peerFingerprint() {
                noteEstablished(fp)
            }
        } catch {
            handle(error)
        }
        guard !halted else { return }
        onMain { $0.didDial(generation: gen) }

        // Poll the Rust core every 50 ms on this queue (a DispatchSource is
        // independent of run-loop modes, so scrolling cannot stall it).
        // Relay mode also drives the Rust-owned relay transport; direct
        // mode's inbound arrives via the bridge instead.
        pumpTimer?.cancel()
        let timer = DispatchSource.makeTimerSource(queue: queue)
        timer.schedule(deadline: .now() + .milliseconds(50),
                       repeating: .milliseconds(50),
                       leeway: .milliseconds(5))
        timer.setEventHandler { [weak self] in
            self?.tick()
        }
        pumpTimer = timer
        timer.resume()
    }

    /// Drop the previous attempt (transports, session, assembler) and
    /// reset the per-attempt flags. Session queue.
    private func resetAttemptOnQueue() {
        haltOnQueue()
        session?.clearCallbacks()
        session = nil
        screen.clearCallbacks()
        screen = OvScreenBox()
        halted = false
        established = false
        isDirect = false
        directQueued = 0
        lastErrorLine = nil
        lastLiveness = (alive: true, rtt: -1, fpsTenths: 0)
        videoActive = false
        pendingHideGen = nil
        lastPeerCaps = 0
        sentDesktopFx = false
        relayMode = false
        p2pEnabled = false
        p2pQueued = 0
        p2pPumpScheduled = false
        p2pTimerDue = nil
        lastDirectState = -1
        lastDirectPath = -1
    }

    /// The secure session is up: publish the host fingerprint once.
    private func noteEstablished(_ fp: [UInt8]) {
        guard !established else { return }
        established = true
        let pine = Self.pineString(fp)
        let gen = queueGeneration
        onMain { $0.didEstablish(pine: pine, generation: gen) }
    }

    private func installScreenSinks() {
        // Frames: build the CGImage here, off main, then publish only the
        // finished image (coalesced — if main is behind, the newest wins).
        screen.onFrame = { [weak self] _, width, height, stride, format, pixels, dirty in
            guard let self,
                  let made = FrameImage.make(width: width, height: height, stride: stride,
                                             format: format, pixels: pixels) else { return }
            let img = made.image
            let size = CGSize(width: CGFloat(img.width), height: CGFloat(img.height))
            // Phase 5: the first frame after VIDEO_END carries the lossless
            // refresh of the region; the overlay goes away with it.
            let hide = self.pendingHideGen
            self.pendingHideGen = nil
            var fp = FramePixels(bytes: made.bytes, rowBytes: made.rowBytes,
                                 width: img.width, height: img.height, dirty: dirty)
            self.postScreen {
                // Section 4: a frame main has not shown yet is replaced;
                // its dirty rects carry over so the texture upload covers
                // both.
                if let older = $0.frame?.pixels {
                    fp.dirty = FramePixels.merged(older, fp)
                }
                $0.frame = (image: img, size: size, pixels: fp)
                if let hide { $0.hideVideoGen = hide }
            }
        }
        // Phase 5: H.264 for high-motion regions, only when VideoToolbox can
        // decode it here (`enableVideo` advertises CAP_H264).
        if H264SampleBuilder.available {
            screen.onVideo = { [weak self] packet in
                self?.handleVideo(packet)
            }
            screen.enableVideo()
        }
        // Remote cursor callbacks (both fire on this queue).
        screen.onCursor = { [weak self] x, y in
            let pos = CGPoint(x: CGFloat(x), y: CGFloat(y))
            self?.postScreen { $0.cursorPos = pos }
        }
        screen.onCursorShape = { [weak self] hx, hy, w, h, bgra in
            let img = FrameImage.makeCursor(w: w, h: h, bgra: bgra)
            let hotspot = CGSize(width: CGFloat(hx), height: CGFloat(hy))
            let size = CGSize(width: CGFloat(w), height: CGFloat(h))
            self?.postScreen { $0.cursorShape = (image: img, hotspot: hotspot, size: size) }
        }
        screen.onLiveness = { [weak self] alive, rtt, fpsNow in
            guard let self else { return }
            // Fires every tick (20 Hz): publish only when the visible
            // values change, so the HUD is not re-rendered for nothing.
            let isAlive = alive == 1
            let fpsSafe: Float = fpsNow.isFinite ? min(max(fpsNow, 0), 10_000) : 0
            let tenths = Int((fpsSafe * 10).rounded())
            let last = self.lastLiveness
            guard last.alive != isAlive || last.rtt != rtt || last.fpsTenths != tenths else { return }
            self.lastLiveness = (alive: isAlive, rtt: rtt, fpsTenths: tenths)
            DispatchQueue.main.async { [weak self] in
                guard let model = self?.livenessModel else { return }
                if model.peerAlive != isAlive { model.peerAlive = isAlive }
                if model.rttMs != rtt { model.rttMs = rtt }
                model.fps = fpsSafe
            }
        }
        screen.onMonitors = { [weak self] list in
            let mons: [MonitorInfo] = list.map { (x: $0.0, y: $0.1, w: $0.2, h: $0.3, primary: $0.4) }
            self?.onMain { $0.applyMonitors(mons) }
        }
        // Remote audio (v0.21): each tick is 960 s16le samples; converted to
        // Float32 and scheduled on the player node, here off main.
        screen.onAudio = { [weak self] data in
            self?.audioPlayer.playTick(data)
        }
        // Phase 4: Opus packets (only after we advertise CAP_OPUS, which
        // `enableOpus` does — and only when AVAudioConverter can decode
        // Opus here; otherwise the host keeps sending raw PCM above).
        if RemoteAudioPlayer.opusDecodeAvailable {
            screen.onOpus = { [weak self] packet, samples in
                self?.audioPlayer.playOpus(packet, samples: samples)
            }
            screen.enableOpus()
        }
        // Clipboard sync (v0.26): apply peer text to the pasteboard (main)
        // and arm the echo guard so we never re-send what we just applied.
        screen.onClipboard = { [weak self] kind, payload in
            self?.onMain { $0.clipboard.noteRemote(kind: kind, payload: payload) }
        }
        // File transfer (v0.27): a completed peer transfer lands here (name
        // + bytes); write it into the app container (off main, never
        // overwriting an earlier file of the same name) and surface it in
        // the received-files list.
        screen.onFile = { [weak self] name, data in
            guard let self else { return }
            let dir = FileManager.default.urls(for: .documentDirectory,
                                               in: .userDomainMask).first
                ?? FileManager.default.temporaryDirectory
            let url = Self.uniqueURL(in: dir, name: name)
            do {
                try data.write(to: url, options: .atomic)
                let file = ReceivedFile(name: url.lastPathComponent, url: url,
                                        size: data.count, date: Date())
                self.onMain {
                    $0.receivedFiles.append(file)
                    $0.toast = "Received \(file.name)"
                }
                self.log("File received: \(url.lastPathComponent) (\(data.count) bytes)")
            } catch {
                self.log("File write failed: \(error.localizedDescription)")
            }
        }
    }

    /// Main: the host's monitor list arrived (also after a reconnect):
    /// restore the saved / chosen monitor.
    private func applyMonitors(_ mons: [MonitorInfo]) {
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

    // MARK: H.264 overlay (session queue)

    /// One H.264 packet (fires inside `screen.feed`): build the sample here,
    /// off main, then enqueue it on the overlay layer on main. END only
    /// marks the generation to hide with the next (refresh) frame.
    private func handleVideo(_ packet: RemoteVideoPacket) {
        if packet.isEnd {
            if videoActive {
                videoActive = false
                pendingHideGen = videoGen
            }
            return
        }
        if !videoActive {
            videoActive = true
            videoGen += 1
            pendingHideGen = nil
        }
        let gen = videoGen
        guard let built = videoSamples.make(packet) else {
            // Not decodable (no keyframe yet / bad data): ask for an IDR —
            // deferred, we are still inside ov_screen_feed.
            queue.async { [weak self] in
                guard let self, !self.halted else { return }
                self.screen.requestVideoIdr()
            }
            return
        }
        let overlay = screenModel.video
        let sample = built.sample
        let flush = built.formatChanged
        let rect = packet.rect
        let size = packet.screenSize
        DispatchQueue.main.async {
            overlay.enqueue(sample, rect: rect, screen: size, generation: gen, flush: flush)
        }
    }

    // MARK: Pump (session queue)

    private func tick() {
        guard !halted, let session else { return }
        do {
            try session.pump()
            if !isDirect {
                try session.pumpRelay()
            }
            // Liveness (v0.16): send a keepalive PING when the Rust
            // keepalive says one is due, answer the peer's PINGs, and run
            // the liveness verdict (fires the sink -> LivenessModel).
            let nowMs = Int64((DispatchTime.now().uptimeNanoseconds &- pumpEpochNs) / 1_000_000)
            let seq = screen.keepaliveTick(nowMs: nowMs)
            if seq > 0 {
                var ping = Data([0x10])
                ping.append(contentsOf: withUnsafeBytes(of: seq.bigEndian) { Data($0) })
                try session.sendRecord(channel: 0, payload: ping)
            }
            try sendPongIfOwed()
            screen.livenessTick(nowMs: nowMs)
            // Section 2: publish CAP_INPUT2 once the host's CAPS arrive.
            let caps = screen.peerCaps()
            if caps != lastPeerCaps {
                lastPeerCaps = caps
                let input2 = caps & InputWire.capInput2 != 0
                let fx = caps & Self.capDesktopFx != 0
                if fx && !sentDesktopFx {
                    sentDesktopFx = true
                    try sendDesktopWishes(session)
                }
                DispatchQueue.main.async { [weak self] in
                    guard let self else { return }
                    self.input.input2 = input2
                    if self.hostSupportsDesktopFx != fx {
                        self.hostSupportsDesktopFx = fx
                    }
                }
            }
        } catch {
            handle(error)
        }
        afterRelayPump()
    }

    // MARK: Desktop effects (Section 4)

    /// The host can hide its wallpaper / animations: send the saved wishes
    /// (only the "on" ones — the host leaves its desktop alone otherwise).
    private func sendDesktopWishes(_ session: OvSessionBox) throws {
        let wishes: [(UInt8, Bool)] = [(1, AppSettings.hideWallpaper), (2, AppSettings.disableAnimations)]
        for (key, on) in wishes where on {
            if let payload = OvSessionBox.optionRecord(key: key, on: on) {
                try session.sendRecord(channel: 0, payload: payload)
            }
        }
    }

    /// Session menu: one desktop-effect switch changed (main). `key` 1 =
    /// hide wallpaper, 2 = disable animations. Ignored for hosts without
    /// CAP_DESKTOP_FX.
    func setDesktopOption(key: UInt8, on: Bool) {
        guard hostSupportsDesktopFx, let payload = OvSessionBox.optionRecord(key: key, on: on) else { return }
        sendCommand(payload)
    }

    // MARK: Direct path (Section 5, session queue)

    /// Install the direct-path sinks and advertise CAP_DIRECT. Relay
    /// sessions only; the host then offers candidates and Rust probes,
    /// promotes and falls back inside the pump calls.
    private func enableDirectPath(_ session: OvSessionBox) {
        let paths = DirectPathSet(queue: queue) { [weak self] pathID, data in
            self?.directDatagramIn(pathID: pathID, data)
        }
        session.onDirectOpen = { [weak self] pathID, address in
            // Fires inside a pump (session lock held): only open the socket.
            guard let self, !self.halted, let paths = self.p2pPaths else { return }
            if paths.open(pathID: pathID, address: address) {
                self.log("Direct path: probing \(address)")
            } else {
                self.log("Direct path: unusable candidate \(address)")
            }
        }
        session.onDirectSend = { [weak self] pathID, data in
            self?.p2pPaths?.send(pathID: pathID, data)
        }
        do {
            try session.setDirectSinks()
        } catch {
            session.onDirectOpen = nil
            session.onDirectSend = nil
            paths.closeAll()
            log("Direct path unavailable: \(error)")
            return
        }
        p2pPaths = paths
        p2pEnabled = true
        screen.setDirectCapable(true)
        let timer = DispatchSource.makeTimerSource(queue: queue)
        timer.schedule(deadline: .distantFuture, repeating: .never)
        timer.setEventHandler { [weak self] in
            guard let self else { return }
            self.p2pTimerDue = nil
            self.p2pPump()
        }
        p2pTimer = timer
        timer.resume()
    }

    /// One datagram on a direct path: queue it into the engine and pump
    /// once per burst (~1 ms later, or now when many are waiting).
    private func directDatagramIn(pathID: UInt32, _ data: Data) {
        guard !halted, let session else { return }
        session.onDirectDatagram(pathID: pathID, data)
        p2pQueued += 1
        if p2pQueued >= Self.directBatchMax {
            p2pPump()
            return
        }
        guard !p2pPumpScheduled else { return }
        p2pPumpScheduled = true
        queue.asyncAfter(deadline: .now() + .milliseconds(1)) { [weak self] in
            guard let self else { return }
            self.p2pPumpScheduled = false
            if self.p2pQueued > 0 {
                self.p2pPump()
            }
        }
    }

    /// Pump the relay session (relay frames + queued direct datagrams +
    /// engine timers), answer a PING at once, and re-arm the timer. Sends
    /// on the active path go out as one NWConnection batch.
    private func p2pPump() {
        p2pQueued = 0
        guard !halted, let session else { return }
        var failure: Error?
        let run = {
            do {
                try session.pumpRelay()
                try self.sendPongIfOwed()
            } catch {
                failure = error
            }
        }
        if let paths = p2pPaths {
            paths.batch(pathID: session.directPath, run)
        } else {
            run()
        }
        if let failure {
            handle(failure)
        }
        afterRelayPump()
    }

    /// After any relay pump: publish the path state and keep the engine's
    /// timer armed (every <= 20 ms, or at its deadline) while probing or
    /// direct. Idle relay sessions stay on the 50 ms tick.
    private func afterRelayPump() {
        guard relayMode, !halted, let session else { return }
        publishDirectState()
        guard p2pEnabled, let timer = p2pTimer else { return }
        let state = session.directState
        guard state > 0 else { return }
        let deadline = session.directDeadlineMs
        var ms = deadline >= 0 ? min(deadline, Self.p2pMaxIntervalMs) : Self.p2pMaxIntervalMs
        ms = max(ms, 1)
        let due = DispatchTime.now() + .milliseconds(Int(ms))
        if let armed = p2pTimerDue, armed <= due {
            return // an earlier wake-up is already set
        }
        p2pTimerDue = due
        timer.schedule(deadline: due, repeating: .never, leeway: .milliseconds(1))
    }

    /// Publish "Relay" / "Probing…" / "Direct (LAN|Internet)" on change and
    /// log promotions / fallbacks.
    private func publishDirectState() {
        guard relayMode, let session else { return }
        let state = p2pEnabled ? session.directState : 0
        let path = state == 2 ? session.directPath : -1
        guard state != lastDirectState || path != lastDirectPath else { return }
        let previous = lastDirectState
        lastDirectState = state
        lastDirectPath = path
        let label: String
        switch state {
        case 1:
            label = "Probing…"
        case 2:
            let kind = p2pPaths?.kind(of: path) ?? .internet
            label = "Direct (\(kind.label))"
            let addr = p2pPaths?.address(of: path) ?? "?"
            log("Direct path active (\(kind.label), \(addr))")
        default:
            label = "Relay"
            if previous == 2 {
                log("Direct path lost — back on the relay")
            }
        }
        DispatchQueue.main.async { [weak self] in
            guard let model = self?.livenessModel, model.path != label else { return }
            model.path = label
        }
    }

    /// Direct mode: pump once for the batch of datagrams queued so far, ~1 ms
    /// from now (more datagrams of the same burst arrive meanwhile).
    private func scheduleDirectPump() {
        guard !directPumpScheduled else { return }
        directPumpScheduled = true
        queue.asyncAfter(deadline: .now() + .milliseconds(1)) { [weak self] in
            guard let self else { return }
            self.directPumpScheduled = false
            if self.directQueued > 0 {
                self.directPump()
            }
        }
    }

    /// Direct mode: drain the queued datagrams (sinks fire here) and answer
    /// any PING right away. The bridge batches the ACKs / PONG this emits
    /// into one NWConnection send batch.
    private func directPump() {
        directQueued = 0
        guard !halted, let session else { return }
        var failure: Error?
        let run = {
            do {
                try session.pump()
                try self.sendPongIfOwed()
            } catch {
                failure = error
            }
        }
        if let bridge = udpBridge {
            bridge.batch(run)
        } else {
            run()
        }
        if let failure {
            handle(failure)
        }
    }

    /// Answer an inbound PING right after the inbound pump that fed it (not
    /// only on the next tick), so the peer's RTT is not inflated by 50 ms.
    private func sendPongIfOwed() throws {
        guard let session else { return }
        if let pong = screen.takePongReply() {
            try session.sendRecord(channel: 0, payload: pong)
        }
    }

    /// Route an FFI error: handshake failure / closed relay / transport
    /// failure end the attempt once (pump stopped, one clear phase);
    /// anything else is logged, deduplicated against the previous error.
    private func handle(_ error: Error, context: String = "error") {
        if let ffi = error as? OvSessionBox.FfiError {
            switch ffi {
            case .crypto:
                failOnQueue(established ? .disconnected("secure channel error") : .authFailed)
                return
            case .closed:
                failOnQueue(.disconnected("the relay closed the connection"))
                return
            case .transport:
                failOnQueue(established ? .disconnected("network error") : .transport("Network error."))
                return
            case .badArg, .wrongState:
                break
            }
        }
        let line = "\(context): \(error)"
        guard line != lastErrorLine else { return }
        lastErrorLine = line
        log(line)
    }

    /// Fatal for this attempt: stop pumping and close the transports
    /// exactly once, then let main decide (reconnect or error view).
    private func failOnQueue(_ error: SessionError) {
        guard !halted else { return }
        haltOnQueue()
        log(error.logText)
        let gen = queueGeneration
        onMain { $0.handleFailure(error, generation: gen) }
    }

    /// Stop the pump and close the transports (the session object stays
    /// until the next attempt / teardown). Idempotent.
    private func haltOnQueue() {
        halted = true
        pumpTimer?.cancel()
        pumpTimer = nil
        if let ws = relaySocket {
            ws.onMessage = nil
            ws.onClose = nil
            ws.close()
        }
        relaySocket = nil
        udpBridge?.close()
        udpBridge = nil
        p2pTimer?.cancel()
        p2pTimer = nil
        p2pTimerDue = nil
        p2pPaths?.closeAll()
        p2pPaths = nil
        p2pEnabled = false
        audioPlayer.stop()
    }

    /// Full teardown (session closed): stop the pump, close the transports
    /// and release every callback so the session box can deinit.
    private func teardownOnQueue() {
        tornDown = true
        haltOnQueue()
        session?.clearCallbacks()
        screen.clearCallbacks()
    }

    // MARK: Outbound (any thread -> session queue)

    /// Send one chat line on channel 1 (shown right away in the chat).
    func sendChat(_ text: String) {
        guard !text.isEmpty else { return }
        appendChat(text, fromMe: true)
        queue.async { [weak self] in
            guard let self else { return }
            guard !self.halted, let session = self.session else {
                self.log("Chat message not sent: not connected")
                return
            }
            do {
                try session.sendRecord(channel: 1, payload: Data(text.utf8))
            } catch {
                self.handle(error, context: "chat send failed")
            }
        }
    }

    /// Shared command-channel send: errors (e.g. not yet established) are
    /// surfaced in the session log.
    func sendCommand(_ payload: Data) {
        queue.async { [weak self] in
            guard let self, !self.halted, let session = self.session else { return }
            do {
                try session.sendRecord(channel: 0, payload: payload)
            } catch {
                self.handle(error, context: "input failed")
            }
        }
    }

    // MARK: - Display / quality (main thread)

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
        sendQuality(targetFps: fps)
    }

    /// Send one CAPTURE_REGION command (v0.18): i32-BE x, i32-BE y,
    /// u32-BE w, u32-BE h — the monitor rect in virtual-desktop coords.
    func sendCaptureRegion(monitorAt index: Int) {
        guard monitors.indices.contains(index) else { return }
        let m = monitors[index]
        var payload = Data([0x37])
        payload.append(contentsOf: withUnsafeBytes(of: m.x.bigEndian) { Data($0) })
        payload.append(contentsOf: withUnsafeBytes(of: m.y.bigEndian) { Data($0) })
        payload.append(contentsOf: withUnsafeBytes(of: m.w.bigEndian) { Data($0) })
        payload.append(contentsOf: withUnsafeBytes(of: m.h.bigEndian) { Data($0) })
        sendCommand(payload)
    }

    /// Send one QUALITY_SET command (v0.17): 2-byte u16-BE target fps,
    /// 0 = AUTO (the host's RTT-aware governor picks the rate).
    func sendQuality(targetFps: UInt16) {
        var payload = Data([0x36])
        payload.append(contentsOf: withUnsafeBytes(of: targetFps.bigEndian) { Data($0) })
        sendCommand(payload)
    }

    // MARK: - Clipboard / files

    /// Push the local clipboard text to the host now (session menu).
    func sendClipboardNow() {
        guard let text = UIPasteboard.general.string, !text.isEmpty else {
            toast = "The clipboard has no text"
            return
        }
        sendClipboardPayload(text)
        toast = "Clipboard sent to the host"
    }

    /// Type the local clipboard text into the host (for apps that ignore
    /// clipboard sync, e.g. password fields).
    func typeClipboardText() {
        guard let text = UIPasteboard.general.string, !text.isEmpty else {
            toast = "The clipboard has no text"
            return
        }
        input.typeText(text)
    }

    private func sendClipboardPayload(_ text: String) {
        var payload = Data([0x50, 0]) // CLIPBOARD, kind 0 = text
        payload.append(contentsOf: Array(text.utf8))
        sendCommand(payload)
    }

    /// Send-file (v0.27): read the picked document (security-scoped) and
    /// ship it through the Rust chunking path, both off main. Errors land
    /// in the session log.
    func sendFile(url: URL) {
        queue.async { [weak self] in
            guard let self else { return }
            let data: Data
            do {
                let scoped = url.startAccessingSecurityScopedResource()
                defer { if scoped { url.stopAccessingSecurityScopedResource() } }
                data = try Data(contentsOf: url)
            } catch {
                self.log("File read failed: \(error.localizedDescription)")
                self.onMain { $0.toast = "Couldn't read \(url.lastPathComponent)" }
                return
            }
            guard !self.halted, let session = self.session else {
                self.log("File send failed: not connected")
                self.onMain { $0.toast = "Not connected — file not sent" }
                return
            }
            do {
                try session.sendFile(name: url.lastPathComponent, data: data)
                self.log("File sent: \(url.lastPathComponent) (\(data.count) bytes)")
                self.onMain { $0.toast = "Sent \(url.lastPathComponent)" }
            } catch {
                self.handle(error, context: "file send failed")
            }
        }
    }

    // MARK: Clipboard polling (main thread)

    /// Check the pasteboard twice a second. `pollLocal` compares the cheap
    /// `changeCount` first and only reads the text (which may show the paste
    /// prompt) when it actually changed.
    private func startClipboardPolling() {
        let timer = DispatchSource.makeTimerSource(queue: .main)
        timer.schedule(deadline: .now() + .milliseconds(500),
                       repeating: .milliseconds(500),
                       leeway: .milliseconds(100))
        timer.setEventHandler { [weak self] in
            self?.clipboard.pollLocal()
        }
        clipboardTimer = timer
        timer.resume()
    }

    // MARK: Helpers

    /// Run `work` on main with the controller, if it is still alive.
    private func onMain(_ work: @escaping (SessionController) -> Void) {
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            work(self)
        }
    }

    /// Append a session-log line (callable from any thread).
    private func log(_ text: String) {
        onMain { $0.appendLine(text) }
    }

    /// Diagnostics lines from the session UI ("Log touch routing").
    func appendDebugLog(_ text: String) {
        log("[debug] " + text)
    }

    /// Main thread: append and cap the log at `maxLines`.
    private func appendLine(_ text: String) {
        nextLineID += 1
        lines.append(LogLine(id: nextLineID, text: text, date: Date()))
        if lines.count > Self.maxLines {
            lines.removeFirst(lines.count - Self.maxLines)
        }
    }

    /// Main thread: a chat line from the peer.
    private func receiveChat(_ text: String) {
        appendChat(text, fromMe: false)
        if !chatVisible {
            unreadChat += 1
        }
    }

    /// Main thread: append and cap the chat.
    private func appendChat(_ text: String, fromMe: Bool) {
        nextLineID += 1
        chat.append(ChatMessage(id: nextLineID, text: text, fromMe: fromMe, date: Date()))
        if chat.count > Self.maxChat {
            chat.removeFirst(chat.count - Self.maxChat)
        }
    }

    /// Merge a screen update and schedule one main-thread flush if none is
    /// pending (frames / cursor moves coalesce while main is busy).
    private func postScreen(_ mutate: (inout PendingScreen) -> Void) {
        screenLock.lock()
        mutate(&pendingScreen)
        let schedule = !screenFlushScheduled
        screenFlushScheduled = true
        screenLock.unlock()
        guard schedule else { return }
        DispatchQueue.main.async { [weak self] in
            self?.flushScreen()
        }
    }

    /// Main thread: apply the accumulated screen update to `ScreenModel`.
    private func flushScreen() {
        screenLock.lock()
        let p = pendingScreen
        pendingScreen = PendingScreen()
        screenFlushScheduled = false
        screenLock.unlock()
        guard !p.isEmpty else { return }
        let model = screenModel
        if let frame = p.frame {
            // Pixels first: setting `image` re-renders the surface, which
            // pulls them for the Metal texture upload.
            model.pixels.publish(frame.pixels)
            model.image = frame.image
            if model.frameSize != frame.size { model.frameSize = frame.size }
            markLiveIfNeeded()
        }
        if let gen = p.hideVideoGen {
            if p.frame != nil {
                // Hidden by the surface once this frame is presented.
                model.video.requestHide(generation: gen)
            } else {
                model.video.hide(generation: gen)
            }
        }
        if let pos = p.cursorPos {
            model.cursorPos = pos
        }
        if let shape = p.cursorShape {
            model.cursorImage = shape.image
            model.cursorHotspot = shape.hotspot
            model.cursorSize = shape.size
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

    /// A file URL in `dir` for `name` that does not exist yet: "a.txt",
    /// then "a (1).txt", "a (2).txt", …
    private static func uniqueURL(in dir: URL, name: String) -> URL {
        let fm = FileManager.default
        let base = name.isEmpty ? "file" : name
        var url = dir.appendingPathComponent(base)
        guard fm.fileExists(atPath: url.path) else { return url }
        let ns = base as NSString
        let ext = ns.pathExtension
        let stem = ns.deletingPathExtension
        var n = 1
        repeat {
            let candidate = ext.isEmpty ? "\(stem) (\(n))" : "\(stem) (\(n)).\(ext)"
            url = dir.appendingPathComponent(candidate)
            n += 1
        } while fm.fileExists(atPath: url.path) && n < 10_000
        return url
    }
}

// MARK: - Audio

// Audio (v0.21): remote mic streamed as 20 ms 48 kHz mono ticks — raw
// s16le, or (Phase 4) Opus packets decoded with AVAudioConverter — played
// back through AVAudioEngine + AVAudioPlayerNode. Driven from the session
// queue; only the queued-buffer counter is shared with the audio render
// thread (completion handlers), so it is lock-guarded.
//
// Jitter handling mirrors the desktop JitterBuffer: playback starts only
// once `prefillTicks` (60 ms) are queued, a starved player pauses and
// re-prefills, and more than `maxQueued` (120 ms) queued drops new ticks.
final class RemoteAudioPlayer {
    private let engine = AVAudioEngine()
    private let player = AVAudioPlayerNode()
    private let format = AVAudioFormat(commonFormat: .pcmFormatFloat32,
                                       sampleRate: 48_000,
                                       channels: 1,
                                       interleaved: false)
    private var started = false
    /// The player node is running (false while prefilling).
    private var playing = false
    /// Buffers scheduled but not yet consumed by the player.
    private var queued = 0
    private let queuedLock = NSLock()
    /// Max ticks (20 ms each) allowed in flight; beyond that new ticks are
    /// dropped so network bursts cannot ratchet the latency up.
    private static let maxQueued = 6
    /// Ticks queued before playback (re)starts: the jitter cushion.
    private static let prefillTicks = 3
    /// Opus decoder state (Phase 4), created on the first packet.
    private var opusConverter: AVAudioConverter?
    private var opusFormat: AVAudioFormat?
    /// Largest Opus packet (RFC 6716).
    private static let maxOpusPacket = 1275

    /// The Opus stream format: 48 kHz mono, 960-frame (20 ms) packets.
    private static func makeOpusFormat() -> AVAudioFormat? {
        var desc = AudioStreamBasicDescription(mSampleRate: 48_000,
                                               mFormatID: kAudioFormatOpus,
                                               mFormatFlags: 0,
                                               mBytesPerPacket: 0,
                                               mFramesPerPacket: 960,
                                               mBytesPerFrame: 0,
                                               mChannelsPerFrame: 1,
                                               mBitsPerChannel: 0,
                                               mReserved: 0)
        return AVAudioFormat(streamDescription: &desc)
    }

    /// Whether this device can decode Opus (checked once). When false the
    /// shell never advertises CAP_OPUS and the host sends raw PCM.
    static let opusDecodeAvailable: Bool = {
        guard let opus = makeOpusFormat(),
              let pcm = AVAudioFormat(commonFormat: .pcmFormatFloat32,
                                      sampleRate: 48_000,
                                      channels: 1,
                                      interleaved: false) else { return false }
        return AVAudioConverter(from: opus, to: pcm) != nil
    }()

    /// Decode one audio tick (bare s16le 48 kHz mono samples — the sink
    /// delivers samples only, no header) and schedule it for playback.
    /// Starts the engine lazily on the first tick. Malformed ticks are
    /// dropped silently.
    func playTick(_ data: Data) {
        guard let format, data.count >= 2 else { return }
        let n = data.count / 2 // a stray odd trailing byte is ignored
        guard let buf = AVAudioPCMBuffer(pcmFormat: format,
                                         frameCapacity: AVAudioFrameCount(n)),
              let dst = buf.floatChannelData?[0] else { return }
        buf.frameLength = AVAudioFrameCount(n)
        data.withUnsafeBytes { (raw: UnsafeRawBufferPointer) in
            // Byte-wise little-endian decode: no alignment assumptions.
            for i in 0..<n {
                let lo = UInt16(raw[2 * i])
                let hi = UInt16(raw[2 * i + 1])
                dst[i] = Float(Int16(bitPattern: lo | (hi << 8))) / 32768.0
            }
        }
        schedule(buf)
    }

    /// Decode one Opus packet (Phase 4; passed through by Rust) with
    /// AVAudioConverter and schedule the PCM. The converter keeps its state
    /// across packets (`.noDataNow` after each one, never end-of-stream).
    /// Malformed packets are dropped silently.
    func playOpus(_ packet: Data, samples: UInt32) {
        guard let format, !packet.isEmpty, packet.count <= Self.maxOpusPacket else { return }
        if opusConverter == nil {
            guard let opus = Self.makeOpusFormat(),
                  let conv = AVAudioConverter(from: opus, to: format) else { return }
            opusFormat = opus
            opusConverter = conv
        }
        guard let conv = opusConverter, let opusFormat else { return }
        let compressed = AVAudioCompressedBuffer(format: opusFormat,
                                                 packetCapacity: 1,
                                                 maximumPacketSize: Self.maxOpusPacket)
        let size = packet.count
        packet.withUnsafeBytes { (raw: UnsafeRawBufferPointer) in
            guard let src = raw.baseAddress else { return }
            compressed.data.copyMemory(from: src, byteCount: size)
        }
        compressed.byteLength = UInt32(size)
        compressed.packetCount = 1
        if let descs = compressed.packetDescriptions {
            descs[0] = AudioStreamPacketDescription(mStartOffset: 0,
                                                    mVariableFramesInPacket: 0,
                                                    mDataByteSize: UInt32(size))
        }
        let frames = AVAudioFrameCount(max(samples, 960))
        guard let pcm = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: frames) else { return }
        var supplied = false
        var error: NSError?
        let status = conv.convert(to: pcm, error: &error) { _, inputStatus in
            if supplied {
                inputStatus.pointee = .noDataNow
                return nil
            }
            supplied = true
            inputStatus.pointee = .haveData
            return compressed
        }
        guard status != .error, error == nil, pcm.frameLength > 0 else { return }
        schedule(pcm)
    }

    /// Queue one decoded buffer, applying the prefill / underrun / ceiling
    /// rules described above.
    private func schedule(_ buf: AVAudioPCMBuffer) {
        guard let format else { return }
        if !started {
            guard startEngine(format: format) else { return }
        }
        queuedLock.lock()
        let depth = queued
        queuedLock.unlock()
        guard depth < Self.maxQueued else { return }
        if playing && depth == 0 {
            // Underrun: pause and re-prefill so playback resumes with a
            // cushion instead of stuttering tick by tick.
            player.pause()
            playing = false
        }
        queuedLock.lock()
        queued += 1
        let now = queued
        queuedLock.unlock()
        player.scheduleBuffer(buf, completionHandler: { [weak self] in
            guard let self else { return }
            self.queuedLock.lock()
            self.queued = max(0, self.queued - 1)
            self.queuedLock.unlock()
        })
        if !playing && now >= Self.prefillTicks {
            player.play()
            playing = true
        }
    }

    private func startEngine(format: AVAudioFormat) -> Bool {
        // .playback so the ring/silent switch does not mute the remote
        // audio; mixWithOthers so we do not stop the user's music.
        let audioSession = AVAudioSession.sharedInstance()
        try? audioSession.setCategory(.playback, mode: .default, options: [.mixWithOthers])
        try? audioSession.setActive(true)
        do {
            if player.engine == nil {
                engine.attach(player)
                engine.connect(player, to: engine.mainMixerNode, format: format)
            }
            try engine.start()
            // Playback starts once the prefill cushion is queued (schedule()).
            playing = false
            started = true
            return true
        } catch {
            return false
        }
    }

    func stop() {
        guard started else { return }
        player.stop()
        engine.stop()
        started = false
        playing = false
        opusConverter?.reset()
        queuedLock.lock()
        queued = 0
        queuedLock.unlock()
        try? AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
    }
}

// MARK: - Clipboard

// Clipboard sync (v0.26): mirrors `ovapp::clipboard::ClipboardSync` on the
// Rust side. The iOS shell does not run the Rust sync state, so it keeps
// its own echo guard: text just applied from the peer (or just sent) is
// never re-sent, breaking the apply -> poll -> send echo loop. Main only.
final class ClipboardBridge {
    private var lastSeenLocal: String?
    private var lastRemote: String?
    private var lastChangeCount: Int

    /// Wire the channel-0 send path (CLIPBOARD command builder).
    var send: ((String) -> Void)?

    init() {
        // Seed with the current pasteboard generation so pre-existing
        // clipboard content is not blasted to the peer on connect.
        lastChangeCount = UIPasteboard.general.changeCount
    }

    /// Peer CLIPBOARD command arrived: apply it to the pasteboard and arm
    /// the echo guard. Unknown kinds are ignored (only 0 = text exists).
    func noteRemote(kind: UInt8, payload: Data) {
        guard kind == 0, let text = String(data: payload, encoding: .utf8) else { return }
        lastRemote = text
        lastSeenLocal = text
        UIPasteboard.general.string = text
        lastChangeCount = UIPasteboard.general.changeCount
    }

    /// Poll the local pasteboard. `changeCount` is cheap and never triggers
    /// the paste prompt; the text itself is read only after it changed (and
    /// only if the pasteboard holds text). Fires `send` when a genuinely new
    /// local clipboard text appears.
    func pollLocal() {
        let pb = UIPasteboard.general
        let count = pb.changeCount
        guard count != lastChangeCount else { return }
        lastChangeCount = count
        guard pb.hasStrings, let text = pb.string else { return }
        guard text != lastSeenLocal, text != lastRemote else { return }
        lastSeenLocal = text
        send?(text)
    }
}

// MARK: - Frame images

/// CGImage builders for remote frames and cursor sprites. Pure functions,
/// run on the session queue. Malformed sizes return nil, never crash.
enum FrameImage {
    /// Largest accepted frame / cursor edge, in pixels.
    static let maxDimension = 8192
    static let maxCursorDimension = 1024

    /// Build a displayable `CGImage` from a reassembled frame.
    ///
    /// Format 0 (BGRA, 4 bytes/px) is wrapped as-is — zero conversion, the
    /// provider references the frame's `Data` — as `noneSkipFirst` +
    /// `32Little`: the 4th byte is GDI's unused high byte (0 for real
    /// capture), not alpha, so it is skipped and the image is opaque.
    /// Format 1 (BGR, 3 bytes/px) is expanded once into BGRX. `stride` is
    /// the source row pitch in bytes (0 / short buffers fall back to tightly
    /// packed rows).
    static func make(width: UInt32, height: UInt32, stride: UInt32,
                     format: UInt8, pixels: Data) -> (image: CGImage, bytes: Data, rowBytes: Int)? {
        let w = Int(width), h = Int(height)
        guard w > 0, h > 0, w <= maxDimension, h <= maxDimension else { return nil }
        let bpp: Int
        switch format {
        case 0: bpp = 4
        case 1: bpp = 3
        default: return nil
        }
        guard let rowBytes = rowPitch(width: w, height: h, bpp: bpp,
                                      stride: Int(stride), available: pixels.count) else { return nil }
        let info = CGBitmapInfo(rawValue: CGImageAlphaInfo.noneSkipFirst.rawValue
            | CGBitmapInfo.byteOrder32Little.rawValue)
        let provider: CGDataProvider
        let outRowBytes: Int
        let bgra: Data
        if bpp == 4 {
            guard let p = CGDataProvider(data: pixels as CFData) else { return nil }
            provider = p
            outRowBytes = rowBytes
            bgra = pixels
        } else {
            // BGR -> BGRX, one pass over raw pointers (no per-pixel bounds
            // checks; the extents were validated above).
            let dstRow = w * 4
            var out = Data(count: dstRow * h)
            out.withUnsafeMutableBytes { (dstRaw: UnsafeMutableRawBufferPointer) in
                pixels.withUnsafeBytes { (srcRaw: UnsafeRawBufferPointer) in
                    guard let dBase = dstRaw.baseAddress?.assumingMemoryBound(to: UInt8.self),
                          let sBase = srcRaw.baseAddress?.assumingMemoryBound(to: UInt8.self) else { return }
                    for y in 0..<h {
                        var s = sBase + y * rowBytes
                        var d = dBase + y * dstRow
                        for _ in 0..<w {
                            d[0] = s[0]; d[1] = s[1]; d[2] = s[2]; d[3] = 255
                            s += 3
                            d += 4
                        }
                    }
                }
            }
            guard let p = CGDataProvider(data: out as CFData) else { return nil }
            provider = p
            outRowBytes = dstRow
            bgra = out
        }
        guard let image = CGImage(
            width: w, height: h,
            bitsPerComponent: 8, bitsPerPixel: 32, bytesPerRow: outRowBytes,
            space: CGColorSpaceCreateDeviceRGB(),
            bitmapInfo: info,
            provider: provider,
            decode: nil,
            shouldInterpolate: true,
            intent: .defaultIntent
        ) else { return nil }
        return (image: image, bytes: bgra, rowBytes: outRowBytes)
    }

    /// The usable source row pitch, or nil when the buffer cannot hold the
    /// frame. Prefers the announced stride; falls back to packed rows.
    /// All products are overflow-checked.
    private static func rowPitch(width w: Int, height h: Int, bpp: Int,
                                 stride: Int, available: Int) -> Int? {
        let (packed, o1) = w.multipliedReportingOverflow(by: bpp)
        guard !o1 else { return nil }
        if stride >= packed {
            let (need, o2) = stride.multipliedReportingOverflow(by: h)
            if !o2, available >= need { return stride }
        }
        let (needPacked, o3) = packed.multipliedReportingOverflow(by: h)
        guard !o3, available >= needPacked else { return nil }
        return packed
    }

    /// Build the cursor overlay image from a premultiplied-BGRA `CURSOR_SHAPE`
    /// bitmap. Unlike frames the alpha channel is kept (that is the whole
    /// point of a cursor sprite); the layout is the same BGRA32-little mapping.
    static func makeCursor(w: UInt16, h: UInt16, bgra: Data) -> CGImage? {
        let width = Int(w), height = Int(h)
        guard width > 0, height > 0,
              width <= maxCursorDimension, height <= maxCursorDimension,
              bgra.count >= width * height * 4 else { return nil }
        // 8-bit premultiplied BGRA: little-endian byte order (B first in
        // memory) = alpha-first 32-bit words, hence premultipliedFirst +
        // byteOrder32Little.
        let info = CGBitmapInfo(rawValue: CGImageAlphaInfo.premultipliedFirst.rawValue
            | CGBitmapInfo.byteOrder32Little.rawValue)
        guard let provider = CGDataProvider(data: bgra as CFData) else { return nil }
        return CGImage(
            width: width, height: height,
            bitsPerComponent: 8, bitsPerPixel: 32, bytesPerRow: width * 4,
            space: CGColorSpaceCreateDeviceRGB(),
            bitmapInfo: info,
            provider: provider,
            decode: nil,
            shouldInterpolate: true,
            intent: .defaultIntent
        )
    }
}
