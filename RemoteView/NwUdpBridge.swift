import Foundation
import Network

// Host-owned UDP datagram transport for direct/LAN connections. Outbound
// datagrams come from the ovffi datagram sink (which fires on the pump
// thread); inbound datagrams are forwarded to the caller on the main queue
// so the session is only ever touched from the main thread (the pump timer
// also runs there), avoiding re-entrancy with ov_session_pump.
final class NwUdpBridge {
    private let connection: NWConnection
    private var onReceive: ((Data) -> Void)?
    private var onState: ((NWConnection.State) -> Void)?
    /// Datagrams queued before the connection reaches `.ready`.
    private var pending: [Data] = []
    private let stateLock = NSLock()

    init(host: String, port: UInt16,
         onReceive: @escaping (Data) -> Void,
         onState: @escaping (NWConnection.State) -> Void) {
        self.onReceive = onReceive
        self.onState = onState
        let params = NWParameters.udp
        let endpoint = NWEndpoint.hostPort(host: NWEndpoint.Host(host),
                                           port: NWEndpoint.Port(rawValue: port) ?? 0)
        connection = NWConnection(to: endpoint, using: params)

        connection.stateUpdateHandler = { [weak self] state in
            // NWConnection callbacks arrive on its internal queue; marshal
            // onto the main queue before touching the session.
            DispatchQueue.main.async {
                self?.handleState(state)
            }
        }
        connection.start(queue: DispatchQueue(label: "ov.udp.bridge"))
    }

    /// Send one datagram to the peer. Buffers until the connection is ready.
    func send(_ data: Data) {
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            if self.isReady {
                self.sendNow(data)
            } else {
                self.pending.append(data)
            }
        }
    }

    /// Cancel the connection; safe to call more than once.
    func close() {
        connection.cancel()
    }

    private var isReady = false

    private func handleState(_ state: NWConnection.State) {
        onState?(state)
        switch state {
        case .ready:
            isReady = true
            for data in pending { sendNow(data) }
            pending.removeAll()
            receiveLoop()
        case .failed, .cancelled:
            isReady = false
        default:
            break
        }
    }

    private func sendNow(_ data: Data) {
        connection.send(content: data, completion: .contentProcessed { _ in })
    }

    private func receiveLoop() {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 65535) { [weak self] content, _, _, error in
            guard let self, error == nil else { return }
            if let content, !content.isEmpty {
                let data = Data(content)
                DispatchQueue.main.async { self.onReceive?(data) }
            }
            if error == nil {
                self.receiveLoop() // keep receiving on the connection queue
            }
        }
    }
}
