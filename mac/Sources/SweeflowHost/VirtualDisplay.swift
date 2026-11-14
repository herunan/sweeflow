import AppKit
import CPrivate
import IOKit.pwr_mgt

/// An invisible HiDPI display sized to the Linux monitor. Streamed windows live here, so they can
/// be as large as the Linux screen, menus have room to open, and everything keeps rendering with
/// the MacBook's lid closed (when it is the only display left).
final class VirtualDisplay {
    private var display: CGVirtualDisplay?
    private(set) var size: CGSize = .zero

    var displayID: CGDirectDisplayID? { display?.displayID }

    /// Global CG bounds (top-left origin, points) of the display, if it is online.
    var bounds: CGRect? {
        guard let id = displayID else { return nil }
        let b = CGDisplayBounds(id)
        return b.isEmpty ? nil : b
    }

    /// The display's area clear of the menu bar and the Dock, in global CG coordinates. (The
    /// Dock sits on this display when it's the leftmost or rightmost one and the Dock is on that
    /// side: a window under it can't be clicked there.)
    var usableBounds: CGRect? {
        guard let id = displayID, let b = bounds else { return nil }
        guard let screen = NSScreen.screens.first(where: {
            ($0.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? NSNumber)?.uint32Value == id
        }) else { return b }
        let f = screen.frame, v = screen.visibleFrame
        // NSScreen is bottom-left based; inset the CG bounds by the same margins.
        let left = v.minX - f.minX, right = f.maxX - v.maxX, top = f.maxY - v.maxY, bottom = v.minY - f.minY
        return CGRect(x: b.minX + left, y: b.minY + top, width: b.width - left - right, height: b.height - top - bottom)
    }

    /// A plain backdrop: black in dark mode, white in light mode. Mac windows tint their
    /// sidebars and toolbars with what's behind them, so a neutral one keeps their colours
    /// clean. Follows the Mac's appearance; only this invisible display is touched.
    private var appearanceObserver: NSObjectProtocol?

    private func applyBackdrop(attempt: Int) {
        guard let id = displayID else { return }
        guard let screen = NSScreen.screens.first(where: {
            ($0.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? NSNumber)?.uint32Value == id
        }) else {
            if attempt < 20 {
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) { [weak self] in self?.applyBackdrop(attempt: attempt + 1) }
            }
            return
        }
        let dark = UserDefaults.standard.string(forKey: "AppleInterfaceStyle") == "Dark"
        let dir = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Library/Application Support/sweeflow")
        // Old copies of the Linux wallpaper (an earlier version), no longer used.
        for f in (try? FileManager.default.contentsOfDirectory(at: dir, includingPropertiesForKeys: nil)) ?? []
        where f.lastPathComponent.hasPrefix("wallpaper-") {
            try? FileManager.default.removeItem(at: f)
        }
        let url = dir.appendingPathComponent(dark ? "backdrop-black.png" : "backdrop-white.png")
        if !FileManager.default.fileExists(atPath: url.path) {
            let image = NSImage(size: NSSize(width: 16, height: 16))
            image.lockFocus()
            (dark ? NSColor.black : NSColor.white).setFill()
            NSRect(x: 0, y: 0, width: 16, height: 16).fill()
            image.unlockFocus()
            if let tiff = image.tiffRepresentation, let rep = NSBitmapImageRep(data: tiff) {
                try? rep.representation(using: .png, properties: [:])?.write(to: url)
            }
        }
        if NSWorkspace.shared.desktopImageURL(for: screen) != url {
            try? NSWorkspace.shared.setDesktopImageURL(url, for: screen, options: [
                .imageScaling: NSImageScaling.scaleAxesIndependently.rawValue, .allowClipping: false,
            ])
            log("virtual display backdrop: \(dark ? "black" : "white")")
        }
        if appearanceObserver == nil {
            appearanceObserver = DistributedNotificationCenter.default().addObserver(
                forName: Notification.Name("AppleInterfaceThemeChangedNotification"), object: nil, queue: .main
            ) { [weak self] _ in self?.applyBackdrop(attempt: 0) }
        }
    }

    /// Size the client asked for, kept so a failed creation can be retried.
    private var wanted: CGSize = .zero
    private var retries = 0

    func remove() {
        display = nil
        size = .zero
        wanted = .zero
    }

    /// Tries again to create the display if it's wanted but missing (e.g. after an unlock).
    func retry() {
        guard display == nil, wanted != .zero else { return }
        ensure(points: wanted)
    }

    /// Creates or resizes the display to `points` (logical size of the Linux monitor).
    /// Whether the Mac's own screen is shut (lid closed) and no other display than Sweeflow's is
    /// on: then Sweeflow's virtual display is all that keeps the Mac awake and unlocked.
    static var onlyDisplayLeft: Bool {
        var ids = [CGDirectDisplayID](repeating: 0, count: 16), n: UInt32 = 0
        CGGetOnlineDisplayList(16, &ids, &n)
        return !ids.prefix(Int(n)).contains { CGDisplayVendorNumber($0) != 0x4D46 }
    }

    /// See `--hold-display` in main.swift. Never returns.
    static func holdDuringRestart() -> Never {
        guard onlyDisplayLeft else { exit(0) }
        let desc = CGVirtualDisplayDescriptor()
        desc.queue = DispatchQueue.main
        desc.name = "Sweeflow (restarting)"
        desc.maxPixelsWide = 1280; desc.maxPixelsHigh = 800
        desc.sizeInMillimeters = CGSize(width: 300, height: 190)
        desc.vendorID = 0x4D46
        desc.productID = 0x0002
        desc.serialNum = 2
        guard let vd = CGVirtualDisplay(descriptor: desc) else { log("hold display: creation failed"); exit(1) }
        let settings = CGVirtualDisplaySettings()
        settings.hiDPI = 0
        settings.modes = [CGVirtualDisplayMode(width: 1280, height: 800, refreshRate: 60)]
        guard vd.apply(settings) else { log("hold display: applySettings failed"); exit(1) }
        log("hold display: up while Sweeflow restarts")
        let mine = vd.displayID
        let start = Date()
        Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { _ in
            var ids = [CGDirectDisplayID](repeating: 0, count: 16), n: UInt32 = 0
            CGGetOnlineDisplayList(16, &ids, &n)
            let other = ids.prefix(Int(n)).contains { $0 != mine && CGDisplayVendorNumber($0) == 0x4D46 }
            // The new Sweeflow's display has been up a little while (windows moved onto it).
            if other { holdOther += 1 }
            if holdOther >= 3 || Date().timeIntervalSince(start) > 60 {
                log("hold display: \(holdOther >= 3 ? "Sweeflow's display is back" : "timed out"), letting go")
                withExtendedLifetime(vd) { exit(0) }
            }
        }
        RunLoop.main.run()
        exit(0)
    }
    private static var holdOther = 0

    func ensure(points: CGSize) {
        let w = UInt32(points.width.rounded()), h = UInt32(points.height.rounded())
        guard w >= 640, h >= 480 else { return }
        if wanted != points { retries = 0 }
        wanted = points
        if display != nil && size == points { return }
        display = nil   // releasing the object removes the display

        let desc = CGVirtualDisplayDescriptor()
        desc.queue = DispatchQueue.main
        desc.name = "Sweeflow"
        desc.maxPixelsWide = w * 2
        desc.maxPixelsHigh = h * 2
        // ~110 ppi, like a 34" ultrawide; only used for macOS's physical-size heuristics.
        desc.sizeInMillimeters = CGSize(width: Double(w) * 25.4 / 110, height: Double(h) * 25.4 / 110)
        desc.vendorID = 0x4D46   // "MF"
        desc.productID = 0x0001
        desc.serialNum = 1
        desc.terminationHandler = { _, _ in log("virtual display terminated") }

        guard let vd = CGVirtualDisplay(descriptor: desc) else {
            // Fails while the screen is locked, among other transient states: keep trying.
            retries += 1
            log("virtual display: creation failed (attempt \(retries))")
            if retries < 60 {
                DispatchQueue.main.asyncAfter(deadline: .now() + 5) { [weak self] in
                    guard let self, self.wanted == points else { return }
                    self.ensure(points: points)
                }
            }
            return
        }
        let settings = CGVirtualDisplaySettings()
        settings.hiDPI = 1
        // 120 Hz so apps animate (and scroll) at more than the Linux screen's rate; 60 if refused.
        settings.modes = [CGVirtualDisplayMode(width: w, height: h, refreshRate: 120)]
        if !vd.apply(settings) {
            settings.modes = [CGVirtualDisplayMode(width: w, height: h, refreshRate: 60)]
            guard vd.apply(settings) else {
                log("virtual display: applySettings failed")
                return
            }
            log("virtual display: 120 Hz refused, using 60 Hz")
        }
        display = vd
        size = points
        log("virtual display \(vd.displayID) created: \(w)x\(h) points")
        applyBackdrop(attempt: 0)
    }
}

/// Keeps the Mac awake (lid closed included) while a client is connected.
final class Wakefulness {
    private var assertions: [IOPMAssertionID] = []
    private var lidSleepDisabled = false

    func hold() {
        guard assertions.isEmpty else { return }
        for type in [kIOPMAssertionTypePreventSystemSleep, kIOPMAssertPreventUserIdleDisplaySleep] {
            var id = IOPMAssertionID(0)
            if IOPMAssertionCreateWithName(type as CFString, IOPMAssertionLevel(kIOPMAssertionLevelOn),
                                           "Sweeflow is streaming to Linux" as CFString, &id) == kIOReturnSuccess {
                assertions.append(id)
            }
        }
        // Closing the lid sleeps a MacBook regardless of assertions; `pmset disablesleep` is the
        // only switch. `sweeflow setup` installs a sudoers rule allowing exactly these two calls.
        lidSleepDisabled = setLidSleep(disabled: true)
    }

    func release() {
        assertions.forEach { IOPMAssertionRelease($0) }
        assertions.removeAll()
        if lidSleepDisabled {
            _ = setLidSleep(disabled: false)
            lidSleepDisabled = false
        }
    }

    /// Streaming is switched off but Linux is still connected: no idle sleep, so Linux can switch
    /// it back on (a sleeping Mac drops off the network). The screen may still turn off, and
    /// closing the lid still sleeps it.
    private var reachable: IOPMAssertionID?

    func holdReachable() {
        guard reachable == nil else { return }
        var id = IOPMAssertionID(0)
        if IOPMAssertionCreateWithName(kIOPMAssertionTypePreventUserIdleSystemSleep as CFString, IOPMAssertionLevel(kIOPMAssertionLevelOn),
                                       "Sweeflow: Linux is connected (streaming off)" as CFString, &id) == kIOReturnSuccess {
            reachable = id
        }
    }

    func releaseReachable() {
        if let id = reachable { IOPMAssertionRelease(id) }
        reachable = nil
    }

    /// Restores normal lid-close sleep (e.g. after a crash left it disabled).
    static func resetLidSleep() { _ = Wakefulness().setLidSleep(disabled: false) }

    @discardableResult
    private func setLidSleep(disabled: Bool) -> Bool {
        do {
            try run("/usr/bin/sudo", ["-n", "/usr/bin/pmset", "-a", "disablesleep", disabled ? "1" : "0"])
            log("lid-close sleep \(disabled ? "disabled" : "restored")")
            return true
        } catch {
            if disabled { log("cannot disable lid-close sleep (no sudoers rule; run `sweeflow setup` in a terminal)") }
            return false
        }
    }
}
