import LocalAuthentication
import SwiftUI
import UIKit

struct SettingsView: View {
    @EnvironmentObject private var app: AppModel
    let onDone: () -> Void

    // Connection
    @AppStorage(SettingsKey.defaultRelayURL) private var relayURL = ""
    @AppStorage(SettingsKey.defaultQuality) private var defaultQuality = 0
    @AppStorage(SettingsKey.autoReconnect) private var autoReconnect = true
    @AppStorage(SettingsKey.autoConnectOnLaunch) private var autoConnectOnLaunch = false
    @AppStorage(SettingsKey.confirmDisconnect) private var confirmDisconnect = true
    @AppStorage(SettingsKey.directConnection) private var directConnection = true
    // Display
    @AppStorage(SettingsKey.sharpen) private var sharpen = true
    // Input
    @AppStorage(RemoteInputController.cmdAsCtrlDefaultsKey) private var cmdAsCtrl = true
    @AppStorage(RemoteInputController.pointerLockDefaultsKey) private var pointerLock = false
    // Session
    @AppStorage(SettingsKey.keepAwake) private var keepAwake = true
    @AppStorage(SettingsKey.playPCSound) private var playPCSound = true
    @AppStorage(SettingsKey.orientation) private var orientation = "auto"
    @AppStorage(SettingsKey.showStats) private var showStats = true
    @AppStorage(SettingsKey.toolbarFade) private var toolbarFade = false
    @AppStorage(SettingsKey.didShowGestureHelp) private var didShowGestureHelp = false
    // Security
    @AppStorage(SettingsKey.appLock) private var appLock = false
    @AppStorage(SettingsKey.pinBiometric) private var pinBiometric = false
    // Appearance
    @AppStorage(SettingsKey.appearance) private var appearance = "system"

    @State private var confirmForgetPINs = false
    @State private var biometryUnavailable = false
    /// Diagnostics: the kept session log files offered to the share sheet.
    @State private var shareLogs: [URL] = []
    @State private var showShareLogs = false

    private var version: String {
        let info = Bundle.main.infoDictionary
        let short = info?["CFBundleShortVersionString"] as? String ?? "?"
        let build = info?["CFBundleVersion"] as? String ?? "?"
        return "\(short) (\(build))"
    }

    var body: some View {
        NavigationView {
            Form {
                connection
                directSection
                input
                display
                session
                security
                appearanceSection
                about
            }
            .navigationTitle("Settings")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done", action: onDone)
                }
            }
            .alert("Face ID unavailable", isPresented: $biometryUnavailable) {
                Button("OK", role: .cancel) {}
            } message: {
                Text("Set up Face ID, Touch ID or a device passcode in iOS Settings first.")
            }
            .confirmationDialog("Forget all saved PINs?", isPresented: $confirmForgetPINs,
                                titleVisibility: .visible) {
                Button("Forget All PINs", role: .destructive) {
                    app.forgetAllPINs()
                }
                Button("Cancel", role: .cancel) {}
            }
        }
        .navigationViewStyle(.stack)
    }

    // MARK: Sections

    /// Footer of the Connection section: what the relay field accepts, and
    /// whether the current value is usable.
    private var relayFooter: String {
        let trimmed = relayURL.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.isEmpty {
            if !AppSettings.bundledRelayURL.isEmpty {
                return "Using this build's relay: \(AppSettings.bundledRelayURL)."
            }
            return "No relay server: only LAN connections work. Enter your relay (ovrelay) as wss://host/relay or host:port."
        }
        if !RelayAddress.isValid(trimmed) {
            return "Not a relay address. Use wss://host/relay (WebSocket) or host:port (TCP)."
        }
        return "Used for new connections. A wss:// address goes through the WebSocket relay; host:port uses TCP."
    }

    private var connection: some View {
        Section(header: Text("Connection"), footer: Text(relayFooter)) {
            TextField(AppSettings.bundledRelayURL.isEmpty ? "Relay server (wss://host/relay)" : AppSettings.bundledRelayURL,
                      text: $relayURL)
                .keyboardType(.URL)
                .disableAutocorrection(true)
                .textInputAutocapitalization(.never)
            Picker("Default quality", selection: $defaultQuality) {
                ForEach(QualityChoice.all) { choice in
                    Text(choice.label).tag(Int(choice.fps))
                }
            }
            Toggle("Reconnect automatically", isOn: $autoReconnect)
            Toggle(isOn: $autoConnectOnLaunch) {
                VStack(alignment: .leading, spacing: 2) {
                    Text("Connect to last device on launch")
                    Text("Opens the most recent device that has a saved PIN when the app starts.")
                        .font(.caption)
                        .foregroundColor(.secondary)
                }
            }
            Toggle("Confirm before disconnecting", isOn: $confirmDisconnect)
        }
    }

    /// Direct UDP path next to relay sessions.
    private var directSection: some View {
        Section(header: Text("Direct Connection"),
                footer: Text(Self.directHelp)) {
            Toggle(isOn: $directConnection) {
                VStack(alignment: .leading, spacing: 2) {
                    Text("Direct connection")
                    Text("Relay sessions switch to a direct UDP path when the host is reachable.")
                        .font(.caption)
                        .foregroundColor(.secondary)
                }
            }
        }
    }

    static let directHelp = """
        Applies to new connections; the relay stays as the fallback. \
        With a VPN app on this iPhone, add DIRECT (bypass) rules for the host's \
        public IP and your LAN so the direct path skips the tunnel, e.g.:
        IP-CIDR,203.0.113.7/32,DIRECT
        IP-CIDR,192.168.1.0/24,DIRECT
        IP-CIDR,100.64.0.0/10,DIRECT
        """

    /// Renderer look.
    private var display: some View {
        Section(header: Text("Display"),
                footer: Text("Sharpens the remote screen after scaling.")) {
            Toggle("Sharpen", isOn: $sharpen)
        }
    }

    private var input: some View {
        Section(header: Text("Input")) {
            Toggle("Cmd acts as Ctrl", isOn: $cmdAsCtrl)
            Toggle(isOn: $pointerLock) {
                VStack(alignment: .leading, spacing: 2) {
                    Text("Pointer lock (iPad)")
                    Text("Full-screen sessions capture the mouse. Cmd+Shift+P toggles it.")
                        .font(.caption)
                        .foregroundColor(.secondary)
                }
            }
        }
    }

    private var session: some View {
        Section(header: Text("Session")) {
            Toggle("Keep screen awake", isOn: $keepAwake)
            Toggle(isOn: $playPCSound) {
                VStack(alignment: .leading, spacing: 2) {
                    Text("Play PC sound")
                    Text("Plays the remote PC's audio here, mixed with other apps.")
                        .font(.caption)
                        .foregroundColor(.secondary)
                }
            }
            Picker("Orientation", selection: $orientation) {
                Text("Automatic").tag("auto")
                Text("Lock to landscape").tag("landscape")
            }
            Toggle("Show connection stats", isOn: $showStats)
            Toggle("Fade toolbar when idle", isOn: $toolbarFade)
            Button("Show gesture help again") {
                didShowGestureHelp = false
            }
            .disabled(!didShowGestureHelp)
        }
    }

    private var security: some View {
        Section(header: Text("Security"),
                footer: Text("Face ID protection for saved PINs applies to PINs saved from now on.")) {
            Toggle("Require Face ID to open", isOn: $appLock)
                .onChange(of: appLock) { on in
                    if on && !Self.canAuthenticate() {
                        appLock = false
                        biometryUnavailable = true
                    }
                }
            Toggle("Require Face ID for saved PINs", isOn: $pinBiometric)
                .onChange(of: pinBiometric) { on in
                    if on && !Self.canAuthenticate() {
                        pinBiometric = false
                        biometryUnavailable = true
                    }
                }
            Button("Forget all saved PINs", role: .destructive) {
                confirmForgetPINs = true
            }
        }
    }

    private var appearanceSection: some View {
        Section(header: Text("Appearance")) {
            Picker("Appearance", selection: $appearance) {
                Text("System").tag("system")
                Text("Light").tag("light")
                Text("Dark").tag("dark")
            }
            .pickerStyle(.segmented)
        }
    }

    private var about: some View {
        Section(header: Text("About")) {
            valueRow("Version", version)
            valueRow("This device's short code", PairingState.shared.code.isEmpty ? "—" : PairingState.shared.code)
            VStack(alignment: .leading, spacing: 4) {
                Text("This device's fingerprint")
                Text(PairingState.shared.pine.isEmpty ? "—" : PairingState.shared.pine)
                    .font(.system(.caption, design: .monospaced))
                    .foregroundColor(.secondary)
                    .textSelection(.enabled)
            }
            .padding(.vertical, 2)
            NavigationLink(destination: SessionLogView(lines: app.lastSessionLog)) {
                Text("Last session log")
            }
            Button {
                shareLogs = DiagLog.shared.allFiles()
                showShareLogs = !shareLogs.isEmpty
            } label: {
                VStack(alignment: .leading, spacing: 2) {
                    Text("Share diagnostic logs")
                    Text("Errors, connection events and network changes from recent launches. Nothing is sent unless you share it.")
                        .font(.caption)
                        .foregroundColor(.secondary)
                }
            }
            .sheet(isPresented: $showShareLogs) {
                ShareSheet(items: shareLogs)
            }
        }
    }

    private func valueRow(_ title: String, _ value: String) -> some View {
        HStack {
            Text(title)
            Spacer()
            Text(value)
                .foregroundColor(.secondary)
                .font(.body.monospacedDigit())
                .textSelection(.enabled)
        }
    }

    private static func canAuthenticate() -> Bool {
        LAContext().canEvaluatePolicy(.deviceOwnerAuthentication, error: nil)
    }
}
