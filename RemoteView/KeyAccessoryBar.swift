import Combine
import UIKit

/// The special-keys bar (UIKit; used as the key input view's
/// `inputAccessoryView`, re-hostable anywhere). One horizontally scrolling
/// ribbon in TeamViewer's order:
///   ⊞ Shift Ctrl Alt | ScrLk Fn | Esc Tab PrtScn Pause Ins Del Home End
///   PgUp PgDn | ← ↑ → ↓ | Ctrl+Alt+Del
/// "Fn" swaps the ribbon for F1–F12 (‹ goes back). A fixed "hide keyboard"
/// button sits on the trailing edge. Modifier buttons are sticky (see
/// `RemoteInputController.tapModifier`): tap = one-shot (tinted), double
/// tap = locked (filled). The first time the bar appears, a hint says the
/// ribbon scrolls.
final class KeyAccessoryBar: UIView {
    static let barHeight: CGFloat = 46
    /// UserDefaults flag: the "swipe for more keys" hint was shown.
    static let hintShownDefaultsKey = "ov.keyBarHintShown"

    private weak var input: RemoteInputController?
    /// Hide button tapped.
    var onDismiss: (() -> Void)?

    private let scroll = UIScrollView()
    private let content = UIStackView()
    private var mainKeys: [UIView] = []
    private var functionKeys: [UIView] = []
    private var modifierButtons: [(key: ModifierKey, button: UIButton)] = []
    private var cancellable: AnyCancellable?
    private var hintLabel: UILabel?

    init(input: RemoteInputController) {
        self.input = input
        super.init(frame: CGRect(x: 0, y: 0, width: 320, height: KeyAccessoryBar.barHeight))
        autoresizingMask = [.flexibleWidth]
        backgroundColor = .secondarySystemBackground
        buildLayout()
        buildKeys()
        showFunctionKeys(false)
        cancellable = input.$modifierStates.sink { [weak self] states in
            self?.applyModifierStates(states)
        }
        applyModifierStates(input.modifierStates)
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    override var intrinsicContentSize: CGSize {
        CGSize(width: UIView.noIntrinsicMetric, height: Self.barHeight)
    }

    override func didMoveToWindow() {
        super.didMoveToWindow()
        if window != nil {
            showHintIfNeeded()
        }
    }

    // MARK: Layout

    private func buildLayout() {
        let hide = makeButton(title: nil, symbol: "keyboard.chevron.compact.down",
                              label: "Hide keyboard") { [weak self] in
            self?.onDismiss?()
        }
        hide.translatesAutoresizingMaskIntoConstraints = false
        addSubview(hide)

        let divider = UIView()
        divider.backgroundColor = .separator
        divider.translatesAutoresizingMaskIntoConstraints = false
        addSubview(divider)

        scroll.showsHorizontalScrollIndicator = false
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
            divider.trailingAnchor.constraint(equalTo: hide.leadingAnchor, constant: -4),
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
        mainKeys.append(separator())
        mainKeys.append(makeButton(title: "Ctrl+Alt+Del", symbol: nil, label: "Control Alt Delete") { [weak self] in
            self?.input?.sendSAS()
        })

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

    /// Uniform key width (TeamViewer-style even ribbon).
    private static let keyWidth: CGFloat = 54

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
                            action: @escaping () -> Void) -> UIButton {
        var cfg = Self.baseConfiguration(.off)
        if let title { cfg.title = title }
        if let symbol { cfg.image = UIImage(systemName: symbol) }
        let b = UIButton(configuration: cfg, primaryAction: UIAction { _ in action() })
        b.accessibilityLabel = label
        // Every key the same width (fits the longest label, "PrtScn").
        b.translatesAutoresizingMaskIntoConstraints = false
        b.widthAnchor.constraint(equalToConstant: Self.keyWidth).isActive = true
        b.titleLabel?.adjustsFontSizeToFitWidth = true
        b.titleLabel?.minimumScaleFactor = 0.8
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
