import Foundation

/// The Dock hides itself (auto-hide) while a Linux computer is being streamed to: on the virtual
/// display it took room from every window (a window couldn't be wider than the display less
/// the Dock) and covered their edges. The person's own setting is noted first and put back once
/// streaming ends, also after a crash (it's kept in the defaults until then). Through the call
/// System Settings itself makes, so the Dock isn't restarted.
enum DockHider {
    private typealias Get = @convention(c) () -> Bool
    private typealias Set = @convention(c) (Bool) -> Void
    private static let handle = dlopen("/System/Library/Frameworks/ApplicationServices.framework/ApplicationServices", RTLD_LAZY)
    private static let get: Get? = dlsym(handle, "CoreDockGetAutoHideEnabled").map { unsafeBitCast($0, to: Get.self) }
    private static let set: Set? = dlsym(handle, "CoreDockSetAutoHideEnabled").map { unsafeBitCast($0, to: Set.self) }
    /// What auto-hide was before Sweeflow turned it on (absent: Sweeflow hasn't).
    private static let key = "dockAutoHideBefore"

    static func hide() {
        guard UserDefaults.standard.object(forKey: key) == nil, let get, let set else { return }
        let was = get()
        UserDefaults.standard.set(was, forKey: key)
        if !was {
            set(true)
            log("Dock: hidden while streaming")
        }
    }

    static func restore() {
        guard let was = UserDefaults.standard.object(forKey: key) as? Bool else { return }
        UserDefaults.standard.removeObject(forKey: key)
        if !was {
            set?(false)
            log("Dock: shown again, as it was")
        }
    }
}
