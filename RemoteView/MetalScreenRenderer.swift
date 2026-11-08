import CoreGraphics
import Foundation
import Metal
import QuartzCore
import UIKit

// Section 4: the TeamViewer-style remote screen renderer. The remote
// framebuffer lives in one persistent GPU-private BGRA8Unorm texture; each
// redraw scales just the visible part of it to device pixels into a
// CAMetalLayer (BGRA8Unorm, no colour management, 2 drawables) with the
// pipeline of crates/ovapp/src/scale.rs — see RemoteScreen.metal. Redraws
// happen only when something changed (frame, zoom / pan, size);
// RemoteSurfaceView paces them with a display link. Main thread only,
// except FrameUploadQueue (Wave 1): the session queue stages each frame's
// dirty rects from the Rust framebuffer into shared staging buffers, and
// every redraw first blits what was staged into the texture (same command
// buffer, GPU-ordered behind the previous draw — no CPU stall).

/// Metal objects shared by every renderer and by the frame upload staging,
/// created once on first use. nil = no Metal device or no compiled shaders:
/// the surface keeps its CGImage layer and the session builds CGImages.
final class MetalContext {
    static let shared: MetalContext? = MetalContext()

    let device: MTLDevice
    let commandQueue: MTLCommandQueue
    let areaX: MTLRenderPipelineState
    let areaY: MTLRenderPipelineState
    let bilinear: MTLRenderPipelineState
    let sharpen: MTLRenderPipelineState

    private init?() {
        guard let device = MTLCreateSystemDefaultDevice(),
              let queue = device.makeCommandQueue(),
              let library = device.makeDefaultLibrary(),
              let vs = library.makeFunction(name: "ov_fullscreen_vs"),
              let ax = MetalContext.pipeline(device, library, vs, "ov_area_x"),
              let ay = MetalContext.pipeline(device, library, vs, "ov_area_y"),
              let bl = MetalContext.pipeline(device, library, vs, "ov_bilinear"),
              let sh = MetalContext.pipeline(device, library, vs, "ov_sharpen") else { return nil }
        self.device = device
        commandQueue = queue
        areaX = ax
        areaY = ay
        bilinear = bl
        sharpen = sh
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
}

/// Wave 1 zero-copy frame uploads. The session queue copies only the rects
/// a frame changed — straight from the Rust framebuffer
/// (`OvScreenBox.FrameView`), no whole-frame `Data` copy — into a shared
/// staging MTLBuffer; the renderer (main) blits the staged regions into its
/// private frame texture at the start of its next command buffer. The blit
/// is GPU-ordered behind the previous draw on the same queue, so there is no
/// CPU wait (`waitUntilCompleted`) and no torn read. A full frame replaces
/// everything staged before it; the renderer asks for a full re-stage
/// (`requestFull`) when it has no base to apply deltas to (new surface, lost
/// staging). Thread safe; staging buffers are recycled once the GPU is done.
final class FrameUploadQueue {
    /// One rect: `w x h` pixels at `(x, y)`, tight BGRA rows at `offset`.
    struct Region {
        let x: Int
        let y: Int
        let w: Int
        let h: Int
        let offset: Int
        let rowBytes: Int
    }

    /// One staged frame (or delta).
    struct Batch {
        let buffer: MTLBuffer
        let regions: [Region]
    }

    /// Everything staged since the last drain. `full`: the first batch
    /// covers the whole `width x height` frame (a new texture is fine).
    struct Drained {
        let width: Int
        let height: Int
        let full: Bool
        let batches: [Batch]
    }

    let context: MetalContext
    /// Main thread only: re-stage the whole current frame (the controller
    /// dispatches to the session queue). Called at most once per request.
    var requestFull: (() -> Void)?

    private let lock = NSLock()
    private var width = 0
    private var height = 0
    private var batches: [Batch] = []
    private var full = false
    private var pendingBytes = 0
    /// The next stage must be a full frame (no base on the renderer side).
    private var wantFull = true
    /// `requestFull` already asked; cleared by the next full stage.
    private var fullRequested = false
    /// Recycled staging buffers.
    private var pool: [MTLBuffer] = []

    /// Staged-but-not-drained bytes beyond which the next frame is staged
    /// whole instead (bounds memory while nothing renders, e.g. backgrounded).
    private static let maxPendingBytes = 24 << 20
    private static let maxPooled = 4
    private static let minBufferBytes = 256 << 10
    static let maxDimension = 8192

    init(context: MetalContext) {
        self.context = context
    }

    /// Session queue: stage the pixels `view` changed (its dirty rects, or
    /// everything when `forceFull`, when the rect list is empty, or when the
    /// renderer needs a base). Reads the Rust framebuffer only during the
    /// call.
    func stage(_ view: OvScreenBox.FrameView, forceFull: Bool = false) {
        let w = view.width, h = view.height
        let bpp: Int
        switch view.format {
        case 0: bpp = 4
        case 1: bpp = 3
        default: return
        }
        guard w > 0, h > 0, w <= Self.maxDimension, h <= Self.maxDimension,
              view.stride >= w * bpp,
              view.count >= view.stride * (h - 1) + w * bpp else { return }
        lock.lock()
        var isFull = forceFull || wantFull || w != width || h != height
            || pendingBytes > Self.maxPendingBytes || view.rects.isEmpty
        lock.unlock()
        var rects: [(x: Int, y: Int, w: Int, h: Int)] = []
        if !isFull {
            let n = view.rects.count / 4
            rects.reserveCapacity(n)
            for i in 0..<n {
                let x0 = Int(view.rects[i * 4])
                let y0 = Int(view.rects[i * 4 + 1])
                let x1 = min(w, x0 + Int(view.rects[i * 4 + 2]))
                let y1 = min(h, y0 + Int(view.rects[i * 4 + 3]))
                guard x1 > x0, y1 > y0 else { continue }
                if x0 == 0, y0 == 0, x1 == w, y1 == h {
                    isFull = true
                    break
                }
                rects.append((x: x0, y: y0, w: x1 - x0, h: y1 - y0))
            }
            if !isFull, rects.isEmpty { return } // nothing visible changed
        }
        if isFull {
            rects = [(x: 0, y: 0, w: w, h: h)]
        }
        var need = 0
        for r in rects {
            need += r.w * 4 * r.h
        }
        guard need > 0, let buffer = takeBuffer(minLength: need) else {
            // No staging memory: start over from a full frame next time.
            lock.lock()
            wantFull = true
            lock.unlock()
            return
        }
        let dst = buffer.contents()
        var regions: [Region] = []
        regions.reserveCapacity(rects.count)
        var off = 0
        for r in rects {
            let rowBytes = r.w * 4
            let src = view.pixels + (r.y * view.stride + r.x * bpp)
            if bpp == 4 {
                if r.x == 0, r.w == w, view.stride == rowBytes {
                    (dst + off).copyMemory(from: src, byteCount: rowBytes * r.h)
                } else {
                    for row in 0..<r.h {
                        (dst + (off + row * rowBytes)).copyMemory(from: src + row * view.stride,
                                                                  byteCount: rowBytes)
                    }
                }
            } else {
                // BGR -> BGRX (opaque), rect by rect.
                for row in 0..<r.h {
                    var s = (src + row * view.stride).assumingMemoryBound(to: UInt8.self)
                    var d = (dst + (off + row * rowBytes)).assumingMemoryBound(to: UInt8.self)
                    for _ in 0..<r.w {
                        d[0] = s[0]; d[1] = s[1]; d[2] = s[2]; d[3] = 255
                        s += 3
                        d += 4
                    }
                }
            }
            regions.append(Region(x: r.x, y: r.y, w: r.w, h: r.h, offset: off, rowBytes: rowBytes))
            off += rowBytes * r.h
        }
        let batch = Batch(buffer: buffer, regions: regions)
        lock.lock()
        if isFull {
            // Supersedes everything staged before (never handed to the GPU).
            recycleLocked(batches.map { $0.buffer })
            batches = [batch]
            full = true
            width = w
            height = h
            pendingBytes = need
            wantFull = false
            fullRequested = false
        } else if w == width, h == height, !wantFull {
            batches.append(batch)
            pendingBytes += need
        } else {
            // The renderer lost its base meanwhile: a delta is useless now.
            recycleLocked([buffer])
        }
        lock.unlock()
    }

    /// Main: take everything staged (nil = nothing new).
    func drain() -> Drained? {
        lock.lock()
        defer { lock.unlock() }
        guard !batches.isEmpty else { return nil }
        let d = Drained(width: width, height: height, full: full, batches: batches)
        batches = []
        full = false
        pendingBytes = 0
        return d
    }

    /// Main: the renderer has no base for deltas (new texture, lost data):
    /// drop what is staged and ask for the whole frame once.
    func needFull() {
        lock.lock()
        wantFull = true
        recycleLocked(batches.map { $0.buffer })
        batches = []
        full = false
        pendingBytes = 0
        let ask = !fullRequested
        fullRequested = true
        lock.unlock()
        if ask {
            requestFull?()
        }
    }

    /// Any thread: staging buffers the GPU is done with.
    func recycle(_ buffers: [MTLBuffer]) {
        guard !buffers.isEmpty else { return }
        lock.lock()
        recycleLocked(buffers)
        lock.unlock()
    }

    private func recycleLocked(_ buffers: [MTLBuffer]) {
        pool.append(contentsOf: buffers)
        if pool.count > Self.maxPooled {
            // Keep the largest (a full frame's buffer serves every delta).
            pool.sort { $0.length > $1.length }
            pool.removeLast(pool.count - Self.maxPooled)
        }
    }

    /// The smallest pooled buffer that fits, or a new shared one.
    private func takeBuffer(minLength: Int) -> MTLBuffer? {
        lock.lock()
        var best: Int?
        for (i, b) in pool.enumerated() where b.length >= minLength {
            if let j = best, pool[j].length <= b.length { continue }
            best = i
        }
        if let i = best {
            let b = pool.remove(at: i)
            lock.unlock()
            return b
        }
        lock.unlock()
        let length = max(Self.minBufferBytes, (minLength + 0xFFFF) & ~0xFFFF)
        // Write-combined: the CPU only ever writes these (no read-back).
        return context.device.makeBuffer(length: length,
                                         options: [.storageModeShared, .cpuCacheModeWriteCombined])
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
    private let ctx: MetalContext
    /// The remote framebuffer (persistent, GPU-private; staged dirty rects
    /// are blitted into it at the start of each command buffer).
    private var frameTexture: MTLTexture?
    /// Intermediates: X-pass result (visible width x source rows) and the
    /// scaled visible rect before the sharpen pass. Grown, never shrunk.
    private var tmpX: MTLTexture?
    private var tmpScaled: MTLTexture?
    /// Command buffers in flight: one. A redraw that finds the previous one
    /// still on the GPU (~1-2 ms of work, so practically never) is retried
    /// at the next display refresh instead of blocking main.
    private let inFlight = DispatchSemaphore(value: 1)

    /// nil when Metal (or the compiled shaders) is unavailable — the
    /// surface then keeps its CGImage layer.
    init?() {
        guard let ctx = MetalContext.shared else { return nil }
        self.ctx = ctx
        layer.device = ctx.device
        layer.pixelFormat = .bgra8Unorm
        layer.framebufferOnly = true
        // Wave 1: double buffering — one drawable on screen, one being
        // drawn; a third would only queue a frame of latency.
        layer.maximumDrawableCount = 2
        layer.isOpaque = true
        layer.backgroundColor = UIColor.black.cgColor
        layer.actions = [
            "position": NSNull(), "bounds": NSNull(), "frame": NSNull(),
            "contents": NSNull(), "hidden": NSNull(), "contentsScale": NSNull(),
        ]
    }

    var hasFrame: Bool { frameTexture != nil }

    /// Encode the staged uploads (blits) at the start of `cb`; returns the
    /// staging buffers to recycle once `cb` completed. Asks for a full
    /// re-stage when there is no texture to apply deltas to.
    private func encodeUploads(_ cb: MTLCommandBuffer, _ uploads: FrameUploadQueue?) -> [MTLBuffer] {
        guard let uploads else { return [] }
        guard let d = uploads.drain() else {
            if frameTexture == nil {
                uploads.needFull()
            }
            return []
        }
        var target: MTLTexture?
        if let t = frameTexture, t.width == d.width, t.height == d.height {
            target = t
        } else if d.full {
            let desc = MTLTextureDescriptor.texture2DDescriptor(pixelFormat: .bgra8Unorm,
                                                                width: d.width,
                                                                height: d.height,
                                                                mipmapped: false)
            desc.usage = [.shaderRead]
            desc.storageMode = .private
            target = ctx.device.makeTexture(descriptor: desc)
            frameTexture = target
        }
        let buffers = d.batches.map { $0.buffer }
        guard let tex = target, let blit = cb.makeBlitCommandEncoder() else {
            // Deltas for a texture we do not have (or no memory): start over.
            uploads.recycle(buffers)
            uploads.needFull()
            return []
        }
        for b in d.batches {
            for r in b.regions {
                blit.copy(from: b.buffer,
                          sourceOffset: r.offset,
                          sourceBytesPerRow: r.rowBytes,
                          sourceBytesPerImage: r.rowBytes * r.h,
                          sourceSize: MTLSize(width: r.w, height: r.h, depth: 1),
                          to: tex,
                          destinationSlice: 0,
                          destinationLevel: 0,
                          destinationOrigin: MTLOrigin(x: r.x, y: r.y, z: 0))
            }
        }
        blit.endEncoding()
        return buffers
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
    /// Wave 1: the frame's staged dirty rects (`uploads`) are blitted into
    /// the frame texture first, in the same command buffer — no CPU wait.
    /// `onPresented` runs (any thread) once this drawable is on screen;
    /// it is dropped when nothing could be drawn (the caller's fallback
    /// covers that).
    /// Returns false when the previous command buffer is still on the GPU:
    /// nothing was done (not even the uploads) — retry at the next refresh.
    @discardableResult
    func render(scale: CGFloat, offset: CGPoint, sharpenOn: Bool,
                uploads: FrameUploadQueue?, onPresented: (() -> Void)? = nil) -> Bool {
        let ds = layer.drawableSize
        let cs = layer.contentsScale
        let dw = Int(ds.width), dh = Int(ds.height)
        guard dw > 0, dh > 0, cs > 0 else { return true }
        // Never block main on the GPU: skip this refresh if it is busy.
        guard inFlight.wait(timeout: .now()) == .success else { return false }
        guard let cb = ctx.commandQueue.makeCommandBuffer() else {
            inFlight.signal()
            return true
        }
        let staged = encodeUploads(cb, uploads)
        let sem = inFlight
        cb.addCompletedHandler { _ in
            uploads?.recycle(staged)
            sem.signal()
        }
        guard let drawable = layer.nextDrawable() else {
            // No drawable (layer not ready): still land the uploads.
            cb.commit()
            return true
        }

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
               let scaled = Self.ensure(&tmpScaled, device: ctx.device, width: visW, height: visH) {
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
                    if let tx = Self.ensure(&tmpX, device: ctx.device, width: visW, height: rows) {
                        encode(cb, target: tx, viewport: (0, 0, visW, rows), clear: false,
                               pipeline: ctx.areaX, texture: tex, params: &params)
                        encode(cb, target: scaled, viewport: (0, 0, visW, visH), clear: false,
                               pipeline: ctx.areaY, texture: tx, params: &params)
                    } else {
                        ok = false
                    }
                } else {
                    encode(cb, target: scaled, viewport: (0, 0, visW, visH), clear: false,
                           pipeline: ctx.bilinear, texture: tex, params: &params)
                }
                if ok {
                    encode(cb, target: drawable.texture, viewport: (vx0, vy0, visW, visH), clear: true,
                           pipeline: ctx.sharpen, texture: scaled, params: &params)
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
        return true
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
