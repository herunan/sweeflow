import Foundation
import ScreenCaptureKit

/// macOS's capture service (replayd) can get stuck: on 2026-11-13, after a day of captures
/// started and stopped hundreds of times (two computers taking the Mac in turns, each one
/// rebuilding the virtual display), it sat at 100% CPU and no new capture delivered a single
/// frame, while stills and input kept working: every window looked frozen on Linux. Restarting
/// it (it runs as the user, and launchd starts it again on demand) fixed it at once.
/// So when a window's capture still gives nothing after its restarts, a capture of the whole
/// display is tried for a second (one always delivers a first frame). If that gives nothing
/// too, the service is stuck: it's restarted, and every window's capture started afresh.
enum CaptureHealth {
    /// Starts every window's capture afresh (set by the session).
    static var restartAll: (() -> Void)?
    private static let lock = NSLock()
    private static var probing = false
    private static var lastReset = Date.distantPast

    /// A window's capture gave no frame even after restarting it.
    static func captureGaveNothing() {
        lock.lock()
        defer { lock.unlock() }
        guard !probing, Date().timeIntervalSince(lastReset) > 120 else { return }
        probing = true
        probe { frames in
            lock.lock()
            probing = false
            let stuck = frames == 0
            if stuck { lastReset = Date() }
            lock.unlock()
            guard stuck else { return log("capture check: \(frames) frames from the display, the capture service is fine") }
            log("capture check: no frame from the display either: restarting the capture service (replayd)")
            let p = Process()
            p.executableURL = URL(fileURLWithPath: "/usr/bin/killall")
            p.arguments = ["replayd"]
            try? p.run()
            p.waitUntilExit()
            DispatchQueue.main.asyncAfter(deadline: .now() + 2) { restartAll?() }
        }
    }

    private final class Counter: NSObject, SCStreamOutput {
        var frames = 0
        func stream(_ stream: SCStream, didOutputSampleBuffer sb: CMSampleBuffer, of type: SCStreamOutputType) {
            if type == .screen { frames += 1 }
        }
    }

    /// Captures the virtual display (or the main one) small, for a second: how many frames came.
    private static func probe(_ done: @escaping (Int) -> Void) {
        let id = host.display.displayID ?? CGMainDisplayID()
        SCShareableContent.getExcludingDesktopWindows(false, onScreenWindowsOnly: true) { content, _ in
            guard let d = content?.displays.first(where: { $0.displayID == id }) ?? content?.displays.first else {
                return done(-1)
            }
            let cfg = SCStreamConfiguration()
            cfg.width = 320
            cfg.height = max(2, 320 * d.height / max(d.width, 1))
            cfg.minimumFrameInterval = CMTime(value: 1, timescale: 30)
            cfg.showsCursor = false
            let counter = Counter()
            let q = DispatchQueue(label: "sweeflow.capturecheck")
            let s = SCStream(filter: SCContentFilter(display: d, excludingWindows: []), configuration: cfg, delegate: nil)
            do { try s.addStreamOutput(counter, type: .screen, sampleHandlerQueue: q) } catch { return done(-1) }
            s.startCapture { err in
                if err != nil { return done(0) }
                q.asyncAfter(deadline: .now() + 1.2) {
                    let n = counter.frames
                    s.stopCapture { _ in done(n) }
                }
            }
        }
    }
}
