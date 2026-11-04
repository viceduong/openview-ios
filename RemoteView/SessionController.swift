import SwiftUI
import Combine
import AVFoundation

// Session engine behind SessionView. Owns the OvSessionBox wiring, the
// screen assembler, the transports and the pump; publishes only finished,
// UI-ready state to the main thread.
//
// Threading: every session / screen / transport call runs on
// `OvQueues.session` (a background serial queue). Sinks fire there, so all
// decode work (frame -> CGImage, audio conversion, file writes) happens off
// main; only the results hop to main. Main-thread-only members are marked.
//
// Lifetime: owned by SessionView through @StateObject. Every closure handed
// to the session, screen, transports and timers captures `self` weakly, and
// `stop()` / `deinit` clear them all and close the transports, so dropping
// the view frees the controller, then the OvSessionBox (ov_session_free ->
// the Rust relay socket closes).

// MARK: - Log lines

/// One chat/status line; a stable id so the list diffs cheaply.
struct LogLine: Identifiable {
    let id: Int
    let text: String
}

// A file received from the peer (v0.27): already written to the app
// container; `url` opens it (Quick Look / share sheet).
struct ReceivedFile: Identifiable {
    let id = UUID()
    let name: String
    let url: URL
}

/// One host monitor from MONITORS_INFO (v0.18).
typealias MonitorInfo = (x: Int32, y: Int32, w: UInt32, h: UInt32, primary: Bool)

// MARK: - Screen + liveness models

/// Remote screen state, observed only by the screen view so frame / cursor
/// updates never re-evaluate the whole SessionView body. Main thread only.
final class ScreenModel: ObservableObject {
    // Reassembled remote screen (nil until the first frame completes).
    @Published var image: CGImage?
    // The last frame's geometry — the space pointer coordinates normalize to.
    @Published var frameSize: CGSize = .zero
    // Remote cursor overlay (v0.15): position in frame pixel space, and the
    // shape bitmap with its hotspot. nil shape = position-only (hidden).
    @Published var cursorPos: CGPoint?
    @Published var cursorImage: CGImage?
    @Published var cursorHotspot: CGSize = .zero
    @Published var cursorSize: CGSize = .zero
}

/// Liveness HUD state (v0.16), observed only by the HUD row and the
/// dead-peer banner. Main thread only.
final class LivenessModel: ObservableObject {
    @Published var peerAlive = true
    @Published var rttMs: Int32 = -1
    @Published var fps: Float = 0
}

/// Screen updates accumulated on the session queue and applied to
/// `ScreenModel` in one main-thread hop (latest wins).
private struct PendingScreen {
    var frame: (image: CGImage, size: CGSize)?
    var cursorPos: CGPoint?
    var cursorShape: (image: CGImage?, hotspot: CGSize, size: CGSize)?

    var isEmpty: Bool { frame == nil && cursorPos == nil && cursorShape == nil }
}

// MARK: - Controller

final class SessionController: ObservableObject {
    // ---- published UI state (main thread only) ----
    @Published private(set) var lines: [LogLine] = []
    @Published private(set) var peerPine: String = "connecting…"
    /// Terminal session state ("Authentication failed", "Disconnected …");
    /// nil while the session is healthy.
    @Published private(set) var status: String?
    @Published private(set) var receivedFiles: [ReceivedFile] = []
    // Monitors (v0.18): the host's monitor list from MONITORS_INFO; the
    // picker sends CAPTURE_REGION for the chosen monitor.
    @Published private(set) var monitors: [MonitorInfo] = []
    @Published var monitorIndex = 0

    let screenModel = ScreenModel()
    let livenessModel = LivenessModel()

    /// Chat log cap — older lines are dropped.
    private static let maxLines = 200
    private var nextLineID = 0 // main thread only

    // ---- session-queue state ----
    private let session: OvSessionBox
    private let queue = OvQueues.session
    // The Rust frame assembler lives for the controller's lifetime.
    private let screen = OvScreenBox()
    // Audio (v0.21): remote mic playback via AVAudioEngine.
    private let audioPlayer = RemoteAudioPlayer()
    private var pumpTimer: DispatchSourceTimer?
    private var udpBridge: NwUdpBridge?
    /// WebSocket relay mode: the socket ConnectView opened.
    private var relaySocket: RelayWebSocket?
    /// Direct (UDP) mode: inbound arrives via the bridge, no relay pump.
    private var isDirect = false
    /// Set once the handshake completes (picks the -3 error wording).
    private var established = false
    /// Set on teardown or a fatal error: the pump is stopped and inbound
    /// traffic is ignored from then on.
    private var halted = false
    /// The last error line logged, so a repeating error is shown once.
    private var lastErrorLine: String?
    // Liveness (v0.16): shell monotonic ms epoch for the keepalive ticks.
    private let pumpEpochNs = DispatchTime.now().uptimeNanoseconds
    // Last liveness values published, so the HUD only re-renders on change.
    private var lastLiveness: (alive: Bool, rtt: Int32, fpsTenths: Int) = (true, -1, 0)

    // Screen updates coalesced across threads (guarded by `screenLock`).
    private let screenLock = NSLock()
    private var pendingScreen = PendingScreen()
    private var screenFlushScheduled = false

    // ---- main-thread state ----
    // Clipboard sync (v0.26): echo-guarded pasteboard bridge, polled on main.
    private let clipboard = ClipboardBridge()
    private var clipboardTimer: DispatchSourceTimer?
    private var started = false
    private var stopped = false

    init(session: OvSessionBox) {
        self.session = session
    }

    deinit {
        // stop() normally ran already (onDisappear); this is the backstop.
        // Capture the parts, not self, and tear them down on the queue that
        // owns them. Everything here is idempotent.
        clipboardTimer?.cancel()
        let session = self.session
        let screen = self.screen
        let audio = self.audioPlayer
        let timer = self.pumpTimer
        let ws = self.relaySocket
        let udp = self.udpBridge
        queue.async {
            timer?.cancel()
            ws?.onMessage = nil
            ws?.onClose = nil
            ws?.close()
            udp?.close()
            session.clearCallbacks()
            screen.clearCallbacks()
            audio.stop()
        }
    }

    // MARK: Lifecycle (main thread)

    func start() {
        guard !started, !stopped else { return }
        started = true
        // Take the transport hand-off from ConnectView and clear it for
        // every mode (relay included) so nothing stale leaks into the next
        // session.
        let mode = DirectTransport.pending
        let ws = DirectTransport.webSocket
        DirectTransport.pending = nil
        DirectTransport.webSocket = nil

        clipboard.send = { [weak self] text in
            var payload = Data([0x50, 0]) // CLIPBOARD, kind 0 = text
            payload.append(contentsOf: Array(text.utf8))
            self?.sendCommand(payload)
        }
        startClipboardPolling()

        queue.async { [weak self] in
            guard let self else {
                ws?.close() // controller already gone: do not orphan the socket
                return
            }
            self.startOnQueue(mode: mode, ws: ws)
        }
    }

    func stop() {
        guard !stopped else { return }
        stopped = true
        clipboardTimer?.cancel()
        clipboardTimer = nil
        clipboard.send = nil
        // Strong capture on purpose: the teardown must run even if the view
        // releases the controller right after this.
        queue.async { self.teardownOnQueue() }
    }

    // MARK: Session-queue setup

    private func startOnQueue(mode: ConnectMode?, ws: RelayWebSocket?) {
        guard !halted else {
            ws?.close() // stopped before we started: do not orphan the socket
            return
        }
        installScreenSinks()

        session.onEstablished = { [weak self] fp in
            guard let self else { return }
            self.established = true
            let pine = Self.pineString(fp)
            self.onMain { $0.peerPine = pine }
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
                    self.log("peer: \(text)")
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
        if case .direct(let host, let port)? = mode {
            isDirect = true
            udpBridge = NwUdpBridge(host: host, port: port, queue: queue,
                                    onReceive: { [weak self] data in
                guard let self, !self.halted else { return }
                do {
                    try self.session.onDatagram(data)
                    try self.session.pump() // process immediately; sinks fire here
                    try self.sendPongIfOwed()
                } catch {
                    self.handle(error)
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
                guard let self, !self.halted else { return }
                do {
                    try self.session.relayFeed(data)
                    try self.session.pumpRelay()
                    try self.sendPongIfOwed()
                } catch {
                    self.handle(error)
                }
            }
            ws.onClose = { [weak self] closeError in
                guard let self, !self.halted else { return }
                self.session.relayEOF()
                // Drain whatever arrived before the close; the relay then
                // reports closed (-5), which ends the session below.
                do {
                    try self.session.pumpRelay()
                } catch {
                    self.handle(error)
                }
                if !self.halted {
                    self.fail(closeError.map { "Disconnected — relay: \($0.localizedDescription)" }
                              ?? "Disconnected — relay closed")
                }
            }
        }

        do {
            try session.pump()
            if let fp = session.peerFingerprint() {
                established = true
                let pine = Self.pineString(fp)
                onMain { $0.peerPine = pine }
            }
        } catch {
            handle(error)
        }
        guard !halted else { return }

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

    private func installScreenSinks() {
        // Frames: build the CGImage here, off main, then publish only the
        // finished image (coalesced — if main is behind, the newest wins).
        screen.onFrame = { [weak self] _, width, height, stride, format, pixels in
            guard let self,
                  let img = FrameImage.make(width: width, height: height, stride: stride,
                                            format: format, pixels: pixels) else { return }
            let size = CGSize(width: CGFloat(img.width), height: CGFloat(img.height))
            self.postScreen { $0.frame = (image: img, size: size) }
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
            self?.onMain {
                $0.monitors = mons
                $0.monitorIndex = 0
            }
        }
        // Remote audio (v0.21): each tick is 960 s16le samples; converted to
        // Float32 and scheduled on the player node, here off main.
        screen.onAudio = { [weak self] data in
            self?.audioPlayer.playTick(data)
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
                let file = ReceivedFile(name: name, url: url)
                self.onMain { $0.receivedFiles.append(file) }
                self.log("file received: \(url.lastPathComponent) (\(data.count) bytes)")
            } catch {
                self.log("file write failed: \(error)")
            }
        }
    }

    // MARK: Pump (session queue)

    private func tick() {
        guard !halted else { return }
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
        } catch {
            handle(error)
        }
    }

    /// Answer an inbound PING right after the inbound pump that fed it (not
    /// only on the next tick), so the peer's RTT is not inflated by 50 ms.
    private func sendPongIfOwed() throws {
        if let pong = screen.takePongReply() {
            try session.sendRecord(channel: 0, payload: pong)
        }
    }

    /// Route an FFI error: handshake failure / closed relay / transport
    /// failure end the session once (pump stopped, one clear status);
    /// anything else is logged, deduplicated against the previous error.
    private func handle(_ error: Error, context: String = "error") {
        if let ffi = error as? OvSessionBox.FfiError {
            switch ffi {
            case .crypto:
                fail(established
                     ? "Session error — crypto failure"
                     : "Authentication failed — check the PIN or pairing code")
                return
            case .closed:
                fail("Disconnected — the relay closed the connection")
                return
            case .transport:
                fail("Disconnected — transport error")
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

    /// Fatal: stop pumping and close the transports exactly once, then show
    /// a single status instead of an error per tick.
    private func fail(_ message: String) {
        guard !halted else { return }
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
        audioPlayer.stop()
        log(message)
        onMain { $0.status = message }
    }

    /// Full teardown (view gone): stop the pump, close the transports and
    /// release every callback so the session box can deinit.
    private func teardownOnQueue() {
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
        session.clearCallbacks()
        screen.clearCallbacks()
        audioPlayer.stop()
    }

    // MARK: Outbound (any thread -> session queue)

    /// Send one chat line on channel 1.
    func sendChat(_ text: String) {
        queue.async { [weak self] in
            guard let self else { return }
            guard !self.halted else { self.log("send failed: not connected"); return }
            do {
                try self.session.sendRecord(channel: 1, payload: Data(text.utf8))
                self.log("me: \(text)")
            } catch {
                self.handle(error, context: "send failed")
            }
        }
    }

    /// Shared command-channel send: errors (e.g. not yet established) are
    /// surfaced in the chat log, like `sendChat` does for chat.
    func sendCommand(_ payload: Data) {
        queue.async { [weak self] in
            guard let self, !self.halted else { return }
            do {
                try self.session.sendRecord(channel: 0, payload: payload)
            } catch {
                self.handle(error, context: "input failed")
            }
        }
    }

    // MARK: - Remote input (POINTER/KEY on command channel 0)

    /// Send one POINTER command (normalized 0..=65535 in frame space).
    func sendPointer(kind: UInt8, x: UInt32, y: UInt32) {
        var payload = Data([0x30, kind])
        payload.append(contentsOf: withUnsafeBytes(of: x.bigEndian) { Data($0) })
        payload.append(contentsOf: withUnsafeBytes(of: y.bigEndian) { Data($0) })
        sendCommand(payload)
    }

    /// Send one KEY command (Windows virtual-key code, down flag).
    private func sendKeyCommand(vk: UInt16, down: Bool) {
        var payload = Data([0x31])
        payload.append(contentsOf: withUnsafeBytes(of: vk.bigEndian) { Data($0) })
        payload.append(down ? 1 : 0)
        sendCommand(payload)
    }

    /// A key-row tap: KEY down immediately followed by KEY up (two records).
    func sendKey(vk: UInt16) {
        sendKeyCommand(vk: vk, down: true)
        sendKeyCommand(vk: vk, down: false)
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

    /// Send-file (v0.27): read the picked document (security-scoped) and
    /// ship it through the Rust chunking path, both off main. Errors land
    /// in the chat log.
    func sendFile(url: URL) {
        queue.async { [weak self] in
            guard let self else { return }
            let data: Data
            do {
                let scoped = url.startAccessingSecurityScopedResource()
                defer { if scoped { url.stopAccessingSecurityScopedResource() } }
                data = try Data(contentsOf: url)
            } catch {
                self.log("file read failed: \(error)")
                return
            }
            guard !self.halted else { self.log("file send failed: not connected"); return }
            do {
                try self.session.sendFile(name: url.lastPathComponent, data: data)
                self.log("file sent: \(url.lastPathComponent) (\(data.count) bytes)")
            } catch {
                self.handle(error, context: "file send failed")
            }
        }
    }

    // MARK: Clipboard (main thread)

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

    /// Append a chat/status line (callable from any thread).
    private func log(_ text: String) {
        onMain { $0.appendLine(text) }
    }

    /// Main thread: append and cap the log at `maxLines`.
    private func appendLine(_ text: String) {
        nextLineID += 1
        lines.append(LogLine(id: nextLineID, text: text))
        if lines.count > Self.maxLines {
            lines.removeFirst(lines.count - Self.maxLines)
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
            model.image = frame.image
            if model.frameSize != frame.size { model.frameSize = frame.size }
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

    /// "A1B2C3D4 E5F60718 293A4B5C D6E7F809"-style display. Lives here, not
    /// on the View (View members are main-actor-isolated; this runs on the
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

// Audio (v0.21): remote mic streamed as raw s16le 48 kHz mono ticks;
// played back through AVAudioEngine + AVAudioPlayerNode. Driven from the
// session queue; only the queued-buffer counter is shared with the audio
// render thread (completion handlers), so it is lock-guarded.
final class RemoteAudioPlayer {
    private let engine = AVAudioEngine()
    private let player = AVAudioPlayerNode()
    private let format = AVAudioFormat(commonFormat: .pcmFormatFloat32,
                                       sampleRate: 48_000,
                                       channels: 1,
                                       interleaved: false)
    private var started = false
    /// Buffers scheduled but not yet consumed by the player.
    private var queued = 0
    private let queuedLock = NSLock()
    /// Max ticks (20 ms each) allowed in flight; beyond that new ticks are
    /// dropped so network bursts cannot ratchet the latency up.
    private static let maxQueued = 6

    /// Decode one audio tick (bare s16le 48 kHz mono samples — the sink
    /// delivers samples only, no header) and schedule it for playback.
    /// Starts the engine lazily on the first tick. Malformed ticks are
    /// dropped silently.
    func playTick(_ data: Data) {
        guard let format, data.count >= 2 else { return }
        let n = data.count / 2 // a stray odd trailing byte is ignored
        if !started {
            guard startEngine(format: format) else { return }
        }
        queuedLock.lock()
        let depth = queued
        queuedLock.unlock()
        guard depth < Self.maxQueued else { return }
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
        queuedLock.lock()
        queued += 1
        queuedLock.unlock()
        player.scheduleBuffer(buf, completionHandler: { [weak self] in
            guard let self else { return }
            self.queuedLock.lock()
            self.queued = max(0, self.queued - 1)
            self.queuedLock.unlock()
        })
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
            player.play()
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
                     format: UInt8, pixels: Data) -> CGImage? {
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
        if bpp == 4 {
            guard let p = CGDataProvider(data: pixels as CFData) else { return nil }
            provider = p
            outRowBytes = rowBytes
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
        }
        return CGImage(
            width: w, height: h,
            bitsPerComponent: 8, bitsPerPixel: 32, bytesPerRow: outRowBytes,
            space: CGColorSpaceCreateDeviceRGB(),
            bitmapInfo: info,
            provider: provider,
            decode: nil,
            shouldInterpolate: true,
            intent: .defaultIntent
        )
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
