import AppKit

/// The Mac's dark or light mode: what the window background setting shows and switches.
/// The setting mirrors the Mac (changed on the Mac, it changes here too); changed from Linux or
/// the menu-bar item, it switches the Mac (through System Events, which asks once).
enum Appearance {
    static var isDark: Bool {
        NSApp.effectiveAppearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
    }

    private static var observers: [NSObjectProtocol] = []

    /// Main thread, once.
    static func start() {
        guard observers.isEmpty else { return }
        mirror()
        observers.append(DistributedNotificationCenter.default().addObserver(
            forName: Notification.Name("AppleInterfaceThemeChangedNotification"), object: nil, queue: .main
        ) { _ in
            // The app's own appearance follows a moment later.
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) { mirror() }
        })
        observers.append(NotificationCenter.default.addObserver(forName: .sweeflowSettingsChanged, object: nil, queue: .main) { _ in
            let wanted = settings.config.appearance
            guard ["dark", "light"].contains(wanted), (wanted == "dark") != isDark else { return }
            switchMac(dark: wanted == "dark")
        })
    }

    /// The setting follows the Mac.
    private static func mirror() {
        let now = isDark ? "dark" : "light"
        if settings.config.appearance != now { settings.update { $0.appearance = now } }
    }

    private static func switchMac(dark: Bool) {
        log("window background: switching the Mac to \(dark ? "dark" : "light") mode")
        DispatchQueue.global().async {
            let p = Process()
            p.executableURL = URL(fileURLWithPath: "/usr/bin/osascript")
            p.arguments = ["-e", "tell application \"System Events\" to tell appearance preferences to set dark mode to \(dark)"]
            let err = Pipe()
            p.standardError = err
            do {
                try p.run()
                p.waitUntilExit()
                if p.terminationStatus != 0 {
                    let msg = String(data: err.fileHandleForReading.readDataToEndOfFile(), encoding: .utf8) ?? ""
                    log("window background: couldn't switch (\(msg.trimmingCharacters(in: .whitespacesAndNewlines)))")
                    // Show the Mac's actual mode again.
                    DispatchQueue.main.async { mirror() }
                }
            } catch {
                log("window background: \(error)")
            }
        }
    }
}
