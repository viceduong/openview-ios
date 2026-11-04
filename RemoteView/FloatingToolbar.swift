import SwiftUI

/// The session toolbar: a collapsible pill (.ultraThinMaterial capsule,
/// 44 pt targets) — Keyboard, Mode (Mouse / Touch), Menu, Chat (unread
/// badge), Disconnect, Collapse. Bottom-trailing in portrait, vertical on
/// the trailing edge in landscape. Collapsed it is a small handle the user
/// drags anywhere; it snaps to the nearest side edge and the position is
/// persisted. Fades to 40% after 3 s without interaction (setting);
/// Cmd+Shift+Space toggles it (SessionHostingController).
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

    var body: some View {
        GeometryReader { geo in
            if chrome.toolbarCollapsed {
                handle(in: geo.size)
            } else {
                pill
                    .frame(width: geo.size.width, height: geo.size.height,
                           alignment: isLandscape ? .trailing : .bottomTrailing)
            }
        }
        .padding(.trailing, chrome.toolbarCollapsed ? 0 : 12)
        .padding(.bottom, chrome.toolbarCollapsed || isLandscape ? 0 : 12)
        .opacity(chrome.toolbarFaded ? 0.4 : 1)
        .animation(.easeInOut(duration: 0.3), value: chrome.toolbarFaded)
        .animation(.spring(response: 0.32, dampingFraction: 0.86), value: chrome.toolbarCollapsed)
    }

    // MARK: Expanded pill

    private var pill: some View {
        Group {
            if isLandscape {
                VStack(spacing: 2) {
                    buttons
                }
            } else {
                HStack(spacing: 2) {
                    buttons
                }
            }
        }
        .padding(4)
        .background(.ultraThinMaterial, in: Capsule())
        .overlay(Capsule().stroke(Color.white.opacity(0.15), lineWidth: 0.5))
        .shadow(color: Color.black.opacity(0.35), radius: 8, y: 3)
    }

    @ViewBuilder
    private var buttons: some View {
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
        toolButton(isLandscape ? "chevron.right" : "chevron.down", label: "Collapse toolbar") {
            chrome.toolbarCollapsed = true
        }
    }

    private func toolButton(_ symbol: String, label: String, tint: Color = .white,
                            action: @escaping () -> Void) -> some View {
        Button {
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
                chrome.poke()
                chrome.toolbarCollapsed = false
            }
            .gesture(
                DragGesture(minimumDistance: 6)
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
                    }
            )
            .accessibilityLabel("Show toolbar")
            .accessibilityAddTraits(.isButton)
            .position(x: x + dragOffset.width, y: y + dragOffset.height)
    }
}
