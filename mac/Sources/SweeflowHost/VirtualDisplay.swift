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

    /// Area below the display's menu bar, in global CG coordinates.
    var usableBounds: CGRect? {
        guard let id = displayID, let b = bounds else { return nil }
        guard let screen = NSScreen.screens.first(where: {
            ($0.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? NSNumber)?.uint32Value == id
        }) else { return b }
        let menuBar = screen.frame.maxY - screen.visibleFrame.maxY
        return CGRect(x: b.minX, y: b.minY + menuBar, width: b.width, height: b.height - menuBar)
    }

    /// An earlier version gave this display the Linux wallpaper; put macOS's default back (and
    /// delete the copies). The display's NSScreen appears a moment after it's created.
    private func resetOldWallpaper(attempt: Int) {
        guard let id = displayID else { return }
        guard let screen = NSScreen.screens.first(where: {
            ($0.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? NSNumber)?.uint32Value == id
        }) else {
            if attempt < 20 {
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) { [weak self] in self?.resetOldWallpaper(attempt: attempt + 1) }
            }
            return
        }
        let dir = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Library/Application Support/sweeflow")
        if let current = NSWorkspace.shared.desktopImageURL(for: screen), current.path.hasPrefix(dir.path) {
            try? NSWorkspace.shared.setDesktopImageURL(URL(fileURLWithPath: "/System/Library/CoreServices/DefaultDesktop.heic"), for: screen, options: [:])
            log("virtual display wallpaper back to the default")
        }
        for f in (try? FileManager.default.contentsOfDirectory(at: dir, includingPropertiesForKeys: nil)) ?? []
        where f.lastPathComponent.hasPrefix("wallpaper-") {
            try? FileManager.default.removeItem(at: f)
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
        resetOldWallpaper(attempt: 0)
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
