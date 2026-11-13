import Combine
import CoreGraphics
import Foundation

// MARK: - Screen + liveness models

/// Remote screen state, observed only by the screen view so frame / cursor
/// updates never re-evaluate the whole session body. Main thread only.
final class ScreenModel: ObservableObject {
    // Reassembled remote screen as a CGImage — CGImage fallback path only
    // (no Metal). Kept across reconnects so overlays dim the last frame.
    @Published var image: CGImage?
    // Bumped for every published frame, on both paths (0 = no frame yet:
    // the surface appears with the first one).
    @Published var frameTick: UInt64 = 0
    // The last frame's geometry — the space pointer coordinates normalize to.
    @Published var frameSize: CGSize = .zero
    // Remote cursor overlay: position in frame pixel space, and the
    // shape bitmap with its hotspot. nil shape = position-only (hidden).
    @Published var cursorPos: CGPoint?
    @Published var cursorImage: CGImage?
    @Published var cursorHotspot: CGSize = .zero
    @Published var cursorSize: CGSize = .zero
    // With Metal, frames skip CGImage entirely — the session queue
    // stages each frame's dirty rects straight from the Rust framebuffer
    // and the renderer blits them (not published: the surface drains it on
    // every redraw). nil = no Metal: the CGImage path above.
    let uploads: FrameUploadQueue? = MetalContext.shared.map { FrameUploadQueue(context: $0) }
    // The H.264 overlay for high-motion regions (not published —
    // the frame view hosts its layer; samples are enqueued directly).
    let video = VideoOverlay()
}

/// Liveness state, observed only by the stats chip and the
/// reconnecting capsule. Main thread only.
final class LivenessModel: ObservableObject {
    @Published var peerAlive = true
    @Published var rttMs: Int32 = -1
    @Published var fps: Float = 0
    /// "Relay" / "Probing…" / "Direct (LAN)" / "Direct
    /// (Internet)" for relay sessions; nil for direct UDP sessions.
    @Published var path: String?
}

/// Screen updates accumulated on the session queue and applied to
/// `ScreenModel` in one main-thread hop (latest wins).
struct PendingScreen {
    /// A new frame (`image` only on the CGImage fallback path).
    var frame: (image: CGImage?, size: CGSize)?
    var cursorPos: CGPoint?
    var cursorShape: (image: CGImage?, hotspot: CGSize, size: CGSize)?
    /// This frame is the lossless refresh after VIDEO_END of that
    /// video generation — hide the overlay together with showing it.
    var hideVideoGen: Int?

    var isEmpty: Bool { frame == nil && cursorPos == nil && cursorShape == nil && hideVideoGen == nil }
}
