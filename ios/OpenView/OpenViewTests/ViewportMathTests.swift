import CoreGraphics
import XCTest

final class ViewportMathTests: XCTestCase {
    private let frame = CGSize(width: 1920, height: 1080)

    func testFitUsesTheTighterAxis() {
        // Portrait phone safe area: width-limited.
        let portrait = CGRect(x: 0, y: 47, width: 390, height: 763)
        XCTAssertEqual(ViewportMath.fitScale(frame: frame, safe: portrait), 390.0 / 1920.0, accuracy: 1e-9)
        // Landscape: height-limited.
        let landscape = CGRect(x: 47, y: 0, width: 750, height: 369)
        XCTAssertEqual(ViewportMath.fitScale(frame: frame, safe: landscape), 369.0 / 1080.0, accuracy: 1e-9)
        XCTAssertEqual(ViewportMath.fitScale(frame: .zero, safe: portrait), 0)
        XCTAssertEqual(ViewportMath.fitScale(frame: frame, safe: .zero), 0)
    }

    func testFillUsesTheLooserAxis() {
        let bounds = CGSize(width: 390, height: 844)
        XCTAssertEqual(ViewportMath.fillScale(frame: frame, bounds: bounds), 844.0 / 1080.0, accuracy: 1e-9)
        XCTAssertEqual(ViewportMath.fillScale(frame: frame, bounds: .zero), 0)
    }

    func testOpenDefaultIsTwoFifthsOfTheViewHeight() {
        let s = ViewportMath.openDefaultScale(frame: frame, viewHeight: 844)
        XCTAssertEqual(s * frame.height, 844 * 0.4, accuracy: 1e-9)
        XCTAssertEqual(ViewportMath.openDefaultScale(frame: frame, viewHeight: 0), 0)
    }

    func testOpenDefaultNeverGoesBelowFit() {
        // Landscape phone: 2/5 of the height is smaller than fit, so the
        // clamp keeps the whole screen visible.
        let fit = ViewportMath.fitScale(frame: frame, safe: CGRect(x: 47, y: 0, width: 750, height: 369))
        let open = ViewportMath.openDefaultScale(frame: frame, viewHeight: 390)
        XCTAssertLessThan(open, fit)
        XCTAssertEqual(ViewportMath.clamp(open, fit: fit), fit)
    }

    func testClampRange() {
        let fit: CGFloat = 0.2
        XCTAssertEqual(ViewportMath.clamp(0.1, fit: fit), 0.2)
        XCTAssertEqual(ViewportMath.clamp(1.5, fit: fit), 1.5)
        XCTAssertEqual(ViewportMath.clamp(9, fit: fit), ViewportMath.maxZoomScale)
        XCTAssertEqual(ViewportMath.clamp(.infinity, fit: fit), fit)
        XCTAssertEqual(ViewportMath.clamp(.nan, fit: fit), fit)
        // A frame smaller than the view: fit itself is above 2 pt/px.
        XCTAssertEqual(ViewportMath.maxScale(fit: 3), 3)
        XCTAssertEqual(ViewportMath.clamp(5, fit: 3), 3)
    }

    func testNativeScale() {
        XCTAssertEqual(ViewportMath.nativeScale(displayScale: 3), 1.0 / 3.0, accuracy: 1e-12)
        XCTAssertEqual(ViewportMath.nativeScale(displayScale: 0), 1)
    }

    func testSnapPicksTheNearestPresetWithinTolerance() {
        let candidates: [(String, CGFloat)] = [("fit", 0.2), ("fill", 0.78), ("native", 0.333)]
        XCTAssertEqual(ViewportMath.snap(scale: 0.21, candidates: candidates)?.preset, "fit")
        XCTAssertEqual(ViewportMath.snap(scale: 0.75, candidates: candidates)?.preset, "fill")
        XCTAssertEqual(ViewportMath.snap(scale: 0.75, candidates: candidates)?.scale, 0.78)
        // 0.5 is more than 20% away from every preset.
        XCTAssertNil(ViewportMath.snap(scale: 0.5, candidates: candidates))
        // Non-positive targets are ignored.
        XCTAssertNil(ViewportMath.snap(scale: 0.1, candidates: [("none", 0)]))
    }
}
