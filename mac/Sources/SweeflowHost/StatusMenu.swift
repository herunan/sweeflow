import AppKit

/// The menu-bar item: the same switches, in the same order, as the Linux tray widget (see Settings).
final class StatusMenu: NSObject, NSMenuDelegate {
    private let item = NSStatusBar.system.statusItem(withLength: NSStatusItem.squareLength)
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
        let image = NSImage(systemSymbolName: on ? "macwindow.on.rectangle" : "rectangle.on.rectangle.slash",
                            accessibilityDescription: "Sweeflow")
        image?.isTemplate = true
        item.button?.image = image
        item.button?.appearsDisabled = !on
    }

    /// Built when opened, so it always shows the current state. Laid out like the Linux tray
    /// widget: where things stand and Pause / Resume first, then the apps, then the settings in
    /// sections. Nothing is greyed out: settings changed while paused apply when streaming resumes.
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
        menu.addItem(apps)

        menu.addItem(.sectionHeader(title: "Where Mac Apps Appear"))
        menu.addItem(toggle("Their Own Desktop", c.ownDesktop) { $0.ownDesktop.toggle() })
        menu.addItem(toggle("Application Launcher", c.launcherMenu) { $0.launcherMenu.toggle() })

        menu.addItem(.sectionHeader(title: "Streaming"))
        menu.addItem(toggle("Play Mac Sound on Linux", c.soundOnLinux) { $0.soundOnLinux.toggle() })
        menu.addItem(toggle("Shared Clipboard", c.clipboard) { $0.clipboard.toggle() })
        menu.addItem(toggle("Drag and Drop Files", c.dragDrop) { $0.dragDrop.toggle() })
        menu.addItem(toggle("Dim This Screen", c.dimMacScreenWhileStreaming) { $0.dimMacScreenWhileStreaming.toggle() })
        menu.addItem(toggle("High Quality", c.quality != "light") { $0.quality = $0.quality == "light" ? "sharp" : "light" })
        let backdrop = NSMenuItem(title: "Window Backdrop", action: nil, keyEquivalent: "")
        let b = NSMenu()
        for (title, value) in [("Match the Mac", "auto"), ("Dark", "dark"), ("Light", "light")] {
            b.addItem(toggle(title, c.backdrop == value) { $0.backdrop = value })
        }
        backdrop.submenu = b
        backdrop.toolTip = "What Mac windows sit on: their see-through sidebars and toolbars take on its tone"
        menu.addItem(backdrop)

        menu.addItem(.sectionHeader(title: "Linked Computers"))
        for p in Identity.pairedClients() {
            let label = p.name.isEmpty ? "Linux \(PairingCode.of(fingerprint: p.fingerprint))" : p.name
            let computer = NSMenuItem(title: label, action: nil, keyEquivalent: "")
            let options = NSMenu()
            options.addItem(ClosureMenuItem(title: "Forget…") { Self.confirmForget(label, p.fingerprint) })
            computer.submenu = options
            menu.addItem(computer)
        }
        menu.addItem(ClosureMenuItem(title: "Link Another Computer…") { PairingWindow.shared.show() })

        menu.addItem(.sectionHeader(title: "General"))
        menu.addItem(ClosureMenuItem(title: "Setup…") { Onboarding.shared.show() })
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
