import SwiftUI
import AVFoundation
import UniformTypeIdentifiers

// Established-session screen: peer fingerprint + chat over AEAD records on
// channel 1 (channel 0 carries the terminal command protocol, which also
// streams remote screen frames — reassembled by `OvScreenBox` and rendered).
// v0.11 adds remote input: POINTER/KEY commands sent on channel 0 (see
// `ovapp::terminal`), turning the screen view into remote control.

// Audio (v0.21): remote mic streamed as raw s16le 48 kHz mono ticks;
// played back through AVAudioEngine + AVAudioPlayerNode. A class so the
// escaping `onAudio` closure can hold a stable reference (a `@State var`
// cannot be captured mutably from an escaping closure).
private final class RemoteAudioPlayer {
    private let engine = AVAudioEngine()
    private let player = AVAudioPlayerNode()
    private let format = AVAudioFormat(commonFormat: .pcmFormatFloat32,
                                       sampleRate: 48_000,
                                       channels: 1,
                                       interleaved: false)
    private var started = false

    /// Decode one audio tick (6-byte header + s16le 48 kHz mono samples)
    /// and schedule it for playback. Starts the engine lazily on the first
    /// tick. Malformed ticks are dropped silently.
    func playTick(_ data: Data) {
        guard let format, data.count > 6 else { return }
        let n = data.count - 6
        guard n % 2 == 0 else { return }
        let samples = data.withUnsafeBytes { raw -> [Float] in
            let base = raw.bindMemory(to: Int16.self).baseAddress ?? UnsafePointer<Int16>(bitPattern: 1)!
            // Skip the 6-byte header (6 is even, so the sample data starts
            // at an even offset — Int16-aligned).
            var out = [Float](repeating: 0, count: n / 2)
            for i in 0..<out.count {
                let v = Int16(littleEndian: base[i + 3])
                out[i] = Float(v) / 32768.0
            }
            return out
        }
        guard let buf = AVAudioPCMBuffer(pcmFormat: format,
                                         frameCapacity: AVAudioFrameCount(samples.count)) else { return }
        buf.frameLength = AVAudioFrameCount(samples.count)
        samples.withUnsafeBufferPointer { src in
            buf.floatChannelData![0].update(from: src.baseAddress!, count: samples.count)
        }
        if !started {
            do {
                engine.attach(player)
                engine.connect(player, to: engine.mainMixerNode, format: format)
                try engine.start()
                player.play()
                started = true
            } catch {
                return
            }
        }
        player.scheduleBuffer(buf)
    }

    func stop() {
        player.stop()
        engine.stop()
        started = false
    }
}

// Clipboard sync (v0.26): mirrors `ovapp::clipboard::ClipboardSync` on the
// Rust side. The iOS shell does not run the Rust sync state, so it keeps
// its own echo guard: text just applied from the peer (or just sent) is
// never re-sent, breaking the apply -> poll -> send echo loop.
private final class ClipboardBridge {
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

    /// Poll the local pasteboard (call each pump tick). Fires `send` when a
    /// genuinely new local clipboard text appears.
    func pollLocal() {
        let pb = UIPasteboard.general
        guard pb.changeCount != lastChangeCount else { return }
        lastChangeCount = pb.changeCount
        guard let text = pb.string else { return }
        guard text != lastSeenLocal, text != lastRemote else { return }
        lastSeenLocal = text
        send?(text)
    }
}

// A file received from the peer (v0.27): already written to the app
// container; `url` opens it (Quick Look / share sheet).
struct ReceivedFile: Identifiable {
    let name: String
    let url: URL
    var id: String { name + "#" + url.lastPathComponent }
}

// Document picker (v0.27): wraps UIDocumentPickerViewController so the user
// can hand a file to send to the peer. `onPick` receives a security-scoped
// URL (caller reads + releases the scope); cancel calls `onCancel`.
struct DocumentPicker: UIViewControllerRepresentable {
    let onPick: (URL) -> Void
    let onCancel: () -> Void

    func makeCoordinator() -> Coordinator {
        Coordinator(onPick: onPick, onCancel: onCancel)
    }

    func makeUIViewController(context: Context) -> UIDocumentPickerViewController {
        let picker = UIDocumentPickerViewController(forOpeningContentTypes: [.data])
        picker.allowsMultipleSelection = false
        picker.delegate = context.coordinator
        return picker
    }

    func updateUIViewController(_ uiViewController: UIDocumentPickerViewController,
                                context: Context) {}

    final class Coordinator: NSObject, UIDocumentPickerDelegate {
        let onPick: (URL) -> Void
        let onCancel: () -> Void
        init(onPick: @escaping (URL) -> Void, onCancel: @escaping () -> Void) {
            self.onPick = onPick
            self.onCancel = onCancel
        }
        func documentPicker(_ controller: UIDocumentPickerViewController,
                            didPickDocumentsAt urls: [URL]) {
            guard let url = urls.first else { onCancel(); return }
            onPick(url)
        }
        func documentPickerWasCancelled(_ controller: UIDocumentPickerViewController) {
            onCancel()
        }
    }
}

struct SessionView: View {
    let session: OvSessionBox

    @State private var lines: [String] = []
    @State private var draft = ""
    @State private var peerPine: String = "connecting…"
    @State private var pumpTimer: Timer?
    @State private var udpBridge: NwUdpBridge?
    // Clipboard sync (v0.26): echo-guarded pasteboard bridge.
    @State private var clipboard = ClipboardBridge()
    // File transfer (v0.27): files received from the peer, written to the
    // app container; and whether the send-file document picker is up.
    @State private var receivedFiles: [ReceivedFile] = []
    @State private var showingFilePicker = false
    // The received file currently offered to the share sheet.
    @State private var shareFile: ReceivedFile?
    // Reassembled remote screen (nil until the first frame completes).
    @State private var screenImage: CGImage?
    // The last frame's geometry — the space pointer coordinates normalize to.
    @State private var frameSize: CGSize = .zero
    // Last pointer-move send time (throttle drags to ~30 Hz).
    @State private var lastMoveSend: Date = .distantPast
    // Whether the pointer is currently held down (touch began, not yet ended).
    @State private var pointerDown = false
    // Remote cursor overlay (v0.15): position in frame pixel space, and the
    // shape bitmap with its hotspot. nil shape = position-only (hidden).
    @State private var cursorPos: CGPoint? = nil
    @State private var cursorImage: CGImage? = nil
    @State private var cursorHotspot: CGSize = .zero
    @State private var cursorSize: CGSize = .zero
    // The Rust frame assembler lives for the view's lifetime.
    private let screen = OvScreenBox()
    // Liveness (v0.16): shell monotonic ms epoch for the keepalive ticks.
    private let pumpEpoch = Date()
    // Liveness HUD state, driven by the liveness sink.
    @State private var peerAlive = true
    @State private var rttMs: Int32 = -1
    @State private var fps: Float = 0
    // Quality (v0.17): the requested capture rate. index 0 = AUTO (the
    // host's RTT-aware governor decides); the rest are fixed fps targets.
    @State private var qualityIndex = 0
    private static let qualityChoices: [(label: String, fps: UInt16)] = [
        ("Auto", 0), ("10 fps", 10), ("30 fps", 30), ("60 fps", 60),
    ]
    // Monitors (v0.18): the host's monitor list from MONITORS_INFO; the
    // picker sends CAPTURE_REGION for the chosen monitor.
    @State private var monitors: [(x: Int32, y: Int32, w: UInt32, h: UInt32, primary: Bool)] = []
    @State private var monitorIndex = 0
    // Audio (v0.21): remote mic playback via AVAudioEngine (see above).
    private let audioPlayer = RemoteAudioPlayer()

    var body: some View {
        VStack(spacing: 0) {
            VStack(spacing: 4) {
                Text("Peer fingerprint")
                    .font(.caption).foregroundStyle(.secondary)
                Text(peerPine)
                    .font(.system(.footnote, design: .monospaced))
                    .textSelection(.enabled)
            }
            .padding(.vertical, 8)

            // Liveness status line (v0.16): RTT once measured, plus a
            // prominent banner when the peer goes silent past the window.
            HStack(spacing: 12) {
                Text(rttMs >= 0 ? "RTT \(rttMs) ms" : "RTT —")
                Text(String(format: "FPS %.1f", fps))
                Spacer()
                Circle()
                    .fill(peerAlive ? Color.green : Color.red)
                    .frame(width: 10, height: 10)
                Text(peerAlive ? "live" : "dead")
                    .font(.caption)
            }
            .font(.caption.monospacedDigit())
            .foregroundStyle(.secondary)
            .padding(.horizontal, 12)
            .padding(.bottom, 4)

            // Quality picker (v0.17): sends QUALITY_SET (0x36) with the
            // chosen target fps; 0 = AUTO (RTT-aware host governor).
            Picker("Quality", selection: $qualityIndex) {
                ForEach(0..<Self.qualityChoices.count, id: \.self) { i in
                    Text(Self.qualityChoices[i].label).tag(i)
                }
            }
            .pickerStyle(.segmented)
            .padding(.horizontal, 12)
            .padding(.bottom, 4)
            .onChange(of: qualityIndex) { newIndex in
                sendQuality(targetFps: Self.qualityChoices[newIndex].fps)
            }

            // Monitor picker (v0.18): shown once the host announces
            // monitors; sends CAPTURE_REGION (0x37) for the chosen one.
            if monitors.count > 1 {
                Picker("Monitor", selection: $monitorIndex) {
                    ForEach(monitors.indices, id: \.self) { i in
                        let m = monitors[i]
                        Text(m.primary
                            ? "MON \(i + 1) \(m.w)x\(m.h) (primary)"
                            : "MON \(i + 1) \(m.w)x\(m.h)").tag(i)
                    }
                }
                .pickerStyle(.menu)
                .padding(.horizontal, 12)
                .padding(.bottom, 4)
                .onChange(of: monitorIndex) { newIndex in
                    sendCaptureRegion(monitor: monitors[newIndex])
                }
            }

            // Remote display: rendered once a frame has reassembled. The
            // GeometryReader letterbox-maps touch points onto the frame.
            if let img = screenImage {
                GeometryReader { g in
                    let content = letterbox(g.size, frameSize)
                    ZStack {
                        Image(decorative: img, scale: 1)
                            .resizable()
                            .aspectRatio(contentMode: .fit)
                            .frame(width: content.size.width, height: content.size.height)
                            .position(x: content.origin.x + content.size.width / 2,
                                      y: content.origin.y + content.size.height / 2)
                            .gesture(dragGesture(in: g))
                        // Remote cursor overlay: anchored at the hotspot so
                        // the tip of the arrow sits on the logical position,
                        // scaled with the letterboxed frame. Hidden when no
                        // shape has been received yet.
                        if let cimg = cursorImage, let pos = cursorPos, frameSize.width > 0 {
                            let scale = content.size.width / frameSize.width
                            Image(decorative: cimg, scale: 1)
                                .resizable()
                                .interpolation(.none)
                                .frame(width: cursorSize.width * scale,
                                       height: cursorSize.height * scale)
                                // `position` centers the view at (x,y); offset by half the
                                // (scaled) image so the cursor HOTSPOT sits on the remote point.
                                .position(x: content.origin.x + (pos.x - cursorHotspot.width + cursorSize.width / 2) * scale,
                                          y: content.origin.y + (pos.y - cursorHotspot.height + cursorSize.height / 2) * scale)
                                .allowsHitTesting(false)
                        }
                    }
                }
                .frame(maxWidth: .infinity)
                .background(Color.black)
                .overlay {
                    if !peerAlive {
                        Text("Peer disconnected — waiting for traffic…")
                            .font(.headline)
                            .foregroundStyle(.white)
                            .padding(12)
                            .background(.red.opacity(0.75), in: RoundedRectangle(cornerRadius: 8))
                    }
                }
            } else {
                Text("Waiting for remote screen…")
                    .font(.caption).foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, minHeight: 120)
                    .background(Color.black.opacity(0.05))
            }

            Divider()

            ScrollView {
                VStack(alignment: .leading, spacing: 6) {
                    ForEach(Array(lines.enumerated()), id: \.offset) { _, line in
                        Text(line)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .padding(.horizontal, 12)
                    }
                }
            }

            Divider()

            // Remote-control key row: each tap sends KEY down+up on channel 0.
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(spacing: 8) {
                    ForEach(Self.keyButtons, id: \.vk) { key in
                        Button(key.label) { sendKey(key.vk) }
                            .buttonStyle(.bordered)
                    }
                }
                .padding(.horizontal, 12)
            }
            .padding(.vertical, 4)

            Divider()

            HStack {
                TextField("Message", text: $draft)
                    .onSubmit(send)
                Button("Send", action: send)
                    .disabled(draft.isEmpty)
                // Send file (v0.27): pick a document and ship it over the
                // file-id-keyed transfer path.
                Button("Send file…") { showingFilePicker = true }
            }
            .padding(8)

            // Received files (v0.27): tap to open the share sheet.
            if !receivedFiles.isEmpty {
                Divider()
                List(receivedFiles) { file in
                    Button {
                        shareFile = file
                    } label: {
                        VStack(alignment: .leading, spacing: 2) {
                            Text(file.name).font(.footnote)
                            Text(file.url.lastPathComponent)
                                .font(.caption2).foregroundStyle(.secondary)
                        }
                    }
                }
                .listStyle(.plain)
                .frame(maxHeight: 120)
            }
        }
        .navigationTitle("Session")
        .navigationBarTitleDisplayMode(.inline)
        .onAppear(perform: start)
        .onDisappear(perform: stop)
        .sheet(isPresented: $showingFilePicker) {
            DocumentPicker(
                onPick: { url in
                    showingFilePicker = false
                    sendPickedFile(url: url)
                },
                onCancel: { showingFilePicker = false }
            )
        }
        .sheet(item: $shareFile) { file in
            ShareSheet(items: [file.url])
        }
    }

    private func start() {
        screen.onFrame = { _, width, height, _, format, pixels in
            frameSize = CGSize(width: CGFloat(width), height: CGFloat(height))
            screenImage = Self.makeImage(width: width, height: height, format: format, pixels: pixels)
        }
        // Remote cursor callbacks (both fire on the pump/pump caller thread,
        // i.e. the main thread here — direct @State updates are safe).
        screen.onCursor = { x, y in
            cursorPos = CGPoint(x: CGFloat(x), y: CGFloat(y))
        }
        screen.onCursorShape = { hx, hy, w, h, bgra in
            cursorHotspot = CGSize(width: CGFloat(hx), height: CGFloat(hy))
            cursorSize = CGSize(width: CGFloat(w), height: CGFloat(h))
            cursorImage = Self.makeCursorImage(w: w, h: h, bgra: bgra)
        }
        screen.onLiveness = { alive, rtt, fpsNow in
            peerAlive = alive == 1
            rttMs = rtt
            fps = fpsNow
        }
        screen.onMonitors = { list in
            monitors = list.map { (x: $0.0, y: $0.1, w: $0.2, h: $0.3, primary: $0.4) }
            monitorIndex = 0
        }
        // Remote audio (v0.21): each tick is 960 s16le samples; convert to
        // Float32 and schedule on the player node.
        screen.onAudio = { [weak audioPlayer] data in
            audioPlayer?.playTick(data)
        }
        // Clipboard sync (v0.26): apply peer text to the pasteboard and
        // arm the echo guard so we never re-send what we just applied.
        screen.onClipboard = { [weak clipboard] kind, payload in
            clipboard?.noteRemote(kind: kind, payload: payload)
        }
        // File transfer (v0.27): a completed peer transfer lands here (name
        // + bytes); write it into the app container and surface it in the
        // received-files list. Fires on the pump thread (the main thread).
        screen.onFile = { name, data in
            let dir = FileManager.default.urls(for: .documentDirectory,
                                               in: .userDomainMask).first
                ?? FileManager.default.temporaryDirectory
            let url = dir.appendingPathComponent(name)
            do {
                try data.write(to: url, options: .atomic)
                receivedFiles.append(ReceivedFile(name: name, url: url))
                lines.append("file received: \(name) (\(data.count) bytes)")
            } catch {
                lines.append("file write failed: \(error)")
            }
        }
        clipboard.send = { [weak session] text in
            guard let session else { return }
            var payload = Data([0x50, 0]) // CLIPBOARD, kind 0 = text
            payload.append(contentsOf: Array(text.utf8))
            try? session.sendRecord(channel: 0, payload: payload)
        }
        session.onEstablished = { fp in
            peerPine = Self.pineString(fp)
        }
        session.onRecord = { channel, payload in
            switch channel {
            case 0:
                // Terminal command channel: screen frames reassemble here.
                screen.feed(payload)
            case 1:
                if let text = String(data: payload, encoding: .utf8) {
                    lines.append("peer: \(text)")
                }
            default:
                break
            }
        }
        session.onDatagramOut = { [weak session] data in
            // Direct mode: forward to the UDP bridge (called on the main
            // thread from pump()). Relay mode: the Rust-owned relay carrier
            // sends from inside pumpRelay(), so nothing to do here.
            udpBridge?.send(data)
            _ = session
        }
        do {
            try session.pump()
            if let fp = session.peerFingerprint() {
                peerPine = Self.pineString(fp)
            }
        } catch {
            lines.append("error: \(error)")
        }
        // Direct/LAN mode: dial the peer over UDP. Inbound datagrams arrive
        // on the main queue via the bridge and are fed to the session here.
        if case .direct(let host, let port) = (DirectTransport.pending ?? .relay(host: "", port: 0)) {
            DirectTransport.pending = nil
            let bridge = NwUdpBridge(host: host, port: port,
                                     onReceive: { [weak session] data in
                guard let session else { return }
                do {
                    try session.onDatagram(data)
                    try session.pump() // process immediately; sinks fire here
                } catch {
                    lines.append("error: \(error)")
                }
            }, onState: { state in
                if case .failed(let error) = state {
                    lines.append("udp: \(error)")
                }
            })
            udpBridge = bridge
        }
        // Poll the Rust core. Relay mode also drives the Rust-owned relay
        // transport; direct mode's inbound arrives via the bridge instead.
        let isDirect = udpBridge != nil
        pumpTimer = Timer.scheduledTimer(withTimeInterval: 0.05, repeats: true) { _ in
            do {
                try session.pump()
                if !isDirect {
                    try session.pumpRelay()
                }
                // Liveness (v0.16): send a keepalive PING when the Rust
                // keepalive says one is due, answer the peer's PINGs, and
                // run the liveness verdict (fires the sink -> @State).
                let nowMs = Int64(Date().timeIntervalSince(pumpEpoch) * 1000)
                let seq = screen.keepaliveTick(nowMs: nowMs)
                if seq > 0 {
                    var ping = Data([0x10])
                    ping.append(contentsOf: withUnsafeBytes(of: seq.bigEndian) { Data($0) })
                    try session.sendRecord(channel: 0, payload: ping)
                }
                if let pong = screen.takePongReply() {
                    try session.sendRecord(channel: 0, payload: pong)
                }
                screen.livenessTick(nowMs: nowMs)
                // Clipboard sync (v0.26): poll the local pasteboard on
                // each tick; a new local text goes out as one CLIPBOARD.
                clipboard.pollLocal()
            } catch {
                lines.append("error: \(error)")
            }
        }
    }

    private func stop() {
        pumpTimer?.invalidate()
        pumpTimer = nil
        udpBridge?.close()
        udpBridge = nil
        audioPlayer.stop()
    }

    private func send() {
        let text = draft
        draft = ""
        do {
            try session.sendRecord(channel: 1, payload: Data(text.utf8))
            lines.append("me: \(text)")
        } catch {
            lines.append("send failed: \(error)")
        }
    }

    // MARK: - Remote input (POINTER/KEY on command channel 0)

    /// The key row: label + Windows virtual-key code.
    private static let keyButtons: [(label: String, vk: UInt16)] = [
        ("⌫", 0x08), ("↵", 0x0D), ("esc", 0x1B), ("tab", 0x09),
        ("←", 0x25), ("↑", 0x26), ("→", 0x27), ("↓", 0x28),
        ("ctrl", 0x11), ("alt", 0x12),
    ]

    /// Send one POINTER command (normalized 0..=65535 in frame space).
    private func sendPointer(kind: UInt8, x: UInt32, y: UInt32) {
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
    private func sendKey(vk: UInt16) {
        sendKeyCommand(vk: vk, down: true)
        sendKeyCommand(vk: vk, down: false)
    }

    /// Send one CAPTURE_REGION command (v0.18): i32-BE x, i32-BE y,
    /// u32-BE w, u32-BE h — the monitor rect in virtual-desktop coords.
    private func sendCaptureRegion(monitor m: (x: Int32, y: Int32, w: UInt32, h: UInt32, primary: Bool)) {
        var payload = Data([0x37])
        payload.append(contentsOf: withUnsafeBytes(of: m.x.bigEndian) { Data($0) })
        payload.append(contentsOf: withUnsafeBytes(of: m.y.bigEndian) { Data($0) })
        payload.append(contentsOf: withUnsafeBytes(of: m.w.bigEndian) { Data($0) })
        payload.append(contentsOf: withUnsafeBytes(of: m.h.bigEndian) { Data($0) })
        sendCommand(payload)
    }

    /// Send one QUALITY_SET command (v0.17): 2-byte u16-BE target fps,
    /// 0 = AUTO (the host's RTT-aware governor picks the rate).
    private func sendQuality(targetFps: UInt16) {
        var payload = Data([0x36])
        payload.append(contentsOf: withUnsafeBytes(of: targetFps.bigEndian) { Data($0) })
        sendCommand(payload)
    }

    /// Send-file (v0.27): read the picked document (security-scoped) and
    /// ship it through the Rust chunking path. Errors land in the chat log.
    private func sendPickedFile(url: URL) {
        let data: Data
        do {
            let scoped = url.startAccessingSecurityScopedResource()
            defer { if scoped { url.stopAccessingSecurityScopedResource() } }
            data = try Data(contentsOf: url)
        } catch {
            lines.append("file read failed: \(error)")
            return
        }
        do {
            try session.sendFile(name: url.lastPathComponent, data: data)
            lines.append("file sent: \(url.lastPathComponent) (\(data.count) bytes)")
        } catch {
            lines.append("file send failed: \(error)")
        }
    }

    /// Share sheet wrapper for opening a received file.
    struct ShareSheet: UIViewControllerRepresentable {
        let items: [Any]
        func makeUIViewController(context: Context) -> UIActivityViewController {
            UIActivityViewController(activityItems: items, applicationActivities: nil)
        }
        func updateUIViewController(_ uiViewController: UIActivityViewController,
                                    context: Context) {}
    }

    /// Shared command-channel send: catches the not-yet-established error and
    /// surfaces it in the chat log, like `send()` does for chat.
    private func sendCommand(_ payload: Data) {
        do {
            try session.sendRecord(channel: 0, payload: payload)
        } catch {
            lines.append("input failed: \(error)")
        }
    }

    /// Letterbox geometry for a frame of `frame` aspect inside `bounds`:
    /// the centered content rect (origin + size in view coordinates).
    private func letterbox(_ bounds: CGSize, _ frame: CGSize) -> (origin: CGPoint, size: CGSize) {
        guard frame.width > 0, frame.height > 0 else {
            return (CGPoint.zero, bounds)
        }
        let contentW = min(bounds.width, bounds.height * frame.width / frame.height)
        let contentH = contentW * frame.height / frame.width
        return (
            CGPoint(x: (bounds.width - contentW) / 2, y: (bounds.height - contentH) / 2),
            CGSize(width: contentW, height: contentH)
        )
    }

    /// Map a touch point to normalized frame coordinates (0..=65535), or nil
    /// when the touch falls outside the letterboxed content rect.
    private func normalizedPoint(_ p: CGPoint, in g: GeometryProxy) -> CGPoint? {
        let content = letterbox(g.size, frameSize)
        let px = p.x - content.origin.x
        let py = p.y - content.origin.y
        guard px >= 0, px <= content.size.width,
              py >= 0, py <= content.size.height else { return nil }
        let xn = px / content.size.width * 65_535
        let yn = py / content.size.height * 65_535
        return CGPoint(x: min(max(xn, 0), 65_535), y: min(max(yn, 0), 65_535))
    }

    /// Drag (tap) gesture over the screen image: began = pointer down,
    /// changed = pointer move (throttled ~30 Hz), ended = pointer up.
    private func dragGesture(in g: GeometryProxy) -> some Gesture {
        DragGesture(minimumDistance: 0)
            .onChanged { value in
                guard frameSize.width > 0,
                      let p = normalizedPoint(value.location, in: g) else { return }
                if !pointerDown {
                    // DragGesture has no onBegan; the first onChanged *is*
                    // the touch-began event — send the button-down there.
                    pointerDown = true
                    lastMoveSend = Date()
                    sendPointer(kind: 1, x: UInt32(p.x.rounded()), y: UInt32(p.y.rounded()))
                    return
                }
                let now = Date()
                guard now.timeIntervalSince(lastMoveSend) >= 1.0 / 30.0 else { return }
                lastMoveSend = now
                sendPointer(kind: 0, x: UInt32(p.x.rounded()), y: UInt32(p.y.rounded()))
            }
            .onEnded { value in
                defer { pointerDown = false }
                guard frameSize.width > 0 else { return }
                // Up lands where the drag ended (the same point on a tap).
                if let up = normalizedPoint(value.location, in: g) {
                    sendPointer(kind: 2, x: UInt32(up.x.rounded()), y: UInt32(up.y.rounded()))
                }
            }
    }

    /// "A1B2C3D4 E5F60718 293A4B5C D6E7F809"-style display.
    static func pineString(_ fp: [UInt8]) -> String {
        let hex = fp.map { String(format: "%02X", $0) }.joined()
        return stride(from: 0, to: hex.count, by: 8).map {
            String(hex.dropFirst($0).prefix(8))
        }.joined(separator: " ")
    }

    /// Build the cursor overlay image from a premultiplied-BGRA `CURSOR_SHAPE`
    /// bitmap. Unlike `makeImage` the alpha channel is kept (that is the whole
    /// point of a cursor sprite); the layout is the same BGRA32-little mapping.
    static func makeCursorImage(w: UInt16, h: UInt16, bgra: Data) -> CGImage? {
        let width = Int(w), height = Int(h)
        guard width > 0, height > 0, bgra.count >= width * height * 4 else { return nil }
        let info = CGBitmapInfo(rawValue: CGImageAlphaInfo.premultipliedFirst.rawValue
            | CGBitmapInfo.byteOrder32Little.rawValue)
        guard let provider = CGDataProvider(data: bgra) else { return nil }
        return CGImage(
            width: width, height: height,
            bitsPerComponent: 8, bitsPerPixel: 32, bytesPerRow: width * 4,
            provider: provider, info: info,
            decode: nil, shouldInterpolate: true, intent: .defaultIntent,
        )
    }

    /// Build a displayable `CGImage` from a reassembled frame.
    ///
    /// Normalizes both pixel layouts to a 32-bit BGRA buffer (B,G,R,A in
    /// memory) and wraps it with a `premultipliedFirst` + `32Little` CGImage,
    /// which is the native iOS screen layout. BGR (3-byte) frames get an
    /// opaque alpha byte appended.
    static func makeImage(width: UInt32, height: UInt32, format: UInt8, pixels: Data) -> CGImage? {
        let w = Int(width), h = Int(height)
        guard w > 0, h > 0 else { return nil }
        let buf = [UInt8](repeating: 0, count: w * h * 4)
        switch format {
        case 0: // BGRA, 4 bytes/px. The 4th byte is GDI's unused high byte (0 for
            // real screen capture), not an alpha channel — force it opaque so the
            // premultipliedFirst CGImage below renders correctly instead of all-
            // transparent. (Synthetic frames already carry 255, so this is a no-op
            // for them.)
            let src = [UInt8](pixels)
            let n = min(src.count / 4, w * h)
            for i in 0..<n {
                let s = i * 4, d = i * 4
                buf[d] = src[s]; buf[d + 1] = src[s + 1]; buf[d + 2] = src[s + 2]; buf[d + 3] = 255
            }
        case 1: // BGR, 3 bytes/px — pad an opaque alpha byte.
            let n = min(pixels.count / 3, w * h)
            let src = [UInt8](pixels)
            for i in 0..<n {
                let s = i * 3, d = i * 4
                buf[d] = src[s]; buf[d + 1] = src[s + 1]; buf[d + 2] = src[s + 2]; buf[d + 3] = 255
            }
        default:
            return nil
        }
        let info = CGBitmapInfo(rawValue: CGImageAlphaInfo.premultipliedFirst.rawValue
            | CGBitmapInfo.byteOrder32Little.rawValue)
        guard let provider = CGDataProvider(data: Data(buf)) else { return nil }
        return CGImage(
            width: w, height: h,
            bitsPerComponent: 8, bitsPerPixel: 32, bytesPerRow: w * 4,
            provider: provider, info: info,
            decode: nil, shouldInterpolate: true, intent: .defaultIntent,
        )
    }
}
