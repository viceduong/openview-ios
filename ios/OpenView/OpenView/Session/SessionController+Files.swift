import Foundation

// File transfer for SessionController: sending a picked document through
// the Rust chunking path, and storing files the host sent.

extension SessionController {
    /// Read the picked document (security-scoped) and
    /// ship it through the Rust chunking path, both off main. Errors land
    /// in the session log.
    func sendFile(url: URL) {
        queue.async { [weak self] in
            guard let self else { return }
            let data: Data
            do {
                let scoped = url.startAccessingSecurityScopedResource()
                defer { if scoped { url.stopAccessingSecurityScopedResource() } }
                data = try Data(contentsOf: url)
            } catch {
                self.log("File read failed: \(error.localizedDescription)")
                self.onMain { $0.toast = "Couldn't read \(url.lastPathComponent)" }
                return
            }
            guard !self.halted, let session = self.session else {
                self.log("File send failed: not connected")
                self.onMain { $0.toast = "Not connected — file not sent" }
                return
            }
            do {
                try session.sendFile(name: url.lastPathComponent, data: data)
                self.log("File sent: \(url.lastPathComponent) (\(data.count) bytes)")
                self.onMain { $0.toast = "Sent \(url.lastPathComponent)" }
            } catch {
                self.handle(error, context: "file send failed")
            }
        }
    }

    /// A completed peer transfer (session queue, from the screen file
    /// sink): write it into the app's Documents folder (never overwriting
    /// an earlier file of the same name) and list it in the received files.
    func receiveFile(name: String, data: Data) {
        let dir = FileManager.default.urls(for: .documentDirectory,
                                           in: .userDomainMask).first
            ?? FileManager.default.temporaryDirectory
        let url = Self.uniqueURL(in: dir, name: name)
        do {
            try data.write(to: url, options: .atomic)
            let file = ReceivedFile(name: url.lastPathComponent, url: url,
                                    size: data.count, date: Date())
            onMain { $0.noteReceivedFile(file) }
            log("File received: \(url.lastPathComponent) (\(data.count) bytes)")
        } catch {
            log("File write failed: \(error.localizedDescription)")
        }
    }

    /// A file URL in `dir` for `name` that does not exist yet: "a.txt",
    /// then "a (1).txt", "a (2).txt", …
    static func uniqueURL(in dir: URL, name: String) -> URL {
        let fm = FileManager.default
        let base = name.isEmpty ? "file" : name
        var url = dir.appendingPathComponent(base)
        guard fm.fileExists(atPath: url.path) else { return url }
        let ns = base as NSString
        let ext = ns.pathExtension
        let stem = ns.deletingPathExtension
        var n = 1
        repeat {
            let candidate = ext.isEmpty ? "\(stem) (\(n))" : "\(stem) (\(n)).\(ext)"
            url = dir.appendingPathComponent(candidate)
            n += 1
        } while fm.fileExists(atPath: url.path) && n < 10_000
        return url
    }
}
