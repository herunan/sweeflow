import AppKit
import ScreenCaptureKit
import Network
import ServiceManagement

/// Puts one of Sweeflow's own windows where a person at the Mac sees it: the built-in screen,
/// else any screen but Sweeflow's virtual display (nobody sees that, and Sweeflow's own windows
/// aren't streamed). Lights the built-in screen up while it's open.
func presentToPerson(_ w: NSWindow) {
    BuiltInScreen.pause()
    let virtual = host.display.displayID
    let screen = BuiltInScreen.screen ?? NSScreen.screens.first {
        ($0.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? CGDirectDisplayID) != virtual
    } ?? NSScreen.main
    if let f = screen?.visibleFrame {
        w.setFrameOrigin(NSPoint(x: f.midX - w.frame.width / 2, y: f.midY - w.frame.height / 2))
    } else {
        w.center()
    }
    NSApp.activate()
    w.makeKeyAndOrderFront(nil)
}

/// How Sweeflow starts with the Mac: a LaunchAgent inside the app bundle (registered with
/// SMAppService), unless a development install put its own in ~/Library/LaunchAgents.
enum LoginItem {
    static let agentLabel = "dev.sweeflow.agent"
    static let devAgentLabel = "dev.sweeflow.host"
    static let service = SMAppService.agent(plistName: "\(agentLabel).plist")

    static var devInstall: Bool {
        FileManager.default.fileExists(atPath: FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/LaunchAgents/\(devAgentLabel).plist").path)
    }

    /// Started by launchd (at login, or kept alive), rather than opened by hand.
    static var underLaunchd: Bool {
        let name = ProcessInfo.processInfo.environment["XPC_SERVICE_NAME"] ?? ""
        return name == agentLabel || name == devAgentLabel
    }

    static var enabled: Bool { devInstall || service.status == .enabled }
    static var needsApproval: Bool { !devInstall && service.status == .requiresApproval }

    /// Stops starting with the Mac (a development install keeps its own LaunchAgent).
    static func disable() {
        if devInstall { return }
        do { try service.unregister() } catch { log("login item: \(error)") }
    }

    /// Another copy of Sweeflow (the one launchd runs) is up.
    static var running: Bool {
        let me = getpid()
        return NSRunningApplication.runningApplications(withBundleIdentifier: Bundle.main.bundleIdentifier ?? "")
            .contains { $0.processIdentifier != me && !$0.isTerminated }
    }

    /// Gets launchd running the login item when it isn't: after Quit (which takes it out of
    /// launchd), a force quit, or Sweeflow.app replaced with a newer one. True once it's up.
    static func start() -> Bool {
        let domain = "gui/\(getuid())"
        let label = devInstall ? devAgentLabel : agentLabel
        let kicked = (try? run("/bin/launchctl", ["kickstart", "\(domain)/\(label)"])) != nil
        if kicked && waitRunning(seconds: 2) { return true }
        // Not loaded any more, or launchd won't start it: after Sweeflow.app is replaced, the
        // registration still names the old copy's code and every start fails. Load it again
        // (registering starts it).
        if devInstall {
            let plist = FileManager.default.homeDirectoryForCurrentUser
                .appendingPathComponent("Library/LaunchAgents/\(devAgentLabel).plist").path
            _ = try? run("/bin/launchctl", ["bootstrap", domain, plist])
        } else {
            try? service.unregister()
            do { try service.register() } catch { log("login item: \(error)") }
        }
        return waitRunning(seconds: 5)
    }

    private static func waitRunning(seconds: Double) -> Bool {
        let until = Date().addingTimeInterval(seconds)
        repeat {
            if running { return true }
            RunLoop.current.run(until: Date().addingTimeInterval(0.25))
        } while Date() < until
        return running
    }

    /// Registers the login item (macOS starts it right away, too). True if it's running now.
    @discardableResult
    static func enable() -> Bool {
        if devInstall { return true }
        do { try service.register() } catch { log("login item: \(error)") }
        return service.status == .enabled
    }
}

/// The first-run window: permissions, starting at login, then pairing with Linux.
final class Onboarding: NSObject, NSWindowDelegate {
    static let shared = Onboarding()
    private var window: NSWindow?
    private var timer: Timer?
    private let screenRow = PermissionRow(
        title: "Screen & System Audio Recording",
        detail: "To show your apps' windows and play their sound on Linux. Once it's on, macOS restarts Sweeflow (choose Quit & Reopen).",
        settings: "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
    private let axRow = PermissionRow(
        title: "Accessibility",
        detail: "To type and click in your apps from Linux, and move their windows.",
        settings: "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
    private let networkRow = PermissionRow(
        title: "Local Network",
        detail: "So your Linux machine finds this Mac. When macOS asks, choose Allow.",
        settings: "x-apple.systempreferences:com.apple.preference.security?Privacy_LocalNetwork")
    private let loginRow = PermissionRow(
        title: "Open at login (optional)",
        detail: "So your apps are there whenever Linux connects.",
        settings: nil)
    private let pair = NSButton(title: "Link a Linux machine…", target: nil, action: nil)
    /// Looking for Sweeflow on the network: what asks macOS for (and tells about) Local Network.
    private var browser: NWBrowser?
    private var networkOK = false
    private let note = NSTextField(wrappingLabelWithString: "")

    /// Everything Sweeflow needs is in place, and some Linux machine is linked. (Opening at login
    /// is optional.)
    static var complete: Bool {
        CGPreflightScreenCaptureAccess() && AXIsProcessTrusted() && !Identity.pairedClients().isEmpty
    }

    func show() {
        if window == nil { build() }
        watchNetwork()
        refresh()
        presentToPerson(window!)
        timer?.invalidate()
        timer = Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { [weak self] _ in self?.refresh() }
    }

    private func build() {
        let icon = NSImageView(image: NSApp.applicationIconImage)
        icon.widthAnchor.constraint(equalToConstant: 56).isActive = true
        icon.heightAnchor.constraint(equalToConstant: 56).isActive = true
        let title = NSTextField(labelWithString: "Welcome to Sweeflow")
        title.font = .systemFont(ofSize: 20, weight: .semibold)
        let intro = NSTextField(wrappingLabelWithString:
            "Your Mac's apps, as windows on your Linux machine. A few things first, then you link them:")
        intro.preferredMaxLayoutWidth = 420
        // Not allowed though Settings may show Sweeflow switched on: that entry can be an older
        // copy's (another signature, after an update), which macOS keeps and doesn't apply to
        // this one. Cleared first (Sweeflow's own entry only), so macOS asks afresh and Settings
        // lists this copy.
        screenRow.onAction = {
            if !CGPreflightScreenCaptureAccess() { Self.resetPermission("ScreenCapture") }
            _ = CGRequestScreenCaptureAccess()
        }
        axRow.onAction = {
            if !AXIsProcessTrusted() { Self.resetPermission("Accessibility") }
            _ = AXIsProcessTrustedWithOptions([kAXTrustedCheckOptionPrompt.takeUnretainedValue(): true] as CFDictionary)
        }
        loginRow.onAction = { if !LoginItem.enable() && LoginItem.needsApproval { SMAppService.openSystemSettingsLoginItems() } }
        loginRow.button.title = "Turn On"
        pair.target = self
        pair.action = #selector(openPairing)
        pair.bezelStyle = .push
        pair.keyEquivalent = "\r"
        note.textColor = .secondaryLabelColor
        note.font = .systemFont(ofSize: 11)
        note.preferredMaxLayoutWidth = 420
        networkRow.onAction = { [weak self] in self?.watchNetwork(restart: true) }
        networkRow.button.title = "Ask Again"
        let rows = NSStackView(views: [screenRow.view, axRow.view, networkRow.view, loginRow.view])
        rows.orientation = .vertical
        rows.alignment = .leading
        rows.spacing = 12
        let stack = NSStackView(views: [icon, title, intro, rows, note, pair])
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 16
        stack.edgeInsets = NSEdgeInsets(top: 24, left: 28, bottom: 22, right: 28)
        stack.setCustomSpacing(22, after: rows)
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 480, height: 520), styleMask: [.titled, .closable],
                         backing: .buffered, defer: false)
        w.title = "Sweeflow"
        w.contentView = stack
        w.isReleasedWhenClosed = false
        w.delegate = self
        window = w
    }

    private func refresh() {
        let screen = CGPreflightScreenCaptureAccess(), ax = AXIsProcessTrusted(), login = LoginItem.enabled
        screenRow.set(granted: screen)
        axRow.set(granted: ax)
        networkRow.set(granted: networkOK)
        loginRow.set(granted: login)
        loginRow.button.title = LoginItem.needsApproval ? "Open Settings" : "Turn On"
        pair.isEnabled = screen && ax
        note.stringValue = screen && ax ? "All set. Now link this Mac with your Linux machine." : ""
        if screen { warmUpCapture() }
    }

    /// Recording allowed: capture for a moment now, so anything else macOS asks the first time an
    /// app records (like letting it skip the window picker) comes up here, not once Linux connects.
    private var warmedUp = false
    private func warmUpCapture() {
        guard !warmedUp else { return }
        warmedUp = true
        Task {
            do {
                let content = try await SCShareableContent.excludingDesktopWindows(true, onScreenWindowsOnly: true)
                guard let display = content.displays.first else { return }
                let cfg = SCStreamConfiguration()
                cfg.width = 64
                cfg.height = 64
                cfg.capturesAudio = true
                let stream = SCStream(filter: SCContentFilter(display: display, excludingWindows: []), configuration: cfg, delegate: nil)
                try await stream.startCapture()
                try? await Task.sleep(for: .milliseconds(500))
                try? await stream.stopCapture()
                log("setup: test capture ok")
            } catch {
                log("setup: test capture failed: \(error)")
            }
            // And the sound's own question ("record your system audio"), asked when a tap is
            // first made: here rather than when Linux first connects.
            DispatchQueue.global().async { log("setup: audio tap \(AudioTap.ask() ? "allowed" : "not made")") }
        }
    }

    /// Browsing for Sweeflow on the network makes macOS ask for Local Network (once), and says
    /// whether it's allowed: ready, or waiting because of the policy.
    private func watchNetwork(restart: Bool = false) {
        if restart { browser?.cancel(); browser = nil }
        guard browser == nil else { return }
        let b = NWBrowser(for: .bonjour(type: "_sweeflow._udp", domain: nil), using: .udp)
        b.stateUpdateHandler = { [weak self] state in
            DispatchQueue.main.async {
                switch state {
                case .ready: self?.networkOK = true
                case .waiting, .failed: self?.networkOK = false
                default: break
                }
                self?.refresh()
            }
        }
        b.start(queue: .main)
        browser = b
    }

    /// Clears this app's entry for one privacy permission (tccutil, no password needed for an
    /// app's own entry).
    static func resetPermission(_ service: String) {
        guard let bid = Bundle.main.bundleIdentifier else { return }
        let p = Process()
        p.executableURL = URL(fileURLWithPath: "/usr/bin/tccutil")
        p.arguments = ["reset", service, bid]
        p.standardOutput = FileHandle.nullDevice
        p.standardError = FileHandle.nullDevice
        do { try p.run(); p.waitUntilExit(); log("permission \(service): stale entry cleared") } catch {}
    }

    @objc private func openPairing() {
        window?.close()
        PairingWindow.shared.show()
    }

    func windowWillClose(_ notification: Notification) {
        timer?.invalidate()
        timer = nil
        browser?.cancel()
        browser = nil
        BuiltInScreen.resume()
    }
}

/// One line of the first-run window: what, why, and ✓ or a button to go and allow it.
private final class PermissionRow {
    let view: NSStackView
    let button: NSButton
    private let mark = NSTextField(labelWithString: "")
    private let settingsURL: URL?
    var onAction: () -> Void = {}

    private let title: String

    init(title: String, detail: String, settings: String?) {
        self.title = title
        settingsURL = settings.flatMap(URL.init(string:))
        let t = NSTextField(labelWithString: title)
        t.font = .systemFont(ofSize: 13, weight: .semibold)
        let d = NSTextField(wrappingLabelWithString: detail)
        d.font = .systemFont(ofSize: 11)
        d.textColor = .secondaryLabelColor
        d.preferredMaxLayoutWidth = 300
        let text = NSStackView(views: [t, d])
        text.orientation = .vertical
        text.alignment = .leading
        text.spacing = 2
        button = NSButton(title: settings == nil ? "Open Settings" : "Allow…", target: nil, action: nil)
        button.bezelStyle = .push
        mark.font = .systemFont(ofSize: 18)
        let spacer = NSView()
        spacer.setContentHuggingPriority(.defaultLow, for: .horizontal)
        view = NSStackView(views: [mark, text, spacer, button])
        view.orientation = .horizontal
        view.alignment = .centerY
        view.spacing = 10
        view.widthAnchor.constraint(equalToConstant: 424).isActive = true
        mark.widthAnchor.constraint(equalToConstant: 22).isActive = true
        button.target = self
        button.action = #selector(act)
    }

    func set(granted: Bool) {
        mark.stringValue = granted ? "✓" : "○"
        mark.textColor = granted ? .systemGreen : .tertiaryLabelColor
        button.isHidden = granted
    }

    /// macOS is asked every time (its own question, which has its own way to Settings): that
    /// also puts Sweeflow back in the list in Settings after it was removed there (once asked,
    /// Sweeflow used to open Settings only, and the entry never came back). When macOS shows no
    /// question (it was turned down before), the settings page opens instead. Never both.
    @objc private func act() {
        guard let url = settingsURL else { return onAction() }
        onAction()
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.0) {
            if !Self.permissionPromptShowing() { NSWorkspace.shared.open(url) }
        }
    }

    /// Whether macOS has a permission question up (screen recording, accessibility).
    private static func permissionPromptShowing() -> Bool {
        let owners: Set<String> = ["UserNotificationCenter", "universalAccessAuthWarn", "CoreServicesUIAgent", "SecurityAgent"]
        let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly], kCGNullWindowID) as? [[String: Any]] ?? []
        return list.contains { owners.contains($0[kCGWindowOwnerName as String] as? String ?? "") }
    }
}
