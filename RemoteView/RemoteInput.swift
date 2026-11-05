import Combine
import CoreGraphics
import Foundation
import QuartzCore

// Remote input (section 2): the wire encoders for every input command on
// channel 0, plus `RemoteInputController`, the main-thread object that turns
// high-level intents (move the cursor, click, scroll, type, sticky
// modifiers) into those commands. The gesture surface (RemoteScreenSurface),
// the key accessory bar and the keyboard views only talk to the controller,
// so they can be re-hosted (section 3) without touching the wire format.
//
// Wire protocol (every integer big-endian):
//   legacy, always understood:
//     0x30 POINTER  [kind u8 0 move/1 down/2 up][x u32][y u32] (0..65535)
//     0x31 KEY      [vk u16][down u8]
//   only when the host's CAPS has CAP_INPUT2 (0x40):
//     0x34 POINTER_EX [action u8][button u8][flags u8][x i32][y i32]
//     0x35 WHEEL      [dy i16][dx i16]   (1/120 notch, dy > 0 = scroll up)
//     0x39 KEY_EX     [vk u16][scan u16][flags u8] (bit0 down, bit1 E0)
//     0x3A TEXT       [utf-8...]
//     0x3B RELEASE_ALL, 0x3C SAS

/// POINTER_EX button ids.
enum MouseButton: UInt8 {
    case left = 0
    case right = 1
    case middle = 2
    case x1 = 3
    case x2 = 4
}

/// One Windows key: virtual-key code, PC set-1 make code (0 = the host
/// derives it from the vk) and whether it is an E0-extended key.
struct KeyStroke: Hashable {
    let vk: UInt16
    let scan: UInt16
    let extended: Bool

    init(_ vk: UInt16, _ scan: UInt16, ext: Bool = false) {
        self.vk = vk
        self.scan = scan
        self.extended = ext
    }

    static let enter = KeyStroke(0x0D, 0x1C)
    static let escape = KeyStroke(0x1B, 0x01)
    static let tab = KeyStroke(0x09, 0x0F)
    static let backspace = KeyStroke(0x08, 0x0E)
    static let space = KeyStroke(0x20, 0x39)
    static let insert = KeyStroke(0x2D, 0x52, ext: true)
    static let delete = KeyStroke(0x2E, 0x53, ext: true)
    static let home = KeyStroke(0x24, 0x47, ext: true)
    static let end = KeyStroke(0x23, 0x4F, ext: true)
    static let pageUp = KeyStroke(0x21, 0x49, ext: true)
    static let pageDown = KeyStroke(0x22, 0x51, ext: true)
    static let left = KeyStroke(0x25, 0x4B, ext: true)
    static let up = KeyStroke(0x26, 0x48, ext: true)
    static let right = KeyStroke(0x27, 0x4D, ext: true)
    static let down = KeyStroke(0x28, 0x50, ext: true)
    static let printScreen = KeyStroke(0x2C, 0x37, ext: true)
    static let leftCtrl = KeyStroke(0xA2, 0x1D)
    static let leftShift = KeyStroke(0xA0, 0x2A)
    static let leftAlt = KeyStroke(0xA4, 0x38)
    static let leftWin = KeyStroke(0x5B, 0x5B, ext: true)
    static let scrollLock = KeyStroke(0x91, 0x46)
    /// Pause/Break (an E1 sequence on the wire: the host derives the scancode).
    static let pause = KeyStroke(0x13, 0)
    // Letters used by the session menu's shortcuts (vk, set-1 make code).
    static let keyC = KeyStroke(0x43, 0x2E)
    static let keyD = KeyStroke(0x44, 0x20)
    static let keyL = KeyStroke(0x4C, 0x26)
    static let keyR = KeyStroke(0x52, 0x13)
    static let keyS = KeyStroke(0x53, 0x1F)
    static let keyV = KeyStroke(0x56, 0x2F)

    /// F1...F24 (n is 1-based); nil outside that range.
    static func function(_ n: Int) -> KeyStroke? {
        guard n >= 1, n <= 24 else { return nil }
        let scans: [UInt16] = [0x3B, 0x3C, 0x3D, 0x3E, 0x3F, 0x40, 0x41, 0x42, 0x43, 0x44, 0x57, 0x58,
                               0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6A, 0x6B, 0x6C, 0x6D, 0x6E, 0x76]
        return KeyStroke(UInt16(0x70 + n - 1), scans[n - 1])
    }

    /// Set-1 make codes of A...Z.
    private static let letterScans: [UInt16] = [
        0x1E, 0x30, 0x2E, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32,
        0x31, 0x18, 0x19, 0x10, 0x13, 0x1F, 0x14, 0x16, 0x2F, 0x11, 0x2D, 0x15, 0x2C,
    ]
    /// Set-1 make codes of 0...9.
    private static let digitScans: [UInt16] = [0x0B, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A]

    /// The key for an ASCII letter (either case) or digit; nil otherwise.
    static func ascii(_ scalar: Unicode.Scalar) -> KeyStroke? {
        let v = scalar.value
        if v >= 0x61 && v <= 0x7A { // a-z
            let i = Int(v - 0x61)
            return KeyStroke(UInt16(0x41 + i), letterScans[i])
        }
        if v >= 0x41 && v <= 0x5A { // A-Z
            let i = Int(v - 0x41)
            return KeyStroke(UInt16(0x41 + i), letterScans[i])
        }
        if v >= 0x30 && v <= 0x39 { // 0-9
            let i = Int(v - 0x30)
            return KeyStroke(UInt16(0x30 + i), digitScans[i])
        }
        return nil
    }

    /// The vk for the legacy KEY command: the side-specific modifier codes
    /// KEY_EX uses are folded into the generic ones old hosts understand.
    var legacyVK: UInt16 {
        switch vk {
        case 0xA0, 0xA1: return 0x10
        case 0xA2, 0xA3: return 0x11
        case 0xA4, 0xA5: return 0x12
        default: return vk
        }
    }
}

/// The four sticky modifiers of the accessory bar.
enum ModifierKey: Int, CaseIterable {
    case ctrl = 0
    case alt = 1
    case shift = 2
    case win = 3

    var stroke: KeyStroke {
        switch self {
        case .ctrl: return .leftCtrl
        case .alt: return .leftAlt
        case .shift: return .leftShift
        case .win: return .leftWin
        }
    }
}

/// Local viewport zoom presets (session menu / pinch snapping).
enum ZoomPreset {
    /// The whole remote screen visible (letterboxed).
    case fit
    /// The view filled (cropped on one axis).
    case fill
    /// One remote pixel = one device pixel.
    case native
}

/// Sticky modifier state: latched = one-shot (applies to the next key or
/// click, then turns off); locked = applies until tapped again.
enum ModifierState {
    case off
    case latched
    case locked
}

// MARK: - Wire encoders

/// Pure builders for every input command (no state; unit-test friendly).
enum InputWire {
    static let opPointer: UInt8 = 0x30
    static let opKey: UInt8 = 0x31
    static let opPointerEx: UInt8 = 0x34
    static let opWheel: UInt8 = 0x35
    static let opKeyEx: UInt8 = 0x39
    static let opText: UInt8 = 0x3A
    static let opReleaseAll: UInt8 = 0x3B
    static let opSAS: UInt8 = 0x3C

    /// POINTER_EX actions.
    static let actionMoveAbs: UInt8 = 0
    static let actionDown: UInt8 = 1
    static let actionUp: UInt8 = 2
    static let actionMoveRel: UInt8 = 3
    /// POINTER_EX flags.
    static let flagHasPos: UInt8 = 0x01
    static let flagRaw: UInt8 = 0x02
    /// KEY_EX flags.
    static let keyFlagDown: UInt8 = 0x01
    static let keyFlagExtended: UInt8 = 0x02

    /// Host CAPS bit announcing the commands above.
    static let capInput2: UInt32 = 0x40
    /// Max TEXT payload bytes per record.
    static let maxTextChunk = 60 * 1024

    private static func be16(_ v: UInt16, into d: inout Data) {
        d.append(UInt8(truncatingIfNeeded: v >> 8))
        d.append(UInt8(truncatingIfNeeded: v))
    }

    private static func be32(_ v: UInt32, into d: inout Data) {
        d.append(UInt8(truncatingIfNeeded: v >> 24))
        d.append(UInt8(truncatingIfNeeded: v >> 16))
        d.append(UInt8(truncatingIfNeeded: v >> 8))
        d.append(UInt8(truncatingIfNeeded: v))
    }

    /// Legacy POINTER (kind 0 move, 1 left-down, 2 left-up).
    static func pointer(kind: UInt8, x: UInt32, y: UInt32) -> Data {
        var d = Data([opPointer, kind])
        be32(x, into: &d)
        be32(y, into: &d)
        return d
    }

    /// Legacy KEY.
    static func key(vk: UInt16, down: Bool) -> Data {
        var d = Data([opKey])
        be16(vk, into: &d)
        d.append(down ? 1 : 0)
        return d
    }

    /// POINTER_EX (12 bytes).
    static func pointerEx(action: UInt8, button: UInt8, flags: UInt8, x: Int32, y: Int32) -> Data {
        var d = Data([opPointerEx, action, button, flags])
        be32(UInt32(bitPattern: x), into: &d)
        be32(UInt32(bitPattern: y), into: &d)
        return d
    }

    /// WHEEL (5 bytes), 1/120 notch units.
    static func wheel(dy: Int16, dx: Int16) -> Data {
        var d = Data([opWheel])
        be16(UInt16(bitPattern: dy), into: &d)
        be16(UInt16(bitPattern: dx), into: &d)
        return d
    }

    /// KEY_EX (6 bytes).
    static func keyEx(_ k: KeyStroke, down: Bool) -> Data {
        var d = Data([opKeyEx])
        be16(k.vk, into: &d)
        be16(k.scan, into: &d)
        var flags: UInt8 = down ? keyFlagDown : 0
        if k.extended { flags |= keyFlagExtended }
        d.append(flags)
        return d
    }

    /// TEXT records: the UTF-8 of `text` split into chunks of at most
    /// `maxTextChunk` bytes, never inside a multi-byte sequence.
    static func text(_ text: String) -> [Data] {
        let bytes = Array(text.utf8)
        var out: [Data] = []
        var start = 0
        while start < bytes.count {
            var end = min(start + maxTextChunk, bytes.count)
            // Back off while the cut would land on a continuation byte.
            while end < bytes.count && end > start + 1 && (bytes[end] & 0xC0) == 0x80 {
                end -= 1
            }
            var d = Data([opText])
            d.append(contentsOf: bytes[start..<end])
            out.append(d)
            start = end
        }
        return out
    }

    static func releaseAll() -> Data { Data([opReleaseAll]) }
    static func sas() -> Data { Data([opSAS]) }

    /// A frame pixel coordinate -> 0...65535 (px * 65535 / (extent - 1)),
    /// clamped to the frame.
    static func normalize(_ v: CGFloat, extent: CGFloat) -> UInt32 {
        guard extent > 1, v.isFinite else { return 0 }
        let c = min(max(v, 0), extent - 1)
        let n = (c * 65_535 / (extent - 1)).rounded()
        return UInt32(min(max(n, 0), 65_535))
    }

    /// Clamp a Double to the Int32 range (used for relative deltas).
    static func clampI32(_ v: Double) -> Int32 {
        guard v.isFinite else { return 0 }
        return Int32(min(max(v, Double(Int32.min)), Double(Int32.max)))
    }
}

// MARK: - Display-rate ticker

/// A CADisplayLink wrapper with a closure. The link retains only a small
/// proxy (not the owner), so owners can deinit; `stop()` / deinit
/// invalidate the link.
final class FrameTicker {
    private final class Proxy: NSObject {
        var handler: ((CFTimeInterval) -> Void)?
        @objc func tick(_ link: CADisplayLink) {
            handler?(link.timestamp)
        }
    }

    private let proxy = Proxy()
    private var link: CADisplayLink?

    init(_ handler: @escaping (CFTimeInterval) -> Void) {
        proxy.handler = handler
    }

    deinit {
        link?.invalidate()
    }

    var isRunning: Bool { link != nil }

    func start() {
        guard link == nil else { return }
        let l = CADisplayLink(target: proxy, selector: #selector(Proxy.tick(_:)))
        // ProMotion devices may go up to 120 Hz (iOS 15+ API; the target is
        // 15.6 so no availability check is needed).
        l.preferredFrameRateRange = CAFrameRateRange(minimum: 30, maximum: 120, preferred: 120)
        l.add(to: .main, forMode: .common)
        link = l
    }

    func stop() {
        link?.invalidate()
        link = nil
    }
}

// MARK: - Controller

/// Turns input intents into channel-0 commands. Main thread only.
///
/// Pointer moves are coalesced: callers update the predicted cursor and the
/// pending delta; the first one after a display tick goes out at once, the
/// rest of that frame is flushed by a display-rate ticker (and any
/// button / key / wheel command flushes pending moves first, so ordering is
/// preserved). Every button or key we press on the host is tracked, so
/// `releaseAll()` can always undo it.
final class RemoteInputController: ObservableObject {
    static let cmdAsCtrlDefaultsKey = "ov.cmdAsCtrl"
    static let pointerLockDefaultsKey = "ov.pointerLock"
    /// "Lock local zoom": every gesture, pinch included, goes to the host.
    static let lockLocalZoomDefaultsKey = "ov.lockLocalZoom"
    /// Keys of control settings removed in favour of the fixed TeamViewer
    /// behaviour (cursor speed / acceleration / fling, immediate clicks,
    /// scroll inertia, classic touch gestures, viewport follow, the removed
    /// Mouse / Touch input-mode switch) and of their old migration flags;
    /// `migrateDefaults` deletes them.
    static let obsoleteDefaultsKeys = [
        "ov.cursorSpeed", "ov.cursorAccel", "ov.cursorFling", "ov.immediateClicks",
        "ov.scrollInertia", "ov.classicTouchGestures", "ov.viewportFollow",
        "ov.cursorSpeedTVMigrated", "ov.tvParityDefaultsV42",
        "ov.inputMode",
    ]

    /// Sends one channel-0 command (SessionController.sendCommand).
    var send: ((Data) -> Void)?
    /// The host understands the CAP_INPUT2 commands (set from the session
    /// queue through a main hop).
    var input2 = false

    /// Sticky modifier states, indexed by `ModifierKey.rawValue`.
    @Published private(set) var modifierStates: [ModifierState] = [.off, .off, .off, .off]

    /// Show (soft = true: system keyboard + accessory bar; false: the bar
    /// alone) or hide (nil) the remote keyboard. Installed by the surface.
    var keyboardPresenter: ((_ soft: Bool?) -> Void)?
    /// Called after `releaseAll()` (the hardware-key router clears its state).
    var onReleaseAll: (() -> Void)?
    /// Called when the cursor was resynced to the host position.
    var onCursorResync: (() -> Void)?
    /// Applies a local zoom preset (installed by the surface).
    var zoomPresetHandler: ((ZoomPreset) -> Void)?
    /// The surface that installed the hooks above (a re-hosted surface
    /// takes over; a dismantled one only unhooks if it is still active).
    weak var activeSurface: AnyObject?
    /// Frames (window coordinates) of the tappable session chrome laid over
    /// the remote screen (toolbar / handle, stats chip). The surface never
    /// takes touches inside them, so they always reach the chrome.
    var chromeExclusionRects: [CGRect] = []
    /// Where the remote image sits at fit zoom (window coordinates; `.null`
    /// until the surface has laid out a frame). Published by the surface
    /// when it refits; the floating toolbar docks below it when there is
    /// room (portrait).
    @Published var fitImageFrame: CGRect = .null

    // ---- cursor ----
    /// Frame size the cursor and normalized coordinates refer to.
    var frameSize: CGSize = .zero {
        didSet {
            guard frameSize != oldValue, frameSize.width > 0, frameSize.height > 0 else { return }
            if !hasCursor {
                pct = CGPoint(x: 0.5, y: 0.5) // TeamViewer: start at the centre
                hasCursor = true
            }
            // Keep the normalised position (TeamViewer's state) across
            // frame-size changes; the host's next CURSOR resyncs it.
            cursor = Self.pixel(fromPct: pct, frame: frameSize)
        }
    }
    /// TeamViewer's cursor state: the normalised position in [0, 1]^2
    /// (trackpad moves update this; `cursor` follows).
    private(set) var pct = CGPoint(x: 0.5, y: 0.5)
    /// Predicted cursor position in frame pixels (drawn locally; the wire
    /// position is its whole pixel). Kept in step with `pct`.
    private(set) var cursor: CGPoint = .zero
    private(set) var hasCursor = false
    /// Fingers on the surface: host CURSOR echoes are ignored meanwhile.
    var touching = false {
        didSet {
            guard touching != oldValue else { return }
            lastLocalInput = CACurrentMediaTime()
            if !touching { scheduleResync() }
        }
    }
    private var lastLocalInput: CFTimeInterval = 0
    /// Newest host cursor position (also the ones we ignored).
    private var latestHostCursor: CGPoint?
    private var resyncScheduled = false
    private static let echoSuppression: CFTimeInterval = 0.3

    // ---- pending pointer / wheel ----
    private var pendingAbs = false
    private var pendingRel = CGPoint.zero
    private var pendingRaw = CGPoint.zero
    private var relRemainder = CGPoint.zero
    private var rawRemainder = CGPoint.zero
    private var wheelAcc = CGPoint.zero
    private var idleTicks = 0
    /// Pointer / wheel output already went out since the last display
    /// tick (later motion in the same frame is coalesced into the tick).
    private var sentSinceTick = false
    private lazy var ticker = FrameTicker { [weak self] _ in self?.tick() }

    // ---- what we hold on the host ----
    private var pressedButtons = Set<MouseButton>()
    private var pressedKeys = Set<KeyStroke>()
    /// Sticky modifiers pressed for a held button / hardware key.
    private var stickiesForButton: [MouseButton: [ModifierKey]] = [:]
    private var stickiesForKey: [KeyStroke: [ModifierKey]] = [:]
    private var lastModifierTap: [CFTimeInterval] = [0, 0, 0, 0]
    private static let modifierDoubleTap: CFTimeInterval = 0.35

    init() {
        RemoteInputController.migrateDefaults()
    }

    deinit {
        ticker.stop()
    }

    // MARK: Settings

    /// Cmd+C/V/X/... from a hardware keyboard act as Ctrl+... (default on).
    var cmdAsCtrl: Bool {
        UserDefaults.standard.object(forKey: Self.cmdAsCtrlDefaultsKey) as? Bool ?? true
    }

    /// iPad pointer lock + raw GCMouse deltas (default off).
    var pointerLockEnabled: Bool {
        UserDefaults.standard.bool(forKey: Self.pointerLockDefaultsKey)
    }

    /// Settings cleanup (idempotent; called at app start and by every
    /// controller): drop the stored values of the removed control settings.
    /// Removing an absent key is a no-op, so this needs no flag of its own.
    static func migrateDefaults() {
        let d = UserDefaults.standard
        for key in obsoleteDefaultsKeys where d.object(forKey: key) != nil {
            d.removeObject(forKey: key)
        }
    }

    /// Local zoom locked: pinch becomes Ctrl+wheel on the host (default off).
    var lockLocalZoom: Bool {
        UserDefaults.standard.bool(forKey: Self.lockLocalZoomDefaultsKey)
    }

    /// Ask the surface to apply a zoom preset (session menu).
    func applyZoomPreset(_ preset: ZoomPreset) {
        zoomPresetHandler?(preset)
    }

    /// Show / toggle the remote keyboard (see `keyboardPresenter`).
    func showKeyboard(soft: Bool) {
        keyboardPresenter?(soft)
    }

    func hideKeyboard() {
        keyboardPresenter?(nil)
    }

    /// Stop the ticker and drop the send path (session teardown).
    func shutdown() {
        ticker.stop()
        send = nil
        keyboardPresenter = nil
        onReleaseAll = nil
        onCursorResync = nil
        zoomPresetHandler = nil
    }

    // MARK: Cursor

    private func clampToFrame(_ p: CGPoint) -> CGPoint {
        guard frameSize.width > 0, frameSize.height > 0 else { return p }
        let x = p.x.isFinite ? p.x : 0
        let y = p.y.isFinite ? p.y : 0
        return CGPoint(x: min(max(x, 0), frameSize.width - 1),
                       y: min(max(y, 0), frameSize.height - 1))
    }

    /// TeamViewer's pct -> frame pixel: 1 + pct * (R - 2) per axis (its
    /// wire position is the truncation of this), clamped to the frame.
    static func pixel(fromPct q: CGPoint, frame r: CGSize) -> CGPoint {
        func axis(_ t: CGFloat, _ extent: CGFloat) -> CGFloat {
            let last = max(extent - 1, 0)
            let v = extent > 2 ? 1 + t * (extent - 2) : t * last
            return v.isFinite ? min(max(v, 0), last) : last / 2
        }
        return CGPoint(x: axis(q.x, r.width), y: axis(q.y, r.height))
    }

    /// Inverse of `pixel(fromPct:)`, clamped to [0, 1].
    static func normalised(fromPixel p: CGPoint, frame r: CGSize) -> CGPoint {
        func axis(_ v: CGFloat, _ extent: CGFloat) -> CGFloat {
            let t: CGFloat
            if extent > 2 {
                t = (v - 1) / (extent - 2)
            } else if extent > 1 {
                t = v / (extent - 1)
            } else {
                t = 0.5
            }
            return t.isFinite ? min(max(t, 0), 1) : 0.5
        }
        return CGPoint(x: axis(p.x, r.width), y: axis(p.y, r.height))
    }

    /// Set the predicted cursor to frame pixel `p` (clamped) and derive
    /// the normalised position from it.
    private func setCursorPixel(_ p: CGPoint) {
        cursor = clampToFrame(p)
        pct = Self.normalised(fromPixel: cursor, frame: frameSize)
        hasCursor = true
    }

    func noteLocalInput() {
        lastLocalInput = CACurrentMediaTime()
    }

    /// A host CURSOR update. Ignored while local input is in flight (the
    /// prediction is ahead of the echo); returns whether it was applied.
    @discardableResult
    func hostCursorMoved(to p: CGPoint) -> Bool {
        latestHostCursor = p
        guard frameSize.width > 0 else { return false }
        if touching || !pressedButtons.isEmpty || hasPendingPointer
            || CACurrentMediaTime() - lastLocalInput < Self.echoSuppression {
            scheduleResync()
            return false
        }
        // The host reports whole pixels: keep the sub-pixel prediction
        // (TeamViewer's pct) when it is in that pixel already.
        if isOnWirePixel(p) { return false }
        setCursorPixel(p)
        return true
    }

    /// The predicted cursor's wire pixel (truncated, as sent) is `p`.
    private func isOnWirePixel(_ p: CGPoint) -> Bool {
        let q = clampToFrame(p)
        return cursor.x.rounded(.towardZero) == q.x.rounded(.towardZero)
            && cursor.y.rounded(.towardZero) == q.y.rounded(.towardZero)
    }

    private var hasPendingPointer: Bool {
        pendingAbs || pendingRel != .zero || pendingRaw != .zero
    }

    /// After local input settles, snap to the newest host position.
    private func scheduleResync() {
        guard !resyncScheduled else { return }
        resyncScheduled = true
        DispatchQueue.main.asyncAfter(deadline: .now() + Self.echoSuppression + 0.05) { [weak self] in
            guard let self else { return }
            self.resyncScheduled = false
            guard let host = self.latestHostCursor, self.frameSize.width > 0 else { return }
            if self.touching || !self.pressedButtons.isEmpty || self.hasPendingPointer
                || CACurrentMediaTime() - self.lastLocalInput < Self.echoSuppression {
                self.scheduleResync()
                return
            }
            let p = self.clampToFrame(host)
            if !self.isOnWirePixel(p) {
                self.setCursorPixel(p)
                self.onCursorResync?()
            }
        }
    }

    /// Trackpad move by `d` frame pixels (exact delta; RAW = raw mouse
    /// motion subject to host acceleration, for pointer-lock deltas only).
    /// The local cursor clamps to the frame; the unclamped delta goes to the
    /// host, which clamps itself (pushing into an edge resyncs both).
    /// The finger trackpad uses `moveCursorTrackpad` (absolute, like
    /// TeamViewer); this path remains for pointer-lock RAW deltas.
    func moveCursorRelative(by d: CGPoint, raw: Bool = false) {
        guard d.x.isFinite, d.y.isFinite, frameSize.width > 0 else { return }
        setCursorPixel(CGPoint(x: cursor.x + d.x, y: cursor.y + d.y))
        if raw {
            pendingRaw.x += d.x
            pendingRaw.y += d.y
        } else {
            pendingRel.x += d.x
            pendingRel.y += d.y
        }
        noteLocalInput()
        ensureTicking()
    }

    /// Absolute move to frame pixel `p` (iPad hover / pointer clicks, tap
    /// pixel restore). Replaces any pending relative motion.
    func moveCursorAbsolute(to p: CGPoint) {
        guard frameSize.width > 0 else { return }
        setCursorPixel(p)
        queueAbsoluteMove()
    }

    /// TeamViewer's trackpad model. `d` = finger travel in
    /// screen points (1:1, no speed factor or acceleration); `displayed` = the
    /// remote image's on-screen size in points (frame pixels x zoom, i.e.
    /// TeamViewer's R * z). pct += d / max(displayed, 1) per axis, clamped
    /// to [0, 1] (no wrap); the host gets the ABSOLUTE position
    /// trunc(1 + pct * (R - 2)), so it can never drift from the
    /// prediction. Sliding across the displayed image's width crosses the
    /// whole remote width; zoomed in by z, 1 pt = 1/z remote pixels.
    func moveCursorTrackpad(byPoints d: CGPoint, displayed: CGSize) {
        guard d.x.isFinite, d.y.isFinite, frameSize.width > 0, frameSize.height > 0,
              displayed.width.isFinite, displayed.height.isFinite else { return }
        let w = max(displayed.width, 1)
        let h = max(displayed.height, 1)
        pct = CGPoint(x: min(max(pct.x + d.x / w, 0), 1),
                      y: min(max(pct.y + d.y / h, 0), 1))
        cursor = Self.pixel(fromPct: pct, frame: frameSize)
        hasCursor = true
        queueAbsoluteMove()
    }

    /// Send the predicted cursor as an absolute move at the next flush
    /// (replaces any pending relative motion).
    private func queueAbsoluteMove() {
        pendingAbs = true
        pendingRel = .zero
        pendingRaw = .zero
        relRemainder = .zero
        rawRemainder = .zero
        noteLocalInput()
        ensureTicking()
    }

    /// Pending pointer / wheel output: leading edge — when nothing went
    /// out since the last display tick it is sent at once (the first move
    /// after idle never waits for the ticker to start or for the next
    /// frame); motion that follows within the same frame is coalesced and
    /// flushed by the tick (at most ~2 sends per display frame).
    private func ensureTicking() {
        idleTicks = 0
        if !sentSinceTick {
            sentSinceTick = true
            flushPointer()
            flushWheel()
        }
        if !ticker.isRunning { ticker.start() }
    }

    private func tick() {
        let busy = hasPendingPointer || abs(wheelAcc.x) >= 1 || abs(wheelAcc.y) >= 1
        flushPointer()
        flushWheel()
        sentSinceTick = false
        if busy {
            idleTicks = 0
        } else {
            idleTicks += 1
            if idleTicks > 30 { ticker.stop() } // ~0.5 s idle
        }
    }

    /// Send the coalesced pointer motion now.
    func flushPointer() {
        guard frameSize.width > 0 else {
            pendingAbs = false
            pendingRel = .zero
            pendingRaw = .zero
            return
        }
        if pendingAbs {
            pendingAbs = false
            pendingRel = .zero
            pendingRaw = .zero
            sendAbsoluteMove(cursor)
            return
        }
        if pendingRel != .zero {
            let d = pendingRel
            pendingRel = .zero
            if input2 {
                sendRelative(d, remainder: &relRemainder, flags: 0)
            } else {
                sendAbsoluteMove(cursor)
            }
        }
        if pendingRaw != .zero {
            let d = pendingRaw
            pendingRaw = .zero
            if input2 {
                sendRelative(d, remainder: &rawRemainder, flags: InputWire.flagRaw)
            } else {
                sendAbsoluteMove(cursor)
            }
        }
    }

    /// One move-rel with whole pixels; the sub-pixel rest carries over.
    private func sendRelative(_ d: CGPoint, remainder: inout CGPoint, flags: UInt8) {
        let tx = Double(d.x + remainder.x)
        let ty = Double(d.y + remainder.y)
        let ix = tx.rounded()
        let iy = ty.rounded()
        remainder = CGPoint(x: tx - ix, y: ty - iy)
        guard ix != 0 || iy != 0 else { return }
        emit(InputWire.pointerEx(action: InputWire.actionMoveRel, button: 0, flags: flags,
                                 x: InputWire.clampI32(ix), y: InputWire.clampI32(iy)))
    }

    private func sendAbsoluteMove(_ p: CGPoint) {
        let x = InputWire.normalize(p.x.rounded(.towardZero), extent: frameSize.width)
        let y = InputWire.normalize(p.y.rounded(.towardZero), extent: frameSize.height)
        if input2 {
            emit(InputWire.pointerEx(action: InputWire.actionMoveAbs, button: 0, flags: 0,
                                     x: Int32(x), y: Int32(y)))
        } else {
            emit(InputWire.pointer(kind: 0, x: x, y: y))
        }
    }

    // MARK: Buttons

    /// Press `button`, at frame pixel `p` (moving there first) or at the
    /// host's current cursor when nil. Latched / locked sticky modifiers
    /// are pressed around the button (released with its up). Without
    /// CAP_INPUT2 only the left button exists.
    func buttonDown(_ button: MouseButton, at p: CGPoint? = nil) {
        guard frameSize.width > 0 else { return }
        flushPointer()
        if let p {
            setCursorPixel(p)
        }
        noteLocalInput()
        guard input2 || button == .left else { return }
        guard !pressedButtons.contains(button) else { return }
        stickiesForButton[button] = pressStickies()
        pressedButtons.insert(button)
        emitButton(button, down: true, withPos: p != nil)
    }

    /// Release `button` (no-op unless we pressed it: every DOWN gets
    /// exactly one UP).
    func buttonUp(_ button: MouseButton, at p: CGPoint? = nil) {
        guard pressedButtons.contains(button) else { return }
        flushPointer()
        if let p {
            setCursorPixel(p)
        }
        noteLocalInput()
        pressedButtons.remove(button)
        emitButton(button, down: false, withPos: p != nil)
        if let mods = stickiesForButton.removeValue(forKey: button) {
            releaseStickies(mods)
        }
    }

    /// Down + up.
    func click(_ button: MouseButton, at p: CGPoint? = nil) {
        buttonDown(button, at: p)
        buttonUp(button, at: p)
    }

    func isPressed(_ button: MouseButton) -> Bool {
        pressedButtons.contains(button)
    }

    private func emitButton(_ button: MouseButton, down: Bool, withPos: Bool) {
        if input2 {
            var x: Int32 = 0
            var y: Int32 = 0
            if withPos {
                x = Int32(InputWire.normalize(cursor.x.rounded(.towardZero), extent: frameSize.width))
                y = Int32(InputWire.normalize(cursor.y.rounded(.towardZero), extent: frameSize.height))
            }
            emit(InputWire.pointerEx(action: down ? InputWire.actionDown : InputWire.actionUp,
                                     button: button.rawValue,
                                     flags: withPos ? InputWire.flagHasPos : 0,
                                     x: x, y: y))
        } else {
            // Legacy POINTER always carries a position: the tracked cursor.
            emit(InputWire.pointer(kind: down ? 1 : 2,
                                   x: InputWire.normalize(cursor.x.rounded(.towardZero), extent: frameSize.width),
                                   y: InputWire.normalize(cursor.y.rounded(.towardZero), extent: frameSize.height)))
        }
    }

    // MARK: Wheel

    /// Scroll by (dx, dy) in 1/120-notch units (dy > 0 = up, dx > 0 =
    /// right). Accumulated; whole units go out once per display frame.
    /// Dropped without CAP_INPUT2 (the legacy protocol has no wheel).
    func wheel(dx: Double, dy: Double) {
        guard input2, dx.isFinite, dy.isFinite else { return }
        wheelAcc.x += CGFloat(dx)
        wheelAcc.y += CGFloat(dy)
        noteLocalInput()
        ensureTicking()
    }

    /// Send whole accumulated wheel units now (also used before releasing
    /// the Ctrl of a pinch-to-Ctrl+wheel gesture, so ordering holds).
    func flushWheel() {
        guard input2 else {
            wheelAcc = .zero
            return
        }
        let wx = Double(wheelAcc.x).rounded(.towardZero)
        let wy = Double(wheelAcc.y).rounded(.towardZero)
        guard wx != 0 || wy != 0 else { return }
        let sx = Int16(min(max(wx, Double(Int16.min)), Double(Int16.max)))
        let sy = Int16(min(max(wy, Double(Int16.min)), Double(Int16.max)))
        wheelAcc.x -= CGFloat(sx)
        wheelAcc.y -= CGFloat(sy)
        flushPointer()
        emit(InputWire.wheel(dy: sy, dx: sx))
    }

    // MARK: Keys

    /// One raw key transition (tracked, so releaseAll can undo it). An up
    /// for a key we never pressed is dropped.
    func sendKey(_ k: KeyStroke, down: Bool) {
        if down {
            pressedKeys.insert(k)
        } else {
            guard pressedKeys.remove(k) != nil else { return }
        }
        flushPointer()
        if input2 {
            emit(InputWire.keyEx(k, down: down))
        } else {
            emit(InputWire.key(vk: k.legacyVK, down: down))
        }
    }

    /// Down + up, with the sticky modifiers around it (then latched ones
    /// turn off).
    func tapKey(_ k: KeyStroke) {
        let mods = pressStickies()
        sendKey(k, down: true)
        sendKey(k, down: false)
        releaseStickies(mods)
    }

    /// A hardware-keyboard transition of a non-modifier key: sticky
    /// modifiers wrap it (pressed before the down, released after the up).
    func hardwareKey(_ k: KeyStroke, down: Bool) {
        if down {
            if pressedKeys.contains(k) { return }
            stickiesForKey[k] = pressStickies()
            sendKey(k, down: true)
        } else {
            sendKey(k, down: false)
            if let mods = stickiesForKey.removeValue(forKey: k) {
                releaseStickies(mods)
            }
        }
    }

    /// Soft-keyboard text. "\n" is Enter. With a non-shift sticky modifier
    /// active, a single ASCII letter/digit goes out as that key with the
    /// modifiers (Ctrl+C works); everything else is TEXT (host Unicode
    /// injection) — or, for old hosts, best-effort per-character KEYs.
    func typeText(_ text: String) {
        guard !text.isEmpty else { return }
        if text == "\n" || text == "\r" || text == "\r\n" {
            tapKey(.enter)
            return
        }
        let scalars = Array(text.unicodeScalars)
        let nonShift = ModifierKey.allCases.contains { $0 != .shift && state(of: $0) != .off }
        if nonShift, scalars.count == 1, let k = KeyStroke.ascii(scalars[0]) {
            tapKey(k)
            return
        }
        flushPointer()
        if input2 {
            for record in InputWire.text(text) {
                emit(record)
            }
        } else {
            typeLegacy(scalars)
        }
        consumeLatched()
    }

    /// Old hosts: letters, digits, space, tab and newline as KEY taps.
    private func typeLegacy(_ scalars: [Unicode.Scalar]) {
        for s in scalars {
            let v = s.value
            var vk: UInt16 = 0
            var shifted = false
            if let k = KeyStroke.ascii(s) {
                vk = k.vk
                shifted = v >= 0x41 && v <= 0x5A
            } else if v == 0x20 {
                vk = 0x20
            } else if v == 0x0A || v == 0x0D {
                vk = 0x0D
            } else if v == 0x09 {
                vk = 0x09
            } else {
                continue
            }
            if shifted { emit(InputWire.key(vk: 0x10, down: true)) }
            emit(InputWire.key(vk: vk, down: true))
            emit(InputWire.key(vk: vk, down: false))
            if shifted { emit(InputWire.key(vk: 0x10, down: false)) }
        }
    }

    /// A key chord (menu shortcuts): every key down in order, then up in
    /// reverse order. Sticky modifiers are not applied.
    func sendCombo(_ keys: [KeyStroke]) {
        guard !keys.isEmpty else { return }
        for k in keys {
            sendKey(k, down: true)
        }
        for k in keys.reversed() {
            sendKey(k, down: false)
        }
    }

    /// Ctrl+Alt+Del (secure attention).
    func sendSAS() {
        flushPointer()
        if input2 {
            emit(InputWire.sas())
        } else {
            let keys: [UInt16] = [0x11, 0x12, 0x2E]
            for vk in keys { emit(InputWire.key(vk: vk, down: true)) }
            for vk in keys.reversed() { emit(InputWire.key(vk: vk, down: false)) }
        }
        consumeLatched()
    }

    /// Release every key / button we hold on the host and clear the sticky
    /// modifiers (backgrounding, disconnect).
    func releaseAll() {
        flushPointer()
        wheelAcc = .zero
        if input2 {
            emit(InputWire.releaseAll())
        } else {
            if pressedButtons.contains(.left) {
                emit(InputWire.pointer(kind: 2,
                                       x: InputWire.normalize(cursor.x.rounded(.towardZero), extent: frameSize.width),
                                       y: InputWire.normalize(cursor.y.rounded(.towardZero), extent: frameSize.height)))
            }
            for k in pressedKeys {
                emit(InputWire.key(vk: k.legacyVK, down: false))
            }
        }
        pressedButtons.removeAll()
        pressedKeys.removeAll()
        stickiesForButton.removeAll()
        stickiesForKey.removeAll()
        if modifierStates.contains(where: { $0 != .off }) {
            modifierStates = [.off, .off, .off, .off]
        }
        onReleaseAll?()
    }

    // MARK: Sticky modifiers

    func state(of m: ModifierKey) -> ModifierState {
        modifierStates[m.rawValue]
    }

    func setModifier(_ m: ModifierKey, _ s: ModifierState) {
        guard modifierStates[m.rawValue] != s else { return }
        modifierStates[m.rawValue] = s
    }

    /// Accessory-bar tap: off -> latched; latched -> locked when tapped
    /// again within 0.35 s, else off; locked -> off.
    /// Win is different (like TeamViewer's Win button): a tap sends the
    /// Windows key on its own (opens Start); a second tap within 0.35 s
    /// locks Win held for combos (Win+R, Win+E...); a tap releases it.
    func tapModifier(_ m: ModifierKey) {
        let now = CACurrentMediaTime()
        let last = lastModifierTap[m.rawValue]
        lastModifierTap[m.rawValue] = now
        if m == .win {
            switch state(of: m) {
            case .off:
                if now - last < Self.modifierDoubleTap {
                    setModifier(m, .locked)
                } else {
                    sendKey(m.stroke, down: true)
                    sendKey(m.stroke, down: false)
                }
            case .latched, .locked:
                setModifier(m, .off)
            }
            return
        }
        switch state(of: m) {
        case .off:
            setModifier(m, .latched)
        case .latched:
            setModifier(m, now - last < Self.modifierDoubleTap ? .locked : .off)
        case .locked:
            setModifier(m, .off)
        }
    }

    /// Press every active sticky modifier; returns them for the release.
    private func pressStickies() -> [ModifierKey] {
        let active = ModifierKey.allCases.filter { state(of: $0) != .off }
        for m in active {
            sendKey(m.stroke, down: true)
        }
        return active
    }

    /// Release what `pressStickies` pressed, then latched ones turn off.
    private func releaseStickies(_ mods: [ModifierKey]) {
        for m in mods.reversed() {
            sendKey(m.stroke, down: false)
        }
        consumeLatched()
    }

    private func consumeLatched() {
        guard modifierStates.contains(.latched) else { return }
        modifierStates = modifierStates.map { $0 == .latched ? .off : $0 }
    }

    // MARK: Output

    private func emit(_ d: Data) {
        send?(d)
    }
}
