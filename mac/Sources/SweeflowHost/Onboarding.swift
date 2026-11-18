import AppKit
import ScreenCaptureKit
import Network
import ServiceManagement

/// Puts one of Sweeflow's own windows where a person at the Mac sees it: the primary display
/// (the one with the menu bar, where they look: a monitor on a laptop), else any screen but
/// Sweeflow's virtual display (nobody sees that, and Sweeflow's own windows aren't streamed).
/// Lights the built-in screen up while it's open, in case that's the one.
func presentToPerson(_ w: NSWindow) {
    BuiltInScreen.pause()
    let virtual = host.display.displayID
    let screen = NSScreen.screens.first {
        ($0.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? CGDirectDisplayID) != virtual
    } ?? NSScreen.main
    if let f = screen?.visibleFrame {
        w.setFrameOrigin(NSPoint(x: f.midX - w.frame.width / 2, y: f.midY - w.frame.height / 2))
    } else {
        w.center()
    }
    NSApp.activate()
    w.makeKeyAndOrderFront(nil)
    // Started by macOS in the background (after Quit & Reopen, or the copy opened by hand handing
    // over), Sweeflow may not be let to come forward: the window went behind System Settings.
    // In front of other apps' windows either way.
    w.orderFrontRegardless()
}

/// How Sweeflow starts with the Mac: macOS opens the app itself at login (SMAppService.mainApp,
/// listed under Login Items › Open at Login), unless a development install put its own
/// LaunchAgent in ~/Library/LaunchAgents. Not a LaunchAgent inside the bundle, as before: macOS
/// refuses to start one from an app without a Developer ID (a launch constraint, exit 78), so
/// nothing opened at login.
enum LoginItem {
    static let devAgentLabel = "dev.sweeflow.host"
    static var service: SMAppService { .mainApp }

    static var devInstall: Bool {
        FileManager.default.fileExists(atPath: FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/LaunchAgents/\(devAgentLabel).plist").path)
    }

    /// Started by the development LaunchAgent, rather than opened (by hand or at login).
    static var underLaunchd: Bool { ProcessInfo.processInfo.environment["XPC_SERVICE_NAME"] == devAgentLabel }

    /// Opening at login is on: macOS has Sweeflow among the apps it opens at login.
    static var enabled: Bool { devInstall || service.status == .enabled }
    static var needsApproval: Bool { !devInstall && service.status == .requiresApproval }

    /// Whether the person wants Sweeflow opened at login (on until they switch it off): kept,
    /// so a first start registers it but one after switching it off doesn't.
    private static let wantedKey = "openAtLogin"
    static var wanted: Bool { UserDefaults.standard.object(forKey: wantedKey) as? Bool ?? true }
    static func set(_ on: Bool) {
        UserDefaults.standard.set(on, forKey: wantedKey)
        if devInstall { return }
        do {
            if on, service.status != .enabled { try service.register() }
            if !on, service.status != .notRegistered { try service.unregister() }
        } catch { log("login item: \(error)") }
    }

    /// At start: Sweeflow registered to open at login, if wanted and not yet.
    static func check() {
        if devInstall { return }
        if wanted, service.status == .notRegistered {
            do { try service.register() } catch { log("login item: \(error)") }
        }
        log("login item: \(["not registered", "on", "needs approval in System Settings", "not found"][min(service.status.rawValue, 3)])")
    }
}

/// What macOS says about one thing Sweeflow needs.
enum Allowance { case allowed, notAsked, refused }

/// The first-run window, "Set up Sweeflow": the four things macOS has to allow, each with its
/// state and one button, then opening at login, then Continue to the Link window (never opened
/// by itself: the person may still want to change opening at login).
///
/// Screen recording and system audio are two rows. macOS shows them on one Settings page
/// ("Screen & System Audio Recording"), but Sweeflow's sound comes through a Core Audio tap
/// (~45 ms sooner than ScreenCaptureKit's sound), which needs the page's second list, "System
/// Audio Recording Only". Its question comes only from its own Allow button, so it's asked in
/// setup, not when Linux first connects.
final class Onboarding: NSObject, NSWindowDelegate {
    static let shared = Onboarding()
    private var window: NSWindow?
    private var timer: Timer?

    private let networkStep = SetupStep(symbol: "network", title: "Local Network",
                                        detail: "Lets your Linux machine find this Mac.")
    private let screenStep = SetupStep(symbol: "rectangle.dashed.badge.record", title: "Screen Recording",
                                       detail: "Shows your apps' windows on Linux.")
    private let audioStep = SetupStep(symbol: "speaker.wave.2", title: "System Audio Recording",
                                      detail: "Plays your apps' sound on Linux.")
    private let axStep = SetupStep(symbol: "accessibility", title: "Accessibility",
                                   detail: "Lets you type, click and move windows from Linux.")
    private let loginBox = NSButton(checkboxWithTitle: "Open Sweeflow when you log in", target: nil, action: nil)
    private let status = NSTextField(wrappingLabelWithString: "")
    private let done = NSButton(title: "Done", target: nil, action: nil)

    /// Looking for Sweeflow on the network: what makes macOS ask about Local Network, and what
    /// tells whether it's allowed (ready) or not (waiting, for the policy).
    private var browser: NWBrowser?
    private var network: Allowance = .notAsked
    /// The audio question is being asked (a tap is running for a moment).
    private var askingAudio = false
    /// macOS's answer can't be read (TCC's private call missing): a tap that started counts.
    private(set) var audioStarted = false

    static let width: CGFloat = 560

    /// Everything Sweeflow needs is allowed, and some Linux computer is linked.
    static var complete: Bool { permissionsReady && !Identity.pairedClients().isEmpty }

    /// Screen recording, system audio and accessibility are all on: linking may open.
    static var permissionsReady: Bool {
        let screen = CGPreflightScreenCaptureAccess() || shared.screenAllowed
        let audio = AudioTap.permission() ?? shared.audioStarted
        return screen && audio && AXIsProcessTrusted()
    }

    func show() {
        if window == nil { build() }
        watchNetwork()
        refresh()
        presentToPerson(window!)
        timer?.invalidate()
        timer = Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { [weak self] _ in self?.refresh() }
    }

    /// `SweeflowHost --render-setup FILE.png`: draws the window as it is now into a picture (to see
    /// it without being at the Mac), then quits.
    static func render(to path: String, allDone: Bool = false) -> Never {
        let o = Onboarding()
        o.previewDone = allDone
        o.build()
        o.refresh()
        guard let w = o.window, let view = w.contentView else { exit(1) }
        // On screen but invisible, so AppKit lays it out and draws it as it would for real.
        w.alphaValue = 0
        w.setFrameOrigin(NSPoint(x: -20000, y: -20000))
        w.orderFrontRegardless()
        RunLoop.main.run(until: Date().addingTimeInterval(1.5))
        o.refresh()
        RunLoop.main.run(until: Date().addingTimeInterval(0.5))
        if let rep = view.bitmapImageRepForCachingDisplay(in: view.bounds) {
            view.cacheDisplay(in: view.bounds, to: rep)
            try? rep.representation(using: .png, properties: [:])?.write(to: URL(fileURLWithPath: path))
        }
        exit(0)
    }

    private func build() {
        let icon = NSImageView(image: NSApp.applicationIconImage)
        icon.translatesAutoresizingMaskIntoConstraints = false
        icon.widthAnchor.constraint(equalToConstant: 64).isActive = true
        icon.heightAnchor.constraint(equalToConstant: 64).isActive = true
        let title = NSTextField(labelWithString: "Set up Sweeflow")
        title.font = .systemFont(ofSize: 22, weight: .semibold)
        let subtitle = NSTextField(labelWithString: "Use your Mac's apps as windows on Linux.")
        subtitle.font = .systemFont(ofSize: 13)
        subtitle.textColor = .secondaryLabelColor
        let words = NSStackView(views: [title, subtitle])
        words.orientation = .vertical
        words.alignment = .leading
        words.spacing = 3
        let header = NSStackView(views: [icon, words])
        header.spacing = 14
        header.alignment = .centerY

        let inner = Self.width - 2 * 28
        let group = SetupGroup(steps: [networkStep, screenStep, audioStep, axStep], width: inner)

        networkStep.onAct = { [weak self] in self?.askNetwork() }
        screenStep.onAct = { [weak self] in self?.askScreen() }
        audioStep.onAct = { [weak self] in self?.askAudioStep() }
        axStep.onAct = { Self.ask(settings: "Privacy_Accessibility") {
            if !AXIsProcessTrusted() { Self.resetPermission("Accessibility") }
            _ = AXIsProcessTrustedWithOptions([kAXTrustedCheckOptionPrompt.takeUnretainedValue(): true] as CFDictionary)
        } }

        loginBox.target = self
        loginBox.action = #selector(toggleLogin)

        status.font = .systemFont(ofSize: 12)
        status.textColor = .secondaryLabelColor
        status.preferredMaxLayoutWidth = inner - 110
        done.bezelStyle = .push
        done.keyEquivalent = "\r"
        done.target = self
        done.action = #selector(finish)
        let footer = NSStackView(views: [status, NSView(), done])
        footer.alignment = .centerY
        footer.spacing = 12
        footer.translatesAutoresizingMaskIntoConstraints = false
        footer.widthAnchor.constraint(equalToConstant: inner).isActive = true
        footer.views[1].setContentHuggingPriority(.defaultLow, for: .horizontal)
        status.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        done.setContentCompressionResistancePriority(.required, for: .horizontal)

        let line = NSBox()
        line.boxType = .separator
        line.translatesAutoresizingMaskIntoConstraints = false
        line.widthAnchor.constraint(equalToConstant: inner).isActive = true

        let boxes = NSStackView(views: [loginBox])
        boxes.orientation = .vertical
        boxes.alignment = .leading
        boxes.spacing = 8
        let stack = NSStackView(views: [header, group.view, boxes, line, footer])
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 18
        stack.setCustomSpacing(22, after: header)
        stack.setCustomSpacing(14, after: group.view)
        stack.setCustomSpacing(16, after: boxes)
        stack.setCustomSpacing(14, after: line)
        // The title bar is see-through: room for the window buttons above the header.
        stack.edgeInsets = NSEdgeInsets(top: 40, left: 28, bottom: 20, right: 28)

        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: Self.width, height: 400),
                         styleMask: [.titled, .closable, .fullSizeContentView], backing: .buffered, defer: false)
        w.title = "Set up Sweeflow"
        w.titleVisibility = .hidden
        w.titlebarAppearsTransparent = true
        w.isMovableByWindowBackground = true
        w.contentView = stack
        w.isReleasedWhenClosed = false
        w.delegate = self
        window = w
        fit()
    }

    /// The window as tall as what it shows (rows' second lines come and go).
    private func fit() {
        guard let w = window, let v = w.contentView else { return }
        v.layoutSubtreeIfNeeded()
        let h = v.fittingSize.height
        if abs(w.contentLayoutRect.height - h) > 0.5 || abs(w.frame.height - h) > 0.5 {
            var f = w.frame
            f.origin.y += f.height - h
            f.size = NSSize(width: Self.width, height: h)
            w.setFrame(f, display: true)
        }
    }

    /// Whether ScreenCaptureKit lets Sweeflow record now. CGPreflightScreenCaptureAccess keeps its
    /// answer from when the app started, so after turning it on the row stayed unticked. Asked
    /// every two seconds until it's allowed.
    private(set) var screenAllowed = CGPreflightScreenCaptureAccess()
    private var checkingScreen = false
    private var lastScreenCheck = Date.distantPast
    private func checkScreen() {
        guard !screenAllowed, !checkingScreen, Date().timeIntervalSince(lastScreenCheck) > 2 else { return }
        checkingScreen = true
        lastScreenCheck = Date()
        Task { @MainActor in
            let ok = (try? await SCShareableContent.excludingDesktopWindows(true, onScreenWindowsOnly: true)) != nil
            self.checkingScreen = false
            if ok != self.screenAllowed { self.screenAllowed = ok; self.refresh() }
        }
    }

    /// `--render-setup FILE done`: drawn as if everything were allowed (permissions read from a
    /// process started over SSH are that process's, not Sweeflow's).
    private var previewDone = false

    private var audio: Allowance {
        if previewDone { return .allowed }
        switch AudioTap.permission() {
        case true?: return .allowed
        case false?: return .refused
        case nil: return audioStarted ? .allowed : .notAsked
        }
    }

    private func refresh() {
        checkScreen()
        let screen = previewDone || screenAllowed || CGPreflightScreenCaptureAccess()
        let ax = previewDone || AXIsProcessTrusted(), audio = self.audio
        if previewDone { network = .allowed }

        switch network {
        case .allowed: networkStep.show(done: true)
        case .notAsked: networkStep.show(done: false, button: "Allow…", next: "Choose Allow when macOS asks.")
        case .refused: networkStep.show(done: false, button: "Open Settings", next: "Turn on Sweeflow in Local Network.")
        }

        if screen {
            warmUpCapture()
            screenStep.show(done: true)
        } else {
            screenStep.show(done: false, button: "Allow…", next: "macOS restarts Sweeflow once it's on. Choose Quit & Reopen.")
        }

        switch audio {
        case .allowed: audioStep.show(done: true)
        case .notAsked: audioStep.show(done: false, button: askingAudio ? nil : "Allow…", next: "Choose Allow when macOS asks.")
        case .refused: audioStep.show(done: false, button: "Open Settings", next: "Turn on Sweeflow under System Audio Recording Only.")
        }

        if ax { axStep.show(done: true) } else { axStep.show(done: false, button: "Allow…") }

        loginBox.state = LoginItem.enabled ? .on : .off

        let ready = screen && ax && audio == .allowed
        let linkedTo = Identity.pairedClients().map(\.name).first
        if let name = linkedTo {
            status.stringValue = ready ? "Linked with \(name). You're all set." : "Linked with \(name). Allow the rest above."
            done.title = "Done"
        } else if ready {
            status.stringValue = "All allowed. Continue to link your Linux machine."
            done.title = "Continue"
        } else {
            status.stringValue = "Allow everything above, then link your Linux machine."
            done.title = "Done"
        }
        fit()
    }

    private func askNetwork() {
        if network == .refused { return Self.openSettings("Privacy_LocalNetwork") }
        networkAsked = true
        Self.ask(settings: "Privacy_LocalNetwork") { [weak self] in self?.watchNetwork(restart: true) }
    }

    private func askScreen() {
        Self.ask(settings: "Privacy_ScreenCapture") {
            Self.resetPermission("ScreenCapture")
            _ = CGRequestScreenCaptureAccess()
        }
    }

    /// The same Settings page as the screen's: its second list, "System Audio Recording Only".
    private func askAudioStep() {
        if audio == .refused { return Self.openSettings("Privacy_ScreenCapture") }
        askAudio()
    }

    /// Starts a tap for a moment: macOS asks its "record system audio" question then.
    private func askAudio() {
        askingAudio = true
        AudioTap.ask { [weak self] ok in
            self?.askingAudio = false
            if ok { self?.audioStarted = true }
            log("setup: audio tap \(ok ? "started" : "not made"), permission \(AudioTap.permission().map { $0 ? "allowed" : "refused" } ?? "unknown")")
            self?.refresh()
        }
    }

    @objc private func toggleLogin() {
        if LoginItem.needsApproval {
            LoginItem.set(true)
            return SMAppService.openSystemSettingsLoginItems()
        }
        LoginItem.set(!LoginItem.enabled)
        refresh()
    }


    @objc private func finish() {
        window?.close()
        if Self.permissionsReady && Identity.pairedClients().isEmpty { PairingWindow.shared.show() }
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
                let stream = SCStream(filter: SCContentFilter(display: display, excludingWindows: []), configuration: cfg, delegate: nil)
                try await stream.startCapture()
                try? await Task.sleep(for: .milliseconds(500))
                try? await stream.stopCapture()
                log("setup: test capture ok")
            } catch {
                log("setup: test capture failed: \(error)")
            }
        }
    }

    private func watchNetwork(restart: Bool = false) {
        if restart { browser?.cancel(); browser = nil }
        guard browser == nil else { return }
        let b = NWBrowser(for: .bonjour(type: "_sweeflow._udp", domain: nil), using: .udp)
        b.stateUpdateHandler = { [weak self] state in
            DispatchQueue.main.async {
                guard let self else { return }
                switch state {
                case .ready: self.network = .allowed
                case .waiting(let e), .failed(let e):
                    // Refused by the policy (asked before and turned down, or switched off in
                    // Settings), or not answered yet: the same state until macOS hears back.
                    if case .dns(let code) = e, code == DNSServiceErrorType(kDNSServiceErr_PolicyDenied) {
                        self.network = self.networkAsked ? .refused : .notAsked
                    }
                default: break
                }
                self.refresh()
            }
        }
        b.start(queue: .main)
        browser = b
    }
    /// Allow… was pressed for Local Network: still refused after that means turned down.
    private var networkAsked = false

    /// A permission's question: macOS's own when it shows one, else its page in Settings (once
    /// asked, macOS doesn't ask again). Never both.
    private static func ask(settings page: String, _ question: @escaping () -> Void) {
        question()
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.0) {
            if !permissionPromptShowing() { openSettings(page) }
        }
    }

    private static func openSettings(_ page: String) {
        if let url = URL(string: "x-apple.systempreferences:com.apple.preference.security?\(page)") {
            NSWorkspace.shared.open(url)
        }
    }

    /// Whether macOS has a permission question up.
    private static func permissionPromptShowing() -> Bool {
        let owners: Set<String> = ["UserNotificationCenter", "universalAccessAuthWarn", "CoreServicesUIAgent", "SecurityAgent"]
        let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly], kCGNullWindowID) as? [[String: Any]] ?? []
        return list.contains { owners.contains($0[kCGWindowOwnerName as String] as? String ?? "") }
    }

    /// Clears this app's entry for one privacy permission (tccutil, no password needed for an
    /// app's own entry): Settings may show Sweeflow switched on for an older copy (another
    /// signature, after an update), which macOS keeps and doesn't apply to this one.
    static func resetPermission(_ service: String) {
        guard let bid = Bundle.main.bundleIdentifier else { return }
        let p = Process()
        p.executableURL = URL(fileURLWithPath: "/usr/bin/tccutil")
        p.arguments = ["reset", service, bid]
        p.standardOutput = FileHandle.nullDevice
        p.standardError = FileHandle.nullDevice
        do { try p.run(); p.waitUntilExit(); log("permission \(service): stale entry cleared") } catch {}
    }

    func windowWillClose(_ notification: Notification) {
        timer?.invalidate()
        timer = nil
        browser?.cancel()
        browser = nil
        BuiltInScreen.resume()
    }
}

/// The rows' rounded box, like a group in System Settings, with a line between rows. A view of
/// its own (an NSBox didn't take its height from the rows, and everything landed on top of each
/// other).
private final class SetupGroup {
    let view: GroupView

    init(steps: [SetupStep], width: CGFloat) {
        view = GroupView()
        view.translatesAutoresizingMaskIntoConstraints = false
        var views: [NSView] = []
        for (i, step) in steps.enumerated() {
            if i > 0 {
                let line = NSBox()
                line.boxType = .separator
                line.translatesAutoresizingMaskIntoConstraints = false
                let holder = NSView()
                holder.translatesAutoresizingMaskIntoConstraints = false
                holder.addSubview(line)
                NSLayoutConstraint.activate([
                    holder.widthAnchor.constraint(equalToConstant: width),
                    holder.heightAnchor.constraint(equalToConstant: 1),
                    line.leadingAnchor.constraint(equalTo: holder.leadingAnchor, constant: 56),
                    line.trailingAnchor.constraint(equalTo: holder.trailingAnchor),
                    line.centerYAnchor.constraint(equalTo: holder.centerYAnchor),
                ])
                views.append(holder)
            }
            step.layout(width: width)
            views.append(step.view)
        }
        let rows = NSStackView(views: views)
        rows.orientation = .vertical
        rows.alignment = .leading
        rows.spacing = 0
        rows.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(rows)
        NSLayoutConstraint.activate([
            view.widthAnchor.constraint(equalToConstant: width),
            rows.topAnchor.constraint(equalTo: view.topAnchor),
            rows.bottomAnchor.constraint(equalTo: view.bottomAnchor),
            rows.leadingAnchor.constraint(equalTo: view.leadingAnchor),
            rows.trailingAnchor.constraint(equalTo: view.trailingAnchor),
        ])
    }
}

/// Draws the group's rounded background and border in the window's light or dark look.
final class GroupView: NSView {
    override func draw(_ dirtyRect: NSRect) {
        let path = NSBezierPath(roundedRect: bounds.insetBy(dx: 0.5, dy: 0.5), xRadius: 10, yRadius: 10)
        let dark = effectiveAppearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
        (dark ? NSColor.white.withAlphaComponent(0.05) : NSColor.black.withAlphaComponent(0.03)).setFill()
        path.fill()
        NSColor.separatorColor.setStroke()
        path.lineWidth = 1
        path.stroke()
    }
}

/// One thing macOS has to allow: an icon, what it is, why, what happens next when there's
/// something to do, and a check or a button. The button sits in a column of its own, so it is
/// never pushed past the edge.
final class SetupStep {
    let view = NSView()
    var onAct: () -> Void = {}
    private let icon = NSImageView()
    private let titleLabel: NSTextField
    private let detailLabel: NSTextField
    private let nextLabel = NSTextField(wrappingLabelWithString: "")
    private let check = NSImageView()
    private let button = NSButton(title: "Allow…", target: nil, action: nil)
    private let spinner = NSProgressIndicator()
    static let trailing: CGFloat = 100

    init(symbol: String, title: String, detail: String) {
        titleLabel = NSTextField(labelWithString: title)
        detailLabel = NSTextField(wrappingLabelWithString: detail)
        let config = NSImage.SymbolConfiguration(pointSize: 18, weight: .regular)
        icon.image = NSImage(systemSymbolName: symbol, accessibilityDescription: title)?.withSymbolConfiguration(config)
        icon.contentTintColor = .sweeflowViolet
        check.image = NSImage(systemSymbolName: "checkmark.circle.fill", accessibilityDescription: "Allowed")?
            .withSymbolConfiguration(NSImage.SymbolConfiguration(pointSize: 20, weight: .regular))
        check.contentTintColor = .systemGreen
        spinner.style = .spinning
        spinner.controlSize = .small
        button.bezelStyle = .push
        button.target = self
        button.action = #selector(act)
    }

    func layout(width: CGFloat) {
        titleLabel.font = .systemFont(ofSize: 13, weight: .semibold)
        detailLabel.font = .systemFont(ofSize: 12)
        detailLabel.textColor = .secondaryLabelColor
        nextLabel.font = .systemFont(ofSize: 12)
        nextLabel.textColor = .labelColor
        let textWidth = width - 16 - 28 - 12 - 12 - Self.trailing - 16
        for l in [titleLabel, detailLabel, nextLabel] { l.preferredMaxLayoutWidth = textWidth }
        let text = NSStackView(views: [titleLabel, detailLabel, nextLabel])
        text.orientation = .vertical
        text.alignment = .leading
        text.spacing = 2
        text.translatesAutoresizingMaskIntoConstraints = false
        text.widthAnchor.constraint(equalToConstant: textWidth).isActive = true

        // The check or button, right-aligned in a column of its own.
        let trailing = NSStackView()
        for v in [spinner, check, button] { trailing.addView(v, in: .trailing) }
        trailing.alignment = .centerY
        trailing.spacing = 6
        trailing.translatesAutoresizingMaskIntoConstraints = false
        trailing.widthAnchor.constraint(equalToConstant: Self.trailing).isActive = true

        icon.translatesAutoresizingMaskIntoConstraints = false
        icon.widthAnchor.constraint(equalToConstant: 28).isActive = true

        let row = NSStackView(views: [icon, text, trailing])
        row.orientation = .horizontal
        row.alignment = .centerY
        row.spacing = 12
        row.translatesAutoresizingMaskIntoConstraints = false
        view.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(row)
        NSLayoutConstraint.activate([
            view.widthAnchor.constraint(equalToConstant: width),
            row.topAnchor.constraint(equalTo: view.topAnchor, constant: 14),
            row.bottomAnchor.constraint(equalTo: view.bottomAnchor, constant: -14),
            row.leadingAnchor.constraint(equalTo: view.leadingAnchor, constant: 16),
            row.trailingAnchor.constraint(equalTo: view.trailingAnchor, constant: -16),
        ])
    }

    /// Allowed (a check), or a button (nil: waiting on macOS, a spinner) and what happens next.
    func show(done: Bool, button title: String? = nil, next: String? = nil) {
        check.isHidden = !done
        button.isHidden = done || title == nil
        if let title { button.title = title }
        let waiting = !done && title == nil
        spinner.isHidden = !waiting
        if waiting { spinner.startAnimation(nil) } else { spinner.stopAnimation(nil) }
        nextLabel.stringValue = done ? "" : (next ?? "")
        nextLabel.isHidden = nextLabel.stringValue.isEmpty
    }

    @objc private func act() { onAct() }
}
