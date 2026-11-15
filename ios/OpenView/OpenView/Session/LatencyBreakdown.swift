import SwiftUI

// Per-stage latency telemetry (CAP_TIMING hosts): the Rust side joins the
// host's capture / encode / send stamps with reassembly, delivery and the
// present this shell reports (ov_screen_latency_*), and keeps rolling
// p50 / p95 per stage. This is the published, display-ready copy.

/// Rolling latency per stage, in milliseconds (nil = no data yet).
struct LatencyBreakdown: Equatable {
    struct Stage: Equatable {
        let label: String
        let p50Ms: Double?
        let p95Ms: Double?
    }

    /// cap, enc, send, net, dec, pres — in path order.
    let stages: [Stage]
    /// Host capture -> presented here (needs the clock offset).
    let total: Stage
    /// Updates measured so far.
    let frames: UInt64

    /// "12" / "3.4" / "?" (one decimal below 10 ms).
    static func format(_ ms: Double?) -> String {
        guard let ms else { return "?" }
        return ms < 9.95 ? String(format: "%.1f", ms) : String(format: "%.0f", ms)
    }

    /// The compact line: "cap 3.0 · enc 4.0 · … = 29 ms" (medians).
    var line: String {
        let parts = stages.map { "\($0.label) \(Self.format($0.p50Ms))" }
        return parts.joined(separator: " · ") + " = \(Self.format(total.p50Ms)) ms"
    }
}

/// Session-info rows: end-to-end latency plus the per-stage medians /
/// 95th percentiles. Shows nothing until a timing-capable host's first
/// update was measured.
struct LatencyRows: View {
    @ObservedObject var liveness: LivenessModel

    private static let names: [String: String] = [
        "cap": "Capture",
        "enc": "Encode",
        "send": "Send queue",
        "net": "Network",
        "dec": "Decode",
        "pres": "Present",
    ]

    var body: some View {
        if let lat = liveness.latency {
            row("End-to-end latency", lat.total, bold: true)
            ForEach(lat.stages, id: \.label) { stage in
                row(Self.names[stage.label] ?? stage.label, stage, bold: false)
            }
        }
    }

    private func row(_ title: String, _ stage: LatencyBreakdown.Stage, bold: Bool) -> some View {
        HStack {
            Text(title)
                .font(bold ? .body : .subheadline)
                .foregroundColor(bold ? .primary : .secondary)
            Spacer()
            Text("\(LatencyBreakdown.format(stage.p50Ms)) ms")
                .font(.body.monospacedDigit())
            Text("p95 \(LatencyBreakdown.format(stage.p95Ms))")
                .font(.caption.monospacedDigit())
                .foregroundColor(.secondary)
                .frame(minWidth: 56, alignment: .trailing)
        }
        .accessibilityElement(children: .combine)
    }
}
