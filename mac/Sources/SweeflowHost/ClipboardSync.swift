import AppKit

/// One clipboard representation on the wire: text, a PNG image, a file (name + contents), or a
/// file named only (`fileRef`: its contents fetched when it's pasted, see ClipboardSync.files).
struct ClipItem {
    enum Kind: String { case text = "text/plain;charset=utf-8", png = "image/png", file = "file", fileRef = "file-ref" }
    let kind: Kind
    let name: String
    let data: Data
}

extension MsgWriter {
    /// A clipboard item list as the wire has it: u16 count, then (str kind, str name, bytes data) each.
    mutating func items(_ items: [ClipItem]) {
        u16(UInt16(items.count))
        for it in items { str(it.kind.rawValue); str(it.name); bytes(it.data) }
    }
}

/// Mirrors the Mac clipboard with the Linux one: text, images and files, both directions.
final class ClipboardSync {
    /// Called with the new Mac clipboard contents (on a background queue).
    var onChange: (([ClipItem]) -> Void)?
    private var lastChange = NSPasteboard.general.changeCount
    private var timer: Timer?
    /// The files on the clipboard as last told to Linux, by number (main thread). Not read
    /// until Linux pastes them: reading a file in iCloud Drive, Documents, Desktop or Downloads
    /// makes macOS ask whether Sweeflow may, and it asked on every copy of one, pasted on
    /// Linux or not (a copy in the Mac's own Finder too).
    private var offered: (number: UInt32, urls: [URL]) = (0, [])
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
        let urls = pb.readObjects(forClasses: [NSURL.self], options: [.urlReadingFileURLsOnly: true]) as? [URL] ?? []
        var items: [ClipItem] = []
        if !urls.isEmpty {
            // Named only, numbered so a paste asks for these and not a later copy's. As text,
            // their names (what a copied file pastes as on the Mac too).
            offered = (offered.number &+ 1, urls)
            var number = offered.number.littleEndian
            let tag = Data(bytes: &number, count: 4)
            items = urls.map { ClipItem(kind: .fileRef, name: Self.wireName($0), data: tag) }
            items.append(ClipItem(kind: .text, name: "", data: Data(urls.map(\.lastPathComponent).joined(separator: "\n").utf8)))
        } else {
            offered = (offered.number, [])
            if let png = pb.data(forType: .png) {
                items.append(ClipItem(kind: .png, name: "", data: png))
            } else if let tiff = pb.data(forType: .tiff), let rep = NSBitmapImageRep(data: tiff),
                      let png = rep.representation(using: .png, properties: [:]) {
                items.append(ClipItem(kind: .png, name: "", data: png))
            }
            if let text = pb.string(forType: .string) { items.append(ClipItem(kind: .text, name: "", data: Data(text.utf8))) }
        }
        guard !items.isEmpty else { return }
        let send = onChange
        DispatchQueue.global(qos: .userInitiated).async { send?(items) }
    }

    /// A file's name as it goes across: a folder's as the zip it travels as. Told by the URL
    /// alone (Finder's end in a slash for folders): looking at the file is what macOS asks about.
    private static func wireName(_ url: URL) -> String {
        url.hasDirectoryPath ? url.lastPathComponent + ".zip" : url.lastPathComponent
    }

    /// The files of copy `number`, read now that Linux pastes them (nil: the clipboard has
    /// changed since, or they're too big). `done` is called on a background queue.
    func files(_ number: UInt32, done: @escaping ([ClipItem]?) -> Void) {
        DispatchQueue.main.async {
            guard self.offered.number == number, !self.offered.urls.isEmpty else { return done(nil) }
            let urls = self.offered.urls
            DispatchQueue.global(qos: .userInitiated).async {
                var items: [ClipItem] = []
                var total = 0
                for url in urls {
                    let data: Data?
                    if url.hasDirectoryPath {
                        // Folders travel as a zip.
                        let zip = FileManager.default.temporaryDirectory.appendingPathComponent(url.lastPathComponent + ".zip")
                        try? FileManager.default.removeItem(at: zip)
                        data = (try? run("/usr/bin/ditto", ["-c", "-k", "--keepParent", url.path, zip.path])) != nil
                            ? try? Data(contentsOf: zip) : nil
                        try? FileManager.default.removeItem(at: zip)
                    } else {
                        data = try? Data(contentsOf: url, options: .mappedIfSafe)
                    }
                    guard let data else {
                        log("clipboard: \(url.lastPathComponent) couldn't be read (macOS's privacy settings for Sweeflow?)")
                        continue
                    }
                    total += data.count
                    if total > Self.maxBytes { log("clipboard: files too large to copy across (limit 512 MB)"); return done(nil) }
                    items.append(ClipItem(kind: .file, name: Self.wireName(url), data: data))
                }
                done(items)
            }
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
