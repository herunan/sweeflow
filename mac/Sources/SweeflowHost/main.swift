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
    /// The built-in screen is dimmed now: asked for ("Dim Mac screen", from either side) and
    /// switched off again as soon as someone uses the Mac itself (see BuiltInScreen).
    var dimMacScreenWhileStreaming = false

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

// `SweeflowHost --pair`: ask the running Sweeflow to show how to link a Linux machine, then quit.
if CommandLine.arguments.contains("--pair") {
    DistributedNotificationCenter.default().postNotificationName(.init("dev.sweeflow.host.pair"), object: nil,
                                                                 userInfo: nil, deliverImmediately: true)
    exit(0)
}

// `SweeflowHost --render-setup FILE.png`: draws the setup window into a picture, then quits.
if let i = CommandLine.arguments.firstIndex(of: "--render-setup"), i + 1 < CommandLine.arguments.count {
    Onboarding.render(to: CommandLine.arguments[i + 1], allDone: CommandLine.arguments.dropFirst(i + 2).first == "done")
}

// `SweeflowHost --render-link FILE.png`: draws the Link window into a picture, then quits.
if let i = CommandLine.arguments.firstIndex(of: "--render-link"), i + 1 < CommandLine.arguments.count {
    PairingWindow.render(to: CommandLine.arguments[i + 1], code: CommandLine.arguments.count > i + 2 ? CommandLine.arguments[i + 2] : nil)
}

// `SweeflowHost --hold-display`: keeps a stand-in display up while Sweeflow restarts (an update),
// then quits once the new Sweeflow's own display is up (a minute at most). With the lid closed
// and no monitor, the virtual display is the Mac's only one: it went with the old process, and
// the Mac fell asleep and locked before the new one was up.
if CommandLine.arguments.contains("--hold-display") {
    VirtualDisplay.holdDuringRestart()
}

// Started by the bundled login item (or opened by hand), stderr goes nowhere: keep a log where
// the dev setup does. Before the hand-off below, so what a copy opened by hand does is in it too.
if ProcessInfo.processInfo.environment["XPC_SERVICE_NAME"] != LoginItem.devAgentLabel {
    let logURL = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Library/Logs/sweeflow.log")
    if !FileManager.default.fileExists(atPath: logURL.path) { FileManager.default.createFile(atPath: logURL.path, contents: nil) }
    if let h = try? FileHandle(forWritingTo: logURL) {
        h.seekToEndOfFile()
        dup2(h.fileDescriptor, STDERR_FILENO)
    }
}

// Which copy runs. The one the person opens (Finder, Launchpad, macOS's Quit & Reopen) runs
// itself; the login item only starts Sweeflow at login. That item is often refused by macOS
// (exit 78, a launch constraint: no Developer ID yet), and handing over to it cost every opening
// ~13 s, or left nothing running when the old copy was still quitting.
let otherCopy = { NSRunningApplication.runningApplications(withBundleIdentifier: Bundle.main.bundleIdentifier ?? "")
    .first { $0.processIdentifier != getpid() && !$0.isTerminated } }
if !LoginItem.underLaunchd {
    let me = Bundle.main.executableURL.flatMap { try? $0.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate }
    if let other = otherCopy() {
        // Started before the app on disk was replaced: an older version. It makes way.
        if let me, let started = other.launchDate, started < me {
            log("an older Sweeflow is running: replacing it")
            other.terminate()
        } else {
            // A copy that answers is the one: it shows its window, in front (this copy, opened by
            // the person, may bring Sweeflow forward, and hands that over), and this one goes.
            var answered = false
            let here = DistributedNotificationCenter.default().addObserver(forName: .init("dev.sweeflow.host.here"), object: nil, queue: nil) { _ in answered = true }
            let until = Date().addingTimeInterval(3)
            repeat {
                NSApplication.shared.yieldActivation(toApplicationWithBundleIdentifier: Bundle.main.bundleIdentifier ?? "")
                other.activate(from: NSRunningApplication.current, options: [])
                DistributedNotificationCenter.default().postNotificationName(.init("dev.sweeflow.host.show"), object: nil,
                                                                             userInfo: nil, deliverImmediately: true)
                RunLoop.current.run(until: Date().addingTimeInterval(0.3))
            } while !answered && !other.isTerminated && Date() < until
            DistributedNotificationCenter.default().removeObserver(here)
            if answered && !other.isTerminated { exit(0) }
            // No answer: it's quitting (Quit & Reopen opens this copy meanwhile) or stuck.
            log("the other copy didn't answer: taking over")
        }
        let until = Date().addingTimeInterval(8)
        while !other.isTerminated && Date() < until { RunLoop.current.run(until: Date().addingTimeInterval(0.25)) }
        if !other.isTerminated { other.forceTerminate(); RunLoop.current.run(until: Date().addingTimeInterval(1)) }
    }
    // For the next login.
    DispatchQueue.main.async { LoginItem.check(); Updater.shared.start() }
}

let settings = Settings(Config.load())

// Permissions are asked for in the setup window, one after another (not macOS's prompts all at
// once at launch).
if !CGPreflightScreenCaptureAccess() { log("Screen Recording permission not granted yet") }
if !AXIsProcessTrusted() { log("Accessibility permission not granted yet — input and window control will not work") }

let identity: Identity
do { identity = try Identity.loadOrCreate() } catch { log("identity: \(error)"); exit(1) }
log("host fingerprint \(identity.fingerprint)")

/// Owns the virtual display and sleep prevention across sessions (main thread only). Both live
/// while a client is connected, plus a grace period so a Wi-Fi blip doesn't reshuffle windows.
final class HostState {
    let display = VirtualDisplay()
    let wake = Wakefulness()
    private var sessions = 0 {
        didSet { if (sessions > 0) != (oldValue > 0) { NotificationCenter.default.post(name: .sweeflowConnectionChanged, object: nil) } }
    }
    var connected: Bool { sessions > 0 }
    /// The Linux machine connected now (its certificate's fingerprint). Main thread.
    var client: String? {
        didSet { if client != oldValue { NotificationCenter.default.post(name: .sweeflowConnectionChanged, object: nil) } }
    }
    /// What that computer calls itself (said in its hello; empty from older ones). Main thread.
    var clientSaysName = ""
    /// The version of Sweeflow the connected Linux machine has ("" for an older one that doesn't say).
    var clientAppVersion = ""
    private var teardown: DispatchWorkItem?

    private var mutedByUs = false

    init() {
        // Hidden by a run that ended without putting it back (a crash, a forced quit).
        DockHider.restore()
        NotificationCenter.default.addObserver(forName: .sweeflowSettingsChanged, object: nil, queue: .main) { [weak self] _ in
            self?.apply()
        }
        // Someone at the Mac: dimming is over (until asked for again).
        BuiltInScreen.onLocalUse = {
            if settings.config.dimMacScreenWhileStreaming { settings.update { $0.dimMacScreenWhileStreaming = false } }
        }
    }

    /// Dimming lasts one connection at most: a new one (or the last one ending) never finds the
    /// screen left dark.
    private func undimForGood() {
        if settings.config.dimMacScreenWhileStreaming { settings.update { $0.dimMacScreenWhileStreaming = false } }
    }

    func sessionStarted() {
        sessions += 1
        teardown?.cancel()
        BuiltInScreen.setConnected(true)
        undimForGood()
        apply()
    }

    /// `leaving`: the client said goodbye (switched to another Mac, disconnected, quit). Tidied
    /// up at once then: the grace period is for connections that drop (a Wi-Fi blip).
    func sessionEnded(leaving: Bool = false) {
        sessions = max(0, sessions - 1)
        guard sessions == 0 else { return }
        BuiltInScreen.setConnected(false)
        undimForGood()
        // Brightness comes back quickly (you may be back at the Mac); the rest after a grace period.
        DispatchQueue.main.asyncAfter(deadline: .now() + 3) { [weak self] in
            guard let self, self.sessions == 0, self.dimmed else { return }
            self.dimmed = false
            BuiltInScreen.stopDimming()
        }
        let work = DispatchWorkItem { [weak self] in
            guard let self, self.sessions == 0 else { return }
            log("no client: removing virtual display, allowing sleep")
            self.display.remove()
            DockHider.restore()
            self.wake.release()
            self.wake.releaseReachable()
            self.setMuted(false)
        }
        teardown = work
        if leaving {
            if dimmed { dimmed = false; BuiltInScreen.stopDimming() }
            log("client left: tidying up now")
            work.perform()
            // Not asleep straight away, though: a computer restarting Sweeflow (an update, a
            // restart, Stop then Start) says goodbye too, and a Mac idle for a while slept the
            // moment it was let go, before that computer was back.
            wake.holdReachable()
            DispatchQueue.main.asyncAfter(deadline: .now() + 60) { [weak self] in
                guard let self, self.sessions == 0 else { return }
                self.wake.releaseReachable()
            }
        } else {
            DispatchQueue.main.asyncAfter(deadline: .now() + 20, execute: work)
        }
    }

    /// A client is connected, or the settings changed: keep the Mac awake, dimmed and muted as
    /// they say. Switched off, the Mac is left entirely alone (the sessions drop the windows).
    func apply() {
        guard sessions > 0 else { return }
        let c = settings.config
        // The Dock follows streaming, not the connection: Stop (here or on Linux) keeps the
        // connection, and the Dock stayed hidden until the computer went away.
        if c.enabled {
            wake.releaseReachable()
            wake.hold()
            DockHider.hide()
        } else {
            wake.release()
            wake.holdReachable()
            display.remove()
            DockHider.restore()
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
ScrollAcceleration.reload()
BuiltInScreen.restore()
// Started afresh (after a crash, an update): never dimmed from the start.
if settings.config.dimMacScreenWhileStreaming { settings.update { $0.dimMacScreenWhileStreaming = false } }

let server = Server(identity: identity) { group, queue in Session(group: group, queue: queue) }
do { try server.start(port: settings.config.port) } catch { log("listen failed: \(error)"); exit(1) }
// Link requests are taken for as long as Sweeflow runs (nothing has to be opened here first).
Pairing.shared.start()

let debugPort = DebugPort()
if settings.config.debugPort != 0 { debugPort.start(port: settings.config.debugPort) }
let statusMenu = StatusMenu()

// Something still to set up (a fresh install): the setup window says what. Already linked (an
// update, a restart): macOS can be slow to report permissions right after a new copy starts, so
// look again before asking.
if Identity.pairedClients().isEmpty {
    Onboarding.shared.show()
} else if !Onboarding.complete {
    DispatchQueue.main.asyncAfter(deadline: .now() + 5) { if !Onboarding.complete { Onboarding.shared.show() } }
}
// Shows how to link from elsewhere on this Mac (`SweeflowHost --pair`, or a script).
DistributedNotificationCenter.default().addObserver(forName: .init("dev.sweeflow.host.pair"), object: nil, queue: .main) { _ in
    PairingWindow.shared.show()
}
// Sweeflow opened again by hand while running (see above), or by an update: the setup window, if
// anything is left to set up. Otherwise the menu bar item is where Sweeflow lives.
DistributedNotificationCenter.default().addObserver(forName: .init("dev.sweeflow.host.show"), object: nil, queue: .main) { _ in
    // Answered, so the copy that asked knows this one is alive and goes.
    DistributedNotificationCenter.default().postNotificationName(.init("dev.sweeflow.host.here"), object: nil,
                                                                 userInfo: nil, deliverImmediately: true)
    if !Onboarding.complete { Onboarding.shared.show() }
}

// Quit (the menu, an update) or ended by macOS or launchd (logout, `launchctl bootout`, kill): the
// Dock as the person had it. It used to come back only at the next launch, which never came when
// Sweeflow was removed.
NotificationCenter.default.addObserver(forName: NSApplication.willTerminateNotification, object: nil, queue: .main) { _ in
    DockHider.restore()
}
var endSignals: [DispatchSourceSignal] = []
for sig in [SIGTERM, SIGINT, SIGHUP] {
    signal(sig, SIG_IGN)
    let source = DispatchSource.makeSignalSource(signal: sig, queue: .main)
    source.setEventHandler {
        log("ending (signal \(sig))")
        DockHider.restore()
        exit(0)
    }
    source.resume()
    endSignals.append(source)
}

/// Opened again while running (Finder, Launchpad, Spotlight): macOS doesn't start another copy
/// then, it tells this one. The setup window again while there's something left in it; a newer
/// Sweeflow installed over this one meanwhile, that one takes over (it was left running the old).
final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows: Bool) -> Bool {
        let onDisk = Bundle.main.executableURL.flatMap { try? $0.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate }
        if let onDisk, let started = NSRunningApplication.current.launchDate, started < onDisk {
            log("opened again, and a newer Sweeflow is installed: handing over to it")
            let cfg = NSWorkspace.OpenConfiguration()
            cfg.createsNewApplicationInstance = true
            NSWorkspace.shared.openApplication(at: Bundle.main.bundleURL, configuration: cfg) { _, error in
                if let error { log("opening the newer Sweeflow: \(error)") }
            }
            return false
        }
        if !Onboarding.complete { Onboarding.shared.show() }
        return false
    }
}
let appDelegate = AppDelegate()
app.delegate = appDelegate

app.run()
