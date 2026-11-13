import Foundation
import UIKit

// Outbound side of SessionController: channel-0 commands (input, quality,
// monitor, desktop effects, clipboard) and chat on channel 1. Public entry
// points may be called from main; the actual sends hop to
// `OvQueues.session`.

extension SessionController {
    // MARK: Desktop effects

    /// The host can hide its wallpaper / animations: send the saved wishes
    /// (only the "on" ones — the host leaves its desktop alone otherwise).
    func sendDesktopWishes(_ session: OvSessionBox) throws {
        let wishes: [(UInt8, Bool)] = [(1, AppSettings.hideWallpaper), (2, AppSettings.disableAnimations)]
        for (key, on) in wishes where on {
            if let payload = OvSessionBox.optionRecord(key: key, on: on) {
                try session.sendRecord(channel: 0, payload: payload)
            }
        }
    }

    /// Session menu: one desktop-effect switch changed (main). `key` 1 =
    /// hide wallpaper, 2 = disable animations. Ignored for hosts without
    /// CAP_DESKTOP_FX.
    func setDesktopOption(key: UInt8, on: Bool) {
        guard hostSupportsDesktopFx, let payload = OvSessionBox.optionRecord(key: key, on: on) else { return }
        sendCommand(payload)
    }

    // MARK: Outbound (any thread -> session queue)

    /// Send one chat line on channel 1 (shown right away in the chat).
    func sendChat(_ text: String) {
        guard !text.isEmpty else { return }
        appendChat(text, fromMe: true)
        queue.async { [weak self] in
            guard let self else { return }
            guard !self.halted, let session = self.session else {
                self.log("Chat message not sent: not connected")
                return
            }
            do {
                try session.sendRecord(channel: 1, payload: Data(text.utf8))
            } catch {
                self.handle(error, context: "chat send failed")
            }
        }
    }

    /// Shared command-channel send: errors (e.g. not yet established) are
    /// surfaced in the session log.
    func sendCommand(_ payload: Data) {
        queue.async { [weak self] in
            guard let self, !self.halted, let session = self.session else { return }
            do {
                try session.sendRecord(channel: 0, payload: payload)
            } catch {
                self.handle(error, context: "input failed")
            }
        }
    }


    // MARK: Display / quality

    /// Send one CAPTURE_REGION command: i32-BE x, i32-BE y,
    /// u32-BE w, u32-BE h — the monitor rect in virtual-desktop coords.
    func sendCaptureRegion(monitorAt index: Int) {
        guard monitors.indices.contains(index) else { return }
        let m = monitors[index]
        var payload = Data([0x37])
        payload.append(contentsOf: withUnsafeBytes(of: m.x.bigEndian) { Data($0) })
        payload.append(contentsOf: withUnsafeBytes(of: m.y.bigEndian) { Data($0) })
        payload.append(contentsOf: withUnsafeBytes(of: m.w.bigEndian) { Data($0) })
        payload.append(contentsOf: withUnsafeBytes(of: m.h.bigEndian) { Data($0) })
        sendCommand(payload)
    }

    /// Send one QUALITY_SET command: 2-byte u16-BE target fps,
    /// 0 = AUTO (the host's RTT-aware governor picks the rate).
    func sendQuality(targetFps: UInt16) {
        var payload = Data([0x36])
        payload.append(contentsOf: withUnsafeBytes(of: targetFps.bigEndian) { Data($0) })
        sendCommand(payload)
    }

    // MARK: Clipboard

    /// Push the local clipboard text to the host now (session menu).
    func sendClipboardNow() {
        guard let text = UIPasteboard.general.string, !text.isEmpty else {
            toast = "The clipboard has no text"
            return
        }
        sendClipboardPayload(text)
        toast = "Clipboard sent to the host"
    }

    /// Type the local clipboard text into the host (for apps that ignore
    /// clipboard sync, e.g. password fields).
    func typeClipboardText() {
        guard let text = UIPasteboard.general.string, !text.isEmpty else {
            toast = "The clipboard has no text"
            return
        }
        input.typeText(text)
    }

    func sendClipboardPayload(_ text: String) {
        var payload = Data([0x50, 0]) // CLIPBOARD, kind 0 = text
        payload.append(contentsOf: Array(text.utf8))
        sendCommand(payload)
    }

    // MARK: Clipboard polling (main thread)

    /// Check the pasteboard twice a second. `pollLocal` compares the cheap
    /// `changeCount` first and only reads the text (which may show the paste
    /// prompt) when it actually changed.
    func startClipboardPolling() {
        let timer = DispatchSource.makeTimerSource(queue: .main)
        timer.schedule(deadline: .now() + .milliseconds(500),
                       repeating: .milliseconds(500),
                       leeway: .milliseconds(100))
        timer.setEventHandler { [weak self] in
            self?.clipboard.pollLocal()
        }
        clipboardTimer = timer
        timer.resume()
    }
}
