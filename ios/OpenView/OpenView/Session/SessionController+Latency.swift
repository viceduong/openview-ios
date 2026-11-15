import Foundation
import QuartzCore

// Latency telemetry side of SessionController: report presents to the Rust
// assembler and publish its per-stage breakdown to LivenessModel.

extension SessionController {
    /// How often the breakdown is read on the session queue.
    static let latencyPublishMs: Int64 = 500

    /// Session queue (from `tick`): read the rolling breakdown about twice
    /// a second and publish it when it changed.
    func publishLatency(nowMs: Int64) {
        guard nowMs - lastLatencyPublishMs >= Self.latencyPublishMs else { return }
        lastLatencyPublishMs = nowMs
        guard let lat = screen.latencyStats(), lat.frames > 0, lat != lastLatency else { return }
        lastLatency = lat
        DispatchQueue.main.async { [weak self] in
            self?.livenessModel.latency = lat
        }
    }

    /// Any thread: a picture drawn at `drawnAt` reached the screen at
    /// `presentedAt` (both CACurrentMediaTime). Hops to the session queue,
    /// where the assembler turns it into present samples.
    func notePresented(drawnAt: CFTimeInterval, presentedAt: CFTimeInterval) {
        queue.async { [weak self] in
            guard let self, !self.halted else { return }
            let now = CACurrentMediaTime()
            func ageUs(_ t: CFTimeInterval) -> Int64 {
                Int64((max(0, now - t) * 1_000_000).rounded())
            }
            self.screen.latencyPresented(drawnAgeUs: ageUs(drawnAt),
                                         presentedAgeUs: ageUs(presentedAt))
        }
    }
}
