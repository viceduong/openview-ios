import SwiftUI
import UIKit

// The immersive session (section 3): the remote screen full bleed, with
// the chrome layered on top by phase —
//   connecting (dialing / authenticating / first frame): dimmed last frame
//     + spinner, "Connecting to <name>", step text, Cancel
//   live / reconnecting: stats chip (top-leading), "Reconnecting…" capsule
//     (top), floating toolbar, toasts, first-session gesture help
//   failed: error card (Re-enter PIN / Retry / Close)
// Sheets: session menu, chat, files, document picker, PIN re-entry.

/// Sheets presented over the session (one at a time).
enum SessionSheet: String, Identifiable {
    case menu
    case chat
    case files
    case filePicker
    case pin

    var id: String { rawValue }
}

struct SessionContainer: View {
    @ObservedObject var controller: SessionController
    @ObservedObject var chrome: SessionChrome
    let onClose: () -> Void

    @AppStorage(SettingsKey.showStats) private var showStats = true
    @AppStorage(SettingsKey.didShowGestureHelp) private var didShowGestureHelp = false
    @State private var sheet: SessionSheet?
    @State private var confirmDisconnect = false
    @State private var showGestureHelp = false

    var body: some View {
        GeometryReader { geo in
            ZStack {
                Color.black.ignoresSafeArea()
                // Full bleed under the notch / home indicator, but resized
                // above the keyboard (the surface refits and keeps the
                // cursor visible).
                RemoteScreenView(model: controller.screenModel, input: controller.input)
                    .ignoresSafeArea(.container, edges: .all)
                phaseLayer(landscape: geo.size.width > geo.size.height)
            }
            .frame(width: geo.size.width, height: geo.size.height)
        }
        .statusBar(hidden: true)
        .onAppear {
            controller.start()
            chrome.poke()
            updateIdleTimer()
        }
        .onChange(of: controller.phase) { phase in
            phaseChanged(phase)
        }
        .onChange(of: controller.toast) { value in
            guard let shown = value else { return }
            DispatchQueue.main.asyncAfter(deadline: .now() + 2.5) {
                if controller.toast == shown {
                    controller.toast = nil
                }
            }
        }
        .sheet(item: $sheet) { which in
            sheetView(which)
        }
        .alert("Host identity changed", isPresented: identityAlertShown, presenting: controller.identityMismatch) { _ in
            Button("Disconnect", role: .cancel) {
                controller.identityMismatch = nil
                onClose()
            }
            Button("Continue") {
                controller.acceptNewIdentity()
            }
        } message: { mismatch in
            Text("Expected \(mismatch.expected), got \(mismatch.got). Continue only if the host was reinstalled or its identity was reset on purpose.")
        }
    }

    // MARK: Layers

    @ViewBuilder
    private func phaseLayer(landscape: Bool) -> some View {
        switch controller.phase {
        case .dialing, .authenticating, .waitingForFirstFrame:
            ConnectingOverlay(name: controller.request.displayName,
                              step: controller.phase.stepText,
                              onCancel: onClose)
                .transition(.opacity)
        case .failed(let error):
            SessionErrorView(error: error,
                             isPairing: controller.request.isPairing,
                             onReenterPIN: { sheet = .pin },
                             onRetry: { controller.retry() },
                             onClose: onClose)
                .transition(.opacity)
        case .live, .reconnecting:
            liveChrome(landscape: landscape)
        }
    }

    private func liveChrome(landscape: Bool) -> some View {
        ZStack {
            // Stats chip, top-leading.
            VStack {
                HStack(alignment: .top) {
                    if showStats {
                        StatsChip(liveness: controller.livenessModel,
                                  transport: controller.request.transport.detail,
                                  fingerprint: controller.peerPine)
                    }
                    Spacer(minLength: 0)
                }
                Spacer(minLength: 0)
            }
            .padding(.leading, 12)
            .padding(.top, 8)

            // Reconnecting capsule + toast, top-center.
            VStack(spacing: 8) {
                ReconnectCapsule(phase: controller.phase, liveness: controller.livenessModel)
                if let toast = controller.toast {
                    ToastView(text: toast)
                        .transition(.move(edge: .top).combined(with: .opacity))
                }
                Spacer(minLength: 0)
            }
            .padding(.top, 8)
            .animation(.easeInOut(duration: 0.25), value: controller.toast)

            FloatingToolbar(chrome: chrome,
                            input: controller.input,
                            isLandscape: landscape,
                            unread: controller.unreadChat,
                            onKeyboard: { controller.input.showKeyboard(soft: true) },
                            onMenu: { sheet = .menu },
                            onChat: { sheet = .chat },
                            onDisconnect: requestDisconnect)
                .confirmationDialog("Disconnect from \(controller.request.displayName)?",
                                    isPresented: $confirmDisconnect,
                                    titleVisibility: .visible) {
                    Button("Disconnect", role: .destructive) {
                        onClose()
                    }
                    Button("Cancel", role: .cancel) {}
                }

            if showGestureHelp {
                GestureHelpOverlay(mode: controller.input.mode) {
                    didShowGestureHelp = true
                    showGestureHelp = false
                }
                .transition(.opacity)
            }
        }
        // The remote surface leaves touches inside these frames to the
        // chrome (see RemoteSurfaceView.point(inside:with:)).
        .onPreferenceChange(ChromeFramesKey.self) { rects in
            controller.input.chromeExclusionRects = rects
        }
        .onDisappear {
            controller.input.chromeExclusionRects = []
        }
    }

    @ViewBuilder
    private func sheetView(_ which: SessionSheet) -> some View {
        switch which {
        case .menu:
            SessionMenuSheet(controller: controller,
                             input: controller.input,
                             onAction: handleMenuAction)
        case .chat:
            ChatSheet(controller: controller)
        case .files:
            FilesSheet(controller: controller, onSendFile: {
                switchSheet(to: .filePicker)
            })
        case .filePicker:
            DocumentPicker(onPick: { url in
                sheet = nil
                controller.sendFile(url: url)
            }, onCancel: {
                sheet = nil
            })
        case .pin:
            PinPromptView(title: "Re-enter PIN",
                          subtitle: "PIN for \(controller.request.displayName)",
                          rememberDefault: controller.request.rememberPIN || controller.request.connectionID != nil,
                          onSubmit: { pin, remember in
                              sheet = nil
                              controller.retry(pin: pin, remember: remember)
                          },
                          onCancel: {
                              sheet = nil
                          })
        }
    }

    // MARK: Actions

    private var identityAlertShown: Binding<Bool> {
        Binding(get: { controller.identityMismatch != nil },
                set: { _ in })
    }

    private func requestDisconnect() {
        if AppSettings.confirmDisconnect {
            confirmDisconnect = true
        } else {
            onClose()
        }
    }

    private func handleMenuAction(_ action: SessionMenuAction) {
        sheet = nil
        // Let the menu finish closing before the next presentation.
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.45) {
            switch action {
            case .disconnect:
                requestDisconnect()
            case .sendFile:
                sheet = .filePicker
            case .showFiles:
                sheet = .files
            case .gestureHelp:
                showGestureHelp = true
            case .showKeyBar:
                controller.input.showKeyboard(soft: false)
            }
        }
    }

    private func switchSheet(to next: SessionSheet) {
        sheet = nil
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.45) {
            sheet = next
        }
    }

    private func phaseChanged(_ phase: SessionPhase) {
        updateIdleTimer()
        // Wrong PIN: the stored one was deleted — ask for it right away
        // (the error card stays behind with Retry / Close).
        if phase == .failed(.authFailed) && !controller.request.isPairing && sheet == nil {
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.35) {
                if controller.phase == .failed(.authFailed) && sheet == nil {
                    sheet = .pin
                }
            }
        }
        if phase == .live {
            chrome.poke()
            if !didShowGestureHelp {
                showGestureHelp = true
            }
        }
    }

    /// Keep the screen awake while connected (setting).
    private func updateIdleTimer() {
        let connected: Bool
        switch controller.phase {
        case .live, .reconnecting: connected = true
        default: connected = false
        }
        UIApplication.shared.isIdleTimerDisabled = AppSettings.keepAwake && connected
    }
}

// MARK: - Remote screen

/// The remote display: the surface once the first frame exists, black
/// before (the connecting overlay explains what is happening).
private struct RemoteScreenView: View {
    @ObservedObject var model: ScreenModel
    let input: RemoteInputController

    var body: some View {
        if let img = model.image {
            RemoteScreenSurface(image: img,
                                pixels: model.pixels,
                                frameSize: model.frameSize,
                                video: model.video,
                                cursorPos: model.cursorPos,
                                cursorImage: model.cursorImage,
                                cursorHotspot: model.cursorHotspot,
                                cursorSize: model.cursorSize,
                                input: input)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .background(Color.black)
        } else {
            Color.black
        }
    }
}

// MARK: - Connecting / error

struct ConnectingOverlay: View {
    let name: String
    let step: String
    let onCancel: () -> Void

    var body: some View {
        ZStack {
            Color.black.opacity(0.65).ignoresSafeArea()
            VStack(spacing: 14) {
                ProgressView()
                    .progressViewStyle(CircularProgressViewStyle(tint: .white))
                    .scaleEffect(1.4)
                    .padding(.bottom, 6)
                Text("Connecting to \(name)")
                    .font(.headline)
                    .foregroundColor(.white)
                    .multilineTextAlignment(.center)
                Text(step)
                    .font(.subheadline)
                    .foregroundColor(Color.white.opacity(0.7))
                    .multilineTextAlignment(.center)
                Button(role: .cancel, action: onCancel) {
                    Text("Cancel")
                        .frame(minWidth: 120)
                }
                .buttonStyle(.bordered)
                .controlSize(.large)
                .tint(.white)
                .padding(.top, 8)
            }
            .padding(24)
        }
    }
}

struct SessionErrorView: View {
    let error: SessionError
    let isPairing: Bool
    let onReenterPIN: () -> Void
    let onRetry: () -> Void
    let onClose: () -> Void

    private var isAuth: Bool {
        if case .authFailed = error { return true }
        return false
    }

    private var symbol: String {
        switch error {
        case .authFailed: return "lock.fill"
        case .hostUnreachable: return "exclamationmark.triangle.fill"
        case .disconnected, .transport: return "wifi.exclamationmark"
        }
    }

    private var title: String {
        switch error {
        case .authFailed: return "Couldn't sign in"
        case .hostUnreachable: return "Host not responding"
        case .disconnected: return "Disconnected"
        case .transport: return "Connection failed"
        }
    }

    private var message: String {
        switch error {
        case .authFailed:
            return isPairing
                ? "The pairing code was rejected. Check the code shown on the host and try again."
                : "PIN rejected. Check the access PIN set in OpenView Host."
        case .hostUnreachable:
            return "Check that OpenView Host is running on the remote computer and that it is online."
        case .disconnected(let reason):
            return "The session ended: \(reason)."
        case .transport(let reason):
            return reason
        }
    }

    var body: some View {
        ZStack {
            Color.black.opacity(0.78).ignoresSafeArea()
            VStack(spacing: 12) {
                Image(systemName: symbol)
                    .font(.system(size: 44))
                    .foregroundColor(isAuth ? .orange : .red)
                    .padding(.bottom, 4)
                Text(title)
                    .font(.title3.weight(.semibold))
                    .foregroundColor(.white)
                Text(message)
                    .font(.subheadline)
                    .foregroundColor(Color.white.opacity(0.75))
                    .multilineTextAlignment(.center)
                    .frame(maxWidth: 340)
                VStack(spacing: 10) {
                    if isAuth && !isPairing {
                        Button(action: onReenterPIN) {
                            Text("Re-enter PIN").frame(maxWidth: .infinity)
                        }
                        .buttonStyle(.borderedProminent)
                        Button(action: onRetry) {
                            Text("Retry").frame(maxWidth: .infinity)
                        }
                        .buttonStyle(.bordered)
                    } else {
                        Button(action: onRetry) {
                            Text("Retry").frame(maxWidth: .infinity)
                        }
                        .buttonStyle(.borderedProminent)
                    }
                    Button(role: .cancel, action: onClose) {
                        Text("Close").frame(maxWidth: .infinity)
                    }
                    .buttonStyle(.bordered)
                    .tint(.white)
                }
                .controlSize(.large)
                .frame(maxWidth: 280)
                .padding(.top, 10)
            }
            .padding(24)
        }
    }
}

// MARK: - Live chrome pieces

/// "Reconnecting…" capsule: while auto-reconnecting, or while the host is
/// silent past the liveness window.
private struct ReconnectCapsule: View {
    let phase: SessionPhase
    @ObservedObject var liveness: LivenessModel

    private var label: String? {
        switch phase {
        case .reconnecting(let n):
            return "Reconnecting… (\(n)/\(SessionController.maxReconnectAttempts))"
        case .live:
            return liveness.peerAlive ? nil : "Reconnecting…"
        default:
            return nil
        }
    }

    var body: some View {
        if let label {
            HStack(spacing: 8) {
                ProgressView()
                    .progressViewStyle(CircularProgressViewStyle(tint: .white))
                    .scaleEffect(0.8)
                Text(label)
                    .font(.subheadline.weight(.semibold))
                    .foregroundColor(.white)
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 8)
            .background(Capsule().fill(Color.orange.opacity(0.92)))
            .shadow(color: Color.black.opacity(0.3), radius: 6, y: 2)
            .allowsHitTesting(false)
        }
    }
}

/// Connection quality chip: dot (green / amber / red from RTT + liveness)
/// and the RTT; tap for RTT, FPS, transport and host fingerprint.
private struct StatsChip: View {
    @ObservedObject var liveness: LivenessModel
    let transport: String
    let fingerprint: String
    @State private var expanded = false

    private var qualityColor: Color {
        guard liveness.peerAlive else { return .red }
        let rtt = liveness.rttMs
        if rtt < 0 { return .gray }
        if rtt < 100 { return .green }
        if rtt < 250 { return .orange }
        return .red
    }

    private var rttText: String {
        liveness.rttMs >= 0 ? "\(liveness.rttMs) ms" : "— ms"
    }

    private var accessibilityText: String {
        guard let path = liveness.path else { return "Connection quality \(rttText)" }
        return "Connection quality \(rttText), \(path)"
    }

    /// Section 5: "Direct" / "Relay" / "Probing…" next to the RTT.
    private var shortPath: String? {
        guard let path = liveness.path else { return nil }
        if path.hasPrefix("Direct") { return "Direct" }
        return path
    }

    var body: some View {
        Button {
            withAnimation(.easeInOut(duration: 0.2)) {
                expanded.toggle()
            }
        } label: {
            VStack(alignment: .leading, spacing: 6) {
                HStack(spacing: 6) {
                    Circle()
                        .fill(qualityColor)
                        .frame(width: 8, height: 8)
                    Text(rttText)
                        .font(.caption.monospacedDigit().weight(.semibold))
                    if let p = shortPath {
                        Text("· \(p)")
                            .font(.caption.weight(.semibold))
                            .foregroundColor(p == "Direct" ? .green : Color.white.opacity(0.75))
                    }
                }
                if expanded {
                    detailRow("RTT", rttText)
                    detailRow("FPS", String(format: "%.1f", Double(liveness.fps)))
                    detailRow("Link", transport)
                    if let path = liveness.path {
                        detailRow("Path", path)
                    }
                    detailRow("Host", fingerprint.isEmpty ? "—" : fingerprint)
                }
            }
            .foregroundColor(.white)
            .padding(.horizontal, 10)
            .padding(.vertical, 7)
            .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
        }
        .buttonStyle(.plain)
        .reportsChromeFrame()
        .accessibilityLabel(accessibilityText)
    }

    private func detailRow(_ key: String, _ value: String) -> some View {
        HStack(alignment: .top, spacing: 8) {
            Text(key)
                .foregroundColor(Color.white.opacity(0.6))
                .frame(width: 36, alignment: .leading)
            Text(value)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: 210, alignment: .leading)
        }
        .font(.caption2.monospacedDigit())
    }
}

private struct ToastView: View {
    let text: String

    var body: some View {
        Text(text)
            .font(.subheadline.weight(.medium))
            .foregroundColor(.white)
            .lineLimit(2)
            .padding(.horizontal, 14)
            .padding(.vertical, 8)
            .background(.ultraThinMaterial, in: Capsule())
            .allowsHitTesting(false)
    }
}

// MARK: - Gesture help

struct GestureHelpItem: Identifiable {
    let id: Int
    let symbol: String
    let text: String
}

enum GestureHelp {
    /// The help rows for `mode`, matching RemoteScreenSurface's gestures
    /// (and the "Immediate clicks" / "Classic touch gestures" settings).
    static func items(for mode: InputMode) -> [GestureHelpItem] {
        let d = UserDefaults.standard
        switch mode {
        case .mouse:
            let immediate = d.bool(forKey: RemoteInputController.immediateClicksDefaultsKey)
            return [
                GestureHelpItem(id: 0, symbol: "hand.draw", text: "Slide a finger to move the mouse"),
                GestureHelpItem(id: 1, symbol: "hand.tap", text: "Tap to click at the mouse pointer"),
                GestureHelpItem(id: 2, symbol: "hand.tap", text: "Double-tap to double-click"),
                GestureHelpItem(id: 3, symbol: "cursorarrow.motionlines",
                                text: immediate ? "Tap, then hold or drag to select" : "Double-tap and drag to select"),
                GestureHelpItem(id: 4, symbol: "hand.point.up.left", text: "Long press to right-click"),
                GestureHelpItem(id: 5, symbol: "hand.tap.fill", text: "Two-finger tap to right-click"),
                GestureHelpItem(id: 6, symbol: "arrow.up.and.down", text: "Two fingers to scroll"),
                GestureHelpItem(id: 7, symbol: "arrow.up.left.and.arrow.down.right", text: "Pinch to zoom"),
            ]
        case .touch:
            let classic = d.bool(forKey: RemoteInputController.classicTouchDefaultsKey)
            return [
                GestureHelpItem(id: 0, symbol: "hand.tap", text: "Tap to click where you touch"),
                GestureHelpItem(id: 1, symbol: "hand.point.up.left", text: "Long press to right-click (with magnifier)"),
                GestureHelpItem(id: 2, symbol: "hand.draw", text: "Long press and drag to select"),
                GestureHelpItem(id: 3, symbol: "hand.tap.fill", text: "Two-finger tap to right-click"),
                GestureHelpItem(id: 4, symbol: "arrow.up.and.down",
                                text: classic ? "Two fingers to scroll" : "One finger to scroll"),
                GestureHelpItem(id: 5, symbol: "arrow.up.and.down.and.arrow.left.and.right",
                                text: classic ? "Drag to move around a zoomed screen" : "Two fingers to pan"),
                GestureHelpItem(id: 6, symbol: "arrow.up.left.and.arrow.down.right", text: "Pinch to zoom"),
            ]
        }
    }
}

struct GestureHelpRow: View {
    let item: GestureHelpItem

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: item.symbol)
                .font(.system(size: 17))
                .foregroundColor(.accentColor)
                .frame(width: 28)
            Text(item.text)
                .font(.subheadline)
        }
    }
}

/// First-session gesture help (also from the menu).
struct GestureHelpOverlay: View {
    let mode: InputMode
    let onContinue: () -> Void

    var body: some View {
        ZStack {
            Color.black.opacity(0.7).ignoresSafeArea()
            VStack(alignment: .leading, spacing: 12) {
                Text(mode == .mouse ? "Mouse mode" : "Touch mode")
                    .font(.title3.weight(.bold))
                Text("Switch modes from the toolbar.")
                    .font(.footnote)
                    .foregroundColor(Color.white.opacity(0.7))
                ForEach(GestureHelp.items(for: mode)) { item in
                    GestureHelpRow(item: item)
                }
                Button(action: onContinue) {
                    Text("Continue").frame(maxWidth: .infinity)
                }
                .buttonStyle(.borderedProminent)
                .controlSize(.large)
                .padding(.top, 6)
            }
            .foregroundColor(.white)
            .padding(22)
            .frame(maxWidth: 380)
            .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 20, style: .continuous))
            .padding(20)
        }
    }
}
