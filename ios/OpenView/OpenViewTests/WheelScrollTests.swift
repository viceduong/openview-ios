import CoreGraphics
import XCTest

final class WheelScrollTests: XCTestCase {
    // MARK: Direction and resolution

    func testNaturalDirectionAndTouchResolution() {
        // Fingers down 10 pt = content down = one notch up.
        let down = WheelScroll.units(forTravel: CGPoint(x: 0, y: 10),
                                     unitsPerPoint: WheelScroll.touchUnitsPerPoint)
        XCTAssertEqual(down.dy, 120, accuracy: 1e-9)
        XCTAssertEqual(down.dx, 0, accuracy: 1e-9)
        // Fingers right = scroll left (negative HWHEEL).
        let right = WheelScroll.units(forTravel: CGPoint(x: 1, y: 0),
                                      unitsPerPoint: WheelScroll.touchUnitsPerPoint)
        XCTAssertEqual(right.dx, -12, accuracy: 1e-9)
        // Trackpad: a notch per 36 pt.
        let pad = WheelScroll.units(forTravel: CGPoint(x: 0, y: -36),
                                    unitsPerPoint: WheelScroll.trackpadUnitsPerPoint)
        XCTAssertEqual(pad.dy, -120, accuracy: 1e-9)
        // Non-finite travel is ignored.
        let bad = WheelScroll.units(forTravel: CGPoint(x: CGFloat.nan, y: 1),
                                    unitsPerPoint: WheelScroll.touchUnitsPerPoint)
        XCTAssertEqual(bad.dx, 0)
        XCTAssertEqual(bad.dy, 0)
    }

    // MARK: Accumulator

    func testFractionsCarryAndNothingIsLost() {
        var acc = WheelAccumulator(minInterval: 0)
        var sent = 0
        var now: CFTimeInterval = 0
        // 100 slow moves of 0.25 units each = 25 units in total.
        for _ in 0..<100 {
            acc.add(dx: 0, dy: 0.25)
            now += 0.001
            if let w = acc.take(now: now) {
                XCTAssertEqual(w.dx, 0)
                sent += Int(w.dy)
            }
        }
        XCTAssertEqual(sent, 25)
        XCTAssertEqual(acc.dy, 0, accuracy: 1e-9)
        XCTAssertFalse(acc.hasWholeUnits)
        XCTAssertNil(acc.take(now: now + 1), "fractions alone never go out")
    }

    func testReleasesAreRateLimitedUnlessForced() {
        var acc = WheelAccumulator(minInterval: 0.007)
        acc.add(dx: 0, dy: 24)
        XCTAssertEqual(acc.take(now: 1.000)?.dy, 24)
        acc.add(dx: 0, dy: 12)
        XCTAssertNil(acc.take(now: 1.003), "within 7 ms: wait for the next frame")
        XCTAssertTrue(acc.hasWholeUnits, "kept, not dropped")
        acc.add(dx: -5, dy: 12)
        let w = acc.take(now: 1.008)
        XCTAssertEqual(w?.dy, 24, "coalesced")
        XCTAssertEqual(w?.dx, -5)
        acc.add(dx: 0, dy: 3)
        XCTAssertEqual(acc.take(now: 1.009, force: true)?.dy, 3, "forced flush ignores pacing")
        // ~120 Hz: a 120 Hz display tick (8.3 ms) is never skipped.
        acc.add(dx: 0, dy: 1)
        XCTAssertNotNil(acc.take(now: 1.009 + 1.0 / 120.0))
    }

    func testLargeDeltasClampToInt16AndCarry() {
        var acc = WheelAccumulator(minInterval: 0)
        acc.add(dx: 0, dy: 40_000)
        XCTAssertEqual(acc.take(now: 0)?.dy, Int16.max)
        XCTAssertEqual(acc.take(now: 1)?.dy, Int16(40_000 - Int(Int16.max)))
        acc.add(dx: -1.9, dy: .infinity)
        XCTAssertEqual(acc.dy, 0, "non-finite input ignored")
        acc.add(dx: -1.9, dy: 0)
        XCTAssertEqual(acc.take(now: 2)?.dx, -1, "toward zero")
        acc.reset()
        XCTAssertEqual(acc.dx, 0)
        XCTAssertNil(acc.take(now: 3))
    }

    // MARK: Fling (inertia)

    func testFlingDecaysExponentiallyAndCoversTheModelDistance() {
        var f = FlingTracker(timeConstant: 250)
        // 1000 pt/s downwards = 1 pt/ms.
        f.start(velocity: CGPoint(x: 0, y: 1000), now: 10)
        XCTAssertTrue(f.isActive)
        var total: CGFloat = 0
        var steps: [CGFloat] = []
        var t: CFTimeInterval = 10
        while let d = f.step(now: t + 1.0 / 120.0) {
            t += 1.0 / 120.0
            XCTAssertEqual(d.x, 0, accuracy: 1e-9)
            total += d.y
            steps.append(d.y)
            XCTAssertLessThan(t, 12, "the glide ends")
        }
        XCTAssertFalse(f.isActive)
        // x(T_end) = T·v0·(1 − e^(−dur/T)) with dur = −T·ln(0.01/v0): 247.5 pt.
        XCTAssertEqual(Double(total), 247.5, accuracy: 3)
        // Each step is smaller than the one before (exponential decay).
        for (a, b) in zip(steps, steps.dropFirst()) {
            XCTAssertLessThanOrEqual(b, a + 1e-9)
        }
    }

    func testFlingStopsOnTouchDownAndIgnoresSlowLifts() {
        var f = FlingTracker(timeConstant: 250)
        f.start(velocity: CGPoint(x: 800, y: -600), now: 0)
        XCTAssertNotNil(f.step(now: 0.016))
        f.stop() // a finger landed
        XCTAssertFalse(f.isActive)
        XCTAssertNil(f.step(now: 0.032))
        // A slow lift (total glide under 20 pt) does not fling at all.
        f.start(velocity: CGPoint(x: 0, y: 50), now: 1)
        XCTAssertFalse(f.isActive)
        // Non-finite velocity is ignored.
        f.start(velocity: CGPoint(x: CGFloat.infinity, y: 0), now: 2)
        XCTAssertFalse(f.isActive)
    }

    func testTouchFlingInWheelUnits() {
        // A 2000 pt/s flick glides ~495 pt = ~5940 units (~50 notches),
        // in the direction of the pan (fingers up = scroll down).
        var f = FlingTracker(timeConstant: 250)
        f.start(velocity: CGPoint(x: 0, y: -2000), now: 0)
        var acc = WheelAccumulator(minInterval: 0)
        var sent = 0
        var t: CFTimeInterval = 0
        while let d = f.step(now: t + 1.0 / 60.0) {
            t += 1.0 / 60.0
            let w = WheelScroll.units(forTravel: d, unitsPerPoint: WheelScroll.touchUnitsPerPoint)
            acc.add(dx: w.dx, dy: w.dy)
            if let out = acc.take(now: t) { sent += Int(out.dy) }
        }
        XCTAssertLessThan(sent, 0, "scrolls down")
        XCTAssertEqual(Double(-sent), 2000.0 / 1000.0 * 250.0 * 0.995 * 12.0, accuracy: 60)
    }
}
