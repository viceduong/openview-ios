import SwiftUI

// Landing screen: shows our pairing code, lets the user enter the peer's
// code + relay endpoint, or dial over the LAN directly (UDP).

/// How the session reaches its peer.
enum ConnectMode {
    case relay(host: String, port: UInt16)
    case direct(host: String, port: UInt16)
}

struct ConnectView: View {
    @State private var peerCode = ""
    @State private var peerId = ""
    @State private var pin = ""
    @State private var relayHost = "127.0.0.1"
    @State private var relayPort = "4569"
    @State private var peerHost = "192.168.1.10"
    @State private var peerPort = "4570"
    @State private var useLan = false
    @State private var session: OvSessionBox?
    @State private var errorText: String?

    var body: some View {
        NavigationView {
            Form {
                Section("Your pairing code") {
                    Text(PairingState.shared.code)
                        .font(.system(.title2, design: .monospaced))
                        .frame(maxWidth: .infinity, alignment: .center)
                }
                Section("Peer") {
                    TextField("Peer pairing code", text: $peerCode)
                        .keyboardType(.numberPad)
                    // v0.22 unattended access: dial by the host's stable
                    // device id + access PIN instead of a pairing code.
                    TextField("Or device ID (unattended)", text: $peerId)
                        .keyboardType(.numberPad)
                    SecureField("Access PIN (with device ID)", text: $pin)
                    Picker("Connection", selection: $useLan) {
                        Text("Relay").tag(false)
                        Text("Direct (LAN UDP)").tag(true)
                    }
                    .pickerStyle(.segmented)
                    if useLan {
                        TextField("Peer host (IP)", text: $peerHost)
                            .disableAutocorrection(true)
                            .textInputAutocapitalization(.never)
                            .keyboardType(.decimalPad)
                        TextField("Peer UDP port", text: $peerPort)
                            .keyboardType(.numberPad)
                    } else {
                        TextField("Relay host", text: $relayHost)
                            .disableAutocorrection(true)
                            .textInputAutocapitalization(.never)
                        TextField("Relay port", text: $relayPort)
                            .keyboardType(.numberPad)
                    }
                }
                Section {
                    Button("Connect") { connect() }
                        .disabled(peerCode.count != 6
                                  && !(UInt64(peerId) != nil && !pin.isEmpty))
                    if let errorText {
                        Text(errorText).foregroundStyle(.red).font(.footnote)
                    }
                }
            }
            .navigationTitle("OpenView")
            // Hidden NavigationLink lives INSIDE the Form (a second child of
            // NavigationView breaks the iPad two-column layout), and the
            // view is forced to a single column.
            .background(NavigationLink(isActive: Binding(
                get: { session != nil },
                set: { if !$0 { session = nil } }
            )) {
                if let session {
                    SessionView(session: session)
                }
            } label: {
                EmptyView()
            }
            .hidden())
        }
        .navigationViewStyle(.stack)
    }

    private func connect() {
        errorText = nil
        do {
            // Unattended path (v0.22): a device ID + PIN connects with PIN
            // auth, no pairing. Otherwise the pairing-code path as before.
            if !peerId.isEmpty {
                guard let devId = UInt64(peerId), !pin.isEmpty else {
                    throw OvSessionBox.FfiError.badArg
                }
                let s = try OvSessionBox(isServer: false, pairingCode: "000000")
                let mode: ConnectMode
                if useLan {
                    guard let port = UInt16(peerPort), !peerHost.isEmpty else {
                        throw OvSessionBox.FfiError.badArg
                    }
                    mode = .direct(host: peerHost, port: port)
                } else {
                    mode = .relay(host: relayHost, port: UInt16(relayPort) ?? 4569)
                }
                switch mode {
                case .relay(let host, let port):
                    // Relay session id = the host device id's low 32 bits,
                    // matching ovhost --unattended / ovview --connect. The relay
                    // carrier is Rust-owned, so no datagram sink is needed.
                    try s.relayConnect(host: host, port: port,
                                       sessionID: UInt32(truncatingIfNeeded: devId))
                case .direct:
                    // Create the Datagram carrier. The real outbound sink
                    // (NwUdpBridge) is installed by SessionView.start() before
                    // pumping, so a placeholder closure is enough here.
                    try s.setDatagramSink { _ in }
                }
                try s.beginPin(pin)
                session = s
                DirectTransport.pending = mode
                return
            }
            // The client presents the peer's code; the server (this device,
            // when a peer dials us) presents its own. v0.7 shell: dial only.
            let s = try OvSessionBox(isServer: false, pairingCode: peerCode)
            let mode: ConnectMode
            if useLan {
                guard let port = UInt16(peerPort), !peerHost.isEmpty else {
                    throw OvSessionBox.FfiError.badArg
                }
                mode = .direct(host: peerHost, port: port)
            } else {
                mode = .relay(host: relayHost, port: UInt16(relayPort) ?? 4569)
            }
            switch mode {
            case .relay(let host, let port):
                // Rust-owned relay carrier — no datagram sink needed.
                try s.relayConnect(host: host, port: port,
                                   sessionID: OvSessionBox.relaySessionID(code: peerCode))
            case .direct:
                // Create the Datagram carrier; SessionView.start() installs the
                // real outbound sink (NwUdpBridge) before pumping.
                try s.setDatagramSink { _ in }
            }
            try s.begin()
            session = s
            // Hand the chosen transport to SessionView (direct mode dials there).
            DirectTransport.pending = mode
        } catch {
            errorText = "Connect failed: \(error)"
        }
    }
}

/// Lazily created identity + its pairing code for the app run.
enum PairingState {
    static let shared = PairingStateBox()
}

final class PairingStateBox {
    let identity: OpaquePointer?
    let code: String
    let pine: String

    init() {
        let p = ov_identity_create()
        identity = p
        // A failed identity allocation (returns null) degrades to empty
        // strings rather than crashing the app at launch.
        code = p.map { OvSessionBox.pairingCode(of: $0) } ?? ""
        pine = p.map { OvSessionBox.fingerprintPine(of: $0) } ?? ""
    }
}

/// Transport choice handed from ConnectView to SessionView. Set right before
/// navigation; SessionView consumes it in `start()` and clears it.
enum DirectTransport {
    static var pending: ConnectMode?
}
