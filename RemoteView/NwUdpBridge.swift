import Foundation
import Network

// Host-owned UDP datagram transport for direct/LAN connections. Outbound
// datagrams come from the ovffi datagram sink (which fires on the pump
// thread); inbound datagrams are handed to the caller on `queue`. The
// NWConnection itself runs on `queue` (the session queue, `OvQueues.session`)
// so its callbacks already arrive there — no hop, and the session is only
// ever touched from that one serial queue, avoiding re-entrancy with
// ov_session_pump. All methods must be called on `queue`.
//
// Phase 4: the Rust side frames records into <= 1.2 KB reliable-UDP
// datagrams (ACKs + retransmissions, see ovtransport::rudp) once the host
// switches; this bridge still just moves opaque datagrams. Each callback
// re-arms the receive immediately (before the caller's work), and
// SessionController batches the pumps.
final class NwUdpBridge {
    private let connection: NWConnection
    private var onReceive: ((Data) -> Void)?
    private var onState: ((NWConnection.State) -> Void)?
    /// Datagrams queued before the connection reaches `.ready` (bounded —
    /// a peer that never answers must not grow this forever).
    private var pending: [Data] = []
    private static let maxPending = 256
    private var isReady = false
    private var closed = false

    init(host: String, port: UInt16,
         queue: DispatchQueue,
         onReceive: @escaping (Data) -> Void,
         onState: @escaping (NWConnection.State) -> Void) {
        self.onReceive = onReceive
        self.onState = onState
        let params = NWParameters.udp
        let endpoint = NWEndpoint.hostPort(host: NWEndpoint.Host(host),
                                           port: NWEndpoint.Port(rawValue: port) ?? 0)
        connection = NWConnection(to: endpoint, using: params)

        connection.stateUpdateHandler = { [weak self] state in
            // Already on `queue` (the connection's queue).
            self?.handleState(state)
        }
        connection.start(queue: queue)
    }

    /// Send one datagram to the peer. Buffers until the connection is ready.
    func send(_ data: Data) {
        guard !closed else { return }
        if isReady {
            sendNow(data)
        } else if pending.count < Self.maxPending {
            pending.append(data)
        }
    }

    /// Run `body` (which may `send` several datagrams, e.g. a pump emitting
    /// reliable-UDP ACKs and retransmissions) as one NWConnection batch, so
    /// the sends are coalesced into fewer system calls (iOS 12+).
    func batch(_ body: () -> Void) {
        guard isReady, !closed else {
            body()
            return
        }
        connection.batch(body)
    }

    /// Cancel the connection and drop the callbacks; safe to call more than
    /// once.
    func close() {
        guard !closed else { return }
        closed = true
        isReady = false
        pending.removeAll()
        onReceive = nil
        onState = nil
        connection.cancel()
    }

    private func handleState(_ state: NWConnection.State) {
        onState?(state)
        switch state {
        case .ready:
            guard !closed else { return }
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
        connection.receiveMessage { [weak self] content, _, _, error in
            guard let self, !self.closed, error == nil else { return }
            // Re-arm first so the next datagram is already being received
            // while this one is processed (keeps up with bursts).
            self.receiveLoop()
            if let content, !content.isEmpty {
                self.onReceive?(content)
            }
        }
    }
}
