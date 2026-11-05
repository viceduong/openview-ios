import Combine
import QuartzCore
import SwiftUI
import UIKit
import UIKit.UIGestureRecognizerSubclass

// The remote screen surface (section 2): frame + H.264 overlay inside one
// zoom/pan-transformed container layer, a locally drawn cursor, the touch /
// pointer / keyboard input layer and the viewport. Self-contained so the
// immersive session (section 3) can re-host it.
//
// Gestures (CRD structure, timings tuned to TeamViewer iOS 15.81). Mouse
// (trackpad) input is the only touch interpretation:
//
//     1-finger pan        TeamViewer 15.81 model: normalised position
//                         pct += finger travel (screen points) / displayed
//                         image size (points), clamped, sent as ABSOLUTE
//                         moves (RemoteInputController.moveCursorTrackpad).
//                         At fit, sliding across the image crosses the
//                         whole remote screen; zoomed in by z, 1 pt = 1/z
//                         remote px. Linear, 1:1 (no speed factor, no
//                         acceleration, no cursor inertia). Zoomed in, the
//                         viewport keeps the cursor centred, but never
//                         scrolls past the image edges (near an edge the
//                         cursor moves off-centre towards it).
//                         One custom recognizer (TrackpadTouchRecognizer,
//                         the MouseGestureRecognizer model) moves the cursor
//                         from the very first touchesMoved (no pan slop, no
//                         waiting for a tap to fail); tap / drag / long
//                         press are decided afterwards, see "Trackpad
//                         (one finger)".
//     tap                 a touch that never travelled 10 pt: counted; 0.3 s
//                         after the last tap lifts, 1 tap = left click, 2 =
//                         double click, at the cursor (the tapped pixel:
//                         sub-slop jitter is undone)
//     double-tap and drag second touch-down < 0.3 s after a tap, then move
//                         10 pt (or hold 0.3 s) = left drag, DOWN at the
//                         tapped pixel (haptic), UP on lift, + ring
//     long-press 0.30 s   right click at the cursor (10 pt cancel radius)
//     2-finger tap        right click (instant: no double tap to wait for);
//                         3-finger tap: middle click. Any second finger
//                         cancels pending taps / the long press
//     2-finger pan        wheel at the cursor: one notch (120) per 10 pt,
//                         natural direction, both axes, no inertia
//     pinch               zoom around the cursor; snaps to fit / fill / 1:1
//                         within +-20% on release. Zoom range (TeamViewer):
//                         fit inside the safe area ... max(2 pt/px, fit)
//   LOCK LOCAL ZOOM (setting "ov.lockLocalZoom"): pinch becomes Ctrl+wheel
//   on the host (the cursor stays where it is); everything else as above.
//   Apple Pencil touches are handled exactly like a finger.
//   After a multi-finger gesture the remaining finger is ignored until
//   every finger lifted + 0.1 s (no stray cursor moves / clicks).
//   Fit / fill / 1:1 are menu commands (RemoteInputController.applyZoomPreset).
//   iPad pointer: hover = absolute move, clicks / right clicks via indirect
//   pointer touches, trackpad / wheel scroll = WHEEL (trackpad scrolling
//   glides on after release), optional raw GCMouse
//   deltas while the pointer is locked.
//
// Every DOWN gets its UP: positions are clamped to the frame (never
// dropped) and cancelled / failed gestures release what they pressed.
//
// Coexistence with the SwiftUI chrome drawn over this view (v0.40): the
// surface consumes its touches (never forwards them up the responder
// chain into the hosting view), never takes touches inside the chrome's
// frames (`RemoteInputController.chromeExclusionRects`), and its
// recognizers never block recognizers of ancestor views (SwiftUI's hosting
// view gesture recognizer drives every SwiftUI Button in the session).

/// SwiftUI wrapper. The parent observes ScreenModel and passes values in.
struct RemoteScreenSurface: UIViewRepresentable {
    let image: CGImage
    /// Section 4: the frame's BGRA bytes + dirty rects for the Metal path.
    let pixels: FramePixelStore
    let frameSize: CGSize
    let video: VideoOverlay
    let cursorPos: CGPoint?
    let cursorImage: CGImage?
    let cursorHotspot: CGSize
    let cursorSize: CGSize
    let input: RemoteInputController

    func makeUIView(context: Context) -> RemoteSurfaceView {
        let view = RemoteSurfaceView(input: input)
        view.attachVideo(video)
        return view
    }

    func updateUIView(_ uiView: RemoteSurfaceView, context: Context) {
        uiView.attachVideo(video)
        uiView.setFrame(image, size: frameSize, pixels: pixels)
        uiView.setCursorShape(cursorImage, hotspot: cursorHotspot, size: cursorSize)
        if let p = cursorPos {
            uiView.hostCursorMoved(to: p)
        }
    }

    static func dismantleUIView(_ uiView: RemoteSurfaceView, coordinator: ()) {
        uiView.teardown()
    }
}

// MARK: - Fling

/// iPad trackpad scroll inertia (finger gestures have none).
/// CRD FlingTracker: exponential decay x(t) = T·v0·(1 − e^(−t/T)), stops
/// when the speed falls below 0.01 pt/ms (duration = −T·ln(0.01/v0)); no
/// fling when the total displacement would be under 20 pt.
struct FlingTracker {
    /// Time constant, ms.
    let timeConstant: Double
    private var speed: Double = 0 // pt/ms
    private var ratioX: Double = 0
    private var ratioY: Double = 0
    private var duration: Double = 0 // ms
    private var startTime: CFTimeInterval = 0
    private var prevX: Double = 0
    private var prevY: Double = 0

    private static let minSpeed = 0.01
    private static let minDisplacement = 20.0

    init(timeConstant: Double) {
        self.timeConstant = timeConstant
    }

    var isActive: Bool { speed > 0 }

    /// Start from a release velocity in points per second.
    mutating func start(velocity v: CGPoint, now: CFTimeInterval) {
        let vx = Double(v.x) / 1000
        let vy = Double(v.y) / 1000
        let s = (vx * vx + vy * vy).squareRoot()
        guard s.isFinite, s >= Self.minSpeed else {
            stop()
            return
        }
        let dur = -timeConstant * log(Self.minSpeed / s)
        let total = timeConstant * s * -expm1(-dur / timeConstant)
        guard total >= Self.minDisplacement else {
            stop()
            return
        }
        speed = s
        ratioX = vx / s
        ratioY = vy / s
        duration = dur
        startTime = now
        prevX = 0
        prevY = 0
    }

    mutating func stop() {
        speed = 0
    }

    /// The displacement (points) since the previous step; nil when done.
    mutating func step(now: CFTimeInterval) -> CGPoint? {
        guard speed > 0 else { return nil }
        let t = max(0, (now - startTime) * 1000)
        if t > duration {
            speed = 0
            return nil
        }
        let d = timeConstant * speed * -expm1(-t / timeConstant)
        let x = d * ratioX
        let y = d * ratioY
        let out = CGPoint(x: x - prevX, y: y - prevY)
        prevX = x
        prevY = y
        return out
    }
}

// MARK: - Trackpad touch source

/// The one-finger input (TeamViewer's MouseGestureRecognizer):
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

// MARK: - Surface view

final class RemoteSurfaceView: UIView, UIGestureRecognizerDelegate, RemoteKeyCommandTarget {
    let input: RemoteInputController
    let keyRouter: HardwareKeyRouter
    let keyInput: RemoteKeyInputView

    // ---- layers ----
    /// Zoom/pan-transformed container: bounds = frame pixels, transform =
    /// scale (points per pixel), position = view offset of the frame origin.
    private let container = CALayer()
    /// The CGImage frame (fallback when Metal is unavailable; hidden
    /// otherwise — the Metal layer below the container draws the frame).
    private let frameLayer = CALayer()
    /// Section 4: TeamViewer-style scaler (nil = no Metal: CGImage path).
    private let metal = MetalScreenRenderer()
    private weak var pixelStore: FramePixelStore?
    /// A redraw is wanted at the next display refresh.
    private var needsRender = false
    private var idleRenderTicks = 0
    /// Hide the H.264 overlay of this generation once the next drawable
    /// (which holds the lossless refresh) has been presented.
    private var hideVideoAfterPresent: Int?
    private lazy var renderTicker = FrameTicker { [weak self] _ in
        self?.renderTick()
    }
    private weak var video: VideoOverlay?
    private let cursorLayer = CALayer()
    private let dragRing = CAShapeLayer()

    // ---- frame / cursor shape ----
    private var image: CGImage?
    private var frameSize: CGSize = .zero
    private var cursorImage: CGImage?
    private var cursorHotspot: CGSize = .zero
    private var cursorPixelSize: CGSize = .zero
    private var shownCursorImage: CGImage?
    private var lastHostCursor: CGPoint?

    // ---- viewport ----
    /// Points per remote pixel (0 until laid out with a frame).
    private var scale: CGFloat = 0
    /// View position of the frame's top-left corner.
    private var offset: CGPoint = .zero
    /// The preset the viewport follows when the view / frame size changes
    /// (nil = free zoom, kept as is).
    private var followPreset: ZoomPreset? = .fit
    private var lastBoundsSize: CGSize = .zero
    /// Bottom edge (window y) of the top chrome (stats chip), 0 = none:
    /// the image is laid out below it (`safeInsets`).
    private var topChromeBottom: CGFloat = 0
    private var topChromeCancellable: AnyCancellable?

    // ---- gestures ----
    private let tap2 = UITapGestureRecognizer(target: nil, action: nil)
    private let tap3 = UITapGestureRecognizer(target: nil, action: nil)
    private let pan2 = UIPanGestureRecognizer(target: nil, action: nil)
    private let pinch = UIPinchGestureRecognizer(target: nil, action: nil)
    /// The one-finger trackpad (moves, taps, drags, long press).
    private let trackpad = TrackpadTouchRecognizer(target: nil, action: nil)
    private let hover = UIHoverGestureRecognizer(target: nil, action: nil)
    private let scrollContinuous = UIPanGestureRecognizer(target: nil, action: nil)
    private let scrollDiscrete = UIPanGestureRecognizer(target: nil, action: nil)

    private let haptic = UIImpactFeedbackGenerator(style: .medium)
    private let pointerHider = PointerHider()
    private let gcMouse = GCMouseBridge()

    /// A double-tap-and-drag holds the left button.
    private var dragActive = false
    /// Trackpad touch (TrackpadTouchRecognizer) state.
    private var tp = TrackpadTouch()
    /// Trackpad taps not yet answered: how many, the cursor pixel they
    /// click, and when the last one lifted. A touch starting within
    /// `tapWindow` of that is a further tap or a double-tap-and-drag.
    private var lastTap: (count: Int, cursor: CGPoint, time: CFTimeInterval)?
    /// TeamViewer tap timer: fires `tapWindow` after the last tap lifted
    /// and sends the counted clicks.
    private var tapTimer: DispatchWorkItem?
    /// Long-press (right click) / double-tap-and-hold (drag) timer.
    private var tpTimer: DispatchWorkItem?
    /// Two-finger wheel scrolling: finger travel (points)
    /// not yet emitted as whole 10 pt wheel steps.
    private var scrollStepAcc: CGPoint = .zero
    /// Indirect pointer touch -> the button it pressed.
    private var pointerButtons: [ObjectIdentifier: MouseButton] = [:]
    /// Multi-finger lift debounce: a multi-finger touch sequence is in
    /// progress (until every finger lifted), and when the last one ended.
    private var multiTouchSequence = false
    private var multiTouchEnd: CFTimeInterval = 0
    /// Lock local zoom: Ctrl held on the host for a pinch (Ctrl+wheel).
    private var pinchCtrlHeld = false

    // ---- animation ----
    /// iPad trackpad (continuous pointer scroll) inertia.
    private var scrollFling = FlingTracker(timeConstant: 250)
    /// Zoomed in: the cursor-centred offset the viewport is
    /// easing towards (nil = not animating).
    private var followTarget: CGPoint?
    /// Timestamp of the previous animation tick (0 = none yet).
    private var lastAnimTime: CFTimeInterval = 0
    private lazy var animTicker = FrameTicker { [weak self] now in
        self?.animate(now)
    }

    // ---- constants ----
    private static let cursorMinPoints: CGFloat = 20
    private static let cursorMaxPoints: CGFloat = 64
    /// iPad pointer scrolling: wheel units (1/120 notch) per point, one
    /// notch per 36 pt.
    private static let wheelUnitsPerPoint: CGFloat = 120.0 / 36.0
    /// Max zoom (TeamViewer): 2 view points per remote pixel, or fit when
    /// that is larger.
    private static let maxZoomScale: CGFloat = 2
    /// Smooth centring (viewport catching up after a jump: resync, zoom,
    /// preset), exponential time constant in seconds.
    private static let followTimeConstant: Double = 0.08
    /// Pinch release snaps to fit / fill / 1:1 within this ratio.
    private static let snapTolerance: CGFloat = 0.2
    /// Tap windows (double click at the same pixel, drag-end tap swallow).
    private static let tapWindow: CFTimeInterval = 0.3
    /// Ignore the remaining finger this long after a multi-finger gesture.
    private static let multiFingerLiftDebounce: CFTimeInterval = 0.1
    /// Lock local zoom: pinch scale change per wheel notch (10%).
    private static let pinchNotchScale: Double = 1.1
    private static let defaultCursorSize = CGSize(width: 12, height: 19)

    private static let noActions: [String: CAAction] = [
        "position": NSNull(), "bounds": NSNull(), "frame": NSNull(), "transform": NSNull(),
        "contents": NSNull(), "contentsRect": NSNull(), "hidden": NSNull(), "anchorPoint": NSNull(),
        "opacity": NSNull(), "sublayers": NSNull(), "path": NSNull(),
    ]

    init(input: RemoteInputController) {
        self.input = input
        let router = HardwareKeyRouter()
        self.keyRouter = router
        self.keyInput = RemoteKeyInputView(input: input, router: router)
        super.init(frame: .zero)
        backgroundColor = .black
        clipsToBounds = true
        isMultipleTouchEnabled = true
        keyRouter.input = input
        setupLayers()
        addSubview(keyInput)
        setupGestures()
        addInteraction(UIPointerInteraction(delegate: pointerHider))
        setupPointerLock()
        // The stats chip appeared / moved / went away: refit below it.
        // (@Published emits before the property changes: use the value.)
        topChromeCancellable = input.$topChromeInset
            .removeDuplicates()
            .sink { [weak self] bottom in
                guard let self else { return }
                self.topChromeBottom = bottom
                self.refit()
            }
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    // MARK: Setup

    private func setupLayers() {
        if let metal {
            // Below the zoom container: drawn at device resolution for the
            // current viewport (the container still carries the H.264
            // overlay, transformed with the zoom).
            layer.addSublayer(metal.layer)
            frameLayer.isHidden = true
        }
        container.anchorPoint = .zero
        container.actions = Self.noActions
        frameLayer.actions = Self.noActions
        frameLayer.contentsGravity = .resize
        frameLayer.magnificationFilter = .linear
        frameLayer.minificationFilter = .trilinear
        frameLayer.backgroundColor = UIColor.black.cgColor
        container.addSublayer(frameLayer)
        layer.addSublayer(container)

        cursorLayer.actions = Self.noActions
        cursorLayer.contentsGravity = .resize
        cursorLayer.magnificationFilter = .nearest
        cursorLayer.isHidden = true
        layer.addSublayer(cursorLayer)

        let r: CGFloat = 16
        dragRing.actions = Self.noActions
        dragRing.path = UIBezierPath(ovalIn: CGRect(x: -r, y: -r, width: 2 * r, height: 2 * r)).cgPath
        dragRing.fillColor = UIColor.systemBlue.withAlphaComponent(0.15).cgColor
        dragRing.strokeColor = UIColor.systemBlue.cgColor
        dragRing.lineWidth = 2.5
        dragRing.isHidden = true
        layer.addSublayer(dragRing)
    }

    private func setupGestures() {
        // Touch recognizers ignore indirect pointer touches (handled in
        // touchesBegan with the button mask) and never cancel touches, so
        // touchesEnded keeps the "finger down" state exact.
        let touchTypes: [NSNumber] = [
            NSNumber(value: UITouch.TouchType.direct.rawValue),
            NSNumber(value: UITouch.TouchType.pencil.rawValue),
        ]

        tap2.addTarget(self, action: #selector(handleTap2(_:)))
        tap2.numberOfTouchesRequired = 2
        tap3.addTarget(self, action: #selector(handleTap3(_:)))
        tap3.numberOfTouchesRequired = 3
        pan2.addTarget(self, action: #selector(handleScroll(_:)))
        pan2.minimumNumberOfTouches = 2
        pan2.maximumNumberOfTouches = 2
        pinch.addTarget(self, action: #selector(handlePinch(_:)))
        // One finger (or Apple Pencil): moves from the first touchesMoved;
        // taps, double-tap-and-drag and long presses are decided by the
        // trackpad handlers (TeamViewer's MouseGestureRecognizer model).
        trackpad.onTouch = { [weak self] phase, location, time in
            self?.handleTrackpad(phase, location, time)
        }
        trackpad.delaysTouchesEnded = false

        let touchRecognizers: [UIGestureRecognizer] = [
            tap2, tap3, pan2, pinch, trackpad,
        ]
        for g in touchRecognizers {
            g.delegate = self
            g.cancelsTouchesInView = false
            g.allowedTouchTypes = touchTypes
            addGestureRecognizer(g)
        }

        // Recognizer wiring (CRD): the two-finger tap waits for the
        // three-finger tap to fail. There is no two-finger double tap, so a
        // two-finger tap right-clicks without waiting for a second tap. The
        // one-finger input (the trackpad recognizer) depends on nothing, so
        // the cursor moves from the first touchesMoved; one-finger taps,
        // drags and long presses are decided by its handlers.
        tap2.require(toFail: tap3)

        // iPad pointer: hover = absolute moves; scroll-only pans (no
        // touches) = wheel. Trackpads scroll continuously (inertia on
        // release), mouse wheels in discrete steps (no inertia).
        hover.addTarget(self, action: #selector(handleHover(_:)))
        hover.delegate = self
        addGestureRecognizer(hover)
        scrollContinuous.addTarget(self, action: #selector(handleScroll(_:)))
        scrollContinuous.allowedScrollTypesMask = .continuous
        scrollContinuous.allowedTouchTypes = []
        scrollContinuous.delegate = self
        addGestureRecognizer(scrollContinuous)
        scrollDiscrete.addTarget(self, action: #selector(handleScroll(_:)))
        scrollDiscrete.allowedScrollTypesMask = .discrete
        scrollDiscrete.allowedTouchTypes = []
        scrollDiscrete.delegate = self
        addGestureRecognizer(scrollDiscrete)
    }

    private func setupPointerLock() {
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
    private var pointerLocked: Bool {
        guard input.pointerLockEnabled else { return false }
        return window?.windowScene?.pointerLockState?.isLocked ?? false
    }

    /// Hook the controller's view-side callbacks to this surface.
    private func installInputHooks() {
        input.keyboardPresenter = { [weak self] soft in
            self?.presentKeyboard(soft)
        }
        input.onReleaseAll = { [weak self] in
            self?.keyRouter.reset()
        }
        input.onCursorResync = { [weak self] in
            self?.updateCursorLayer()
            self?.followHostCursor()
        }
        input.zoomPresetHandler = { [weak self] preset in
            self?.applyPreset(preset)
        }
        input.activeSurface = self
    }

    /// Session view gone: stop timers, release held pointer buttons and
    /// unhook from the controller (only if still the active surface).
    func teardown() {
        animTicker.stop()
        renderTicker.stop()
        gcMouse.stop()
        releasePointerButtons()
        endLocalGestures()
        if input.activeSurface === self {
            input.keyboardPresenter = nil
            input.onReleaseAll = nil
            input.onCursorResync = nil
            input.zoomPresetHandler = nil
            input.activeSurface = nil
        }
    }

    override func didMoveToWindow() {
        super.didMoveToWindow()
        if window == nil {
            animTicker.stop()
            lastAnimTime = 0
            renderTicker.stop()
            gcMouse.stop()
            releasePointerButtons()
            endLocalGestures()
            input.touching = false
        } else {
            installInputHooks()
            // The safe area (fit scale) is only known inside a window.
            refit()
            setNeedsRender()
            // Always observe mice: GCMouseBridge delivers only while the
            // scene's pointer is actually locked, and the lock can be
            // toggled during the session (Cmd+Shift+P).
            gcMouse.start()
            // Hardware keyboard input without showing any keyboard.
            DispatchQueue.main.async { [weak self] in
                self?.claimKeyboardFocus()
            }
        }
    }

    /// Drop transient gesture UI / held buttons (teardown, leaving the
    /// window).
    private func endLocalGestures() {
        cancelTrackpadTimer()
        cancelPendingTaps()
        tp = TrackpadTouch()
        if dragActive {
            dragActive = false
            input.buttonUp(.left)
            setDragRingVisible(false)
        }
        if pinchCtrlHeld {
            pinchCtrlHeld = false
            input.flushWheel()
            input.sendKey(.leftCtrl, down: false)
        }
    }

    private func releasePointerButtons() {
        for b in pointerButtons.values {
            input.buttonUp(b)
        }
        pointerButtons.removeAll()
    }

    // MARK: Model updates

    /// Host the H.264 overlay's layer inside the transformed container
    /// (above the frame image), so it zooms / pans with it.
    func attachVideo(_ overlay: VideoOverlay) {
        guard overlay.layer.superlayer !== container else { return }
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        container.addSublayer(overlay.layer)
        CATransaction.commit()
        video = overlay
        if frameSize.width > 0 {
            overlay.layout(in: container.bounds)
        }
    }

    func setFrame(_ img: CGImage, size: CGSize, pixels: FramePixelStore) {
        pixelStore = pixels
        if size != frameSize && size.width > 0 && size.height > 0 {
            frameSize = size
            CATransaction.begin()
            CATransaction.setDisableActions(true)
            container.bounds = CGRect(origin: .zero, size: size)
            frameLayer.frame = container.bounds
            CATransaction.commit()
            video?.layout(in: container.bounds)
            input.frameSize = size
            if followPreset == nil { followPreset = .fit }
            refit()
        }
        if img !== image {
            image = img
            CATransaction.begin()
            CATransaction.setDisableActions(true)
            if metal == nil {
                frameLayer.contents = img
            }
            CATransaction.commit()
            // Metal: upload what changed, redraw at the next refresh.
            if let metal, metal.sync(from: pixels) {
                setNeedsRender()
            }
        } else if let metal, !metal.hasFrame, metal.sync(from: pixels) {
            setNeedsRender()
        }
        // The frame that carries the lossless refresh after VIDEO_END is
        // now in the texture / layer: drop the H.264 overlay together with
        // showing it — with Metal only once the drawable holding it is
        // presented (renderNow), with the CGImage layer in this same
        // Core Animation commit.
        if let gen = video?.takePendingHide() {
            if metal != nil {
                hideVideoAfterPresent = gen
                setNeedsRender()
            } else {
                video?.hide(generation: gen)
            }
        }
    }

    // MARK: Metal rendering (Section 4)

    /// Ask for one redraw at the next display refresh (frame, zoom / pan or
    /// size changed). The display link runs only while redraws keep coming
    /// (pan / zoom / fling animation, up to 120 Hz) and stops when idle.
    private func setNeedsRender() {
        guard metal != nil else { return }
        needsRender = true
        idleRenderTicks = 0
        if window != nil && !renderTicker.isRunning {
            renderTicker.start()
        }
    }

    private func renderTick() {
        guard needsRender else {
            idleRenderTicks += 1
            if idleRenderTicks >= 2 {
                renderTicker.stop()
            }
            return
        }
        needsRender = false
        idleRenderTicks = 0
        renderNow()
    }

    private func renderNow() {
        guard let metal, window != nil, scale > 0, bounds.width > 0, bounds.height > 0,
              UIApplication.shared.applicationState != .background else { return }
        if !metal.hasFrame, let store = pixelStore {
            metal.sync(from: store)
        }
        var presented: (() -> Void)?
        if let gen = hideVideoAfterPresent, let overlay = video {
            hideVideoAfterPresent = nil
            presented = { [weak overlay] in
                DispatchQueue.main.async {
                    overlay?.hide(generation: gen)
                }
            }
        }
        metal.render(scale: scale, offset: offset, sharpenOn: AppSettings.sharpen, onPresented: presented)
    }

    func setCursorShape(_ img: CGImage?, hotspot: CGSize, size: CGSize) {
        guard img !== cursorImage || hotspot != cursorHotspot || size != cursorPixelSize else { return }
        cursorImage = img
        cursorHotspot = hotspot
        cursorPixelSize = size
        updateCursorLayer()
    }

    /// The host's cursor moved (frame pixels). The controller ignores it
    /// while local input is in flight.
    func hostCursorMoved(to p: CGPoint) {
        guard p != lastHostCursor else { return }
        lastHostCursor = p
        if input.hostCursorMoved(to: p) {
            updateCursorLayer()
            followHostCursor()
        }
    }

    // MARK: Viewport

    private var displayScale: CGFloat {
        let s = traitCollection.displayScale
        return s > 0 ? s : UIScreen.main.scale
    }

    /// Gap between the stats chip and the top of the remote image.
    private static let topChromeGap: CGFloat = 6

    /// The part of the view outside the window's safe area insets (notch,
    /// home indicator, status bar), in view coordinates. Computed from the
    /// window (the SwiftUI host lays this view out ignoring the safe area,
    /// and what it reports as the view's own safeAreaInsets is not
    /// guaranteed); zero outside a window. While the stats chip is shown
    /// the top inset reaches below it (+ `topChromeGap`): the image at fit /
    /// pinned to the top starts under the chip, not behind it.
    private var safeInsets: UIEdgeInsets {
        guard let window, bounds.width > 0, bounds.height > 0 else { return .zero }
        let wi = window.safeAreaInsets
        let wb = window.bounds
        let f = convert(bounds, to: window)
        var top = max(0, wi.top - f.minY)
        if topChromeBottom > 0 {
            top = max(top, topChromeBottom + Self.topChromeGap - f.minY)
        }
        return UIEdgeInsets(top: top,
                            left: max(0, wi.left - f.minX),
                            bottom: max(0, f.maxY - (wb.maxY - wi.bottom)),
                            right: max(0, f.maxX - (wb.maxX - wi.right)))
    }

    /// The safe part of the view (the whole view if the insets leave
    /// nothing).
    private var safeRect: CGRect {
        let r = bounds.inset(by: safeInsets)
        return r.width >= 1 && r.height >= 1 ? r : bounds
    }

    /// TeamViewer's minimum zoom: the whole frame inside the safe area,
    /// min((w - l - r) / R.w, (h - t - b) / R.h).
    private var fitScale: CGFloat {
        guard frameSize.width > 0, frameSize.height > 0,
              bounds.width > 0, bounds.height > 0 else { return 0 }
        let s = safeRect
        return min(s.width / frameSize.width, s.height / frameSize.height)
    }

    /// Scale that fills the view (cropped on one axis): max of the ratios.
    private var fillScale: CGFloat {
        guard frameSize.width > 0, frameSize.height > 0,
              bounds.width > 0, bounds.height > 0 else { return 0 }
        return max(bounds.width / frameSize.width, bounds.height / frameSize.height)
    }

    /// One remote pixel = one device pixel.
    private var nativeScale: CGFloat {
        1 / displayScale
    }

    /// TeamViewer's maximum zoom: max(2.0, fit) view points per pixel.
    private var maxScale: CGFloat {
        max(Self.maxZoomScale, fitScale)
    }

    private func clampScale(_ s: CGFloat) -> CGFloat {
        guard s.isFinite else { return fitScale }
        return min(max(s, fitScale), maxScale)
    }

    /// Fit / fill / 1:1, each capped to the zoom range (1:1 above
    /// max(2, fit) becomes the max).
    private func scaleFor(_ preset: ZoomPreset) -> CGFloat {
        switch preset {
        case .fit: return fitScale
        case .fill: return clampScale(fillScale)
        case .native: return clampScale(nativeScale)
        }
    }

    /// The content fits the safe area vertically at the current zoom: it is
    /// pinned to the top of the safe area (TeamViewer layout: the screen at
    /// the top, the free space below it), whatever the cursor does.
    private var pinsToTop: Bool {
        frameSize.height * scale <= safeRect.height + 0.5 // tolerance: fit is computed from it
    }

    /// `p` (a frame-origin offset) with the content kept in view, per axis:
    /// where the content fits the safe area, horizontally centred and
    /// vertically at the top of the safe area (below the stats chip;
    /// TeamViewer layout); where it is larger than the view, never scrolled
    /// past its edges (the image always covers the view, no black band);
    /// in between, kept inside the view.
    private func clamped(_ p: CGPoint) -> CGPoint {
        let cw = frameSize.width * scale
        let ch = frameSize.height * scale
        let safe = safeRect
        var q = p
        if cw <= safe.width + 0.5 { // tolerance: fit is computed from it
            q.x = safe.minX + (safe.width - cw) / 2
        } else {
            let lo = min(0, bounds.width - cw)
            let hi = max(0, bounds.width - cw)
            q.x = min(max(p.x, lo), hi)
        }
        if ch <= safe.height + 0.5 {
            q.y = safe.minY
        } else {
            let lo = min(0, bounds.height - ch)
            let hi = max(0, bounds.height - ch)
            q.y = min(max(p.y, lo), hi)
        }
        return q
    }

    /// Zoomed in beyond fit: the viewport follows the cursor, centring it
    /// as far as the image edges allow (`viewportTarget`). Near an edge the
    /// view stops at the edge and the cursor moves off-centre towards it;
    /// centring resumes once the cursor is back far enough inside.
    private var centresCursor: Bool {
        guard scale > 0, input.hasCursor else { return false }
        let f = fitScale
        return f > 0 && scale > f * 1.001
    }

    /// The offset that puts the predicted cursor at the view's centre.
    private var cursorCentredOffset: CGPoint {
        let c = input.cursor
        return CGPoint(x: bounds.midX - c.x * scale, y: bounds.midY - c.y * scale)
    }

    /// Where the viewport belongs at the current zoom: zoomed in, the
    /// cursor-centred offset clamped to the image edges (a continuous
    /// function of the cursor, so following never jumps or oscillates);
    /// else the current offset clamped. On an axis where the image is no
    /// larger than the safe area the clamp keeps it centred / at the top
    /// (no jump away from the top on a small zoom).
    private func viewportTarget() -> CGPoint {
        clamped(centresCursor ? cursorCentredOffset : offset)
    }

    /// Place the viewport for the current zoom (`viewportTarget`).
    /// Viewport jumps (zoom, resync, leaving / reaching fit) are eased when
    /// `animated` or while an easing is already running, else immediate.
    private func settleViewport(animated: Bool) {
        guard scale > 0 else { return }
        let target = viewportTarget()
        if animated || followTarget != nil {
            followTarget = target
            if !animTicker.isRunning {
                lastAnimTime = 0
                animTicker.start()
            }
            return
        }
        followTarget = nil
        if target != offset {
            offset = target
            applyViewport()
        }
    }

    /// After the cursor moved locally (trackpad, pointer lock).
    private func followCursorAfterMove() {
        if centresCursor || followTarget != nil {
            settleViewport(animated: false)
        }
        updateCursorLayer()
    }

    /// The host moved the cursor (resync after local input, or someone at
    /// the host): zoomed in, the viewport eases over only when the cursor
    /// left the middle 80% of the view (no hover feedback loop on iPad) and
    /// the clamped target actually differs (a cursor parked near an image
    /// edge, where the view already stops, starts no easing).
    private func followHostCursor() {
        guard scale > 0, centresCursor else { return }
        let v = frameToView(input.cursor)
        let inner = bounds.insetBy(dx: bounds.width * 0.1, dy: bounds.height * 0.1)
        // A top-pinned axis never follows: only the horizontal position
        // counts then.
        let outX = v.x < inner.minX || v.x > inner.maxX
        let outY = !pinsToTop && (v.y < inner.minY || v.y > inner.maxY)
        guard outX || outY else { return }
        let target = viewportTarget()
        if followTarget != nil || abs(target.x - offset.x) > 0.5 || abs(target.y - offset.y) > 0.5 {
            settleViewport(animated: true)
        }
    }

    /// One easing step towards `followTarget` (re-clamped, so it stays
    /// inside the image edges); false when done.
    private func stepFollow(_ now: CFTimeInterval) -> Bool {
        guard let rawTarget = followTarget else { return false }
        guard scale > 0 else {
            followTarget = nil
            return false
        }
        let target = clamped(rawTarget)
        let dt = lastAnimTime > 0 ? min(max(now - lastAnimTime, 0), 0.1) : 1.0 / 60.0
        let k = CGFloat(1 - exp(-dt / Self.followTimeConstant))
        offset.x += (target.x - offset.x) * k
        offset.y += (target.y - offset.y) * k
        if abs(target.x - offset.x) < 0.25 && abs(target.y - offset.y) < 0.25 {
            offset = target
            followTarget = nil
        }
        applyViewport()
        return followTarget != nil
    }

    /// View or frame size changed (rotation, keyboard, monitor switch):
    /// re-apply the followed preset, else keep the zoom (clamped).
    private func refit() {
        let f = fitScale
        guard f > 0 else { return }
        if scale <= 0 {
            followPreset = .fit
        }
        if let preset = followPreset {
            scale = scaleFor(preset)
        } else {
            scale = clampScale(scale)
        }
        followTarget = nil
        settleViewport(animated: false)
        // Centred mode with an unchanged offset skips applyViewport: the
        // scale may still have changed.
        applyViewport()
        publishFitImageFrame()
    }

    /// Tell the chrome where the image sits at fit (window coordinates:
    /// centred horizontally, top of the safe area), so the toolbar can dock
    /// in the free space below it. Independent of the current zoom, so the
    /// toolbar does not move while zooming / panning. Published on the next
    /// main-queue turn: refit runs inside SwiftUI view updates
    /// (updateUIView -> setFrame), where publishing is not allowed.
    private func publishFitImageFrame() {
        let f = fitScale
        guard f > 0, window != nil else { return }
        let safe = safeRect
        let w = frameSize.width * f
        let h = frameSize.height * f
        let local = CGRect(x: safe.minX + (safe.width - w) / 2, y: safe.minY, width: w, height: h)
        let r = convert(local, to: nil).integral
        DispatchQueue.main.async { [weak self] in
            guard let self, self.input.activeSurface === self,
                  self.input.fitImageFrame != r else { return }
            self.input.fitImageFrame = r
        }
    }

    private func applyViewport() {
        guard scale > 0 else { return }
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        container.position = offset
        container.transform = CATransform3DMakeScale(scale, scale, 1)
        // Crisp pixels when strongly magnified, smooth otherwise.
        frameLayer.magnificationFilter = scale * displayScale >= 3 ? .nearest : .linear
        CATransaction.commit()
        updateCursorLayer()
        setNeedsRender()
    }

    /// Zoom by `factor` keeping the view point `pivot` fixed.
    private func zoom(by factor: CGFloat, pivot: CGPoint) {
        guard scale > 0, factor.isFinite, factor > 0 else { return }
        let newScale = clampScale(scale * factor)
        let fx = (pivot.x - offset.x) / scale
        let fy = (pivot.y - offset.y) / scale
        scale = newScale
        offset = CGPoint(x: pivot.x - fx * newScale, y: pivot.y - fy * newScale)
        followPreset = abs(newScale - fitScale) < 0.0001 ? .fit : nil
        // Left unclamped: the caller eases it (settleViewport) to the
        // cursor-centred or clamped offset.
        applyViewport()
    }

    /// Menu command: fit / fill / 1:1, centered on the cursor (or on what
    /// is in the middle of the view before the cursor is known).
    func applyPreset(_ preset: ZoomPreset) {
        guard scale > 0, fitScale > 0 else { return }
        let center = CGPoint(x: bounds.midX, y: bounds.midY)
        let fp = input.hasCursor ? input.cursor : viewToFrame(center)
        scale = scaleFor(preset)
        followPreset = preset
        offset = CGPoint(x: center.x - fp.x * scale, y: center.y - fp.y * scale)
        followTarget = nil
        settleViewport(animated: false)
        applyViewport()
    }

    /// Pinch released: snap to fit / fill / 1:1 when within +-20%.
    private func snapZoom(pivot: CGPoint) {
        guard scale > 0 else { return }
        let presets: [ZoomPreset] = [.fit, .fill, .native]
        var bestPreset: ZoomPreset?
        var bestScale: CGFloat = 0
        var bestDistance = CGFloat.greatestFiniteMagnitude
        for preset in presets {
            let target = scaleFor(preset)
            guard target > 0 else { continue }
            let distance = abs(scale / target - 1)
            if distance <= Self.snapTolerance && distance < bestDistance {
                bestDistance = distance
                bestPreset = preset
                bestScale = target
            }
        }
        guard let preset = bestPreset, bestScale > 0 else { return }
        zoom(by: bestScale / scale, pivot: pivot)
        followPreset = preset
    }

    /// View point -> frame pixel, clamped to the frame.
    func viewToFrame(_ p: CGPoint) -> CGPoint {
        guard scale > 0, frameSize.width > 0, frameSize.height > 0 else { return .zero }
        let x = (p.x - offset.x) / scale
        let y = (p.y - offset.y) / scale
        return CGPoint(x: min(max(x, 0), frameSize.width - 1),
                       y: min(max(y, 0), frameSize.height - 1))
    }

    /// Frame pixel -> view point.
    func frameToView(_ p: CGPoint) -> CGPoint {
        CGPoint(x: offset.x + p.x * scale, y: offset.y + p.y * scale)
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        if let metal, metal.layout(bounds: bounds, contentsScale: displayScale) {
            setNeedsRender()
        }
        if bounds.size != lastBoundsSize {
            lastBoundsSize = bounds.size
            // Also keeps the cursor in sight (centred when zoomed in)
            // when the view shrank (soft keyboard / key bar)
            // or rotated.
            refit()
        }
    }

    /// Rotation / status bar changes move the notch or home indicator:
    /// the fit scale (safe area) changes even when the bounds do not.
    override func safeAreaInsetsDidChange() {
        super.safeAreaInsetsDidChange()
        refit()
    }

    // MARK: Cursor

    private static let defaultCursor: CGImage? = {
        let size = RemoteSurfaceView.defaultCursorSize
        let format = UIGraphicsImageRendererFormat()
        format.scale = 3
        let renderer = UIGraphicsImageRenderer(size: size, format: format)
        let img = renderer.image { _ in
            let p = UIBezierPath()
            p.move(to: CGPoint(x: 0.75, y: 0.75))
            p.addLine(to: CGPoint(x: 0.75, y: 15.5))
            p.addLine(to: CGPoint(x: 4.5, y: 12))
            p.addLine(to: CGPoint(x: 7, y: 18))
            p.addLine(to: CGPoint(x: 9.25, y: 17))
            p.addLine(to: CGPoint(x: 6.75, y: 11.25))
            p.addLine(to: CGPoint(x: 11.25, y: 11.25))
            p.close()
            UIColor.white.setFill()
            p.fill()
            UIColor.black.setStroke()
            p.lineWidth = 1
            p.stroke()
        }
        return img.cgImage
    }()

    /// Draw the cursor at the predicted position: hotspot-anchored, scaled
    /// with the zoom but at least `cursorMinPoints` (and at most
    /// `cursorMaxPoints`) for its larger side.
    private func updateCursorLayer() {
        guard frameSize.width > 0, scale > 0, input.hasCursor else {
            if !cursorLayer.isHidden {
                CATransaction.begin()
                CATransaction.setDisableActions(true)
                cursorLayer.isHidden = true
                CATransaction.commit()
            }
            return
        }
        let img: CGImage?
        let px: CGSize
        let hs: CGSize
        if let c = cursorImage, cursorPixelSize.width > 0, cursorPixelSize.height > 0 {
            img = c
            px = cursorPixelSize
            hs = cursorHotspot
        } else {
            img = Self.defaultCursor
            px = Self.defaultCursorSize
            hs = .zero
        }
        let maxDim = max(px.width, px.height)
        var s = scale
        if maxDim * s < Self.cursorMinPoints { s = Self.cursorMinPoints / maxDim }
        if maxDim * s > Self.cursorMaxPoints { s = Self.cursorMaxPoints / maxDim }
        let pos = frameToView(input.cursor)

        CATransaction.begin()
        CATransaction.setDisableActions(true)
        if img !== shownCursorImage {
            shownCursorImage = img
            cursorLayer.contents = img
        }
        cursorLayer.isHidden = img == nil
        cursorLayer.bounds = CGRect(x: 0, y: 0, width: px.width * s, height: px.height * s)
        cursorLayer.anchorPoint = CGPoint(x: min(max(hs.width / px.width, 0), 1),
                                          y: min(max(hs.height / px.height, 0), 1))
        cursorLayer.position = pos
        dragRing.position = pos
        CATransaction.commit()
    }

    /// Trackpad motion of `d` screen points (finger travel in this view,
    /// which is never transformed: the zoom lives on the container layer;
    /// 1:1, no gain or acceleration) -> TeamViewer's
    /// pct += d / (R * z): `scale` is view POINTS per remote pixel (not
    /// device pixels), so frameSize * scale is the displayed image size in
    /// points. The controller sends the resulting absolute position.
    private func moveCursor(byViewPoints d: CGPoint) {
        guard scale > 0, frameSize.width > 0, frameSize.height > 0 else { return }
        let displayed = CGSize(width: frameSize.width * scale, height: frameSize.height * scale)
        input.moveCursorTrackpad(byPoints: d, displayed: displayed)
        followCursorAfterMove()
    }

    private func setDragRingVisible(_ visible: Bool) {
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        dragRing.isHidden = !visible
        dragRing.position = frameToView(input.cursor)
        CATransaction.commit()
    }

    // MARK: Animation (iPad trackpad scroll inertia, viewport easing)

    private func stopFlings() {
        scrollFling.stop()
    }

    private func animate(_ now: CFTimeInterval) {
        var running = false
        if let d = scrollFling.step(now: now) {
            running = true
            sendWheel(byViewPoints: d, factor: Self.wheelUnitsPerPoint)
        }
        if stepFollow(now) {
            running = true
        }
        lastAnimTime = now
        if !running {
            animTicker.stop()
            lastAnimTime = 0
        }
    }

    // MARK: Keyboard

    override var canBecomeFirstResponder: Bool { true }

    /// Become first responder (hardware keys) unless the key input view
    /// already is.
    private func claimKeyboardFocus() {
        guard window != nil, !isFirstResponder, !keyInput.isFirstResponder else { return }
        _ = becomeFirstResponder()
    }

    /// soft: true = system keyboard + bar, false = bar alone, nil = hide.
    /// Asking again for what is already shown hides it (toggle).
    private func presentKeyboard(_ soft: Bool?) {
        guard window != nil else { return }
        guard let soft else {
            if keyInput.isFirstResponder { _ = keyInput.resignFirstResponder() }
            claimKeyboardFocus()
            return
        }
        if keyInput.isFirstResponder {
            if keyInput.showsSoftKeyboard == soft {
                _ = keyInput.resignFirstResponder()
                claimKeyboardFocus()
            } else {
                keyInput.showsSoftKeyboard = soft
            }
        } else {
            keyInput.showsSoftKeyboard = soft
            _ = keyInput.becomeFirstResponder()
        }
    }

    override func pressesBegan(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        let rest = keyRouter.pressesBegan(presses)
        if !rest.isEmpty { super.pressesBegan(rest, with: event) }
    }

    override func pressesEnded(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        let rest = keyRouter.pressesEnded(presses)
        if !rest.isEmpty { super.pressesEnded(rest, with: event) }
    }

    override func pressesCancelled(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        let rest = keyRouter.pressesEnded(presses)
        if !rest.isEmpty { super.pressesCancelled(rest, with: event) }
    }

    override var keyCommands: [UIKeyCommand]? {
        keyRouter.keyCommands()
    }

    @objc func ovKeyCommand(_ sender: UIKeyCommand) {
        keyRouter.handleKeyCommand(sender)
    }

    // MARK: Raw touches (finger-down state, iPad pointer buttons)

    /// Fingers (not pointer touches) currently on this view.
    private func updateTouching(_ event: UIEvent?) {
        let touches = event?.touches(for: self) ?? []
        let active = touches.filter {
            $0.type != .indirectPointer && $0.phase != .ended && $0.phase != .cancelled
        }
        input.touching = !active.isEmpty
        // Multi-finger lift debounce bookkeeping.
        if active.count >= 2 {
            if !multiTouchSequence {
                // Any multi-finger touch cancels pending taps and the
                // long press (a two-finger tap is a right click instead).
                cancelPendingTaps()
                cancelTrackpadTimer()
            }
            multiTouchSequence = true
        } else if active.isEmpty && multiTouchSequence {
            multiTouchSequence = false
            multiTouchEnd = CACurrentMediaTime()
        }
    }

    /// A finger left over from (or just after) a multi-finger gesture.
    private var inMultiFingerAftermath: Bool {
        multiTouchSequence || CACurrentMediaTime() - multiTouchEnd < Self.multiFingerLiftDebounce
    }

    /// Never take touches over the session chrome (toolbar / handle, stats
    /// chip): those points fall through to the SwiftUI hosting view, so the
    /// chrome's buttons get them even if SwiftUI's own hit testing would
    /// have picked this platform view underneath.
    override func point(inside point: CGPoint, with event: UIEvent?) -> Bool {
        guard super.point(inside: point, with: event) else { return false }
        let rects = input.chromeExclusionRects
        guard !rects.isEmpty, window != nil else { return true }
        let p = convert(point, to: nil)
        return !rects.contains { $0.contains(p) }
    }

    // The touch overrides below deliberately do NOT call super: UIView's
    // implementation forwards every touch up the responder chain into
    // SwiftUI's hosting view, which then tracks remote-screen touches in
    // the same event bridge that drives the toolbar's Buttons. The surface
    // consumes its touches completely (all four phases are overridden).

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        var finger = false
        for t in touches {
            if t.type == .indirectPointer {
                pointerTouchBegan(t, event: event)
            } else {
                finger = true
            }
        }
        if finger {
            stopFlings()
            haptic.prepare()
            claimKeyboardFocus()
        }
        updateTouching(event)
    }

    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
        for t in touches where t.type == .indirectPointer {
            guard pointerButtons[ObjectIdentifier(t)] != nil else { continue }
            input.moveCursorAbsolute(to: viewToFrame(t.location(in: self)))
            updateCursorLayer()
        }
    }

    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) {
        for t in touches where t.type == .indirectPointer {
            pointerTouchEnded(t)
        }
        updateTouching(event)
    }

    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) {
        for t in touches where t.type == .indirectPointer {
            pointerTouchEnded(t)
        }
        updateTouching(event)
    }

    /// A mouse / trackpad click: the button comes from the event's button
    /// mask; pressed at the pointer position.
    private func pointerTouchBegan(_ t: UITouch, event: UIEvent?) {
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

    private func pointerTouchEnded(_ t: UITouch) {
        guard let button = pointerButtons.removeValue(forKey: ObjectIdentifier(t)) else { return }
        input.buttonUp(button, at: viewToFrame(t.location(in: self)))
        updateCursorLayer()
    }

    // MARK: Gesture handlers

    private var isPinching: Bool {
        pinch.state == .began || pinch.state == .changed
    }

    @objc private func handleTap2(_ g: UITapGestureRecognizer) {
        guard g.state == .ended else { return }
        clickWithFingers(.right)
    }

    @objc private func handleTap3(_ g: UITapGestureRecognizer) {
        guard g.state == .ended else { return }
        clickWithFingers(.middle)
    }

    /// Multi-finger taps: click at the cursor.
    private func clickWithFingers(_ button: MouseButton) {
        input.click(button)
        updateCursorLayer()
    }

    // MARK: Trackpad (one finger)

    // TeamViewer's MouseGestureRecognizer model: the cursor follows the
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
    private static let tapSlop: CGFloat = 10
    private static let longPressDelay: TimeInterval = 0.30
    /// TeamViewer scrolling: one wheel notch (120 units) per 10 pt.
    private static let scrollStepPoints: CGFloat = 10

    private func handleTrackpad(_ phase: TrackpadTouchRecognizer.Phase, _ loc: CGPoint, _ time: TimeInterval) {
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

    private func trackpadBegan(_ loc: CGPoint, _ time: TimeInterval) {
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

    private func trackpadMoved(_ loc: CGPoint, _ time: TimeInterval) {
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

    private func trackpadEnded(_ loc: CGPoint, _ time: TimeInterval) {
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
        // TeamViewer: count the tap and answer when no further tap
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
    private func fireCountedTaps() {
        tapTimer = nil
        guard let taps = lastTap else { return }
        lastTap = nil
        guard !dragActive, !multiTouchSequence else { return }
        for _ in 0..<max(1, min(taps.count, 3)) {
            input.click(.left, at: taps.cursor)
        }
        updateCursorLayer()
    }

    /// Drop taps not yet answered (and the TeamViewer tap timer): a second
    /// finger, a move, a long press or teardown.
    private func cancelPendingTaps() {
        tapTimer?.cancel()
        tapTimer = nil
        lastTap = nil
    }

    private func trackpadCancelled() {
        cancelTrackpadTimer()
        cancelPendingTaps()
        tp = TrackpadTouch()
        endTrackpadDrag()
    }

    /// Hold timer (0.3 s, not moved): right click at the cursor.
    private func trackpadLongPress() {
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
    private func beginTrackpadDrag() {
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

    private func endTrackpadDrag() {
        guard dragActive else { return }
        dragActive = false
        input.buttonUp(.left)
        setDragRingVisible(false)
        updateCursorLayer()
    }

    /// Where a tap / long press of `t` clicks: its touch-down pixel when
    /// the cursor has moved since (sub-slop jitter), else nil (the cursor).
    private func trackpadClickPoint(_ t: TrackpadTouch) -> CGPoint? {
        input.cursor == t.downCursor ? nil : t.downCursor
    }

    private func scheduleTrackpadTimer(after delay: TimeInterval, _ action: @escaping () -> Void) {
        cancelTrackpadTimer()
        let work = DispatchWorkItem(block: action)
        tpTimer = work
        DispatchQueue.main.asyncAfter(deadline: .now() + delay, execute: work)
    }

    private func cancelTrackpadTimer() {
        tpTimer?.cancel()
        tpTimer = nil
    }

    /// TeamViewer scrolling: finger travel accumulates and every whole
    /// 10 pt is one wheel notch (120 units), natural direction (content
    /// follows the finger), horizontal too.
    private func stepWheel(byViewPoints t: CGPoint) {
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
    private func startScrollFling(velocity v: CGPoint) {
        scrollFling.start(velocity: v, now: CACurrentMediaTime())
        if scrollFling.isActive { animTicker.start() }
    }

    /// Two-finger pan and iPad trackpad / wheel scrolling -> WHEEL.
    /// Two-finger pan: at the cursor, one notch per 10 pt, no inertia
    /// (TeamViewer); iPad pointer scrolling stays continuous (trackpads
    /// glide on after release).
    @objc private func handleScroll(_ g: UIPanGestureRecognizer) {
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
    private func sendWheel(byViewPoints t: CGPoint, factor k: CGFloat) {
        input.wheel(dx: Double(-t.x * k), dy: Double(t.y * k))
    }

    @objc private func handlePinch(_ g: UIPinchGestureRecognizer) {
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
            // centre (TeamViewer).
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
    private func handleRemotePinch(_ g: UIPinchGestureRecognizer) {
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

    @objc private func handleHover(_ g: UIHoverGestureRecognizer) {
        guard g.state == .began || g.state == .changed, !pointerLocked else { return }
        input.moveCursorAbsolute(to: viewToFrame(g.location(in: self)))
        updateCursorLayer()
    }

    // MARK: UIGestureRecognizerDelegate

    /// Pinch and the two-finger pan (wheel) exclude each other; the
    /// trackpad recognizer is simultaneous with everything
    /// (it never recognizes; others must not force it out). Pointer recognizers
    /// (hover, scroll) never block anything. Recognizers of ancestor views
    /// (SwiftUI's hosting-view gesture recognizer, the touch-routing
    /// logger) are never blocked either: by default a surface recognizer
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
