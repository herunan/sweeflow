import CoreGraphics
import AppKit

/// The MacBook's own screen, turned down to near-black while streaming, so it can run with the lid
/// slightly open (a closed lid locks the Mac, and a locked Mac can't be captured) without
/// lighting up or heating. Restored when streaming stops.
enum BuiltInScreen {
    private typealias Get = @convention(c) (CGDirectDisplayID, UnsafeMutablePointer<Float>) -> Int32
    private typealias Set = @convention(c) (CGDirectDisplayID, Float) -> Int32
    private static let lib = dlopen("/System/Library/PrivateFrameworks/DisplayServices.framework/DisplayServices", RTLD_LAZY)
    private static let getBrightness = lib.flatMap { dlsym($0, "DisplayServicesGetBrightness") }.map { unsafeBitCast($0, to: Get.self) }
    private static let setBrightness = lib.flatMap { dlsym($0, "DisplayServicesSetBrightness") }.map { unsafeBitCast($0, to: Set.self) }
    /// Kept on disk so a crash while dimmed still gets the brightness back on the next start.
    static let dimLevel: Float = 0.01
    private static let savedKey = "builtInBrightnessBeforeDim"

    private static var builtIn: CGDirectDisplayID? {
        var ids = [CGDirectDisplayID](repeating: 0, count: 16)
        var n: UInt32 = 0
        guard CGGetOnlineDisplayList(16, &ids, &n) == .success else { return nil }
        return ids.prefix(Int(n)).first { CGDisplayIsBuiltin($0) != 0 }
    }

    private static var wasOnline = false

    /// Keeps the screen dark while streaming: now, and each time it comes back on (lid opened).
    /// Main thread.
    /// Screens come and go (lid opened or closed): macOS says so; nothing runs in between.
    private static var observer: NSObjectProtocol?
    private static var dimming = false
    /// Lit up for a moment (Sweeflow showing something there, like pairing), even while streaming.
    private static var paused = false

    /// The built-in screen, for Sweeflow's own windows: the one a person at the Mac looks at.
    static var screen: NSScreen? {
        NSScreen.screens.first { s in
            (s.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? CGDirectDisplayID).map { CGDisplayIsBuiltin($0) != 0 } ?? false
        }
    }

    /// Brightens the screen until `resume()`, so a window shown there can be seen. Main thread.
    static func pause() {
        guard !paused else { return }
        paused = true
        restore()
    }

    static func resume() {
        guard paused else { return }
        paused = false
        if dimming { dim() }
    }

    private static func watchScreens() {
        guard observer == nil else { return }
        observer = NotificationCenter.default.addObserver(forName: NSApplication.didChangeScreenParametersNotification,
                                                          object: nil, queue: .main) { _ in
            let online = builtIn != nil
            if dimming && !paused {
                if online && !wasOnline { dim() }
            } else {
                restore()   // brightness still owed from when the lid was closed
            }
            wasOnline = online
        }
    }

    static func startDimming() {
        dimming = true
        wasOnline = builtIn != nil
        if !paused { dim() }
        watchScreens()
    }

    static func stopDimming() {
        dimming = false
        restore()   // if the lid is closed, done when the screen comes back (watchScreens)
        watchScreens()
    }

    private static func dim() {
        guard let id = builtIn, let get = getBrightness, let set = setBrightness else { return }
        var current: Float = 0
        guard get(id, &current) == 0 else { return }
        if UserDefaults.standard.object(forKey: savedKey) == nil && current > dimLevel {
            UserDefaults.standard.set(current, forKey: savedKey)
        }
        // Not 0: that switches the backlight off, which macOS counts as the display turning off
        // (and the Mac may then lock within seconds).
        if set(id, dimLevel) == 0 { log(String(format: "built-in screen dimmed (was %.2f)", current)) }
    }

    /// True once nothing is left to restore.
    @discardableResult
    static func restore() -> Bool {
        guard let saved = UserDefaults.standard.object(forKey: savedKey) as? Float else { return true }
        // With the lid closed the screen is offline: the caller tries again later.
        guard let id = builtIn, let set = setBrightness, set(id, max(saved, 0.1)) == 0 else { return false }
        UserDefaults.standard.removeObject(forKey: savedKey)
        log(String(format: "built-in screen brightness restored to %.2f", saved))
        return true
    }
}
