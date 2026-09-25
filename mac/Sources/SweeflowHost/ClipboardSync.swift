import AppKit

/// One clipboard representation on the wire: text, a PNG image, or a file (name + contents).
struct ClipItem {
    enum Kind: String { case text = "text/plain;charset=utf-8", png = "image/png", file = "file" }
    let kind: Kind
    let name: String
    let data: Data
}

/// Mirrors the Mac clipboard with the Linux one: text, images and files, both directions.
final class ClipboardSync {
    /// Called with the new Mac clipboard contents (on a background queue).
    var onChange: (([ClipItem]) -> Void)?
    private var lastChange = NSPasteboard.general.changeCount
    private var timer: Timer?
    /// Largest total we copy across (files included).
    static let maxBytes = 512 << 20

    func start() {
        DispatchQueue.main.async {
            self.lastChange = NSPasteboard.general.changeCount
            // Copies happen after a click or key (see checkSoon); this only catches the rest.
            let t = Timer(timeInterval: 5, repeats: true) { [weak self] _ in self?.poll() }
            RunLoop.main.add(t, forMode: .common)
            self.timer = t
        }
    }

    func stop() {
        DispatchQueue.main.async { self.timer?.invalidate(); self.timer = nil }
    }

    /// Just after a click or key press (⌘C, a Copy menu item): look a little later, once the
    /// app has had time to write the clipboard.
    func checkSoon() {
        for delay in [0.15, 0.6] {
            DispatchQueue.main.asyncAfter(deadline: .now() + delay) { [weak self] in self?.poll() }
        }
    }

    private func poll() {
        let pb = NSPasteboard.general
        guard pb.changeCount != lastChange else { return }
        lastChange = pb.changeCount
        // Grab what's on the clipboard now (cheap), read files off the main thread (can be big).
        let urls = pb.readObjects(forClasses: [NSURL.self], options: [.urlReadingFileURLsOnly: true]) as? [URL] ?? []
        var image: Data?
        if urls.isEmpty {
            if let png = pb.data(forType: .png) {
                image = png
            } else if let tiff = pb.data(forType: .tiff), let rep = NSBitmapImageRep(data: tiff) {
                image = rep.representation(using: .png, properties: [:])
            }
        }
        let text = urls.isEmpty ? pb.string(forType: .string) : nil
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            var items: [ClipItem] = []
            var total = 0
            for url in urls {
                var isDir: ObjCBool = false
                guard FileManager.default.fileExists(atPath: url.path, isDirectory: &isDir) else { continue }
                var name = url.lastPathComponent
                let data: Data?
                if isDir.boolValue {
                    // Folders travel as a zip.
                    let zip = FileManager.default.temporaryDirectory.appendingPathComponent(name + ".zip")
                    try? FileManager.default.removeItem(at: zip)
                    data = (try? run("/usr/bin/ditto", ["-c", "-k", "--keepParent", url.path, zip.path])) != nil
                        ? try? Data(contentsOf: zip) : nil
                    try? FileManager.default.removeItem(at: zip)
                    name += ".zip"
                } else {
                    data = try? Data(contentsOf: url, options: .mappedIfSafe)
                }
                guard let data else { continue }
                total += data.count
                if total > Self.maxBytes { log("clipboard: files too large to copy across (limit 512 MB)"); return }
                items.append(ClipItem(kind: .file, name: name, data: data))
            }
            if let image { items.append(ClipItem(kind: .png, name: "", data: image)) }
            if let text { items.append(ClipItem(kind: .text, name: "", data: Data(text.utf8))) }
            guard !items.isEmpty else { return }
            self?.onChange?(items)
        }
    }

    /// Puts the Linux clipboard on the Mac clipboard. Files live in a private cache (replaced on
    /// the next copy) and only appear where they're pasted.
    func apply(_ items: [ClipItem]) {
        DispatchQueue.main.async {
            let pb = NSPasteboard.general
            pb.clearContents()
            var objects: [NSPasteboardWriting] = []
            let files = items.filter { $0.kind == .file }
            if !files.isEmpty {
                let cache = FileManager.default.homeDirectoryForCurrentUser
                    .appendingPathComponent("Library/Caches/sweeflow/clipboard")
                try? FileManager.default.removeItem(at: cache)
                let dir = cache.appendingPathComponent(String(Int(Date().timeIntervalSince1970 * 1000)))
                try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
                for f in files {
                    let name = (f.name as NSString).lastPathComponent
                    let url = self.unique(dir.appendingPathComponent(name.isEmpty ? "file" : name))
                    if (try? f.data.write(to: url)) != nil { objects.append(url as NSURL) }
                }
            }
            if let png = items.first(where: { $0.kind == .png }), let image = NSImage(data: png.data) {
                objects.append(image)
            }
            if !objects.isEmpty { pb.writeObjects(objects) }
            if let text = items.first(where: { $0.kind == .text }) {
                pb.setString(String(decoding: text.data, as: UTF8.self), forType: .string)
            }
            // Don't send our own write straight back.
            self.lastChange = pb.changeCount
            log("clipboard from Linux: \(items.map { $0.kind == .file ? $0.name : $0.kind.rawValue })")
        }
    }

    private func unique(_ url: URL) -> URL {
        var candidate = url
        var n = 2
        while FileManager.default.fileExists(atPath: candidate.path) {
            let base = url.deletingPathExtension().lastPathComponent, ext = url.pathExtension
            candidate = url.deletingLastPathComponent().appendingPathComponent(ext.isEmpty ? "\(base) \(n)" : "\(base) \(n).\(ext)")
            n += 1
        }
        return candidate
    }
}
