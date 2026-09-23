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

let server = Server(identity: identity) { group, queue in Session(group: group, queue: queue, config: config) }
do { try server.start(port: config.port) } catch { log("listen failed: \(error)"); exit(1) }

app.run()
