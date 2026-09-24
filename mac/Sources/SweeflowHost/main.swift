import AppKit
import ScreenCaptureKit

/// User configuration: ~/Library/Application Support/sweeflow/config.json
struct Config: Codable {
    var port: UInt16 = 44120
    /// Bundle identifiers whose windows are streamed. Empty = every regular app.
    var apps: [String] = [
        "com.example.chat",
        "com.example.videocall",
        "com.example.assistant",
        "com.apple.Terminal",
        "com.example.browser",
    ]

    /// Put streamed windows on an invisible display sized to the Linux monitor (lets windows be
    /// as large as that monitor and keeps working with the lid closed). Turn off to keep windows
    /// on the Mac's own screen while also using the Mac directly.
    var virtualDisplay = true
    /// Localhost debug hook port (development only); 0 = off.
    var debugPort: UInt16 = 0

    init() {}

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let d = Config()
        port = try c.decodeIfPresent(UInt16.self, forKey: .port) ?? d.port
        apps = try c.decodeIfPresent([String].self, forKey: .apps) ?? d.apps
        virtualDisplay = try c.decodeIfPresent(Bool.self, forKey: .virtualDisplay) ?? d.virtualDisplay
        debugPort = try c.decodeIfPresent(UInt16.self, forKey: .debugPort) ?? 0
    }

    func allows(_ bundleId: String) -> Bool {
        if bundleId == Bundle.main.bundleIdentifier { return false }
        return apps.isEmpty || apps.contains(bundleId)
    }

    static func load() -> Config {
        let url = Identity.dir.appendingPathComponent("config.json")
        if let d = try? Data(contentsOf: url), let c = try? JSONDecoder().decode(Config.self, from: d) { return c }
        let c = Config()
        let enc = JSONEncoder()
        enc.outputFormatting = [.prettyPrinted, .sortedKeys]
        try? FileManager.default.createDirectory(at: Identity.dir, withIntermediateDirectories: true)
        try? enc.encode(c).write(to: url)
        return c
    }
}

setvbuf(stderr, nil, _IOLBF, 0)
let app = NSApplication.shared
app.setActivationPolicy(.accessory)

let config = Config.load()

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
    private var teardown: DispatchWorkItem?

    func sessionStarted() {
        sessions += 1
        teardown?.cancel()
        wake.hold()
    }

    func sessionEnded() {
        sessions = max(0, sessions - 1)
        guard sessions == 0 else { return }
        let work = DispatchWorkItem { [weak self] in
            guard let self, self.sessions == 0 else { return }
            log("no client for a while: removing virtual display, allowing sleep")
            self.display.remove()
            self.wake.release()
        }
        teardown = work
        DispatchQueue.main.asyncAfter(deadline: .now() + 20, execute: work)
    }
}

let host = HostState()
Wakefulness.resetLidSleep()   // in case a previous run died while streaming

let server = Server(identity: identity) { group, queue in Session(group: group, queue: queue, config: config) }
do { try server.start(port: config.port) } catch { log("listen failed: \(error)"); exit(1) }

let debugPort = DebugPort()
if config.debugPort != 0 { debugPort.start(port: config.debugPort) }

app.run()
