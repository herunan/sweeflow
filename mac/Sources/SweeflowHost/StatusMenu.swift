import AppKit

/// The menu-bar item: the same switches as the Linux tray menu (see Settings).
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

    /// Built when opened, so it always shows the current state.
    func menuNeedsUpdate(_ menu: NSMenu) {
        menu.removeAllItems()
        let c = settings.config
        let status = NSMenuItem(title: host.connected ? "Linux connected" : "No Linux connected", action: nil, keyEquivalent: "")
        status.isEnabled = false
        menu.addItem(status)
        menu.addItem(toggle("Stream to Linux", c.enabled) { $0.enabled.toggle() })
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
        apps.isEnabled = c.enabled
        menu.addItem(apps)
        menu.addItem(.separator())

        for (title, on, change) in [
            ("Mac Sound on Linux", c.soundOnLinux, { (c: inout Config) in c.soundOnLinux.toggle() }),
            ("Dim Mac Screen", c.dimMacScreenWhileStreaming, { (c: inout Config) in c.dimMacScreenWhileStreaming.toggle() }),
            ("Shared Clipboard", c.clipboard, { (c: inout Config) in c.clipboard.toggle() }),
            ("Drag and Drop", c.dragDrop, { (c: inout Config) in c.dragDrop.toggle() }),
            ("Mac Apps in Linux Menu", c.launcherMenu, { (c: inout Config) in c.launcherMenu.toggle() }),
            ("Mac Apps on Own Linux Desktop", c.ownDesktop, { (c: inout Config) in c.ownDesktop.toggle() }),
        ] as [(String, Bool, (inout Config) -> Void)] {
            let i = toggle(title, on, change)
            i.isEnabled = c.enabled
            menu.addItem(i)
        }

        let quality = NSMenuItem(title: "Quality", action: nil, keyEquivalent: "")
        let q = NSMenu()
        for (title, value) in [("Sharpest", "sharp"), ("Lighter (for slower networks)", "light")] {
            q.addItem(toggle(title, c.quality == value) { $0.quality = value })
        }
        quality.submenu = q
        quality.isEnabled = c.enabled
        menu.addItem(quality)
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
