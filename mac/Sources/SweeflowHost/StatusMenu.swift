import AppKit
import ServiceManagement

/// The menu-bar item: what concerns this Mac (the rest is set on Linux, see Settings).
final class StatusMenu: NSObject, NSMenuDelegate {
    private let item = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
    private let menu = NSMenu()

    override init() {
        super.init()
        menu.delegate = self
        menu.autoenablesItems = false
        item.menu = menu
        updateIcon()
        for name in [Notification.Name.sweeflowSettingsChanged, .sweeflowConnectionChanged] {
            NotificationCenter.default.addObserver(forName: name, object: nil, queue: .main) { [weak self] _ in
                self?.updateIcon()
            }
        }
    }

    /// Where this Mac stands, as a dot in the logo's empty bottom-right corner (the sweet runs
    /// from bottom left to top right), in the colours Linux gives it (its trays and Sweeflow's
    /// window): violet streaming, a violet ring paused, green waiting for a Linux computer.
    private let dot = CALayer()

    /// The logo (a template: macOS draws it in the menu bar's colour), always solid, with the dot.
    private func updateIcon() {
        let connected = host.connected
        let on = settings.config.enabled && connected
        let base = NSImage(named: "MenuIcon")
            ?? NSImage(systemSymbolName: "macwindow.on.rectangle", accessibilityDescription: "Sweeflow")
        guard let base, let button = item.button else { return }
        // The logo is wider than tall (a wrapped sweet): the menu bar's usual height, its own width.
        if base.size.height > 0 {
            base.size = NSSize(width: (16 * base.size.width / base.size.height).rounded(), height: 16)
        }
        base.isTemplate = true
        button.image = base
        button.toolTip = !connected ? "Sweeflow: not streaming" : on ? "Sweeflow: streaming" : "Sweeflow: paused"
        let violet = NSColor(srgbRed: 0x8b / 255.0, green: 0x5c / 255.0, blue: 0xf6 / 255.0, alpha: 1).cgColor
        let green = NSColor(srgbRed: 0x30 / 255.0, green: 0xa4 / 255.0, blue: 0x6c / 255.0, alpha: 1).cgColor
        let ring = connected && !on
        dot.backgroundColor = ring ? nil : (connected ? violet : green)
        dot.borderColor = violet
        dot.borderWidth = ring ? 1.4 : 0
        button.wantsLayer = true
        if dot.superlayer == nil { button.layer?.addSublayer(dot) }
        DispatchQueue.main.async { [weak self] in self?.placeDot() }
    }

    /// At the image's bottom-right corner, wherever the button has put it.
    private func placeDot() {
        guard let button = item.button, let image = button.image else { return }
        button.layoutSubtreeIfNeeded()
        let b = button.bounds, side: CGFloat = 5.5
        let img = CGRect(x: (b.width - image.size.width) / 2, y: (b.height - image.size.height) / 2,
                         width: image.size.width, height: image.size.height)
        // The image's bottom edge, in the button's (and so its layer's) coordinates.
        let y = button.isFlipped ? img.maxY - side : img.minY
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        dot.frame = CGRect(x: img.maxX - side, y: y, width: side, height: side)
        dot.cornerRadius = side / 2
        CATransaction.commit()
    }

    /// Built when opened, so it always shows the current state. Only what concerns this Mac:
    /// the connection, the apps, this screen, starting at login, and the Linux machines it's
    /// linked to. (How Mac apps show up on Linux is set on Linux.)
    func menuNeedsUpdate(_ menu: NSMenu) {
        menu.removeAllItems()
        let c = settings.config
        // Connected even before its name is known (a handshake that didn't say who).
        let client: String? = host.connected ? (host.client ?? "") : nil
        let refused = Identity.disconnectedClients().intersection(Identity.pairedClients().map(\.fingerprint))
        let statusText: String
        if client != nil {
            let who = host.clientName
            statusText = c.enabled ? "Streaming to \(who)" : "Paused · \(who)"
        } else {
            statusText = refused.isEmpty ? "Not streaming" : "Stopped"
        }
        // Setup not finished: first in the menu, where it's seen.
        if !Onboarding.complete {
            let setup = ClosureMenuItem(title: "Finish setting up…") { Onboarding.shared.show() }
            setup.attributedTitle = NSAttributedString(string: "Finish setting up…",
                                                       attributes: [.font: NSFont.boldSystemFont(ofSize: NSFont.menuFont(ofSize: 0).pointSize)])
            setup.image = NSImage(systemSymbolName: "exclamationmark.circle.fill", accessibilityDescription: nil)?
                .withSymbolConfiguration(.init(paletteColors: [.white, .sweeflowViolet]))
            menu.addItem(setup)
            menu.addItem(.separator())
        }
        let status = NSMenuItem(title: statusText, action: nil, keyEquivalent: "")
        status.isEnabled = false
        menu.addItem(status)
        // The Linux machine on another version of Sweeflow (it says which when it connects).
        if client != nil, !host.clientAppVersion.isEmpty, host.clientAppVersion != Updater.version {
            let other = host.clientAppVersion
            let behind = Updater.newer((Updater.version, 0), than: (other, 0))
            let note = NSMenuItem(title: behind ? "Linux has Sweeflow \(other): update it there" : "Linux has Sweeflow \(other), newer than this Mac's", action: nil, keyEquivalent: "")
            note.isEnabled = false
            menu.addItem(note)
        }
        if let fp = client {
            menu.addItem(ClosureMenuItem(title: c.enabled ? "Pause" : "Resume") { settings.update { $0.enabled.toggle() } })
            menu.addItem(ClosureMenuItem(title: "Stop") { Self.disconnect(fp) })
        } else if let fp = refused.first ?? Identity.lastServed.flatMap({ fp in
            Identity.pairedClients().contains { $0.fingerprint == fp } ? fp : nil
        }) {
            // Stopped here or there: Start here starts it there too.
            menu.addItem(ClosureMenuItem(title: "Start") { Self.connect(fp) })
        }
        menu.addItem(.separator())

        let found = AppList.found()
        // How many of them stream, out of all: "Allowed apps (85/87)", kept up as they're ticked.
        let count = { () -> String in
            let off = Set(settings.config.excludeApps)
            return "Allowed apps (\(found.filter { !off.contains($0.bundleId) }.count)/\(found.count))"
        }
        let apps = NSMenuItem(title: count(), action: nil, keyEquivalent: "")
        let sub = NSMenu()
        for a in found {
            // Ticked: its windows stream. A click ticks or unticks it and leaves the menu open, so
            // several can be changed in one go.
            let icon = NSWorkspace.shared.icon(forFile: URL(fileURLWithPath: a.path).resolvingSymlinksInPath().path)
            icon.size = NSSize(width: 16, height: 16)
            let i = NSMenuItem()
            i.view = StayOpenToggle(title: a.name, icon: icon, on: !settings.config.excludeApps.contains(a.bundleId)) { on in
                settings.update { c in
                    c.excludeApps.removeAll { $0 == a.bundleId }
                    if !on { c.excludeApps.append(a.bundleId) }
                }
                apps.title = count()
            }
            sub.addItem(i)
        }
        apps.submenu = sub
        menu.addItem(apps)
        menu.addItem(.separator())

        let dim = ClosureMenuItem(title: c.dimMacScreenWhileStreaming ? "Display dimmed" : "Dim display") {
            settings.update { $0.dimMacScreenWhileStreaming = true }
        }
        dim.isEnabled = c.enabled && !c.dimMacScreenWhileStreaming
        // Only while a Linux machine is connected (there's nothing to dim for otherwise).
        if client != nil { menu.addItem(dim) }
        let login = ClosureMenuItem(title: "Open at login") {
            if LoginItem.needsApproval { LoginItem.set(true); SMAppService.openSystemSettingsLoginItems() } else { LoginItem.set(!LoginItem.enabled) }
        }
        login.state = LoginItem.enabled ? .on : .off
        login.isEnabled = !LoginItem.devInstall
        menu.addItem(login)
        // A MacBook sleeps when its lid closes, whatever an app asks, unless pmset is told
        // otherwise (as root: see LidSleepRule).
        let lid = ClosureMenuItem(title: "Stream with lid closed") { LidSleepRule.ask(!LidSleepRule.shown) }
        lid.state = LidSleepRule.shown ? .on : .off
        lid.toolTip = "Keep the MacBook available while its lid is closed. Asks for your admin password to turn it on or off."
        if LidSleepRule.hasLid { menu.addItem(lid) }
        menu.addItem(.separator())

        let linked = NSMenuItem(title: "Linux machines", action: nil, keyEquivalent: "")
        let list = NSMenu()
        // Linking opens only once every permission is on (PairingWindow, Pairing.setOpen).
        let paired = Identity.pairedClients()
        if Onboarding.permissionsReady {
            list.addItem(ClosureMenuItem(title: "Link a Linux machine…") { PairingWindow.shared.show() })
            if !paired.isEmpty { list.addItem(.separator()) }
        }
        for p in paired {
            let name = Identity.clientName(p.fingerprint)
            let computer = NSMenuItem(title: name, action: nil, keyEquivalent: "")
            let options = NSMenu()
            if p.fingerprint == client {
                computer.state = .on
                options.addItem(ClosureMenuItem(title: "Stop") { Self.disconnect(p.fingerprint) })
            } else if refused.contains(p.fingerprint) {
                options.addItem(ClosureMenuItem(title: "Start") { Self.connect(p.fingerprint) })
            } else {
                let idle = NSMenuItem(title: "Not streaming", action: nil, keyEquivalent: "")
                idle.isEnabled = false
                options.addItem(idle)
            }
            options.addItem(ClosureMenuItem(title: "Rename…") { Self.rename(p.fingerprint) })
            options.addItem(ClosureMenuItem(title: "Unlink…") { Self.confirmForget(name, p.fingerprint) })
            computer.submenu = options
            list.addItem(computer)
        }
        linked.submenu = list
        menu.addItem(linked)

        menu.addItem(.separator())
        // Updates: "Check for updates", or "Update available" once one is out (Updater).
        let update = ClosureMenuItem(title: Updater.shared.menuTitle) { Updater.shared.menuChosen() }
        // One that's out stands out: the badge macOS gives a menu line with an update behind it.
        if Updater.shared.updateIsOut { update.badge = NSMenuItemBadge.updates(count: 1) }
        menu.addItem(update)
        let quit = ClosureMenuItem(title: "Quit Sweeflow") { Self.quit() }
        quit.keyEquivalent = "q"
        menu.addItem(quit)
    }

    /// The built-in screen as it was, and the virtual display gone: before this copy stops.
    static func leaveMacAsItWas() {
        BuiltInScreen.stopDimming()
        BuiltInScreen.restore()
        host.display.remove()
    }

    /// Leaves the Mac as it was (screen, virtual display), then stops. Started by the development
    /// LaunchAgent (kept running), Sweeflow is taken out of launchd first, or it would start again.
    private static func quit() {
        log("quitting (menu)")
        leaveMacAsItWas()
        // The development LaunchAgent keeps itself alive: it's taken out of launchd.
        if LoginItem.underLaunchd, LoginItem.devInstall {
            // launchd ends this process as it unloads it.
            _ = try? run("/bin/launchctl", ["bootout", "gui/\(getuid())/\(LoginItem.devAgentLabel)"])
        }
        NSApp.terminate(nil)
    }

    /// Quits for an update (Updater): left as it was, then the new copy opens.
    static func quitForUpdate() {
        log("quitting for an update")
        leaveMacAsItWas()
        NSApp.terminate(nil)
    }

    /// Stops streaming to that computer: it stops there too (as its own Stop would), and starts
    /// again with Start on either side. (An older one is refused until Start here.)
    static func disconnect(_ fingerprint: String) {
        Identity.setDisconnected(fingerprint, true)
        Identity.setStartWanted(fingerprint, false)
        server.drop(fingerprint, telling: .macDisconnected)
        server.announce()
    }

    /// Starts streaming to it: said on the network, which a stopped computer looks out for; it
    /// connects within a few seconds.
    static func connect(_ fingerprint: String) {
        Identity.setDisconnected(fingerprint, false)
        Identity.setStartWanted(fingerprint, true)
        server.announce()
    }

    private static func rename(_ fingerprint: String) {
        let alert = NSAlert()
        alert.messageText = "Rename \(Identity.clientName(fingerprint))"
        alert.informativeText = "What this Mac calls it."
        let field = NSTextField(frame: NSRect(x: 0, y: 0, width: 240, height: 24))
        field.stringValue = Identity.clientName(fingerprint)
        alert.accessoryView = field
        alert.addButton(withTitle: "Rename")
        alert.addButton(withTitle: "Cancel")
        alert.window.initialFirstResponder = field
        NSApp.activate(ignoringOtherApps: true)
        let name = field.stringValue.trimmingCharacters(in: .whitespaces)
        if alert.runModal() == .alertFirstButtonReturn {
            let new = field.stringValue.trimmingCharacters(in: .whitespaces)
            if !new.isEmpty, new != name { Identity.renameClient(fingerprint, to: new) }
        }
    }

    /// Asks once: to use it again, it has to be linked again. The Linux machine unlinks this Mac
    /// too (now if it's connected, otherwise once it next connects or sees this Mac online).
    static func confirmForget(_ label: String, _ fingerprint: String) {
        let alert = NSAlert()
        alert.messageText = "Unlink \(label)?"
        alert.informativeText = "It's unlinked on \(label) too. To use it again, link it again from there."
        alert.addButton(withTitle: "Unlink")
        alert.addButton(withTitle: "Cancel")
        NSApp.activate(ignoringOtherApps: true)
        guard alert.runModal() == .alertFirstButtonReturn else { return }
        forget(fingerprint)
    }

    /// What Unlink does once confirmed: the computer forgotten here, and told (now if it's the one
    /// connected, else when it next connects or sees this Mac online).
    static func forget(_ fingerprint: String) {
        Identity.removeClient(fingerprint)
        Identity.setDisconnected(fingerprint, false)
        if host.client == fingerprint {
            server.drop(fingerprint, telling: .forgotten)
        } else {
            Identity.setForgotten(fingerprint, true)
        }
    }

    private func toggle(_ title: String, _ on: Bool, _ change: @escaping (inout Config) -> Void) -> NSMenuItem {
        let i = ClosureMenuItem(title: title) { settings.update(change) }
        i.state = on ? .on : .off
        return i
    }
}

private final class ClosureMenuItem: NSMenuItem {
    private let run: () -> Void

    init(title: String, run: @escaping () -> Void) {
        self.run = run
        super.init(title: title, action: #selector(fire), keyEquivalent: "")
        target = self
    }

    required init(coder: NSCoder) { fatalError() }

    @objc private func fire() { run() }
}


/// A menu row with a tick that changes in place: clicking it doesn't close the menu (a plain
/// menu item always does), so several can be ticked or unticked in one go.
private final class StayOpenToggle: NSView {
    private let title: String
    private let icon: NSImage
    private var on: Bool
    private let changed: (Bool) -> Void
    private var hovered = false {
        didSet { needsDisplay = true }
    }

    init(title: String, icon: NSImage, on: Bool, changed: @escaping (Bool) -> Void) {
        self.title = title
        self.icon = icon
        self.on = on
        self.changed = changed
        let width = max(200, (title as NSString).size(withAttributes: [.font: NSFont.menuFont(ofSize: 0)]).width + 64)
        super.init(frame: NSRect(x: 0, y: 0, width: width, height: 22))
        autoresizingMask = .width
        setAccessibilityRole(.checkBox)
        setAccessibilityLabel(title)
    }

    required init?(coder: NSCoder) { fatalError() }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        trackingAreas.forEach(removeTrackingArea)
        addTrackingArea(NSTrackingArea(rect: bounds, options: [.mouseEnteredAndExited, .activeAlways, .inVisibleRect], owner: self))
    }

    override func mouseEntered(with event: NSEvent) { hovered = true }
    override func mouseExited(with event: NSEvent) { hovered = false }
    override func viewDidMoveToWindow() { hovered = false }

    override func mouseUp(with event: NSEvent) {
        on.toggle()
        needsDisplay = true
        changed(on)
    }

    override func accessibilityValue() -> Any? { on }

    override func draw(_ dirtyRect: NSRect) {
        let inner = bounds.insetBy(dx: 5, dy: 1)
        if hovered {
            NSColor.selectedContentBackgroundColor.setFill()
            NSBezierPath(roundedRect: inner, xRadius: 4, yRadius: 4).fill()
        }
        let ink: NSColor = hovered ? .selectedMenuItemTextColor : .labelColor
        let font = NSFont.menuFont(ofSize: 0)
        if on {
            let tick = "✓" as NSString
            let attrs: [NSAttributedString.Key: Any] = [.font: NSFont.systemFont(ofSize: font.pointSize, weight: .semibold), .foregroundColor: ink]
            let sz = tick.size(withAttributes: attrs)
            tick.draw(at: NSPoint(x: 12, y: (bounds.height - sz.height) / 2), withAttributes: attrs)
        }
        icon.draw(in: NSRect(x: 28, y: (bounds.height - 16) / 2, width: 16, height: 16))
        let attrs: [NSAttributedString.Key: Any] = [.font: font, .foregroundColor: ink]
        let text = title as NSString
        let sz = text.size(withAttributes: attrs)
        text.draw(at: NSPoint(x: 50, y: (bounds.height - sz.height) / 2), withAttributes: attrs)
    }
}
