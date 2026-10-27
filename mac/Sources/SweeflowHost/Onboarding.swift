import AppKit
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
        detail: "To show your apps' windows and play their sound on Linux.",
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
        screenRow.onAction = { CGRequestScreenCaptureAccess() }
        axRow.onAction = {
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
        note.stringValue = !screen
            ? "After allowing screen recording, macOS may ask to quit and reopen Sweeflow: choose Quit & Reopen."
            : (screen && ax ? "All set. Now link this Mac with your Linux machine." : "")
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

    init(title: String, detail: String, settings: String?) {
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
        button = NSButton(title: "Open Settings", target: nil, action: nil)
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

    @objc private func act() {
        onAction()
        // The request may show nothing (asked before): the settings page always works.
        if let url = settingsURL { NSWorkspace.shared.open(url) }
    }
}
