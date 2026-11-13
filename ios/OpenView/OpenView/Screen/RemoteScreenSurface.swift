import Combine
import QuartzCore
import SwiftUI
import UIKit
import UIKit.UIGestureRecognizerSubclass

// The remote screen surface: frame + H.264 overlay inside one
// zoom/pan-transformed container layer, a locally drawn cursor, the touch /
// pointer / keyboard input layer and the viewport. Self-contained so the
// immersive session can re-host it.
//
// Files: this one holds the view's state, setup, rendering, cursor and
// keyboard handling; RemoteSurfaceView+Viewport.swift the zoom / pan
// geometry (scale math in ViewportMath.swift), +Gestures.swift the touch
// gestures, +Pointer.swift the iPad pointer. A session opens at the
// `.openDefault` zoom (remote height = 2/5 of the view, never below fit).
//
// Gestures (familiar remote-desktop trackpad model). Mouse
// (trackpad) input is the only touch interpretation:
//
//     1-finger pan        absolute model: normalised position
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
//                         Soft keyboard / key bar up (the view keeps its
//                         size, the keyboard covers its bottom): the zoom
//                         is unchanged, but the image may scroll up until
//                         its bottom meets the keyboard's top edge, and the
//                         viewport scrolls just enough to keep the cursor
//                         24 pt above the keyboard (centred in the area
//                         above it when zoomed in); restored when it hides.
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
//                         within +-20% on release. Zoom range:
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
// Coexistence with the SwiftUI chrome drawn over this view: the
// surface consumes its touches (never forwards them up the responder
// chain into the hosting view), never takes touches inside the chrome's
// frames (`RemoteInputController.chromeExclusionRects`), and its
// recognizers never block recognizers of ancestor views (SwiftUI's hosting
// view gesture recognizer drives every SwiftUI Button in the session).

/// SwiftUI wrapper. The parent observes ScreenModel and passes values in.
struct RemoteScreenSurface: UIViewRepresentable {
    /// The frame as a CGImage — only on the CGImage fallback path (no
    /// Metal); nil when frames go straight to the Metal texture.
    let image: CGImage?
    /// Bumped for every published frame (both paths).
    let frameTick: UInt64
    /// The frame uploads staged for the Metal renderer (nil = no
    /// Metal).
    let uploads: FrameUploadQueue?
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
        uiView.setFrame(image, tick: frameTick, size: frameSize, uploads: uploads)
        uiView.setCursorShape(cursorImage, hotspot: cursorHotspot, size: cursorSize)
        if let p = cursorPos {
            uiView.hostCursorMoved(to: p)
        }
    }

    static func dismantleUIView(_ uiView: RemoteSurfaceView, coordinator: ()) {
        uiView.teardown()
    }
}

// MARK: - Surface view

final class RemoteSurfaceView: UIView, RemoteKeyCommandTarget {
    let input: RemoteInputController
    let keyRouter: HardwareKeyRouter
    let keyInput: RemoteKeyInputView

    // ---- layers ----
    /// Zoom/pan-transformed container: bounds = frame pixels, transform =
    /// scale (points per pixel), position = view offset of the frame origin.
    let container = CALayer()
    /// The CGImage frame (fallback when Metal is unavailable; hidden
    /// otherwise — the Metal layer below the container draws the frame).
    let frameLayer = CALayer()
    /// Area/sharpen scaler (nil = no Metal: CGImage path).
    let metal = MetalScreenRenderer()
    /// Staged dirty-rect uploads the renderer drains every redraw.
    var uploads: FrameUploadQueue?
    /// The last frame tick shown.
    var frameTick: UInt64 = 0
    /// A redraw is wanted at the next display refresh.
    var needsRender = false
    var idleRenderTicks = 0
    /// Hide the H.264 overlay of this generation once the next drawable
    /// (which holds the lossless refresh) has been presented.
    var hideVideoAfterPresent: Int?
    lazy var renderTicker = FrameTicker { [weak self] _ in
        self?.renderTick()
    }
    weak var video: VideoOverlay?
    let cursorLayer = CALayer()
    let dragRing = CAShapeLayer()

    // ---- frame / cursor shape ----
    var image: CGImage?
    var frameSize: CGSize = .zero
    var cursorImage: CGImage?
    var cursorHotspot: CGSize = .zero
    var cursorPixelSize: CGSize = .zero
    var shownCursorImage: CGImage?
    var lastHostCursor: CGPoint?

    // ---- viewport ----
    /// Points per remote pixel (0 until laid out with a frame).
    var scale: CGFloat = 0
    /// View position of the frame's top-left corner.
    var offset: CGPoint = .zero
    /// The preset the viewport follows when the view / frame size changes
    /// (nil = free zoom, kept as is).
    var followPreset: ZoomPreset? = .openDefault
    var lastBoundsSize: CGSize = .zero
    /// Bottom edge (window y) of the top chrome (stats chip), 0 = none:
    /// the image is laid out below it (`safeInsets`).
    var topChromeBottom: CGFloat = 0
    var topChromeCancellable: AnyCancellable?
    /// The on-screen keyboard's end frame (screen coordinates, includes the
    /// key bar input accessory view, also when shown alone); nil = hidden.
    /// The view ignores the keyboard safe area (it keeps its size), so the
    /// keyboard covers its bottom part: `keyboardVisibleBottom`.
    var keyboardScreenFrame: CGRect?

    // ---- gestures ----
    let tap2 = UITapGestureRecognizer(target: nil, action: nil)
    let tap3 = UITapGestureRecognizer(target: nil, action: nil)
    let pan2 = UIPanGestureRecognizer(target: nil, action: nil)
    let pinch = UIPinchGestureRecognizer(target: nil, action: nil)
    /// The one-finger trackpad (moves, taps, drags, long press).
    let trackpad = TrackpadTouchRecognizer(target: nil, action: nil)
    let hover = UIHoverGestureRecognizer(target: nil, action: nil)
    let scrollContinuous = UIPanGestureRecognizer(target: nil, action: nil)
    let scrollDiscrete = UIPanGestureRecognizer(target: nil, action: nil)

    let haptic = UIImpactFeedbackGenerator(style: .medium)
    let pointerHider = PointerHider()
    let gcMouse = GCMouseBridge()

    /// A double-tap-and-drag holds the left button.
    var dragActive = false
    /// Trackpad touch (TrackpadTouchRecognizer) state.
    var tp = TrackpadTouch()
    /// Trackpad taps not yet answered: how many, the cursor pixel they
    /// click, and when the last one lifted. A touch starting within
    /// `tapWindow` of that is a further tap or a double-tap-and-drag.
    var lastTap: (count: Int, cursor: CGPoint, time: CFTimeInterval)?
    /// Tap timer: fires `tapWindow` after the last tap lifted
    /// and sends the counted clicks.
    var tapTimer: DispatchWorkItem?
    /// Long-press (right click) / double-tap-and-hold (drag) timer.
    var tpTimer: DispatchWorkItem?
    /// Two-finger wheel scrolling: finger travel (points)
    /// not yet emitted as whole 10 pt wheel steps.
    var scrollStepAcc: CGPoint = .zero
    /// Indirect pointer touch -> the button it pressed.
    var pointerButtons: [ObjectIdentifier: MouseButton] = [:]
    /// Multi-finger lift debounce: a multi-finger touch sequence is in
    /// progress (until every finger lifted), and when the last one ended.
    var multiTouchSequence = false
    var multiTouchEnd: CFTimeInterval = 0
    /// Lock local zoom: Ctrl held on the host for a pinch (Ctrl+wheel).
    var pinchCtrlHeld = false

    // ---- animation ----
    /// iPad trackpad (continuous pointer scroll) inertia.
    var scrollFling = FlingTracker(timeConstant: 250)
    /// Zoomed in: the cursor-centred offset the viewport is
    /// easing towards (nil = not animating).
    var followTarget: CGPoint?
    /// Easing time constant for the current `followTarget` easing when it
    /// was started by the keyboard (derived from the keyboard animation's
    /// duration); nil = `followTimeConstant`. Cleared when the easing ends.
    var followTimeConstantOverride: Double?
    /// Timestamp of the previous animation tick (0 = none yet).
    var lastAnimTime: CFTimeInterval = 0
    lazy var animTicker = FrameTicker { [weak self] now in
        self?.animate(now)
    }

    // ---- constants ----
    static let cursorMinPoints: CGFloat = 20
    static let cursorMaxPoints: CGFloat = 64
    /// iPad pointer scrolling: wheel units (1/120 notch) per point, one
    /// notch per 36 pt.
    static let wheelUnitsPerPoint: CGFloat = 120.0 / 36.0
    /// Smooth centring (viewport catching up after a jump: resync, zoom,
    /// preset), exponential time constant in seconds.
    static let followTimeConstant: Double = 0.08
    /// Keyboard up: the cursor is kept at least this far above the
    /// keyboard's top edge (and below the top of the visible area).
    static let keyboardFollowMargin: CGFloat = 24
    /// Tap windows (double click at the same pixel, drag-end tap swallow).
    static let tapWindow: CFTimeInterval = 0.3
    /// Ignore the remaining finger this long after a multi-finger gesture.
    static let multiFingerLiftDebounce: CFTimeInterval = 0.1
    /// Lock local zoom: pinch scale change per wheel notch (10%).
    static let pinchNotchScale: Double = 1.1
    static let defaultCursorSize = CGSize(width: 12, height: 19)

    static let noActions: [String: CAAction] = [
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
        // Soft keyboard / key bar: the viewport keeps the cursor above it
        // (selector observers are removed automatically on deallocation).
        let center = NotificationCenter.default
        center.addObserver(self, selector: #selector(keyboardWillChangeFrame(_:)),
                           name: UIResponder.keyboardWillChangeFrameNotification, object: nil)
        center.addObserver(self, selector: #selector(keyboardWillHide(_:)),
                           name: UIResponder.keyboardWillHideNotification, object: nil)
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    // MARK: Setup

    func setupLayers() {
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

    func setupGestures() {
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
        // trackpad handlers.
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

        // Recognizer wiring: the two-finger tap waits for the
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


    /// Hook the controller's view-side callbacks to this surface.
    func installInputHooks() {
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
    func endLocalGestures() {
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

    func releasePointerButtons() {
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

    func setFrame(_ img: CGImage?, tick: UInt64, size: CGSize, uploads: FrameUploadQueue?) {
        self.uploads = uploads
        if size != frameSize && size.width > 0 && size.height > 0 {
            frameSize = size
            CATransaction.begin()
            CATransaction.setDisableActions(true)
            container.bounds = CGRect(origin: .zero, size: size)
            frameLayer.frame = container.bounds
            CATransaction.commit()
            video?.layout(in: container.bounds)
            input.frameSize = size
            if followPreset == nil { followPreset = .openDefault }
            refit()
        }
        if tick != frameTick {
            frameTick = tick
            if metal == nil {
                // CGImage fallback: show the new image.
                if let img, img !== image {
                    image = img
                    CATransaction.begin()
                    CATransaction.setDisableActions(true)
                    frameLayer.contents = img
                    CATransaction.commit()
                }
            } else {
                // Metal: the redraw blits the staged dirty rects first.
                setNeedsRender()
            }
        } else if let metal, !metal.hasFrame {
            // A new surface over an existing session: the redraw asks the
            // session to stage the whole frame again.
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

    // MARK: Metal rendering

    /// Ask for one redraw at the next display refresh (frame, zoom / pan or
    /// size changed). The display link runs only while redraws keep coming
    /// (pan / zoom / fling animation, up to 120 Hz) and stops when idle.
    func setNeedsRender() {
        guard metal != nil else { return }
        needsRender = true
        idleRenderTicks = 0
        if window != nil && !renderTicker.isRunning {
            renderTicker.start()
        }
    }

    func renderTick() {
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

    func renderNow() {
        guard let metal, window != nil, scale > 0, bounds.width > 0, bounds.height > 0,
              UIApplication.shared.applicationState != .background else { return }
        var presented: (() -> Void)?
        let hideGen = hideVideoAfterPresent
        if let gen = hideGen, let overlay = video {
            hideVideoAfterPresent = nil
            presented = { [weak overlay] in
                DispatchQueue.main.async {
                    overlay?.hide(generation: gen)
                }
            }
        }
        let done = metal.render(scale: scale, offset: offset, sharpenOn: AppSettings.sharpen,
                                uploads: uploads, onPresented: presented)
        if !done {
            // The GPU still has the previous frame: retry at the next
            // refresh (keeps the display link running), nothing lost.
            if presented != nil {
                hideVideoAfterPresent = hideGen
            }
            needsRender = true
        }
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

    static let defaultCursor: CGImage? = {
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
    func updateCursorLayer() {
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
    /// 1:1, no gain or acceleration) ->
    /// pct += d / (R * z): `scale` is view POINTS per remote pixel (not
    /// device pixels), so frameSize * scale is the displayed image size in
    /// points. The controller sends the resulting absolute position.
    func moveCursor(byViewPoints d: CGPoint) {
        guard scale > 0, frameSize.width > 0, frameSize.height > 0 else { return }
        let displayed = CGSize(width: frameSize.width * scale, height: frameSize.height * scale)
        input.moveCursorTrackpad(byPoints: d, displayed: displayed)
        followCursorAfterMove()
    }

    func setDragRingVisible(_ visible: Bool) {
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        dragRing.isHidden = !visible
        dragRing.position = frameToView(input.cursor)
        CATransaction.commit()
    }

    // MARK: Animation (iPad trackpad scroll inertia, viewport easing)

    func stopFlings() {
        scrollFling.stop()
    }

    func animate(_ now: CFTimeInterval) {
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

    /// The soft keyboard / key bar is moving to a new frame (shown, resized,
    /// undocked, or going away: the end frame is then off screen).
    @objc func keyboardWillChangeFrame(_ note: Notification) {
        let info = note.userInfo ?? [:]
        let end = (info[UIResponder.keyboardFrameEndUserInfoKey] as? NSValue)?.cgRectValue
        keyboardFrameChanged(end, note)
    }

    @objc func keyboardWillHide(_ note: Notification) {
        keyboardFrameChanged(nil, note)
    }

    /// Re-place the viewport for the new area above the keyboard, eased
    /// over the keyboard's animation duration: scrolled just enough to keep
    /// the cursor above the keyboard when it comes up (never zoomed), back
    /// to the normal placement (pinned to the top at fit, cursor-centred
    /// when zoomed in) when it goes away.
    func keyboardFrameChanged(_ end: CGRect?, _ note: Notification) {
        var frame = end
        if let f = frame, f.isNull || f.isEmpty {
            frame = nil
        }
        guard frame != keyboardScreenFrame else { return }
        let before = keyboardVisibleBottom
        keyboardScreenFrame = frame
        guard window != nil, scale > 0, before != keyboardVisibleBottom else { return }
        let duration = (note.userInfo?[UIResponder.keyboardAnimationDurationUserInfoKey] as? NSNumber)?
            .doubleValue ?? 0.25
        if duration > 0 {
            // ~95% of the way after the keyboard's duration (its spring
            // settles a little later).
            settleViewport(animated: true, timeConstant: max(duration, 0.1) / 3)
        } else {
            followTarget = nil
            followTimeConstantOverride = nil
            settleViewport(animated: false)
        }
    }

    /// Become first responder (hardware keys) unless the key input view
    /// already is.
    func claimKeyboardFocus() {
        guard window != nil, !isFirstResponder, !keyInput.isFirstResponder else { return }
        _ = becomeFirstResponder()
    }

    /// soft: true = system keyboard + bar, false = bar alone, nil = hide.
    /// Asking again for what is already shown hides it (toggle).
    func presentKeyboard(_ soft: Bool?) {
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
    func updateTouching(_ event: UIEvent?) {
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
    var inMultiFingerAftermath: Bool {
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

}
