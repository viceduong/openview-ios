import CoreGraphics
import Foundation

// Value types the session publishes to the UI.

// MARK: - Log lines

/// One session-log line (status / errors); a stable id so lists diff cheaply.
struct LogLine: Identifiable {
    let id: Int
    let text: String
    let date: Date
}

/// One chat message (channel 1).
struct ChatMessage: Identifiable {
    let id: Int
    let text: String
    let fromMe: Bool
    let date: Date
}

// A file received from the peer: already written to the app
// container; `url` opens it (Quick Look / share sheet).
struct ReceivedFile: Identifiable {
    let id = UUID()
    let name: String
    let url: URL
    let size: Int
    let date: Date
}

/// One host monitor from MONITORS_INFO.
typealias MonitorInfo = (x: Int32, y: Int32, w: UInt32, h: UInt32, primary: Bool)

/// TOFU: the host presented a different identity than the pinned one.
struct IdentityMismatch: Identifiable {
    let id = UUID()
    let expected: String
    let got: String
}

// MARK: - Session phase

enum SessionError: Equatable {
    /// PIN / pairing code rejected (`OV_ERR_AUTH_FAILED`). The only error
    /// that discards a stored PIN.
    case authFailed
    /// The host refuses PIN attempts for `retryAfter` more seconds
    /// (`OV_ERR_RATE_LIMITED`).
    case rateLimited(retryAfter: Int)
    /// The host speaks an incompatible handshake version
    /// (`OV_ERR_INCOMPATIBLE_VERSION`).
    case incompatibleVersion
    /// No secure session within the handshake timeout.
    case hostUnreachable
    /// An established session ended.
    case disconnected(String)
    /// The transport could not be set up / failed.
    case transport(String)

    /// Retrying on its own cannot help: wrong credentials, a host that is
    /// locked for a while, or a version mismatch.
    var canAutoReconnect: Bool {
        switch self {
        case .authFailed, .rateLimited, .incompatibleVersion: return false
        case .hostUnreachable, .disconnected, .transport: return true
        }
    }

    /// "Too many attempts" text shared by the log line and the error card.
    static func rateLimitText(_ seconds: Int) -> String {
        seconds > 0
            ? "Too many attempts — try again in \(seconds) s"
            : "Too many attempts — try again later"
    }

    static let incompatibleVersionText =
        "This PC runs an incompatible OpenView version — update both apps"

    /// One line for the session log.
    var logText: String {
        switch self {
        case .authFailed: return "Authentication failed"
        case .rateLimited(let seconds): return Self.rateLimitText(seconds)
        case .incompatibleVersion: return Self.incompatibleVersionText
        case .hostUnreachable: return "Host not responding"
        case .disconnected(let reason): return "Disconnected — \(reason)"
        case .transport(let reason): return "Connection failed — \(reason)"
        }
    }
}

enum SessionPhase: Equatable {
    case dialing(String)
    case authenticating
    case waitingForFirstFrame
    case live
    case reconnecting(Int)
    case failed(SessionError)

    /// The full-screen connecting overlay is shown.
    var isConnecting: Bool {
        switch self {
        case .dialing, .authenticating, .waitingForFirstFrame: return true
        default: return false
        }
    }

    var stepText: String {
        switch self {
        case .dialing(let transport): return "Contacting the host via \(transport)…"
        case .authenticating: return "Verifying identity…"
        case .waitingForFirstFrame: return "Waiting for the remote screen…"
        case .live: return "Connected"
        case .reconnecting(let n): return "Reconnecting… (\(n)/\(SessionController.maxReconnectAttempts))"
        case .failed(let error): return error.logText
        }
    }
}
