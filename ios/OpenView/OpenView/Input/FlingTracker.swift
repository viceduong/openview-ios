import CoreGraphics
import Foundation
import QuartzCore

/// iPad trackpad scroll inertia (finger gestures have none).
/// Fling model: exponential decay x(t) = T·v0·(1 − e^(−t/T)), stops
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
