import AppKit
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
    private let loginRow = PermissionRow(
        title: "Open at Login",
        detail: "So your apps are there whenever Linux connects.",
        settings: nil)
    private let pair = NSButton(title: "Pair with Linux…", target: nil, action: nil)
    private let note = NSTextField(wrappingLabelWithString: "")

    /// Everything Sweeflow needs is in place, and some computer is paired.
    static var complete: Bool {
        CGPreflightScreenCaptureAccess() && AXIsProcessTrusted() && LoginItem.enabled && !Identity.pairedClients().isEmpty
    }

    func show() {
        if window == nil { build() }
        refresh()
        presentToPerson(window!)
        timer?.invalidate()
        timer = Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { [weak self] _ in self?.refresh() }
    }

    private func build() {
        let title = NSTextField(labelWithString: "Welcome to Sweeflow")
        title.font = .systemFont(ofSize: 20, weight: .semibold)
        let intro = NSTextField(wrappingLabelWithString:
            "Sweeflow shows this Mac's apps as windows on your Linux computer. It needs a few things first:")
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
        let rows = NSStackView(views: [screenRow.view, axRow.view, loginRow.view])
        rows.orientation = .vertical
        rows.alignment = .leading
        rows.spacing = 12
        let stack = NSStackView(views: [title, intro, rows, note, pair])
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 16
        stack.edgeInsets = NSEdgeInsets(top: 24, left: 28, bottom: 22, right: 28)
        stack.setCustomSpacing(22, after: rows)
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 480, height: 400), styleMask: [.titled, .closable],
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
        loginRow.set(granted: login)
        loginRow.button.title = LoginItem.needsApproval ? "Open Settings" : "Turn On"
        pair.isEnabled = screen && ax
        note.stringValue = !screen
            ? "After allowing screen recording, macOS may ask to quit and reopen Sweeflow: choose Quit & Reopen."
            : (screen && ax ? "All set. Now pair this Mac with your Linux computer." : "")
    }

    @objc private func openPairing() {
        window?.close()
        PairingWindow.shared.show()
    }

    func windowWillClose(_ notification: Notification) {
        timer?.invalidate()
        timer = nil
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
