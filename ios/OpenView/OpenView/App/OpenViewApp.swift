import LocalAuthentication
import SwiftUI

@main
struct OpenViewApp: App {
    @StateObject private var app = AppModel()

    init() {
        // Screenshot mode (Debug, -OVScreenshotMode): in-memory demo relay.
        ScreenshotMode.registerDefaults()
        // Diagnostics: persistent log, network / lifecycle events, hang
        // monitor (see DiagLog.swift).
        DiagLog.shared.start()
    }

    var body: some Scene {
        WindowGroup {
            RootView()
                .environmentObject(app)
                .environmentObject(app.store)
        }
    }
}

/// Home plus the optional Face ID app lock and the appearance setting.
/// (The session itself is presented full screen from UIKit by AppModel.)
struct RootView: View {
    @EnvironmentObject private var app: AppModel
    @Environment(\.scenePhase) private var scenePhase
    @AppStorage(SettingsKey.appearance) private var appearance = "system"
    @AppStorage(SettingsKey.appLock) private var appLock = false
    @State private var locked = !ScreenshotMode.isActive && UserDefaults.standard.bool(forKey: SettingsKey.appLock)

    var body: some View {
        ZStack {
            HomeView()
            if locked {
                AppLockView(onUnlock: { locked = false })
                    .transition(.opacity)
                    .zIndex(1)
            }
        }
        .onAppear {
            AppearanceStyle.apply(appearance)
            // The window may attach after the first appearance.
            DispatchQueue.main.async { AppearanceStyle.apply(appearance) }
        }
        .onChange(of: appearance) { value in
            AppearanceStyle.apply(value)
        }
        .onChange(of: scenePhase) { phase in
            // Lock again when leaving the app (not under a running
            // session: that one reconnects on return instead).
            if phase == .background && appLock && !app.sessionActive {
                locked = true
            }
        }
    }
}

/// Full-screen lock: Face ID / Touch ID / passcode to open the app.
struct AppLockView: View {
    let onUnlock: () -> Void
    @Environment(\.scenePhase) private var scenePhase
    @State private var authenticating = false

    var body: some View {
        VStack(spacing: 18) {
            Image(systemName: "lock.fill")
                .font(.system(size: 46, weight: .semibold))
                .foregroundColor(.accentColor)
            Text("OpenView is locked")
                .font(.title2.weight(.semibold))
            Button {
                authenticate()
            } label: {
                Label("Unlock", systemImage: "faceid")
                    .frame(minWidth: 160)
            }
            .buttonStyle(.borderedProminent)
            .controlSize(.large)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color(.systemBackground).ignoresSafeArea())
        .onAppear {
            if scenePhase == .active {
                authenticate()
            }
        }
        .onChange(of: scenePhase) { phase in
            if phase == .active {
                authenticate()
            }
        }
    }

    private func authenticate() {
        guard !authenticating else { return }
        let context = LAContext()
        var error: NSError?
        guard context.canEvaluatePolicy(.deviceOwnerAuthentication, error: &error) else {
            // No passcode / biometrics on this device: nothing to check.
            onUnlock()
            return
        }
        authenticating = true
        context.evaluatePolicy(.deviceOwnerAuthentication, localizedReason: "Unlock OpenView") { ok, _ in
            DispatchQueue.main.async {
                authenticating = false
                if ok {
                    onUnlock()
                }
            }
        }
    }
}
