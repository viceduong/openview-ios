import GameController
import SwiftUI
import UIKit

// iPadOS hardware pointer support (section 2). Every API used here is
// iOS 13.4 / 14.0, below the 15.6 deployment target, so no availability
// checks are needed.
//
// - `PointerHider`: UIPointerInteraction delegate hiding the system pointer
//   over the remote screen (the remote cursor is drawn instead).
// - `GCMouseBridge`: raw mouse deltas / buttons from GameController, used
//   only while the scene's pointer is locked (opt-in setting
//   "ov.pointerLock").
// - `PointerLockHostingController`: a hosting controller that can request
//   pointer lock. iPadOS grants the lock only to a full-screen app's
//   foreground scene, and the request must come from the scene's root /
//   presented view controller; the current NavigationView layout does not
//   use it yet (section 3's immersive session should host its content in
//   it and set `wantsPointerLock`).

/// Hides the system pointer over the view it is installed on.
final class PointerHider: NSObject, UIPointerInteractionDelegate {
    var hidesPointer = true

    func pointerInteraction(_ interaction: UIPointerInteraction,
                            styleFor region: UIPointerRegion) -> UIPointerStyle? {
        hidesPointer ? UIPointerStyle.hidden() : nil
    }
}

/// Raw mouse input from GameController. Handlers fire on main and only
/// while `isLocked()` says the pointer is locked (unlocked, UIKit's hover /
/// pointer touches drive the cursor instead).
final class GCMouseBridge {
    /// Whether the scene's pointer is currently locked.
    var isLocked: () -> Bool = { false }
    /// Raw motion: (dx, dy) in mouse counts, y pointing down.
    var onMove: ((CGFloat, CGFloat) -> Void)?
    /// Button transitions.
    var onButton: ((MouseButton, Bool) -> Void)?

    private var observers: [NSObjectProtocol] = []
    private var mice: [GCMouse] = []

    deinit {
        stop()
    }

    func start() {
        guard observers.isEmpty else { return }
        let nc = NotificationCenter.default
        observers.append(nc.addObserver(forName: .GCMouseDidConnect, object: nil, queue: .main) { [weak self] note in
            guard let mouse = note.object as? GCMouse else { return }
            self?.attach(mouse)
        })
        observers.append(nc.addObserver(forName: .GCMouseDidDisconnect, object: nil, queue: .main) { [weak self] note in
            guard let mouse = note.object as? GCMouse else { return }
            self?.detach(mouse)
        })
        for mouse in GCMouse.mice() {
            attach(mouse)
        }
    }

    func stop() {
        let nc = NotificationCenter.default
        for o in observers {
            nc.removeObserver(o)
        }
        observers.removeAll()
        for mouse in mice {
            clearHandlers(mouse)
        }
        mice.removeAll()
    }

    private func attach(_ mouse: GCMouse) {
        guard !mice.contains(where: { $0 === mouse }), let mi = mouse.mouseInput else { return }
        mice.append(mouse)
        mi.mouseMovedHandler = { [weak self] _, dx, dy in
            // GameController's y axis points up; the remote screen's down.
            self?.deliver { $0.onMove?(CGFloat(dx), CGFloat(-dy)) }
        }
        mi.leftButton.pressedChangedHandler = { [weak self] _, _, pressed in
            self?.deliver { $0.onButton?(.left, pressed) }
        }
        mi.rightButton?.pressedChangedHandler = { [weak self] _, _, pressed in
            self?.deliver { $0.onButton?(.right, pressed) }
        }
        mi.middleButton?.pressedChangedHandler = { [weak self] _, _, pressed in
            self?.deliver { $0.onButton?(.middle, pressed) }
        }
    }

    private func detach(_ mouse: GCMouse) {
        clearHandlers(mouse)
        mice.removeAll { $0 === mouse }
    }

    private func clearHandlers(_ mouse: GCMouse) {
        guard let mi = mouse.mouseInput else { return }
        mi.mouseMovedHandler = nil
        mi.leftButton.pressedChangedHandler = nil
        mi.rightButton?.pressedChangedHandler = nil
        mi.middleButton?.pressedChangedHandler = nil
    }

    /// Run on main, only while locked.
    private func deliver(_ work: @escaping (GCMouseBridge) -> Void) {
        if Thread.isMainThread {
            if isLocked() { work(self) }
        } else {
            DispatchQueue.main.async { [weak self] in
                guard let self, self.isLocked() else { return }
                work(self)
            }
        }
    }
}

/// A UIHostingController that asks for pointer lock while
/// `wantsPointerLock` is set (not wired yet — see the file comment).
final class PointerLockHostingController<Content: View>: UIHostingController<Content> {
    var wantsPointerLock = false {
        didSet {
            guard wantsPointerLock != oldValue else { return }
            setNeedsUpdateOfPrefersPointerLocked()
        }
    }

    override var prefersPointerLocked: Bool {
        wantsPointerLock
    }
}
