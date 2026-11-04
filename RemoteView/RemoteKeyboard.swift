import QuartzCore
import UIKit

// Remote keyboard (section 2):
// - `HIDKeyMap`: USB HID usage (UIKey.keyCode) -> Windows vk + set-1 scancode.
// - `HardwareKeyRouter`: hardware-keyboard presses -> KEY_EX down/up (no
//   synthesized repeats: auto-repeat is the host's job), with the Cmd ->
//   Ctrl mapping.
// - `RemoteKeyInputView`: an invisible UIKeyInput responder that brings up
//   the soft keyboard and/or the KeyAccessoryBar, and forwards typed text.

// MARK: - HID usage -> Windows key

enum HIDKeyMap {
    /// HID usages of the GUI (Cmd) keys.
    static let leftGUI = 0xE3
    static let rightGUI = 0xE7
    /// HID usage of '.' (Cmd+. = Esc).
    static let period = 0x37

    /// Modifier usages (LeftControl...RightGUI).
    static func isModifier(_ usage: Int) -> Bool {
        usage >= 0xE0 && usage <= 0xE7
    }

    static func stroke(for usage: Int) -> KeyStroke? {
        table[usage]
    }

    private static let table: [Int: KeyStroke] = {
        var t: [Int: KeyStroke] = [:]
        // Letters a-z: 0x04...0x1D.
        let letters = Array("abcdefghijklmnopqrstuvwxyz".unicodeScalars)
        for (i, s) in letters.enumerated() {
            if let k = KeyStroke.ascii(s) { t[0x04 + i] = k }
        }
        // Digits 1-9, 0: 0x1E...0x27.
        let digits = Array("1234567890".unicodeScalars)
        for (i, s) in digits.enumerated() {
            if let k = KeyStroke.ascii(s) { t[0x1E + i] = k }
        }
        t[0x28] = .enter
        t[0x29] = .escape
        t[0x2A] = .backspace
        t[0x2B] = .tab
        t[0x2C] = .space
        t[0x2D] = KeyStroke(0xBD, 0x0C) // - _
        t[0x2E] = KeyStroke(0xBB, 0x0D) // = +
        t[0x2F] = KeyStroke(0xDB, 0x1A) // [ {
        t[0x30] = KeyStroke(0xDD, 0x1B) // ] }
        t[0x31] = KeyStroke(0xDC, 0x2B) // \ |
        t[0x32] = KeyStroke(0xDC, 0x2B) // non-US # (same position as \)
        t[0x33] = KeyStroke(0xBA, 0x27) // ; :
        t[0x34] = KeyStroke(0xDE, 0x28) // ' "
        t[0x35] = KeyStroke(0xC0, 0x29) // ` ~
        t[0x36] = KeyStroke(0xBC, 0x33) // , <
        t[0x37] = KeyStroke(0xBE, 0x34) // . >
        t[0x38] = KeyStroke(0xBF, 0x35) // / ?
        t[0x39] = KeyStroke(0x14, 0x3A) // Caps Lock
        // F1...F12: 0x3A...0x45.
        for n in 1...12 {
            if let k = KeyStroke.function(n) { t[0x3A + n - 1] = k }
        }
        t[0x46] = .printScreen
        t[0x47] = KeyStroke(0x91, 0x46) // Scroll Lock
        t[0x48] = KeyStroke(0x13, 0)    // Pause (E1 sequence: host derives)
        t[0x49] = .insert
        t[0x4A] = .home
        t[0x4B] = .pageUp
        t[0x4C] = .delete
        t[0x4D] = .end
        t[0x4E] = .pageDown
        t[0x4F] = .right
        t[0x50] = .left
        t[0x51] = .down
        t[0x52] = .up
        t[0x53] = KeyStroke(0x90, 0)    // Num Lock (host derives the scancode)
        t[0x54] = KeyStroke(0x6F, 0x35, ext: true) // keypad /
        t[0x55] = KeyStroke(0x6A, 0x37) // keypad *
        t[0x56] = KeyStroke(0x6D, 0x4A) // keypad -
        t[0x57] = KeyStroke(0x6B, 0x4E) // keypad +
        t[0x58] = KeyStroke(0x0D, 0x1C, ext: true) // keypad Enter
        t[0x59] = KeyStroke(0x61, 0x4F) // keypad 1
        t[0x5A] = KeyStroke(0x62, 0x50) // keypad 2
        t[0x5B] = KeyStroke(0x63, 0x51) // keypad 3
        t[0x5C] = KeyStroke(0x64, 0x4B) // keypad 4
        t[0x5D] = KeyStroke(0x65, 0x4C) // keypad 5
        t[0x5E] = KeyStroke(0x66, 0x4D) // keypad 6
        t[0x5F] = KeyStroke(0x67, 0x47) // keypad 7
        t[0x60] = KeyStroke(0x68, 0x48) // keypad 8
        t[0x61] = KeyStroke(0x69, 0x49) // keypad 9
        t[0x62] = KeyStroke(0x60, 0x52) // keypad 0
        t[0x63] = KeyStroke(0x6E, 0x53) // keypad .
        t[0x64] = KeyStroke(0xE2, 0x56) // non-US \ (VK_OEM_102)
        t[0x65] = KeyStroke(0x5D, 0x5D, ext: true) // Application / Menu
        // F13...F24: 0x68...0x73.
        for n in 13...24 {
            if let k = KeyStroke.function(n) { t[0x68 + n - 13] = k }
        }
        t[0x87] = KeyStroke(0xC1, 0x73) // International1 (ABNT / Ro)
        t[0x89] = KeyStroke(0xDC, 0x7D) // International3 (Yen)
        // Modifiers.
        t[0xE0] = .leftCtrl
        t[0xE1] = .leftShift
        t[0xE2] = .leftAlt
        t[0xE3] = .leftWin
        t[0xE4] = KeyStroke(0xA3, 0x1D, ext: true) // right Ctrl
        t[0xE5] = KeyStroke(0xA1, 0x36)            // right Shift
        t[0xE6] = KeyStroke(0xA5, 0x38, ext: true) // right Alt (AltGr)
        t[0xE7] = KeyStroke(0x5C, 0x5C, ext: true) // right Win
        return t
    }()
}

// MARK: - Hardware key router

/// The selector UIKeyCommands fire; both keyboard responders implement it.
@objc protocol RemoteKeyCommandTarget {
    @objc func ovKeyCommand(_ sender: UIKeyCommand)
}

/// Routes hardware-keyboard presses to the input controller.
///
/// Cmd handling (setting "ov.cmdAsCtrl", default on): Cmd itself is not
/// forwarded when it goes down. While it is held,
///   - C V X Z A F S Y W T R P N O go out as Ctrl+<key> (Ctrl is pressed
///     once and released when Cmd goes up),
///   - '.' sends Esc,
///   - any other key goes out as Win+<key> (Win pressed once, released
///     with Cmd).
/// Cmd pressed and released alone sends a Win tap (Start menu). With the
/// setting off, Cmd is simply the Windows key.
///
/// UIKeyCommands for Cmd+C/V/X/Z/A/F/. are registered with
/// `wantsPriorityOverSystemBehavior` so the system does not take them; their
/// action only sends when the press path did not already handle that key
/// (and vice versa), so a chord is never sent twice. Main thread only.
final class HardwareKeyRouter {
    weak var input: RemoteInputController?

    /// HID usage -> the stroke we pressed for it (the up sends the same).
    private var down: [Int: KeyStroke] = [:]
    /// Presses consumed without a key down (Cmd+. -> Esc, deduped chords).
    private var swallowed = Set<Int>()
    /// Held Cmd keys (only while cmdAsCtrl swallows them).
    private var cmdHeld = Set<Int>()
    private var cmdUsed = false
    private var ctrlForCmd = false
    private var winForCmd = false
    /// The last chord a UIKeyCommand sent (usage, time).
    private var commandChord: (usage: Int, time: CFTimeInterval)?

    /// Letters sent as Ctrl+<letter> while Cmd is held.
    private static let ctrlLetters: Set<Int> = {
        let letters = "cvxzafsywtrpno"
        var s = Set<Int>()
        for c in letters.unicodeScalars {
            s.insert(0x04 + Int(c.value - 0x61))
        }
        return s
    }()

    /// Inputs of the registered key commands -> HID usage.
    private static let commandInputs: [String: Int] = [
        "c": 0x06, "v": 0x19, "x": 0x1B, "z": 0x1D, "a": 0x04, "f": 0x09, ".": HIDKeyMap.period,
    ]

    /// Physical keys currently held (insertText ignores text meanwhile, in
    /// case UIKit also turns the press into text).
    var hasKeysDown: Bool {
        !down.isEmpty || !cmdHeld.isEmpty || !swallowed.isEmpty
    }

    private static func isGUI(_ usage: Int) -> Bool {
        usage == HIDKeyMap.leftGUI || usage == HIDKeyMap.rightGUI
    }

    /// Handle presses going down; returns the ones we did not handle (pass
    /// them to super).
    func pressesBegan(_ presses: Set<UIPress>) -> Set<UIPress> {
        var unhandled = Set<UIPress>()
        for press in presses {
            guard let key = press.key else {
                unhandled.insert(press)
                continue
            }
            let usage = key.keyCode.rawValue
            guard let stroke = HIDKeyMap.stroke(for: usage), let input else {
                unhandled.insert(press)
                continue
            }
            keyDown(usage, stroke, input)
        }
        return unhandled
    }

    /// Handle presses going up (ended or cancelled); returns the unhandled.
    func pressesEnded(_ presses: Set<UIPress>) -> Set<UIPress> {
        var unhandled = Set<UIPress>()
        for press in presses {
            guard let key = press.key else {
                unhandled.insert(press)
                continue
            }
            let usage = key.keyCode.rawValue
            guard HIDKeyMap.stroke(for: usage) != nil else {
                unhandled.insert(press)
                continue
            }
            keyUp(usage)
        }
        return unhandled
    }

    private func keyDown(_ usage: Int, _ stroke: KeyStroke, _ input: RemoteInputController) {
        guard down[usage] == nil, !swallowed.contains(usage), !cmdHeld.contains(usage) else { return }
        let cmdAsCtrl = input.cmdAsCtrl

        if Self.isGUI(usage) && cmdAsCtrl {
            if cmdHeld.isEmpty {
                cmdUsed = false
            }
            cmdHeld.insert(usage)
            return
        }

        if !cmdHeld.isEmpty {
            cmdUsed = true
            // A UIKeyCommand already sent this chord.
            if let chord = commandChord, chord.usage == usage,
               CACurrentMediaTime() - chord.time < 0.25 {
                swallowed.insert(usage)
                return
            }
            if usage == HIDKeyMap.period {
                input.tapKey(.escape)
                swallowed.insert(usage)
                return
            }
            if Self.ctrlLetters.contains(usage) {
                if !ctrlForCmd {
                    input.sendKey(.leftCtrl, down: true)
                    ctrlForCmd = true
                }
                input.sendKey(stroke, down: true)
                down[usage] = stroke
                return
            }
            if !winForCmd && !HIDKeyMap.isModifier(usage) {
                input.sendKey(.leftWin, down: true)
                winForCmd = true
            }
        }

        if HIDKeyMap.isModifier(usage) {
            input.sendKey(stroke, down: true)
        } else {
            input.hardwareKey(stroke, down: true)
        }
        down[usage] = stroke
    }

    private func keyUp(_ usage: Int) {
        if swallowed.remove(usage) != nil { return }
        if cmdHeld.remove(usage) != nil {
            guard cmdHeld.isEmpty, let input else { return }
            if ctrlForCmd {
                input.sendKey(.leftCtrl, down: false)
            }
            if winForCmd {
                input.sendKey(.leftWin, down: false)
            }
            if !cmdUsed {
                // Cmd alone: a Windows-key tap.
                input.tapKey(.leftWin)
            }
            ctrlForCmd = false
            winForCmd = false
            cmdUsed = false
            return
        }
        guard let stroke = down.removeValue(forKey: usage), let input else { return }
        if HIDKeyMap.isModifier(usage) {
            input.sendKey(stroke, down: false)
        } else {
            input.hardwareKey(stroke, down: false)
        }
    }

    /// The UIKeyCommands a keyboard responder should return.
    func keyCommands() -> [UIKeyCommand] {
        var out: [UIKeyCommand] = []
        for name in Self.commandInputs.keys.sorted() {
            let cmd = UIKeyCommand(input: name, modifierFlags: .command,
                                   action: #selector(RemoteKeyCommandTarget.ovKeyCommand(_:)))
            cmd.wantsPriorityOverSystemBehavior = true
            out.append(cmd)
        }
        return out
    }

    /// A registered key command fired. If the press path already handled
    /// this key it is a no-op; otherwise the chord is sent here and the
    /// press path will swallow the matching press.
    func handleKeyCommand(_ command: UIKeyCommand) {
        guard let name = command.input?.lowercased(),
              let usage = Self.commandInputs[name],
              let input else { return }
        if down[usage] != nil || swallowed.contains(usage) { return }
        if let chord = commandChord, chord.usage == usage,
           CACurrentMediaTime() - chord.time < 0.25 { return }
        commandChord = (usage: usage, time: CACurrentMediaTime())
        cmdUsed = true
        if usage == HIDKeyMap.period {
            input.tapKey(.escape)
            return
        }
        guard let stroke = HIDKeyMap.stroke(for: usage) else { return }
        if input.cmdAsCtrl {
            // Ctrl may already be held for an earlier chord of this Cmd hold.
            let pressCtrl = !ctrlForCmd
            if pressCtrl { input.sendKey(.leftCtrl, down: true) }
            input.sendKey(stroke, down: true)
            input.sendKey(stroke, down: false)
            if pressCtrl { input.sendKey(.leftCtrl, down: false) }
        } else {
            input.sendKey(.leftWin, down: true)
            input.sendKey(stroke, down: true)
            input.sendKey(stroke, down: false)
            input.sendKey(.leftWin, down: false)
        }
    }

    /// Forget every held key (after RELEASE_ALL / focus loss).
    func reset() {
        down.removeAll()
        swallowed.removeAll()
        cmdHeld.removeAll()
        cmdUsed = false
        ctrlForCmd = false
        winForCmd = false
    }
}

// MARK: - Key input view

/// Invisible first responder for the remote keyboard. `showsSoftKeyboard`
/// false keeps only the accessory bar on screen (CRD trick: an empty
/// zero-height `inputView` replaces the system keyboard).
final class RemoteKeyInputView: UIView, UIKeyInput, RemoteKeyCommandTarget {
    weak var input: RemoteInputController?
    weak var router: HardwareKeyRouter?
    let accessoryBar: KeyAccessoryBar
    private let emptyInputView = UIView(frame: .zero)

    var showsSoftKeyboard = true {
        didSet {
            guard showsSoftKeyboard != oldValue else { return }
            if isFirstResponder { reloadInputViews() }
        }
    }

    // UITextInputTraits: plain keystrokes, no text "help".
    var autocorrectionType: UITextAutocorrectionType = .no
    var autocapitalizationType: UITextAutocapitalizationType = .none
    var spellCheckingType: UITextSpellCheckingType = .no
    var smartQuotesType: UITextSmartQuotesType = .no
    var smartDashesType: UITextSmartDashesType = .no
    var smartInsertDeleteType: UITextSmartInsertDeleteType = .no
    var keyboardType: UIKeyboardType = .default
    var keyboardAppearance: UIKeyboardAppearance = .default
    var returnKeyType: UIReturnKeyType = .default
    var enablesReturnKeyAutomatically: Bool = false

    init(input: RemoteInputController, router: HardwareKeyRouter) {
        self.input = input
        self.router = router
        self.accessoryBar = KeyAccessoryBar(input: input)
        super.init(frame: CGRect(x: 0, y: 0, width: 1, height: 1))
        alpha = 0
        isUserInteractionEnabled = false
        accessoryBar.onDismiss = { [weak self] in
            self?.input?.hideKeyboard()
        }
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    override var canBecomeFirstResponder: Bool { true }

    override var inputView: UIView? {
        showsSoftKeyboard ? nil : emptyInputView
    }

    override var inputAccessoryView: UIView? {
        accessoryBar
    }

    // MARK: UIKeyInput

    /// Always true so the soft keyboard's backspace keeps firing.
    var hasText: Bool { true }

    func insertText(_ text: String) {
        // A hardware key is down: the press path already sent it.
        if let router, router.hasKeysDown { return }
        guard let input else { return }
        if text == "\n" || text == "\r" {
            input.tapKey(.enter)
        } else {
            input.typeText(text)
        }
    }

    func deleteBackward() {
        if let router, router.hasKeysDown { return }
        input?.tapKey(.backspace)
    }

    // MARK: Hardware keys (this view is first responder while the
    // keyboard / bar is up)

    override func pressesBegan(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        let rest = router?.pressesBegan(presses) ?? presses
        if !rest.isEmpty { super.pressesBegan(rest, with: event) }
    }

    override func pressesEnded(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        let rest = router?.pressesEnded(presses) ?? presses
        if !rest.isEmpty { super.pressesEnded(rest, with: event) }
    }

    override func pressesCancelled(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        let rest = router?.pressesEnded(presses) ?? presses
        if !rest.isEmpty { super.pressesCancelled(rest, with: event) }
    }

    override var keyCommands: [UIKeyCommand]? {
        router?.keyCommands()
    }

    @objc func ovKeyCommand(_ sender: UIKeyCommand) {
        router?.handleKeyCommand(sender)
    }
}
