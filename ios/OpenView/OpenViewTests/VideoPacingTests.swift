import XCTest

final class VideoPacingTests: XCTestCase {
    // MARK: Newest-wins frame slot

    func testNothingIsAcceptedOutsideAMotionPeriod() {
        var p = VideoFramePacer<Int>()
        XCTAssertEqual(p.offer(1, generation: 0), .rejected)
        let t = p.take()
        XCTAssertNil(t.frame)
        XCTAssertFalse(t.active)
    }

    func testNewestWinsAndAtMostOneWaits() {
        var p = VideoFramePacer<Int>()
        let g = p.begin()
        XCTAssertEqual(p.offer(1, generation: g), .queued)
        XCTAssertEqual(p.offer(2, generation: g), .replaced)
        XCTAssertEqual(p.offer(3, generation: g), .replaced)
        XCTAssertEqual(p.dropped, 2)
        let t = p.take()
        XCTAssertEqual(t.frame, 3)
        XCTAssertTrue(t.active)
        XCTAssertEqual(t.generation, g)
        // The slot is empty after a take: the next tick gets nothing new,
        // and the next frame asks for a redraw again.
        XCTAssertNil(p.take().frame)
        XCTAssertFalse(p.hasPending)
        XCTAssertEqual(p.offer(4, generation: g), .queued)
    }

    func testEndDropsTheWaitingFrameAndRefusesLateOnes() {
        var p = VideoFramePacer<Int>()
        let g = p.begin()
        XCTAssertEqual(p.offer(1, generation: g), .queued)
        p.end()
        // A picture the decoder finishes after VIDEO_END must not land
        // over the lossless refresh.
        XCTAssertEqual(p.offer(2, generation: g), .rejected)
        let t = p.take()
        XCTAssertNil(t.frame)
        XCTAssertFalse(t.active)
    }

    func testANewPeriodRefusesFramesOfTheOldOne() {
        var p = VideoFramePacer<Int>()
        let g1 = p.begin()
        XCTAssertEqual(p.offer(1, generation: g1), .queued)
        let g2 = p.begin()
        XCTAssertNotEqual(g1, g2)
        XCTAssertFalse(p.hasPending)
        XCTAssertEqual(p.offer(2, generation: g1), .rejected)
        XCTAssertEqual(p.offer(3, generation: g2), .queued)
        let t = p.take()
        XCTAssertEqual(t.frame, 3)
        XCTAssertEqual(t.generation, g2)
    }

    // MARK: YCbCr -> RGB

    private func assertRGB(_ c: YUVToRGB, y: Int, cb: Int, cr: Int,
                           _ r: Float, _ g: Float, _ b: Float,
                           accuracy: Float = 0.006, file: StaticString = #filePath, line: UInt = #line) {
        let v = c.apply(y: Float(y) / 255, cb: Float(cb) / 255, cr: Float(cr) / 255)
        XCTAssertEqual(v.x, r, accuracy: accuracy, "R", file: file, line: line)
        XCTAssertEqual(v.y, g, accuracy: accuracy, "G", file: file, line: line)
        XCTAssertEqual(v.z, b, accuracy: accuracy, "B", file: file, line: line)
    }

    func testMatrixSelection() {
        XCTAssertEqual(YUVToRGB.Matrix.from(codePoint: 1), .bt709)
        XCTAssertEqual(YUVToRGB.Matrix.from(codePoint: 5), .bt601)
        XCTAssertEqual(YUVToRGB.Matrix.from(codePoint: 6), .bt601)
        XCTAssertEqual(YUVToRGB.Matrix.from(codePoint: 7), .smpte240m)
        XCTAssertEqual(YUVToRGB.Matrix.from(codePoint: 9), .bt2020)
        XCTAssertEqual(YUVToRGB.Matrix.from(codePoint: 10), .bt2020)
        // Unspecified / identity / reserved: what the host produces.
        XCTAssertEqual(YUVToRGB.Matrix.from(codePoint: 2), .bt709)
        XCTAssertEqual(YUVToRGB.Matrix.from(codePoint: 0), .bt709)
        XCTAssertEqual(YUVToRGB.Matrix.from(codePoint: 200), .bt709)
        XCTAssertEqual(YUVToRGB.make(matrixCodePoint: 6, fullRange: false),
                       YUVToRGB.make(matrix: .bt601, fullRange: false))
    }

    func testVideoRangeBlackWhiteGrey() {
        for m in [YUVToRGB.Matrix.bt709, .bt601, .bt2020, .smpte240m] {
            let c = YUVToRGB.make(matrix: m, fullRange: false)
            assertRGB(c, y: 16, cb: 128, cr: 128, 0, 0, 0)
            assertRGB(c, y: 235, cb: 128, cr: 128, 1, 1, 1)
            assertRGB(c, y: 126, cb: 128, cr: 128, 110.0 / 219, 110.0 / 219, 110.0 / 219)
        }
    }

    func testFullRangeBlackWhite() {
        let c = YUVToRGB.make(matrix: .bt709, fullRange: true)
        assertRGB(c, y: 0, cb: 128, cr: 128, 0, 0, 0)
        assertRGB(c, y: 255, cb: 128, cr: 128, 1, 1, 1)
    }

    func testBT709VideoRangePrimaries() {
        // BT.709 limited-range codes of 100% red / green / blue.
        let c = YUVToRGB.make(matrix: .bt709, fullRange: false)
        assertRGB(c, y: 63, cb: 102, cr: 240, 1, 0, 0)
        assertRGB(c, y: 173, cb: 42, cr: 26, 0, 1, 0)
        assertRGB(c, y: 32, cb: 240, cr: 118, 0, 0, 1)
    }

    func testBT601VideoRangePrimaries() {
        let c = YUVToRGB.make(matrix: .bt601, fullRange: false)
        assertRGB(c, y: 81, cb: 90, cr: 240, 1, 0, 0)
        assertRGB(c, y: 145, cb: 54, cr: 34, 0, 1, 0)
        assertRGB(c, y: 41, cb: 240, cr: 110, 0, 0, 1)
    }
}
