import SwiftUI
import UniformTypeIdentifiers

// Established-session screen: peer fingerprint + chat over AEAD records on
// channel 1 (channel 0 carries the terminal command protocol, which also
// streams remote screen frames — reassembled by `OvScreenBox` and rendered).
// v0.11 adds remote input: POINTER/KEY commands sent on channel 0 (see
// `ovapp::terminal`), turning the screen view into remote control.
//
// All session work lives in `SessionController` (SessionController.swift),
// owned here via @StateObject so it survives parent re-renders and is
// released (closing the transports and freeing the Rust session) when the
// view goes away. The remote screen and the liveness HUD are separate small
// views observing their own models, so frame / cursor / fps updates never
// re-evaluate this body.

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
    @StateObject private var controller: SessionController

    @State private var draft = ""
    // File transfer (v0.27): whether the send-file document picker is up.
    @State private var showingFilePicker = false
    // The received file currently offered to the share sheet.
    @State private var shareFile: ReceivedFile?
    // Quality (v0.17): the requested capture rate. index 0 = AUTO (the
    // host's RTT-aware governor decides); the rest are fixed fps targets.
    @State private var qualityIndex = 0
    private static let qualityChoices: [(label: String, fps: UInt16)] = [
        ("Auto", 0), ("10 fps", 10), ("30 fps", 30), ("60 fps", 60),
    ]

    init(session: OvSessionBox) {
        // The autoclosure runs once per view identity; later re-renders of
        // the parent reuse the same controller.
        _controller = StateObject(wrappedValue: SessionController(session: session))
    }

    var body: some View {
        VStack(spacing: 0) {
            VStack(spacing: 4) {
                Text("Peer fingerprint")
                    .font(.caption).foregroundStyle(.secondary)
                Text(controller.peerPine)
                    .font(.system(.footnote, design: .monospaced))
                    .textSelection(.enabled)
            }
            .padding(.vertical, 8)

            // Terminal session state (wrong PIN, relay closed, …): one clear
            // line instead of an error per pump tick.
            if let status = controller.status {
                Text(status)
                    .font(.footnote.weight(.semibold))
                    .foregroundStyle(.white)
                    .frame(maxWidth: .infinity)
                    .padding(8)
                    .background(Color.red.opacity(0.85))
            }

            // Liveness status line (v0.16): RTT once measured, plus a
            // prominent banner when the peer goes silent past the window.
            LivenessHUD(model: controller.livenessModel)

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
                controller.sendQuality(targetFps: Self.qualityChoices[newIndex].fps)
            }

            // Monitor picker (v0.18): shown once the host announces
            // monitors; sends CAPTURE_REGION (0x37) for the chosen one.
            if controller.monitors.count > 1 {
                Picker("Monitor", selection: $controller.monitorIndex) {
                    ForEach(controller.monitors.indices, id: \.self) { i in
                        let m = controller.monitors[i]
                        Text(m.primary
                            ? "MON \(i + 1) \(m.w)x\(m.h) (primary)"
                            : "MON \(i + 1) \(m.w)x\(m.h)").tag(i)
                    }
                }
                .pickerStyle(.menu)
                .padding(.horizontal, 12)
                .padding(.bottom, 4)
                .onChange(of: controller.monitorIndex) { newIndex in
                    controller.sendCaptureRegion(monitorAt: newIndex)
                }
            }

            // Remote display: its own view observing ScreenModel only.
            RemoteScreenView(model: controller.screenModel,
                             liveness: controller.livenessModel,
                             controller: controller)

            Divider()

            // Chat / status log: capped in the controller, lazily rendered.
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 6) {
                    ForEach(controller.lines) { line in
                        Text(line.text)
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
                        Button(key.label) { controller.sendKey(vk: key.vk) }
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
            if !controller.receivedFiles.isEmpty {
                Divider()
                List(controller.receivedFiles) { file in
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
        .onAppear { controller.start() }
        .onDisappear { controller.stop() }
        .sheet(isPresented: $showingFilePicker) {
            DocumentPicker(
                onPick: { url in
                    showingFilePicker = false
                    controller.sendFile(url: url)
                },
                onCancel: { showingFilePicker = false }
            )
        }
        .sheet(item: $shareFile) { file in
            ShareSheet(items: [file.url])
        }
    }

    private func send() {
        let text = draft
        guard !text.isEmpty else { return }
        draft = ""
        controller.sendChat(text)
    }

    /// The key row: label + Windows virtual-key code.
    private static let keyButtons: [(label: String, vk: UInt16)] = [
        ("⌫", 0x08), ("↵", 0x0D), ("esc", 0x1B), ("tab", 0x09),
        ("←", 0x25), ("↑", 0x26), ("→", 0x27), ("↓", 0x28),
        ("ctrl", 0x11), ("alt", 0x12),
    ]

    /// Share sheet wrapper for opening a received file.
    struct ShareSheet: UIViewControllerRepresentable {
        let items: [Any]
        func makeUIViewController(context: Context) -> UIActivityViewController {
            UIActivityViewController(activityItems: items, applicationActivities: nil)
        }
        func updateUIViewController(_ uiViewController: UIActivityViewController,
                                    context: Context) {}
    }
}

// MARK: - Liveness HUD

/// RTT / FPS / live dot (v0.16). Observes only LivenessModel, which the
/// controller updates only when a visible value changes.
private struct LivenessHUD: View {
    @ObservedObject var model: LivenessModel

    var body: some View {
        HStack(spacing: 12) {
            Text(model.rttMs >= 0 ? "RTT \(model.rttMs) ms" : "RTT —")
            Text(String(format: "FPS %.1f", Double(model.fps)))
            Spacer()
            Circle()
                .fill(model.peerAlive ? Color.green : Color.red)
                .frame(width: 10, height: 10)
            Text(model.peerAlive ? "live" : "dead")
                .font(.caption)
        }
        .font(.caption.monospacedDigit())
        .foregroundStyle(.secondary)
        .padding(.horizontal, 12)
        .padding(.bottom, 4)
    }
}

/// "Peer disconnected" overlay for the screen, driven by LivenessModel.
private struct DeadPeerBanner: View {
    @ObservedObject var model: LivenessModel

    var body: some View {
        if !model.peerAlive {
            Text("Peer disconnected — waiting for traffic…")
                .font(.headline)
                .foregroundStyle(.white)
                .padding(12)
                .background(.red.opacity(0.75), in: RoundedRectangle(cornerRadius: 8))
        }
    }
}

// MARK: - Remote screen

/// The remote display + cursor overlay + touch-to-pointer input. Rendered
/// once a frame has reassembled; the GeometryReader letterbox-maps touch
/// points onto the frame.
private struct RemoteScreenView: View {
    @ObservedObject var model: ScreenModel
    let liveness: LivenessModel
    let controller: SessionController

    // Last pointer-move send time (throttle drags to ~30 Hz).
    @State private var lastMoveSend: Date = .distantPast
    // Whether the pointer is currently held down (touch began, not yet ended).
    @State private var pointerDown = false

    var body: some View {
        if let img = model.image {
            GeometryReader { g in
                let content = letterbox(g.size, model.frameSize)
                ZStack {
                    // Layer-backed frame: setting CALayer.contents is the
                    // cheapest way to show a fresh CGImage each frame.
                    FrameLayerView(image: img)
                        .frame(width: content.size.width, height: content.size.height)
                        .position(x: content.origin.x + content.size.width / 2,
                                  y: content.origin.y + content.size.height / 2)
                        .allowsHitTesting(false)
                    // Remote cursor overlay: anchored at the hotspot so the
                    // tip of the arrow sits on the logical position, scaled
                    // with the letterboxed frame. Hidden when no shape has
                    // been received yet.
                    if let cimg = model.cursorImage, let pos = model.cursorPos, model.frameSize.width > 0 {
                        let scale = content.size.width / model.frameSize.width
                        Image(decorative: cimg, scale: 1)
                            .resizable()
                            .interpolation(.none)
                            .frame(width: model.cursorSize.width * scale,
                                   height: model.cursorSize.height * scale)
                            // `position` centers the view at (x,y); offset by half the
                            // (scaled) image so the cursor HOTSPOT sits on the remote point.
                            .position(x: content.origin.x + (pos.x - model.cursorHotspot.width + model.cursorSize.width / 2) * scale,
                                      y: content.origin.y + (pos.y - model.cursorHotspot.height + model.cursorSize.height / 2) * scale)
                            .allowsHitTesting(false)
                    }
                    // Touch surface covering the whole area, so gesture
                    // locations are in GeometryReader coordinates.
                    Color.clear
                        .contentShape(Rectangle())
                        .gesture(dragGesture(in: g))
                }
            }
            .frame(maxWidth: .infinity)
            .background(Color.black)
            .overlay(DeadPeerBanner(model: liveness))
        } else {
            Text("Waiting for remote screen…")
                .font(.caption).foregroundStyle(.secondary)
                .frame(maxWidth: .infinity, minHeight: 120)
                .background(Color.black.opacity(0.05))
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
        let content = letterbox(g.size, model.frameSize)
        guard content.size.width > 0, content.size.height > 0 else { return nil }
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
        DragGesture(minimumDistance: 0, coordinateSpace: .local)
            .onChanged { value in
                guard model.frameSize.width > 0,
                      let p = normalizedPoint(value.location, in: g) else { return }
                if !pointerDown {
                    // DragGesture has no onBegan; the first onChanged *is*
                    // the touch-began event — send the button-down there.
                    pointerDown = true
                    lastMoveSend = Date()
                    controller.sendPointer(kind: 1, x: UInt32(p.x.rounded()), y: UInt32(p.y.rounded()))
                    return
                }
                let now = Date()
                guard now.timeIntervalSince(lastMoveSend) >= 1.0 / 30.0 else { return }
                lastMoveSend = now
                controller.sendPointer(kind: 0, x: UInt32(p.x.rounded()), y: UInt32(p.y.rounded()))
            }
            .onEnded { value in
                defer { pointerDown = false }
                guard pointerDown, model.frameSize.width > 0 else { return }
                // Up lands where the drag ended (the same point on a tap).
                if let up = normalizedPoint(value.location, in: g) {
                    controller.sendPointer(kind: 2, x: UInt32(up.x.rounded()), y: UInt32(up.y.rounded()))
                }
            }
    }
}

/// Shows a CGImage through a plain CALayer (`contents`), without implicit
/// animations. The image is already letterbox-sized by the parent.
private struct FrameLayerView: UIViewRepresentable {
    let image: CGImage

    func makeUIView(context: Context) -> FrameLayerUIView {
        FrameLayerUIView()
    }

    func updateUIView(_ uiView: FrameLayerUIView, context: Context) {
        uiView.setImage(image)
    }
}

private final class FrameLayerUIView: UIView {
    // Strong on purpose (CF types do not support weak references); the
    // layer holds the same image anyway.
    private var current: CGImage?

    override init(frame: CGRect) {
        super.init(frame: frame)
        isUserInteractionEnabled = false
        backgroundColor = .black
        layer.contentsGravity = .resize
        layer.magnificationFilter = .linear
        layer.minificationFilter = .trilinear
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    func setImage(_ image: CGImage) {
        // Unrelated re-renders hand the same image back: skip those.
        guard image !== current else { return }
        current = image
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        layer.contents = image
        CATransaction.commit()
    }
}
