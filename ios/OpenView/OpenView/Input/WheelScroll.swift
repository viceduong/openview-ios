import CoreGraphics
import Foundation

/// Smooth (high-resolution) wheel scrolling: the pure math behind
/// two-finger touch scrolling, iPad trackpad scrolling and their fling.
///
/// Windows takes WHEEL / HWHEEL deltas below one notch (120 units): modern
/// apps scroll by the fraction, legacy ones accumulate to a notch, so the
/// shell streams finger travel continuously instead of in whole notches.
enum WheelScroll {
    /// Two-finger touch scrolling: wheel units (1/120 notch) per point of
    /// finger travel — a notch every 10 pt, sent continuously.
    static let touchUnitsPerPoint: CGFloat = 12
    /// iPad pointer (trackpad / mouse wheel) scrolling: one notch per 36 pt.
    static let trackpadUnitsPerPoint: CGFloat = 120.0 / 36.0
    /// Shortest gap between two WHEEL commands: about one per 120 Hz
    /// display frame (a little under 1/120 s, so a ProMotion tick that
    /// fires early is not skipped).
    static let minInterval: CFTimeInterval = 0.007

    /// Natural scrolling (content follows the fingers, as on iOS and in
    /// TeamViewer): fingers down (t.y > 0) = scroll up (dy > 0); fingers
    /// right (t.x > 0) = scroll left (dx < 0). Units are 1/120 notch.
    static func units(forTravel t: CGPoint, unitsPerPoint k: CGFloat) -> (dx: Double, dy: Double) {
        guard t.x.isFinite, t.y.isFinite, k.isFinite else { return (0, 0) }
        return (Double(-t.x * k), Double(t.y * k))
    }
}

/// Wheel units waiting to go out: fractions accumulate, whole units are
/// released at most once per `minInterval` (the remainder carries over, so
/// slow scrolling still adds up and nothing is lost to rounding).
struct WheelAccumulator {
    let minInterval: CFTimeInterval
    private(set) var dx: Double = 0
    private(set) var dy: Double = 0
    private var lastEmit: CFTimeInterval = -.infinity

    init(minInterval: CFTimeInterval = WheelScroll.minInterval) {
        self.minInterval = minInterval
    }

    /// Add `(dx, dy)` units (non-finite input is ignored).
    mutating func add(dx: Double, dy: Double) {
        guard dx.isFinite, dy.isFinite else { return }
        self.dx += dx
        self.dy += dy
    }

    /// At least one whole unit is waiting.
    var hasWholeUnits: Bool {
        abs(dx) >= 1 || abs(dy) >= 1
    }

    /// The whole units to send now (toward zero, clamped to Int16; the
    /// rest stays), or nil when there are none — or, unless `force`, when
    /// the previous release was less than `minInterval` before `now`.
    mutating func take(now: CFTimeInterval, force: Bool = false) -> (dx: Int16, dy: Int16)? {
        guard hasWholeUnits else { return nil }
        guard force || now - lastEmit >= minInterval else { return nil }
        let sx = Int16(min(max(dx.rounded(.towardZero), Double(Int16.min)), Double(Int16.max)))
        let sy = Int16(min(max(dy.rounded(.towardZero), Double(Int16.min)), Double(Int16.max)))
        dx -= Double(sx)
        dy -= Double(sy)
        lastEmit = now
        return (sx, sy)
    }

    /// Drop everything waiting (release-all, disconnect).
    mutating func reset() {
        dx = 0
        dy = 0
    }
}
