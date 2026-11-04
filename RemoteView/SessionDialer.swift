import Foundation

// What to connect to (ConnectRequest) and how to dial it (SessionDialer).
// Dialing runs on the session queue inside SessionController, so a
// blocking TCP relay connect never stalls the UI, and a reconnect simply
// dials the same request again.

/// How the session reaches its peer.
enum ConnectMode {
    case relay(host: String, port: UInt16)
    /// Relay over WebSocket (`wss://host/relay`) — reaches ovrelay through
    /// an HTTP-only front such as a Cloudflare tunnel.
    case relayWebSocket(url: URL)
    case direct(host: String, port: UInt16)
}

/// One connection attempt's parameters (also used to rebuild the session
/// on reconnect).
struct ConnectRequest {
    enum Target {
        /// Unattended access (v0.22): host device ID + access PIN.
        case device(id: UInt64, pin: String)
        /// One-off pairing with the code shown on the host.
        case pairing(code: String)
    }

    /// The saved connection this came from (nil = not saved yet).
    var connectionID: UUID?
    /// Display name ("" = formatted device ID / "Pairing session").
    var name: String
    var target: Target
    var transport: ConnectionTransport
    /// Store the PIN in the Keychain once the session is established.
    var rememberPIN: Bool
    /// The PIN came from the Keychain (no need to store it again).
    var pinIsStored: Bool
    /// Target fps to request once established (0 = Auto).
    var defaultFps: UInt16
    /// Monitor to switch to once the host announces its monitors.
    var lastMonitor: Int
    var inputMode: InputMode?
    /// TOFU: the fingerprint the host must present (nil = first contact).
    var pinnedFingerprint: String?

    init(connectionID: UUID? = nil,
         name: String,
         target: Target,
         transport: ConnectionTransport,
         rememberPIN: Bool = false,
         pinIsStored: Bool = false,
         defaultFps: UInt16 = 0,
         lastMonitor: Int = 0,
         inputMode: InputMode? = nil,
         pinnedFingerprint: String? = nil) {
        self.connectionID = connectionID
        self.name = name
        self.target = target
        self.transport = transport
        self.rememberPIN = rememberPIN
        self.pinIsStored = pinIsStored
        self.defaultFps = defaultFps
        self.lastMonitor = lastMonitor
        self.inputMode = inputMode
        self.pinnedFingerprint = pinnedFingerprint
    }

    var isPairing: Bool {
        if case .pairing = target { return true }
        return false
    }

    var deviceID: UInt64? {
        if case .device(let id, _) = target { return id }
        return nil
    }

    var displayName: String {
        let trimmed = name.trimmingCharacters(in: .whitespacesAndNewlines)
        if !trimmed.isEmpty { return trimmed }
        switch target {
        case .device(let id, _): return DeviceID.format(String(id))
        case .pairing(let code): return "Pairing \(code)"
        }
    }
}

/// Builds and dials one OvSessionBox for a request. Session queue only.
enum SessionDialer {
    struct Result {
        let session: OvSessionBox
        let mode: ConnectMode
        /// The open WebSocket of a `.relayWebSocket` session.
        let webSocket: RelayWebSocket?
    }

    /// Create the session, connect its transport and begin the handshake
    /// (PIN mode for a device ID, pairing mode for a code). The relay TCP
    /// connect blocks (Rust); the WebSocket queues CONNECT until open;
    /// direct mode only creates the datagram carrier — SessionController
    /// installs the NwUdpBridge before the first pump.
    static func dial(_ req: ConnectRequest) throws -> Result {
        let mode = try req.transport.connectMode()
        let code: String
        let sessionID: UInt32
        switch req.target {
        case .device(let id, _):
            // Relay session id = the host device id's low 32 bits, matching
            // ovhost --unattended / ovview --connect.
            code = "000000"
            sessionID = UInt32(truncatingIfNeeded: id)
        case .pairing(let c):
            code = c
            sessionID = OvSessionBox.relaySessionID(code: c)
        }
        let session = try OvSessionBox(isServer: false, pairingCode: code)
        var socket: RelayWebSocket?
        switch mode {
        case .relay(let host, let port):
            try session.relayConnect(host: host, port: port, sessionID: sessionID)
        case .relayWebSocket(let url):
            // Inbound messages are delivered on the session queue.
            let ws = RelayWebSocket(url: url, queue: OvQueues.session)
            do {
                try session.relayConnectStream(sessionID: sessionID) { [weak ws] data in
                    ws?.send(data)
                }
            } catch {
                ws.close()
                throw error
            }
            socket = ws
        case .direct:
            // The datagram carrier; the real outbound sink is installed by
            // SessionController before pumping.
            try session.setDatagramSink { _ in }
        }
        do {
            switch req.target {
            case .device(_, let pin):
                try session.beginPin(pin)
            case .pairing:
                try session.begin()
            }
        } catch {
            socket?.close()
            throw error
        }
        return Result(session: session, mode: mode, webSocket: socket)
    }

    /// User-facing text for a dial failure.
    static func describe(_ error: Error) -> String {
        if let ffi = error as? OvSessionBox.FfiError {
            switch ffi {
            case .badArg: return "The connection settings are not valid."
            case .transport: return "Couldn't reach the relay server. Check the relay address and your network."
            case .closed: return "The relay closed the connection."
            case .crypto: return "The secure handshake failed."
            case .wrongState: return "The session could not be started."
            }
        }
        return error.localizedDescription
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
