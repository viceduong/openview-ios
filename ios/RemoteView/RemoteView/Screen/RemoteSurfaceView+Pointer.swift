import UIKit

// iPad pointer input for RemoteSurfaceView: hover moves, indirect pointer
// clicks (button from the event's button mask) and pointer lock with raw
// GCMouse deltas.

extension RemoteSurfaceView {
    func setupPointerLock() {
        gcMouse.isLocked = { [weak self] in
            self?.pointerLocked ?? false
        }
        gcMouse.onMove = { [weak self] dx, dy in
            guard let self else { return }
            self.input.moveCursorRelative(by: CGPoint(x: dx, y: dy), raw: true)
            self.followCursorAfterMove()
        }
        gcMouse.onButton = { [weak self] button, pressed in
            guard let self else { return }
            if pressed {
                self.input.buttonDown(button)
            } else {
                self.input.buttonUp(button)
            }
        }
    }

    /// The scene's pointer is locked (pointer-lock setting on, iPad,
    /// full-screen, and the hosting controller asked for it).
    var pointerLocked: Bool {
        guard input.pointerLockEnabled else { return false }
        return window?.windowScene?.pointerLockState?.isLocked ?? false
    }

    /// A mouse / trackpad click: the button comes from the event's button
    /// mask; pressed at the pointer position.
    func pointerTouchBegan(_ t: UITouch, event: UIEvent?) {
        guard !pointerLocked else { return } // GCMouse delivers buttons then
        var button = MouseButton.left
        if let mask = event?.buttonMask {
            if mask.contains(.secondary) {
                button = .right
            } else if mask.contains(UIEvent.ButtonMask.button(3)) {
                button = .middle
            }
        }
        pointerButtons[ObjectIdentifier(t)] = button
        claimKeyboardFocus()
        input.buttonDown(button, at: viewToFrame(t.location(in: self)))
        updateCursorLayer()
    }

    func pointerTouchEnded(_ t: UITouch) {
        guard let button = pointerButtons.removeValue(forKey: ObjectIdentifier(t)) else { return }
        input.buttonUp(button, at: viewToFrame(t.location(in: self)))
        updateCursorLayer()
    }

    @objc func handleHover(_ g: UIHoverGestureRecognizer) {
        guard g.state == .began || g.state == .changed, !pointerLocked else { return }
        input.moveCursorAbsolute(to: viewToFrame(g.location(in: self)))
        updateCursorLayer()
    }
}
