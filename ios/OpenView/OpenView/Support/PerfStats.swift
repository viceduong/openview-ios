import Foundation
import QuartzCore

// Pacing diagnostics: cheap counters (one lock, a few adds per event, no
// per-frame logging) summarised into the DiagLog every 5 s while a session
// runs (`SessionController.logPerfIfDue`). Two halves, so network
// unevenness can be told apart from render stalls:
//
//   RenderStats  main thread + Metal completion threads: display-link
//                ticks, renders done / skipped (GPU busy), presents and the
//                gaps between them, GPU time per command buffer, uploads
//                applied per render, video pictures composited.
//   NetStats     session queue + VideoToolbox thread: channel-0 records
//                received (count, bytes, max inter-arrival gap), frames
//                delivered by Rust, video units received / decoded /
//                failed and decode time.

/// Render-side counters (any thread).
final class RenderStats {
    static let shared = RenderStats()

    private let lock = NSLock()
    private var ticks = 0
    private var renders = 0
    private var skippedBusy = 0
    private var presents = 0
    private var lastPresent: CFTimeInterval = 0
    /// Present-to-present gaps (ms) of this window, for the p95.
    private var gaps: [Double] = []
    private var maxGap: Double = 0
    private var gpuCount = 0
    private var gpuSumMs: Double = 0
    private var gpuMaxMs: Double = 0
    private var uploadRenders = 0
    private var uploadRegions = 0
    private var uploadBytes = 0
    private var videoComposites = 0
    private static let maxGaps = 2048

    init() {
        gaps.reserveCapacity(Self.maxGaps)
    }

    func displayTick() {
        lock.lock(); ticks += 1; lock.unlock()
    }

    func rendered() {
        lock.lock(); renders += 1; lock.unlock()
    }

    func skipped() {
        lock.lock(); skippedBusy += 1; lock.unlock()
    }

    /// A drawable reached the screen at `t` (CACurrentMediaTime clock).
    func presented(at t: CFTimeInterval) {
        guard t > 0 else { return }
        lock.lock()
        presents += 1
        if lastPresent > 0, t > lastPresent {
            let gap = (t - lastPresent) * 1000
            // Gaps over 1 s are idle time (nothing to draw), not jitter.
            if gap < 1000 {
                if gaps.count < Self.maxGaps { gaps.append(gap) }
                if gap > maxGap { maxGap = gap }
            }
        }
        lastPresent = t
        lock.unlock()
    }

    /// One command buffer's GPU time (gpuEndTime - gpuStartTime, seconds).
    func gpu(seconds: CFTimeInterval) {
        guard seconds.isFinite, seconds >= 0 else { return }
        let ms = seconds * 1000
        lock.lock()
        gpuCount += 1
        gpuSumMs += ms
        if ms > gpuMaxMs { gpuMaxMs = ms }
        lock.unlock()
    }

    /// One render applied `regions` staged rects of `bytes` in total.
    func uploads(regions: Int, bytes: Int) {
        guard regions > 0 else { return }
        lock.lock()
        uploadRenders += 1
        uploadRegions += regions
        uploadBytes += bytes
        lock.unlock()
    }

    func videoComposited() {
        lock.lock(); videoComposites += 1; lock.unlock()
    }

    /// The window's summary; the counters start over.
    func takeSummary() -> String {
        lock.lock()
        var g = gaps
        let line = (ticks, renders, skippedBusy, presents, maxGap, gpuCount, gpuSumMs, gpuMaxMs,
                    uploadRenders, uploadRegions, uploadBytes, videoComposites)
        ticks = 0; renders = 0; skippedBusy = 0; presents = 0
        gaps.removeAll(keepingCapacity: true)
        maxGap = 0
        gpuCount = 0; gpuSumMs = 0; gpuMaxMs = 0
        uploadRenders = 0; uploadRegions = 0; uploadBytes = 0
        videoComposites = 0
        lock.unlock()
        var p95: Double = 0
        if !g.isEmpty {
            g.sort()
            p95 = g[min(g.count - 1, Int(Double(g.count) * 0.95))]
        }
        let gpuAvg = line.5 > 0 ? line.6 / Double(line.5) : 0
        let perRender = line.8 > 0 ? Double(line.9) / Double(line.8) : 0
        let kbPerRender = line.8 > 0 ? Double(line.10) / Double(line.8) / 1024 : 0
        return "render ticks=\(line.0) renders=\(line.1) skippedBusy=\(line.2) presents=\(line.3)"
            + " gapMax=\(fmt(line.4))ms gapP95=\(fmt(p95))ms"
            + " gpu avg=\(fmt(gpuAvg))ms max=\(fmt(line.7))ms"
            + " uploads renders=\(line.8) rects/render=\(fmt(perRender)) KB/render=\(fmt(kbPerRender))"
            + " videoComposites=\(line.11)"
    }
}

/// Session-side counters (session queue, VideoToolbox thread).
final class NetStats {
    private let lock = NSLock()
    private var records = 0
    private var recordBytes = 0
    private var lastRecord: CFTimeInterval = 0
    private var maxGap: Double = 0
    private var frames = 0
    private var videoUnits = 0
    private var videoDecoded = 0
    private var videoErrors = 0
    private var decodeSumMs: Double = 0
    private var decodeMaxMs: Double = 0
    private var lastDropped: UInt64 = 0

    /// One channel-0 record arrived (session queue).
    func record(bytes: Int) {
        let now = CACurrentMediaTime()
        lock.lock()
        records += 1
        recordBytes += bytes
        if lastRecord > 0 {
            let gap = (now - lastRecord) * 1000
            if gap > maxGap { maxGap = gap }
        }
        lastRecord = now
        lock.unlock()
    }

    /// Rust delivered a reassembled frame to the frame sink.
    func frame() {
        lock.lock(); frames += 1; lock.unlock()
    }

    func videoUnit() {
        lock.lock(); videoUnits += 1; lock.unlock()
    }

    /// VideoToolbox returned a picture `ms` after the unit was submitted.
    func videoDecoded(ms: Double) {
        lock.lock()
        videoDecoded += 1
        decodeSumMs += ms
        if ms > decodeMaxMs { decodeMaxMs = ms }
        lock.unlock()
    }

    func videoError() {
        lock.lock(); videoErrors += 1; lock.unlock()
    }

    /// The window's summary over `seconds`; `droppedTotal` = the frame
    /// queue's running newest-wins drop count. The counters start over.
    func takeSummary(seconds: Double, droppedTotal: UInt64) -> String {
        lock.lock()
        let r = records, b = recordBytes, gap = maxGap, f = frames
        let vu = videoUnits, vd = videoDecoded, ve = videoErrors
        let dAvg = vd > 0 ? decodeSumMs / Double(vd) : 0, dMax = decodeMaxMs
        let dropped = droppedTotal >= lastDropped ? droppedTotal - lastDropped : droppedTotal
        lastDropped = droppedTotal
        records = 0; recordBytes = 0; maxGap = 0; frames = 0
        videoUnits = 0; videoDecoded = 0; videoErrors = 0
        decodeSumMs = 0; decodeMaxMs = 0
        lock.unlock()
        let s = max(seconds, 0.001)
        return "net records=\(r) (\(fmt(Double(r) / s))/s) KB/s=\(fmt(Double(b) / 1024 / s))"
            + " maxGap=\(fmt(gap))ms frames=\(f) (\(fmt(Double(f) / s))/s)"
            + " | video units=\(vu) decoded=\(vd) errors=\(ve) dropped=\(dropped)"
            + " decode avg=\(fmt(dAvg))ms max=\(fmt(dMax))ms"
    }

    func reset() {
        lock.lock()
        records = 0; recordBytes = 0; lastRecord = 0; maxGap = 0; frames = 0
        videoUnits = 0; videoDecoded = 0; videoErrors = 0
        decodeSumMs = 0; decodeMaxMs = 0
        lock.unlock()
    }
}

private func fmt(_ v: Double) -> String {
    String(format: "%.1f", v)
}
