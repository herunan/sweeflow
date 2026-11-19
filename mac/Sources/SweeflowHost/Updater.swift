import AppKit
import CryptoKit
import UserNotifications

/// Updates. Each release has `latest.json` (linux/release/build.sh): its version, what's new, and
/// where the Mac app is. Sweeflow looks a little after it starts and every few hours. A newer one
/// is said once in a notification, and the menu's line turns from "Check for Updates…" into
/// "Update to Sweeflow X…". Chosen, it shows what's new while it downloads the disk image, checks
/// it (its checksum, and that the app in it is signed as this one is), swaps it in and opens it.
final class Updater: NSObject, UNUserNotificationCenterDelegate {
    static let shared = Updater()

    struct Feed: Decodable {
        struct Part: Decodable { let version: String; let build: Int; let url: String; let sha256: String }
        let version: String
        let notes: String?
        let page: String?
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
        UNUserNotificationCenter.current().delegate = self
        sayIfUpdated()
        DispatchQueue.main.asyncAfter(deadline: .now() + 30) { [weak self] in self?.check(manual: false) }
        timer = Timer.scheduledTimer(withTimeInterval: Self.every, repeats: true) { [weak self] _ in self?.check(manual: false) }
    }

    /// The menu's line: what it says, and what it does.
    var menuTitle: String { available.map { "Update to Sweeflow \($0.version)…" } ?? "Check for Updates…" }

    func menuChosen() {
        if let feed = available { return showAndInstall(feed) }
        check(manual: true)
    }

    /// Looks at the latest release. `manual`: chosen from the menu, so the answer is shown either
    /// way (a window with what's new, or that this is the latest).
    func check(manual: Bool) {
        let url = UserDefaults.standard.string(forKey: "updateFeed").flatMap(URL.init(string:)) ?? URL(string: Self.feedURL)!
        URLSession.shared.dataTask(with: URLRequest(url: url, cachePolicy: .reloadIgnoringLocalCacheData, timeoutInterval: 30)) { data, response, error in
            let feed = data.flatMap { try? JSONDecoder().decode(Feed.self, from: $0) }
            DispatchQueue.main.async { [self] in
                guard let feed, let mac = feed.mac, (response as? HTTPURLResponse)?.statusCode ?? 200 < 400 else {
                    log("update check: \(error.map { "\($0)" } ?? "no release found")")
                    if manual { self.say("Couldn't check for updates", "The latest release couldn't be reached. Try again later.") }
                    return
                }
                guard Self.newer((mac.version, mac.build), than: (Self.version, Self.build)) else {
                    self.available = nil
                    if manual { self.say("Sweeflow is up to date", "You have the latest version, \(Self.version).") }
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
        let center = UNUserNotificationCenter.current()
        center.requestAuthorization(options: [.alert]) { granted, _ in
            guard granted else { return }
            let c = UNMutableNotificationContent()
            c.title = title
            c.body = body
            center.add(UNNotificationRequest(identifier: id, content: c, trigger: nil))
        }
    }

    /// The update notification clicked: what's new, and the update under way.
    func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse,
                                withCompletionHandler done: @escaping () -> Void) {
        DispatchQueue.main.async { [self] in
            if response.notification.request.identifier == "update", let feed = available { showAndInstall(feed) }
        }
        done()
    }

    func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification,
                                withCompletionHandler done: @escaping (UNNotificationPresentationOptions) -> Void) {
        done([.banner])
    }

    /// After an update, once: what it is now.
    private func sayIfUpdated() {
        let last = UserDefaults.standard.string(forKey: "lastRunVersion")
        UserDefaults.standard.set(Self.version, forKey: "lastRunVersion")
        if let last, last != Self.version, Self.newer((Self.version, 0), than: (last, 0)) {
            notify("Sweeflow is updated", "You now have Sweeflow \(Self.version).", id: "updated")
        }
    }

    private func say(_ title: String, _ text: String) {
        let a = NSAlert()
        a.messageText = title
        a.informativeText = text
        NSApp.activate(ignoringOtherApps: true)
        a.runModal()
    }

    private func show(_ feed: Feed, installing: Bool) {
        if window == nil { window = UpdateWindow { [weak self] in self?.install() } }
        window?.show(feed, installing: installing)
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
        window?.status("Downloading Sweeflow \(mac.version)…", progress: 0)
        let task = URLSession.shared.downloadTask(with: url) { [self] file, _, error in
            do {
                guard let file else { throw UpdateError("the download didn't come (\(error.map { "\($0)" } ?? "no file"))") }
                let dmg = FileManager.default.temporaryDirectory.appendingPathComponent("Sweeflow-update.dmg")
                try? FileManager.default.removeItem(at: dmg)
                try FileManager.default.moveItem(at: file, to: dmg)
                DispatchQueue.main.async { self.window?.status("Checking it…", progress: nil) }
                let app = try Self.verifiedApp(dmg: dmg, sha256: mac.sha256)
                DispatchQueue.main.async {
                    self.window?.status("Restarting…", progress: nil)
                    log("update: installing Sweeflow \(mac.version) from \(app.path)")
                    self.swapAndRestart(with: app)
                }
            } catch {
                log("update failed: \(error)")
                DispatchQueue.main.async {
                    self.installing = false
                    self.window?.failed("The update didn't install: \(error). Sweeflow stays as it is.")
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
            window?.failed("The update didn't install: \(error). Sweeflow stays as it is.")
            return
        }
        StatusMenu.quitForUpdate()
    }
}

/// What's new in a release, and how its update is going.
final class UpdateWindow: NSObject {
    private let window: NSWindow
    private let heading = NSTextField(labelWithString: "")
    private let notes = NSTextView()
    private let line = NSTextField(labelWithString: "")
    private let bar = NSProgressIndicator()
    private let button = NSButton(title: "Update and Restart", target: nil, action: nil)
    private let update: () -> Void

    init(update: @escaping () -> Void) {
        self.update = update
        window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 460, height: 420), styleMask: [.titled, .closable], backing: .buffered, defer: false)
        super.init()
        window.title = "Sweeflow Update"
        window.isReleasedWhenClosed = false
        heading.font = .boldSystemFont(ofSize: 15)
        notes.isEditable = false
        notes.drawsBackground = false
        notes.textContainerInset = NSSize(width: 0, height: 4)
        let scroll = NSScrollView()
        scroll.documentView = notes
        scroll.hasVerticalScroller = true
        scroll.drawsBackground = false
        notes.autoresizingMask = [.width]
        bar.style = .bar
        bar.minValue = 0
        bar.maxValue = 1
        line.textColor = .secondaryLabelColor
        button.target = self
        button.action = #selector(chosen)
        button.keyEquivalent = "\r"
        let stack = NSStackView(views: [heading, scroll, bar, line, button])
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 10
        stack.edgeInsets = NSEdgeInsets(top: 20, left: 20, bottom: 20, right: 20)
        stack.translatesAutoresizingMaskIntoConstraints = false
        window.contentView = NSView()
        window.contentView!.addSubview(stack)
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: window.contentView!.leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: window.contentView!.trailingAnchor),
            stack.topAnchor.constraint(equalTo: window.contentView!.topAnchor),
            stack.bottomAnchor.constraint(equalTo: window.contentView!.bottomAnchor),
            scroll.widthAnchor.constraint(equalTo: stack.widthAnchor, constant: -40),
            scroll.heightAnchor.constraint(greaterThanOrEqualToConstant: 220),
            bar.widthAnchor.constraint(equalTo: scroll.widthAnchor),
        ])
        button.setContentHuggingPriority(.required, for: .horizontal)
    }

    @objc private func chosen() { update() }

    func show(_ feed: Updater.Feed, installing: Bool) {
        heading.stringValue = "Sweeflow \(feed.version)"
        let text = feed.notes?.isEmpty == false ? feed.notes! : "No notes for this release."
        let md = (try? NSAttributedString(markdown: text, options: .init(interpretedSyntax: .inlineOnlyPreservingWhitespace)))
            ?? NSAttributedString(string: text)
        let body = NSMutableAttributedString(attributedString: md)
        body.addAttributes([.foregroundColor: NSColor.labelColor, .font: NSFont.systemFont(ofSize: 13)], range: NSRange(location: 0, length: body.length))
        notes.textStorage?.setAttributedString(body)
        bar.isHidden = !installing
        button.isHidden = installing
        line.stringValue = installing ? "Getting it ready…" : "You have \(Updater.version). Sweeflow restarts to update."
        NSApp.activate(ignoringOtherApps: true)
        window.center()
        window.makeKeyAndOrderFront(nil)
    }

    /// How the update is going: a line, and how far the download is (nil: it can't say).
    func status(_ text: String?, progress: Double?) {
        if let text { line.stringValue = text }
        bar.isHidden = false
        button.isHidden = true
        if let progress { bar.isIndeterminate = false; bar.doubleValue = progress } else { bar.isIndeterminate = true; bar.startAnimation(nil) }
    }

    func failed(_ text: String) {
        line.stringValue = text
        bar.isHidden = true
        button.isHidden = false
        button.title = "Try Again"
    }
}
