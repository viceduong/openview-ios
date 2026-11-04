import Foundation

// WebSocket transport to ovrelay (`wss://host/relay`). The relay speaks the
// same byte protocol inside binary messages, so this only moves bytes: each
// outbound chunk from Rust goes out as one message, each inbound message is
// handed to the session (`relayFeed`). Used when the relay sits behind an
// HTTP-only front such as a Cloudflare tunnel. iOS handles TLS.
//
// Callbacks are delivered on the main queue (the app pumps on the main
// thread). Messages that arrive before `onMessage` is set are buffered, so
// ConnectView can open the socket and SessionView attach later.

final class RelayWebSocket {
    private let task: URLSessionWebSocketTask
    private var buffered: [Data] = []
    private var closedError: Error??
    private var closed = false

    /// Inbound message bytes (main queue). Setting it flushes the buffer.
    var onMessage: ((Data) -> Void)? {
        didSet { flush() }
    }
    /// The socket closed (nil) or failed (main queue). Fires once.
    var onClose: ((Error?) -> Void)? {
        didSet { flush() }
    }

    init(url: URL) {
        task = URLSession.shared.webSocketTask(with: url)
        task.resume()
        receive()
    }

    /// Send one chunk as a binary message. URLSession queues it until the
    /// handshake completes.
    func send(_ data: Data) {
        task.send(.data(data)) { [weak self] error in
            guard let error else { return }
            DispatchQueue.main.async { self?.finish(error) }
        }
    }

    func close() {
        task.cancel(with: .normalClosure, reason: nil)
    }

    private func receive() {
        task.receive { [weak self] result in
            DispatchQueue.main.async {
                guard let self else { return }
                switch result {
                case .success(.data(let data)):
                    self.deliver(data)
                    self.receive()
                case .success(.string(let text)):
                    self.deliver(Data(text.utf8))
                    self.receive()
                case .success:
                    self.receive()
                case .failure(let error):
                    self.finish(error)
                }
            }
        }
    }

    private func deliver(_ data: Data) {
        if let onMessage {
            onMessage(data)
        } else {
            buffered.append(data)
        }
    }

    private func finish(_ error: Error?) {
        guard !closed else { return }
        closed = true
        if let onClose {
            onClose(error)
        } else {
            closedError = .some(error)
        }
    }

    private func flush() {
        if let onMessage, !buffered.isEmpty {
            let pending = buffered
            buffered.removeAll()
            pending.forEach(onMessage)
        }
        if let onClose, let error = closedError {
            closedError = nil
            onClose(error)
        }
    }
}
