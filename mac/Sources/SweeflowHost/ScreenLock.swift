import AppKit

/// Whether the Mac's screen is locked. While it is, ScreenCaptureKit only delivers blank frames,
/// so the streamers skip them (the windows keep their last picture) and Linux is told.
enum ScreenLock {
    private static let lock = NSLock()
    private static var locked = currentlyLocked()
    private static var observers: [() -> Void] = []

    static var isLocked: Bool { lock.lock(); defer { lock.unlock() }; return locked }

    /// Calls `handler` (main thread) whenever the lock state changes.
    static func onChange(_ handler: @escaping () -> Void) {
        DispatchQueue.main.async {
            if observers.isEmpty { startObserving() }
            observers.append(handler)
        }
    }

    private static func startObserving() {
        let dnc = DistributedNotificationCenter.default()
        for (name, value) in [("com.apple.screenIsLocked", true), ("com.apple.screenIsUnlocked", false)] {
            dnc.addObserver(forName: Notification.Name(name), object: nil, queue: .main) { _ in set(value) }
        }
        // In case a notification is missed (e.g. across sleep).
        let t = Timer(timeInterval: 2, repeats: true) { _ in set(currentlyLocked()) }
        RunLoop.main.add(t, forMode: .common)
    }

    private static func set(_ value: Bool) {
        lock.lock()
        let changed = locked != value
        locked = value
        lock.unlock()
        guard changed else { return }
        log("screen \(value ? "locked" : "unlocked")")
        observers.forEach { $0() }
    }

    private static func currentlyLocked() -> Bool {
        let d = CGSessionCopyCurrentDictionary() as? [String: Any]
        return d?["CGSSessionScreenIsLocked"] as? Bool ?? false
    }
}
