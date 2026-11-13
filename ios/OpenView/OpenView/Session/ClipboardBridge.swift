import Foundation
import UIKit

// Clipboard sync: mirrors `ovapp::clipboard::ClipboardSync` on the
// Rust side. The iOS shell does not run the Rust sync state, so it keeps
// its own echo guard: text just applied from the peer (or just sent) is
// never re-sent, breaking the apply -> poll -> send echo loop. Main only.
final class ClipboardBridge {
    private var lastSeenLocal: String?
    private var lastRemote: String?
    private var lastChangeCount: Int

    /// Wire the channel-0 send path (CLIPBOARD command builder).
    var send: ((String) -> Void)?

    init() {
        // Seed with the current pasteboard generation so pre-existing
        // clipboard content is not blasted to the peer on connect.
        lastChangeCount = UIPasteboard.general.changeCount
    }

    /// Peer CLIPBOARD command arrived: apply it to the pasteboard and arm
    /// the echo guard. Unknown kinds are ignored (only 0 = text exists).
    func noteRemote(kind: UInt8, payload: Data) {
        guard kind == 0, let text = String(data: payload, encoding: .utf8) else { return }
        lastRemote = text
        lastSeenLocal = text
        UIPasteboard.general.string = text
        lastChangeCount = UIPasteboard.general.changeCount
    }

    /// Poll the local pasteboard. `changeCount` is cheap and never triggers
    /// the paste prompt; the text itself is read only after it changed (and
    /// only if the pasteboard holds text). Fires `send` when a genuinely new
    /// local clipboard text appears.
    func pollLocal() {
        let pb = UIPasteboard.general
        let count = pb.changeCount
        guard count != lastChangeCount else { return }
        lastChangeCount = count
        guard pb.hasStrings, let text = pb.string else { return }
        guard text != lastSeenLocal, text != lastRemote else { return }
        lastSeenLocal = text
        send?(text)
    }
}
