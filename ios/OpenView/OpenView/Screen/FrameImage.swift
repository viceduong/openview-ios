import CoreGraphics
import Foundation

// MARK: - Frame images

/// CGImage builders for remote frames and cursor sprites. Pure functions,
/// run on the session queue. Malformed sizes return nil, never crash.
enum FrameImage {
    /// Largest accepted frame / cursor edge, in pixels.
    static let maxDimension = 8192
    static let maxCursorDimension = 1024

    /// Build a displayable `CGImage` from a reassembled frame.
    ///
    /// Format 0 (BGRA, 4 bytes/px) is wrapped as-is — zero conversion, the
    /// provider references the frame's `Data` — as `noneSkipFirst` +
    /// `32Little`: the 4th byte is GDI's unused high byte (0 for real
    /// capture), not alpha, so it is skipped and the image is opaque.
    /// Format 1 (BGR, 3 bytes/px) is expanded once into BGRX. `stride` is
    /// the source row pitch in bytes (0 / short buffers fall back to tightly
    /// packed rows).
    static func make(width: UInt32, height: UInt32, stride: UInt32,
                     format: UInt8, pixels: Data) -> (image: CGImage, bytes: Data, rowBytes: Int)? {
        let w = Int(width), h = Int(height)
        guard w > 0, h > 0, w <= maxDimension, h <= maxDimension else { return nil }
        let bpp: Int
        switch format {
        case 0: bpp = 4
        case 1: bpp = 3
        default: return nil
        }
        guard let rowBytes = rowPitch(width: w, height: h, bpp: bpp,
                                      stride: Int(stride), available: pixels.count) else { return nil }
        let info = CGBitmapInfo(rawValue: CGImageAlphaInfo.noneSkipFirst.rawValue
            | CGBitmapInfo.byteOrder32Little.rawValue)
        let provider: CGDataProvider
        let outRowBytes: Int
        let bgra: Data
        if bpp == 4 {
            guard let p = CGDataProvider(data: pixels as CFData) else { return nil }
            provider = p
            outRowBytes = rowBytes
            bgra = pixels
        } else {
            // BGR -> BGRX, one pass over raw pointers (no per-pixel bounds
            // checks; the extents were validated above).
            let dstRow = w * 4
            var out = Data(count: dstRow * h)
            out.withUnsafeMutableBytes { (dstRaw: UnsafeMutableRawBufferPointer) in
                pixels.withUnsafeBytes { (srcRaw: UnsafeRawBufferPointer) in
                    guard let dBase = dstRaw.baseAddress?.assumingMemoryBound(to: UInt8.self),
                          let sBase = srcRaw.baseAddress?.assumingMemoryBound(to: UInt8.self) else { return }
                    for y in 0..<h {
                        var s = sBase + y * rowBytes
                        var d = dBase + y * dstRow
                        for _ in 0..<w {
                            d[0] = s[0]; d[1] = s[1]; d[2] = s[2]; d[3] = 255
                            s += 3
                            d += 4
                        }
                    }
                }
            }
            guard let p = CGDataProvider(data: out as CFData) else { return nil }
            provider = p
            outRowBytes = dstRow
            bgra = out
        }
        guard let image = CGImage(
            width: w, height: h,
            bitsPerComponent: 8, bitsPerPixel: 32, bytesPerRow: outRowBytes,
            space: CGColorSpaceCreateDeviceRGB(),
            bitmapInfo: info,
            provider: provider,
            decode: nil,
            shouldInterpolate: true,
            intent: .defaultIntent
        ) else { return nil }
        return (image: image, bytes: bgra, rowBytes: outRowBytes)
    }

    /// The usable source row pitch, or nil when the buffer cannot hold the
    /// frame. Prefers the announced stride; falls back to packed rows.
    /// All products are overflow-checked.
    private static func rowPitch(width w: Int, height h: Int, bpp: Int,
                                 stride: Int, available: Int) -> Int? {
        let (packed, o1) = w.multipliedReportingOverflow(by: bpp)
        guard !o1 else { return nil }
        if stride >= packed {
            let (need, o2) = stride.multipliedReportingOverflow(by: h)
            if !o2, available >= need { return stride }
        }
        let (needPacked, o3) = packed.multipliedReportingOverflow(by: h)
        guard !o3, available >= needPacked else { return nil }
        return packed
    }

    /// Build the cursor overlay image from a premultiplied-BGRA `CURSOR_SHAPE`
    /// bitmap. Unlike frames the alpha channel is kept (that is the whole
    /// point of a cursor sprite); the layout is the same BGRA32-little mapping.
    static func makeCursor(w: UInt16, h: UInt16, bgra: Data) -> CGImage? {
        let width = Int(w), height = Int(h)
        guard width > 0, height > 0,
              width <= maxCursorDimension, height <= maxCursorDimension,
              bgra.count >= width * height * 4 else { return nil }
        // 8-bit premultiplied BGRA: little-endian byte order (B first in
        // memory) = alpha-first 32-bit words, hence premultipliedFirst +
        // byteOrder32Little.
        let info = CGBitmapInfo(rawValue: CGImageAlphaInfo.premultipliedFirst.rawValue
            | CGBitmapInfo.byteOrder32Little.rawValue)
        guard let provider = CGDataProvider(data: bgra as CFData) else { return nil }
        return CGImage(
            width: width, height: height,
            bitsPerComponent: 8, bitsPerPixel: 32, bytesPerRow: width * 4,
            space: CGColorSpaceCreateDeviceRGB(),
            bitmapInfo: info,
            provider: provider,
            decode: nil,
            shouldInterpolate: true,
            intent: .defaultIntent
        )
    }
}
