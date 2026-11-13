import UIKit
import UIKit.UIGestureRecognizerSubclass

// MARK: - Trackpad touch source

/// The one-finger input:
/// reports the tracked finger from touch-down and on every touchesMoved —
/// there is no recognition slop and no dependency on other recognizers, so
/// cursor motion starts with the first movement event. It never leaves
/// `.possible` while tracking (it neither blocks nor delays any other
/// recognizer) and fails when the finger lifts, the touch is cancelled or
/// a second finger lands; what a touch means (move / tap / drag / long
/// press) is decided by the owner from these reports.
final class TrackpadTouchRecognizer: UIGestureRecognizer {
    enum Phase {
        case began
        case moved
        case ended
        case cancelled
    }

    /// (phase, location in the view, touch timestamp).
    var onTouch: ((Phase, CGPoint, TimeInterval) -> Void)?

    private var tracked: UITouch?
    private var lastLocation: CGPoint = .zero
    private var lastTime: TimeInterval = 0

    /// Fingers (not pointer touches) currently down on the view.
    private func activeFingers(_ event: UIEvent) -> Int {
        guard let v = view else { return 0 }
        let touches = event.touches(for: v) ?? []
        return touches.filter {
            $0.type != .indirectPointer && $0.phase != .ended && $0.phase != .cancelled
        }.count
    }

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent) {
        super.touchesBegan(touches, with: event)
        if tracked != nil {
            // A second finger: this is a two-finger gesture now.
            cancelTracking()
            state = .failed
            return
        }
        guard touches.count == 1, let t = touches.first, activeFingers(event) <= 1 else {
            state = .failed
            return
        }
        tracked = t
        lastLocation = t.location(in: view)
        lastTime = t.timestamp
        onTouch?(.began, lastLocation, lastTime)
    }

    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent) {
        super.touchesMoved(touches, with: event)
        guard let t = tracked, touches.contains(t) else { return }
        let p = t.location(in: view)
        guard p != lastLocation else { return }
        lastLocation = p
        lastTime = t.timestamp
        onTouch?(.moved, p, lastTime)
    }

    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent) {
        super.touchesEnded(touches, with: event)
        guard let t = tracked, touches.contains(t) else { return }
        let p = t.location(in: view)
        tracked = nil
        onTouch?(.ended, p, t.timestamp)
        state = .failed
    }

    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent) {
        super.touchesCancelled(touches, with: event)
        guard let t = tracked, touches.contains(t) else { return }
        cancelTracking()
        state = .failed
    }

    /// Forced out by the system (another recognizer, ...):
    /// the owner releases whatever the touch pressed.
    override func reset() {
        super.reset()
        cancelTracking()
    }

    private func cancelTracking() {
        guard tracked != nil else { return }
        tracked = nil
        onTouch?(.cancelled, lastLocation, lastTime)
    }
}

/// One trackpad touch (owned by RemoteSurfaceView).
struct TrackpadTouch {
    /// A tracked finger is down (false: idle, or ignored to its end).
    var active = false
    var downLocation: CGPoint = .zero
    var lastLocation: CGPoint = .zero
    /// Where a tap / long press of this touch clicks: the predicted
    /// cursor at touch-down (or the previous tap's pixel for a second tap).
    var downCursor: CGPoint = .zero
    /// Travelled more than the tap slop: not a tap / long press any more.
    var moved = false
    /// The long press fired (right click): the lift is not a tap.
    var longPressed = false
    /// Began within the tap window after a tap: lift = double click, hold
    /// 0.3 s or move past the slop = left drag.
    var pendingDrag = false
    /// pendingDrag: the taps counted before this touch.
    var priorTaps = 0
    /// pendingDrag: cursor travel held back until the drag has started.
    var heldTravel: CGPoint = .zero
}
