import UIKit

// Touch gestures of RemoteSurfaceView: the one-finger trackpad model
// (moves, taps, double-tap-and-drag, long press), multi-finger taps,
// two-finger scrolling, pinch zoom and the recognizer delegate.

extension RemoteSurfaceView {
    // MARK: Gesture handlers

    var isPinching: Bool {
        pinch.state == .began || pinch.state == .changed
    }

    @objc func handleTap2(_ g: UITapGestureRecognizer) {
        guard g.state == .ended else { return }
        clickWithFingers(.right)
    }

    @objc func handleTap3(_ g: UITapGestureRecognizer) {
        guard g.state == .ended else { return }
        clickWithFingers(.middle)
    }

    /// Multi-finger taps: click at the cursor.
    func clickWithFingers(_ button: MouseButton) {
        input.click(button)
        updateCursorLayer()
    }

    // MARK: Trackpad (one finger)

    // Trackpad model: the cursor follows the
    // finger from its first movement (delta from the previous location;
    // the previous location is reset at touch-down, no slop, no speed
    // factor or acceleration), and what the touch meant is decided
    // afterwards:
    //   lift, never moved 10 pt, no long press   a tap: the tap count goes
    //                                             up and a 0.3 s timer starts;
    //                                             when it fires, 1 tap = click,
    //                                             2 = double click (3 = triple)
    //                                             at the cursor pixel of the
    //                                             first tap's touch-down
    //   held 0.3 s, not moved 10 pt               right click (haptic)
    //   touch-down < 0.3 s after a tap, then      left drag: DOWN at the
    //     move 10 pt (or hold 0.3 s)               tapped pixel (haptic), UP
    //                                             on lift ("Double-tap and
    //                                             drag to select")
    //   a second finger anywhere                  cancels pending taps / the
    //                                             long press
    // Sub-slop jitter moves the cursor (zero latency), but taps / long
    // presses click at the touch-down pixel, so double clicks stay exact.

    /// Finger travel (points) that makes a touch a move instead of a tap.
    static let tapSlop: CGFloat = 10
    static let longPressDelay: TimeInterval = 0.30
    /// Touch scrolling: one wheel notch (120 units) per 10 pt.
    static let scrollStepPoints: CGFloat = 10

    func handleTrackpad(_ phase: TrackpadTouchRecognizer.Phase, _ loc: CGPoint, _ time: TimeInterval) {
        switch phase {
        case .began:
            trackpadBegan(loc, time)
        case .moved:
            trackpadMoved(loc, time)
        case .ended:
            trackpadEnded(loc, time)
        case .cancelled:
            trackpadCancelled()
        }
    }

    func trackpadBegan(_ loc: CGPoint, _ time: TimeInterval) {
        cancelTrackpadTimer()
        tp = TrackpadTouch()
        // A finger left over from (or right after) a multi-finger gesture
        // is ignored to its end.
        guard input.hasCursor, !inMultiFingerAftermath, !isPinching else {
            cancelPendingTaps()
            return
        }
        tp.active = true
        tp.downLocation = loc
        tp.lastLocation = loc
        tp.downCursor = input.cursor
        let now = CACurrentMediaTime()
        if let last = lastTap, now - last.time < Self.tapWindow, !dragActive {
            // A further tap, or a double-tap-and-drag: the counted clicks
            // wait for this touch's outcome.
            tapTimer?.cancel()
            tapTimer = nil
            tp.pendingDrag = true
            tp.priorTaps = last.count
            tp.downCursor = last.cursor
            scheduleTrackpadTimer(after: Self.longPressDelay) { [weak self] in
                self?.beginTrackpadDrag()
            }
        } else {
            // Taps whose window just closed but whose timer has not run
            // yet go out now (never dropped); this touch starts afresh.
            if lastTap != nil, tapTimer != nil {
                tapTimer?.cancel()
                fireCountedTaps()
            }
            cancelPendingTaps()
            scheduleTrackpadTimer(after: Self.longPressDelay) { [weak self] in
                self?.trackpadLongPress()
            }
        }
    }

    func trackpadMoved(_ loc: CGPoint, _ time: TimeInterval) {
        guard tp.active else { return }
        let d = CGPoint(x: loc.x - tp.lastLocation.x, y: loc.y - tp.lastLocation.y)
        tp.lastLocation = loc
        if !tp.moved && hypot(loc.x - tp.downLocation.x, loc.y - tp.downLocation.y) > Self.tapSlop {
            tp.moved = true
            if !tp.pendingDrag {
                cancelTrackpadTimer() // no long press any more
            }
        }
        if isPinching || multiTouchSequence { return }
        if tp.pendingDrag && !dragActive {
            // A touch right after a tap: either one more tap (keep the
            // cursor on the tapped pixel) or a drag (starts once it
            // travelled past the slop; the held travel is applied then).
            tp.heldTravel.x += d.x
            tp.heldTravel.y += d.y
            if tp.moved {
                beginTrackpadDrag()
            }
            return
        }
        moveCursor(byViewPoints: d)
    }

    func trackpadEnded(_ loc: CGPoint, _ time: TimeInterval) {
        cancelTrackpadTimer()
        var t = tp
        tp = TrackpadTouch()
        guard t.active else { return }
        if hypot(loc.x - t.downLocation.x, loc.y - t.downLocation.y) > Self.tapSlop {
            t.moved = true
        }
        if dragActive {
            endTrackpadDrag()
            return
        }
        if t.moved || t.longPressed {
            cancelPendingTaps()
            return
        }
        let now = CACurrentMediaTime()
        guard !multiTouchSequence, now - multiTouchEnd >= Self.multiFingerLiftDebounce else {
            cancelPendingTaps()
            return
        }
        // Count the tap and answer when no further tap
        // follows within the tap window. The cursor goes back to the
        // tapped pixel at once (it is where the click will land).
        let count = min(t.priorTaps + 1, 3)
        if input.cursor != t.downCursor {
            input.moveCursorAbsolute(to: t.downCursor)
            updateCursorLayer()
        }
        lastTap = (count: count, cursor: t.downCursor, time: now)
        let work = DispatchWorkItem { [weak self] in
            self?.fireCountedTaps()
        }
        tapTimer?.cancel()
        tapTimer = work
        DispatchQueue.main.asyncAfter(deadline: .now() + Self.tapWindow, execute: work)
    }

    /// The tap timer fired: send the counted clicks at the tapped pixel
    /// (Windows sees a double click: two clicks at the same pixel well
    /// within its double-click time).
    func fireCountedTaps() {
        tapTimer = nil
        guard let taps = lastTap else { return }
        lastTap = nil
        guard !dragActive, !multiTouchSequence else { return }
        for _ in 0..<max(1, min(taps.count, 3)) {
            input.click(.left, at: taps.cursor)
        }
        updateCursorLayer()
    }

    /// Drop taps not yet answered (and the tap timer): a second
    /// finger, a move, a long press or teardown.
    func cancelPendingTaps() {
        tapTimer?.cancel()
        tapTimer = nil
        lastTap = nil
    }

    func trackpadCancelled() {
        cancelTrackpadTimer()
        cancelPendingTaps()
        tp = TrackpadTouch()
        endTrackpadDrag()
    }

    /// Hold timer (0.3 s, not moved): right click at the cursor.
    func trackpadLongPress() {
        tpTimer = nil
        guard tp.active, !tp.moved, !tp.pendingDrag, !tp.longPressed,
              !multiTouchSequence, !isPinching else { return }
        tp.longPressed = true
        cancelPendingTaps()
        haptic.impactOccurred()
        input.click(.right, at: trackpadClickPoint(tp))
        updateCursorLayer()
    }

    /// Double-tap and drag: left button down at the tapped pixel (haptic);
    /// the travel held back meanwhile is applied at once.
    func beginTrackpadDrag() {
        cancelTrackpadTimer()
        guard tp.active, tp.pendingDrag, !dragActive,
              !multiTouchSequence, !isPinching else { return }
        cancelPendingTaps()
        dragActive = true
        haptic.impactOccurred()
        input.buttonDown(.left, at: tp.downCursor)
        setDragRingVisible(true)
        let held = tp.heldTravel
        tp.heldTravel = .zero
        if held != .zero {
            moveCursor(byViewPoints: held)
        } else {
            updateCursorLayer()
        }
    }

    func endTrackpadDrag() {
        guard dragActive else { return }
        dragActive = false
        input.buttonUp(.left)
        setDragRingVisible(false)
        updateCursorLayer()
    }

    /// Where a tap / long press of `t` clicks: its touch-down pixel when
    /// the cursor has moved since (sub-slop jitter), else nil (the cursor).
    func trackpadClickPoint(_ t: TrackpadTouch) -> CGPoint? {
        input.cursor == t.downCursor ? nil : t.downCursor
    }

    func scheduleTrackpadTimer(after delay: TimeInterval, _ action: @escaping () -> Void) {
        cancelTrackpadTimer()
        let work = DispatchWorkItem(block: action)
        tpTimer = work
        DispatchQueue.main.asyncAfter(deadline: .now() + delay, execute: work)
    }

    func cancelTrackpadTimer() {
        tpTimer?.cancel()
        tpTimer = nil
    }

    /// Touch scrolling: finger travel accumulates and every whole
    /// 10 pt is one wheel notch (120 units), natural direction (content
    /// follows the finger), horizontal too.
    func stepWheel(byViewPoints t: CGPoint) {
        guard t.x.isFinite, t.y.isFinite else { return }
        scrollStepAcc.x += t.x
        scrollStepAcc.y += t.y
        let sx = (scrollStepAcc.x / Self.scrollStepPoints).rounded(.towardZero)
        let sy = (scrollStepAcc.y / Self.scrollStepPoints).rounded(.towardZero)
        guard sx != 0 || sy != 0 else { return }
        scrollStepAcc.x -= sx * Self.scrollStepPoints
        scrollStepAcc.y -= sy * Self.scrollStepPoints
        // Fingers down / right = content down / right = wheel up / left.
        input.wheel(dx: Double(-sx * 120), dy: Double(sy * 120))
    }

    /// iPad trackpad scrolling: glide on after the release.
    func startScrollFling(velocity v: CGPoint) {
        scrollFling.start(velocity: v, now: CACurrentMediaTime())
        if scrollFling.isActive { animTicker.start() }
    }

    /// Two-finger pan and iPad trackpad / wheel scrolling -> WHEEL.
    /// Two-finger pan: at the cursor, one notch per 10 pt, no inertia;
    /// iPad pointer scrolling stays continuous (trackpads
    /// glide on after release).
    @objc func handleScroll(_ g: UIPanGestureRecognizer) {
        let touchPan = g === pan2
        switch g.state {
        case .began:
            scrollFling.stop()
            scrollStepAcc = .zero
        case .changed:
            let t = g.translation(in: self)
            g.setTranslation(.zero, in: self)
            if touchPan {
                stepWheel(byViewPoints: t)
            } else {
                sendWheel(byViewPoints: t, factor: Self.wheelUnitsPerPoint)
            }
        case .ended:
            guard !touchPan, g !== scrollDiscrete, input.input2 else { return }
            startScrollFling(velocity: g.velocity(in: self))
        default:
            break
        }
    }

    /// Natural scrolling: fingers down (t.y > 0) = content down = scroll
    /// up (dy > 0); fingers right = content right = scroll left (dx < 0).
    func sendWheel(byViewPoints t: CGPoint, factor k: CGFloat) {
        input.wheel(dx: Double(-t.x * k), dy: Double(t.y * k))
    }

    @objc func handlePinch(_ g: UIPinchGestureRecognizer) {
        if input.lockLocalZoom || pinchCtrlHeld {
            handleRemotePinch(g)
            return
        }
        switch g.state {
        case .began:
            stopFlings()
        case .changed:
            let s = g.scale
            g.scale = 1
            // Around the cursor; zoomed in, the viewport eases it to the
            // centre.
            zoom(by: s, pivot: frameToView(input.cursor))
            settleViewport(animated: true)
        case .ended:
            snapZoom(pivot: frameToView(input.cursor))
            settleViewport(animated: true)
        default:
            break
        }
    }

    /// Lock local zoom: pinch = Ctrl+wheel on the host (one notch per 10%
    /// scale change), at the cursor.
    func handleRemotePinch(_ g: UIPinchGestureRecognizer) {
        switch g.state {
        case .began:
            stopFlings()
            guard input.input2 else { return } // no wheel on legacy hosts
            pinchCtrlHeld = true
            input.sendKey(.leftCtrl, down: true)
        case .changed:
            let s = g.scale
            g.scale = 1
            guard pinchCtrlHeld, s.isFinite, s > 0 else { return }
            let notches = log(Double(s)) / log(Self.pinchNotchScale)
            input.wheel(dx: 0, dy: notches * 120)
        default:
            guard pinchCtrlHeld else { return }
            pinchCtrlHeld = false
            input.flushWheel()
            input.sendKey(.leftCtrl, down: false)
        }
    }
}

extension RemoteSurfaceView: UIGestureRecognizerDelegate {
    // MARK: UIGestureRecognizerDelegate

    /// Pinch and the two-finger pan (wheel) exclude each other; the
    /// trackpad recognizer is simultaneous with everything
    /// (it never recognizes; others must not force it out). Pointer recognizers
    /// (hover, scroll) never block anything. Recognizers of ancestor views
    /// (SwiftUI's hosting-view gesture recognizer) are never blocked
    /// either: by default a surface recognizer
    /// that recognizes forces them to fail for the whole touch sequence,
    /// and while any finger stays on the remote screen every new touch on
    /// the toolbar would join that failed sequence and be ignored. Window
    /// (system) recognizers keep the default exclusivity.
    func gestureRecognizer(_ gestureRecognizer: UIGestureRecognizer,
                           shouldRecognizeSimultaneouslyWith other: UIGestureRecognizer) -> Bool {
        if let v = other.view, v !== self, !(v is UIWindow), isDescendant(of: v) {
            return true
        }
        let g = gestureRecognizer
        if g === trackpad || other === trackpad { return true }
        if g === hover || other === hover { return true }
        if g === scrollContinuous || other === scrollContinuous { return true }
        if g === scrollDiscrete || other === scrollDiscrete { return true }
        return false
    }
}
