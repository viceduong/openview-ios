import SwiftUI
import UIKit

// Home: one-tap reconnect to the most recent device with a saved PIN,
// Quick Connect by device ID, "Pair with code instead", ON THIS NETWORK
// (OpenView PCs found by Bonjour, LanDiscovery), FAVORITES and
// RECENT saved connections (tap = connect; swipe Edit / Delete; context
// menu), empty state and "New connection". On first launch it offers the
// relay setup sheet.

/// A PIN prompt for a device (saved or typed into Quick Connect).
struct PinRequest: Identifiable {
    let id = UUID()
    /// Device ID digits.
    let deviceID: String
    let saved: SavedConnection?
    /// Name to show for an unsaved device (e.g. the PC name from Bonjour).
    var name: String = ""
    /// Dial this instead of the default relay (a PC found on the LAN).
    var transport: ConnectionTransport? = nil

    var title: String {
        saved?.displayName ?? (name.isEmpty ? DeviceID.format(deviceID) : name)
    }
}

enum HomeSheet: Identifiable {
    case relaySetup
    case settings
    case pair
    case newConnection
    case edit(SavedConnection)
    case pin(PinRequest)

    var id: String {
        switch self {
        case .relaySetup: return "relaySetup"
        case .settings: return "settings"
        case .pair: return "pair"
        case .newConnection: return "new"
        case .edit(let c): return "edit-\(c.id.uuidString)"
        case .pin(let p): return "pin-\(p.id.uuidString)"
        }
    }
}

struct HomeView: View {
    @EnvironmentObject private var app: AppModel
    @EnvironmentObject private var store: ConnectionStore
    @State private var quickID = ""
    @State private var search = ""
    @State private var sheet: HomeSheet?
    @StateObject private var lan = LanDiscovery()
    /// The LAN device being resolved (its row shows a spinner).
    @State private var resolvingID: String?
    @State private var lanError: String?

    var body: some View {
        NavigationView {
            List {
                if let last = reconnectCandidate {
                    Section {
                        ReconnectCard(connection: last) {
                            connect(last)
                        }
                    }
                }
                quickConnectSection
                lanSection
                if !favorites.isEmpty {
                    Section(header: Text("Favorites")) {
                        ForEach(favorites) { c in
                            row(c)
                        }
                    }
                }
                if !recents.isEmpty {
                    Section(header: Text("Recent")) {
                        ForEach(recents) { c in
                            row(c)
                        }
                    }
                }
                if store.connections.isEmpty {
                    Section {
                        EmptyConnectionsView()
                    }
                }
                Section {
                    Button {
                        sheet = .newConnection
                    } label: {
                        Label("New connection", systemImage: "plus.circle.fill")
                    }
                }
            }
            .listStyle(.insetGrouped)
            .alert("Can't reach this PC", isPresented: lanErrorShown) {
                Button("OK", role: .cancel) {}
            } message: {
                Text(lanError ?? "")
            }
            .navigationTitle("OpenView")
            .toolbar {
                ToolbarItem(placement: .navigationBarTrailing) {
                    Button {
                        sheet = .settings
                    } label: {
                        Image(systemName: "gearshape")
                    }
                    .accessibilityLabel("Settings")
                }
            }
            .modifier(SearchableIfNeeded(enabled: store.connections.count > 6, text: $search))
        }
        .navigationViewStyle(.stack)
        .onAppear(perform: firstAppear)
        .onAppear { if !app.sessionActive { lan.start() } }
        .onDisappear { lan.stop() }
        .onChange(of: app.sessionActive) { active in
            // No browsing while a session runs.
            if active { lan.stop() } else { lan.start() }
        }
        .sheet(item: $sheet) { which in
            sheetView(which)
                .environmentObject(app)
                .environmentObject(store)
        }
        .alert("No relay server", isPresented: $app.relayMissing) {
            Button("Open Settings") {
                sheet = .settings
            }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("This connection goes through a relay server, but none is configured. Add your relay in Settings > Connection, or use a LAN connection.")
        }
    }

    // MARK: Lists

    private var reconnectCandidate: SavedConnection? {
        search.isEmpty ? store.mostRecentWithPIN : nil
    }

    private var favorites: [SavedConnection] {
        store.favorites.filter { matches($0) }
    }

    private var recents: [SavedConnection] {
        store.recents.filter { matches($0) }
    }

    private func matches(_ c: SavedConnection) -> Bool {
        let q = search.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !q.isEmpty else { return true }
        if c.displayName.localizedCaseInsensitiveContains(q) { return true }
        let digits = DeviceID.digits(q)
        return !digits.isEmpty && c.deviceId.contains(digits)
    }

    // MARK: Quick Connect

    private var quickConnectSection: some View {
        Section(header: Text("Quick Connect"),
                footer: Text("Enter the partner's device ID. You'll be asked for its access PIN.")) {
            HStack(spacing: 12) {
                TextField("Partner ID", text: quickIDBinding)
                    .keyboardType(.numberPad)
                    .font(.title3.monospacedDigit())
                    .disableAutocorrection(true)
                Button("Connect", action: quickConnect)
                    .buttonStyle(.borderedProminent)
                    .disabled(UInt64(quickID) == nil)
            }
            .padding(.vertical, 4)
            Button {
                sheet = .pair
            } label: {
                HStack {
                    Text("Pair with code instead")
                    Spacer()
                    Image(systemName: "chevron.right")
                        .font(.footnote.weight(.semibold))
                        .foregroundColor(.secondary)
                }
            }
        }
    }

    /// Shows the ID grouped ("123 456 789"), stores digits only.
    private var quickIDBinding: Binding<String> {
        Binding(get: { DeviceID.format(quickID) },
                set: { quickID = String(DeviceID.digits($0).prefix(18)) })
    }

    private func quickConnect() {
        guard UInt64(quickID) != nil, !app.sessionActive else { return }
        UIApplication.shared.sendAction(#selector(UIResponder.resignFirstResponder), to: nil, from: nil, for: nil)
        if let existing = store.find(deviceId: quickID) {
            connect(existing)
        } else if AppSettings.defaultRelayURL.isEmpty {
            // Quick Connect dials through the default relay: ask for one
            // before prompting for the PIN.
            app.relayMissing = true
        } else {
            sheet = .pin(PinRequest(deviceID: quickID, saved: nil))
        }
    }

    // MARK: On this network

    @ViewBuilder
    private var lanSection: some View {
        if !lan.hosts.isEmpty {
            Section(header: Text("On this network"),
                    footer: Text("OpenView PCs on your Wi-Fi. Tap one to connect directly.")) {
                ForEach(lan.hosts) { pc in
                    Button {
                        connectLan(pc)
                    } label: {
                        LanHostRow(pc: pc,
                                   saved: store.find(deviceId: pc.deviceID),
                                   resolving: resolvingID == pc.deviceID)
                    }
                    .foregroundColor(.primary)
                    .disabled(resolvingID != nil || app.sessionActive)
                }
            }
        }
    }

    private var lanErrorShown: Binding<Bool> {
        Binding(get: { lanError != nil }, set: { if !$0 { lanError = nil } })
    }

    /// Resolve the PC's Bonjour service to IP:port and dial it directly: a
    /// saved device keeps its PIN / name / pinned identity, a new one asks
    /// for the PIN first.
    private func connectLan(_ pc: DiscoveredPC) {
        guard !app.sessionActive, resolvingID == nil else { return }
        resolvingID = pc.deviceID
        LanDiscovery.resolve(pc) { host, port in
            resolvingID = nil
            guard let host, let port, port > 0 else {
                lanError = "\(pc.name) was found on this network but could not be resolved. Check that both devices are on the same network and try again."
                return
            }
            let transport = ConnectionTransport.lan(host: host, port: port)
            if let saved = store.find(deviceId: pc.deviceID) {
                connect(saved, via: transport)
            } else {
                sheet = .pin(PinRequest(deviceID: pc.deviceID, saved: nil,
                                        name: pc.name, transport: transport))
            }
        }
    }

    // MARK: Rows

    private func row(_ c: SavedConnection) -> some View {
        Button {
            connect(c)
        } label: {
            ConnectionRow(connection: c)
        }
        .foregroundColor(.primary)
        .swipeActions(edge: .trailing, allowsFullSwipe: false) {
            Button(role: .destructive) {
                app.delete(c)
            } label: {
                Label("Delete", systemImage: "trash")
            }
            Button {
                sheet = .edit(c)
            } label: {
                Label("Edit", systemImage: "pencil")
            }
            .tint(.accentColor)
        }
        .contextMenu {
            Button {
                connect(c)
            } label: {
                Label("Connect", systemImage: "play.fill")
            }
            Button {
                sheet = .edit(c)
            } label: {
                Label("Edit", systemImage: "pencil")
            }
            Button {
                UIPasteboard.general.string = c.deviceId
            } label: {
                Label("Copy ID", systemImage: "doc.on.doc")
            }
            if c.hasPIN {
                Button {
                    app.forgetPIN(c)
                } label: {
                    Label("Forget PIN", systemImage: "key")
                }
            }
            Button {
                app.toggleFavorite(c)
            } label: {
                Label(c.isFavorite ? "Remove from Favorites" : "Add to Favorites",
                      systemImage: c.isFavorite ? "star.slash" : "star")
            }
        }
    }

    // MARK: Connecting

    /// Once per app launch (not when returning from a session): offer the
    /// first-run relay setup, else connect to the most recent device with a
    /// saved PIN if that setting is on.
    private static var didHandleFirstAppear = false

    private func firstAppear() {
        guard !Self.didHandleFirstAppear else { return }
        Self.didHandleFirstAppear = true
        if AppSettings.needsRelaySetup {
            sheet = .relaySetup
            return
        }
        guard AppSettings.autoConnectOnLaunch, !app.sessionActive,
              let last = store.mostRecentWithPIN else { return }
        connect(last)
    }

    /// Connect a saved device: stored PIN (Face ID if protected) or prompt.
    /// `override` dials another transport for this attempt only (a LAN
    /// address just found); the saved transport is left unchanged.
    private func connect(_ c: SavedConnection, via override: ConnectionTransport? = nil) {
        guard !app.sessionActive else { return }
        var c = c
        if let override {
            c.transport = override
        }
        guard !c.transport.isMissingRelay else {
            app.relayMissing = true
            return
        }
        guard c.hasPIN else {
            sheet = .pin(PinRequest(deviceID: c.deviceId, saved: c))
            return
        }
        app.loadPIN(for: c) { pin in
            if let pin, let req = app.request(for: c, pin: pin, remember: true, pinIsStored: true) {
                app.connect(req)
            } else {
                sheet = .pin(PinRequest(deviceID: c.deviceId, saved: c))
            }
        }
    }

    private func submitPIN(_ p: PinRequest, pin: String, remember: Bool) {
        sheet = nil
        let req: ConnectRequest?
        if let saved = p.saved {
            req = app.request(for: saved, pin: pin, remember: remember, pinIsStored: false)
        } else {
            var quick = app.quickRequest(deviceID: p.deviceID, pin: pin, remember: remember)
            if let transport = p.transport {
                quick?.transport = transport
            }
            if !p.name.isEmpty {
                quick?.name = p.name
            }
            req = quick
        }
        guard let req else { return }
        quickID = ""
        // SessionPresenter waits for the PIN sheet to finish closing.
        app.connect(req)
    }

    /// "Save & Connect" from the editor.
    private func connectAfterEdit(_ c: SavedConnection, pin: String?, remember: Bool) {
        sheet = nil
        if let pin, !pin.isEmpty,
           let req = app.request(for: c, pin: pin, remember: remember, pinIsStored: remember) {
            app.connect(req)
            return
        }
        // No PIN typed: use the stored one or ask, once the editor closed.
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) {
            connect(c)
        }
    }

    // MARK: Sheets

    @ViewBuilder
    private func sheetView(_ which: HomeSheet) -> some View {
        switch which {
        case .relaySetup:
            RelaySetupView(onDone: { sheet = nil })
        case .settings:
            SettingsView(onDone: { sheet = nil })
        case .pair:
            PairView(onDone: { sheet = nil })
        case .newConnection:
            ConnectionEditView(existing: nil,
                               onCancel: { sheet = nil },
                               onSaved: { _ in sheet = nil },
                               onSaveAndConnect: { c, pin, remember in
                                   connectAfterEdit(c, pin: pin, remember: remember)
                               })
        case .edit(let c):
            ConnectionEditView(existing: c,
                               onCancel: { sheet = nil },
                               onSaved: { _ in sheet = nil },
                               onSaveAndConnect: { saved, pin, remember in
                                   connectAfterEdit(saved, pin: pin, remember: remember)
                               })
        case .pin(let p):
            PinPromptView(title: "Enter PIN",
                          subtitle: "Access PIN for \(p.title)",
                          rememberDefault: true,
                          onSubmit: { pin, remember in
                              submitPIN(p, pin: pin, remember: remember)
                          },
                          onCancel: { sheet = nil })
        }
    }
}

// MARK: - Pieces

/// `.searchable` only when the list is long enough to need it.
struct SearchableIfNeeded: ViewModifier {
    let enabled: Bool
    @Binding var text: String

    @ViewBuilder
    func body(content: Content) -> some View {
        if enabled {
            content.searchable(text: $text, prompt: Text("Search connections"))
        } else {
            content
        }
    }
}

struct ConnectionRow: View {
    let connection: SavedConnection

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: connection.symbol)
                .font(.system(size: 20))
                .foregroundColor(.accentColor)
                .frame(width: 34, height: 34)
                .background(RoundedRectangle(cornerRadius: 8, style: .continuous)
                    .fill(Color.accentColor.opacity(0.12)))
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    Text(connection.displayName)
                        .font(.body.weight(.semibold))
                        .lineLimit(1)
                    if connection.hasPIN {
                        Image(systemName: "key.fill")
                            .font(.caption)
                            .foregroundColor(.secondary)
                            .accessibilityLabel("PIN saved")
                    }
                }
                Text(connection.subtitle)
                    .font(.subheadline)
                    .foregroundColor(.secondary)
                    .lineLimit(1)
            }
            Spacer(minLength: 0)
            if connection.isFavorite {
                Image(systemName: "star.fill")
                    .font(.caption)
                    .foregroundColor(.yellow)
            }
        }
        .padding(.vertical, 2)
        .contentShape(Rectangle())
    }
}

/// A PC found on the local network.
struct LanHostRow: View {
    let pc: DiscoveredPC
    let saved: SavedConnection?
    let resolving: Bool

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: "desktopcomputer")
                .font(.system(size: 18))
                .foregroundColor(.accentColor)
                .frame(width: 34, height: 34)
                .background(RoundedRectangle(cornerRadius: 8, style: .continuous)
                    .fill(Color.accentColor.opacity(0.12)))
            VStack(alignment: .leading, spacing: 2) {
                Text(saved?.displayName ?? pc.name)
                    .font(.body.weight(.semibold))
                    .lineLimit(1)
                Text(pc.isCompatible
                     ? DeviceID.format(pc.deviceID)
                     : "\(DeviceID.format(pc.deviceID)) · different OpenView version")
                    .font(.subheadline.monospacedDigit())
                    .foregroundColor(.secondary)
                    .lineLimit(1)
            }
            Spacer(minLength: 0)
            if resolving {
                ProgressView()
            } else if saved?.hasPIN == true {
                Image(systemName: "key.fill")
                    .font(.caption)
                    .foregroundColor(.secondary)
                    .accessibilityLabel("PIN saved")
            }
        }
        .padding(.vertical, 2)
        .contentShape(Rectangle())
        .accessibilityElement(children: .combine)
        .accessibilityHint("Connects over the local network")
    }
}

/// One-tap reconnect to the most recent device with a saved PIN.
struct ReconnectCard: View {
    let connection: SavedConnection
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: 14) {
                ZStack {
                    Circle()
                        .fill(Color.accentColor)
                        .frame(width: 44, height: 44)
                    Image(systemName: "arrow.clockwise")
                        .font(.system(size: 18, weight: .semibold))
                        .foregroundColor(.white)
                }
                VStack(alignment: .leading, spacing: 2) {
                    Text("Reconnect")
                        .font(.caption.weight(.semibold))
                        .foregroundColor(.accentColor)
                        .textCase(.uppercase)
                    Text(connection.displayName)
                        .font(.headline)
                        .foregroundColor(.primary)
                        .lineLimit(1)
                    Text(connection.subtitle)
                        .font(.subheadline)
                        .foregroundColor(.secondary)
                        .lineLimit(1)
                }
                Spacer(minLength: 0)
                Image(systemName: "key.fill")
                    .foregroundColor(.secondary)
            }
            .padding(.vertical, 6)
            .contentShape(Rectangle())
        }
        .accessibilityLabel("Reconnect to \(connection.displayName)")
    }
}

struct EmptyConnectionsView: View {
    var body: some View {
        VStack(spacing: 10) {
            Image(systemName: "desktopcomputer")
                .font(.system(size: 40))
                .foregroundColor(.secondary)
            Text("No saved connections")
                .font(.headline)
            Text("Connect with a device ID and PIN — the device is remembered here for one-tap reconnects.")
                .font(.subheadline)
                .foregroundColor(.secondary)
                .multilineTextAlignment(.center)
        }
        .frame(maxWidth: .infinity)
        .padding(.vertical, 20)
    }
}

// MARK: - PIN prompt

struct PinPromptView: View {
    let title: String
    let subtitle: String
    let rememberDefault: Bool
    let onSubmit: (String, Bool) -> Void
    let onCancel: () -> Void

    @State private var pin = ""
    @State private var remember: Bool
    @FocusState private var focused: Bool

    init(title: String, subtitle: String, rememberDefault: Bool,
         onSubmit: @escaping (String, Bool) -> Void, onCancel: @escaping () -> Void) {
        self.title = title
        self.subtitle = subtitle
        self.rememberDefault = rememberDefault
        self.onSubmit = onSubmit
        self.onCancel = onCancel
        _remember = State(initialValue: rememberDefault)
    }

    var body: some View {
        NavigationView {
            Form {
                Section(header: Text(subtitle)) {
                    SecureField("PIN", text: $pin)
                        .focused($focused)
                        .submitLabel(.go)
                        .onSubmit(submit)
                    Toggle("Remember PIN", isOn: $remember)
                }
                Section(footer: Text("Saved PINs stay in this device's Keychain and never leave it.")) {
                    Button(action: submit) {
                        Text("Connect")
                            .frame(maxWidth: .infinity)
                    }
                    .disabled(pin.isEmpty)
                }
            }
            .navigationTitle(title)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel", action: onCancel)
                }
            }
            .onAppear {
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) {
                    focused = true
                }
            }
        }
        .navigationViewStyle(.stack)
    }

    private func submit() {
        guard !pin.isEmpty else { return }
        onSubmit(pin, remember)
    }
}

// MARK: - Pairing

/// The pairing-code flow (one-off sessions, never saved): the host's
/// one-time code + transport. Codes are 10 digits (`1234 567890`): the first
/// four name the relay slot, the rest is the secret (never sent).
struct PairView: View {
    @EnvironmentObject private var app: AppModel
    let onDone: () -> Void

    @State private var code = ""
    @State private var useLan = false
    @State private var relayURL = AppSettings.defaultRelayURL
    @State private var lanHost = "192.168.1.10"
    @State private var lanPort = String(ConnectionTransport.defaultLanPort)

    private var codeBinding: Binding<String> {
        Binding(get: { code }, set: { code = String(DeviceID.digits($0).prefix(Self.codeDigits)) })
    }

    /// Digits in a pairing code (`ovcore::pairing::CODE_DIGITS`).
    static let codeDigits = 10

    private var canConnect: Bool {
        guard code.count == Self.codeDigits else { return false }
        if useLan {
            return UInt16(lanPort) != nil && !lanHost.isEmpty
        }
        return RelayAddress.isValid(relayURL)
    }

    var body: some View {
        NavigationView {
            Form {
                Section(header: Text("Partner's pairing code"),
                        footer: Text("The 10-digit one-time code shown on the host.")) {
                    TextField("10-digit code", text: codeBinding)
                        .keyboardType(.numberPad)
                        .font(.title3.monospacedDigit())
                }
                Section(header: Text("Connection")) {
                    Picker("Connection", selection: $useLan) {
                        Text("Relay").tag(false)
                        Text("LAN").tag(true)
                    }
                    .pickerStyle(.segmented)
                    if useLan {
                        TextField("Host IP address", text: $lanHost)
                            .keyboardType(.decimalPad)
                            .disableAutocorrection(true)
                            .textInputAutocapitalization(.never)
                        TextField("UDP port", text: $lanPort)
                            .keyboardType(.numberPad)
                    } else {
                        TextField("wss://relay.example.com/relay", text: $relayURL)
                            .keyboardType(.URL)
                            .disableAutocorrection(true)
                            .textInputAutocapitalization(.never)
                        if !RelayAddress.isValid(relayURL) {
                            Text(relayURL.trimmingCharacters(in: .whitespaces).isEmpty
                                 ? "No relay server configured: enter one, or use LAN."
                                 : "Use wss://host/relay or host:port.")
                                .font(.caption)
                                .foregroundColor(.secondary)
                        }
                    }
                }
                Section {
                    Button(action: connect) {
                        Text("Connect")
                            .frame(maxWidth: .infinity)
                    }
                    .disabled(!canConnect)
                }
            }
            .navigationTitle("Pair with Code")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel", action: onDone)
                }
            }
        }
        .navigationViewStyle(.stack)
    }

    private func connect() {
        guard canConnect, !app.sessionActive else { return }
        let transport: ConnectionTransport = useLan
            ? .lan(host: lanHost.trimmingCharacters(in: .whitespaces),
                   port: UInt16(lanPort) ?? ConnectionTransport.defaultLanPort)
            : .relay(relayURL.trimmingCharacters(in: .whitespaces))
        let req = ConnectRequest(name: "",
                                 target: .pairing(code: code),
                                 transport: transport,
                                 defaultFps: AppSettings.defaultQualityFps)
        onDone()
        app.connect(req)
    }
}

// MARK: - New / edit connection

struct ConnectionEditView: View {
    @EnvironmentObject private var app: AppModel
    let existing: SavedConnection?
    let onCancel: () -> Void
    let onSaved: (SavedConnection) -> Void
    let onSaveAndConnect: (SavedConnection, String?, Bool) -> Void

    @State private var name: String
    @State private var deviceID: String
    @State private var pin = ""
    @State private var remember: Bool
    @State private var useLan: Bool
    @State private var relayURL: String
    @State private var lanHost: String
    @State private var lanPort: String
    @State private var fps: UInt16
    @State private var favorite: Bool

    init(existing: SavedConnection?,
         onCancel: @escaping () -> Void,
         onSaved: @escaping (SavedConnection) -> Void,
         onSaveAndConnect: @escaping (SavedConnection, String?, Bool) -> Void) {
        self.existing = existing
        self.onCancel = onCancel
        self.onSaved = onSaved
        self.onSaveAndConnect = onSaveAndConnect
        let t = existing?.transport ?? ConnectionTransport.relay(AppSettings.defaultRelayURL)
        _name = State(initialValue: existing?.name ?? "")
        _deviceID = State(initialValue: existing?.deviceId ?? "")
        _remember = State(initialValue: existing?.hasPIN ?? true)
        _useLan = State(initialValue: t.kind == .lan)
        _relayURL = State(initialValue: t.relayURL.isEmpty ? AppSettings.defaultRelayURL : t.relayURL)
        _lanHost = State(initialValue: t.lanHost)
        _lanPort = State(initialValue: String(t.lanPort))
        _fps = State(initialValue: existing?.defaultFps ?? AppSettings.defaultQualityFps)
        _favorite = State(initialValue: existing?.isFavorite ?? false)
    }

    private var deviceIDBinding: Binding<String> {
        Binding(get: { DeviceID.format(deviceID) },
                set: { deviceID = String(DeviceID.digits($0).prefix(18)) })
    }

    private var isValid: Bool {
        guard UInt64(deviceID) != nil else { return false }
        if useLan {
            return !lanHost.trimmingCharacters(in: .whitespaces).isEmpty && (UInt16(lanPort) ?? 0) > 0
        }
        return RelayAddress.isValid(relayURL)
    }

    private var pinPlaceholder: String {
        existing?.hasPIN == true ? "Saved — leave empty to keep" : "PIN"
    }

    var body: some View {
        NavigationView {
            Form {
                Section(header: Text("Device")) {
                    TextField("Name (optional)", text: $name)
                    TextField("Device ID", text: deviceIDBinding)
                        .keyboardType(.numberPad)
                        .font(.body.monospacedDigit())
                    Toggle("Favorite", isOn: $favorite)
                }
                Section(header: Text("Access PIN")) {
                    SecureField(pinPlaceholder, text: $pin)
                    Toggle("Remember PIN", isOn: $remember)
                }
                Section(header: Text("Connection")) {
                    Picker("Connection", selection: $useLan) {
                        Text("Relay").tag(false)
                        Text("LAN").tag(true)
                    }
                    .pickerStyle(.segmented)
                    if !useLan {
                        TextField("wss://relay.example.com/relay", text: $relayURL)
                            .keyboardType(.URL)
                            .disableAutocorrection(true)
                            .textInputAutocapitalization(.never)
                        if !RelayAddress.isValid(relayURL) {
                            Text("Relay server: wss://host/relay or host:port (set a default in Settings).")
                                .font(.caption)
                                .foregroundColor(.secondary)
                        }
                    }
                }
                if useLan {
                    Section(header: Text("Advanced"),
                            footer: Text("Direct UDP to the host on your local network.")) {
                        TextField("Host IP address", text: $lanHost)
                            .keyboardType(.decimalPad)
                            .disableAutocorrection(true)
                            .textInputAutocapitalization(.never)
                        TextField("UDP port", text: $lanPort)
                            .keyboardType(.numberPad)
                    }
                }
                Section(header: Text("Default quality")) {
                    Picker("Quality", selection: $fps) {
                        ForEach(QualityChoice.all) { choice in
                            Text(choice.label).tag(choice.fps)
                        }
                    }
                    .pickerStyle(.segmented)
                }
                Section {
                    Button {
                        let c = save()
                        onSaveAndConnect(c, pin.isEmpty ? nil : pin, remember)
                    } label: {
                        Text("Save & Connect")
                            .frame(maxWidth: .infinity)
                    }
                    .disabled(!isValid || app.sessionActive)
                }
            }
            .navigationTitle(existing == nil ? "New Connection" : "Edit Connection")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel", action: onCancel)
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Save") {
                        onSaved(save())
                    }
                    .disabled(!isValid)
                }
            }
        }
        .navigationViewStyle(.stack)
    }

    /// Persist the form (PIN to the Keychain when given + remembered).
    private func save() -> SavedConnection {
        var c = existing ?? SavedConnection(name: "", deviceId: deviceID,
                                            transport: .relay(AppSettings.defaultRelayURL))
        if c.deviceId != deviceID {
            // A different machine: the pinned identity no longer applies.
            c.pinnedFingerprint = nil
        }
        c.name = name.trimmingCharacters(in: .whitespacesAndNewlines)
        c.deviceId = deviceID
        if useLan {
            c.transport = .lan(host: lanHost.trimmingCharacters(in: .whitespaces),
                               port: UInt16(lanPort) ?? ConnectionTransport.defaultLanPort)
        } else {
            c.transport = .relay(relayURL.trimmingCharacters(in: .whitespaces))
        }
        c.defaultFps = fps
        c.isFavorite = favorite
        return app.save(c, pin: pin.isEmpty ? nil : pin, remember: remember)
    }
}
