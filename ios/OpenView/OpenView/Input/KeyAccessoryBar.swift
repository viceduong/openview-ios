import Combine
import UIKit

/// The special-keys bar (UIKit; used as the key input view's
/// `inputAccessoryView`, re-hostable anywhere). One horizontally scrolling
/// ribbon:
///   ⊞ Shift Ctrl Alt | ScrLk Fn | Esc Tab PrtScn Pause Ins Del Home End
///   PgUp PgDn | ← ↑ → ↓
/// "Fn" swaps the ribbon for F1–F12 (‹ goes back). A fixed "hide keyboard"
/// button sits on the trailing edge. Modifier buttons are sticky (see
/// `RemoteInputController.tapModifier`): tap = one-shot (tinted), double
/// tap = locked (filled). The first time the bar appears, a hint says the
/// ribbon scrolls.
///
/// Toolbar: a fixed toolbar button (next to "hide keyboard") expands /
/// collapses the floating toolbar over the keyboard, and while the toolbar
/// is collapsed its handle is drawn by this bar, protruding above the
/// top-trailing corner, so it moves with the key row by construction
/// (`KeyBarToolbarLink` carries the state and actions).
final class KeyAccessoryBar: UIView {
    static let barHeight: CGFloat = 46
    /// UserDefaults flag: the "swipe for more keys" hint was shown.
    static let hintShownDefaultsKey = "ov.keyBarHintShown"

    private weak var input: RemoteInputController?
    /// Hide button tapped.
    var onDismiss: (() -> Void)?

    private let scroll = KeyStripScrollView()
    private let content = UIStackView()
    private var mainKeys: [UIView] = []
    private var functionKeys: [UIView] = []
    private var modifierButtons: [(key: ModifierKey, button: UIButton)] = []
    private var cancellable: AnyCancellable?
    private var hintLabel: UILabel?

    // Floating-toolbar handle / button.
    private let link: KeyBarToolbarLink
    private var linkCancellable: AnyCancellable?
    private let toolbarHandle = KeyBarToolbarHandle()
    private var toolbarButton: UIButton?
    private var handleFramePending = false
    /// Geometry of the floating toolbar's own handle: 44 pt wide touch
    /// target, 26 x 64 capsule, 8 pt above the key row.
    private static let handleWidth: CGFloat = 44
    private static let handleHeight: CGFloat = 64
    private static let handleGap: CGFloat = 8
    private static let toolbarButtonWidth: CGFloat = 44

    init(input: RemoteInputController) {
        self.input = input
        self.link = KeyBarToolbarLink.link(for: input)
        super.init(frame: CGRect(x: 0, y: 0, width: 320, height: KeyAccessoryBar.barHeight))
        autoresizingMask = [.flexibleWidth]
        backgroundColor = .secondarySystemBackground
        // The toolbar handle protrudes above the bar.
        clipsToBounds = false
        buildLayout()
        buildKeys()
        buildToolbarHandle()
        showFunctionKeys(false)
        cancellable = input.$modifierStates.sink { [weak self] states in
            self?.applyModifierStates(states)
        }
        applyModifierStates(input.modifierStates)
        // @Published emits before the property changes: use the values
        // passed in, never read them back from the link here.
        linkCancellable = Publishers.CombineLatest3(link.$attached, link.$handleVisible, link.$faded)
            .sink { [weak self] attached, visible, faded in
                self?.applyToolbarState(attached: attached, handleVisible: visible, faded: faded)
            }
        let center = NotificationCenter.default
        center.addObserver(self, selector: #selector(keyboardDidMove(_:)),
                           name: UIResponder.keyboardDidChangeFrameNotification, object: nil)
        center.addObserver(self, selector: #selector(keyboardDidMove(_:)),
                           name: UIResponder.keyboardDidShowNotification, object: nil)
        center.addObserver(self, selector: #selector(keyboardWillHide(_:)),
                           name: UIResponder.keyboardWillHideNotification, object: nil)
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    deinit {
        NotificationCenter.default.removeObserver(self)
    }

    override var intrinsicContentSize: CGSize {
        CGSize(width: UIView.noIntrinsicMetric, height: Self.barHeight)
    }

    override func didMoveToWindow() {
        super.didMoveToWindow()
        if window != nil {
            showHintIfNeeded()
        }
        scheduleHandleFramePublish()
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        scheduleHandleFramePublish()
    }

    /// Touches on the protruding handle belong to the bar too (whether
    /// they reach it depends on the keyboard's host views; the toolbar
    /// keeps a tap target under the handle and the toolbar button for
    /// when they do not).
    override func point(inside point: CGPoint, with event: UIEvent?) -> Bool {
        if super.point(inside: point, with: event) { return true }
        return handleTakesTouches && toolbarHandle.frame.contains(point)
    }

    private var handleTakesTouches: Bool {
        !toolbarHandle.isHidden && toolbarHandle.alpha > 0.01 && toolbarHandle.isUserInteractionEnabled
    }

    // MARK: Layout

    private func buildLayout() {
        let hide = makeButton(title: nil, symbol: "keyboard.chevron.compact.down",
                              label: "Hide keyboard") { [weak self] in
            self?.onDismiss?()
        }
        // Distinct from the keys: filled accent key with a white glyph.
        var hideCfg = UIButton.Configuration.filled()
        hideCfg.image = UIImage(systemName: "keyboard.chevron.compact.down",
                                withConfiguration: UIImage.SymbolConfiguration(pointSize: 16, weight: .semibold))
        hideCfg.baseBackgroundColor = .systemBlue
        hideCfg.baseForegroundColor = .white
        // Same rounded-rectangle shape and insets as the other keys.
        hideCfg.cornerStyle = .medium
        hideCfg.contentInsets = NSDirectionalEdgeInsets(top: 6, leading: 4, bottom: 6, trailing: 4)
        hide.configuration = hideCfg
        hide.translatesAutoresizingMaskIntoConstraints = false
        addSubview(hide)

        // Floating toolbar: show (or hide) it over the keyboard.
        let toolbar = makeButton(title: nil, symbol: "sidebar.right", label: "Show toolbar",
                                 width: Self.toolbarButtonWidth) { [weak self] in
            self?.link.onToggleToolbar?()
        }
        addSubview(toolbar)
        toolbarButton = toolbar

        let divider = UIView()
        divider.backgroundColor = .separator
        divider.translatesAutoresizingMaskIntoConstraints = false
        addSubview(divider)

        scroll.showsHorizontalScrollIndicator = false
        // Deliver touches to the keys immediately (default: held ~150 ms to
        // detect a scroll, so a quick tap's press never showed). Scrolling
        // still works: a drag cancels the key's touch.
        scroll.delaysContentTouches = false
        scroll.canCancelContentTouches = true
        scroll.alwaysBounceHorizontal = true
        scroll.translatesAutoresizingMaskIntoConstraints = false
        addSubview(scroll)

        content.axis = .horizontal
        content.spacing = 6
        content.alignment = .center
        content.translatesAutoresizingMaskIntoConstraints = false
        scroll.addSubview(content)

        NSLayoutConstraint.activate([
            hide.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -6),
            hide.centerYAnchor.constraint(equalTo: centerYAnchor),
            toolbar.trailingAnchor.constraint(equalTo: hide.leadingAnchor, constant: -4),
            toolbar.centerYAnchor.constraint(equalTo: centerYAnchor),
            divider.trailingAnchor.constraint(equalTo: toolbar.leadingAnchor, constant: -4),
            divider.widthAnchor.constraint(equalToConstant: 1),
            divider.heightAnchor.constraint(equalToConstant: 26),
            divider.centerYAnchor.constraint(equalTo: centerYAnchor),
            scroll.leadingAnchor.constraint(equalTo: leadingAnchor),
            scroll.trailingAnchor.constraint(equalTo: divider.leadingAnchor, constant: -4),
            scroll.topAnchor.constraint(equalTo: topAnchor),
            scroll.heightAnchor.constraint(equalToConstant: Self.barHeight),
            content.leadingAnchor.constraint(equalTo: scroll.contentLayoutGuide.leadingAnchor, constant: 6),
            content.trailingAnchor.constraint(equalTo: scroll.contentLayoutGuide.trailingAnchor, constant: -6),
            content.topAnchor.constraint(equalTo: scroll.contentLayoutGuide.topAnchor),
            content.bottomAnchor.constraint(equalTo: scroll.contentLayoutGuide.bottomAnchor),
            content.heightAnchor.constraint(equalTo: scroll.frameLayoutGuide.heightAnchor),
        ])
    }

    private func buildKeys() {
        // Sticky modifiers: ⊞ Win, Shift, Ctrl, Alt.
        let modifiers: [(ModifierKey, String, String)] = [
            (.win, "⊞", "Windows"), (.shift, "Shift", "Shift"), (.ctrl, "Ctrl", "Control"), (.alt, "Alt", "Alt"),
        ]
        for (m, title, label) in modifiers {
            let b = makeButton(title: m == .win ? nil : title, symbol: nil, label: label) { [weak self] in
                self?.input?.tapModifier(m)
            }
            if m == .win {
                // Drawn Windows logo (no SF Symbol exists for it).
                var cfg = b.configuration ?? Self.baseConfiguration(.off)
                cfg.image = Self.windowsLogo
                b.configuration = cfg
            }
            modifierButtons.append((key: m, button: b))
            mainKeys.append(b)
        }
        mainKeys.append(separator())
        mainKeys.append(keyButton("ScrLk", .scrollLock, label: "Scroll Lock"))
        mainKeys.append(makeButton(title: "Fn", symbol: nil, label: "Function keys") { [weak self] in
            self?.showFunctionKeys(true)
        })
        mainKeys.append(separator())
        mainKeys.append(keyButton("Esc", .escape, label: "Escape"))
        mainKeys.append(keyButton("Tab", .tab))
        mainKeys.append(keyButton("PrtScn", .printScreen, label: "Print Screen"))
        mainKeys.append(keyButton("Pause", .pause))
        mainKeys.append(keyButton("Ins", .insert, label: "Insert"))
        mainKeys.append(keyButton("Del", .delete, label: "Delete"))
        mainKeys.append(keyButton("Home", .home))
        mainKeys.append(keyButton("End", .end))
        mainKeys.append(keyButton("PgUp", .pageUp, label: "Page Up"))
        mainKeys.append(keyButton("PgDn", .pageDown, label: "Page Down"))
        mainKeys.append(separator())
        mainKeys.append(keyButton("←", .left, label: "Left arrow"))
        mainKeys.append(keyButton("↑", .up, label: "Up arrow"))
        mainKeys.append(keyButton("→", .right, label: "Right arrow"))
        mainKeys.append(keyButton("↓", .down, label: "Down arrow"))

        // Function-key page.
        functionKeys.append(makeButton(title: "‹", symbol: nil, label: "Back") { [weak self] in
            self?.showFunctionKeys(false)
        })
        for n in 1...12 {
            if let k = KeyStroke.function(n) {
                functionKeys.append(keyButton("F\(n)", k))
            }
        }
    }

    private func showFunctionKeys(_ on: Bool) {
        for v in content.arrangedSubviews {
            content.removeArrangedSubview(v)
            v.removeFromSuperview()
        }
        for v in (on ? functionKeys : mainKeys) {
            content.addArrangedSubview(v)
        }
        scroll.setContentOffset(.zero, animated: false)
    }

    // MARK: Floating-toolbar handle

    /// The collapsed toolbar's handle, above the bar's top-trailing corner
    /// (same look as the toolbar's SwiftUI handle). A subview of the bar, so
    /// UIKit animates it together with the key row.
    private func buildToolbarHandle() {
        let handle = toolbarHandle
        handle.translatesAutoresizingMaskIntoConstraints = false
        handle.alpha = 0
        handle.isUserInteractionEnabled = false
        handle.accessibilityElementsHidden = true
        handle.onActivate = { [weak self] in
            self?.link.onShowToolbar?()
        }
        addSubview(handle)
        NSLayoutConstraint.activate([
            handle.widthAnchor.constraint(equalToConstant: Self.handleWidth),
            handle.heightAnchor.constraint(equalToConstant: Self.handleHeight),
            // The right edge, like the toolbar's handle (chevron points left).
            handle.rightAnchor.constraint(equalTo: safeAreaLayoutGuide.rightAnchor),
            handle.bottomAnchor.constraint(equalTo: topAnchor, constant: -Self.handleGap),
        ])
    }

    private func applyToolbarState(attached: Bool, handleVisible: Bool, faded: Bool) {
        let show = attached && handleVisible
        toolbarHandle.isUserInteractionEnabled = show
        toolbarHandle.accessibilityElementsHidden = !show
        let target: CGFloat = show ? (faded ? CGFloat(SessionChrome.fadedOpacity) : 1) : 0
        if toolbarHandle.alpha != target {
            // Alpha only: the position always follows the bar.
            UIView.animate(withDuration: 0.2, delay: 0,
                           options: [.beginFromCurrentState, .allowUserInteraction, .curveEaseInOut]) {
                self.toolbarHandle.alpha = target
            }
        }
        if let button = toolbarButton {
            // Expanded over the keyboard: tinted, and the button hides it.
            let expanded = attached && !handleVisible
            var cfg = Self.baseConfiguration(expanded ? .latched : .off)
            cfg.image = button.configuration?.image
            button.configuration = cfg
            button.accessibilityLabel = expanded ? "Hide toolbar" : "Show toolbar"
            button.isEnabled = attached
        }
    }

    @objc private func keyboardDidMove(_ note: Notification) {
        scheduleHandleFramePublish()
    }

    @objc private func keyboardWillHide(_ note: Notification) {
        if !link.handleFrame.isNull {
            link.handleFrame = .null
        }
    }

    /// Publish the handle's frame (app window coordinates) for the
    /// toolbar's tap target, on the next main-queue turn (never from inside
    /// a layout or SwiftUI update; coalesced).
    private func scheduleHandleFramePublish() {
        guard !handleFramePending else { return }
        handleFramePending = true
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            self.handleFramePending = false
            let frame = self.currentHandleFrame()
            if self.link.handleFrame != frame {
                self.link.handleFrame = frame
            }
        }
    }

    /// The handle's frame in the session window (SwiftUI `.global`): the
    /// bar lives in the keyboard's window, so go through screen
    /// coordinates. Model values, i.e. where it ends up after an animation.
    private func currentHandleFrame() -> CGRect {
        guard window != nil else { return .null }
        let screenSpace = UIScreen.main.coordinateSpace
        let onScreen = toolbarHandle.convert(toolbarHandle.bounds, to: screenSpace)
        guard let appWindow = (input?.activeSurface as? UIView)?.window else { return onScreen }
        return appWindow.convert(onScreen, from: screenSpace)
    }

    // MARK: First-use hint

    private func showHintIfNeeded() {
        guard hintLabel == nil,
              !UserDefaults.standard.bool(forKey: Self.hintShownDefaultsKey) else { return }
        UserDefaults.standard.set(true, forKey: Self.hintShownDefaultsKey)
        let label = UILabel()
        label.text = "  Swipe to access all keys ›  "
        label.font = UIFont.systemFont(ofSize: 13, weight: .semibold)
        label.textColor = .white
        label.backgroundColor = UIColor.systemBlue.withAlphaComponent(0.92)
        label.layer.cornerRadius = 12
        label.layer.masksToBounds = true
        label.isUserInteractionEnabled = false
        label.translatesAutoresizingMaskIntoConstraints = false
        addSubview(label)
        NSLayoutConstraint.activate([
            label.centerXAnchor.constraint(equalTo: centerXAnchor),
            label.centerYAnchor.constraint(equalTo: centerYAnchor),
            label.heightAnchor.constraint(equalToConstant: 26),
        ])
        hintLabel = label
        UIView.animate(withDuration: 0.4, delay: 3.0, options: [.curveEaseInOut], animations: {
            label.alpha = 0
        }, completion: { [weak self] _ in
            label.removeFromSuperview()
            self?.hintLabel = nil
        })
        // Nudge the ribbon so the scrollability is visible.
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.6) { [weak self] in
            guard let self else { return }
            let maxX = self.scroll.contentSize.width - self.scroll.bounds.width
            guard maxX > 20 else { return }
            self.scroll.setContentOffset(CGPoint(x: min(60, maxX), y: 0), animated: true)
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) { [weak self] in
                self?.scroll.setContentOffset(.zero, animated: true)
            }
        }
    }

    // MARK: Buttons

    private func keyButton(_ title: String, _ key: KeyStroke, label: String? = nil) -> UIButton {
        makeButton(title: title, symbol: nil, label: label ?? title) { [weak self] in
            self?.input?.tapKey(key)
        }
    }

    private func separator() -> UIView {
        let v = UIView()
        v.backgroundColor = .separator
        v.translatesAutoresizingMaskIntoConstraints = false
        NSLayoutConstraint.activate([
            v.widthAnchor.constraint(equalToConstant: 1),
            v.heightAnchor.constraint(equalToConstant: 24),
        ])
        return v
    }

    @objc private func keyPressed(_ sender: UIButton) {
        UIView.animate(withDuration: 0.08, delay: 0, options: [.beginFromCurrentState, .allowUserInteraction]) {
            sender.transform = CGAffineTransform(scaleX: 0.9, y: 0.9)
            sender.alpha = 0.55
        }
    }

    @objc private func keyReleased(_ sender: UIButton) {
        UIView.animate(withDuration: 0.3, delay: 0.06,
                       options: [.beginFromCurrentState, .allowUserInteraction, .curveEaseOut]) {
            sender.transform = .identity
            sender.alpha = 1
        }
    }

    /// Uniform key width (an even ribbon).
    private static let keyWidth: CGFloat = 60

    /// The Windows 11 logo: four equal squares in a 2 x 2 grid, as a
    /// template image so it takes the button's tint in every state.
    private static let windowsLogo: UIImage = {
        let side: CGFloat = 15
        let gap: CGFloat = 1.6
        let cell = (side - gap) / 2
        let image = UIGraphicsImageRenderer(size: CGSize(width: side, height: side)).image { ctx in
            UIColor.black.setFill()
            for row in 0..<2 {
                for col in 0..<2 {
                    let rect = CGRect(x: CGFloat(col) * (cell + gap),
                                      y: CGFloat(row) * (cell + gap),
                                      width: cell, height: cell)
                    UIBezierPath(roundedRect: rect, cornerRadius: 0.8).fill()
                }
            }
            _ = ctx
        }
        return image.withRenderingMode(.alwaysTemplate)
    }()

    private static func baseConfiguration(_ style: ModifierState) -> UIButton.Configuration {
        var cfg: UIButton.Configuration
        switch style {
        case .off: cfg = .gray()
        case .latched: cfg = .tinted()
        case .locked: cfg = .filled()
        }
        cfg.cornerStyle = .medium
        cfg.contentInsets = NSDirectionalEdgeInsets(top: 6, leading: 4, bottom: 6, trailing: 4)
        let locked = style == .locked
        cfg.titleTextAttributesTransformer = UIConfigurationTextAttributesTransformer { incoming in
            var out = incoming
            out.font = UIFont.systemFont(ofSize: 14, weight: locked ? .bold : .medium)
            return out
        }
        return cfg
    }

    private func makeButton(title: String?, symbol: String?, label: String,
                            width: CGFloat = KeyAccessoryBar.keyWidth,
                            action: @escaping () -> Void) -> UIButton {
        var cfg = Self.baseConfiguration(.off)
        if let title { cfg.title = title }
        if let symbol { cfg.image = UIImage(systemName: symbol) }
        let b = UIButton(configuration: cfg, primaryAction: UIAction { _ in action() })
        b.accessibilityLabel = label
        // Every key the same width (fits the longest label, "PrtScn").
        b.translatesAutoresizingMaskIntoConstraints = false
        b.widthAnchor.constraint(equalToConstant: width).isActive = true
        b.titleLabel?.adjustsFontSizeToFitWidth = true
        b.titleLabel?.minimumScaleFactor = 0.8
        // Visible press feedback that lingers a moment after the finger
        // lifts (the configuration's own highlight is instant).
        b.addTarget(self, action: #selector(keyPressed(_:)), for: [.touchDown, .touchDragEnter])
        b.addTarget(self, action: #selector(keyReleased(_:)),
                    for: [.touchUpInside, .touchUpOutside, .touchCancel, .touchDragExit])
        return b
    }

    /// Restyle the modifier buttons: off = gray, latched = tinted, locked =
    /// filled + bold.
    private func applyModifierStates(_ states: [ModifierState]) {
        for (m, b) in modifierButtons {
            guard m.rawValue < states.count else { continue }
            let s = states[m.rawValue]
            var cfg = Self.baseConfiguration(s)
            cfg.title = b.configuration?.title
            cfg.image = b.configuration?.image
            b.configuration = cfg
            b.accessibilityValue = s == .off ? nil : (s == .locked ? "locked" : "latched")
        }
    }
}

/// The floating toolbar's collapsed handle as drawn above the key bar: a
/// 26 x 64 ultra-thin capsule with a left chevron in a 44 x 64 target. Tap
/// (or VoiceOver activate) shows the toolbar.
final class KeyBarToolbarHandle: UIView {
    var onActivate: (() -> Void)?

    override init(frame: CGRect) {
        super.init(frame: frame)
        backgroundColor = .clear
        // The session chrome is always dark.
        overrideUserInterfaceStyle = .dark
        isAccessibilityElement = true
        accessibilityLabel = "Show toolbar"
        accessibilityTraits = .button

        let capsuleWidth: CGFloat = 26
        let capsule = UIVisualEffectView(effect: UIBlurEffect(style: .systemUltraThinMaterial))
        capsule.translatesAutoresizingMaskIntoConstraints = false
        capsule.isUserInteractionEnabled = false
        capsule.layer.cornerRadius = capsuleWidth / 2
        capsule.layer.masksToBounds = true
        capsule.layer.borderWidth = 0.5
        capsule.layer.borderColor = UIColor(white: 1, alpha: 0.15).cgColor
        addSubview(capsule)

        let symbol = UIImage.SymbolConfiguration(pointSize: 22, weight: .bold)
        let chevron = UIImageView(image: UIImage(systemName: "chevron.compact.left", withConfiguration: symbol))
        chevron.translatesAutoresizingMaskIntoConstraints = false
        chevron.tintColor = .white
        chevron.contentMode = .center
        capsule.contentView.addSubview(chevron)

        NSLayoutConstraint.activate([
            capsule.centerXAnchor.constraint(equalTo: centerXAnchor),
            capsule.centerYAnchor.constraint(equalTo: centerYAnchor),
            capsule.widthAnchor.constraint(equalToConstant: capsuleWidth),
            capsule.heightAnchor.constraint(equalTo: heightAnchor),
            chevron.centerXAnchor.constraint(equalTo: capsule.contentView.centerXAnchor),
            chevron.centerYAnchor.constraint(equalTo: capsule.contentView.centerYAnchor),
        ])

        addGestureRecognizer(UITapGestureRecognizer(target: self, action: #selector(tapped(_:))))
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    @objc private func tapped(_ recognizer: UITapGestureRecognizer) {
        guard recognizer.state == .ended else { return }
        onActivate?()
    }

    override func accessibilityActivate() -> Bool {
        onActivate?()
        return true
    }
}

/// The key strip: touches reach the keys at once (no press delay), and a
/// drag that starts on a key still scrolls the strip.
private final class KeyStripScrollView: UIScrollView {
    override func touchesShouldCancel(in view: UIView) -> Bool {
        true
    }
}
