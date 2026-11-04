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
    // Section 2: Mouse (trackpad) / Touch (direct) input mode, shared with
    // RemoteInputController through the same UserDefaults key.
    @AppStorage(RemoteInputController.modeDefaultsKey) private var inputModeRaw: String = InputMode.mouse.rawValue
    @Environment(\.scenePhase) private var scenePhase

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

            // Remote-control row (section 2): input mode + keyboard buttons.
            // Special keys live in the keyboard accessory bar.
            HStack(spacing: 8) {
                Picker("Input mode", selection: $inputModeRaw) {
                    Text("Mouse").tag(InputMode.mouse.rawValue)
                    Text("Touch").tag(InputMode.touch.rawValue)
                }
                .pickerStyle(.segmented)
                .frame(maxWidth: 180)
                Spacer()
                // Soft keyboard + special-keys bar (tap again to hide).
                Button {
                    controller.input.showKeyboard(soft: true)
                } label: {
                    Image(systemName: "keyboard")
                }
                .buttonStyle(.bordered)
                .accessibilityLabel("Keyboard")
                // Special-keys bar alone, without the soft keyboard.
                Button {
                    controller.input.showKeyboard(soft: false)
                } label: {
                    Image(systemName: "command")
                }
                .buttonStyle(.bordered)
                .accessibilityLabel("Special keys")
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 4)
            .onChange(of: inputModeRaw) { raw in
                controller.input.setMode(InputMode(rawValue: raw) ?? .mouse)
            }

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
        .onAppear {
            controller.start()
            controller.input.setMode(InputMode(rawValue: inputModeRaw) ?? .mouse)
        }
        .onDisappear { controller.stop() }
        // Section 2: nothing stays pressed on the host while we are away.
        .onChange(of: scenePhase) { phase in
            if phase == .background {
                controller.input.releaseAll()
            }
        }
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

/// The remote display. Until the first frame reassembles it shows a
/// placeholder; then `RemoteScreenSurface` (RemoteScreenSurface.swift)
/// renders the frame + H.264 overlay + local cursor inside a zoom/pan
/// viewport and turns gestures, keyboards and the iPad pointer into input
/// commands via `controller.input`.
private struct RemoteScreenView: View {
    @ObservedObject var model: ScreenModel
    let liveness: LivenessModel
    let controller: SessionController

    var body: some View {
        if let img = model.image {
            RemoteScreenSurface(image: img,
                                frameSize: model.frameSize,
                                video: model.video,
                                cursorPos: model.cursorPos,
                                cursorImage: model.cursorImage,
                                cursorHotspot: model.cursorHotspot,
                                cursorSize: model.cursorSize,
                                input: controller.input)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .background(Color.black)
                .overlay(DeadPeerBanner(model: liveness).allowsHitTesting(false))
        } else {
            Text("Waiting for remote screen…")
                .font(.caption).foregroundStyle(.secondary)
                .frame(maxWidth: .infinity, minHeight: 120)
                .background(Color.black.opacity(0.05))
        }
    }
}
