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
    /// "sharp": Retina, 10-bit. "light": 1x, 8-bit, for slower networks.
    var quality = "sharp"

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
    }

    func allows(_ bundleId: String) -> Bool {
        if bundleId == Bundle.main.bundleIdentifier || excludeApps.contains(bundleId) { return false }
        if !apps.isEmpty { return apps.contains(bundleId) }
        // Regular apps only: menu-bar utilities and background agents have no real windows.
        return NSRunningApplication.runningApplications(withBundleIdentifier: bundleId)
            .contains { $0.activationPolicy == .regular }
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
        if c.enabled { wake.hold() } else { wake.release(); display.remove() }
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

app.run()
