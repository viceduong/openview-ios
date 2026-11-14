import SwiftUI
import UIKit

// The collapsed toolbar handle while the soft keyboard / key bar is up.
//
// It is a UIKit view constrained to `keyboardLayoutGuide`, so UIKit moves it
// inside the keyboard's own animation, in lockstep with the keyboard and its
// key row (the guide includes the input accessory view). A SwiftUI position
// fed by keyboard notifications lagged behind the key row: iOS may report a
// second frame (the key row joining) mid-flight, and a retargeted SwiftUI
// spring does not follow UIKit's additive keyboard animation.
//
// The overlay is installed once, while the toolbar is on screen, so its
// guide already exists when the keyboard starts to move (a guide created
// mid-animation would only jump to the end position). WHEN the handle shows
// stays SwiftUI's call (`KeyboardTracker.up`, collapsed state): it only
// toggles the overlay's visibility.
//
// The overlay sits right above the session's hosting view (a sibling, not a
// SwiftUI platform view), so plain UIKit hit testing applies: it takes
// touches only on the visible handle, everything else falls through.

/// Invisible SwiftUI placeholder that installs / drives the overlay.
struct KeyboardHandleAnchor: UIViewRepresentable {
    /// Show the handle (keyboard up and the toolbar collapsed).
    let visible: Bool
    let faded: Bool
    let onTap: () -> Void
    let onPoke: () -> Void
    /// Drag ended: the handle centre as a fraction (x, y) of the safe area.
    let onDragEnded: (CGFloat, CGFloat) -> Void

    func makeUIView(context: Context) -> KeyboardHandleInstallerView {
        KeyboardHandleInstallerView()
    }

    func updateUIView(_ uiView: KeyboardHandleInstallerView, context: Context) {
        let overlay = uiView.overlay
        overlay.onTap = onTap
        overlay.onPoke = onPoke
        overlay.onDragEnded = onDragEnded
        overlay.setHandle(visible: visible, faded: faded)
    }

    static func dismantleUIView(_ uiView: KeyboardHandleInstallerView, coordinator: ()) {
        uiView.uninstall()
    }
}

/// Zero-size, non-interactive view inside the SwiftUI tree. While it is in a
/// window it keeps `overlay` installed above the hosting controller's view.
final class KeyboardHandleInstallerView: UIView {
    let overlay = KeyboardHandleOverlay()

    override init(frame: CGRect) {
        super.init(frame: frame)
        backgroundColor = .clear
        isUserInteractionEnabled = false
        isAccessibilityElement = false
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    override func didMoveToWindow() {
        super.didMoveToWindow()
        if window != nil {
            install()
        } else {
            uninstall()
        }
    }

    /// Insert the overlay right above the nearest view controller's root
    /// view (the session's hosting view), pinned to it. Falls back to the
    /// window.
    private func install() {
        guard let window else { return }
        var host: UIView?
        var current: UIView? = superview
        while let view = current {
            if let vc = view.next as? UIViewController, vc.view === view {
                host = view
                break
            }
            current = view.superview
        }
        let reference: UIView
        let container: UIView
        if let host, let parent = host.superview {
            reference = host
            container = parent
        } else {
            reference = window
            container = window
        }
        if overlay.superview === container { return }
        overlay.removeFromSuperview()
        overlay.translatesAutoresizingMaskIntoConstraints = false
        if reference === container {
            container.addSubview(overlay)
        } else {
            container.insertSubview(overlay, aboveSubview: reference)
        }
        NSLayoutConstraint.activate([
            overlay.leadingAnchor.constraint(equalTo: reference.leadingAnchor),
            overlay.trailingAnchor.constraint(equalTo: reference.trailingAnchor),
            overlay.topAnchor.constraint(equalTo: reference.topAnchor),
            overlay.bottomAnchor.constraint(equalTo: reference.bottomAnchor),
        ])
    }

    func uninstall() {
        overlay.removeFromSuperview()
    }
}

/// Full-size, transparent overlay holding the keyboard handle. Only the
/// visible handle takes touches.
final class KeyboardHandleOverlay: UIView {
    var onTap: (() -> Void)?
    var onPoke: (() -> Void)?
    var onDragEnded: ((CGFloat, CGFloat) -> Void)?

    /// Same geometry as FloatingToolbar's SwiftUI handle (44 pt wide touch
    /// target, 26 x 64 capsule) and its 8 pt gap above the keyboard.
    private static let width: CGFloat = 44
    private static let height: CGFloat = 64
    private static let capsuleWidth: CGFloat = 26
    private static let keyboardGap: CGFloat = 8
    /// Guide vs keyboard-notification disagreement (pt) that switches to
    /// the notification-driven fallback (less than the 46 pt key row, so a
    /// guide ignoring the bar alone is caught too).
    private static let guideTolerance: CGFloat = 24

    private let handle = UIView()
    private var wantsVisible = false
    private var isFaded = false
    private var built = false
    /// handle.bottom == keyboardLayoutGuide.top - gap (the normal path).
    private var guideConstraint: NSLayoutConstraint?
    /// handle.bottom == bottom - keyboard height - gap (fallback only).
    private var manualConstraint: NSLayoutConstraint?
    /// The guide disagreed with the keyboard notifications: follow the
    /// notifications instead (UIKit animation with the keyboard's curve).
    private var usesManualTracking = false
    /// The latest keyboard frame from a notification (screen coordinates;
    /// nil when hidden).
    private var lastKeyboardFrame: CGRect?
    private var checkWork: DispatchWorkItem?

    init() {
        super.init(frame: .zero)
        backgroundColor = .clear
        // The session chrome is always dark (SessionHostingController).
        overrideUserInterfaceStyle = .dark
        buildHandle()
        let center = NotificationCenter.default
        // Selector observers run synchronously inside the keyboard's
        // animation setup (a block observer on a queue may not).
        center.addObserver(self, selector: #selector(keyboardWillChangeFrame(_:)),
                           name: UIResponder.keyboardWillChangeFrameNotification, object: nil)
        center.addObserver(self, selector: #selector(keyboardWillHide(_:)),
                           name: UIResponder.keyboardWillHideNotification, object: nil)
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    deinit {
        NotificationCenter.default.removeObserver(self)
        checkWork?.cancel()
    }

    // MARK: Hit testing

    /// Transparent to touches except on the (visible) handle.
    override func hitTest(_ point: CGPoint, with event: UIEvent?) -> UIView? {
        let hit = super.hitTest(point, with: event)
        return hit === self ? nil : hit
    }

    // MARK: Visibility

    func setHandle(visible: Bool, faded: Bool) {
        guard visible != wantsVisible || faded != isFaded else { return }
        wantsVisible = visible
        isFaded = faded
        handle.isUserInteractionEnabled = visible
        handle.accessibilityElementsHidden = !visible
        let target: CGFloat = visible ? (faded ? CGFloat(SessionChrome.fadedOpacity) : 1) : 0
        // Alpha only: the position keeps riding the keyboard animation.
        UIView.animate(withDuration: 0.2, delay: 0,
                       options: [.beginFromCurrentState, .allowUserInteraction, .curveEaseInOut]) {
            self.handle.alpha = target
        }
    }

    // MARK: Layout

    override func didMoveToWindow() {
        super.didMoveToWindow()
        guard window != nil, !built else { return }
        built = true
        let guide = keyboardLayoutGuide
        // A floating / undocked iPad keyboard: the guide stays at the
        // bottom, so the handle sits bottom-trailing.
        guide.followsUndockedKeyboard = false
        let safe = safeAreaLayoutGuide

        let follow = handle.bottomAnchor.constraint(equalTo: guide.topAnchor, constant: -Self.keyboardGap)
        follow.priority = UILayoutPriority(999)
        let manual = handle.bottomAnchor.constraint(equalTo: safe.bottomAnchor, constant: -Self.keyboardGap)
        manual.priority = UILayoutPriority(999)
        guideConstraint = follow
        manualConstraint = manual

        NSLayoutConstraint.activate([
            handle.widthAnchor.constraint(equalToConstant: Self.width),
            handle.heightAnchor.constraint(equalToConstant: Self.height),
            // The right edge, like the SwiftUI handle (chevron points left).
            handle.rightAnchor.constraint(equalTo: safe.rightAnchor),
            // Never off screen, whatever the guide says.
            handle.topAnchor.constraint(greaterThanOrEqualTo: safe.topAnchor, constant: 4),
            handle.bottomAnchor.constraint(lessThanOrEqualTo: safe.bottomAnchor, constant: -Self.keyboardGap),
            follow,
        ])
    }

    @objc private func keyboardWillChangeFrame(_ note: Notification) {
        let end = (note.userInfo?[UIResponder.keyboardFrameEndUserInfoKey] as? NSValue)?.cgRectValue
        if let end, !end.isNull, !end.isEmpty {
            lastKeyboardFrame = end
        } else {
            lastKeyboardFrame = nil
        }
        keyboardMoved(note)
    }

    @objc private func keyboardWillHide(_ note: Notification) {
        lastKeyboardFrame = nil
        keyboardMoved(note)
    }

    /// The keyboard is moving. The guide updates itself; laying out here,
    /// inside an animation with the keyboard's duration and curve, makes
    /// sure the handle animates with it even if the guide only changed its
    /// constraint. (A no-op when the guide already laid out.)
    private func keyboardMoved(_ note: Notification) {
        guard built, window != nil else { return }
        if usesManualTracking {
            manualConstraint?.constant = -(manualKeyboardHeight() + Self.keyboardGap)
        }
        let info = note.userInfo ?? [:]
        let duration = (info[UIResponder.keyboardAnimationDurationUserInfoKey] as? NSNumber)?.doubleValue ?? 0
        let curve = (info[UIResponder.keyboardAnimationCurveUserInfoKey] as? NSNumber)?.uintValue ?? 7
        if duration > 0 {
            UIView.animate(withDuration: duration, delay: 0,
                           options: [UIView.AnimationOptions(rawValue: curve << 16), .allowUserInteraction]) {
                self.layoutIfNeeded()
            }
        } else {
            layoutIfNeeded()
        }
        scheduleGuideCheck(after: duration)
    }

    // MARK: Fallback

    /// The docked keyboard's top edge in this view (nil: hidden, undocked
    /// or floating — the handle then sits at the bottom).
    private func dockedKeyboardTop() -> CGFloat? {
        guard let frame = lastKeyboardFrame, window != nil else { return nil }
        let local = convert(frame, from: UIScreen.main.coordinateSpace)
        // Docked: reaches this view's bottom edge and starts above it.
        guard local.maxY >= bounds.maxY - 1, local.minY < bounds.maxY - 1 else { return nil }
        return max(local.minY, bounds.minY)
    }

    /// Keyboard height above the safe-area bottom, for the fallback
    /// constraint (pinned to the safe-area bottom).
    private func manualKeyboardHeight() -> CGFloat {
        guard let top = dockedKeyboardTop() else { return 0 }
        let safeBottom = bounds.maxY - safeAreaInsets.bottom
        return max(safeBottom - top, 0)
    }

    /// Once the keyboard settled, compare the guide with the notification
    /// frame. Should the guide be wrong (it would leave the handle behind
    /// the keyboard), switch to the notification-driven constraint for good.
    private func scheduleGuideCheck(after duration: Double) {
        checkWork?.cancel()
        guard !usesManualTracking else { return }
        let work = DispatchWorkItem { [weak self] in
            self?.checkGuide()
        }
        checkWork = work
        DispatchQueue.main.asyncAfter(deadline: .now() + max(duration, 0.25) + 0.3, execute: work)
    }

    private func checkGuide() {
        guard !usesManualTracking, built, window != nil, let top = dockedKeyboardTop() else { return }
        let guideTop = keyboardLayoutGuide.layoutFrame.minY
        guard abs(guideTop - top) > Self.guideTolerance else { return }
        usesManualTracking = true
        guideConstraint?.isActive = false
        manualConstraint?.constant = -(manualKeyboardHeight() + Self.keyboardGap)
        manualConstraint?.isActive = true
        UIView.animate(withDuration: 0.25, delay: 0, options: [.beginFromCurrentState, .allowUserInteraction]) {
            self.layoutIfNeeded()
        }
    }

    // MARK: Handle view

    private func buildHandle() {
        handle.translatesAutoresizingMaskIntoConstraints = false
        handle.backgroundColor = .clear
        handle.alpha = 0
        handle.isUserInteractionEnabled = false
        handle.isAccessibilityElement = true
        handle.accessibilityElementsHidden = true
        handle.accessibilityLabel = "Show toolbar"
        handle.accessibilityTraits = .button
        addSubview(handle)

        let capsule = UIVisualEffectView(effect: UIBlurEffect(style: .systemUltraThinMaterial))
        capsule.translatesAutoresizingMaskIntoConstraints = false
        capsule.isUserInteractionEnabled = false
        capsule.layer.cornerRadius = Self.capsuleWidth / 2
        capsule.layer.masksToBounds = true
        capsule.layer.borderWidth = 0.5
        capsule.layer.borderColor = UIColor(white: 1, alpha: 0.15).cgColor
        handle.addSubview(capsule)

        let symbol = UIImage.SymbolConfiguration(pointSize: 22, weight: .bold)
        let chevron = UIImageView(image: UIImage(systemName: "chevron.compact.left", withConfiguration: symbol))
        chevron.translatesAutoresizingMaskIntoConstraints = false
        chevron.tintColor = .white
        chevron.contentMode = .center
        capsule.contentView.addSubview(chevron)

        NSLayoutConstraint.activate([
            capsule.centerXAnchor.constraint(equalTo: handle.centerXAnchor),
            capsule.centerYAnchor.constraint(equalTo: handle.centerYAnchor),
            capsule.widthAnchor.constraint(equalToConstant: Self.capsuleWidth),
            capsule.heightAnchor.constraint(equalTo: handle.heightAnchor),
            chevron.centerXAnchor.constraint(equalTo: capsule.contentView.centerXAnchor),
            chevron.centerYAnchor.constraint(equalTo: capsule.contentView.centerYAnchor),
        ])

        let tap = UITapGestureRecognizer(target: self, action: #selector(tapped(_:)))
        let pan = UIPanGestureRecognizer(target: self, action: #selector(panned(_:)))
        handle.addGestureRecognizer(tap)
        handle.addGestureRecognizer(pan)
    }

    @objc private func tapped(_ recognizer: UITapGestureRecognizer) {
        guard wantsVisible, recognizer.state == .ended else { return }
        onTap?()
    }

    /// Drag like the SwiftUI handle: it follows the finger, the end point
    /// is persisted as the normal handle position, and it springs back
    /// above the keyboard.
    @objc private func panned(_ recognizer: UIPanGestureRecognizer) {
        let t = recognizer.translation(in: self)
        switch recognizer.state {
        case .began, .changed:
            handle.transform = CGAffineTransform(translationX: t.x, y: t.y)
            onPoke?()
        case .ended:
            let area = safeAreaLayoutGuide.layoutFrame
            let center = handle.center
            let x = (center.x + t.x - area.minX) / max(area.width, 1)
            let y = (center.y + t.y - area.minY) / max(area.height, 1)
            onDragEnded?(x, y)
            springBack()
        default:
            springBack()
        }
    }

    private func springBack() {
        UIView.animate(withDuration: 0.35, delay: 0, usingSpringWithDamping: 0.86, initialSpringVelocity: 0,
                       options: [.allowUserInteraction, .beginFromCurrentState]) {
            self.handle.transform = .identity
        }
    }
}
