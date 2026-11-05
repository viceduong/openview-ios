import SwiftUI

/// The session toolbar: a collapsible pill (.ultraThinMaterial capsule,
/// 44 pt targets) — Keyboard, Mode (Mouse / Touch), Menu, Chat (unread
/// badge), Disconnect, Collapse. Bottom-trailing in portrait, vertical on
/// the trailing edge in landscape (horizontal at the bottom when the
/// landscape height cannot fit the vertical pill, e.g. keyboard up).
/// Collapsed it is a small handle the user drags anywhere; it snaps to the
/// nearest side edge and the position is persisted. Optionally fades to
/// 70% after 3 s without interaction ("Fade toolbar when idle", default
/// off) — purely visual: every tap acts immediately, faded or not.
/// Cmd+Shift+Space toggles it (SessionHostingController).
///
/// The pill / handle report their frames (`reportsChromeFrame`) so the
/// remote surface underneath never takes touches meant for them.
struct FloatingToolbar: View {
    @ObservedObject var chrome: SessionChrome
    @ObservedObject var input: RemoteInputController
    let isLandscape: Bool
    let unread: Int
    let onKeyboard: () -> Void
    let onMenu: () -> Void
    let onChat: () -> Void
    let onDisconnect: () -> Void

    @AppStorage(SettingsKey.toolbarHandleEdge) private var handleEdge = "trailing"
    @AppStorage(SettingsKey.toolbarHandleY) private var handleY: Double = 0.7
    @State private var dragOffset: CGSize = .zero

    private static let target: CGFloat = 44
    private static let handleHeight: CGFloat = 64
    private static let buttonCount: CGFloat = 6
    private static let spacing: CGFloat = 2
    private static let pillInset: CGFloat = 4
    private static let edgeGap: CGFloat = 12
    /// Length of the pill along its axis (6 x 44 + spacing + insets).
    private static let pillLength: CGFloat =
        FloatingToolbar.buttonCount * FloatingToolbar.target
            + (FloatingToolbar.buttonCount - 1) * FloatingToolbar.spacing
            + 2 * FloatingToolbar.pillInset

    var body: some View {
        GeometryReader { geo in
            if chrome.toolbarCollapsed {
                handle(in: geo.size)
            } else {
                let vertical = isLandscape && geo.size.height >= Self.pillLength + 2 * Self.edgeGap
                pill(vertical: vertical)
                    .padding(.trailing, Self.edgeGap)
                    .padding(.bottom, vertical ? 0 : Self.edgeGap)
                    .frame(width: geo.size.width, height: geo.size.height,
                           alignment: vertical ? .trailing : .bottomTrailing)
            }
        }
        .opacity(chrome.toolbarFaded ? SessionChrome.fadedOpacity : 1)
        .animation(.easeInOut(duration: 0.3), value: chrome.toolbarFaded)
        .animation(.spring(response: 0.32, dampingFraction: 0.86), value: chrome.toolbarCollapsed)
    }

    // MARK: Expanded pill

    private func pill(vertical: Bool) -> some View {
        Group {
            if vertical {
                VStack(spacing: Self.spacing) {
                    buttons(vertical: true)
                }
            } else {
                HStack(spacing: Self.spacing) {
                    buttons(vertical: false)
                }
            }
        }
        .padding(Self.pillInset)
        .background(.ultraThinMaterial, in: Capsule())
        .overlay(Capsule().stroke(Color.white.opacity(0.15), lineWidth: 0.5))
        .shadow(color: Color.black.opacity(0.35), radius: 8, y: 3)
        .reportsChromeFrame()
    }

    @ViewBuilder
    private func buttons(vertical: Bool) -> some View {
        toolButton("keyboard", label: "Keyboard", action: onKeyboard)
        toolButton(input.mode == .mouse ? "cursorarrow" : "hand.point.up.left",
                   label: input.mode == .mouse ? "Mouse mode, switch to touch" : "Touch mode, switch to mouse") {
            input.setMode(input.mode == .mouse ? .touch : .mouse)
        }
        toolButton("ellipsis.circle", label: "Session menu", action: onMenu)
        toolButton("bubble.left.and.bubble.right", label: "Chat", action: onChat)
            .overlay(alignment: .topTrailing) {
                if unread > 0 {
                    Text(unread > 99 ? "99+" : "\(unread)")
                        .font(.caption2.weight(.bold))
                        .foregroundColor(.white)
                        .padding(.horizontal, 5)
                        .padding(.vertical, 1)
                        .background(Capsule().fill(Color.red))
                        .offset(x: -1, y: 3)
                        .allowsHitTesting(false)
                }
            }
        toolButton("xmark", label: "Disconnect", tint: Color(red: 1, green: 0.42, blue: 0.4), action: onDisconnect)
        toolButton(vertical ? "chevron.right" : "chevron.down", label: "Collapse toolbar") {
            chrome.toolbarCollapsed = true
        }
    }

    /// One 44 x 44 pt button. The action runs on the first tap whether or
    /// not the toolbar is faded (poke only restores full opacity).
    private func toolButton(_ symbol: String, label: String, tint: Color = .white,
                            action: @escaping () -> Void) -> some View {
        Button {
            chrome.logTouchRouting("toolbar: \(label)")
            chrome.poke()
            action()
        } label: {
            Image(systemName: symbol)
                .font(.system(size: 18, weight: .semibold))
                .foregroundColor(tint)
                .frame(width: Self.target, height: Self.target)
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityLabel(label)
    }

    // MARK: Collapsed handle

    private func handle(in size: CGSize) -> some View {
        let onTrailing = handleEdge != "leading"
        let halfWidth = Self.target / 2
        let halfHeight = Self.handleHeight / 2
        let x = onTrailing ? size.width - halfWidth : halfWidth
        let maxY = max(halfHeight, size.height - halfHeight)
        let y = min(max(CGFloat(handleY) * size.height, halfHeight), maxY)
        return Image(systemName: onTrailing ? "chevron.compact.left" : "chevron.compact.right")
            .font(.system(size: 22, weight: .bold))
            .foregroundColor(.white)
            .frame(width: 26, height: Self.handleHeight)
            .background(.ultraThinMaterial, in: Capsule())
            .overlay(Capsule().stroke(Color.white.opacity(0.15), lineWidth: 0.5))
            .frame(width: Self.target, height: Self.handleHeight)
            .contentShape(Rectangle())
            .onTapGesture {
                chrome.logTouchRouting("toolbar: Show toolbar (handle)")
                chrome.poke()
                chrome.toolbarCollapsed = false
            }
            // Global coordinates: the handle moves with the finger, so a
            // local-space translation would feed back on itself.
            .gesture(
                DragGesture(minimumDistance: 6, coordinateSpace: .global)
                    .onChanged { value in
                        dragOffset = value.translation
                        chrome.poke()
                    }
                    .onEnded { value in
                        let endX = x + value.translation.width
                        let endY = y + value.translation.height
                        handleEdge = endX < size.width / 2 ? "leading" : "trailing"
                        let fraction = endY / max(size.height, 1)
                        handleY = Double(min(max(fraction, 0.08), 0.92))
                        dragOffset = .zero
                        chrome.logTouchRouting("toolbar: handle moved to \(handleEdge)")
                    }
            )
            .accessibilityLabel("Show toolbar")
            .accessibilityAddTraits(.isButton)
            .reportsChromeFrame()
            .position(x: x + dragOffset.width, y: y + dragOffset.height)
    }
}

// MARK: - Chrome frames

/// Frames (global coordinates) of the tappable session chrome, collected by
/// the session view into `RemoteInputController.chromeExclusionRects`.
struct ChromeFramesKey: PreferenceKey {
    static let defaultValue: [CGRect] = []

    static func reduce(value: inout [CGRect], nextValue: () -> [CGRect]) {
        value.append(contentsOf: nextValue())
    }
}

extension View {
    /// Report this view's frame as session chrome (see `ChromeFramesKey`):
    /// the remote surface below does not take touches inside it.
    func reportsChromeFrame() -> some View {
        background(
            GeometryReader { g in
                Color.clear.preference(key: ChromeFramesKey.self, value: [g.frame(in: .global)])
            }
        )
    }
}
