import CoreGraphics
import Foundation
import Metal
import QuartzCore
import UIKit

// Section 4: the TeamViewer-style remote screen renderer. The remote
// framebuffer lives in one persistent BGRA8Unorm texture (only the rects a
// frame changed are uploaded); each redraw scales just the visible part of
// it to device pixels into a CAMetalLayer (BGRA8Unorm, no colour
// management) with the pipeline of crates/ovapp/src/scale.rs — see
// RemoteScreen.metal. Redraws happen only when something changed (frame,
// zoom / pan, size); RemoteSurfaceView paces them with a display link.
// Main thread only.

/// One frame's BGRA bytes (B, G, R, X per pixel, `rowBytes` per row) and
/// the rects it changed (frame pixels; nil = everything).
struct FramePixels {
    let bytes: Data
    let rowBytes: Int
    let width: Int
    let height: Int
    var dirty: [CGRect]?

    /// More rects than this count as a full update.
    static let maxRects = 512

    /// The rects to upload when `newer` replaces a not yet shown `older`.
    static func merged(_ older: FramePixels, _ newer: FramePixels) -> [CGRect]? {
        guard older.width == newer.width, older.height == newer.height,
              let a = older.dirty, let b = newer.dirty,
              a.count + b.count <= maxRects else { return nil }
        return a + b
    }

    /// The buffer really holds `height` rows of `width` BGRA pixels.
    var isValid: Bool {
        guard width > 0, height > 0, rowBytes >= width * 4 else { return false }
        let (rows, o1) = rowBytes.multipliedReportingOverflow(by: height - 1)
        guard !o1 else { return false }
        let (need, o2) = rows.addingReportingOverflow(width * 4)
        return !o2 && bytes.count >= need
    }
}

/// The latest frame for the renderer plus the rects changed since the
/// renderer last took it. Main thread only (filled by SessionController's
/// screen flush, drained by the surface).
final class FramePixelStore {
    private(set) var latest: FramePixels?
    /// Rects changed since the last `take` (nil = everything).
    private var pendingDirty: [CGRect]?
    private var hasUpdate = false

    func publish(_ frame: FramePixels) {
        let sameSize = latest.map { $0.width == frame.width && $0.height == frame.height } ?? false
        if !sameSize {
            pendingDirty = nil
        } else if !hasUpdate {
            pendingDirty = frame.dirty
        } else if let pending = pendingDirty, let d = frame.dirty,
                  pending.count + d.count <= FramePixels.maxRects {
            pendingDirty = pending + d
        } else {
            pendingDirty = nil
        }
        latest = frame
        hasUpdate = true
    }

    /// The frame and the rects changed since the previous take (nil =
    /// everything), or nil when nothing new arrived.
    func take() -> (frame: FramePixels, dirty: [CGRect]?)? {
        guard hasUpdate, let frame = latest else { return nil }
        hasUpdate = false
        let dirty = pendingDirty
        pendingDirty = nil
        return (frame: frame, dirty: dirty)
    }
}

/// Uniforms shared by every pass; layout must match `ScaleParams` in
/// RemoteScreen.metal (56 bytes: four 8-byte vectors, six 4-byte scalars).
private struct ScaleParams {
    var org: SIMD2<Float>
    var srcSize: SIMD2<Int32>
    var dstOrigin: SIMD2<Int32>
    var dstSize: SIMD2<Int32>
    var scale: Float
    var invScale: Float
    var rowBase: Int32
    var rowCount: Int32
    var sharpen: Float
    var pad: Float
}

final class MetalScreenRenderer {
    /// TeamViewer's unsharp-mask strength (scale.rs TEAMVIEWER_SHARPEN).
    static let sharpenStrength: Float = 0.5

    let layer = CAMetalLayer()
    private let device: MTLDevice
    private let commandQueue: MTLCommandQueue
    private let areaX: MTLRenderPipelineState
    private let areaY: MTLRenderPipelineState
    private let bilinear: MTLRenderPipelineState
    private let sharpen: MTLRenderPipelineState
    /// The remote framebuffer (persistent; dirty rects replace into it).
    private var frameTexture: MTLTexture?
    /// Intermediates: X-pass result (visible width x source rows) and the
    /// scaled visible rect before the sharpen pass. Grown, never shrunk.
    private var tmpX: MTLTexture?
    private var tmpScaled: MTLTexture?
    /// The last submitted draw (uploads wait for it: no torn reads).
    private var lastCommandBuffer: MTLCommandBuffer?
    /// A taken update could not be applied: the next sync uploads the
    /// whole frame.
    private var needsFullUpload = false

    /// nil when Metal (or the compiled shaders) is unavailable — the
    /// surface then keeps its CGImage layer.
    init?() {
        guard let device = MTLCreateSystemDefaultDevice(),
              let queue = device.makeCommandQueue(),
              let library = device.makeDefaultLibrary(),
              let vs = library.makeFunction(name: "ov_fullscreen_vs"),
              let ax = MetalScreenRenderer.pipeline(device, library, vs, "ov_area_x"),
              let ay = MetalScreenRenderer.pipeline(device, library, vs, "ov_area_y"),
              let bl = MetalScreenRenderer.pipeline(device, library, vs, "ov_bilinear"),
              let sh = MetalScreenRenderer.pipeline(device, library, vs, "ov_sharpen") else { return nil }
        self.device = device
        commandQueue = queue
        areaX = ax
        areaY = ay
        bilinear = bl
        sharpen = sh
        layer.device = device
        layer.pixelFormat = .bgra8Unorm
        layer.framebufferOnly = true
        layer.isOpaque = true
        layer.backgroundColor = UIColor.black.cgColor
        layer.actions = [
            "position": NSNull(), "bounds": NSNull(), "frame": NSNull(),
            "contents": NSNull(), "hidden": NSNull(), "contentsScale": NSNull(),
        ]
    }

    private static func pipeline(_ device: MTLDevice, _ library: MTLLibrary,
                                 _ vs: MTLFunction, _ name: String) -> MTLRenderPipelineState? {
        guard let fs = library.makeFunction(name: name) else { return nil }
        let d = MTLRenderPipelineDescriptor()
        d.vertexFunction = vs
        d.fragmentFunction = fs
        d.colorAttachments[0].pixelFormat = .bgra8Unorm
        return try? device.makeRenderPipelineState(descriptor: d)
    }

    var hasFrame: Bool { frameTexture != nil }

    /// Bring the framebuffer texture up to date with `store`: only the
    /// changed rects, or everything after a size change / first use.
    /// Returns true when the texture changed.
    @discardableResult
    func sync(from store: FramePixelStore) -> Bool {
        let update = store.take()
        guard let frame = update?.frame ?? (frameTexture == nil ? store.latest : nil),
              frame.isValid else {
            // The rects just taken would be lost: re-upload everything
            // next time instead of leaving stale regions.
            if update != nil { needsFullUpload = true }
            return false
        }
        var rects = needsFullUpload ? nil : update?.dirty
        needsFullUpload = false
        let texture: MTLTexture
        if let t = frameTexture, t.width == frame.width, t.height == frame.height {
            texture = t
        } else {
            let d = MTLTextureDescriptor.texture2DDescriptor(pixelFormat: .bgra8Unorm,
                                                             width: frame.width,
                                                             height: frame.height,
                                                             mipmapped: false)
            d.usage = [.shaderRead]
            d.storageMode = .shared
            guard let t = device.makeTexture(descriptor: d) else {
                needsFullUpload = true
                return false
            }
            texture = t
            frameTexture = t
            rects = nil
        }
        // The GPU may still be scaling the previous frame out of this
        // texture (a ~1-2 ms draw): finish it so an upload never tears.
        lastCommandBuffer?.waitUntilCompleted()
        frame.bytes.withUnsafeBytes { (raw: UnsafeRawBufferPointer) in
            guard let base = raw.baseAddress else { return }
            guard let list = rects else {
                texture.replace(region: MTLRegionMake2D(0, 0, frame.width, frame.height),
                                mipmapLevel: 0, withBytes: base, bytesPerRow: frame.rowBytes)
                return
            }
            for r in list {
                guard r.minX.isFinite, r.minY.isFinite, r.maxX.isFinite, r.maxY.isFinite else { continue }
                let x0 = max(0, Int(r.minX.rounded(.down)))
                let y0 = max(0, Int(r.minY.rounded(.down)))
                let x1 = min(frame.width, Int(r.maxX.rounded(.up)))
                let y1 = min(frame.height, Int(r.maxY.rounded(.up)))
                guard x1 > x0, y1 > y0 else { continue }
                texture.replace(region: MTLRegionMake2D(x0, y0, x1 - x0, y1 - y0),
                                mipmapLevel: 0,
                                withBytes: base + y0 * frame.rowBytes + x0 * 4,
                                bytesPerRow: frame.rowBytes)
            }
        }
        return true
    }

    /// Lay the layer out over `bounds` (view points) at `contentsScale`.
    /// Returns true when the geometry changed (a redraw is needed).
    @discardableResult
    func layout(bounds: CGRect, contentsScale: CGFloat) -> Bool {
        let w = (bounds.width * contentsScale).rounded()
        let h = (bounds.height * contentsScale).rounded()
        let size = CGSize(width: max(w, 1), height: max(h, 1))
        guard layer.frame != bounds || layer.contentsScale != contentsScale
                || layer.drawableSize != size else { return false }
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        layer.frame = bounds
        layer.contentsScale = contentsScale
        CATransaction.commit()
        if layer.drawableSize != size {
            layer.drawableSize = size
        }
        return true
    }

    /// Draw the frame at `scale` (view points per remote pixel) with its
    /// top-left at `offset` (view points). Only the visible part is scaled,
    /// at device-pixel resolution.
    /// `onPresented` runs (any thread) once this drawable is on screen;
    /// it is dropped when nothing could be drawn (the caller's fallback
    /// covers that).
    func render(scale: CGFloat, offset: CGPoint, sharpenOn: Bool, onPresented: (() -> Void)? = nil) {
        let ds = layer.drawableSize
        let cs = layer.contentsScale
        let dw = Int(ds.width), dh = Int(ds.height)
        guard dw > 0, dh > 0, cs > 0 else { return }
        guard let drawable = layer.nextDrawable(),
              let cb = commandQueue.makeCommandBuffer() else { return }

        var params = ScaleParams(org: .zero, srcSize: .zero, dstOrigin: .zero, dstSize: .zero,
                                 scale: 1, invScale: 1, rowBase: 0, rowCount: 1,
                                 sharpen: sharpenOn ? Self.sharpenStrength : 0, pad: 0)
        var drewFrame = false
        let s = Double(scale * cs) // device pixels per remote pixel
        if let tex = frameTexture, s.isFinite, s > 0 {
            let sw = tex.width, sh = tex.height
            let ox = Double(offset.x * cs), oy = Double(offset.y * cs)
            let vx0 = max(0, Int(ox.rounded(.down)))
            let vy0 = max(0, Int(oy.rounded(.down)))
            let vx1 = min(dw, Int((ox + Double(sw) * s).rounded(.up)))
            let vy1 = min(dh, Int((oy + Double(sh) * s).rounded(.up)))
            let visW = vx1 - vx0, visH = vy1 - vy0
            if visW > 0, visH > 0,
               let scaled = Self.ensure(&tmpScaled, device: device, width: visW, height: visH) {
                let inv = 1 / s
                params.org = SIMD2<Float>(Float(ox - Double(vx0)), Float(oy - Double(vy0)))
                params.srcSize = SIMD2<Int32>(Int32(sw), Int32(sh))
                params.dstOrigin = SIMD2<Int32>(Int32(vx0), Int32(vy0))
                params.dstSize = SIMD2<Int32>(Int32(visW), Int32(visH))
                params.scale = Float(s)
                params.invScale = Float(inv)
                var ok = true
                if s < 1 {
                    // Area filter: X into (visible width x needed source
                    // rows), then Y into the visible rect.
                    let r0 = max(0, Int(((Double(vy0) - oy) * inv).rounded(.down)))
                    let r1 = min(sh, Int(((Double(vy1) - oy) * inv).rounded(.up)))
                    let rows = max(1, r1 - r0)
                    params.rowBase = Int32(r0)
                    params.rowCount = Int32(rows)
                    if let tx = Self.ensure(&tmpX, device: device, width: visW, height: rows) {
                        encode(cb, target: tx, viewport: (0, 0, visW, rows), clear: false,
                               pipeline: areaX, texture: tex, params: &params)
                        encode(cb, target: scaled, viewport: (0, 0, visW, visH), clear: false,
                               pipeline: areaY, texture: tx, params: &params)
                    } else {
                        ok = false
                    }
                } else {
                    encode(cb, target: scaled, viewport: (0, 0, visW, visH), clear: false,
                           pipeline: bilinear, texture: tex, params: &params)
                }
                if ok {
                    encode(cb, target: drawable.texture, viewport: (vx0, vy0, visW, visH), clear: true,
                           pipeline: sharpen, texture: scaled, params: &params)
                    drewFrame = true
                }
            }
        }
        if !drewFrame {
            // Nothing visible (or no frame yet): just black.
            let rp = MTLRenderPassDescriptor()
            rp.colorAttachments[0].texture = drawable.texture
            rp.colorAttachments[0].loadAction = .clear
            rp.colorAttachments[0].clearColor = MTLClearColor(red: 0, green: 0, blue: 0, alpha: 1)
            rp.colorAttachments[0].storeAction = .store
            cb.makeRenderCommandEncoder(descriptor: rp)?.endEncoding()
        }
        if let onPresented {
            #if targetEnvironment(simulator)
            // addPresentedHandler is unavailable in the simulator SDK; the
            // command buffer completing is the closest stand-in there.
            cb.addCompletedHandler { _ in onPresented() }
            #else
            drawable.addPresentedHandler { _ in onPresented() }
            #endif
        }
        cb.present(drawable)
        cb.commit()
        lastCommandBuffer = cb
    }

    /// One full-viewport triangle with `pipeline`, reading `texture`.
    private func encode(_ cb: MTLCommandBuffer, target: MTLTexture,
                        viewport v: (x: Int, y: Int, w: Int, h: Int), clear: Bool,
                        pipeline: MTLRenderPipelineState, texture: MTLTexture,
                        params: inout ScaleParams) {
        let rp = MTLRenderPassDescriptor()
        rp.colorAttachments[0].texture = target
        rp.colorAttachments[0].loadAction = clear ? .clear : .dontCare
        rp.colorAttachments[0].clearColor = MTLClearColor(red: 0, green: 0, blue: 0, alpha: 1)
        rp.colorAttachments[0].storeAction = .store
        guard let enc = cb.makeRenderCommandEncoder(descriptor: rp) else { return }
        enc.setRenderPipelineState(pipeline)
        enc.setViewport(MTLViewport(originX: Double(v.x), originY: Double(v.y),
                                    width: Double(v.w), height: Double(v.h),
                                    znear: 0, zfar: 1))
        enc.setFragmentTexture(texture, index: 0)
        enc.setFragmentBytes(&params, length: MemoryLayout<ScaleParams>.stride, index: 0)
        enc.drawPrimitives(type: .triangle, vertexStart: 0, vertexCount: 3)
        enc.endEncoding()
    }

    /// A render-target texture at least `width x height` (grown in 256 px
    /// steps so pinching does not reallocate every frame).
    private static func ensure(_ slot: inout MTLTexture?, device: MTLDevice,
                               width: Int, height: Int) -> MTLTexture? {
        if let t = slot, t.width >= width, t.height >= height {
            return t
        }
        let limit = 8192
        func grow(_ need: Int, _ have: Int) -> Int {
            min(limit, max(have, (need + 255) / 256 * 256))
        }
        let w = grow(width, slot?.width ?? 0)
        let h = grow(height, slot?.height ?? 0)
        guard w >= width, h >= height else { return nil }
        let d = MTLTextureDescriptor.texture2DDescriptor(pixelFormat: .bgra8Unorm,
                                                         width: w, height: h, mipmapped: false)
        d.usage = [.renderTarget, .shaderRead]
        d.storageMode = .private
        slot = device.makeTexture(descriptor: d)
        return slot
    }
}
