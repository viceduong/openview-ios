import Foundation
import Network
import UIKit
import os

// Persistent diagnostics, kept small and local. Nothing here leaves the
// device on its own: the user shares the files explicitly (Settings > About
// > Share diagnostic logs).
//
// - `DiagLog`: a per-launch log file (Application Support/logs/
//   session-<start>.log) with errors, session events, app lifecycle,
//   network path changes and hangs. Lines are timestamped (ms) and written
//   from a dedicated serial queue that never waits on the session queue or
//   the main thread, so a hang there still leaves a complete log behind.
//   The newest `maxFiles` files are kept; one file stops growing at
//   `maxBytes`.
// - `HangMonitor`: a plain Thread (not GCD-scheduled) that pings the session
//   queue and the main queue every second and logs once either has not run
//   the ping for more than 2 s.
// - `Breadcrumb` (debug builds only): which blocking FFI call is running,
//   added to hang reports.

final class DiagLog {
    static let shared = DiagLog()

    static let maxFiles = 5
    static let maxBytes = 1 << 20

    private let queue = DispatchQueue(label: "ov.diaglog", qos: .utility)
    let directory: URL
    /// The file this launch writes.
    let currentURL: URL
    private var handle: FileHandle?
    private var written = 0
    private var capped = false
    private let lineFormatter: DateFormatter
    private var pathMonitor: NWPathMonitor?
    private var lastPath = ""
    private var observers: [NSObjectProtocol] = []
    private var started = false

    private init() {
        let base = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first
            ?? FileManager.default.temporaryDirectory
        directory = base.appendingPathComponent("logs", isDirectory: true)
        try? FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let stamp = DateFormatter()
        stamp.locale = Locale(identifier: "en_US_POSIX")
        stamp.dateFormat = "yyyyMMdd-HHmmss"
        currentURL = directory.appendingPathComponent("session-\(stamp.string(from: Date())).log")
        let f = DateFormatter()
        f.locale = Locale(identifier: "en_US_POSIX")
        f.dateFormat = "yyyy-MM-dd HH:mm:ss.SSS"
        lineFormatter = f
        Self.prune(directory, keep: Self.maxFiles - 1)
        FileManager.default.createFile(atPath: currentURL.path, contents: nil)
        handle = try? FileHandle(forWritingTo: currentURL)
    }

    /// Start the extra sources (lifecycle, network path, hang monitor).
    /// Main thread, once at launch.
    func start() {
        guard !started else { return }
        started = true
        let info = Bundle.main.infoDictionary
        let version = (info?["CFBundleShortVersionString"] as? String ?? "?") + " (" + (info?["CFBundleVersion"] as? String ?? "?") + ")"
        log("App launch: OpenView \(version), iOS \(UIDevice.current.systemVersion), \(UIDevice.current.model)")
        let nc = NotificationCenter.default
        let events: [(Notification.Name, String)] = [
            (UIApplication.didBecomeActiveNotification, "App: active"),
            (UIApplication.didEnterBackgroundNotification, "App: background"),
            (UIApplication.willEnterForegroundNotification, "App: foreground"),
            (UIApplication.willTerminateNotification, "App: terminate"),
            (UIApplication.didReceiveMemoryWarningNotification, "App: memory warning"),
            (UIApplication.protectedDataWillBecomeUnavailableNotification, "Device: locking"),
            (UIApplication.protectedDataDidBecomeAvailableNotification, "Device: unlocked"),
        ]
        for (name, text) in events {
            observers.append(nc.addObserver(forName: name, object: nil, queue: nil) { [weak self] _ in
                self?.log(text)
            })
        }
        let monitor = NWPathMonitor()
        monitor.pathUpdateHandler = { [weak self] path in
            self?.notePath(path)
        }
        monitor.start(queue: DispatchQueue(label: "ov.diaglog.path", qos: .utility))
        pathMonitor = monitor
        HangMonitor.shared.start()
    }

    /// Append one line (any thread; never blocks on the app's queues).
    func log(_ text: String) {
        let now = Date()
        queue.async { [self] in
            write("\(lineFormatter.string(from: now)) \(text)\n")
        }
    }

    private func write(_ line: String) {
        guard let handle, !capped else { return }
        let data = Data(line.utf8)
        if written + data.count > Self.maxBytes {
            capped = true
            handle.write(Data("(log size cap reached; further lines dropped)\n".utf8))
            return
        }
        // FileHandle.write is an unbuffered write(2): the line is in the
        // file even if the app is killed / frozen right after.
        handle.write(data)
        written += data.count
    }

    private func notePath(_ path: NWPath) {
        let status: String
        switch path.status {
        case .satisfied: status = "satisfied"
        case .unsatisfied: status = "unsatisfied"
        case .requiresConnection: status = "requires connection"
        @unknown default: status = "unknown"
        }
        var kinds: [String] = []
        let types: [(NWInterface.InterfaceType, String)] = [
            (.wifi, "wifi"), (.cellular, "cellular"), (.wiredEthernet, "wired"),
            (.loopback, "loopback"), (.other, "other"),
        ]
        for (t, label) in types where path.usesInterfaceType(t) {
            kinds.append(label)
        }
        let names = path.availableInterfaces.map { "\($0.name)" }
        let tunnels = names.filter { $0.hasPrefix("utun") || $0.hasPrefix("ipsec") || $0.hasPrefix("ppp") }
        var flags: [String] = []
        if path.isExpensive { flags.append("expensive") }
        if path.isConstrained { flags.append("constrained") }
        if path.supportsIPv4 { flags.append("v4") }
        if path.supportsIPv6 { flags.append("v6") }
        let uses = kinds.isEmpty ? "none" : kinds.joined(separator: "+")
        let text = "Network: \(status), uses \(uses), "
            + "interfaces [\(names.joined(separator: ","))], VPN tunnel \(tunnels.isEmpty ? "none" : tunnels.joined(separator: ",")), "
            + flags.joined(separator: " ")
        guard text != lastPath else { return }
        lastPath = text
        log(text)
    }

    // MARK: Files

    /// Every kept log file (for the Share sheet), newest first.
    func allFiles() -> [URL] {
        queue.sync {
            Array(Self.files(directory).reversed())
        }
    }

    private static func files(_ dir: URL) -> [URL] {
        let all = (try? FileManager.default.contentsOfDirectory(at: dir, includingPropertiesForKeys: nil)) ?? []
        return all
            .filter { $0.lastPathComponent.hasPrefix("session-") }
            .sorted { $0.lastPathComponent < $1.lastPathComponent }
    }

    private static func prune(_ dir: URL, keep: Int) {
        let all = files(dir)
        guard all.count > keep else { return }
        for url in all.prefix(all.count - keep) {
            try? FileManager.default.removeItem(at: url)
        }
    }
}

/// Which blocking FFI call is running, and since when (see `HangMonitor`).
/// Nested calls (a sink inside a pump) show the innermost one. Debug builds
/// only: in release builds `run` is a plain call and `describe` is empty.
final class Breadcrumb {
    static let shared = Breadcrumb()

    #if DEBUG
    private let lock = NSLock()
    private var op: StaticString = "idle"
    private var since: UInt64 = 0
    #endif

    @inline(__always)
    func run<T>(_ name: StaticString, _ body: () throws -> T) rethrows -> T {
        #if DEBUG
        let now = DispatchTime.now().uptimeNanoseconds
        lock.lock()
        let prev = (op, since)
        op = name
        since = now
        lock.unlock()
        defer {
            lock.lock()
            (op, since) = prev
            lock.unlock()
        }
        #endif
        return try body()
    }

    /// "in <op> for <s> s" (empty in release builds).
    func describe() -> String {
        #if DEBUG
        lock.lock()
        let (o, s) = (op, since)
        lock.unlock()
        let name = "\(o)"
        guard name != "idle", s > 0 else { return "no FFI call in progress" }
        let secs = Double(DispatchTime.now().uptimeNanoseconds &- s) / 1e9
        return "in \(name) for \(String(format: "%.1f", secs)) s"
        #else
        return ""
        #endif
    }
}

/// Detects a blocked session queue / main thread from an independent thread.
final class HangMonitor {
    static let shared = HangMonitor()

    private struct Watched {
        let name: String
        let queue: DispatchQueue
        /// When the outstanding ping was queued (nil = none outstanding).
        var pendingSince: UInt64?
        /// When the block was last reported (0 = not reported).
        var reportedAt: UInt64 = 0
    }

    private let lock = NSLock()
    private var watched: [Watched] = []
    private var started = false
    private static let blockedAfter: UInt64 = 2_000_000_000
    private static let repeatEvery: UInt64 = 5_000_000_000

    func start() {
        lock.lock()
        defer { lock.unlock() }
        guard !started else { return }
        started = true
        watched = [
            Watched(name: "SESSION QUEUE", queue: OvQueues.session),
            Watched(name: "MAIN THREAD", queue: .main),
        ]
        let thread = Thread { [weak self] in
            while let self {
                Thread.sleep(forTimeInterval: 1.0)
                self.tick()
            }
        }
        thread.name = "ov.hangmonitor"
        thread.qualityOfService = .utility
        thread.start()
    }

    private func tick() {
        let now = DispatchTime.now().uptimeNanoseconds
        var reports: [String] = []
        var pings: [(Int, DispatchQueue)] = []
        lock.lock()
        for i in watched.indices {
            if let since = watched[i].pendingSince {
                let blocked = now &- since
                if blocked > Self.blockedAfter,
                   watched[i].reportedAt == 0 || now &- watched[i].reportedAt >= Self.repeatEvery {
                    watched[i].reportedAt = now
                    reports.append("\(watched[i].name) BLOCKED for \(blocked / 1_000_000_000) s")
                }
            } else {
                watched[i].pendingSince = now
                pings.append((i, watched[i].queue))
            }
        }
        lock.unlock()
        for r in reports {
            let crumb = Breadcrumb.shared.describe()
            let line = crumb.isEmpty ? "HANG: \(r)" : "HANG: \(r) (\(crumb))"
            Log.diag.error("\(line, privacy: .public)")
            DiagLog.shared.log(line)
        }
        for (i, q) in pings {
            q.async { [weak self] in
                self?.answered(i)
            }
        }
    }

    private func answered(_ i: Int) {
        let now = DispatchTime.now().uptimeNanoseconds
        lock.lock()
        let since = watched[i].pendingSince
        let reported = watched[i].reportedAt != 0
        let name = watched[i].name
        watched[i].pendingSince = nil
        watched[i].reportedAt = 0
        lock.unlock()
        if reported, let since {
            DiagLog.shared.log("HANG: \(name) unblocked after \(String(format: "%.1f", Double(now &- since) / 1e9)) s")
        }
    }
}
