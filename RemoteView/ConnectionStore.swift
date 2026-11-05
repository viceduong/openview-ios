import Foundation
import LocalAuthentication
import Security

// Remembered connections (device ID + transport + per-device preferences),
// persisted as JSON in Application Support, with the access PINs kept in
// the Keychain (never in the JSON). Pairing-code sessions are one-off and
// never saved: only device-ID (unattended) connections are remembered.

// MARK: - Transport

/// How a saved connection reaches its host.
struct ConnectionTransport: Codable, Equatable {
    enum Kind: String, Codable {
        case relay
        case lan
    }

    var kind: Kind
    /// Relay: `wss://host/relay` (WebSocket) or `host[:port]` (TCP, 4569).
    var relayURL: String
    /// LAN (direct UDP).
    var lanHost: String
    var lanPort: UInt16

    static let defaultLanPort: UInt16 = 4570
    static let defaultRelayPort: UInt16 = 4569

    static func relay(_ url: String) -> ConnectionTransport {
        ConnectionTransport(kind: .relay, relayURL: url, lanHost: "", lanPort: defaultLanPort)
    }

    static func lan(host: String, port: UInt16) -> ConnectionTransport {
        ConnectionTransport(kind: .lan, relayURL: AppSettings.defaultRelayURL, lanHost: host, lanPort: port)
    }

    /// "Relay" / "LAN" (subtitles, stats).
    var label: String {
        kind == .lan ? "LAN" : "Relay"
    }

    /// Longer description for the stats details.
    var detail: String {
        switch kind {
        case .lan: return "LAN \(lanHost):\(lanPort)"
        case .relay: return "Relay \(relayURL)"
        }
    }

    /// The concrete dial mode; throws `badArg` for unusable settings.
    func connectMode() throws -> ConnectMode {
        switch kind {
        case .lan:
            let host = lanHost.trimmingCharacters(in: .whitespacesAndNewlines)
            guard !host.isEmpty, lanPort > 0 else { throw OvSessionBox.FfiError.badArg }
            return .direct(host: host, port: lanPort)
        case .relay:
            let text = relayURL.trimmingCharacters(in: .whitespacesAndNewlines)
            let lower = text.lowercased()
            if lower.hasPrefix("wss://") || lower.hasPrefix("ws://") {
                guard let url = URL(string: text) else { throw OvSessionBox.FfiError.badArg }
                return .relayWebSocket(url: url)
            }
            guard !text.isEmpty else { throw OvSessionBox.FfiError.badArg }
            // "host" or "host:port" (a bare IPv6 address has several colons
            // and is taken as a host).
            let parts = text.split(separator: ":", omittingEmptySubsequences: false)
            if parts.count == 2, let port = UInt16(parts[1]), !parts[0].isEmpty {
                return .relay(host: String(parts[0]), port: port)
            }
            return .relay(host: text, port: Self.defaultRelayPort)
        }
    }
}

// MARK: - Saved connection

struct SavedConnection: Codable, Identifiable, Equatable {
    var id: UUID
    /// User-chosen name ("" = show the formatted device ID).
    var name: String
    /// Host device ID, digits only.
    var deviceId: String
    var transport: ConnectionTransport
    /// TOFU: the host fingerprint (pine string) seen on the first session.
    var pinnedFingerprint: String?
    var lastConnected: Date?
    var isFavorite: Bool
    /// Target fps (0 = Auto).
    var defaultFps: UInt16
    var lastMonitor: Int
    /// SF Symbol shown in lists.
    var symbol: String
    /// A PIN is stored in the Keychain for this connection.
    var hasPIN: Bool

    init(id: UUID = UUID(),
         name: String,
         deviceId: String,
         transport: ConnectionTransport,
         pinnedFingerprint: String? = nil,
         lastConnected: Date? = nil,
         isFavorite: Bool = false,
         defaultFps: UInt16 = 0,
         lastMonitor: Int = 0,
         symbol: String = "desktopcomputer",
         hasPIN: Bool = false) {
        self.id = id
        self.name = name
        self.deviceId = deviceId
        self.transport = transport
        self.pinnedFingerprint = pinnedFingerprint
        self.lastConnected = lastConnected
        self.isFavorite = isFavorite
        self.defaultFps = defaultFps
        self.lastMonitor = lastMonitor
        self.symbol = symbol
        self.hasPIN = hasPIN
    }

    enum CodingKeys: String, CodingKey {
        case id, name, deviceId, transport, pinnedFingerprint, lastConnected
        case isFavorite, defaultFps, lastMonitor, symbol, hasPIN
    }

    /// Tolerant decoding: fields added later default instead of failing;
    /// keys no longer used (e.g. the removed per-connection "inputMode")
    /// are ignored and dropped on the next save.
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(UUID.self, forKey: .id)
        name = try c.decodeIfPresent(String.self, forKey: .name) ?? ""
        deviceId = try c.decode(String.self, forKey: .deviceId)
        transport = try c.decodeIfPresent(ConnectionTransport.self, forKey: .transport)
            ?? ConnectionTransport.relay(builtInRelayURL)
        pinnedFingerprint = try c.decodeIfPresent(String.self, forKey: .pinnedFingerprint)
        lastConnected = try c.decodeIfPresent(Date.self, forKey: .lastConnected)
        isFavorite = try c.decodeIfPresent(Bool.self, forKey: .isFavorite) ?? false
        defaultFps = try c.decodeIfPresent(UInt16.self, forKey: .defaultFps) ?? 0
        lastMonitor = try c.decodeIfPresent(Int.self, forKey: .lastMonitor) ?? 0
        symbol = try c.decodeIfPresent(String.self, forKey: .symbol) ?? "desktopcomputer"
        hasPIN = try c.decodeIfPresent(Bool.self, forKey: .hasPIN) ?? false
    }

    var displayName: String {
        let trimmed = name.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? DeviceID.format(deviceId) : trimmed
    }

    /// "123 456 789 · Relay · 2h ago".
    var subtitle: String {
        var parts = [DeviceID.format(deviceId), transport.label]
        if let lastConnected {
            parts.append(DisplayTime.relative(lastConnected))
        }
        return parts.joined(separator: " · ")
    }
}

// MARK: - Store

/// The saved connections, persisted on every change. Main thread only.
final class ConnectionStore: ObservableObject {
    @Published private(set) var connections: [SavedConnection] = []
    private let fileURL: URL?

    init() {
        if let base = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first {
            fileURL = base.appendingPathComponent("OpenView", isDirectory: true)
                .appendingPathComponent("connections.json")
        } else {
            fileURL = nil
        }
        load()
    }

    // MARK: Queries

    func connection(id: UUID) -> SavedConnection? {
        connections.first { $0.id == id }
    }

    /// The first connection to that device ID (digits compared).
    func find(deviceId: String) -> SavedConnection? {
        let digits = DeviceID.digits(deviceId)
        return connections.first { $0.deviceId == digits }
    }

    var favorites: [SavedConnection] {
        connections.filter { $0.isFavorite }.sorted {
            $0.displayName.localizedCaseInsensitiveCompare($1.displayName) == .orderedAscending
        }
    }

    /// Non-favorites, most recently used first.
    var recents: [SavedConnection] {
        connections.filter { !$0.isFavorite }.sorted {
            ($0.lastConnected ?? .distantPast) > ($1.lastConnected ?? .distantPast)
        }
    }

    /// The most recently used connection with a stored PIN (one-tap
    /// reconnect on Home).
    var mostRecentWithPIN: SavedConnection? {
        connections.filter { $0.hasPIN && $0.lastConnected != nil }.max {
            ($0.lastConnected ?? .distantPast) < ($1.lastConnected ?? .distantPast)
        }
    }

    // MARK: Mutations

    /// Insert or replace (by id).
    func upsert(_ connection: SavedConnection) {
        if let i = connections.firstIndex(where: { $0.id == connection.id }) {
            connections[i] = connection
        } else {
            connections.append(connection)
        }
        persist()
    }

    func update(id: UUID, _ change: (inout SavedConnection) -> Void) {
        guard let i = connections.firstIndex(where: { $0.id == id }) else { return }
        var c = connections[i]
        change(&c)
        connections[i] = c
        persist()
    }

    /// Remove the connection and its stored PIN.
    func delete(id: UUID) {
        KeychainPIN.delete(for: id)
        connections.removeAll { $0.id == id }
        persist()
    }

    /// Forget every stored PIN.
    func forgetAllPINs() {
        KeychainPIN.deleteAll()
        for i in connections.indices {
            connections[i].hasPIN = false
        }
        persist()
    }

    // MARK: Persistence

    private func load() {
        guard let url = fileURL, let data = try? Data(contentsOf: url) else { return }
        let decoder = JSONDecoder()
        decoder.dateDecodingStrategy = .iso8601
        if let list = try? decoder.decode([SavedConnection].self, from: data) {
            connections = list
        }
    }

    private func persist() {
        guard let url = fileURL else { return }
        let encoder = JSONEncoder()
        encoder.dateEncodingStrategy = .iso8601
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
        do {
            try FileManager.default.createDirectory(at: url.deletingLastPathComponent(),
                                                    withIntermediateDirectories: true)
            let data = try encoder.encode(connections)
            try data.write(to: url, options: [.atomic])
        } catch {
            NSLog("OpenView: saving connections failed: \(error)")
        }
    }
}

// MARK: - Keychain

/// Access PINs in the Keychain: generic passwords, service
/// "com.openview.pin", account = the connection's UUID, this device only
/// and only while unlocked. With "Require Face ID for saved PINs" on, new
/// items are protected by `.userPresence` (Face ID / Touch ID / passcode
/// on every read).
enum KeychainPIN {
    static let service = "com.openview.pin"

    private static func baseQuery(_ id: UUID) -> [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: id.uuidString,
        ]
    }

    /// Store (replace) the PIN. Returns whether it was saved.
    @discardableResult
    static func save(_ pin: String, for id: UUID, requireUserPresence: Bool) -> Bool {
        delete(for: id)
        var query = baseQuery(id)
        query[kSecValueData as String] = Data(pin.utf8)
        if requireUserPresence,
           let access = SecAccessControlCreateWithFlags(nil,
                                                        kSecAttrAccessibleWhenUnlockedThisDeviceOnly,
                                                        .userPresence,
                                                        nil) {
            query[kSecAttrAccessControl as String] = access
        } else {
            query[kSecAttrAccessible as String] = kSecAttrAccessibleWhenUnlockedThisDeviceOnly
        }
        return SecItemAdd(query as CFDictionary, nil) == errSecSuccess
    }

    /// Read the PIN off main (a protected item shows the Face ID prompt and
    /// blocks the reading thread); `completion` runs on main, nil when
    /// missing or the user cancelled.
    static func load(for id: UUID, reason: String, completion: @escaping (String?) -> Void) {
        DispatchQueue.global(qos: .userInitiated).async {
            let context = LAContext()
            context.localizedReason = reason
            var query = KeychainPIN.baseQuery(id)
            query[kSecReturnData as String] = true
            query[kSecMatchLimit as String] = kSecMatchLimitOne
            query[kSecUseAuthenticationContext as String] = context
            var result: CFTypeRef?
            let status = SecItemCopyMatching(query as CFDictionary, &result)
            var pin: String?
            if status == errSecSuccess, let data = result as? Data {
                pin = String(data: data, encoding: .utf8)
            }
            DispatchQueue.main.async {
                completion(pin)
            }
        }
    }

    static func delete(for id: UUID) {
        SecItemDelete(baseQuery(id) as CFDictionary)
    }

    static func deleteAll() {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
        ]
        SecItemDelete(query as CFDictionary)
    }
}
