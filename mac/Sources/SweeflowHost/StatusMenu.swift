import AppKit

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
        NotificationCenter.default.addObserver(forName: .sweeflowSettingsChanged, object: nil, queue: .main) { [weak self] _ in
            self?.updateIcon()
        }
    }

    private func updateIcon() {
        let on = settings.config.enabled
        // The logo (a template: macOS draws it in the menu bar's colour); faded while paused.
        let image = NSImage(named: "MenuIcon")
            ?? NSImage(systemSymbolName: "macwindow.on.rectangle", accessibilityDescription: "Sweeflow")
        image?.isTemplate = true
        // The logo is wider than tall (a wrapped sweet): the menu bar's usual height, its own width.
        if let i = image, i.size.height > 0 {
            i.size = NSSize(width: (16 * i.size.width / i.size.height).rounded(), height: 16)
        }
        item.button?.image = image
        item.button?.appearsDisabled = !on
        item.button?.toolTip = on ? "Sweeflow: streaming to Linux" : "Sweeflow: paused"
    }

    /// Built when opened, so it always shows the current state. Only what concerns this Mac:
    /// the connection, the apps, this screen, starting at login, and the Linux computers it's
    /// linked to. (How Mac apps show up on Linux is set on Linux.)
    func menuNeedsUpdate(_ menu: NSMenu) {
        menu.removeAllItems()
        let c = settings.config
        let client = host.client
        let refused = Identity.disconnectedClients().intersection(Identity.pairedClients().map(\.fingerprint))
        let statusText: String
        if let fp = client {
            statusText = c.enabled ? "Streaming to \(Identity.clientName(fp))" : "Paused · \(Identity.clientName(fp))"
        } else {
            statusText = refused.isEmpty ? "Not connected" : "Disconnected"
        }
        let status = NSMenuItem(title: statusText, action: nil, keyEquivalent: "")
        status.isEnabled = false
        menu.addItem(status)
        if let fp = client {
            menu.addItem(ClosureMenuItem(title: c.enabled ? "Pause" : "Resume") { settings.update { $0.enabled.toggle() } })
            menu.addItem(ClosureMenuItem(title: "Disconnect") { Self.disconnect(fp) })
        } else if let fp = refused.first {
            menu.addItem(ClosureMenuItem(title: "Connect") { Self.connect(fp) })
        }
        menu.addItem(.separator())

        let apps = NSMenuItem(title: "Apps", action: nil, keyEquivalent: "")
        let sub = NSMenu()
        for a in AppList.found() {
            let i = toggle(a.name, !c.excludeApps.contains(a.bundleId)) { c in
                if let n = c.excludeApps.firstIndex(of: a.bundleId) { c.excludeApps.remove(at: n) } else { c.excludeApps.append(a.bundleId) }
            }
            let icon = NSWorkspace.shared.icon(forFile: a.path)
            icon.size = NSSize(width: 16, height: 16)
            i.image = icon
            sub.addItem(i)
        }
        apps.submenu = sub
        apps.toolTip = "Which apps' windows go to Linux"
        menu.addItem(apps)
        menu.addItem(.separator())

        let dim = ClosureMenuItem(title: c.dimMacScreenWhileStreaming ? "Display dimmed" : "Dim display") {
            settings.update { $0.dimMacScreenWhileStreaming = true }
        }
        dim.isEnabled = client != nil && c.enabled && !c.dimMacScreenWhileStreaming
        dim.toolTip = "Turns this screen down until someone uses this Mac's keyboard or trackpad"
        menu.addItem(dim)
        let login = ClosureMenuItem(title: "Open at login") {
            if LoginItem.enabled { LoginItem.disable() } else { LoginItem.enable() }
        }
        login.state = LoginItem.enabled ? .on : .off
        login.isEnabled = !LoginItem.devInstall
        login.toolTip = LoginItem.devInstall ? "A development install always starts at login" : "Start Sweeflow when you log in to this Mac"
        menu.addItem(login)
        menu.addItem(.separator())

        let linked = NSMenuItem(title: "Linux computers", action: nil, keyEquivalent: "")
        let list = NSMenu()
        let link = ClosureMenuItem(title: "Link a Linux computer…") { PairingWindow.shared.show() }
        link.toolTip = "Shows a code to type on the Linux computer"
        list.addItem(link)
        let paired = Identity.pairedClients()
        if !paired.isEmpty { list.addItem(.separator()) }
        for p in paired {
            let name = Identity.clientName(p.fingerprint)
            let computer = NSMenuItem(title: name, action: nil, keyEquivalent: "")
            let options = NSMenu()
            if p.fingerprint == client {
                computer.state = .on
                options.addItem(ClosureMenuItem(title: "Disconnect") { Self.disconnect(p.fingerprint) })
            } else if refused.contains(p.fingerprint) {
                options.addItem(ClosureMenuItem(title: "Connect") { Self.connect(p.fingerprint) })
            } else {
                let idle = NSMenuItem(title: "Not connected", action: nil, keyEquivalent: "")
                idle.isEnabled = false
                options.addItem(idle)
            }
            options.addItem(ClosureMenuItem(title: "Rename…") { Self.rename(p.fingerprint) })
            options.addItem(ClosureMenuItem(title: "Forget…") { Self.confirmForget(name, p.fingerprint) })
            computer.submenu = options
            list.addItem(computer)
        }
        linked.submenu = list
        menu.addItem(linked)

        // Only while something's missing (a permission, starting at login, a linked computer).
        if !Onboarding.complete {
            menu.addItem(.separator())
            let setup = ClosureMenuItem(title: "Finish setting up…") { Onboarding.shared.show() }
            setup.toolTip = "Permissions Sweeflow still needs on this Mac"
            menu.addItem(setup)
        }
    }

    /// Ends that computer's connection and refuses it until Connect (it shows "Disconnected on
    /// the Mac" meanwhile).
    private static func disconnect(_ fingerprint: String) {
        Identity.setDisconnected(fingerprint, true)
        server.drop(fingerprint, telling: .macDisconnected)
    }

    /// Lets it in again: it connects by itself within a few seconds.
    private static func connect(_ fingerprint: String) {
        Identity.setDisconnected(fingerprint, false)
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

    /// Asks once: linking it again needs a new code. The Linux computer forgets this Mac too
    /// (now, or the next time it connects).
    private static func confirmForget(_ label: String, _ fingerprint: String) {
        let alert = NSAlert()
        alert.messageText = "Forget \(label)?"
        alert.informativeText = "It forgets this Mac too. Linking them again needs a new code."
        alert.addButton(withTitle: "Forget")
        alert.addButton(withTitle: "Cancel")
        NSApp.activate(ignoringOtherApps: true)
        guard alert.runModal() == .alertFirstButtonReturn else { return }
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
