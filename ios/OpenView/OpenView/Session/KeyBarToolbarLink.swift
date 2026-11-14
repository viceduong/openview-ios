import Combine
import CoreGraphics

/// Bridge between the floating toolbar (SwiftUI) and the key accessory bar
/// (UIKit, the key input view's `inputAccessoryView`), one per
/// `RemoteInputController`. Main thread only.
///
/// While the keyboard / key bar is up the collapsed toolbar handle is drawn
/// by the bar itself, protruding above its top-trailing corner, so it moves
/// with the key row by construction (keyboard frame reports — notifications
/// and the keyboard layout guide alike — sometimes include the key row only
/// late). The toolbar publishes here whether that handle shows; the bar
/// publishes where it is (for a SwiftUI tap target underneath, in case the
/// keyboard window does not deliver touches outside the bar's bounds).
final class KeyBarToolbarLink: ObservableObject {
    /// A floating toolbar is on screen and wired (its actions are set).
    @Published var attached = false
    /// The toolbar is collapsed (as it is while the keyboard is up, unless
    /// the user expanded it over the keyboard): the bar shows the handle.
    @Published var handleVisible = true
    /// The toolbar is faded (idle): the handle is drawn at 70%.
    @Published var faded = false
    /// The protruding handle in the app window's coordinates (SwiftUI
    /// `.global`); `.null` while the bar is not on screen.
    @Published var handleFrame: CGRect = .null

    /// Handle tapped: expand the toolbar over the keyboard.
    var onShowToolbar: (() -> Void)?
    /// The key row's toolbar button: expand / collapse the toolbar.
    var onToggleToolbar: (() -> Void)?

    private final class Entry {
        weak var owner: RemoteInputController?
        let link: KeyBarToolbarLink

        init(owner: RemoteInputController, link: KeyBarToolbarLink) {
            self.owner = owner
            self.link = link
        }
    }

    private static var entries: [ObjectIdentifier: Entry] = [:]

    /// The link of `input` (created on first use).
    static func link(for input: RemoteInputController) -> KeyBarToolbarLink {
        let id = ObjectIdentifier(input)
        if let entry = entries[id], entry.owner === input {
            return entry.link
        }
        // Drop links of controllers that are gone (their identifier may be
        // reused by a new one).
        entries = entries.filter { $0.value.owner != nil }
        let link = KeyBarToolbarLink()
        entries[id] = Entry(owner: input, link: link)
        return link
    }
}
