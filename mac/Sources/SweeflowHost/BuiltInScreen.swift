import CoreGraphics
import AppKit

/// The MacBook's own screen, turned down to near-black while streaming, so it can run with the lid
/// slightly open (a closed lid locks the Mac, and a locked Mac can't be captured) without
/// lighting up or heating. Asked for from either side; restored as soon as someone uses the Mac
/// itself (its own keyboard or trackpad), or when streaming stops. It stays dark through anything
/// else that brightens it (macOS does on some input from Linux).
enum BuiltInScreen {
    private typealias Get = @convention(c) (CGDirectDisplayID, UnsafeMutablePointer<Float>) -> Int32
    private typealias Set = @convention(c) (CGDirectDisplayID, Float) -> Int32
    private static let lib = dlopen("/System/Library/PrivateFrameworks/DisplayServices.framework/DisplayServices", RTLD_LAZY)
    private static let getBrightness = lib.flatMap { dlsym($0, "DisplayServicesGetBrightness") }.map { unsafeBitCast($0, to: Get.self) }
    private static let setBrightness = lib.flatMap { dlsym($0, "DisplayServicesSetBrightness") }.map { unsafeBitCast($0, to: Set.self) }
    /// Kept on disk so a crash while dimmed still gets the brightness back on the next start.
    static let dimLevel: Float = 0.01
    private static let savedKey = "builtInBrightnessBeforeDim"
    /// Never brought back dimmer than this.
    private static let minRestore: Float = 0.5
    /// The brightness it last had while lit (what a dull reading is replaced with).
    private static var lastLit: Float = 0.75

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
        watchUse()
    }

    static func stopDimming() {
        dimming = false
        watch?.invalidate()
        watch = nil
        if !connected { stopWatchingLocalInput() }
        restore()   // if the lid is closed, done when the screen comes back (watchScreens)
        watchScreens()
    }

    // MARK: someone at the Mac

    /// Input from Linux being posted now (it can count as activity, like a person's would).
    private static var lastInjected: TimeInterval = 0
    private static var watch: Timer?
    /// Someone used the Mac's own keyboard or trackpad: dimming ends (the settings say so).
    static var onLocalUse: (() -> Void)?

    /// Marks an event Sweeflow posts (for Linux), so the watch below tells it from the Mac's own:
    /// posted at the HID level, macOS reports it as if it came from the hardware.
    static let injectedMark: Int64 = 0x5357_4545_464C   // "SWEEFL"
    static func noteInjected(_ e: CGEvent) {
        e.setIntegerValueField(.eventSourceUserData, value: injectedMark)
        lastInjected = ProcessInfo.processInfo.systemUptime
    }

    /// Watches the Mac's own input while dimmed. An event tap sees every event with where it came
    /// from: the Mac's keyboard, trackpad or mouse (no posting process) vs Sweeflow's, posted for
    /// Linux. A pointer moved on the Mac itself ends dimming; one moved over a Mac window on
    /// Linux doesn't.
    private static var tap: CFMachPort?
    private static var tapSource: CFRunLoopSource?
    /// The tap is served on a thread of its own. It's an active tap, so every event (the ones
    /// Sweeflow posts for Linux included) waits until its callback has run; on the main thread
    /// that meant every click and pointer move waited for whatever the main thread was busy with.
    private static var tapRunLoop: CFRunLoop?

    private static func watchLocalInput() {
        guard tap == nil else { return }
        let types: [CGEventType] = [.mouseMoved, .leftMouseDown, .rightMouseDown, .otherMouseDown, .leftMouseDragged,
                                    .scrollWheel, .keyDown, .flagsChanged]
        let mask = types.reduce(CGEventMask(0)) { $0 | (CGEventMask(1) << $1.rawValue) }
        // An active tap (it passes everything through): allowed by Accessibility, which Sweeflow
        // has. A listen-only one would make macOS ask for Input Monitoring as well.
        guard let t = CGEvent.tapCreate(tap: .cgSessionEventTap, place: .tailAppendEventTap, options: .defaultTap,
                                        eventsOfInterest: mask, callback: { _, type, event, _ in
            if type == .tapDisabledByTimeout || type == .tapDisabledByUserInput {
                if let t = BuiltInScreen.tap { CGEvent.tapEnable(tap: t, enable: true) }
            } else if event.getIntegerValueField(.eventSourceUserData) != BuiltInScreen.injectedMark,
                      event.getIntegerValueField(.eventSourceUnixProcessID) == 0 {
                let pointer = type != .keyDown && type != .flagsChanged
                DispatchQueue.main.async { BuiltInScreen.localInput(pointer: pointer) }
            }
            return Unmanaged.passUnretained(event)
        }, userInfo: nil) else {
            return log("can't watch this Mac's own input (Accessibility): idle time instead")
        }
        tap = t
        let source = CFMachPortCreateRunLoopSource(nil, t, 0)
        tapSource = source
        let ready = DispatchSemaphore(value: 0)
        let thread = Thread {
            tapRunLoop = CFRunLoopGetCurrent()
            CFRunLoopAddSource(CFRunLoopGetCurrent(), source, .commonModes)
            CGEvent.tapEnable(tap: t, enable: true)
            ready.signal()
            CFRunLoopRun()   // until stopWatchingLocalInput stops it
        }
        thread.name = "sweeflow.eventtap"
        thread.qualityOfService = .userInteractive
        thread.start()
        // So stopWatchingLocalInput always finds the run loop to stop.
        _ = ready.wait(timeout: .now() + 2)
    }

    private static func stopWatchingLocalInput() {
        let (t, s, rl) = (tap, tapSource, tapRunLoop)
        // Forgotten first: the tap's callback turns `tap` back on when macOS says it went off.
        tap = nil
        tapSource = nil
        tapRunLoop = nil
        if let t {
            CGEvent.tapEnable(tap: t, enable: false)
            // Out of the window server for good: left there and on, with no thread serving it, it
            // held the next key or click until macOS gave up on it, and that input was lost (the
            // first keys typed after each reconnect, one lot per earlier connection).
            CFMachPortInvalidate(t)
        }
        if let rl {
            if let s { CFRunLoopRemoveSource(rl, s, .commonModes) }
            CFRunLoopStop(rl)
        }
    }

    private static func localInput(pointer: Bool) {
        if pointer { bringPointerBack() }
        guard dimming, !paused else { return }
        log("someone's using the Mac: its screen back on")
        onLocalUse?()
    }

    // MARK: the pointer, for someone at the Mac

    /// Watched while Linux is connected (and while dimmed): see `bringPointerBack`.
    private static var connected = false

    static func setConnected(_ on: Bool) {
        connected = on
        if on { watchLocalInput() } else if !dimming { stopWatchingLocalInput() }
    }

    private static var lastWarp: TimeInterval = 0

    /// While Linux is connected the pointer lives on the virtual display (where Linux's clicks
    /// land), which nobody at the Mac can see. Moved with the Mac's own trackpad or mouse, it
    /// comes back to a screen they see.
    private static func bringPointerBack() {
        let now = ProcessInfo.processInfo.systemUptime
        guard now - lastWarp > 0.5, let vid = host.display.displayID else { return }
        let at = CGEvent(source: nil)?.location ?? .zero
        guard CGDisplayBounds(vid).contains(at) else { return }
        let visible = NSScreen.screens.compactMap {
            $0.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? CGDirectDisplayID
        }.filter { $0 != vid }
        guard let target = builtIn.flatMap({ id in visible.contains(id) ? id : nil }) ?? visible.first else { return }
        let b = CGDisplayBounds(target)
        lastWarp = now
        CGWarpMouseCursorPosition(CGPoint(x: b.midX, y: b.midY))
        CGAssociateMouseAndMouseCursorPosition(1)
        log("pointer moved on the Mac: brought back to its own screen")
    }

    /// Once a second while dimmed: anything that brightens the screen (macOS does, on some input
    /// from Linux) is turned down again. Without the event tap, the Mac's idle time stands in.
    private static func watchUse() {
        watchLocalInput()
        guard watch == nil else { return }
        watch = Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { _ in
            guard dimming, !paused else { return }
            let now = ProcessInfo.processInfo.systemUptime
            if tap == nil {
                let idle = CGEventSource.secondsSinceLastEventType(.hidSystemState, eventType: CGEventType(rawValue: ~0)!)
                // Recent input that Sweeflow didn't post: the Mac's own keyboard or trackpad.
                if idle < 1.1 && now - lastInjected > 1.5 { return localInput(pointer: false) }
            }
            guard let id = builtIn, let get = getBrightness else { return }
            var current: Float = 0
            if get(id, &current) == 0, current > dimLevel + 0.02 { dim() }
        }
    }

    private static func dim() {
        guard let id = builtIn, let get = getBrightness, let set = setBrightness else { return }
        var current: Float = 0
        guard get(id, &current) == 0 else { return }
        // The brightness to come back to. Only a real one: read while macOS is already bringing a
        // dimmed screen back up, it would be a dull in-between, and restoring that looks stuck dim.
        if UserDefaults.standard.object(forKey: savedKey) == nil {
            UserDefaults.standard.set(current >= minRestore ? current : max(lastLit, minRestore), forKey: savedKey)
        }
        // Not 0: that switches the backlight off, which macOS counts as the display turning off
        // (and the Mac may then lock within seconds).
        // (Logged when it was really lit: macOS nudges it up a little now and then.)
        if set(id, dimLevel) == 0, current > 0.3 { log(String(format: "built-in screen dimmed (was %.2f)", current)) }
    }

    /// True once nothing is left to restore.
    @discardableResult
    static func restore() -> Bool {
        guard let saved = UserDefaults.standard.object(forKey: savedKey) as? Float else { return true }
        // With the lid closed the screen is offline: the caller tries again later.
        guard let id = builtIn, let set = setBrightness, set(id, max(saved, minRestore)) == 0 else { return false }
        lastLit = max(saved, minRestore)
        UserDefaults.standard.removeObject(forKey: savedKey)
        log(String(format: "built-in screen brightness restored to %.2f", saved))
        return true
    }
}
