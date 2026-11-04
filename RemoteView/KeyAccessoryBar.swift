import Combine
import UIKit

/// The special-keys bar (UIKit; used as the key input view's
/// `inputAccessoryView`, re-hostable anywhere). Two pages in a horizontal
/// scroll view:
///   1: esc tab ctrl alt ⊞ shift | ← ↑ ↓ → | ›
///   2: ‹ F1–F12 | Home End PgUp PgDn Ins Del PrtSc | Ctrl+Alt+Del
/// plus a fixed "hide keyboard" button. Modifier buttons are sticky (see
/// `RemoteInputController.tapModifier`): latched = tinted, locked = filled.
final class KeyAccessoryBar: UIView {
    static let barHeight: CGFloat = 46

    private weak var input: RemoteInputController?
    /// Hide button tapped.
    var onDismiss: (() -> Void)?

    private let scroll = UIScrollView()
    private let content = UIStackView()
    private var page1: [UIView] = []
    private var page2: [UIView] = []
    private var modifierButtons: [(key: ModifierKey, button: UIButton)] = []
    private var cancellable: AnyCancellable?

    init(input: RemoteInputController) {
        self.input = input
        super.init(frame: CGRect(x: 0, y: 0, width: 320, height: KeyAccessoryBar.barHeight))
        autoresizingMask = [.flexibleWidth]
        backgroundColor = .secondarySystemBackground
        buildLayout()
        buildPages()
        showPage(1)
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

    private func buildPages() {
        // Page 1.
        page1.append(keyButton("esc", .escape))
        page1.append(keyButton("tab", .tab))
        let modifiers: [(ModifierKey, String)] = [(.ctrl, "ctrl"), (.alt, "alt"), (.win, "⊞"), (.shift, "shift")]
        for (m, title) in modifiers {
            let b = makeButton(title: title, symbol: nil, label: title == "⊞" ? "Windows" : title) { [weak self] in
                self?.input?.tapModifier(m)
            }
            modifierButtons.append((key: m, button: b))
            page1.append(b)
        }
        page1.append(separator())
        page1.append(keyButton("←", .left, label: "Left arrow"))
        page1.append(keyButton("↑", .up, label: "Up arrow"))
        page1.append(keyButton("↓", .down, label: "Down arrow"))
        page1.append(keyButton("→", .right, label: "Right arrow"))
        page1.append(separator())
        page1.append(makeButton(title: "›", symbol: nil, label: "More keys") { [weak self] in
            self?.showPage(2)
        })

        // Page 2.
        page2.append(makeButton(title: "‹", symbol: nil, label: "Back") { [weak self] in
            self?.showPage(1)
        })
        for n in 1...12 {
            if let k = KeyStroke.function(n) {
                page2.append(keyButton("F\(n)", k))
            }
        }
        page2.append(separator())
        page2.append(keyButton("Home", .home))
        page2.append(keyButton("End", .end))
        page2.append(keyButton("PgUp", .pageUp))
        page2.append(keyButton("PgDn", .pageDown))
        page2.append(keyButton("Ins", .insert))
        page2.append(keyButton("Del", .delete))
        page2.append(keyButton("PrtSc", .printScreen))
        page2.append(separator())
        page2.append(makeButton(title: "Ctrl+Alt+Del", symbol: nil, label: "Control Alt Delete") { [weak self] in
            self?.input?.sendSAS()
        })
    }

    private func showPage(_ n: Int) {
        for v in content.arrangedSubviews {
            content.removeArrangedSubview(v)
            v.removeFromSuperview()
        }
        for v in (n == 1 ? page1 : page2) {
            content.addArrangedSubview(v)
        }
        scroll.setContentOffset(.zero, animated: false)
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

    private static func baseConfiguration(_ style: ModifierState) -> UIButton.Configuration {
        var cfg: UIButton.Configuration
        switch style {
        case .off: cfg = .gray()
        case .latched: cfg = .tinted()
        case .locked: cfg = .filled()
        }
        cfg.cornerStyle = .medium
        cfg.contentInsets = NSDirectionalEdgeInsets(top: 6, leading: 10, bottom: 6, trailing: 10)
        let locked = style == .locked
        cfg.titleTextAttributesTransformer = UIConfigurationTextAttributesTransformer { incoming in
            var out = incoming
            out.font = UIFont.systemFont(ofSize: 15, weight: locked ? .bold : .medium)
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
            b.configuration = cfg
            b.accessibilityValue = s == .off ? nil : (s == .locked ? "locked" : "latched")
        }
    }
}
