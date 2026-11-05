import Combine
import SwiftUI
import UIKit
import UIKit.UIGestureRecognizerSubclass

// The immersive session host (section 3). The session is presented full
// screen from UIKit with a UIHostingController subclass, because SwiftUI's
// own `.fullScreenCover` controller cannot be customised on iOS 15: we
// need the home indicator auto-hidden, the status bar hidden, the screen
// edges deferred to the remote screen, an orientation lock option and
// iPad pointer lock (which must come from the presented root controller).

/// Session-chrome state shared by the SwiftUI session view and its hosting
/// controller (keyboard shortcuts toggle it). Main thread only.
final class SessionChrome: ObservableObject {
    /// The floating toolbar is collapsed to its handle (persisted).
    @Published var toolbarCollapsed = false {
        didSet {
            UserDefaults.standard.set(toolbarCollapsed, forKey: SettingsKey.toolbarCollapsed)
        }
    }
    /// iPad pointer lock wanted (persisted as the "ov.pointerLock" setting).
    @Published var pointerLockOn = false {
        didSet {
            UserDefaults.standard.set(pointerLockOn, forKey: RemoteInputController.pointerLockDefaultsKey)
        }
    }
    /// The toolbar faded to `fadedOpacity` after 3 s without interaction
    /// (only with the "Fade toolbar when idle" setting, default off).
    /// Purely visual: a faded toolbar stays fully interactive and its first
    /// tap acts immediately.
    @Published private(set) var toolbarFaded = false
    static let fadedOpacity: Double = 0.7

    /// Session-log sink (set by AppModel); see `logTouchRouting`.
    var debugLog: ((String) -> Void)?

    private var fadeWork: DispatchWorkItem?
    private static let fadeDelay: TimeInterval = 3

    init() {
        toolbarCollapsed = UserDefaults.standard.bool(forKey: SettingsKey.toolbarCollapsed)
        pointerLockOn = UserDefaults.standard.bool(forKey: RemoteInputController.pointerLockDefaultsKey)
    }

    /// Diagnostics ("Log touch routing" setting): one session-log line.
    func logTouchRouting(_ text: String) {
        guard AppSettings.logTouchRouting else { return }
        debugLog?(text)
    }

    /// Any toolbar interaction: full opacity, then fade again when idle.
    func poke() {
        if toolbarFaded {
            toolbarFaded = false
        }
        fadeWork?.cancel()
        fadeWork = nil
        guard AppSettings.toolbarFade else { return }
        let work = DispatchWorkItem { [weak self] in
            self?.toolbarFaded = true
        }
        fadeWork = work
        DispatchQueue.main.asyncAfter(deadline: .now() + Self.fadeDelay, execute: work)
    }

    func toggleToolbar() {
        toolbarCollapsed.toggle()
        poke()
    }

    func togglePointerLock() {
        pointerLockOn.toggle()
    }
}

/// Full-screen host of `SessionContainer`: hidden status bar, auto-hidden
/// home indicator, deferred edge gestures, orientation setting, pointer
/// lock, and the Cmd+Shift+P / Cmd+Shift+Space key commands.
final class SessionHostingController: PointerLockHostingController<SessionContainer> {
    private let chrome: SessionChrome
    private var cancellables = Set<AnyCancellable>()

    init(rootView: SessionContainer, chrome: SessionChrome) {
        self.chrome = chrome
        super.init(rootView: rootView)
        modalPresentationStyle = .fullScreen
        modalTransitionStyle = .crossDissolve
        // Dark session chrome regardless of the app appearance.
        overrideUserInterfaceStyle = .dark
        chrome.$pointerLockOn
            .sink { [weak self] on in
                self?.wantsPointerLock = on
            }
            .store(in: &cancellables)
    }

    required init?(coder aDecoder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .black
        if AppSettings.logTouchRouting {
            let logger = TouchRoutingLogger.make()
            logger.hostController = self
            logger.log = { [weak self] text in
                self?.chrome.logTouchRouting(text)
            }
            view.addGestureRecognizer(logger)
            chrome.logTouchRouting("touch routing log on")
        }
    }

    override func viewDidAppear(_ animated: Bool) {
        super.viewDidAppear(animated)
        setNeedsUpdateOfHomeIndicatorAutoHidden()
        setNeedsUpdateOfScreenEdgesDeferringSystemGestures()
        setNeedsStatusBarAppearanceUpdate()
    }

    override var prefersStatusBarHidden: Bool {
        true
    }

    override var prefersHomeIndicatorAutoHidden: Bool {
        true
    }

    /// Edge swipes go to the remote screen first (a second swipe reaches
    /// the system).
    override var preferredScreenEdgesDeferringSystemGestures: UIRectEdge {
        .all
    }

    override var supportedInterfaceOrientations: UIInterfaceOrientationMask {
        if AppSettings.lockLandscape {
            return .landscape
        }
        return UIDevice.current.userInterfaceIdiom == .pad ? .all : .allButUpsideDown
    }

    override var keyCommands: [UIKeyCommand]? {
        let lock = UIKeyCommand(input: "p", modifierFlags: [.command, .shift],
                                action: #selector(togglePointerLockCommand(_:)))
        lock.discoverabilityTitle = "Toggle Pointer Lock"
        lock.wantsPriorityOverSystemBehavior = true
        let bar = UIKeyCommand(input: " ", modifierFlags: [.command, .shift],
                               action: #selector(toggleToolbarCommand(_:)))
        bar.discoverabilityTitle = "Show / Hide Toolbar"
        bar.wantsPriorityOverSystemBehavior = true
        var out = [lock, bar]
        if let inherited = super.keyCommands {
            out.append(contentsOf: inherited)
        }
        return out
    }

    @objc func togglePointerLockCommand(_ sender: UIKeyCommand) {
        chrome.togglePointerLock()
    }

    @objc func toggleToolbarCommand(_ sender: UIKeyCommand) {
        chrome.toggleToolbar()
    }
}

/// Diagnostics ("Log touch routing"): a passive recognizer on the session's
/// hosting view that logs, for every finger that goes down / up, the view
/// UIKit hit-tested it to, the recognizers it was delivered to (with their
/// states) and any controller presented over the session. It never
/// recognizes, never prevents (or is prevented by) other recognizers and
/// never delays or cancels touches; it fails itself once its touches end.
final class TouchRoutingLogger: UIGestureRecognizer {
    var log: ((String) -> Void)?
    weak var hostController: UIViewController?
    private var tracked = Set<ObjectIdentifier>()

    /// A logger configured to stay out of the way (no init override, so
    /// no NSCoding initializer requirements).
    static func make() -> TouchRoutingLogger {
        let r = TouchRoutingLogger(target: nil, action: nil)
        r.cancelsTouchesInView = false
        r.delaysTouchesBegan = false
        r.delaysTouchesEnded = false
        return r
    }

    override func canPrevent(_ preventedGestureRecognizer: UIGestureRecognizer) -> Bool {
        false
    }

    override func canBePrevented(by preventingGestureRecognizer: UIGestureRecognizer) -> Bool {
        false
    }

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent) {
        for t in touches {
            tracked.insert(ObjectIdentifier(t))
            log?(describe(t, phase: "down"))
        }
    }

    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent) {}

    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent) {
        finish(touches, phase: "up")
    }

    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent) {
        finish(touches, phase: "cancelled")
    }

    override func reset() {
        super.reset()
        tracked.removeAll()
    }

    private func finish(_ touches: Set<UITouch>, phase: String) {
        for t in touches {
            tracked.remove(ObjectIdentifier(t))
            log?(describe(t, phase: phase))
        }
        if tracked.isEmpty && state == .possible {
            state = .failed
        }
    }

    private func describe(_ t: UITouch, phase: String) -> String {
        let p = t.location(in: view)
        let target = t.view.map { Self.typeName($0) } ?? "nil"
        let others = (t.gestureRecognizers ?? []).filter { $0 !== self }
        let recognizers = others.map { r -> String in
            let owner = r.view.map { Self.typeName($0) } ?? "nil"
            return "\(Self.typeName(r))@\(owner):\(Self.stateName(r.state))"
        }
        let presented = hostController?.presentedViewController.map { Self.typeName($0) } ?? "none"
        return "touch \(phase) at (\(Int(p.x)), \(Int(p.y))) -> \(target); presented: \(presented); "
            + "recognizers: [\(recognizers.joined(separator: ", "))]"
    }

    /// The type name without generic arguments (SwiftUI's are huge).
    private static func typeName(_ value: Any) -> String {
        let full = String(describing: type(of: value))
        return full.split(separator: "<").first.map(String.init) ?? full
    }

    private static func stateName(_ s: UIGestureRecognizer.State) -> String {
        switch s {
        case .possible: return "possible"
        case .began: return "began"
        case .changed: return "changed"
        case .ended: return "ended"
        case .cancelled: return "cancelled"
        case .failed: return "failed"
        @unknown default: return "?"
        }
    }
}

/// Presents / dismisses the session controller from the top of the key
/// window's controller stack.
enum SessionPresenter {
    /// Present `vc` full screen. A sheet that is still closing (PIN prompt,
    /// pairing sheet) is waited for — up to ~1.5 s — so the presentation is
    /// not dropped by UIKit.
    static func present(_ vc: UIViewController, attempt: Int = 0) {
        guard let root = rootViewController() else { return }
        var top = root
        while let presented = top.presentedViewController {
            top = presented
        }
        let busy = top.isBeingDismissed || top.isBeingPresented || top !== root
        if busy && attempt < 15 {
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.1) {
                SessionPresenter.present(vc, attempt: attempt + 1)
            }
            return
        }
        top.present(vc, animated: true)
    }

    static func dismiss(_ vc: UIViewController) {
        guard vc.presentingViewController != nil else { return }
        vc.dismiss(animated: true)
    }

    private static func rootViewController() -> UIViewController? {
        let scenes = UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }
        let windows = scenes.flatMap { $0.windows }
        let window = windows.first { $0.isKeyWindow } ?? windows.first
        return window?.rootViewController
    }
}
