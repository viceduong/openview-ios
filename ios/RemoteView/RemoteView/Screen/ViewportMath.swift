import CoreGraphics

/// Zoom-scale math for the remote-screen viewport, as pure functions so it
/// can be unit tested. Scales are view points per remote pixel.
enum ViewportMath {
    /// Max zoom: 2 view points per remote pixel (or fit when that is larger).
    static let maxZoomScale: CGFloat = 2
    /// `ZoomPreset.openDefault`: the remote screen's height fills this
    /// fraction of the view's height.
    static let openDefaultHeightFraction: CGFloat = 0.4
    /// Pinch release snaps to fit / fill / 1:1 within this ratio.
    static let snapTolerance: CGFloat = 0.2

    /// Minimum zoom: the whole frame inside `safe`,
    /// min(safe.w / frame.w, safe.h / frame.h); 0 when either is empty.
    static func fitScale(frame: CGSize, safe: CGRect) -> CGFloat {
        guard frame.width > 0, frame.height > 0, safe.width > 0, safe.height > 0 else { return 0 }
        return min(safe.width / frame.width, safe.height / frame.height)
    }

    /// The scale that fills `bounds` (cropped on one axis): the larger ratio.
    static func fillScale(frame: CGSize, bounds: CGSize) -> CGFloat {
        guard frame.width > 0, frame.height > 0, bounds.width > 0, bounds.height > 0 else { return 0 }
        return max(bounds.width / frame.width, bounds.height / frame.height)
    }

    /// `ZoomPreset.openDefault`: the frame's height fills 2/5 of the view.
    static func openDefaultScale(frame: CGSize, viewHeight: CGFloat) -> CGFloat {
        guard frame.height > 0, viewHeight > 0 else { return 0 }
        return viewHeight * openDefaultHeightFraction / frame.height
    }

    /// One remote pixel = one device pixel.
    static func nativeScale(displayScale: CGFloat) -> CGFloat {
        1 / (displayScale > 0 ? displayScale : 1)
    }

    /// Maximum zoom: max(2, fit).
    static func maxScale(fit: CGFloat) -> CGFloat {
        max(maxZoomScale, fit)
    }

    /// `s` clamped to the zoom range [fit, max(2, fit)]; a non-finite `s`
    /// falls back to fit.
    static func clamp(_ s: CGFloat, fit: CGFloat) -> CGFloat {
        guard s.isFinite else { return fit }
        return min(max(s, fit), maxScale(fit: fit))
    }

    /// The preset scale nearest to `scale` within `snapTolerance` (relative
    /// distance), or nil when none is close enough. `candidates` pairs each
    /// preset with its target scale; non-positive targets are skipped.
    static func snap<P>(scale: CGFloat, candidates: [(P, CGFloat)]) -> (preset: P, scale: CGFloat)? {
        guard scale > 0 else { return nil }
        var best: (preset: P, scale: CGFloat)?
        var bestDistance = CGFloat.greatestFiniteMagnitude
        for (preset, target) in candidates where target > 0 {
            let distance = abs(scale / target - 1)
            if distance <= snapTolerance && distance < bestDistance {
                bestDistance = distance
                best = (preset: preset, scale: target)
            }
        }
        return best
    }
}
