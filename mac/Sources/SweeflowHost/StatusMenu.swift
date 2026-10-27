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
    /// streaming on or off, the apps, this screen, starting with the Mac, and the Linux
    /// computers it's linked to. (How Mac apps show up on Linux is set on Linux.)
    func menuNeedsUpdate(_ menu: NSMenu) {
        menu.removeAllItems()
        let c = settings.config
        let status = NSMenuItem(title: !host.connected ? "No Linux computer connected" : c.enabled ? "Streaming to Linux" : "Paused",
                                action: nil, keyEquivalent: "")
        status.isEnabled = false
        menu.addItem(status)
        menu.addItem(ClosureMenuItem(title: c.enabled ? "Pause" : "Resume") { settings.update { $0.enabled.toggle() } })
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

        menu.addItem(.sectionHeader(title: "This Mac"))
        let dim = ClosureMenuItem(title: c.dimMacScreenWhileStreaming ? "Screen Dimmed" : "Dim This Screen") {
            settings.update { $0.dimMacScreenWhileStreaming = true }
        }
        dim.isEnabled = c.enabled && !c.dimMacScreenWhileStreaming
        dim.toolTip = "Turns this screen down until someone uses this Mac's keyboard or trackpad"
        menu.addItem(dim)
        let background = NSMenuItem(title: "Window Background", action: nil, keyEquivalent: "")
        let b = NSMenu()
        for (title, value) in [("Dark", "dark"), ("Light", "light")] {
            b.addItem(toggle(title, (value == "dark") == Appearance.isDark) { $0.appearance = value })
        }
        background.submenu = b
        background.toolTip = "The background of Mac windows (this Mac's dark or light mode)"
        menu.addItem(background)
        let login = ClosureMenuItem(title: "Open at Login") {
            if LoginItem.enabled { LoginItem.disable() } else { LoginItem.enable() }
        }
        login.state = LoginItem.enabled ? .on : .off
        login.isEnabled = !LoginItem.devInstall
        login.toolTip = LoginItem.devInstall ? "A development install always starts at login" : "Start Sweeflow when you log in to this Mac"
        menu.addItem(login)

        menu.addItem(.sectionHeader(title: "Linked Computers"))
        for p in Identity.pairedClients() {
            let label = p.name.isEmpty ? "Linux \(PairingCode.of(fingerprint: p.fingerprint))" : p.name
            let computer = NSMenuItem(title: label, action: nil, keyEquivalent: "")
            let options = NSMenu()
            options.addItem(ClosureMenuItem(title: "Forget…") { Self.confirmForget(label, p.fingerprint) })
            computer.submenu = options
            menu.addItem(computer)
        }
        let link = ClosureMenuItem(title: "Link a Linux Computer…") { PairingWindow.shared.show() }
        link.toolTip = "Shows a code to type on the Linux computer"
        menu.addItem(link)

        // Only while something's missing (a permission, starting at login, a linked computer).
        if !Onboarding.complete {
            menu.addItem(.separator())
            let setup = ClosureMenuItem(title: "Finish Setting Up…") { Onboarding.shared.show() }
            setup.toolTip = "Permissions Sweeflow still needs on this Mac"
            menu.addItem(setup)
        }
    }

    /// Asks once: linking it again needs a new code.
    private static func confirmForget(_ label: String, _ fingerprint: String) {
        let alert = NSAlert()
        alert.messageText = "Forget \(label)?"
        alert.informativeText = "Linking it again needs a new code."
        alert.addButton(withTitle: "Forget")
        alert.addButton(withTitle: "Cancel")
        NSApp.activate(ignoringOtherApps: true)
        if alert.runModal() == .alertFirstButtonReturn { Identity.removeClient(fingerprint) }
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
