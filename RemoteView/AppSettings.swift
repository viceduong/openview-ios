import Foundation
import UIKit

// App-wide settings (UserDefaults keys + typed readers) and small shared
// formatting helpers. SwiftUI screens bind the same keys with @AppStorage;
// non-view code reads them through `AppSettings`.

/// The built-in relay: ovrelay behind the Cloudflare tunnel.
let builtInRelayURL = "wss://openview.2031.cloud/relay"

enum SettingsKey {
    // Connection
    static let defaultRelayURL = "ov.defaultRelayURL"
    /// Default quality for new connections, target fps (0 = Auto).
    static let defaultQuality = "ov.defaultQuality"
    static let autoReconnect = "ov.autoReconnect"
    static let confirmDisconnect = "ov.confirmDisconnect"
    /// Section 5: try a direct UDP path next to relay sessions.
    static let directConnection = "ov.directConnection"
    // Display (Section 4)
    /// TeamViewer-style unsharp mask after scaling (Metal renderer).
    static let sharpen = "ov.sharpen"
    /// SESSION_OPTION wishes sent to hosts with CAP_DESKTOP_FX.
    static let hideWallpaper = "ov.hideWallpaper"
    static let disableAnimations = "ov.disableAnimations"
    // Input (the rest live on RemoteInputController)
    static let cursorAccel = RemoteInputController.cursorAccelDefaultsKey
    /// Trackpad cursor speed multiplier (Double, 0.5...3.0, default 1.0 =
    /// TeamViewer; a stored v0.40 default of 2.5 is migrated to 1.0 once).
    static let cursorSpeed = RemoteInputController.cursorSpeedDefaultsKey
    /// Mouse mode, zoomed in: "center" (TeamViewer, default) or "edge".
    static let viewportFollow = RemoteInputController.viewportFollowDefaultsKey
    static let lockLocalZoom = RemoteInputController.lockLocalZoomDefaultsKey
    // Session
    static let keepAwake = "ov.keepAwake"
    /// "auto" or "landscape".
    static let orientation = "ov.orientation"
    static let showStats = "ov.showStats"
    /// Legacy key (v0.39 and earlier, default on); superseded by
    /// `toolbarFade` so every install starts with fading off.
    static let toolbarAutoHide = "ov.toolbarAutoHide"
    /// "Fade toolbar when idle" (default off): 70% after 3 s idle.
    static let toolbarFade = "ov.toolbarFadeIdle"
    static let toolbarCollapsed = "ov.toolbarCollapsed"
    /// Collapsed toolbar handle: "leading" / "trailing" edge, and its
    /// vertical position as a fraction of the height.
    static let toolbarHandleEdge = "ov.toolbarHandleEdge"
    static let toolbarHandleY = "ov.toolbarHandleY"
    static let didShowGestureHelp = "ov.didShowGestureHelp"
    // Security
    static let appLock = "ov.appLock"
    static let pinBiometric = "ov.pinBiometric"
    // Appearance: "system" / "light" / "dark".
    static let appearance = "ov.appearance"
    // Diagnostics
    /// Append every touch's routing (receiving view, recognizers, presented
    /// controller) and toolbar actions to the session log (default off).
    static let logTouchRouting = "ov.debugTouchRouting"
}

enum AppSettings {
    static func bool(_ key: String, default def: Bool) -> Bool {
        UserDefaults.standard.object(forKey: key) as? Bool ?? def
    }

    /// The relay new connections use (falls back to the built-in one).
    static var defaultRelayURL: String {
        let raw = UserDefaults.standard.string(forKey: SettingsKey.defaultRelayURL) ?? ""
        let trimmed = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? builtInRelayURL : trimmed
    }

    static var defaultQualityFps: UInt16 {
        UInt16(clamping: UserDefaults.standard.integer(forKey: SettingsKey.defaultQuality))
    }

    static var autoReconnect: Bool { bool(SettingsKey.autoReconnect, default: true) }
    static var confirmDisconnect: Bool { bool(SettingsKey.confirmDisconnect, default: true) }
    static var directConnection: Bool { bool(SettingsKey.directConnection, default: true) }
    static var sharpen: Bool { bool(SettingsKey.sharpen, default: true) }
    static var hideWallpaper: Bool { bool(SettingsKey.hideWallpaper, default: true) }
    static var disableAnimations: Bool { bool(SettingsKey.disableAnimations, default: false) }
    static var keepAwake: Bool { bool(SettingsKey.keepAwake, default: true) }
    static var toolbarFade: Bool { bool(SettingsKey.toolbarFade, default: false) }
    static var logTouchRouting: Bool { bool(SettingsKey.logTouchRouting, default: false) }
    static var pinBiometric: Bool { bool(SettingsKey.pinBiometric, default: false) }
    static var lockLandscape: Bool {
        UserDefaults.standard.string(forKey: SettingsKey.orientation) == "landscape"
    }
}

/// One quality (target fps) choice; 0 = Auto (the host's RTT-aware
/// governor picks the rate).
struct QualityChoice: Identifiable {
    let label: String
    let fps: UInt16
    var id: UInt16 { fps }

    static let all: [QualityChoice] = [
        QualityChoice(label: "Auto", fps: 0),
        QualityChoice(label: "10 fps", fps: 10),
        QualityChoice(label: "30 fps", fps: 30),
        QualityChoice(label: "60 fps", fps: 60),
    ]

    static func label(for fps: UInt16) -> String {
        all.first { $0.fps == fps }?.label ?? "\(fps) fps"
    }
}

/// Device ID display: digits grouped by three ("123 456 789").
enum DeviceID {
    static func digits(_ text: String) -> String {
        String(text.filter { $0.isASCII && $0.isNumber })
    }

    static func format(_ text: String) -> String {
        var out = ""
        for (i, ch) in digits(text).enumerated() {
            if i > 0 && i % 3 == 0 {
                out.append(" ")
            }
            out.append(ch)
        }
        return out
    }
}

enum DisplayTime {
    /// "just now", "5m ago", "2h ago", "3d ago", then a date.
    static func relative(_ date: Date, now: Date = Date()) -> String {
        let s = max(0, now.timeIntervalSince(date))
        if s < 60 { return "just now" }
        if s < 3600 { return "\(Int(s / 60))m ago" }
        if s < 86_400 { return "\(Int(s / 3600))h ago" }
        if s < 86_400 * 30 { return "\(Int(s / 86_400))d ago" }
        return dateFormatter.string(from: date)
    }

    static func clock(_ date: Date) -> String {
        timeFormatter.string(from: date)
    }

    private static let dateFormatter: DateFormatter = {
        let f = DateFormatter()
        f.dateStyle = .medium
        f.timeStyle = .none
        return f
    }()

    private static let timeFormatter: DateFormatter = {
        let f = DateFormatter()
        f.dateStyle = .none
        f.timeStyle = .short
        return f
    }()
}

/// Applies the Appearance setting to every window of the app.
enum AppearanceStyle {
    static func apply(_ raw: String) {
        let style: UIUserInterfaceStyle
        switch raw {
        case "light": style = .light
        case "dark": style = .dark
        default: style = .unspecified
        }
        for scene in UIApplication.shared.connectedScenes {
            guard let windowScene = scene as? UIWindowScene else { continue }
            for window in windowScene.windows {
                window.overrideUserInterfaceStyle = style
            }
        }
    }
}

/// Short haptics for session transitions.
enum Haptics {
    static func success() {
        UINotificationFeedbackGenerator().notificationOccurred(.success)
    }

    static func error() {
        UINotificationFeedbackGenerator().notificationOccurred(.error)
    }
}
