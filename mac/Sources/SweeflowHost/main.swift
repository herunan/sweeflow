import AppKit
import ScreenCaptureKit

/// User configuration: ~/Library/Application Support/sweeflow/config.json
struct Config: Codable {
    var port: UInt16 = 44120
    /// Bundle identifiers whose windows are streamed. Empty = every regular app (anything with
    /// a Dock icon), which is the default.
    var apps: [String] = []
    /// Bundle identifiers never streamed (applies on top of `apps`).
    var excludeApps: [String] = []
    /// Mute the Mac's own speakers while a client is connected (sound plays on Linux instead).
    var muteMacWhileStreaming = true
    var dimMacScreenWhileStreaming = true

    /// Put streamed windows on an invisible display sized to the Linux monitor (lets windows be
    /// as large as that monitor and keeps working with the lid closed). Turn off to keep windows
    /// on the Mac's own screen while also using the Mac directly.
    var virtualDisplay = true
    /// Localhost debug hook port (development only); 0 = off.
    var debugPort: UInt16 = 0

    // Switches in the menu-bar and tray menus (see Settings).
    /// Master switch: off = no windows streamed, no virtual display, Mac left alone.
    var enabled = true
    /// Mac sound plays on Linux (with the Mac's speakers muted, per muteMacWhileStreaming).
    var soundOnLinux = true
    var clipboard = true
    var dragDrop = true
    /// The Mac Apps folder in the Linux app menu.
    var launcherMenu = true
    /// Delta video frames as QUIC datagrams (off: everything on the windows' streams).
    var videoDatagrams = true
    /// Mac windows on a Linux desktop of their own.
    var ownDesktop = true
    /// "sharp": Retina, 10-bit. "light": 1x, 8-bit, for slower networks.
    var quality = "sharp"
    /// Behind Mac windows on the virtual display: "auto" (as the Mac's own appearance), "dark"
    /// or "light". Their see-through sidebars and toolbars take on this tone.
    var backdrop = "auto"

    init() {}

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let d = Config()
        port = try c.decodeIfPresent(UInt16.self, forKey: .port) ?? d.port
        apps = try c.decodeIfPresent([String].self, forKey: .apps) ?? d.apps
        virtualDisplay = try c.decodeIfPresent(Bool.self, forKey: .virtualDisplay) ?? d.virtualDisplay
        debugPort = try c.decodeIfPresent(UInt16.self, forKey: .debugPort) ?? 0
        excludeApps = try c.decodeIfPresent([String].self, forKey: .excludeApps) ?? []
        muteMacWhileStreaming = try c.decodeIfPresent(Bool.self, forKey: .muteMacWhileStreaming) ?? d.muteMacWhileStreaming
        dimMacScreenWhileStreaming = try c.decodeIfPresent(Bool.self, forKey: .dimMacScreenWhileStreaming) ?? d.dimMacScreenWhileStreaming
        enabled = try c.decodeIfPresent(Bool.self, forKey: .enabled) ?? d.enabled
        soundOnLinux = try c.decodeIfPresent(Bool.self, forKey: .soundOnLinux) ?? d.soundOnLinux
        clipboard = try c.decodeIfPresent(Bool.self, forKey: .clipboard) ?? d.clipboard
        dragDrop = try c.decodeIfPresent(Bool.self, forKey: .dragDrop) ?? d.dragDrop
        launcherMenu = try c.decodeIfPresent(Bool.self, forKey: .launcherMenu) ?? d.launcherMenu
        quality = try c.decodeIfPresent(String.self, forKey: .quality) ?? d.quality
        ownDesktop = try c.decodeIfPresent(Bool.self, forKey: .ownDesktop) ?? d.ownDesktop
        backdrop = try c.decodeIfPresent(String.self, forKey: .backdrop) ?? d.backdrop
        videoDatagrams = try c.decodeIfPresent(Bool.self, forKey: .videoDatagrams) ?? d.videoDatagrams
    }

    func allows(_ bundleId: String) -> Bool {
        if bundleId == Bundle.main.bundleIdentifier || excludeApps.contains(bundleId) { return false }
        if !apps.isEmpty { return apps.contains(bundleId) }
        return Self.isRegularApp(bundleId)
    }

    /// Regular apps only: menu-bar utilities and background agents have no real windows.
    /// Asking LaunchServices is a round trip to launchservicesd, and the window list is checked
    /// up to every 8 ms while you work: remember the answer for a few seconds.
    private static var regular: [String: (Bool, Date)] = [:]
    private static let regularLock = NSLock()

    static func isRegularApp(_ bundleId: String) -> Bool {
        let now = Date()
        if let (yes, at) = regularLock.withLock({ regular[bundleId] }), now.timeIntervalSince(at) < 10 { return yes }
        let yes = NSRunningApplication.runningApplications(withBundleIdentifier: bundleId).contains { $0.activationPolicy == .regular }
        regularLock.withLock { regular[bundleId] = (yes, now) }
        return yes
    }

    private static let url = Identity.dir.appendingPathComponent("config.json")

    static func load() -> Config {
        let c = (try? Data(contentsOf: url)).flatMap { try? JSONDecoder().decode(Config.self, from: $0) } ?? Config()
        c.save()   // writes the defaults, and any new settings, out for editing
        return c
    }

    func save() {
        let enc = JSONEncoder()
        enc.outputFormatting = [.prettyPrinted, .sortedKeys]
        try? FileManager.default.createDirectory(at: Identity.dir, withIntermediateDirectories: true)
        try? enc.encode(self).write(to: Self.url)
    }
}

setvbuf(stderr, nil, _IOLBF, 0)
let app = NSApplication.shared
app.setActivationPolicy(.accessory)

// `SweeflowHost --pair`: ask the running Sweeflow to open pairing, then quit.
if CommandLine.arguments.contains("--pair") {
    DistributedNotificationCenter.default().postNotificationName(.init("dev.sweeflow.host.pair"), object: nil,
                                                                 userInfo: nil, deliverImmediately: true)
    exit(0)
}

// Opened by hand (Finder, Launchpad): the copy macOS keeps running is the one that serves Linux.
// Make sure that one exists (the login item), ask it to show its window, and step aside. If
// macOS wants the login item approved first, carry on here; the setup window explains.
if !LoginItem.underLaunchd {
    if LoginItem.devInstall || LoginItem.service.status == .enabled || LoginItem.enable() {
        DistributedNotificationCenter.default().postNotificationName(.init("dev.sweeflow.host.show"), object: nil,
                                                                     userInfo: nil, deliverImmediately: true)
        exit(0)
    }
}

// Started by the bundled login item, stderr goes nowhere: keep a log where the dev setup does.
if ProcessInfo.processInfo.environment["XPC_SERVICE_NAME"] != LoginItem.devAgentLabel {
    let logURL = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Library/Logs/sweeflow.log")
    if !FileManager.default.fileExists(atPath: logURL.path) { FileManager.default.createFile(atPath: logURL.path, contents: nil) }
    if let h = try? FileHandle(forWritingTo: logURL) {
        h.seekToEndOfFile()
        dup2(h.fileDescriptor, STDERR_FILENO)
    }
}

let settings = Settings(Config.load())

if !CGPreflightScreenCaptureAccess() {
    log("requesting Screen Recording permission")
    CGRequestScreenCaptureAccess()
}
let axTrusted = AXIsProcessTrustedWithOptions([kAXTrustedCheckOptionPrompt.takeUnretainedValue(): true] as CFDictionary)
if !axTrusted { log("Accessibility permission not granted yet — input and window control will not work") }

let identity: Identity
do { identity = try Identity.loadOrCreate() } catch { log("identity: \(error)"); exit(1) }
log("host fingerprint \(identity.fingerprint)")

/// Owns the virtual display and sleep prevention across sessions (main thread only). Both live
/// while a client is connected, plus a grace period so a Wi-Fi blip doesn't reshuffle windows.
final class HostState {
    let display = VirtualDisplay()
    let wake = Wakefulness()
    private var sessions = 0
    var connected: Bool { sessions > 0 }
    private var teardown: DispatchWorkItem?

    private var mutedByUs = false

    init() {
        NotificationCenter.default.addObserver(forName: .sweeflowSettingsChanged, object: nil, queue: .main) { [weak self] _ in
            self?.apply()
        }
    }

    func sessionStarted() {
        sessions += 1
        teardown?.cancel()
        apply()
    }

    func sessionEnded() {
        sessions = max(0, sessions - 1)
        guard sessions == 0 else { return }
        // Brightness comes back quickly (you may be back at the Mac); the rest after a grace period.
        DispatchQueue.main.asyncAfter(deadline: .now() + 3) { [weak self] in
            guard let self, self.sessions == 0, self.dimmed else { return }
            self.dimmed = false
            BuiltInScreen.stopDimming()
        }
        let work = DispatchWorkItem { [weak self] in
            guard let self, self.sessions == 0 else { return }
            log("no client for a while: removing virtual display, allowing sleep")
            self.display.remove()
            self.wake.release()
            self.wake.releaseReachable()
            self.setMuted(false)
        }
        teardown = work
        DispatchQueue.main.asyncAfter(deadline: .now() + 20, execute: work)
    }

    /// A client is connected, or the settings changed: keep the Mac awake, dimmed and muted as
    /// they say. Switched off, the Mac is left entirely alone (the sessions drop the windows).
    func apply() {
        guard sessions > 0 else { return }
        let c = settings.config
        if c.enabled {
            wake.releaseReachable()
            wake.hold()
        } else {
            wake.release()
            wake.holdReachable()
            display.remove()
        }
        let dim = c.enabled && c.dimMacScreenWhileStreaming
        if dim != dimmed {
            dimmed = dim
            if dim { BuiltInScreen.startDimming() } else { BuiltInScreen.stopDimming() }
        }
        setMuted(c.enabled && c.soundOnLinux && c.muteMacWhileStreaming)
    }

    private var muting = false
    private var dimmed = false

    private func setMuted(_ on: Bool) {
        if on && !muting && !mutedByUs {
            muting = true
            // Remember the user's own setting so it is restored exactly.
            DispatchQueue.global().async {
                let was = (try? run("/usr/bin/osascript", ["-e", "output muted of (get volume settings)"]))?
                    .trimmingCharacters(in: .whitespacesAndNewlines) == "true"
                if !was { _ = try? run("/usr/bin/osascript", ["-e", "set volume output muted true"]) }
                DispatchQueue.main.async {
                    self.muting = false
                    if !was { self.mutedByUs = true; log("muted the Mac's speakers while streaming") }
                }
            }
        } else if !on && mutedByUs {
            mutedByUs = false
            DispatchQueue.global().async { _ = try? run("/usr/bin/osascript", ["-e", "set volume output muted false"]) }
            log("unmuted the Mac's speakers")
        }
    }
}

let host = HostState()
Wakefulness.resetLidSleep()   // in case a previous run died while streaming
BuiltInScreen.restore()

let server = Server(identity: identity) { group, queue in Session(group: group, queue: queue) }
do { try server.start(port: settings.config.port) } catch { log("listen failed: \(error)"); exit(1) }

let debugPort = DebugPort()
if settings.config.debugPort != 0 { debugPort.start(port: settings.config.debugPort) }
let statusMenu = StatusMenu()

// Something still to set up (a fresh install): the setup window says what.
if !Onboarding.complete { Onboarding.shared.show() }
// Opens pairing from elsewhere on this Mac (`SweeflowHost --pair`, or a script).
DistributedNotificationCenter.default().addObserver(forName: .init("dev.sweeflow.host.pair"), object: nil, queue: .main) { _ in
    PairingWindow.shared.show()
}
// Sweeflow opened again by hand while running (see above): show the setup window.
DistributedNotificationCenter.default().addObserver(forName: .init("dev.sweeflow.host.show"), object: nil, queue: .main) { _ in
    Onboarding.shared.show()
}

app.run()
