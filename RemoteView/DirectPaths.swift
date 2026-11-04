import Foundation
import Network

// Section 5: the shell side of the direct (UDP) path next to a relay
// session. The Rust direct engine (ovffi) decides everything — which host
// candidates to probe, when to promote the session onto a path and when to
// fall back to the relay; this only owns one UDP NWConnection per candidate
// path and moves opaque datagrams:
//
//   open_path(path_id, "ip:port")  -> `open(pathID:address:)`
//   send sink(path_id, bytes)      -> `send(pathID:_:)`
//   inbound datagram on a path     -> `onReceive(pathID, bytes)`
//                                     (-> ov_session_on_direct_datagram)
//
// Every connection runs on `queue` (the session queue), so callbacks arrive
// there and the session is only touched from that one serial queue. All
// methods must be called on `queue`.
final class DirectPathSet {
    /// Where a path leads (shown as "Direct (LAN)" / "Direct (Internet)").
    enum Kind {
        case lan
        case internet

        var label: String {
            switch self {
            case .lan: return "LAN"
            case .internet: return "Internet"
            }
        }
    }

    private final class Path {
        let connection: NWConnection
        let address: String
        let kind: Kind
        var ready = false
        var dead = false
        /// Datagrams sent before `.ready` (bounded: probes are resent).
        var pending: [Data] = []

        init(connection: NWConnection, address: String, kind: Kind) {
            self.connection = connection
            self.address = address
            self.kind = kind
        }
    }

    private let queue: DispatchQueue
    private var onReceive: ((UInt32, Data) -> Void)?
    private var paths: [UInt32: Path] = [:]
    private var closed = false
    private static let maxPending = 64
    /// A host offers a handful of candidates; cap the sockets anyway.
    private static let maxPaths = 32

    init(queue: DispatchQueue, onReceive: @escaping (UInt32, Data) -> Void) {
        self.queue = queue
        self.onReceive = onReceive
    }

    /// Open path `pathID` to `address` ("ip:port" / "[v6]:port"). Returns
    /// false for an unparsable address (the engine then never hears back
    /// from that candidate and probes the others).
    @discardableResult
    func open(pathID: UInt32, address: String) -> Bool {
        guard !closed, paths[pathID] == nil, paths.count < Self.maxPaths,
              let parsed = Self.parse(address),
              let nwPort = NWEndpoint.Port(rawValue: parsed.port) else { return false }
        let host = parsed.host
        let endpoint = NWEndpoint.hostPort(host: NWEndpoint.Host(host), port: nwPort)
        let params = NWParameters.udp
        let connection = NWConnection(to: endpoint, using: params)
        let path = Path(connection: connection, address: address, kind: Self.classify(host: host))
        paths[pathID] = path
        connection.stateUpdateHandler = { [weak self, weak path] state in
            // Already on `queue` (the connection's queue).
            guard let self, let path else { return }
            self.handleState(state, path: path, id: pathID)
        }
        connection.start(queue: queue)
        return true
    }

    /// Put one datagram on path `pathID` (buffered until it is ready).
    func send(pathID: UInt32, _ data: Data) {
        guard !closed, let path = paths[pathID], !path.dead else { return }
        if path.ready {
            path.connection.send(content: data, completion: .idempotent)
        } else if path.pending.count < Self.maxPending {
            path.pending.append(data)
        }
    }

    /// Run `body` (a pump that may emit many datagrams on the active path)
    /// as one NWConnection send batch on path `pathID` (if it is ready).
    func batch(pathID: Int64, _ body: () -> Void) {
        guard pathID >= 0, pathID <= Int64(UInt32.max),
              let path = paths[UInt32(pathID)], path.ready, !path.dead, !closed else {
            body()
            return
        }
        path.connection.batch(body)
    }

    /// LAN / Internet for path `pathID` (nil when unknown).
    func kind(of pathID: Int64) -> Kind? {
        guard pathID >= 0, pathID <= Int64(UInt32.max) else { return nil }
        return paths[UInt32(pathID)]?.kind
    }

    /// The candidate address of path `pathID` (for the session log).
    func address(of pathID: Int64) -> String? {
        guard pathID >= 0, pathID <= Int64(UInt32.max) else { return nil }
        return paths[UInt32(pathID)]?.address
    }

    /// Cancel every connection and drop the callback; idempotent.
    func closeAll() {
        guard !closed else { return }
        closed = true
        onReceive = nil
        for path in paths.values {
            path.dead = true
            path.pending.removeAll()
            path.connection.stateUpdateHandler = nil
            path.connection.cancel()
        }
        paths.removeAll()
    }

    // MARK: Private

    private func handleState(_ state: NWConnection.State, path: Path, id: UInt32) {
        guard !closed else { return }
        switch state {
        case .ready:
            guard !path.ready, !path.dead else { return }
            path.ready = true
            let queued = path.pending
            path.pending.removeAll()
            path.connection.batch {
                for d in queued {
                    path.connection.send(content: d, completion: .idempotent)
                }
            }
            receiveLoop(path, id: id)
        case .failed, .cancelled:
            // The engine notices the silence (probe timeout / path loss)
            // and falls back to the relay; nothing to report here.
            path.ready = false
            path.dead = true
            path.pending.removeAll()
        default:
            break
        }
    }

    private func receiveLoop(_ path: Path, id: UInt32) {
        path.connection.receiveMessage { [weak self, weak path] content, _, _, error in
            guard let self, let path, !self.closed, !path.dead, error == nil else { return }
            // Re-arm first so the next datagram is already being received.
            self.receiveLoop(path, id: id)
            if let content, !content.isEmpty {
                self.onReceive?(id, content)
            }
        }
    }

    /// "1.2.3.4:5" / "[fe80::1]:5" -> (host, port).
    static func parse(_ address: String) -> (host: String, port: UInt16)? {
        let s = address.trimmingCharacters(in: .whitespaces)
        if s.hasPrefix("[") {
            guard let close = s.range(of: "]:") else { return nil }
            let host = String(s[s.index(after: s.startIndex)..<close.lowerBound])
            guard !host.isEmpty, let port = UInt16(s[close.upperBound...]), port > 0 else { return nil }
            return (host: host, port: port)
        }
        guard let colon = s.lastIndex(of: ":") else { return nil }
        let host = String(s[s.startIndex..<colon])
        guard !host.isEmpty, !host.contains(":"),
              let port = UInt16(s[s.index(after: colon)...]), port > 0 else { return nil }
        return (host: host, port: port)
    }

    /// Private / link-local / CGNAT (Tailscale) / loopback -> LAN.
    static func classify(host: String) -> Kind {
        let parts = host.split(separator: ".")
        if parts.count == 4 {
            let o = parts.compactMap { UInt8($0) }
            guard o.count == 4 else { return .internet }
            switch (o[0], o[1]) {
            case (10, _), (127, _), (192, 168), (169, 254):
                return .lan
            case (172, 16...31), (100, 64...127):
                return .lan
            default:
                return .internet
            }
        }
        let h = host.lowercased()
        if h == "::1" || h.hasPrefix("fe8") || h.hasPrefix("fe9") || h.hasPrefix("fea")
            || h.hasPrefix("feb") || h.hasPrefix("fc") || h.hasPrefix("fd") {
            return .lan
        }
        return .internet
    }
}
