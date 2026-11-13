import Foundation

// Parsing / validation of the relay server address. Self-contained (no app
// dependencies) so the unit tests compile it directly.

/// A relay server address as the user types it: `wss://host/relay` or
/// `ws://host/relay` (relay over WebSocket), or `host[:port]` (relay over
/// TCP, default port 4569; IPv6 as `[addr]:port` or a bare address).
enum RelayAddress: Equatable {
    case webSocket(URL)
    case tcp(host: String, port: UInt16)

    /// ovrelay's default TCP port.
    static let defaultTCPPort: UInt16 = 4569

    /// The parsed address, or nil when `text` is empty or not a usable
    /// relay address. Surrounding whitespace is ignored.
    static func parse(_ text: String) -> RelayAddress? {
        let t = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !t.isEmpty, !t.contains(where: { $0.isWhitespace }) else { return nil }
        let lower = t.lowercased()
        if lower.hasPrefix("wss://") || lower.hasPrefix("ws://") {
            guard let url = URL(string: t), let host = url.host, !host.isEmpty else { return nil }
            return .webSocket(url)
        }
        // Any other scheme or a path is not a TCP relay address.
        guard !t.contains("://"), !t.contains("/") else { return nil }
        if t.hasPrefix("[") {
            // "[v6]" or "[v6]:port".
            guard let close = t.firstIndex(of: "]") else { return nil }
            let host = String(t[t.index(after: t.startIndex)..<close])
            guard !host.isEmpty else { return nil }
            let rest = t[t.index(after: close)...]
            if rest.isEmpty {
                return .tcp(host: host, port: defaultTCPPort)
            }
            guard rest.hasPrefix(":"), let port = UInt16(rest.dropFirst()), port > 0 else { return nil }
            return .tcp(host: host, port: port)
        }
        let parts = t.split(separator: ":", omittingEmptySubsequences: false)
        switch parts.count {
        case 1:
            return .tcp(host: t, port: defaultTCPPort)
        case 2:
            guard !parts[0].isEmpty, let port = UInt16(parts[1]), port > 0 else { return nil }
            return .tcp(host: String(parts[0]), port: port)
        default:
            // Several colons: a bare IPv6 address.
            return .tcp(host: t, port: defaultTCPPort)
        }
    }

    /// Whether `text` is a usable relay address.
    static func isValid(_ text: String) -> Bool {
        parse(text) != nil
    }
}
