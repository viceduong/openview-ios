import Combine
import QuartzCore
import SwiftUI
import UIKit

// The remote screen surface (section 2): frame + H.264 overlay inside one
// zoom/pan-transformed container layer, a locally drawn cursor, the touch /
// pointer / keyboard input layer and the viewport. Self-contained so the
// immersive session (section 3) can re-host it.
//
// Gestures (CRD structure, timings tuned to TeamViewer iOS 15.81):
//
//   MOUSE (trackpad) mode
//     1-finger pan        relative cursor move, linear and scaled by zoom
//                         (optional acceleration curve 1x -> 2.5x, setting
//                         "ov.cursorAccel"); viewport edge-push follows
//     tap                 left click at the cursor (2nd tap = double click)
//     tap, then hold/drag left drag (DOWN at 0.15 s, UP on lift) + ring
//     long-press 0.30 s   right click (10 pt cancel radius)
//     2-finger tap        right click (instant: no double tap to wait for);
//                         3-finger tap: middle click
//     2-finger pan        wheel (natural direction) + inertia (T = 250 ms)
//     pinch               zoom around the cursor; snaps to fit / fill / 1:1
//                         within +-20% on release
//   TOUCH (direct) mode
//     tap                 left click at the finger (+ ripple)
//     long-press          loupe; lift = right click, drag = left drag
//     1-finger pan        pan the zoomed view
//     2-finger pan        wheel at the fingers (scaled by zoom)
//     pinch               zoom at the fingers (and pan with them)
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
    private let holdDrag = UILongPressGestureRecognizer(target: nil, action: nil)
    private let hover = UIHoverGestureRecognizer(target: nil, action: nil)
    private let scrollContinuous = UIPanGestureRecognizer(target: nil, action: nil)
    private let scrollDiscrete = UIPanGestureRecognizer(target: nil, action: nil)

    private let haptic = UIImpactFeedbackGenerator(style: .medium)
    private let pointerHider = PointerHider()
    private let gcMouse = GCMouseBridge()
    private var cancellables = Set<AnyCancellable>()

    /// Mouse mode: a tap-then-hold drag holds the left button.
    private var dragActive = false
    private var lastDragEnd: CFTimeInterval = 0
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
    /// Max zoom: one remote pixel = 3 device pixels (3x native).
    private static let maxDevicePixelsPerRemotePixel: CGFloat = 3
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
        longPress.minimumPressDuration = 0.30
        longPress.allowableMovement = 10
        // Tap, then touch again and hold 0.15 s (moving or not) = drag.
        holdDrag.addTarget(self, action: #selector(handleHoldDrag(_:)))
        holdDrag.numberOfTapsRequired = 1
        holdDrag.minimumPressDuration = 0.15
        holdDrag.allowableMovement = 10_000

        let touchRecognizers: [UIGestureRecognizer] = [
            tap1, tap2, tap3, pan1, pan2, pinch, longPress, holdDrag,
        ]
        for g in touchRecognizers {
            g.delegate = self
            g.cancelsTouchesInView = false
            g.allowedTouchTypes = touchTypes
            addGestureRecognizer(g)
        }

        // Recognizer wiring (CRD): single tap waits for the two-finger tap
        // to fail, two-finger tap for the three-finger tap; pinch and the
        // one-finger pan wait for the single tap (a tap fails as soon as the
        // finger moves, so pans are not delayed noticeably). There is no
        // two-finger double tap any more, so a two-finger tap right-clicks
        // without waiting for a second tap.
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
            self.updateCursorLayer()
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
            renderTicker.stop()
            gcMouse.stop()
            releasePointerButtons()
            endLocalGestures()
            input.touching = false
        } else {
            installInputHooks()
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
        // whatever it pressed).
        holdDrag.isEnabled = mode == .mouse
        endLocalGestures()
        cursorFling.stop()
        scrollFling.stop()
    }

    /// Drop transient gesture UI / held buttons (mode switch, teardown).
    private func endLocalGestures() {
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
        metal.render(scale: scale, offset: offset, sharpenOn: AppSettings.sharpen)
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
        }
    }

    // MARK: Viewport

    private var displayScale: CGFloat {
        let s = traitCollection.displayScale
        return s > 0 ? s : UIScreen.main.scale
    }

    /// Scale that fits the whole frame (letterboxed): min of the ratios.
    private var fitScale: CGFloat {
        guard frameSize.width > 0, frameSize.height > 0,
              bounds.width > 0, bounds.height > 0 else { return 0 }
        return min(bounds.width / frameSize.width, bounds.height / frameSize.height)
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

    /// Up to 3x native, and never below fill.
    private var maxScale: CGFloat {
        max(fitScale, fillScale, Self.maxDevicePixelsPerRemotePixel / displayScale)
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

    private func scaleFor(_ preset: ZoomPreset) -> CGFloat {
        switch preset {
        case .fit: return fitScale
        case .fill: return clampScale(fillScale)
        case .native: return clampScale(nativeScale)
        }
    }

    /// Keep the content inside the view (centered on axes where it is
    /// smaller than the view).
    private func clampOffset() {
        let cw = frameSize.width * scale
        let ch = frameSize.height * scale
        if cw <= bounds.width {
            offset.x = (bounds.width - cw) / 2
        } else {
            offset.x = min(0, max(bounds.width - cw, offset.x))
        }
        if ch <= bounds.height {
            offset.y = (bounds.height - ch) / 2
        } else {
            offset.y = min(0, max(bounds.height - ch, offset.y))
        }
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
        clampOffset()
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
        clampOffset()
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
        clampOffset()
        applyViewport()
        if input.mode == .mouse {
            edgePush()
        }
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

    /// Mouse mode: pan the zoomed view so the cursor stays out of the
    /// outer 13% band.
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
            refit()
            // The view shrank (soft keyboard / key bar) or rotated: keep
            // the cursor in sight.
            if input.mode == .mouse {
                edgePush()
            }
        }
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

    /// Trackpad motion of `d` view points -> relative cursor move.
    private func moveCursor(byViewPoints d: CGPoint) {
        guard scale > 0 else { return }
        input.moveCursorRelative(by: CGPoint(x: d.x / scale, y: d.y / scale))
        edgePush()
        updateCursorLayer()
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
            sendWheel(byViewPoints: d, factor: scrollFlingFactor)
        }
        if !running {
            animTicker.stop()
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
        super.touchesBegan(touches, with: event)
    }

    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
        for t in touches where t.type == .indirectPointer {
            guard pointerButtons[ObjectIdentifier(t)] != nil else { continue }
            input.moveCursorAbsolute(to: viewToFrame(t.location(in: self)))
            updateCursorLayer()
        }
        super.touchesMoved(touches, with: event)
    }

    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) {
        for t in touches where t.type == .indirectPointer {
            pointerTouchEnded(t)
        }
        updateTouching(event)
        super.touchesEnded(touches, with: event)
    }

    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) {
        for t in touches where t.type == .indirectPointer {
            pointerTouchEnded(t)
        }
        updateTouching(event)
        super.touchesCancelled(touches, with: event)
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
            // The tap that ends a tap-hold drag is not a click.
            if dragActive || now - lastDragEnd < Self.tapWindow { return }
            input.click(.left)
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

    @objc private func handlePan1(_ g: UIPanGestureRecognizer) {
        if input.mode == .touch && (input.lockLocalZoom || touchDragActive) {
            handleTouchDrag(g)
            return
        }
        switch g.state {
        case .began:
            cursorFling.stop()
            pan1Suppressed = inMultiFingerAftermath
        case .changed:
            let t = g.translation(in: self)
            g.setTranslation(.zero, in: self)
            if isPinching || pan1Suppressed || multiTouchSequence { return }
            switch input.mode {
            case .mouse:
                let gain = cursorGain(g)
                moveCursor(byViewPoints: CGPoint(x: t.x * gain, y: t.y * gain))
            case .touch:
                if lpActive { return }
                panViewport(by: t)
            }
        case .ended:
            let suppressed = pan1Suppressed
            pan1Suppressed = false
            guard input.mode == .mouse, input.cursorFlingEnabled, !dragActive, !suppressed else { return }
            let v = g.velocity(in: self)
            let gain = cursorGain(g)
            cursorFling.start(velocity: CGPoint(x: v.x * gain, y: v.y * gain), now: CACurrentMediaTime())
            if cursorFling.isActive { animTicker.start() }
        default:
            pan1Suppressed = false
        }
    }

    /// Trackpad gain: 1 (linear) unless the acceleration setting is on.
    private func cursorGain(_ g: UIPanGestureRecognizer) -> CGFloat {
        guard input.cursorAccelEnabled else { return 1 }
        let v = g.velocity(in: self)
        return Self.pointerGain(speedPtPerMs: hypot(v.x, v.y) / 1000)
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

    /// Two-finger pan and iPad trackpad / wheel scrolling -> WHEEL.
    @objc private func handleScroll(_ g: UIPanGestureRecognizer) {
        switch g.state {
        case .began:
            scrollFling.stop()
            if g === pan2 && input.mode == .touch {
                // Scroll where the fingers are.
                input.moveCursorAbsolute(to: viewToFrame(g.location(in: self)))
                updateCursorLayer()
            }
        case .changed:
            let t = g.translation(in: self)
            g.setTranslation(.zero, in: self)
            sendWheel(byViewPoints: t, factor: wheelFactor(for: g))
        case .ended:
            guard g !== scrollDiscrete, input.input2 else { return }
            scrollFlingFactor = wheelFactor(for: g)
            scrollFling.start(velocity: g.velocity(in: self), now: CACurrentMediaTime())
            if scrollFling.isActive { animTicker.start() }
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
                zoom(by: s, pivot: frameToView(input.cursor))
                edgePush()
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
                edgePush()
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

    @objc private func handleLongPress(_ g: UILongPressGestureRecognizer) {
        let loc = g.location(in: self)
        if input.mode == .mouse {
            // Right click at the cursor.
            if g.state == .began {
                haptic.impactOccurred()
                input.click(.right)
            }
            return
        }
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

    /// Mouse mode: tap, then touch-and-hold = left drag (pan1 moves the
    /// cursor meanwhile).
    @objc private func handleHoldDrag(_ g: UILongPressGestureRecognizer) {
        switch g.state {
        case .began:
            guard input.mode == .mouse, !dragActive else { return }
            dragActive = true
            haptic.impactOccurred()
            input.buttonDown(.left)
            setDragRingVisible(true)
        case .ended, .cancelled, .failed:
            guard dragActive else { return }
            dragActive = false
            input.buttonUp(.left)
            setDragRingVisible(false)
            lastDragEnd = CACurrentMediaTime()
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
    /// long-press; plus tap-hold drag + pan / tap. Pointer recognizers
    /// (hover, scroll) never block anything.
    func gestureRecognizer(_ gestureRecognizer: UIGestureRecognizer,
                           shouldRecognizeSimultaneouslyWith other: UIGestureRecognizer) -> Bool {
        let g = gestureRecognizer
        if g === hover || other === hover { return true }
        if g === scrollContinuous || other === scrollContinuous { return true }
        if g === scrollDiscrete || other === scrollDiscrete { return true }
        func pair(_ a: UIGestureRecognizer, _ b: UIGestureRecognizer) -> Bool {
            (g === a && other === b) || (g === b && other === a)
        }
        return pair(pinch, pan1)
            || pair(longPress, pan1)
            || pair(holdDrag, pan1)
            || pair(holdDrag, tap1)
            || pair(tap2, longPress)
    }
}
