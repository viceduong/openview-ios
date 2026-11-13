import Combine
import SwiftUI
import UIKit

// The immersive session host. The session is presented full
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

    private var fadeWork: DispatchWorkItem?
    private static let fadeDelay: TimeInterval = 3

    init() {
        toolbarCollapsed = UserDefaults.standard.bool(forKey: SettingsKey.toolbarCollapsed)
        pointerLockOn = UserDefaults.standard.bool(forKey: RemoteInputController.pointerLockDefaultsKey)
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
