import Combine
import SwiftUI
import UIKit

/// App-level state: the saved connections, the one active session and its
/// full-screen presentation, and the remember-on-connect policy (save the
/// device, pin its fingerprint, store the PIN). Main thread only.
final class AppModel: ObservableObject {
    let store = ConnectionStore()
    /// A session is presented.
    @Published private(set) var sessionActive = false
    /// The last session's log (Settings > About).
    @Published private(set) var lastSessionLog: [LogLine] = []

    private var controller: SessionController?
    private var hosting: UIViewController?
    /// The saved connection the active session belongs to (set once saved).
    private var activeConnectionID: UUID?
    private var storeChange: AnyCancellable?

    init() {
        // Re-publish store changes so views observing only the app model
        // refresh too.
        storeChange = store.objectWillChange.sink { [weak self] _ in
            self?.objectWillChange.send()
        }
    }

    // MARK: Requests

    /// A request for a saved device connection.
    func request(for c: SavedConnection, pin: String, remember: Bool, pinIsStored: Bool) -> ConnectRequest? {
        guard let id = UInt64(DeviceID.digits(c.deviceId)) else { return nil }
        return ConnectRequest(connectionID: c.id,
                              name: c.name,
                              target: .device(id: id, pin: pin),
                              transport: c.transport,
                              rememberPIN: remember,
                              pinIsStored: pinIsStored,
                              defaultFps: c.defaultFps,
                              lastMonitor: c.lastMonitor,
                              inputMode: c.inputMode.flatMap { InputMode(rawValue: $0) },
                              pinnedFingerprint: c.pinnedFingerprint)
    }

    /// Quick Connect to a device ID that is not saved yet (default relay).
    func quickRequest(deviceID digits: String, pin: String, remember: Bool) -> ConnectRequest? {
        guard let id = UInt64(DeviceID.digits(digits)) else { return nil }
        return ConnectRequest(name: "",
                              target: .device(id: id, pin: pin),
                              transport: .relay(AppSettings.defaultRelayURL),
                              rememberPIN: remember,
                              defaultFps: AppSettings.defaultQualityFps)
    }

    /// Read a saved PIN (may show Face ID); nil when missing / cancelled.
    func loadPIN(for c: SavedConnection, completion: @escaping (String?) -> Void) {
        KeychainPIN.load(for: c.id, reason: "Use the saved PIN for \(c.displayName)", completion: completion)
    }

    // MARK: Session lifecycle

    /// Build the session controller and present the immersive session.
    func connect(_ req: ConnectRequest) {
        guard controller == nil else { return }
        let c = SessionController(request: req)
        let chrome = SessionChrome()
        c.onEstablished = { [weak self, weak c] pine in
            guard let self, let c else { return }
            self.sessionEstablished(c, pine: pine)
        }
        c.onAuthFailed = { [weak self, weak c] in
            guard let self, let c else { return }
            self.authFailed(c)
        }
        controller = c
        activeConnectionID = req.connectionID
        let root = SessionContainer(controller: c, chrome: chrome, onClose: { [weak self] in
            self?.endSession()
        })
        let vc = SessionHostingController(rootView: root, chrome: chrome)
        hosting = vc
        sessionActive = true
        SessionPresenter.present(vc)
    }

    /// Close the session (Disconnect / Close / Cancel): remember this
    /// session's display choices, stop the engine, dismiss.
    func endSession() {
        guard let c = controller else { return }
        persistPreferences(from: c)
        lastSessionLog = c.lines
        c.stop()
        UIApplication.shared.isIdleTimerDisabled = false
        let vc = hosting
        controller = nil
        hosting = nil
        activeConnectionID = nil
        sessionActive = false
        if let vc {
            SessionPresenter.dismiss(vc)
        }
    }

    /// Established with an accepted host identity: save / update the
    /// device, pin the fingerprint (TOFU) and store the PIN if asked.
    private func sessionEstablished(_ c: SessionController, pine: String) {
        let req = c.request
        guard case .device(let devID, let pin) = req.target else { return } // pairing: one-off
        let digits = String(devID)
        var conn: SavedConnection
        if let id = activeConnectionID ?? req.connectionID, let existing = store.connection(id: id) {
            conn = existing
        } else if let existing = store.find(deviceId: digits) {
            conn = existing
        } else {
            conn = SavedConnection(name: req.name, deviceId: digits, transport: req.transport,
                                   defaultFps: req.defaultFps)
        }
        conn.lastConnected = Date()
        conn.pinnedFingerprint = pine
        if req.rememberPIN && !req.pinIsStored {
            if KeychainPIN.save(pin, for: conn.id, requireUserPresence: AppSettings.pinBiometric) {
                conn.hasPIN = true
            }
        } else if !req.rememberPIN && conn.hasPIN {
            KeychainPIN.delete(for: conn.id)
            conn.hasPIN = false
        }
        store.upsert(conn)
        activeConnectionID = conn.id
    }

    /// Wrong PIN: the stored one is useless — delete it (the error view
    /// offers "Re-enter PIN").
    private func authFailed(_ c: SessionController) {
        guard let id = activeConnectionID ?? c.request.connectionID,
              var conn = store.connection(id: id), conn.hasPIN else { return }
        KeychainPIN.delete(for: id)
        conn.hasPIN = false
        store.upsert(conn)
    }

    private func persistPreferences(from c: SessionController) {
        guard let id = activeConnectionID, var conn = store.connection(id: id) else { return }
        conn.defaultFps = c.targetFps
        conn.lastMonitor = c.monitorIndex
        conn.inputMode = c.input.mode.rawValue
        store.upsert(conn)
    }

    // MARK: Saved connections

    /// Save an edited / new connection. `pin` nil = keep what is stored;
    /// `remember` false forgets a stored PIN. Returns the saved value.
    @discardableResult
    func save(_ connection: SavedConnection, pin: String?, remember: Bool) -> SavedConnection {
        var c = connection
        if let pin, !pin.isEmpty {
            if remember {
                c.hasPIN = KeychainPIN.save(pin, for: c.id, requireUserPresence: AppSettings.pinBiometric)
            } else {
                KeychainPIN.delete(for: c.id)
                c.hasPIN = false
            }
        } else if !remember && c.hasPIN {
            KeychainPIN.delete(for: c.id)
            c.hasPIN = false
        }
        store.upsert(c)
        return c
    }

    func delete(_ c: SavedConnection) {
        store.delete(id: c.id)
    }

    func forgetPIN(_ c: SavedConnection) {
        KeychainPIN.delete(for: c.id)
        store.update(id: c.id) { $0.hasPIN = false }
    }

    func toggleFavorite(_ c: SavedConnection) {
        store.update(id: c.id) { $0.isFavorite.toggle() }
    }

    func forgetAllPINs() {
        store.forgetAllPINs()
    }
}
