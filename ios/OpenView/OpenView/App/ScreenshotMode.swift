import Foundation

/// Launch-argument screenshot mode for scripts/ios_screenshots.sh.
///
/// `-OVScreenshotMode` shows a few demo saved devices instead of the real
/// ones, skips the first-run relay sheet, the app lock and auto-connect, and
/// does not browse the LAN. `-OVScreenshotSettings` (together with
/// `-OVScreenshotMode`) also opens Settings on launch.
///
/// Nothing is persisted: the demo devices live in an in-memory
/// `ConnectionStore` (never loaded from or written to disk) and the demo
/// relay is a registration-domain default (in memory only, never saved),
/// so the user's real saved devices and settings are left untouched.
///
/// Active in Debug builds only: in Release `isActive` is always false and
/// the launch arguments are ignored. The flags are matched anywhere in the
/// arguments, so a value after them (`-OVScreenshotMode YES`, which keeps
/// UserDefaults' `-key value` argument parsing aligned) is fine.
enum ScreenshotMode {
    static let launchArgument = "-OVScreenshotMode"
    static let settingsArgument = "-OVScreenshotSettings"
    static let demoRelayURL = "wss://relay.example.com/relay"

    /// Launched with `-OVScreenshotMode` (Debug builds only).
    static let isActive: Bool = {
        #if DEBUG
        return ProcessInfo.processInfo.arguments.contains(launchArgument)
        #else
        return false
        #endif
    }()

    /// Present Settings on launch (screenshot mode only).
    static var opensSettings: Bool {
        isActive && ProcessInfo.processInfo.arguments.contains(settingsArgument)
    }

    /// In-memory defaults for the screenshots (registration domain: never
    /// written to disk, and a real stored value still wins; the script also
    /// passes `-ov.defaultRelayURL` so the demo relay shows regardless).
    static func registerDefaults() {
        guard isActive else { return }
        UserDefaults.standard.register(defaults: [
            SettingsKey.defaultRelayURL: demoRelayURL,
            SettingsKey.relaySetupDone: true,
        ])
    }

    /// The store the app uses: demo devices in memory in screenshot mode,
    /// else the persisted one.
    static func makeStore() -> ConnectionStore {
        isActive ? ConnectionStore(inMemory: demoConnections()) : ConnectionStore()
    }

    /// Neutral demo devices (example relay, documentation-range LAN address).
    static func demoConnections(now: Date = Date()) -> [SavedConnection] {
        [
            SavedConnection(name: "Office PC",
                            deviceId: "123456789012",
                            transport: .relay(demoRelayURL),
                            lastConnected: now.addingTimeInterval(-2 * 3600),
                            isFavorite: true,
                            hasPIN: true),
            SavedConnection(name: "Home Desktop",
                            deviceId: "234567890123",
                            transport: .relay(demoRelayURL),
                            lastConnected: now.addingTimeInterval(-26 * 3600),
                            isFavorite: true),
            SavedConnection(name: "Studio Laptop",
                            deviceId: "345678901234",
                            transport: .lan(host: "192.0.2.20", port: ConnectionTransport.defaultLanPort),
                            lastConnected: now.addingTimeInterval(-3 * 86_400),
                            symbol: "laptopcomputer"),
        ]
    }
}
