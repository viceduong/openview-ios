import Foundation
import Network

// The phone's current network path (NWPathMonitor), reduced to what the
// LAN-or-relay decision needs (TransportPlan / NetworkReach): is Wi-Fi or
// wired Ethernet available, or only cellular. Started once at app launch
// (AppModel) so the first connect already has a path.

final class NetworkPathWatcher {
    static let shared = NetworkPathWatcher()

    private let monitor = NWPathMonitor()
    private let lock = NSLock()
    /// Latest classification (guarded by `lock`); `.unknown` until the
    /// monitor's first update.
    private var reach: NetworkReach = .unknown
    private var started = false

    private init() {}

    /// Start monitoring (idempotent; any thread).
    func start() {
        lock.lock()
        defer { lock.unlock() }
        guard !started else { return }
        started = true
        monitor.pathUpdateHandler = { [weak self] path in
            self?.update(path)
        }
        monitor.start(queue: DispatchQueue(label: "ov.network-path", qos: .utility))
    }

    /// The latest classification (any thread).
    var current: NetworkReach {
        lock.lock()
        defer { lock.unlock() }
        return reach
    }

    private func update(_ path: NWPath) {
        // `availableInterfaces` also lists the physical interface under a
        // VPN tunnel, so a Wi-Fi network behind a VPN still counts as local.
        let types = path.availableInterfaces.map { $0.type }
        let local = path.usesInterfaceType(.wifi) || path.usesInterfaceType(.wiredEthernet)
            || types.contains(.wifi) || types.contains(.wiredEthernet)
        let cellular = path.usesInterfaceType(.cellular) || types.contains(.cellular)
        let value = NetworkReach.classify(satisfied: path.status == .satisfied,
                                          hasLocalInterface: local,
                                          hasCellular: cellular)
        lock.lock()
        reach = value
        lock.unlock()
    }
}
