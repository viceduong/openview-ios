import Foundation
import os

/// Unified-logging channels (`os.Logger`, iOS 14+). View them in Console.app
/// or with `log stream --predicate 'subsystem == "<bundle id>"'`; debug-level
/// messages are only kept while streaming.
///
/// What goes where:
/// - the in-app session log (`SessionController.log`) holds user-meaningful
///   events only (connecting, connected, auth failed, reconnecting, direct
///   path active / lost, files, errors); those also land in `session` here
///   and in the persistent `DiagLog`;
/// - developer detail (socket events, retries, power state, ...) goes to
///   these loggers at `.debug` level and nowhere else.
enum Log {
    static let subsystem = Bundle.main.bundleIdentifier ?? "org.openview.remoteview"

    /// Session lifecycle and user-visible events.
    static let session = Logger(subsystem: subsystem, category: "session")
    /// Relay / UDP / direct-path transport detail.
    static let transport = Logger(subsystem: subsystem, category: "transport")
    /// Touch, pointer and keyboard input.
    static let input = Logger(subsystem: subsystem, category: "input")
    /// Metal / video rendering.
    static let render = Logger(subsystem: subsystem, category: "render")
    /// Diagnostics: hangs, persistence failures, app lifecycle.
    static let diag = Logger(subsystem: subsystem, category: "diag")
}
