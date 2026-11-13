import Foundation
import Network
import os

// "On this network": OpenView hosts found by Bonjour (NWBrowser over
// `_openview._udp`, published by ovhost via crates/ovdiscovery). Each host's
// instance name is its device ID and its TXT record carries `id`, `v` and
// `name` (LanTxtRecord). Tapping one resolves the service to an IPv4
// address + port with a short-lived UDP NWConnection, then dials it as a
// direct (LAN) connection — see HomeView.connectLan. The port is the host's
// LAN session port (ovhost --lan-port, default 4571): the host accepts a PIN
// handshake there from any viewer on its network, no relay involved, and
// the session runs over that UDP path (the same datagram carrier as the
// `.direct` mode, reliable-UDP framed once both sides advertise it).
//
// Requires NSBonjourServices = [_openview._udp] and
// NSLocalNetworkUsageDescription in Info.plist (iOS 14+ local network
// privacy); without permission the browser just finds nothing.

/// One OpenView host on the local network.
struct DiscoveredPC: Identifiable, Equatable {
    /// Device ID digits.
    let deviceID: String
    /// Display name (the PC name, else the grouped device ID).
    let name: String
    /// Handshake protocol version from the TXT record (nil = not given).
    let version: Int?
    /// The Bonjour service endpoint (resolved on tap).
    let endpoint: NWEndpoint

    var id: String { deviceID }

    var isCompatible: Bool {
        version == nil || version == LanTxtRecord.protocolVersion
    }
}

/// Browses for OpenView hosts while started. Main-thread API; `hosts` is
/// published on the main queue.
final class LanDiscovery: ObservableObject {
    static let serviceType = "_openview._udp"

    @Published private(set) var hosts: [DiscoveredPC] = []

    private let queue = DispatchQueue(label: "openview.lan-discovery")
    private var browser: NWBrowser?
    /// Bumped on every start/stop so late callbacks of an old browser are
    /// dropped.
    private var generation = 0

    func start() {
        guard browser == nil else { return }
        generation += 1
        let gen = generation
        let params = NWParameters()
        params.includePeerToPeer = false
        let b = NWBrowser(for: .bonjourWithTXTRecord(type: LanDiscovery.serviceType, domain: nil),
                          using: params)
        b.browseResultsChangedHandler = { [weak self] results, _ in
            let list = LanDiscovery.hosts(from: results)
            DispatchQueue.main.async {
                guard let self, self.generation == gen else { return }
                self.hosts = list
            }
        }
        b.stateUpdateHandler = { [weak self] state in
            guard case .failed(let error) = state else { return }
            Log.transport.error("LAN discovery failed: \(String(describing: error), privacy: .public)")
            // Retry once the network settles (e.g. Wi-Fi rejoined).
            DispatchQueue.main.asyncAfter(deadline: .now() + 5) {
                guard let self, self.generation == gen else { return }
                self.stop()
                self.start()
            }
        }
        browser = b
        b.start(queue: queue)
    }

    func stop() {
        generation += 1
        browser?.cancel()
        browser = nil
        hosts = []
    }

    /// The OpenView hosts among browse results: one per device ID, sorted
    /// by name.
    static func hosts(from results: Set<NWBrowser.Result>) -> [DiscoveredPC] {
        var byID: [String: DiscoveredPC] = [:]
        for result in results {
            guard case .service(let instance, _, _, _) = result.endpoint else { continue }
            var txt = LanTxtRecord()
            if case .bonjour(let record) = result.metadata {
                txt = LanTxtRecord(dictionary: record.dictionary)
            }
            guard let deviceID = txt.deviceID(instanceName: instance) else { continue }
            byID[deviceID] = DiscoveredPC(deviceID: deviceID,
                                          name: txt.displayName(instanceName: instance),
                                          version: txt.version,
                                          endpoint: result.endpoint)
        }
        return byID.values.sorted {
            let a = $0.name.localizedCaseInsensitiveCompare($1.name)
            return a == .orderedSame ? $0.deviceID < $1.deviceID : a == .orderedAscending
        }
    }

    /// Resolve a host's Bonjour service to an IPv4 address string and UDP
    /// port. Calls `completion` on the main queue exactly once — (nil, nil)
    /// on failure or after `timeout` seconds.
    static func resolve(_ pc: DiscoveredPC, timeout: TimeInterval = 5,
                        completion: @escaping (String?, UInt16?) -> Void) {
        let queue = DispatchQueue(label: "openview.lan-resolve")
        let params = NWParameters.udp
        // The host's direct socket is IPv4; resolve the A record.
        if let ip = params.defaultProtocolStack.internetProtocol as? NWProtocolIP.Options {
            ip.version = .v4
        }
        let connection = NWConnection(to: pc.endpoint, using: params)
        var finished = false
        // Runs on `queue` only.
        func finish(_ host: String?, _ port: UInt16?) {
            guard !finished else { return }
            finished = true
            connection.cancel()
            DispatchQueue.main.async { completion(host, port) }
        }
        connection.stateUpdateHandler = { state in
            switch state {
            case .ready:
                if case .hostPort(let host, let port)? = connection.currentPath?.remoteEndpoint,
                   let text = LanDiscovery.addressString(host) {
                    finish(text, port.rawValue)
                } else {
                    finish(nil, nil)
                }
            case .failed, .cancelled:
                finish(nil, nil)
            default:
                break
            }
        }
        connection.start(queue: queue)
        queue.asyncAfter(deadline: .now() + timeout) {
            finish(nil, nil)
        }
    }

    /// Dotted IPv4 (or a host name); nil for anything else.
    static func addressString(_ host: NWEndpoint.Host) -> String? {
        switch host {
        case .ipv4(let address):
            let bytes = [UInt8](address.rawValue)
            guard bytes.count == 4 else { return nil }
            return bytes.map { String($0) }.joined(separator: ".")
        case .name(let name, _):
            return name.isEmpty ? nil : name
        default:
            return nil
        }
    }
}
