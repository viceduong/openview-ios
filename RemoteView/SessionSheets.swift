import SwiftUI
import UIKit
import UniformTypeIdentifiers

// Sheets over the immersive session: the session menu (TeamViewer's drawer
// equivalents), chat, files, session log, plus the UIKit document picker
// and share sheet wrappers.

/// Menu actions that need the session container (a different sheet, the
/// keyboard, or closing the session).
enum SessionMenuAction {
    case disconnect
    case sendFile
    case showFiles
    case gestureHelp
    case showKeyBar
}

/// Medium + large detents where available (iOS 16+); full sheet on 15.
struct MediumLargeDetents: ViewModifier {
    @ViewBuilder
    func body(content: Content) -> some View {
        if #available(iOS 16.0, *) {
            content.presentationDetents([.medium, .large])
        } else {
            content
        }
    }
}

// MARK: - Session menu

struct SessionMenuSheet: View {
    @ObservedObject var controller: SessionController
    @ObservedObject var input: RemoteInputController
    let onAction: (SessionMenuAction) -> Void

    @Environment(\.dismiss) private var dismiss
    @AppStorage(SettingsKey.lockLocalZoom) private var lockLocalZoom = false
    @AppStorage(SettingsKey.hideWallpaper) private var hideWallpaper = true
    @AppStorage(SettingsKey.disableAnimations) private var disableAnimations = false

    var body: some View {
        NavigationView {
            List {
                quickActions
                shortcuts
                interaction
                display
                quality
                transfer
                sessionInfo
            }
            .listStyle(.insetGrouped)
            .navigationTitle(controller.request.displayName)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") { dismiss() }
                }
            }
        }
        .navigationViewStyle(.stack)
        .modifier(MediumLargeDetents())
    }

    // MARK: Sections

    private var quickActions: some View {
        Section(header: Text("Quick Actions")) {
            sendRow("Send Ctrl+Alt+Del", symbol: "command") {
                input.sendSAS()
            }
            sendRow("Lock (Win+L)", symbol: "lock") {
                input.sendCombo([.leftWin, .keyL])
            }
            sendRow("Task Manager (Ctrl+Shift+Esc)", symbol: "list.bullet.rectangle") {
                input.sendCombo([.leftCtrl, .leftShift, .escape])
            }
            sendRow("Send clipboard text", symbol: "doc.on.clipboard") {
                controller.sendClipboardNow()
            }
            sendRow("Type clipboard text", symbol: "keyboard") {
                controller.typeClipboardText()
            }
            Button(role: .destructive) {
                onAction(.disconnect)
            } label: {
                Label("Disconnect", systemImage: "xmark.circle")
            }
        }
    }

    private var shortcuts: some View {
        Section(header: Text("Keyboard Shortcuts (Windows)")) {
            shortcutRow("Copy", keys: "Ctrl+C", [.leftCtrl, .keyC])
            shortcutRow("Paste", keys: "Ctrl+V", [.leftCtrl, .keyV])
            shortcutRow("Save", keys: "Ctrl+S", [.leftCtrl, .keyS])
            shortcutRow("Print Screen", keys: "Win+PrtScn", [.leftWin, .printScreen])
            shortcutRow("Switch apps", keys: "Alt+Tab", [.leftAlt, .tab])
            shortcutRow("Task view", keys: "Win+Tab", [.leftWin, .tab])
            shortcutRow("Show desktop", keys: "Win+D", [.leftWin, .keyD])
            shortcutRow("Run", keys: "Win+R", [.leftWin, .keyR])
            Button {
                onAction(.showKeyBar)
            } label: {
                Label("Show special keys", systemImage: "keyboard")
            }
        }
    }

    private var interaction: some View {
        Section(header: Text("Interaction Method")) {
            Picker("Mode", selection: modeBinding) {
                Text("Mouse").tag(InputMode.mouse)
                Text("Touch").tag(InputMode.touch)
            }
            .pickerStyle(.segmented)
            ForEach(GestureHelp.items(for: input.mode)) { item in
                GestureHelpRow(item: item)
            }
        }
    }

    private var display: some View {
        Section(header: Text("Display")) {
            if controller.monitors.count > 1 {
                Picker("Monitor", selection: monitorBinding) {
                    ForEach(controller.monitors.indices, id: \.self) { i in
                        Text(monitorTitle(i)).tag(i)
                    }
                }
            }
            zoomRow("Fit to screen", symbol: "arrow.down.right.and.arrow.up.left", preset: .fit)
            zoomRow("Fill screen", symbol: "arrow.up.left.and.arrow.down.right", preset: .fill)
            zoomRow("Actual size (1:1)", symbol: "1.magnifyingglass", preset: .native)
            Toggle(isOn: $lockLocalZoom) {
                VStack(alignment: .leading, spacing: 2) {
                    Text("Lock local zoom")
                    Text("All gestures, pinch included, go to the remote computer.")
                        .font(.caption)
                        .foregroundColor(.secondary)
                }
            }
            // Section 4: only hosts announcing CAP_DESKTOP_FX can do these.
            if controller.hostSupportsDesktopFx {
                Toggle("Hide remote wallpaper", isOn: $hideWallpaper)
                    .onChange(of: hideWallpaper) { on in
                        controller.setDesktopOption(key: 1, on: on)
                    }
                Toggle(isOn: $disableAnimations) {
                    VStack(alignment: .leading, spacing: 2) {
                        Text("Disable animations")
                        Text("Turns off window animations on the host while you are connected.")
                            .font(.caption)
                            .foregroundColor(.secondary)
                    }
                }
                .onChange(of: disableAnimations) { on in
                    controller.setDesktopOption(key: 2, on: on)
                }
            }
        }
    }

    private var quality: some View {
        Section(header: Text("Quality"),
                footer: Text("Auto lets the host adapt the frame rate to the connection.")) {
            Picker("Frame rate", selection: fpsBinding) {
                ForEach(QualityChoice.all) { choice in
                    Text(choice.label).tag(choice.fps)
                }
            }
            .pickerStyle(.segmented)
        }
    }

    private var transfer: some View {
        Section(header: Text("Transfer")) {
            Button {
                onAction(.sendFile)
            } label: {
                Label("Send file…", systemImage: "square.and.arrow.up")
            }
            Button {
                onAction(.showFiles)
            } label: {
                HStack {
                    Label("Received files", systemImage: "tray.and.arrow.down")
                    Spacer()
                    Text("\(controller.receivedFiles.count)")
                        .foregroundColor(.secondary)
                    Image(systemName: "chevron.right")
                        .font(.footnote.weight(.semibold))
                        .foregroundColor(.secondary)
                }
            }
        }
    }

    private var sessionInfo: some View {
        Section(header: Text("Session")) {
            HStack(spacing: 12) {
                VStack(alignment: .leading, spacing: 2) {
                    Text("Host fingerprint")
                    Text(controller.peerPine.isEmpty ? "—" : controller.peerPine)
                        .font(.system(.caption, design: .monospaced))
                        .foregroundColor(.secondary)
                        .textSelection(.enabled)
                }
                Spacer()
                Button {
                    UIPasteboard.general.string = controller.peerPine
                } label: {
                    Image(systemName: "doc.on.doc")
                }
                .buttonStyle(.borderless)
                .accessibilityLabel("Copy fingerprint")
                .disabled(controller.peerPine.isEmpty)
            }
            HStack {
                Text("Connection")
                Spacer()
                Text(controller.request.transport.detail)
                    .foregroundColor(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
            Button {
                onAction(.gestureHelp)
            } label: {
                Label("Gesture help", systemImage: "hand.raised")
            }
            NavigationLink(destination: SessionLogView(lines: controller.lines)) {
                Label("Session log", systemImage: "list.bullet")
            }
        }
    }

    // MARK: Rows

    /// A row that sends something to the host and closes the menu.
    private func sendRow(_ title: String, symbol: String, action: @escaping () -> Void) -> some View {
        Button {
            action()
            dismiss()
        } label: {
            Label(title, systemImage: symbol)
        }
    }

    private func shortcutRow(_ title: String, keys: String, _ combo: [KeyStroke]) -> some View {
        Button {
            input.sendCombo(combo)
            dismiss()
        } label: {
            HStack {
                Text(title)
                    .foregroundColor(.primary)
                Spacer()
                Text(keys)
                    .font(.callout.monospacedDigit())
                    .foregroundColor(.secondary)
            }
        }
    }

    private func zoomRow(_ title: String, symbol: String, preset: ZoomPreset) -> some View {
        Button {
            input.applyZoomPreset(preset)
            dismiss()
        } label: {
            Label(title, systemImage: symbol)
        }
    }

    private func monitorTitle(_ i: Int) -> String {
        let m = controller.monitors[i]
        let base = "Monitor \(i + 1) — \(m.w)×\(m.h)"
        return m.primary ? base + " (primary)" : base
    }

    // MARK: Bindings

    private var modeBinding: Binding<InputMode> {
        Binding(get: { input.mode }, set: { input.setMode($0) })
    }

    private var monitorBinding: Binding<Int> {
        Binding(get: { controller.monitorIndex }, set: { controller.selectMonitor($0) })
    }

    private var fpsBinding: Binding<UInt16> {
        Binding(get: { controller.targetFps }, set: { controller.setQuality($0) })
    }
}

// MARK: - Chat

struct ChatSheet: View {
    @ObservedObject var controller: SessionController
    @Environment(\.dismiss) private var dismiss
    @State private var draft = ""

    private var canSend: Bool {
        !draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }

    var body: some View {
        NavigationView {
            VStack(spacing: 0) {
                ScrollViewReader { proxy in
                    ScrollView {
                        LazyVStack(spacing: 8) {
                            if controller.chat.isEmpty {
                                Text("Messages you exchange with the remote computer appear here.")
                                    .font(.subheadline)
                                    .foregroundColor(.secondary)
                                    .multilineTextAlignment(.center)
                                    .padding(.top, 40)
                                    .padding(.horizontal, 24)
                            }
                            ForEach(controller.chat) { message in
                                ChatBubble(message: message)
                                    .id(message.id)
                            }
                        }
                        .padding(12)
                    }
                    .onAppear {
                        scrollToEnd(proxy, animated: false)
                    }
                    .onChange(of: controller.chat.count) { _ in
                        scrollToEnd(proxy, animated: true)
                    }
                }
                Divider()
                HStack(spacing: 8) {
                    TextField("Message", text: $draft)
                        .textFieldStyle(.roundedBorder)
                        .submitLabel(.send)
                        .onSubmit(send)
                    Button(action: send) {
                        Image(systemName: "arrow.up.circle.fill")
                            .font(.system(size: 30))
                    }
                    .disabled(!canSend)
                    .accessibilityLabel("Send")
                }
                .padding(10)
            }
            .navigationTitle("Chat")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") { dismiss() }
                }
            }
        }
        .navigationViewStyle(.stack)
        .onAppear {
            controller.chatVisible = true
            controller.unreadChat = 0
        }
        .onDisappear {
            controller.chatVisible = false
            controller.unreadChat = 0
        }
    }

    private func send() {
        let text = draft.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return }
        draft = ""
        controller.sendChat(text)
    }

    private func scrollToEnd(_ proxy: ScrollViewProxy, animated: Bool) {
        guard let last = controller.chat.last?.id else { return }
        if animated {
            withAnimation(.easeOut(duration: 0.2)) {
                proxy.scrollTo(last, anchor: .bottom)
            }
        } else {
            proxy.scrollTo(last, anchor: .bottom)
        }
    }
}

private struct ChatBubble: View {
    let message: ChatMessage

    var body: some View {
        HStack {
            if message.fromMe {
                Spacer(minLength: 48)
            }
            VStack(alignment: message.fromMe ? .trailing : .leading, spacing: 2) {
                Text(message.text)
                    .foregroundColor(message.fromMe ? .white : .primary)
                    .padding(.horizontal, 12)
                    .padding(.vertical, 8)
                    .background(
                        RoundedRectangle(cornerRadius: 16, style: .continuous)
                            .fill(message.fromMe ? Color.accentColor : Color(.secondarySystemBackground))
                    )
                    .textSelection(.enabled)
                Text(DisplayTime.clock(message.date))
                    .font(.caption2)
                    .foregroundColor(.secondary)
            }
            if !message.fromMe {
                Spacer(minLength: 48)
            }
        }
    }
}

// MARK: - Files

struct FilesSheet: View {
    @ObservedObject var controller: SessionController
    let onSendFile: () -> Void
    @Environment(\.dismiss) private var dismiss
    @State private var shareItem: ReceivedFile?

    var body: some View {
        NavigationView {
            List {
                Section {
                    Button(action: onSendFile) {
                        Label("Send file…", systemImage: "square.and.arrow.up")
                    }
                }
                Section(header: Text("Received")) {
                    if controller.receivedFiles.isEmpty {
                        Text("Files sent from the remote computer appear here.")
                            .font(.subheadline)
                            .foregroundColor(.secondary)
                    }
                    ForEach(Array(controller.receivedFiles.reversed())) { file in
                        Button {
                            shareItem = file
                        } label: {
                            HStack(spacing: 12) {
                                Image(systemName: "doc.fill")
                                    .foregroundColor(.accentColor)
                                VStack(alignment: .leading, spacing: 2) {
                                    Text(file.name)
                                        .foregroundColor(.primary)
                                        .lineLimit(1)
                                        .truncationMode(.middle)
                                    Text(detail(for: file))
                                        .font(.caption)
                                        .foregroundColor(.secondary)
                                }
                                Spacer()
                                Image(systemName: "square.and.arrow.up")
                                    .foregroundColor(.secondary)
                            }
                        }
                    }
                }
            }
            .listStyle(.insetGrouped)
            .navigationTitle("Files")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") { dismiss() }
                }
            }
        }
        .navigationViewStyle(.stack)
        .sheet(item: $shareItem) { file in
            ShareSheet(items: [file.url])
        }
    }

    private func detail(for file: ReceivedFile) -> String {
        let size = ByteCountFormatter.string(fromByteCount: Int64(file.size), countStyle: .file)
        return "\(size) · \(DisplayTime.clock(file.date))"
    }
}

// MARK: - Session log

struct SessionLogView: View {
    let lines: [LogLine]

    var body: some View {
        List {
            if lines.isEmpty {
                Text("No entries")
                    .foregroundColor(.secondary)
            }
            ForEach(Array(lines.reversed())) { line in
                VStack(alignment: .leading, spacing: 2) {
                    Text(line.text)
                        .font(.footnote)
                        .textSelection(.enabled)
                    Text(DisplayTime.clock(line.date))
                        .font(.caption2)
                        .foregroundColor(.secondary)
                }
            }
        }
        .listStyle(.plain)
        .navigationTitle("Session Log")
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .navigationBarTrailing) {
                Button {
                    UIPasteboard.general.string = lines.map { $0.text }.joined(separator: "\n")
                } label: {
                    Image(systemName: "doc.on.doc")
                }
                .accessibilityLabel("Copy log")
                .disabled(lines.isEmpty)
            }
        }
    }
}

// MARK: - UIKit wrappers

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

/// Share sheet wrapper for opening a received file.
struct ShareSheet: UIViewControllerRepresentable {
    let items: [Any]

    func makeUIViewController(context: Context) -> UIActivityViewController {
        UIActivityViewController(activityItems: items, applicationActivities: nil)
    }

    func updateUIViewController(_ uiViewController: UIActivityViewController,
                                context: Context) {}
}
