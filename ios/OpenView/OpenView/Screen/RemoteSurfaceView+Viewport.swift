import UIKit

// Viewport of RemoteSurfaceView: safe area / keyboard geometry, zoom
// presets, the cursor-following offset and its easing. The scale math
// itself lives in ViewportMath (pure, unit tested).

extension RemoteSurfaceView {
    // MARK: Viewport

    var displayScale: CGFloat {
        let s = traitCollection.displayScale
        return s > 0 ? s : UIScreen.main.scale
    }

    /// Gap between the stats chip and the top of the remote image.
    static let topChromeGap: CGFloat = 6

    /// The part of the view outside the window's safe area insets (notch,
    /// home indicator, status bar), in view coordinates. Computed from the
    /// window (the SwiftUI host lays this view out ignoring the safe area,
    /// and what it reports as the view's own safeAreaInsets is not
    /// guaranteed); zero outside a window. While the stats chip is shown
    /// the top inset reaches below it (+ `topChromeGap`): the image at fit /
    /// pinned to the top starts under the chip, not behind it.
    var safeInsets: UIEdgeInsets {
        guard let window, bounds.width > 0, bounds.height > 0 else { return .zero }
        let wi = window.safeAreaInsets
        let wb = window.bounds
        let f = convert(bounds, to: window)
        var top = max(0, wi.top - f.minY)
        if topChromeBottom > 0 {
            top = max(top, topChromeBottom + Self.topChromeGap - f.minY)
        }
        return UIEdgeInsets(top: top,
                            left: max(0, wi.left - f.minX),
                            bottom: max(0, f.maxY - (wb.maxY - wi.bottom)),
                            right: max(0, f.maxX - (wb.maxX - wi.right)))
    }

    /// The safe part of the view (the whole view if the insets leave
    /// nothing).
    var safeRect: CGRect {
        let r = bounds.inset(by: safeInsets)
        return r.width >= 1 && r.height >= 1 ? r : bounds
    }

    /// View y of the keyboard's top edge while the docked keyboard (or the
    /// key bar alone) covers the bottom of this view, capped to the safe
    /// area's bottom; nil when no keyboard covers the view (hidden, floating
    /// / undocked iPad keyboard, outside a window) or when it would leave
    /// less than 1 pt below the top inset (nothing sensible to follow).
    var keyboardVisibleBottom: CGFloat? {
        guard let kb = keyboardScreenFrame, let window,
              bounds.width > 0, bounds.height > 0 else { return nil }
        let local = convert(kb, from: window.screen.coordinateSpace)
        // Docked: reaches the bottom of this view and overlaps it.
        guard local.maxY >= bounds.maxY - 1, local.minY < bounds.maxY,
              local.maxX > bounds.minX, local.minX < bounds.maxX else { return nil }
        let safe = safeRect
        let bottom = min(safe.maxY, max(local.minY, bounds.minY))
        guard bottom - safe.minY >= 1 else { return nil }
        return bottom
    }

    /// The part of the view the remote image is placed in for viewport
    /// purposes: the safe rect, minus what the keyboard covers.
    var visibleRect: CGRect {
        let safe = safeRect
        guard let bottom = keyboardVisibleBottom else { return safe }
        return CGRect(x: safe.minX, y: safe.minY, width: safe.width, height: bottom - safe.minY)
    }

    /// Minimum zoom: the whole frame inside the safe area.
    var fitScale: CGFloat {
        guard bounds.width > 0, bounds.height > 0 else { return 0 }
        return ViewportMath.fitScale(frame: frameSize, safe: safeRect)
    }

    /// Scale that fills the view (cropped on one axis).
    var fillScale: CGFloat {
        ViewportMath.fillScale(frame: frameSize, bounds: bounds.size)
    }

    /// The remote screen's height fills 2/5 of the view's height.
    var openDefaultScale: CGFloat {
        ViewportMath.openDefaultScale(frame: frameSize, viewHeight: bounds.height)
    }

    /// One remote pixel = one device pixel.
    var nativeScale: CGFloat {
        ViewportMath.nativeScale(displayScale: displayScale)
    }

    func clampScale(_ s: CGFloat) -> CGFloat {
        ViewportMath.clamp(s, fit: fitScale)
    }

    /// Fit / fill / 1:1 / open default, each capped to the zoom range (1:1
    /// above max(2, fit) becomes the max).
    func scaleFor(_ preset: ZoomPreset) -> CGFloat {
        switch preset {
        case .fit: return fitScale
        case .fill: return clampScale(fillScale)
        case .native: return clampScale(nativeScale)
        case .openDefault: return clampScale(openDefaultScale)
        }
    }

    /// The content fits the safe area vertically at the current zoom: it is
    /// pinned to the top of the safe area (layout: the screen at
    /// the top, the free space below it), whatever the cursor does.
    var pinsToTop: Bool {
        frameSize.height * scale <= safeRect.height + 0.5 // tolerance: fit is computed from it
    }

    /// `p` (a frame-origin offset) with the content kept in view, per axis:
    /// where the content fits the safe area, horizontally centred and
    /// vertically at the top of the safe area (below the stats
    /// chip); where it is larger than the view, never scrolled
    /// past its edges (the image always covers the view, no black band);
    /// in between, kept inside the view.
    /// While the keyboard covers the bottom of the view, the image may also
    /// scroll up until its bottom edge meets the keyboard's top edge (never
    /// further: no black band above the keyboard); at fit it rests pinned
    /// to the top unless the cursor follow scrolls it.
    func clamped(_ p: CGPoint) -> CGPoint {
        let cw = frameSize.width * scale
        let ch = frameSize.height * scale
        let safe = safeRect
        let kbBottom = keyboardVisibleBottom
        var q = p
        if cw <= safe.width + 0.5 { // tolerance: fit is computed from it
            q.x = safe.minX + (safe.width - cw) / 2
        } else {
            let lo = min(0, bounds.width - cw)
            let hi = max(0, bounds.width - cw)
            q.x = min(max(p.x, lo), hi)
        }
        if ch <= safe.height + 0.5 {
            if let kb = kbBottom, ch > kb - safe.minY {
                // Taller than the area above the keyboard: from pinned to
                // the top down to its bottom edge at the keyboard's top.
                q.y = min(max(p.y, kb - ch), safe.minY)
            } else {
                q.y = safe.minY
            }
        } else {
            // The bottom limit is the keyboard's top edge while it is up.
            let limit = kbBottom ?? bounds.height
            let lo = min(0, limit - ch)
            let hi = max(0, limit - ch)
            q.y = min(max(p.y, lo), hi)
        }
        return q
    }

    /// Zoomed in beyond fit: the viewport follows the cursor, centring it
    /// as far as the image edges allow (`viewportTarget`). Near an edge the
    /// view stops at the edge and the cursor moves off-centre towards it;
    /// centring resumes once the cursor is back far enough inside.
    var centresCursor: Bool {
        guard scale > 0, input.hasCursor else { return false }
        let f = fitScale
        return f > 0 && scale > f * 1.001
    }

    /// The offset that puts the predicted cursor at the view's centre
    /// (vertically: the centre of the area above the keyboard while it is
    /// up).
    var cursorCentredOffset: CGPoint {
        let c = input.cursor
        let midY = keyboardVisibleBottom != nil ? visibleRect.midY : bounds.midY
        return CGPoint(x: bounds.midX - c.x * scale, y: midY - c.y * scale)
    }

    /// Keyboard up: `p` moved vertically just enough that the cursor lies
    /// inside the visible rect (above the keyboard), `keyboardFollowMargin`
    /// from its top / bottom edges. Unchanged without a keyboard / cursor.
    func keyboardFollowed(_ p: CGPoint) -> CGPoint {
        guard keyboardVisibleBottom != nil, input.hasCursor, scale > 0 else { return p }
        let vis = visibleRect
        let m = min(Self.keyboardFollowMargin, vis.height / 4)
        let y = p.y + input.cursor.y * scale
        var q = p
        if y > vis.maxY - m {
            q.y -= y - (vis.maxY - m)
        } else if y < vis.minY + m {
            q.y += (vis.minY + m) - y
        }
        return q
    }

    /// Where the viewport belongs at the current zoom: zoomed in, the
    /// cursor-centred offset clamped to the image edges (a continuous
    /// function of the cursor, so following never jumps or oscillates);
    /// else the current offset clamped (keyboard up: scrolled just enough
    /// to keep the cursor above the keyboard). On an axis where the image
    /// is no larger than the safe area the clamp keeps it centred / at the
    /// top (no jump away from the top on a small zoom).
    func viewportTarget() -> CGPoint {
        clamped(centresCursor ? cursorCentredOffset : keyboardFollowed(offset))
    }

    /// Place the viewport for the current zoom (`viewportTarget`).
    /// Viewport jumps (zoom, resync, leaving / reaching fit) are eased when
    /// `animated` or while an easing is already running, else immediate.
    /// `timeConstant` (keyboard transitions) sets the easing speed; a new
    /// easing started without one uses `followTimeConstant`.
    func settleViewport(animated: Bool, timeConstant: Double? = nil) {
        guard scale > 0 else { return }
        let target = viewportTarget()
        if animated || followTarget != nil {
            if let timeConstant {
                followTimeConstantOverride = timeConstant
            } else if followTarget == nil {
                followTimeConstantOverride = nil
            }
            followTarget = target
            if !animTicker.isRunning {
                lastAnimTime = 0
                animTicker.start()
            }
            return
        }
        followTarget = nil
        if target != offset {
            offset = target
            applyViewport()
        }
    }

    /// After the cursor moved locally (trackpad, pointer lock).
    func followCursorAfterMove() {
        if centresCursor || followTarget != nil || keyboardVisibleBottom != nil {
            settleViewport(animated: false)
        }
        updateCursorLayer()
    }

    /// The host moved the cursor (resync after local input, or someone at
    /// the host): zoomed in, the viewport eases over only when the cursor
    /// left the middle 80% of the view (no hover feedback loop on iPad) and
    /// the clamped target actually differs (a cursor parked near an image
    /// edge, where the view already stops, starts no easing).
    /// Keyboard up: the same, vertically within the area above the keyboard
    /// (also at fit, where the image may scroll up to the keyboard edge).
    func followHostCursor() {
        let keyboardUp = keyboardVisibleBottom != nil
        guard scale > 0, input.hasCursor, centresCursor || keyboardUp else { return }
        let v = frameToView(input.cursor)
        let vis = keyboardUp ? visibleRect : bounds
        let innerX = bounds.insetBy(dx: bounds.width * 0.1, dy: 0)
        let innerY = vis.insetBy(dx: 0, dy: vis.height * 0.1)
        // A top-pinned axis never follows (unless the keyboard is up): only
        // the horizontal position counts then.
        let outX = v.x < innerX.minX || v.x > innerX.maxX
        let outY = (!pinsToTop || keyboardUp) && (v.y < innerY.minY || v.y > innerY.maxY)
        guard outX || outY else { return }
        let target = viewportTarget()
        if followTarget != nil || abs(target.x - offset.x) > 0.5 || abs(target.y - offset.y) > 0.5 {
            settleViewport(animated: true)
        }
    }

    /// One easing step towards `followTarget` (re-clamped, so it stays
    /// inside the image edges); false when done.
    func stepFollow(_ now: CFTimeInterval) -> Bool {
        guard let rawTarget = followTarget else { return false }
        guard scale > 0 else {
            followTarget = nil
            return false
        }
        let target = clamped(rawTarget)
        let dt = lastAnimTime > 0 ? min(max(now - lastAnimTime, 0), 0.1) : 1.0 / 60.0
        let tc = followTimeConstantOverride ?? Self.followTimeConstant
        let k = CGFloat(1 - exp(-dt / tc))
        offset.x += (target.x - offset.x) * k
        offset.y += (target.y - offset.y) * k
        if abs(target.x - offset.x) < 0.25 && abs(target.y - offset.y) < 0.25 {
            offset = target
            followTarget = nil
            followTimeConstantOverride = nil
        }
        applyViewport()
        return followTarget != nil
    }

    /// View or frame size changed (rotation, keyboard, monitor switch):
    /// re-apply the followed preset, else keep the zoom (clamped).
    func refit() {
        let f = fitScale
        guard f > 0 else { return }
        if scale <= 0 {
            followPreset = .openDefault
        }
        if let preset = followPreset {
            scale = scaleFor(preset)
        } else {
            scale = clampScale(scale)
        }
        followTarget = nil
        settleViewport(animated: false)
        // Centred mode with an unchanged offset skips applyViewport: the
        // scale may still have changed.
        applyViewport()
        publishFitImageFrame()
    }

    /// Tell the chrome where the image sits at fit (window coordinates:
    /// centred horizontally, top of the safe area), so the toolbar can dock
    /// in the free space below it. Independent of the current zoom, so the
    /// toolbar does not move while zooming / panning. Published on the next
    /// main-queue turn: refit runs inside SwiftUI view updates
    /// (updateUIView -> setFrame), where publishing is not allowed.
    func publishFitImageFrame() {
        let f = fitScale
        guard f > 0, window != nil else { return }
        let safe = safeRect
        let w = frameSize.width * f
        let h = frameSize.height * f
        let local = CGRect(x: safe.minX + (safe.width - w) / 2, y: safe.minY, width: w, height: h)
        let r = convert(local, to: nil).integral
        DispatchQueue.main.async { [weak self] in
            guard let self, self.input.activeSurface === self,
                  self.input.fitImageFrame != r else { return }
            self.input.fitImageFrame = r
        }
    }

    func applyViewport() {
        guard scale > 0 else { return }
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        container.position = offset
        container.transform = CATransform3DMakeScale(scale, scale, 1)
        // Crisp pixels when strongly magnified, smooth otherwise.
        frameLayer.magnificationFilter = scale * displayScale >= 3 ? .nearest : .linear
        CATransaction.commit()
        updateCursorLayer()
        setNeedsRender()
    }

    /// Zoom by `factor` keeping the view point `pivot` fixed.
    func zoom(by factor: CGFloat, pivot: CGPoint) {
        guard scale > 0, factor.isFinite, factor > 0 else { return }
        let newScale = clampScale(scale * factor)
        let fx = (pivot.x - offset.x) / scale
        let fy = (pivot.y - offset.y) / scale
        scale = newScale
        offset = CGPoint(x: pivot.x - fx * newScale, y: pivot.y - fy * newScale)
        followPreset = abs(newScale - fitScale) < 0.0001 ? .fit : nil
        // Left unclamped: the caller eases it (settleViewport) to the
        // cursor-centred or clamped offset.
        applyViewport()
    }

    /// Menu command: fit / fill / 1:1, centered on the cursor (or on what
    /// is in the middle of the view before the cursor is known).
    func applyPreset(_ preset: ZoomPreset) {
        guard scale > 0, fitScale > 0 else { return }
        let center = CGPoint(x: bounds.midX, y: bounds.midY)
        let fp = input.hasCursor ? input.cursor : viewToFrame(center)
        scale = scaleFor(preset)
        followPreset = preset
        offset = CGPoint(x: center.x - fp.x * scale, y: center.y - fp.y * scale)
        followTarget = nil
        settleViewport(animated: false)
        applyViewport()
    }

    /// Pinch released: snap to fit / fill / 1:1 when within +-20%.
    func snapZoom(pivot: CGPoint) {
        guard scale > 0 else { return }
        let presets: [ZoomPreset] = [.fit, .fill, .native]
        let candidates = presets.map { ($0, scaleFor($0)) }
        guard let best = ViewportMath.snap(scale: scale, candidates: candidates) else { return }
        zoom(by: best.scale / scale, pivot: pivot)
        followPreset = best.preset
    }

    /// View point -> frame pixel, clamped to the frame.
    func viewToFrame(_ p: CGPoint) -> CGPoint {
        guard scale > 0, frameSize.width > 0, frameSize.height > 0 else { return .zero }
        let x = (p.x - offset.x) / scale
        let y = (p.y - offset.y) / scale
        return CGPoint(x: min(max(x, 0), frameSize.width - 1),
                       y: min(max(y, 0), frameSize.height - 1))
    }

    /// Frame pixel -> view point.
    func frameToView(_ p: CGPoint) -> CGPoint {
        CGPoint(x: offset.x + p.x * scale, y: offset.y + p.y * scale)
    }
}
