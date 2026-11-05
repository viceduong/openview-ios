import SwiftUI
import UIKit

/// The session toolbar: a collapsible pill (.ultraThinMaterial capsule,
/// 44 pt targets) — Keyboard, Menu, Disconnect, Collapse. Always a vertical pill docked on the
/// trailing edge (TeamViewer layout, the remote screen at the top): in
/// portrait at the bottom-trailing corner when that leaves it below the
/// fitted remote image, else (and in landscape) vertically centred. When
/// the height cannot fit the whole pill (e.g. landscape with the keyboard
/// up) its buttons scroll inside it.
/// Collapsed it is a small handle the user drags anywhere; it snaps to the
/// nearest side edge and the position is persisted. Optionally fades to
/// 70% after 3 s without interaction ("Fade toolbar when idle", default
/// off) — purely visual: every tap acts immediately, faded or not.
/// Cmd+Shift+Space toggles it (SessionHostingController). While the soft
/// keyboard or the key accessory bar is up (`input.keyInputActive`) the
/// toolbar collapses to its handle (tap to expand over the keyboard).
///
/// The pill / handle report their frames (`reportsChromeFrame`) so the
/// remote surface underneath never takes touches meant for them.
struct FloatingToolbar: View {
    @ObservedObject var chrome: SessionChrome
    /// Publishes where the remote image sits at fit (`fitImageFrame`).
    @ObservedObject var input: RemoteInputController
    let isLandscape: Bool
    let onKeyboard: () -> Void
    let onMenu: () -> Void
    let onDisconnect: () -> Void

    @AppStorage(SettingsKey.toolbarHandleEdge) private var handleEdge = "trailing"
    @AppStorage(SettingsKey.toolbarHandleY) private var handleY: Double = 0.7
    @State private var dragOffset: CGSize = .zero
    /// Expanded by the user while the keyboard is up (reset per keyboard).
    @State private var expandedOverKeyboard = false
    /// The on-screen keyboard (incl. the key bar shown alone) as iOS
    /// reports it, animated with iOS's own keyboard timing.
    @StateObject private var keyboard = KeyboardTracker()

    private static let target: CGFloat = 44
    private static let handleHeight: CGFloat = 64
    private static let buttonCount: CGFloat = 4
    private static let spacing: CGFloat = 2
    private static let pillInset: CGFloat = 4
    private static let edgeGap: CGFloat = 12
    /// Gap between the keyboard's top edge and the handle / pill.
    private static let keyboardGap: CGFloat = 8
    /// Length of the pill along its axis (4 x 44 + spacing + insets).
    private static let pillLength: CGFloat =
        FloatingToolbar.buttonCount * FloatingToolbar.target
            + (FloatingToolbar.buttonCount - 1) * FloatingToolbar.spacing
            + 2 * FloatingToolbar.pillInset

    var body: some View {
        GeometryReader { geo in
            ZStack {
                if isCollapsed {
                    handle(in: geo)
                } else {
                    expanded(in: geo)
                }
            }
            .frame(width: geo.size.width, height: geo.size.height)
        }
        // Don't resize with the keyboard: the area changing size mid-way
        // through the collapse spring made the animation jumpy. Positions
        // used while the keyboard is up are chosen to stay above it.
        .ignoresSafeArea(.keyboard, edges: .bottom)
        .opacity(chrome.toolbarFaded ? SessionChrome.fadedOpacity : 1)
        .animation(.easeInOut(duration: 0.3), value: chrome.toolbarFaded)
        .animation(.spring(response: 0.32, dampingFraction: 0.86), value: isCollapsed)
        .onChange(of: keyboardUp) { _ in
            // Each keyboard appearance starts collapsed again.
            expandedOverKeyboard = false
        }
    }

    /// The keyboard (or the key bar alone) is on screen. iOS's keyboard
    /// notification drives this, so the collapse starts together with the
    /// keyboard animation (the same spring as a manual collapse).
    private var keyboardUp: Bool {
        keyboard.up
    }

    /// Keyboard top edge in this view's local coordinates (the view ignores
    /// the keyboard safe area, so it keeps its full height).
    private func keyboardTop(in geo: GeometryProxy) -> CGFloat {
        let global = geo.frame(in: .global)
        let screenHeight = UIScreen.main.bounds.height
        return screenHeight - keyboard.height - global.minY
    }

    /// While the soft keyboard / key bar is up the toolbar collapses to its
    /// normal handle (the user's own collapsed setting is untouched and
    /// applies again when the keyboard hides). Tapping the handle expands
    /// it over the keyboard until the keyboard goes away.
    private var isCollapsed: Bool {
        chrome.toolbarCollapsed || (keyboardUp && !expandedOverKeyboard)
    }

    private func setCollapsed(_ collapsed: Bool) {
        if keyboardUp {
            expandedOverKeyboard = !collapsed
        } else {
            chrome.toolbarCollapsed = collapsed
        }
    }

    // MARK: Expanded pill

    /// The vertical pill on the trailing edge: bottom-trailing below the
    /// image when there is room (portrait), else vertically centred; its
    /// buttons scroll when the edge is shorter than the pill.
    private func expanded(in geo: GeometryProxy) -> some View {
        let size = geo.size
        let room = max(size.height - 2 * Self.edgeGap, 0)
        let fits = room >= Self.pillLength
        let below = fits && !isLandscape && fitsBelowImage(in: geo)
        // Too short: the button column scrolls inside a pill filling the
        // edge (at least one button tall).
        let scrollLength: CGFloat? = fits ? nil : max(room - 2 * Self.pillInset, Self.target)
        if keyboardUp {
            // Expanded over the keyboard: sits just above it on the trailing
            // edge, scrolling if the space above the keyboard is too short.
            let kbTop = keyboardTop(in: geo)
            let above = max(kbTop - 2 * Self.edgeGap, Self.target)
            let length: CGFloat? = above >= Self.pillLength ? nil : max(above - 2 * Self.pillInset, Self.target)
            return AnyView(pill(scrollLength: length)
                .padding(.trailing, Self.edgeGap)
                .padding(.bottom, max(size.height - kbTop, 0) + Self.keyboardGap)
                .frame(width: size.width, height: size.height, alignment: .bottomTrailing))
        }
        return AnyView(pill(scrollLength: scrollLength)
            .padding(.trailing, Self.edgeGap)
            .padding(.bottom, below ? Self.edgeGap : 0)
            .frame(width: size.width, height: size.height,
                   alignment: below ? .bottomTrailing : .trailing))
    }

    /// Portrait: the pill docked bottom-trailing stays entirely below the
    /// remote image at fit (the surface publishes its window frame).
    private func fitsBelowImage(in geo: GeometryProxy) -> Bool {
        let image = input.fitImageFrame
        guard !image.isNull, !image.isEmpty else { return false }
        // Both in window (global) coordinates -> this view's local space.
        let imageBottom = image.maxY - geo.frame(in: .global).minY
        let pillTop = geo.size.height - Self.edgeGap - Self.pillLength
        return pillTop >= imageBottom + Self.edgeGap
    }

    private func pill(scrollLength: CGFloat?) -> some View {
        Group {
            if let length = scrollLength {
                ScrollView(.vertical, showsIndicators: false) {
                    buttonColumn
                }
                .frame(width: Self.target, height: length)
            } else {
                buttonColumn
            }
        }
        .padding(Self.pillInset)
        .background(.ultraThinMaterial, in: Capsule())
        .overlay(Capsule().stroke(Color.white.opacity(0.15), lineWidth: 0.5))
        .shadow(color: Color.black.opacity(0.35), radius: 8, y: 3)
        .reportsChromeFrame()
    }

    private var buttonColumn: some View {
        VStack(spacing: Self.spacing) {
            buttons
        }
    }

    @ViewBuilder
    private var buttons: some View {
        toolButton("keyboard", label: "Keyboard", action: onKeyboard)
        toolButton("ellipsis.circle", label: "Session menu", action: onMenu)
        toolButton("xmark", label: "Disconnect", tint: Color(red: 1, green: 0.42, blue: 0.4), action: onDisconnect)
        toolButton("chevron.right", label: "Collapse toolbar") {
            setCollapsed(true)
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

    private func handle(in geo: GeometryProxy) -> some View {
        let size = geo.size
        // Keyboard-collapsed: always the trailing edge, high enough to stay
        // clear of the keyboard (the layout ignores the keyboard, so the
        // height here is the full screen).
        let keyboardHandle = keyboardUp
        let onTrailing = keyboardHandle || handleEdge != "leading"
        let halfWidth = Self.target / 2
        let halfHeight = Self.handleHeight / 2
        let x = onTrailing ? size.width - halfWidth : halfWidth
        let maxY = max(halfHeight, size.height - halfHeight)
        // Keyboard up: just above the keyboard (follows it as it moves).
        let y = keyboardHandle
            ? max(keyboardTop(in: geo) - Self.keyboardGap - halfHeight, halfHeight)
            : min(max(CGFloat(handleY) * size.height, halfHeight), maxY)
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
                setCollapsed(false)
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

/// The software keyboard's on-screen height (0 when hidden), from iOS's
/// keyboard frame notifications, applied with the keyboard's own animation
/// duration so views tracking it move in step with the keyboard. Includes
/// an input accessory view (the key bar), also when it is shown alone.
final class KeyboardTracker: ObservableObject {
    /// The keyboard is (coming) up. Set before `height` animates, so a
    /// view switching on it (the toolbar handle) is inserted at the bottom
    /// and then rides up with the keyboard; cleared after it went down.
    @Published private(set) var up = false
    @Published private(set) var height: CGFloat = 0
    private var observers: [NSObjectProtocol] = []
    private var hideWork: DispatchWorkItem?

    init() {
        let center = NotificationCenter.default
        observers.append(center.addObserver(forName: UIResponder.keyboardWillChangeFrameNotification,
                                            object: nil, queue: .main) { [weak self] note in
            self?.update(note)
        })
        observers.append(center.addObserver(forName: UIResponder.keyboardWillHideNotification,
                                            object: nil, queue: .main) { [weak self] note in
            self?.update(note, hiding: true)
        })
    }

    deinit {
        observers.forEach(NotificationCenter.default.removeObserver)
    }

    /// iOS animates the keyboard with a spring (UIKit curve 7); these are
    /// its parameters, so the handle tracks the keyboard frame for frame.
    /// (The notification's duration is shorter than the spring's settle.)
    private static func keyboardAnimation(_ duration: Double) -> Animation {
        _ = duration
        return .interpolatingSpring(mass: 3, stiffness: 1000, damping: 500, initialVelocity: 0)
    }

    private func update(_ note: Notification, hiding: Bool = false) {
        let info = note.userInfo ?? [:]
        let end = (info[UIResponder.keyboardFrameEndUserInfoKey] as? NSValue)?.cgRectValue ?? .zero
        let screen = UIScreen.main.bounds
        let newHeight = hiding ? 0 : max(screen.maxY - end.minY, 0)
        let duration = (info[UIResponder.keyboardAnimationDurationUserInfoKey] as? NSNumber)?.doubleValue ?? 0.25
        let animation = Self.keyboardAnimation(duration)
        hideWork?.cancel()
        hideWork = nil
        if newHeight > 0 {
            if !up {
                // Insert the handle at the bottom first (height still 0),
                // then animate it up together with the keyboard.
                // Insert the handle at the bottom first (a newly inserted
                // view appears at its final layout), then slide it up with
                // the keyboard's spring on the next frame.
                up = true
                DispatchQueue.main.async { [weak self] in
                    withAnimation(animation) { self?.height = newHeight }
                }
            } else {
                withAnimation(animation) { height = newHeight }
            }
        } else if up {
            // Slide the handle down with the keyboard, then restore the
            // normal toolbar once it is gone.
            withAnimation(animation) { height = 0 }
            let work = DispatchWorkItem { [weak self] in
                self?.up = false
            }
            hideWork = work
            // The keyboard spring settles in about 0.5 s.
            DispatchQueue.main.asyncAfter(deadline: .now() + max(duration, 0.45), execute: work)
        }
    }
}
