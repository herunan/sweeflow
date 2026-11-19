import AppKit
import CryptoKit

/// Updates. Each release has `latest.json` (linux/release/build.sh): its version, what's new, and
/// where the Mac app is. Sweeflow looks a little after it starts and every few hours. A newer one
/// is said once in a notification, and the menu's line turns from "Check for updates" into
/// "Update to Sweeflow X". Chosen, it shows what's new while it downloads the disk image, checks
/// it (its checksum, and that the app in it is signed as this one is), swaps it in and opens it.
final class Updater: NSObject {
    static let shared = Updater()

    struct Feed: Decodable {
        struct Part: Decodable { let version: String; let build: Int; let url: String; let sha256: String }
        let version: String
        let notes: String?
        let page: String?
        /// When it was released ("2026-11-19").
        let date: String?
        let mac: Part?
    }

    static var version: String { Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "0" }
    static var build: Int { Int(Bundle.main.object(forInfoDictionaryKey: "CFBundleVersion") as? String ?? "") ?? 0 }

    private static let feedURL = "https://github.com/herunan/sweeflow/releases/latest/download/latest.json"
    /// How often to look, after the first look a little after start.
    private static let every: TimeInterval = 6 * 3600

    /// A newer release, once seen (main thread).
    private(set) var available: Feed?
    private var window: UpdateWindow?
    private var timer: Timer?

    /// Whether `theirs` is newer than `ours`. A build of 0 is a development build: only a higher
    /// version counts as newer then.
    static func newer(_ theirs: (String, Int), than ours: (String, Int)) -> Bool {
        let nums = { (v: String) in v.split(whereSeparator: { !$0.isNumber }).map { Int($0) ?? 0 } }
        let (a, b) = (nums(theirs.0), nums(ours.0))
        for i in 0..<max(a.count, b.count) {
            let x = i < a.count ? a[i] : 0, y = i < b.count ? b[i] : 0
            if x != y { return x > y }
        }
        return ours.1 != 0 && theirs.1 > ours.1
    }

    /// Looks now and again. Main thread.
    func start() {
        Notices.shared.onClick["update"] = { [weak self] in
            if let feed = self?.available { self?.showAndInstall(feed) }
        }
        sayIfUpdated()
        DispatchQueue.main.asyncAfter(deadline: .now() + 30) { [weak self] in self?.check(manual: false) }
        timer = Timer.scheduledTimer(withTimeInterval: Self.every, repeats: true) { [weak self] _ in self?.check(manual: false) }
    }

    /// The menu's line: what it says, and what it does.
    var menuTitle: String { available.map { "Update to Sweeflow \($0.version)" } ?? "Check for updates" }

    func menuChosen() {
        if let feed = available { return showAndInstall(feed) }
        check(manual: true)
    }

    /// Looks at the latest release. `manual`: chosen from the menu, so the answer is shown either
    /// way (a window with what's new, or that this is the latest).
    func check(manual: Bool) {
        if manual { shownWindow().checking() }
        let url = UserDefaults.standard.string(forKey: "updateFeed").flatMap(URL.init(string:)) ?? URL(string: Self.feedURL)!
        URLSession.shared.dataTask(with: URLRequest(url: url, cachePolicy: .reloadIgnoringLocalCacheData, timeoutInterval: 30)) { data, response, error in
            let feed = data.flatMap { try? JSONDecoder().decode(Feed.self, from: $0) }
            DispatchQueue.main.async { [self] in
                guard let feed, let mac = feed.mac, (response as? HTTPURLResponse)?.statusCode ?? 200 < 400 else {
                    log("update check: \(error.map { "\($0)" } ?? "no release found")")
                    if manual { self.shownWindow().failed(checking: true, "Sweeflow couldn't reach github.com. Check you're online, then try again.") }
                    return
                }
                guard Self.newer((mac.version, mac.build), than: (Self.version, Self.build)) else {
                    self.available = nil
                    if manual { self.shownWindow().upToDate() }
                    return
                }
                log("update: Sweeflow \(mac.version) (\(mac.build)) is out, this is \(Self.version) (\(Self.build))")
                self.available = feed
                if manual { self.show(feed, installing: false) } else { self.notifyOnce(feed) }
            }
        }.resume()
    }

    /// A notification for a newer release, once per version.
    private func notifyOnce(_ feed: Feed) {
        guard UserDefaults.standard.string(forKey: "updateNotified") != feed.version else { return }
        UserDefaults.standard.set(feed.version, forKey: "updateNotified")
        notify("Sweeflow \(feed.version) is available", "Choose Update in Sweeflow's menu to install it.", id: "update")
    }

    private func notify(_ title: String, _ body: String, id: String) {
        Notices.shared.post(title, body, kind: id)
    }

    /// After an update, once: what it is now.
    private func sayIfUpdated() {
        let last = UserDefaults.standard.string(forKey: "lastRunVersion")
        UserDefaults.standard.set(Self.version, forKey: "lastRunVersion")
        if let last, last != Self.version, Self.newer((Self.version, 0), than: (last, 0)) {
            notify("Sweeflow is updated", "You now have Sweeflow \(Self.version).", id: "updated")
        }
    }

    /// The update window closed (the debug port's `update hide`).
    func hideWindow() { window?.close() }

    private func shownWindow() -> UpdateWindow {
        if let window { return window }
        let w = UpdateWindow(update: { [weak self] in self?.install() }, retry: { [weak self] in self?.check(manual: true) })
        window = w
        return w
    }

    private func show(_ feed: Feed, installing: Bool) {
        shownWindow().show(feed, installing: installing)
    }

    /// The window in one of its states with made-up contents (the debug port, for screenshots).
    func preview(_ state: String) {
        let feed = Feed(version: "0.2.0", notes: """
            Smoother windows, and Sweeflow tells you when a new version is out.

            - Mac windows open and move about faster, with less delay when typing.
            - Check for updates in the Mac menu and the Linux tray, with this window.
            - The Mac says in a notification when streaming starts, pauses and stops.
            - Shortcuts you keep for Linux now also work while a Mac window is in front.
            - Fixes for sheets on Hyprland and the Alt+Tab switcher on Linux Mint.
            """, page: nil, date: "2026-12-03", mac: nil)
        let w = shownWindow()
        switch state {
        case "checking": w.checking()
        case "latest": w.upToDate()
        case "failed": w.failed(checking: true, "Sweeflow couldn't reach github.com. Check you're online, then try again.")
        case "installing": w.show(feed, installing: false); w.status("Downloading…", progress: 0.42)
        default: w.show(feed, installing: false)
        }
    }

    private func showAndInstall(_ feed: Feed) {
        show(feed, installing: true)
        install()
    }

    private var installing = false

    /// Downloads, checks and swaps in the new copy, then opens it (this one quits).
    private func install() {
        guard !installing, let feed = available, let mac = feed.mac, let url = URL(string: mac.url) else { return }
        installing = true
        window?.status("Downloading…", progress: 0)
        let task = URLSession.shared.downloadTask(with: url) { [self] file, _, error in
            do {
                guard let file else { throw UpdateError("the download didn't come (\(error.map { "\($0)" } ?? "no file"))") }
                let dmg = FileManager.default.temporaryDirectory.appendingPathComponent("Sweeflow-update.dmg")
                try? FileManager.default.removeItem(at: dmg)
                try FileManager.default.moveItem(at: file, to: dmg)
                DispatchQueue.main.async { self.window?.status("Checking the download and its signature…", progress: nil) }
                let app = try Self.verifiedApp(dmg: dmg, sha256: mac.sha256)
                DispatchQueue.main.async {
                    self.window?.status("Restarting Sweeflow…", progress: nil)
                    log("update: installing Sweeflow \(mac.version) from \(app.path)")
                    self.swapAndRestart(with: app)
                }
            } catch {
                log("update failed: \(error)")
                DispatchQueue.main.async {
                    self.installing = false
                    self.window?.failed(checking: false, "The update didn't install (\(error)). Sweeflow stays as it is.")
                }
            }
        }
        progressWatch = task.progress.observe(\.fractionCompleted) { [weak self] p, _ in
            DispatchQueue.main.async { self?.window?.status(nil, progress: p.fractionCompleted) }
        }
        task.resume()
    }
    private var progressWatch: NSKeyValueObservation?

    struct UpdateError: Error, CustomStringConvertible { let description: String; init(_ s: String) { description = s } }

    /// The disk image checked (its checksum as the release lists it), and the app in it copied
    /// out, if it's signed as this one is (the same Sweeflow certificate): else nothing is changed.
    static func verifiedApp(dmg: URL, sha256: String) throws -> URL {
        let data = try Data(contentsOf: dmg, options: .mappedIfSafe)
        let sum = SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
        guard sum == sha256.lowercased() else { throw UpdateError("its checksum isn't the release's") }
        let mount = FileManager.default.temporaryDirectory.appendingPathComponent("sweeflow-update-mount")
        try? FileManager.default.createDirectory(at: mount, withIntermediateDirectories: true)
        _ = try run("/usr/bin/hdiutil", ["attach", "-nobrowse", "-noautoopen", "-noverify", "-mountpoint", mount.path, dmg.path])
        defer { _ = try? run("/usr/bin/hdiutil", ["detach", mount.path, "-quiet"]) }
        let inImage = mount.appendingPathComponent("Sweeflow.app")
        try checkSignature(inImage)
        let staged = FileManager.default.temporaryDirectory.appendingPathComponent("sweeflow-update/Sweeflow.app")
        try? FileManager.default.removeItem(at: staged.deletingLastPathComponent())
        try FileManager.default.createDirectory(at: staged.deletingLastPathComponent(), withIntermediateDirectories: true)
        _ = try run("/usr/bin/ditto", [inImage.path, staged.path])
        try checkSignature(staged)
        return staged
    }

    /// Signed, intact, and by the certificate this copy is signed with (its designated requirement).
    static func checkSignature(_ app: URL) throws {
        var me: SecCode?, meStatic: SecStaticCode?, mine: SecRequirement?, theirs: SecStaticCode?
        guard SecCodeCopySelf([], &me) == errSecSuccess, let me,
              SecCodeCopyStaticCode(me, [], &meStatic) == errSecSuccess, let meStatic,
              SecCodeCopyDesignatedRequirement(meStatic, [], &mine) == errSecSuccess, let mine,
              SecStaticCodeCreateWithPath(app as CFURL, [], &theirs) == errSecSuccess, let theirs else {
            throw UpdateError("its signature couldn't be read")
        }
        let status = SecStaticCodeCheckValidity(theirs, SecCSFlags(rawValue: kSecCSCheckAllArchitectures | kSecCSCheckNestedCode), mine)
        guard status == errSecSuccess else { throw UpdateError("it isn't signed as Sweeflow is (\(status))") }
    }

    /// The new copy put where this one is, by a small script that waits for this one to quit, then
    /// opens it. (Put back as it was if the copy fails.)
    private func swapAndRestart(with staged: URL) {
        let app = Bundle.main.bundleURL.path
        let q = { (s: String) in "'" + s.replacingOccurrences(of: "'", with: "'\\''") + "'" }
        let script = """
        while kill -0 \(getpid()) 2>/dev/null; do sleep 0.2; done
        rm -rf \(q(app + ".old"))
        if mv \(q(app)) \(q(app + ".old")) && /usr/bin/ditto \(q(staged.path)) \(q(app)); then rm -rf \(q(app + ".old")); \
        else rm -rf \(q(app)); mv \(q(app + ".old")) \(q(app)); fi
        rm -rf \(q(staged.deletingLastPathComponent().path))
        /usr/bin/open \(q(app))
        """
        let p = Process()
        p.executableURL = URL(fileURLWithPath: "/bin/sh")
        p.arguments = ["-c", script]
        do { try p.run() } catch {
            installing = false
            window?.failed(checking: false, "The update didn't install (\(error)). Sweeflow stays as it is.")
            return
        }
        StatusMenu.quitForUpdate()
    }
}

/// The update window: looking for a release, up to date, couldn't look, or a newer one with what's
/// new (and how its update is going). The same as Linux's (linux/src/updatewin.rs).
final class UpdateWindow: NSObject, NSWindowDelegate {
    private let window: NSWindow
    private let icon = NSImageView()
    private let heading = NSTextField(labelWithString: "")
    private let subheading = NSTextField(labelWithString: "")
    private let spinner = NSProgressIndicator()
    private let notesLabel = NSTextField(labelWithString: "What's new")
    private let notesBox = NSView()
    private let notes = NSTextView()
    private var notesHeight: NSLayoutConstraint!
    private let bar = NSProgressIndicator()
    private let line = NSTextField(labelWithString: "")
    private let pageButton = NSButton(title: "Release page", target: nil, action: nil)
    private let secondButton = NSButton(title: "Later", target: nil, action: nil)
    private let mainButton = NSButton(title: "Update and Restart", target: nil, action: nil)
    private let update: () -> Void
    private let retry: () -> Void
    private var page: String?
    private enum Main { case update, retry, ok }
    private var main = Main.update
    private var busy = false

    init(update: @escaping () -> Void, retry: @escaping () -> Void) {
        self.update = update
        self.retry = retry
        window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 520, height: 200), styleMask: [.titled, .closable], backing: .buffered, defer: false)
        super.init()
        window.title = "Software Update"
        window.isReleasedWhenClosed = false
        window.delegate = self

        icon.image = NSApp.applicationIconImage
        icon.imageScaling = .scaleProportionallyUpOrDown
        heading.font = .systemFont(ofSize: 17, weight: .semibold)
        heading.lineBreakMode = .byWordWrapping
        subheading.font = .systemFont(ofSize: 13)
        subheading.textColor = .secondaryLabelColor
        subheading.lineBreakMode = .byWordWrapping
        subheading.maximumNumberOfLines = 3
        subheading.preferredMaxLayoutWidth = 380
        spinner.style = .spinning
        spinner.controlSize = .small
        let titles = NSStackView(views: [heading, subheading])
        titles.orientation = .vertical
        titles.alignment = .leading
        titles.spacing = 3
        let header = NSStackView(views: [icon, titles, NSView(), spinner])
        header.spacing = 16
        header.alignment = .centerY

        notesLabel.font = .systemFont(ofSize: 12, weight: .semibold)
        notesLabel.textColor = .secondaryLabelColor
        // The notes on a rounded panel, scrolling when there are many.
        notesBox.wantsLayer = true
        notesBox.layer?.cornerRadius = 10
        notesBox.layer?.backgroundColor = NSColor.quaternaryLabelColor.withAlphaComponent(0.08).cgColor
        notesBox.layer?.borderColor = NSColor.separatorColor.cgColor
        notesBox.layer?.borderWidth = 0.5
        notes.isEditable = false
        notes.isSelectable = true
        notes.drawsBackground = false
        notes.textContainerInset = NSSize(width: 12, height: 14)
        notes.autoresizingMask = [.width]
        let scroll = NSScrollView()
        scroll.documentView = notes
        scroll.hasVerticalScroller = true
        scroll.autohidesScrollers = true
        scroll.drawsBackground = false
        scroll.translatesAutoresizingMaskIntoConstraints = false
        notesBox.addSubview(scroll)

        bar.style = .bar
        bar.minValue = 0
        bar.maxValue = 1
        line.font = .systemFont(ofSize: 12)
        line.textColor = .secondaryLabelColor

        pageButton.isBordered = false
        pageButton.contentTintColor = .labelColor
        pageButton.attributedTitle = NSAttributedString(string: "Release page", attributes: [.foregroundColor: NSColor.labelColor, .font: NSFont.systemFont(ofSize: 13), .underlineStyle: NSUnderlineStyle.single.rawValue])
        pageButton.target = self
        pageButton.action = #selector(openPage)
        secondButton.bezelStyle = .rounded
        secondButton.target = self
        secondButton.action = #selector(dismiss)
        secondButton.keyEquivalent = "\u{1b}"
        mainButton.bezelStyle = .rounded
        mainButton.keyEquivalent = "\r"
        mainButton.target = self
        mainButton.action = #selector(chosen)
        let buttons = NSStackView(views: [pageButton, NSView(), secondButton, mainButton])
        buttons.spacing = 8

        let stack = NSStackView(views: [header, notesLabel, notesBox, bar, line, buttons])
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 14
        stack.setCustomSpacing(8, after: notesLabel)
        stack.setCustomSpacing(8, after: bar)
        stack.setCustomSpacing(20, after: line)
        stack.edgeInsets = NSEdgeInsets(top: 22, left: 24, bottom: 22, right: 24)
        stack.translatesAutoresizingMaskIntoConstraints = false
        let content = NSView()
        window.contentView = content
        content.addSubview(stack)
        notesHeight = notesBox.heightAnchor.constraint(equalToConstant: 200)
        NSLayoutConstraint.activate([
            content.widthAnchor.constraint(equalToConstant: 520),
            stack.leadingAnchor.constraint(equalTo: content.leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: content.trailingAnchor),
            stack.topAnchor.constraint(equalTo: content.topAnchor),
            stack.bottomAnchor.constraint(equalTo: content.bottomAnchor),
            icon.widthAnchor.constraint(equalToConstant: 64),
            icon.heightAnchor.constraint(equalToConstant: 64),
            header.widthAnchor.constraint(equalTo: stack.widthAnchor, constant: -48),
            notesBox.widthAnchor.constraint(equalTo: header.widthAnchor),
            notesHeight,
            scroll.leadingAnchor.constraint(equalTo: notesBox.leadingAnchor),
            scroll.trailingAnchor.constraint(equalTo: notesBox.trailingAnchor),
            scroll.topAnchor.constraint(equalTo: notesBox.topAnchor),
            scroll.bottomAnchor.constraint(equalTo: notesBox.bottomAnchor),
            bar.widthAnchor.constraint(equalTo: header.widthAnchor),
            buttons.widthAnchor.constraint(equalTo: header.widthAnchor),
        ])
    }

    // MARK: the states

    func checking() {
        set(heading: "Checking for updates…", sub: "Looking at the latest release.", notes: false)
        spinner.isHidden = false
        spinner.startAnimation(nil)
        buttons(second: "Cancel", main: nil)
        present()
    }

    func upToDate() {
        set(heading: "Sweeflow is up to date", sub: "You have the latest version, \(Updater.version).", notes: false)
        buttons(second: nil, main: ("OK", .ok))
        present()
    }

    /// `checking`: it couldn't look (else: the update didn't install, its notes still shown).
    func failed(checking: Bool, _ text: String) {
        busy = false
        if checking {
            set(heading: "Couldn't check for updates", sub: text, notes: false)
            buttons(second: "Close", main: ("Try Again", .retry))
        } else {
            bar.isHidden = true
            line.isHidden = false
            line.stringValue = text
            line.textColor = .systemRed
            buttons(second: "Close", main: ("Try Again", .update))
        }
        present()
    }

    func show(_ feed: Updater.Feed, installing: Bool) {
        page = feed.page
        let released = feed.date.flatMap(Self.niceDate).map { "  ·  Released \($0)" } ?? ""
        set(heading: "Sweeflow \(feed.version) is available", sub: "You have \(Updater.version)\(released)", notes: true)
        setNotes(feed.notes ?? "")
        buttons(second: "Later", main: ("Update and Restart", .update))
        if installing { status("Downloading…", progress: nil) }
        present()
    }

    /// How the update is going: a line, and how far the download is (nil: it can't say).
    func status(_ text: String?, progress: Double?) {
        busy = true
        if let text { line.stringValue = text }
        if let v = heading.stringValue.split(separator: " ").dropFirst().first, heading.stringValue.hasSuffix("is available") {
            heading.stringValue = "Updating to Sweeflow \(v)…"
        }
        line.textColor = .secondaryLabelColor
        bar.isHidden = false
        line.isHidden = false
        if let progress { bar.isIndeterminate = false; bar.doubleValue = progress } else { bar.isIndeterminate = true; bar.startAnimation(nil) }
        secondButton.isHidden = true
        mainButton.isEnabled = false
    }

    func close() { window.orderOut(nil) }

    // MARK: pieces

    private func set(heading text: String, sub: String, notes shown: Bool) {
        heading.stringValue = text
        subheading.stringValue = sub
        spinner.stopAnimation(nil)
        spinner.isHidden = true
        notesLabel.isHidden = !shown
        notesBox.isHidden = !shown
        pageButton.isHidden = !shown
        bar.isHidden = true
        line.isHidden = true
        mainButton.isEnabled = true
    }

    private func buttons(second: String?, main: (String, Main)?) {
        secondButton.isHidden = second == nil
        secondButton.title = second ?? ""
        mainButton.isHidden = main == nil
        if let main { mainButton.title = main.0; self.main = main.1 }
        // Return presses the main button, or Cancel / Close while there's none.
        secondButton.keyEquivalent = main == nil ? "\r" : "\u{1b}"
    }

    /// CHANGELOG.md's markdown: "- " items as bullets in the text's colour, hanging; paragraphs
    /// as they are; **bold** and `code` inline.
    private func setNotes(_ md: String) {
        let body = NSMutableAttributedString()
        let font = NSFont.systemFont(ofSize: 13)
        var paragraphs: [(bullet: Bool, text: String)] = []
        for raw in md.components(separatedBy: "\n") {
            let t = raw.trimmingCharacters(in: .whitespaces)
            if t.isEmpty || t.hasPrefix("#") { paragraphs.append((false, "")); continue }
            if t.hasPrefix("- ") || t.hasPrefix("* ") {
                paragraphs.append((true, String(t.dropFirst(2))))
            } else if let last = paragraphs.last, !last.text.isEmpty {
                paragraphs[paragraphs.count - 1].text += " " + t
            } else {
                paragraphs.append((false, t))
            }
        }
        paragraphs = paragraphs.filter { !$0.text.isEmpty }
        if paragraphs.isEmpty { paragraphs = [(false, "No notes for this release.")] }
        for (i, p) in paragraphs.enumerated() {
            let style = NSMutableParagraphStyle()
            style.paragraphSpacing = i == paragraphs.count - 1 ? 0 : 8
            style.lineSpacing = 1.5
            if p.bullet {
                style.headIndent = 16
                style.tabStops = [NSTextTab(textAlignment: .left, location: 16)]
                body.append(NSAttributedString(string: "●\t", attributes: [.font: NSFont.systemFont(ofSize: 8), .foregroundColor: NSColor.labelColor, .paragraphStyle: style, .baselineOffset: 2]))
            }
            let inline = (try? NSAttributedString(markdown: p.text, options: .init(interpretedSyntax: .inlineOnlyPreservingWhitespace))) ?? NSAttributedString(string: p.text)
            let text = NSMutableAttributedString(attributedString: inline)
            text.addAttributes([.foregroundColor: NSColor.labelColor, .paragraphStyle: style], range: NSRange(location: 0, length: text.length))
            text.enumerateAttribute(.font, in: NSRange(location: 0, length: text.length)) { value, range, _ in
                let traits = (value as? NSFont)?.fontDescriptor.symbolicTraits ?? []
                text.addAttribute(.font, value: traits.contains(.bold) ? NSFont.systemFont(ofSize: 13, weight: .semibold) : font, range: range)
            }
            body.append(text)
            if i < paragraphs.count - 1 { body.append(NSAttributedString(string: "\n", attributes: [.paragraphStyle: style])) }
        }
        notes.textStorage?.setAttributedString(body)
        // As tall as the notes, up to a point (then they scroll).
        notes.frame.size.width = 472
        notes.layoutManager?.ensureLayout(for: notes.textContainer!)
        let used = notes.layoutManager?.usedRect(for: notes.textContainer!).height ?? 160
        notesHeight.constant = min(ceil(used) + 30, 300)
    }

    private func present() {
        window.contentView?.layoutSubtreeIfNeeded()
        let size = window.contentView?.fittingSize ?? NSSize(width: 520, height: 200)
        let wasVisible = window.isVisible
        window.setContentSize(size)
        if !wasVisible { window.center() }
        NSApp.activate(ignoringOtherApps: true)
        window.makeKeyAndOrderFront(nil)
    }

    static func niceDate(_ iso: String) -> String? {
        let f = DateFormatter()
        f.dateFormat = "yyyy-MM-dd"
        f.locale = Locale(identifier: "en_US_POSIX")
        guard let d = f.date(from: iso) else { return nil }
        let out = DateFormatter()
        out.dateFormat = "d MMM yyyy"
        return out.string(from: d)
    }

    @objc private func chosen() {
        switch main {
        case .update: update()
        case .retry: retry()
        case .ok: close()
        }
    }
    @objc private func dismiss() { close() }
    @objc private func openPage() {
        let url = URL(string: page ?? "") ?? URL(string: "https://github.com/herunan/sweeflow/releases/latest")!
        NSWorkspace.shared.open(url)
    }

    func windowShouldClose(_ sender: NSWindow) -> Bool { !busy }
}
