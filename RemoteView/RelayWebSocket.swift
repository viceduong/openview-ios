import Foundation

// WebSocket transport to ovrelay (`wss://host/relay`). The relay speaks the
// same byte protocol inside binary messages, so this only moves bytes: each
// outbound chunk from Rust goes out as one message, each inbound message is
// handed to the session (`relayFeed`). Used when the relay sits behind an
// HTTP-only front such as a Cloudflare tunnel. iOS handles TLS.
//
// Callbacks are delivered on `queue` (the session queue, `OvQueues.session`
// — where the app pumps). `onMessage` / `onClose` must be set on that queue
// too. Messages that arrive before `onMessage` is set are buffered, so
// ConnectView can open the socket and SessionController attach later.
// Ordering: messages are delivered in arrival order, and the close always
// lands after every message received before it.

final class RelayWebSocket {
    private let task: URLSessionWebSocketTask
    private let queue: DispatchQueue
    // Queue-confined state (only touched on `queue`).
    private var buffered: [Data] = []
    private var closedError: Error??
    private var closed = false

    /// Inbound message bytes (on `queue`). Setting it flushes the buffer.
    var onMessage: ((Data) -> Void)? {
        didSet { flush() }
    }
    /// The socket closed (nil) or failed (on `queue`). Fires once.
    var onClose: ((Error?) -> Void)? {
        didSet { flush() }
    }

    init(url: URL, queue: DispatchQueue) {
        self.queue = queue
        task = URLSession.shared.webSocketTask(with: url)
        task.resume()
        receive()
    }

    /// Send one chunk as a binary message. URLSession queues it until the
    /// handshake completes. Safe from any thread.
    func send(_ data: Data) {
        task.send(.data(data)) { [weak self] error in
            guard let error, let self else { return }
            self.queue.async { self.finish(error) }
        }
    }

    /// Cancel the socket; safe from any thread and more than once.
    func close() {
        task.cancel(with: .normalClosure, reason: nil)
    }

    private func receive() {
        task.receive { [weak self] result in
            guard let self else { return }
            // Re-arm right away so the next message is already being read
            // while this one is processed. URLSession completes receives in
            // order on its serial delegate queue, and each completion hops
            // onto the serial `queue` in that same order, so delivery order
            // (and close-after-messages) is preserved.
            var failed = false
            if case .failure = result { failed = true }
            if !failed { self.receive() }
            self.queue.async {
                switch result {
                case .success(.data(let data)):
                    self.deliver(data)
                case .success(.string(let text)):
                    self.deliver(Data(text.utf8))
                case .success:
                    break
                case .failure(let error):
                    self.finish(error)
                }
            }
        }
    }

    private func deliver(_ data: Data) {
        guard !closed else { return }
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
