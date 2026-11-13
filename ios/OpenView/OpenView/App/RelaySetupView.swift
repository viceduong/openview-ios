import SwiftUI

/// First-run sheet: asks for the relay server new connections use.
///
/// OpenView ships without a relay; sessions to hosts outside the local
/// network go through a relay you run yourself (`ovrelay`). The address is
/// stored in the regular "Default relay URL" setting, so it can be changed
/// later in Settings > Connection. Shown once (Home, on first launch) unless
/// the build bakes in a relay (`OVDefaultRelayURL`).
struct RelaySetupView: View {
    let onDone: () -> Void

    @State private var address = ""
    @FocusState private var focused: Bool

    private var trimmed: String {
        address.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    private var isValid: Bool {
        RelayAddress.isValid(address)
    }

    private var hint: String {
        if trimmed.isEmpty || isValid {
            return "Use wss://host/relay (WebSocket, works behind HTTPS proxies) or host:port (TCP, default port 4569)."
        }
        return "Not a relay address. Use wss://host/relay, ws://host/relay or host:port."
    }

    var body: some View {
        NavigationView {
            Form {
                Section {
                    VStack(alignment: .leading, spacing: 8) {
                        Image(systemName: "network")
                            .font(.system(size: 32, weight: .semibold))
                            .foregroundColor(.accentColor)
                        Text("Connect through your relay")
                            .font(.headline)
                        Text("To reach computers outside your local network, OpenView connects through a relay server that you run (ovrelay). Sessions are end-to-end encrypted; the relay only forwards traffic.")
                            .font(.subheadline)
                            .foregroundColor(.secondary)
                    }
                    .padding(.vertical, 6)
                }
                Section(header: Text("Relay server"), footer: Text(hint)) {
                    TextField("wss://relay.example.com/relay", text: $address)
                        .keyboardType(.URL)
                        .disableAutocorrection(true)
                        .textInputAutocapitalization(.never)
                        .focused($focused)
                        .submitLabel(.done)
                        .onSubmit(save)
                }
                Section(footer: Text("You can change this any time in Settings > Connection.")) {
                    Button(action: save) {
                        Text("Save")
                            .frame(maxWidth: .infinity)
                    }
                    .disabled(!isValid)
                    Button(action: skip) {
                        Text("Skip — LAN / direct only")
                            .frame(maxWidth: .infinity)
                    }
                }
            }
            .navigationTitle("Relay Server")
            .navigationBarTitleDisplayMode(.inline)
        }
        .navigationViewStyle(.stack)
        // Swiping the sheet away would neither save nor skip.
        .interactiveDismissDisabled()
    }

    private func save() {
        guard isValid else { return }
        let defaults = UserDefaults.standard
        defaults.set(trimmed, forKey: SettingsKey.defaultRelayURL)
        defaults.set(true, forKey: SettingsKey.relaySetupDone)
        onDone()
    }

    private func skip() {
        UserDefaults.standard.set(true, forKey: SettingsKey.relaySetupDone)
        onDone()
    }
}
