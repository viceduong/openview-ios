import Foundation
import simd

// Pure logic of the hardware video path (unit-tested in OpenViewTests, so
// Foundation + simd only): the newest-wins frame slot between the
// VideoToolbox output callback and the display link, and the YCbCr -> RGB
// matrix the Metal compositor applies to a decoded frame.

/// One-deep, newest-wins hand-off of decoded video frames from the decoder
/// (any thread, through `VideoFrameQueue`) to the renderer (display link
/// tick). At most one frame waits: a newer one replaces it (the older is
/// dropped, never shown late). Frames belong to a motion period
/// (`generation`); `end()` closes the period, so a frame the decoder
/// finishes after VIDEO_END is refused instead of overwriting the lossless
/// refresh. Not thread safe on its own.
struct VideoFramePacer<Frame> {
    /// What the renderer gets on a tick.
    struct Take {
        /// The newest frame since the last take (nil = nothing new).
        let frame: Frame?
        /// A motion period is running (false: drop any held frame).
        let active: Bool
        /// The running (or last) motion period.
        let generation: Int
    }

    /// Result of offering a decoded frame.
    enum Offer: Equatable {
        /// Queued into an empty slot: ask for a redraw.
        case queued
        /// Replaced a frame nobody took yet (that one is dropped); a redraw
        /// is already pending.
        case replaced
        /// Not of the running motion period (ended / superseded): ignored.
        case rejected
    }

    private(set) var generation = 0
    private(set) var active = false
    private var pending: Frame?
    /// Frames replaced before any tick took them.
    private(set) var dropped: UInt64 = 0

    var hasPending: Bool { pending != nil }

    /// A motion period starts; returns its generation.
    mutating func begin() -> Int {
        generation &+= 1
        active = true
        pending = nil
        return generation
    }

    /// The running motion period is over (VIDEO_END / reset).
    mutating func end() {
        active = false
        pending = nil
    }

    mutating func offer(_ frame: Frame, generation gen: Int) -> Offer {
        guard active, gen == generation else { return .rejected }
        let replaced = pending != nil
        pending = frame
        if replaced {
            dropped &+= 1
            return .replaced
        }
        return .queued
    }

    /// Display-link tick: the newest frame (the slot empties).
    mutating func take() -> Take {
        let f = pending
        pending = nil
        return Take(frame: f, active: active, generation: generation)
    }
}

/// The YCbCr -> R'G'B' conversion for one stream: `rgb = rows · (y, cb, cr, 1)`
/// on the raw 8-bit-normalised plane samples (Y from the luma plane, Cb/Cr
/// from the interleaved chroma plane), range expansion folded in. Rows feed
/// `ov_yuv_composite` (RemoteScreen.metal) as three float4s.
struct YUVToRGB: Equatable {
    var r: SIMD4<Float>
    var g: SIMD4<Float>
    var b: SIMD4<Float>

    /// The matrix family a stream's `matrix_coefficients` code point
    /// (H.264 / HEVC VUI, ISO 23001-8) selects.
    enum Matrix: Equatable {
        case bt709
        case bt601
        case bt2020
        case smpte240m

        /// (Kr, Kb) luma weights.
        var weights: (kr: Double, kb: Double) {
            switch self {
            case .bt709: return (0.2126, 0.0722)
            case .bt601: return (0.299, 0.114)
            case .bt2020: return (0.2627, 0.0593)
            case .smpte240m: return (0.212, 0.087)
            }
        }

        /// 1 = BT.709, 5 / 6 = BT.601 (BT.470BG / SMPTE 170M), 7 = SMPTE
        /// 240M, 9 / 10 = BT.2020 (non-constant / constant luminance — the
        /// latter approximated). Anything else (0 identity, 2 unspecified,
        /// reserved) = BT.709, what the host's converter produces.
        static func from(codePoint v: UInt8) -> Matrix {
            switch v {
            case 5, 6: return .bt601
            case 7: return .smpte240m
            case 9, 10: return .bt2020
            default: return .bt709
            }
        }
    }

    static func make(matrix: Matrix, fullRange: Bool) -> YUVToRGB {
        let (kr, kb) = matrix.weights
        let kg = 1 - kr - kb
        // Normalised Y' in [0, 1], Cb / Cr in [-0.5, 0.5]:
        //   R = Y + 2(1-Kr) Cr
        //   G = Y - 2Kb(1-Kb)/Kg Cb - 2Kr(1-Kr)/Kg Cr
        //   B = Y + 2(1-Kb) Cb
        let crR = 2 * (1 - kr)
        let cbG = -2 * kb * (1 - kb) / kg
        let crG = -2 * kr * (1 - kr) / kg
        let cbB = 2 * (1 - kb)
        // Raw sample s (0...1, i.e. code / 255) -> normalised:
        //   video range: Y = (s - 16/255) * 255/219, C = (s - 128/255) * 255/224
        //   full range:  Y = s,                      C = s - 128/255
        let ys: Double = fullRange ? 1 : 255.0 / 219.0
        let yo: Double = fullRange ? 0 : -16.0 / 219.0
        let cs: Double = fullRange ? 1 : 255.0 / 224.0
        let co: Double = fullRange ? -128.0 / 255.0 : -128.0 / 224.0
        func row(_ cb: Double, _ cr: Double) -> SIMD4<Float> {
            SIMD4<Float>(Float(ys),
                         Float(cb * cs),
                         Float(cr * cs),
                         Float(yo + (cb + cr) * co))
        }
        return YUVToRGB(r: row(0, crR), g: row(cbG, crG), b: row(cbB, 0))
    }

    /// The conversion for a stream's colour code points.
    static func make(matrixCodePoint: UInt8, fullRange: Bool) -> YUVToRGB {
        make(matrix: .from(codePoint: matrixCodePoint), fullRange: fullRange)
    }

    /// CPU reference of the shader (tests): raw samples (0...1) -> R'G'B'.
    func apply(y: Float, cb: Float, cr: Float) -> SIMD3<Float> {
        let v = SIMD4<Float>(y, cb, cr, 1)
        return SIMD3<Float>(simd_dot(r, v), simd_dot(g, v), simd_dot(b, v))
    }
}
