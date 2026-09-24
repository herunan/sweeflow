import CoreGraphics
import Foundation

/// The MacBook's own screen, turned down to black while streaming, so it can run with the lid
/// slightly open (a closed lid locks the Mac, and a locked Mac can't be captured) without
/// lighting up or heating. Restored when streaming stops.
enum BuiltInScreen {
    private typealias Get = @convention(c) (CGDirectDisplayID, UnsafeMutablePointer<Float>) -> Int32
    private typealias Set = @convention(c) (CGDirectDisplayID, Float) -> Int32
    private static let lib = dlopen("/System/Library/PrivateFrameworks/DisplayServices.framework/DisplayServices", RTLD_LAZY)
    private static let getBrightness = lib.flatMap { dlsym($0, "DisplayServicesGetBrightness") }.map { unsafeBitCast($0, to: Get.self) }
    private static let setBrightness = lib.flatMap { dlsym($0, "DisplayServicesSetBrightness") }.map { unsafeBitCast($0, to: Set.self) }
    /// Kept on disk so a crash while dimmed still gets the brightness back on the next start.
    private static let savedKey = "builtInBrightnessBeforeDim"

    private static var builtIn: CGDirectDisplayID? {
        var ids = [CGDirectDisplayID](repeating: 0, count: 16)
        var n: UInt32 = 0
        guard CGGetOnlineDisplayList(16, &ids, &n) == .success else { return nil }
        return ids.prefix(Int(n)).first { CGDisplayIsBuiltin($0) != 0 }
    }

    private static var timer: Timer?
    private static var wasOnline = false

    /// Keeps the screen dark while streaming: now, and each time it comes back on (lid opened).
    /// Main thread.
    static func startDimming() {
        timer?.invalidate()
        timer = nil
        wasOnline = builtIn != nil
        dim()
        let t = Timer(timeInterval: 1, repeats: true) { _ in
            let online = builtIn != nil
            if online && !wasOnline { dim() }
            wasOnline = online
        }
        RunLoop.main.add(t, forMode: .common)
        timer = t
    }

    static func stopDimming() {
        timer?.invalidate()
        timer = nil
        guard !restore() else { return }
        // Lid still closed: put the brightness back once the screen is on again.
        let t = Timer(timeInterval: 2, repeats: true) { t in
            if restore() { t.invalidate(); if timer === t { timer = nil } }
        }
        RunLoop.main.add(t, forMode: .common)
        timer = t
    }

    private static func dim() {
        guard let id = builtIn, let get = getBrightness, let set = setBrightness else { return }
        var current: Float = 0
        guard get(id, &current) == 0 else { return }
        if UserDefaults.standard.object(forKey: savedKey) == nil {
            UserDefaults.standard.set(current, forKey: savedKey)
        }
        if set(id, 0) == 0 { log(String(format: "built-in screen dimmed (was %.2f)", current)) }
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
