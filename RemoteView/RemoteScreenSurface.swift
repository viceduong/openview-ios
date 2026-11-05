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
// Gestures (CRD structure, timings tuned to TeamViewer iOS 15.81):
//
//   MOUSE (trackpad) mode
//     1-finger pan        TeamViewer 15.81 model: normalised position
//                         pct += finger travel (screen points) / displayed
//                         image size (points), clamped, sent as ABSOLUTE
//                         moves (RemoteInputController.moveCursorTrackpad).
//                         At fit, sliding across the image crosses the
//                         whole remote screen; zoomed in by z, 1 pt = 1/z
//                         remote px. "Cursor speed" (setting
//                         "ov.cursorSpeed", 0.5x-3x, default 1.0x = exact
//                         TeamViewer) multiplies the finger travel; optional
//                         acceleration curve 1x -> 2.5x on top (setting
//                         "ov.cursorAccel", off). Zoomed in, the viewport
//                         keeps the cursor centred (TeamViewer; setting
//                         "ov.viewportFollow" = "edge" restores edge-push)
//     v0.42 (TeamViewer parity): one custom recognizer
//                         (TrackpadTouchRecognizer, the MouseGestureRecognizer
//                         model) moves the cursor from the very first
//                         touchesMoved (no pan slop, no waiting for a tap to
//                         fail); tap / drag / long press are decided
//                         afterwards, see "Mouse-mode trackpad". Cursor
//                         inertia (ov.cursorFling) default off.
//     tap                 a touch that never travelled 10 pt: counted; 0.3 s
//                         after the last tap lifts, 1 tap = left click, 2 =
//                         double click, at the cursor (the tapped pixel:
//                         sub-slop jitter is undone). "Immediate clicks"
//                         (ov.immediateClicks, off): click on lift instead
//     double-tap and drag second touch-down < 0.3 s after a tap, then move
//                         10 pt (or hold 0.3 s) = left drag, DOWN at the
//                         tapped pixel (haptic), UP on lift, + ring
//     long-press 0.30 s   right click at the cursor (10 pt cancel radius)
//     2-finger tap        right click (instant: no double tap to wait for);
//                         3-finger tap: middle click. Any second finger
//                         cancels pending taps / the long press
//     2-finger pan        wheel at the cursor: one notch (120) per 10 pt,
//                         natural direction, both axes; inertia only with
//                         "Scroll inertia" (ov.scrollInertia, off)
//     pinch               zoom around the cursor; snaps to fit / fill / 1:1
//                         within +-20% on release. Zoom range (TeamViewer):
//                         fit inside the safe area ... max(2 pt/px, fit)
//   TOUCH (direct) mode
//     tap                 cursor to the finger + left click there (+ ripple)
//     long tap 0.4 s      loupe; lift = right click at the finger, drag
//                         without lifting = left drag-select
//     1-finger pan        wheel at the finger, one notch per 10 pt
//     2-finger pan        pan the zoomed view (with the pinch)
//     pinch               zoom at the fingers (and pan with them)
//     "Classic touch gestures" (ov.classicTouchGestures, off): 1 finger
//     pans the view, 2 fingers = wheel at the fingers, long tap 0.3 s
//   LOCK LOCAL ZOOM (setting "ov.lockLocalZoom"): pinch becomes Ctrl+wheel
//   on the host and the touch-mode 1-finger pan becomes a left drag.
//   After a multi-finger gesture the remaining finger is ignored until
//   every finger lifted + 0.1 s (no stray cursor moves / clicks).
//   Fit / fill / 1:1 are menu commands (RemoteInputController.applyZoomPreset).
//   iPad pointer: hover = absolute move, clicks / right clicks via indirect
//   pointer touches, trackpad / wheel scroll = WHEEL, optional raw GCMouse
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

/// Mouse mode's one-finger input (TeamViewer's MouseGestureRecognizer):
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

    /// Forced out by the system (another recognizer, mode switch, ...):
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

/// Recent finger samples -> release / acceleration velocity (points per
/// second) over the last 100 ms.
struct TouchVelocity {
    private var samples: [(t: TimeInterval, p: CGPoint)] = []
    private static let window: TimeInterval = 0.1

    mutating func reset(_ p: CGPoint, at t: TimeInterval) {
        samples.removeAll(keepingCapacity: true)
        samples.append((t: t, p: p))
    }

    mutating func add(_ p: CGPoint, at t: TimeInterval) {
        samples.append((t: t, p: p))
        while samples.count > 2, let first = samples.first, t - first.t > Self.window {
            samples.removeFirst()
        }
    }

    /// The velocity at `now` (zero when the finger rested > 50 ms).
    func velocity(now: TimeInterval) -> CGPoint {
        guard let last = samples.last, let first = samples.first, now - last.t <= 0.05 else { return .zero }
        let dt = last.t - first.t
        guard dt >= 0.004 else { return .zero }
        return CGPoint(x: (last.p.x - first.p.x) / CGFloat(dt), y: (last.p.y - first.p.y) / CGFloat(dt))
    }
}

/// One mouse-mode trackpad touch (owned by RemoteSurfaceView).
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
    /// 0.15 s or move past the slop = left drag.
    var pendingDrag = false
    /// pendingDrag: the taps counted before this touch.
    var priorTaps = 0
    /// pendingDrag: cursor travel held back until the drag has started.
    var heldTravel: CGPoint = .zero
    var velocity = TouchVelocity()
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
    private let loupe = CALayer()
    private let loupeDot = CALayer()

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

    // ---- gestures ----
    private let tap1 = UITapGestureRecognizer(target: nil, action: nil)
    private let tap2 = UITapGestureRecognizer(target: nil, action: nil)
    private let tap3 = UITapGestureRecognizer(target: nil, action: nil)
    private let pan1 = UIPanGestureRecognizer(target: nil, action: nil)
    private let pan2 = UIPanGestureRecognizer(target: nil, action: nil)
    private let pinch = UIPinchGestureRecognizer(target: nil, action: nil)
    private let longPress = UILongPressGestureRecognizer(target: nil, action: nil)
    /// Mouse mode's one-finger trackpad (moves, taps, drags, long press).
    private let trackpad = TrackpadTouchRecognizer(target: nil, action: nil)
    private let hover = UIHoverGestureRecognizer(target: nil, action: nil)
    private let scrollContinuous = UIPanGestureRecognizer(target: nil, action: nil)
    private let scrollDiscrete = UIPanGestureRecognizer(target: nil, action: nil)

    private let haptic = UIImpactFeedbackGenerator(style: .medium)
    private let pointerHider = PointerHider()
    private let gcMouse = GCMouseBridge()
    private var cancellables = Set<AnyCancellable>()

    /// Mouse mode: a tap-then-hold drag holds the left button.
    private var dragActive = false
    /// Mouse-mode trackpad touch (TrackpadTouchRecognizer) state.
    private var tp = TrackpadTouch()
    /// Mouse-mode taps not yet answered: how many, the cursor pixel they
    /// click, and when the last one lifted. A touch starting within
    /// `tapWindow` of that is a further tap or a double-tap-and-drag.
    private var lastTap: (count: Int, cursor: CGPoint, time: CFTimeInterval)?
    /// TeamViewer tap timer: fires `tapWindow` after the last tap lifted
    /// and sends the counted clicks (not used with "Immediate clicks").
    private var tapTimer: DispatchWorkItem?
    /// Long-press (right click) / tap-then-hold (drag) timer of the touch.
    private var tpTimer: DispatchWorkItem?
    /// Two-finger / one-finger wheel scrolling: finger travel (points)
    /// not yet emitted as whole 10 pt wheel steps.
    private var scrollStepAcc: CGPoint = .zero
    /// Touch mode long-press state.
    private var lpActive = false
    private var lpDragging = false
    private var lpStartView: CGPoint = .zero
    private var lpStartFrame: CGPoint = .zero
    /// Touch mode: the previous tap, so a double tap clicks twice at the
    /// same remote pixel (Windows' double-click tolerance is tiny).
    private var lastTouchTap: (time: CFTimeInterval, view: CGPoint, frame: CGPoint)?
    /// Touch mode pinch: previous centroid (pan with the fingers).
    private var lastPinchCentroid: CGPoint?
    /// Indirect pointer touch -> the button it pressed.
    private var pointerButtons: [ObjectIdentifier: MouseButton] = [:]
    /// Multi-finger lift debounce: a multi-finger touch sequence is in
    /// progress (until every finger lifted), and when the last one ended.
    private var multiTouchSequence = false
    private var multiTouchEnd: CFTimeInterval = 0
    /// The current one-finger pan started inside a multi-finger sequence.
    private var pan1Suppressed = false
    /// Lock local zoom: touch-mode one-finger drag holding the left button.
    private var touchDragActive = false
    /// Lock local zoom: Ctrl held on the host for a pinch (Ctrl+wheel).
    private var pinchCtrlHeld = false

    // ---- animation ----
    private var cursorFling = FlingTracker(timeConstant: 180)
    private var scrollFling = FlingTracker(timeConstant: 250)
    private var scrollFlingFactor: CGFloat = 0
    /// The scroll fling emits 10 pt wheel steps (finger scrolling).
    private var scrollFlingStepped = false
    /// Mouse mode, zoomed in: the cursor-centred offset the viewport is
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
    private static let edgeMargin: CGFloat = 0.13
    /// Wheel units (1/120 notch) per point of two-finger travel: one
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
    private static let loupeDiameter: CGFloat = 100
    private static let loupeMagnification: CGFloat = 2
    private static let loupeOffset: CGFloat = 80
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
        input.$mode
            .sink { [weak self] mode in self?.applyMode(mode) }
            .store(in: &cancellables)
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

        let d = Self.loupeDiameter
        loupe.actions = Self.noActions
        loupe.bounds = CGRect(x: 0, y: 0, width: d, height: d)
        loupe.cornerRadius = d / 2
        loupe.masksToBounds = true
        loupe.borderWidth = 2
        loupe.borderColor = UIColor.white.cgColor
        loupe.backgroundColor = UIColor.black.cgColor
        loupe.contentsGravity = .resize
        loupe.magnificationFilter = .linear
        loupe.isHidden = true
        loupeDot.actions = Self.noActions
        loupeDot.bounds = CGRect(x: 0, y: 0, width: 6, height: 6)
        loupeDot.cornerRadius = 3
        loupeDot.backgroundColor = UIColor.systemRed.cgColor
        loupeDot.position = CGPoint(x: d / 2, y: d / 2)
        loupe.addSublayer(loupeDot)
        layer.addSublayer(loupe)
    }

    private func setupGestures() {
        // Touch recognizers ignore indirect pointer touches (handled in
        // touchesBegan with the button mask) and never cancel touches, so
        // touchesEnded keeps the "finger down" state exact.
        let touchTypes: [NSNumber] = [
            NSNumber(value: UITouch.TouchType.direct.rawValue),
            NSNumber(value: UITouch.TouchType.pencil.rawValue),
        ]

        tap1.addTarget(self, action: #selector(handleTap1(_:)))
        tap2.addTarget(self, action: #selector(handleTap2(_:)))
        tap2.numberOfTouchesRequired = 2
        tap3.addTarget(self, action: #selector(handleTap3(_:)))
        tap3.numberOfTouchesRequired = 3
        pan1.addTarget(self, action: #selector(handlePan1(_:)))
        pan1.minimumNumberOfTouches = 1
        pan1.maximumNumberOfTouches = 1
        pan2.addTarget(self, action: #selector(handleScroll(_:)))
        pan2.minimumNumberOfTouches = 2
        pan2.maximumNumberOfTouches = 2
        pinch.addTarget(self, action: #selector(handlePinch(_:)))
        longPress.addTarget(self, action: #selector(handleLongPress(_:)))
        longPress.minimumPressDuration = 0.40 // touch mode long tap; see shouldReceive
        longPress.allowableMovement = 10
        // Mouse mode, one finger: moves from the first touchesMoved; taps,
        // tap-then-hold drags and long presses are decided by the
        // trackpad handlers (TeamViewer's MouseGestureRecognizer model).
        trackpad.onTouch = { [weak self] phase, location, time in
            self?.handleTrackpad(phase, location, time)
        }
        trackpad.delaysTouchesEnded = false

        let touchRecognizers: [UIGestureRecognizer] = [
            tap1, tap2, tap3, pan1, pan2, pinch, longPress, trackpad,
        ]
        for g in touchRecognizers {
            g.delegate = self
            g.cancelsTouchesInView = false
            g.allowedTouchTypes = touchTypes
            addGestureRecognizer(g)
        }

        // Recognizer wiring (CRD): single tap waits for the two-finger tap
        // to fail, two-finger tap for the three-finger tap; pinch and the
        // one-finger pan wait for the single tap. There is no two-finger
        // double tap any more, so a two-finger tap right-clicks without
        // waiting for a second tap. Mouse mode's one-finger input (the
        // trackpad recognizer) depends on nothing: before v0.42 the cursor
        // pan had to wait for the single tap to fail (~10 pt of travel,
        // plus the pan's own slop) before the cursor moved at all; pan1 and
        // the long press are touch-mode only now (applyMode), and tap1 is
        // ignored in mouse mode.
        tap1.require(toFail: tap2)
        tap2.require(toFail: tap3)
        pinch.require(toFail: tap1)
        pan1.require(toFail: tap1)

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
        cancellables.removeAll()
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

    private func applyMode(_ mode: InputMode) {
        // Disabling an active recognizer cancels it (its handler releases
        // whatever it pressed). One finger: the trackpad recognizer in
        // mouse mode, pan / long press (loupe) in touch mode.
        trackpad.isEnabled = mode == .mouse
        pan1.isEnabled = mode == .touch
        longPress.isEnabled = mode == .touch
        endLocalGestures()
        cursorFling.stop()
        scrollFling.stop()
        // Touch mode keeps the image inside the view; mouse mode may centre
        // the cursor. @Published delivers in willSet (input.mode is still
        // the old mode here), so settle on the next main-queue turn.
        DispatchQueue.main.async { [weak self] in
            self?.settleViewport(animated: false)
        }
    }

    /// Drop transient gesture UI / held buttons (mode switch, teardown).
    private func endLocalGestures() {
        cancelTrackpadTimer()
        cancelPendingTaps()
        tp = TrackpadTouch()
        if dragActive {
            dragActive = false
            input.buttonUp(.left)
            setDragRingVisible(false)
        }
        if lpActive {
            lpActive = false
            if lpDragging { input.buttonUp(.left) }
            lpDragging = false
            hideLoupe()
        }
        if touchDragActive {
            touchDragActive = false
            input.buttonUp(.left)
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
            if !loupe.isHidden { loupe.contents = img }
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

    /// The part of the view outside the window's safe area insets (notch,
    /// home indicator, status bar), in view coordinates. Computed from the
    /// window (the SwiftUI host lays this view out ignoring the safe area,
    /// and what it reports as the view's own safeAreaInsets is not
    /// guaranteed); zero outside a window.
    private var safeInsets: UIEdgeInsets {
        guard let window, bounds.width > 0, bounds.height > 0 else { return .zero }
        let wi = window.safeAreaInsets
        let wb = window.bounds
        let f = convert(bounds, to: window)
        return UIEdgeInsets(top: max(0, wi.top - f.minY),
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

    /// Zoom relative to fit (1 = fit).
    private var zoomFactor: CGFloat {
        let f = fitScale
        return f > 0 && scale > 0 ? scale / f : 1
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

    /// Keep the content in view: centred in the safe area on axes where it
    /// fits there, else inside the view (no empty band when it is larger
    /// than the view).
    private func clampOffset() {
        let cw = frameSize.width * scale
        let ch = frameSize.height * scale
        let safe = safeRect
        if cw <= safe.width + 0.5 { // tolerance: fit is computed from it
            offset.x = safe.minX + (safe.width - cw) / 2
        } else {
            let lo = min(0, bounds.width - cw)
            let hi = max(0, bounds.width - cw)
            offset.x = min(max(offset.x, lo), hi)
        }
        if ch <= safe.height + 0.5 {
            offset.y = safe.minY + (safe.height - ch) / 2
        } else {
            let lo = min(0, bounds.height - ch)
            let hi = max(0, bounds.height - ch)
            offset.y = min(max(offset.y, lo), hi)
        }
    }

    /// Mouse mode, zoomed in beyond fit, "centre" follow (TeamViewer: its
    /// scroll view has insets of half the visible size, so the cursor can
    /// always sit at the centre, the image edges included).
    private var centresCursor: Bool {
        guard input.mode == .mouse, scale > 0, input.hasCursor,
              input.viewportFollow == .center else { return false }
        let f = fitScale
        return f > 0 && scale > f * 1.001
    }

    /// The offset that puts the predicted cursor at the view's centre.
    private var cursorCentredOffset: CGPoint {
        let c = input.cursor
        return CGPoint(x: bounds.midX - c.x * scale, y: bounds.midY - c.y * scale)
    }

    /// Mouse mode with "centre" follow: viewport jumps (zoom, resync,
    /// leaving / reaching fit) are eased instead of applied at once.
    private var easesViewport: Bool {
        input.mode == .mouse && input.viewportFollow == .center
    }

    /// Place the viewport for the current zoom / mode: cursor-centred
    /// (mouse mode zoomed in, TeamViewer) or clamped (plus the edge-push
    /// in mouse mode with "edge" follow). In "centre" mode the change is
    /// eased when `animated` or while an easing is already running, else
    /// immediate; other modes are always immediate.
    private func settleViewport(animated: Bool) {
        guard scale > 0 else { return }
        let target: CGPoint
        if centresCursor {
            target = cursorCentredOffset
        } else {
            let saved = offset
            clampOffset()
            target = offset
            offset = saved
        }
        if easesViewport && (animated || followTarget != nil) {
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
        if input.mode == .mouse && !centresCursor {
            edgePush()
        }
    }

    /// After the cursor moved locally (trackpad, fling, pointer lock).
    private func followCursorAfterMove() {
        if centresCursor || followTarget != nil {
            settleViewport(animated: false)
        } else {
            edgePush()
        }
        updateCursorLayer()
    }

    /// The host moved the cursor (resync after local input, or someone at
    /// the host): centred mode eases the viewport over only when the
    /// cursor left the middle 80% of the view (no hover feedback loop on
    /// iPad); edge mode pushes.
    private func followHostCursor() {
        guard input.mode == .mouse, scale > 0 else { return }
        if centresCursor {
            let v = frameToView(input.cursor)
            let inner = bounds.insetBy(dx: bounds.width * 0.1, dy: bounds.height * 0.1)
            if !inner.contains(v) {
                settleViewport(animated: true)
            }
        } else {
            edgePush()
        }
    }

    /// One easing step towards `followTarget`; false when done.
    private func stepFollow(_ now: CFTimeInterval) -> Bool {
        guard let target = followTarget else { return false }
        guard easesViewport, scale > 0 else {
            // Mode / setting changed meanwhile: place it at once.
            followTarget = nil
            settleViewport(animated: false)
            return false
        }
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
        // Mouse mode, "centre" follow: left unclamped, the caller eases it
        // (settleViewport) to the cursor-centred or clamped offset.
        if !easesViewport {
            clampOffset()
        }
        applyViewport()
    }

    private func panViewport(by t: CGPoint) {
        guard scale > 0 else { return }
        offset.x += t.x
        offset.y += t.y
        clampOffset()
        applyViewport()
    }

    /// Menu command: fit / fill / 1:1, centered on the cursor (mouse mode)
    /// or on what is in the middle of the view (touch mode).
    func applyPreset(_ preset: ZoomPreset) {
        guard scale > 0, fitScale > 0 else { return }
        let center = CGPoint(x: bounds.midX, y: bounds.midY)
        let fp = input.mode == .mouse && input.hasCursor ? input.cursor : viewToFrame(center)
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

    /// Mouse mode ("edge" follow): pan the zoomed view so the cursor stays
    /// out of the outer 13% band.
    private func edgePush() {
        guard scale > 0, bounds.width > 0, bounds.height > 0 else { return }
        let cw = frameSize.width * scale
        let ch = frameSize.height * scale
        guard cw > bounds.width + 0.5 || ch > bounds.height + 0.5 else { return }
        let v = frameToView(input.cursor)
        let mx = bounds.width * Self.edgeMargin
        let my = bounds.height * Self.edgeMargin
        var dx: CGFloat = 0
        var dy: CGFloat = 0
        if v.x < mx { dx = mx - v.x } else if v.x > bounds.width - mx { dx = bounds.width - mx - v.x }
        if v.y < my { dy = my - v.y } else if v.y > bounds.height - my { dy = bounds.height - my - v.y }
        guard dx != 0 || dy != 0 else { return }
        offset.x += dx
        offset.y += dy
        clampOffset()
        applyViewport()
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
            // Also keeps the cursor in sight (centred / edge-pushed in
            // mouse mode) when the view shrank (soft keyboard / key bar)
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
    /// already multiplied by `trackpadGain`, 1.0 by default) -> TeamViewer's
    /// pct += d / (R * z): `scale` is view POINTS per remote pixel (not
    /// device pixels), so frameSize * scale is the displayed image size in
    /// points. The controller sends the resulting absolute position.
    private func moveCursor(byViewPoints d: CGPoint) {
        guard scale > 0, frameSize.width > 0, frameSize.height > 0 else { return }
        let displayed = CGSize(width: frameSize.width * scale, height: frameSize.height * scale)
        input.moveCursorTrackpad(byPoints: d, displayed: displayed)
        followCursorAfterMove()
    }

    /// Pointer acceleration: 1x below 0.5 pt/ms, smoothstep up to 2.5x at
    /// 1.5 pt/ms and above.
    private static func pointerGain(speedPtPerMs speed: CGFloat) -> CGFloat {
        guard speed.isFinite else { return 1 }
        let t = min(max(speed - 0.5, 0), 1)
        return 1 + 1.5 * t * t * (3 - 2 * t)
    }

    private func setDragRingVisible(_ visible: Bool) {
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        dragRing.isHidden = !visible
        dragRing.position = frameToView(input.cursor)
        CATransaction.commit()
    }

    // MARK: Feedback layers

    /// Touch-mode tap feedback: an expanding, fading circle.
    private func ripple(at p: CGPoint) {
        let r: CGFloat = 22
        let ring = CAShapeLayer()
        ring.path = UIBezierPath(ovalIn: CGRect(x: -r, y: -r, width: 2 * r, height: 2 * r)).cgPath
        ring.fillColor = UIColor.white.withAlphaComponent(0.3).cgColor
        ring.strokeColor = UIColor.white.withAlphaComponent(0.9).cgColor
        ring.lineWidth = 2
        ring.opacity = 0
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        ring.position = p
        layer.insertSublayer(ring, below: cursorLayer)
        CATransaction.commit()

        let grow = CABasicAnimation(keyPath: "transform.scale")
        grow.fromValue = 0.4
        grow.toValue = 1.3
        let fade = CABasicAnimation(keyPath: "opacity")
        fade.fromValue = 1.0
        fade.toValue = 0.0
        let group = CAAnimationGroup()
        group.animations = [grow, fade]
        group.duration = 0.35
        group.timingFunction = CAMediaTimingFunction(name: .easeOut)
        CATransaction.begin()
        CATransaction.setCompletionBlock {
            ring.removeFromSuperlayer()
        }
        ring.add(group, forKey: "ripple")
        CATransaction.commit()
    }

    /// Touch-mode loupe: ~2x of the frame around the finger, 80 pt above
    /// it (below when too close to the top). Shows the tile frame (not the
    /// H.264 overlay).
    private func showLoupe(at p: CGPoint) {
        guard let img = image, frameSize.width > 0, frameSize.height > 0, scale > 0 else { return }
        let fx = (p.x - offset.x) / scale
        let fy = (p.y - offset.y) / scale
        let region = Self.loupeDiameter / (scale * Self.loupeMagnification)
        let rect = CGRect(x: (fx - region / 2) / frameSize.width,
                          y: (fy - region / 2) / frameSize.height,
                          width: region / frameSize.width,
                          height: region / frameSize.height)
        let half = Self.loupeDiameter / 2
        var center = CGPoint(x: p.x, y: p.y - Self.loupeOffset)
        if center.y - half < 0 { center.y = p.y + Self.loupeOffset }
        center.x = min(max(center.x, half), max(half, bounds.width - half))
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        if loupe.isHidden || loupe.contents == nil {
            loupe.contents = img
        }
        loupe.contentsRect = rect
        loupe.position = center
        loupe.isHidden = false
        CATransaction.commit()
    }

    private func hideLoupe() {
        guard !loupe.isHidden else { return }
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        loupe.isHidden = true
        loupe.contents = nil
        CATransaction.commit()
    }

    // MARK: Animation (flings)

    private func stopFlings() {
        cursorFling.stop()
        scrollFling.stop()
    }

    private func animate(_ now: CFTimeInterval) {
        var running = false
        if let d = cursorFling.step(now: now) {
            running = true
            moveCursor(byViewPoints: d)
        }
        if let d = scrollFling.step(now: now) {
            running = true
            if scrollFlingStepped {
                stepWheel(byViewPoints: d)
            } else {
                sendWheel(byViewPoints: d, factor: scrollFlingFactor)
            }
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

    @objc private func handleTap1(_ g: UITapGestureRecognizer) {
        guard g.state == .ended else { return }
        let now = CACurrentMediaTime()
        if now - multiTouchEnd < Self.multiFingerLiftDebounce { return }
        switch input.mode {
        case .mouse:
            // Mouse-mode taps are the trackpad recognizer's (on lift).
            return
        case .touch:
            let loc = g.location(in: self)
            var p = viewToFrame(loc)
            if let last = lastTouchTap, now - last.time < Self.tapWindow,
               hypot(loc.x - last.view.x, loc.y - last.view.y) < 24 {
                p = last.frame
            }
            lastTouchTap = (time: now, view: loc, frame: p)
            input.click(.left, at: p)
            ripple(at: frameToView(p))
        }
        updateCursorLayer()
    }

    @objc private func handleTap2(_ g: UITapGestureRecognizer) {
        guard g.state == .ended else { return }
        clickWithFingers(.right, g)
    }

    @objc private func handleTap3(_ g: UITapGestureRecognizer) {
        guard g.state == .ended else { return }
        clickWithFingers(.middle, g)
    }

    /// Multi-finger taps: at the cursor (mouse mode) or at the fingers'
    /// centroid (touch mode).
    private func clickWithFingers(_ button: MouseButton, _ g: UIGestureRecognizer) {
        switch input.mode {
        case .mouse:
            input.click(button)
        case .touch:
            let p = viewToFrame(g.location(in: self))
            input.click(button, at: p)
            ripple(at: frameToView(p))
        }
        updateCursorLayer()
    }

    /// Touch mode, one finger (mouse mode: disabled — the trackpad
    /// recognizer owns one-finger input there). Default: scroll at the
    /// finger, one wheel notch per 10 pt ("One finger to scroll"); classic
    /// touch gestures: pan the zoomed view; lock local zoom: left drag.
    @objc private func handlePan1(_ g: UIPanGestureRecognizer) {
        guard input.mode == .touch else { return }
        if input.lockLocalZoom || touchDragActive {
            handleTouchDrag(g)
            return
        }
        let classic = input.classicTouchGestures
        switch g.state {
        case .began:
            cursorFling.stop()
            scrollFling.stop()
            scrollStepAcc = .zero
            pan1Suppressed = inMultiFingerAftermath
            if !classic && !pan1Suppressed && !lpActive {
                // Scroll where the finger went down.
                let loc = g.location(in: self)
                let t = g.translation(in: self)
                input.moveCursorAbsolute(to: viewToFrame(CGPoint(x: loc.x - t.x, y: loc.y - t.y)))
                updateCursorLayer()
            }
        case .changed:
            let t = g.translation(in: self)
            g.setTranslation(.zero, in: self)
            if isPinching || pan1Suppressed || multiTouchSequence || lpActive { return }
            if classic {
                panViewport(by: t)
            } else {
                stepWheel(byViewPoints: t)
            }
        case .ended:
            let suppressed = pan1Suppressed
            pan1Suppressed = false
            guard !classic, !suppressed, !lpActive, input.input2, input.scrollInertiaEnabled else { return }
            startScrollFling(velocity: g.velocity(in: self), stepped: true, factor: 0)
        default:
            pan1Suppressed = false
        }
    }

    /// Touch mode, two fingers (default): pan the view with the fingers
    /// ("Two fingers to pan"); while pinching the pinch pans itself.
    private func handleTwoFingerPan(_ g: UIPanGestureRecognizer) {
        switch g.state {
        case .began:
            scrollFling.stop()
        case .changed:
            let t = g.translation(in: self)
            g.setTranslation(.zero, in: self)
            if isPinching { return }
            panViewport(by: t)
        default:
            break
        }
    }

    // MARK: Mouse-mode trackpad

    // TeamViewer's MouseGestureRecognizer model: the cursor follows the
    // finger from its first movement (delta from the previous location;
    // the previous location is reset at touch-down, no slop, no
    // acceleration by default), and what the touch meant is decided
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
    // "Immediate clicks" (setting, pre-v0.42): a tap clicks on lift (the
    // second one at the first one's pixel = double click) and tap, then
    // hold 0.15 s also drags.
    // Sub-slop jitter moves the cursor (zero latency), but taps / long
    // presses click at the touch-down pixel, so double clicks stay exact.

    /// Finger travel (points) that makes a touch a move instead of a tap.
    private static let tapSlop: CGFloat = 10
    private static let longPressDelay: TimeInterval = 0.30
    /// "Immediate clicks": tap, then touch and hold this long = drag.
    private static let immediateHoldDragDelay: TimeInterval = 0.15
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
        cursorFling.stop()
        tp = TrackpadTouch()
        // A finger left over from (or right after) a multi-finger gesture
        // is ignored to its end.
        guard input.mode == .mouse, input.hasCursor, !inMultiFingerAftermath, !isPinching else {
            cancelPendingTaps()
            return
        }
        tp.active = true
        tp.downLocation = loc
        tp.lastLocation = loc
        tp.downCursor = input.cursor
        tp.velocity.reset(loc, at: time)
        let now = CACurrentMediaTime()
        if let last = lastTap, now - last.time < Self.tapWindow, !dragActive {
            // A further tap, or a double-tap-and-drag: the counted clicks
            // wait for this touch's outcome.
            tapTimer?.cancel()
            tapTimer = nil
            tp.pendingDrag = true
            tp.priorTaps = last.count
            tp.downCursor = last.cursor
            let hold = input.immediateClicks ? Self.immediateHoldDragDelay : Self.longPressDelay
            scheduleTrackpadTimer(after: hold) { [weak self] in
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
        guard tp.active, input.mode == .mouse else { return }
        let d = CGPoint(x: loc.x - tp.lastLocation.x, y: loc.y - tp.lastLocation.y)
        tp.lastLocation = loc
        tp.velocity.add(loc, at: time)
        if !tp.moved && hypot(loc.x - tp.downLocation.x, loc.y - tp.downLocation.y) > Self.tapSlop {
            tp.moved = true
            if !tp.pendingDrag {
                cancelTrackpadTimer() // no long press any more
            }
        }
        if isPinching || multiTouchSequence { return }
        let gain = trackpadGain(tp.velocity.velocity(now: time))
        let step = CGPoint(x: d.x * gain, y: d.y * gain)
        if tp.pendingDrag && !dragActive {
            // A touch right after a tap: either one more tap (keep the
            // cursor on the tapped pixel) or a drag (starts once it
            // travelled past the slop; the held travel is applied then).
            tp.heldTravel.x += step.x
            tp.heldTravel.y += step.y
            if tp.moved {
                beginTrackpadDrag()
            }
            return
        }
        moveCursor(byViewPoints: step)
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
            if t.moved, input.cursorFlingEnabled, !multiTouchSequence {
                let v = t.velocity.velocity(now: time)
                let gain = trackpadGain(v)
                cursorFling.start(velocity: CGPoint(x: v.x * gain, y: v.y * gain), now: CACurrentMediaTime())
                if cursorFling.isActive { animTicker.start() }
            }
            return
        }
        let now = CACurrentMediaTime()
        guard !multiTouchSequence, now - multiTouchEnd >= Self.multiFingerLiftDebounce else {
            cancelPendingTaps()
            return
        }
        if input.immediateClicks {
            // Pre-v0.42: click now; a second tap clicks the same pixel.
            input.click(.left, at: trackpadClickPoint(t))
            if t.pendingDrag {
                lastTap = nil
            } else {
                lastTap = (count: 1, cursor: input.cursor, time: now)
            }
            updateCursorLayer()
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
        guard input.mode == .mouse, !dragActive, !multiTouchSequence else { return }
        for _ in 0..<max(1, min(taps.count, 3)) {
            input.click(.left, at: taps.cursor)
        }
        updateCursorLayer()
    }

    /// Drop taps not yet answered (and the TeamViewer tap timer): a second
    /// finger, a move, a long press or a mode switch.
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
        guard tp.active, !tp.moved, !tp.pendingDrag, !tp.longPressed, input.mode == .mouse,
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
        guard tp.active, tp.pendingDrag, !dragActive, input.mode == .mouse,
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

    /// Trackpad gain: the Cursor speed setting (0.5x...3x, linear, default
    /// 1x = TeamViewer), times the acceleration curve when that setting is
    /// on (evaluated on the finger's own speed `v`, points per second).
    /// Used for move steps and the fling velocity alike.
    private func trackpadGain(_ v: CGPoint) -> CGFloat {
        let speed = input.cursorSpeed
        guard input.cursorAccelEnabled else { return speed }
        return speed * Self.pointerGain(speedPtPerMs: hypot(v.x, v.y) / 1000)
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

    /// Scroll inertia: glide on after a scrolling release.
    private func startScrollFling(velocity v: CGPoint, stepped: Bool, factor: CGFloat) {
        scrollFlingStepped = stepped
        scrollFlingFactor = factor
        scrollFling.start(velocity: v, now: CACurrentMediaTime())
        if scrollFling.isActive { animTicker.start() }
    }

    /// Lock local zoom, touch mode: a one-finger drag is a left-button drag
    /// on the host (pressed where the finger first touched).
    private func handleTouchDrag(_ g: UIPanGestureRecognizer) {
        let loc = g.location(in: self)
        switch g.state {
        case .began:
            guard !lpActive, !inMultiFingerAftermath else { return }
            let t = g.translation(in: self)
            let start = CGPoint(x: loc.x - t.x, y: loc.y - t.y)
            touchDragActive = true
            input.buttonDown(.left, at: viewToFrame(start))
            input.moveCursorAbsolute(to: viewToFrame(loc))
            updateCursorLayer()
        case .changed:
            guard touchDragActive, !multiTouchSequence else { return }
            input.moveCursorAbsolute(to: viewToFrame(loc))
            updateCursorLayer()
        case .ended:
            guard touchDragActive else { return }
            touchDragActive = false
            input.buttonUp(.left, at: viewToFrame(loc))
            updateCursorLayer()
        case .cancelled, .failed:
            guard touchDragActive else { return }
            touchDragActive = false
            input.buttonUp(.left)
        default:
            break
        }
    }

    /// Two-finger pan and iPad trackpad / wheel scrolling -> WHEEL. Mouse
    /// mode two-finger pan: at the cursor, one notch per 10 pt (TeamViewer);
    /// touch mode two-finger pan pans the view (classic touch gestures:
    /// wheel at the fingers); iPad pointer scrolling stays continuous.
    @objc private func handleScroll(_ g: UIPanGestureRecognizer) {
        let touchPan = g === pan2
        if touchPan && input.mode == .touch && !input.classicTouchGestures {
            handleTwoFingerPan(g)
            return
        }
        let stepped = touchPan && input.mode == .mouse
        switch g.state {
        case .began:
            scrollFling.stop()
            scrollStepAcc = .zero
            if touchPan && input.mode == .touch {
                // Scroll where the fingers are.
                input.moveCursorAbsolute(to: viewToFrame(g.location(in: self)))
                updateCursorLayer()
            }
        case .changed:
            let t = g.translation(in: self)
            g.setTranslation(.zero, in: self)
            if stepped {
                stepWheel(byViewPoints: t)
            } else {
                sendWheel(byViewPoints: t, factor: wheelFactor(for: g))
            }
        case .ended:
            guard g !== scrollDiscrete, input.input2 else { return }
            // Finger scrolling glides on only with "Scroll inertia".
            if touchPan && !input.scrollInertiaEnabled { return }
            startScrollFling(velocity: g.velocity(in: self), stepped: stepped, factor: wheelFactor(for: g))
        default:
            break
        }
    }

    /// Wheel units per view point; in touch mode the two-finger scroll is
    /// divided by the zoom so content keeps up with the fingers.
    private func wheelFactor(for g: UIGestureRecognizer) -> CGFloat {
        var k = Self.wheelUnitsPerPoint
        if g === pan2 && input.mode == .touch {
            k /= max(zoomFactor, 1)
        }
        return k
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
            lastPinchCentroid = g.numberOfTouches >= 2 ? g.location(in: self) : nil
        case .changed:
            let s = g.scale
            g.scale = 1
            switch input.mode {
            case .mouse:
                // Around the cursor; zoomed in, the viewport eases it to
                // the centre (TeamViewer) or edge-pushes.
                zoom(by: s, pivot: frameToView(input.cursor))
                settleViewport(animated: true)
            case .touch:
                let loc = g.location(in: self)
                if g.numberOfTouches >= 2 {
                    if let last = lastPinchCentroid {
                        offset.x += loc.x - last.x
                        offset.y += loc.y - last.y
                    }
                    lastPinchCentroid = loc
                } else {
                    lastPinchCentroid = nil
                }
                zoom(by: s, pivot: loc)
            }
        case .ended:
            lastPinchCentroid = nil
            let pivot = input.mode == .mouse ? frameToView(input.cursor) : g.location(in: self)
            snapZoom(pivot: pivot)
            if input.mode == .mouse {
                settleViewport(animated: true)
            }
        default:
            lastPinchCentroid = nil
        }
    }

    /// Lock local zoom: pinch = Ctrl+wheel on the host (one notch per 10%
    /// scale change), at the cursor (mouse) or the fingers (touch).
    private func handleRemotePinch(_ g: UIPinchGestureRecognizer) {
        switch g.state {
        case .began:
            stopFlings()
            guard input.input2 else { return } // no wheel on legacy hosts
            if input.mode == .touch && g.numberOfTouches >= 2 {
                input.moveCursorAbsolute(to: viewToFrame(g.location(in: self)))
                updateCursorLayer()
            }
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

    /// Touch mode long tap (0.4 s; classic 0.3 s; 10 pt): loupe; lift =
    /// right click at the finger, drag without lifting = left drag-select.
    /// (Mouse mode: disabled — the trackpad's long press right-clicks.)
    @objc private func handleLongPress(_ g: UILongPressGestureRecognizer) {
        let loc = g.location(in: self)
        guard input.mode == .touch || lpActive else { return }
        switch g.state {
        case .began:
            lpActive = true
            lpDragging = false
            lpStartView = loc
            lpStartFrame = viewToFrame(loc)
            haptic.impactOccurred()
            showLoupe(at: loc)
        case .changed:
            guard lpActive else { return }
            if !lpDragging && hypot(loc.x - lpStartView.x, loc.y - lpStartView.y) > 10 {
                lpDragging = true
                input.buttonDown(.left, at: lpStartFrame)
            }
            if lpDragging {
                input.moveCursorAbsolute(to: viewToFrame(loc))
                updateCursorLayer()
            }
            showLoupe(at: loc)
        case .ended:
            guard lpActive else { return }
            lpActive = false
            hideLoupe()
            if lpDragging {
                input.buttonUp(.left, at: viewToFrame(loc))
            } else {
                input.click(.right, at: lpStartFrame)
            }
            lpDragging = false
            updateCursorLayer()
        case .cancelled, .failed:
            guard lpActive else { return }
            lpActive = false
            hideLoupe()
            if lpDragging { input.buttonUp(.left) }
            lpDragging = false
        default:
            break
        }
    }

    @objc private func handleHover(_ g: UIHoverGestureRecognizer) {
        guard g.state == .began || g.state == .changed, !pointerLocked else { return }
        input.moveCursorAbsolute(to: viewToFrame(g.location(in: self)))
        updateCursorLayer()
    }

    // MARK: UIGestureRecognizerDelegate

    /// CRD pairs: pinch + pan, long-press + pan, two-finger tap +
    /// long-press; touch mode pinch + two-finger pan (pan the view while
    /// zooming); the trackpad recognizer is simultaneous with everything
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
        func pair(_ a: UIGestureRecognizer, _ b: UIGestureRecognizer) -> Bool {
            (g === a && other === b) || (g === b && other === a)
        }
        if pair(pinch, pan2) {
            return input.mode == .touch && !input.classicTouchGestures && !input.lockLocalZoom
        }
        return pair(pinch, pan1)
            || pair(longPress, pan1)
            || pair(tap2, longPress)
    }

    /// Touch mode long tap: 0.4 s (TeamViewer), 0.3 s with the classic
    /// touch gestures — read when each touch arrives, so the setting
    /// applies live.
    func gestureRecognizer(_ gestureRecognizer: UIGestureRecognizer, shouldReceive touch: UITouch) -> Bool {
        if gestureRecognizer === longPress {
            let d: TimeInterval = input.classicTouchGestures ? 0.30 : 0.40
            if longPress.minimumPressDuration != d {
                longPress.minimumPressDuration = d
            }
        }
        return true
    }
}
