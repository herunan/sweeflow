import AppKit
import Network
import ScreenCaptureKit
import CoreImage

/// Development hook: a localhost-only line protocol for driving the host over SSH
/// (`echo "rclick 4053 300 200" | nc 127.0.0.1 PORT`). Off unless `debugPort` is set in config.
///
///   shot PATH                 PNG of the virtual display (or main display)
///   winshot WID PATH          PNG of one window via a display filter (tests the capture badge)
///   move WID X Y              pointer move, window-relative points
///   click WID X Y [right]     press + release
///   windows                   list tracked windows
final class DebugPort {
    private var listener: NWListener?
    static weak var session: Session?

    func start(port: UInt16) {
        let params = NWParameters.tcp
        params.requiredLocalEndpoint = NWEndpoint.hostPort(host: "127.0.0.1", port: NWEndpoint.Port(rawValue: port)!)
        guard let l = try? NWListener(using: params) else { log("debug port unavailable"); return }
        l.newConnectionHandler = { conn in
            conn.start(queue: .main)
            conn.receive(minimumIncompleteLength: 1, maximumLength: 4096) { data, _, _, _ in
                let line = String(decoding: data ?? Data(), as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines)
                DebugPort.handle(line) { reply in
                    conn.send(content: Data((reply + "\n").utf8), completion: .contentProcessed { _ in conn.cancel() })
                }
            }
        }
        l.start(queue: .main)
        listener = l
        log("debug port listening on 127.0.0.1:\(port)")
    }

    private static func handle(_ line: String, reply: @escaping (String) -> Void) {
        let p = line.split(separator: " ").map(String.init)
        guard let cmd = p.first else { return reply("empty") }
        switch cmd {
        case "shot" where p.count == 2:
            let id = host.display.displayID ?? CGMainDisplayID()
            SCShareableContent.getExcludingDesktopWindows(false, onScreenWindowsOnly: true) { content, _ in
                guard let d = content?.displays.first(where: { $0.displayID == id }) else { return reply("no display") }
                let cfg = SCStreamConfiguration()
                cfg.width = d.width * 2; cfg.height = d.height * 2
                capture(SCContentFilter(display: d, excludingWindows: []), cfg, to: p[1], reply: reply)
            }
        case "winshot" where p.count == 3:
            let wid = UInt32(p[1]) ?? 0
            SCShareableContent.getExcludingDesktopWindows(false, onScreenWindowsOnly: true) { content, _ in
                guard let w = content?.windows.first(where: { $0.windowID == wid }),
                      let d = content?.displays.first(where: { $0.frame.intersects(w.frame) }) else { return reply("no window") }
                let cfg = SCStreamConfiguration()
                cfg.sourceRect = w.frame.offsetBy(dx: -d.frame.minX, dy: -d.frame.minY)
                cfg.width = Int(w.frame.width * 2); cfg.height = Int(w.frame.height * 2)
                cfg.showsCursor = false
                capture(SCContentFilter(display: d, including: [w]), cfg, to: p[2], reply: reply)
            }
        case "axapp" where p.count == 2:
            // An app's windows as the Accessibility API sees them (texts and buttons).
            guard let pid = pid_t(p[1]) else { return reply("usage: axapp pid") }
            DispatchQueue.global().async { reply(BlindWindows.describe(pid: pid).map { $0.debugDescription }.joined(separator: "\n")) }
        case "axpress" where p.count >= 3:
            // Press a button (by its label) in one of an app's windows: axpress pid Label words
            guard let pid = pid_t(p[1]) else { return reply("usage: axpress pid label") }
            let label = p[2...].joined(separator: " ")
            DispatchQueue.global().async {
                for c in BlindWindows.describe(pid: pid) {
                    if let b = c.buttons.first(where: { $0.label == label }) {
                        return reply(AXUIElementPerformAction(b.element, kAXPressAction as CFString) == .success ? "pressed" : "failed")
                    }
                }
                reply("no such button")
            }
        case "blindtest":
            // A pretend stand-in (id 1): shows the Linux dialog; its answer is only logged.
            guard let s = session else { return reply("no session") }
            s.debugBlindTest()
            reply("sent")
        case "move", "click":
            guard p.count >= 4, let wid = UInt32(p[1]), let x = Double(p[2]), let y = Double(p[3]),
                  let s = session else { return reply("usage / no session") }
            s.queue.async {
                let r = s.debugPointer(wid: wid, x: x, y: y, click: cmd == "click", right: p.count > 4 && p[4] == "right")
                DispatchQueue.main.async { reply(r) }
            }
        case "gmove", "gdown", "gup":
            // Global pointer control (points, CG coordinates) for windows we don't stream.
            guard p.count == 3, let x = Double(p[1]), let y = Double(p[2]) else { return reply("usage") }
            let pt = CGPoint(x: x, y: y)
            let src = CGEventSource(stateID: .hidSystemState)
            let type: CGEventType = cmd == "gmove" ? .mouseMoved : cmd == "gdown" ? .leftMouseDown : .leftMouseUp
            CGEvent(mouseEventSource: src, mouseType: type, mouseCursorPosition: pt, mouseButton: .left)?.post(tap: .cghidEventTap)
            reply("ok")
        case "mainshot" where p.count == 2:
            SCShareableContent.getExcludingDesktopWindows(false, onScreenWindowsOnly: true) { content, _ in
                guard let d = content?.displays.first(where: { $0.displayID == CGMainDisplayID() }) else { return reply("no display") }
                let cfg = SCStreamConfiguration()
                cfg.width = d.width * 2; cfg.height = d.height * 2
                cfg.showsCursor = false
                capture(SCContentFilter(display: d, excludingWindows: []), cfg, to: p[1], reply: reply)
            }
        case "finder":
            // Frontmost Finder window frame, for locating real traffic lights.
            let l = CGWindowListCopyWindowInfo([.optionOnScreenOnly], kCGNullWindowID) as? [[String: Any]] ?? []
            let f = l.first { ($0[kCGWindowOwnerName as String] as? String) == "Finder" && ($0[kCGWindowLayer as String] as? Int) == 0 }
            reply(f.map { "\($0[kCGWindowNumber as String]!) \($0[kCGWindowBounds as String]!)" } ?? "none")
        case "ax" where p.count == 2:
            guard let s = session, let wid = UInt32(p[1]) else { return reply("no session") }
            s.queue.async {
                let r = s.debugAX(wid)
                DispatchQueue.main.async { reply(r) }
            }
        case "streamtest" where p.count == 3:
            // Streams one window through a display filter for 2 s and saves the last frame:
            // does macOS badge windows captured this way?
            let wid = UInt32(p[1]) ?? 0
            SCShareableContent.getExcludingDesktopWindows(false, onScreenWindowsOnly: true) { content, _ in
                guard let w = content?.windows.first(where: { $0.windowID == wid }),
                      let d = content?.displays.first(where: { $0.frame.intersects(w.frame) }) else { return reply("no window") }
                let cfg = SCStreamConfiguration()
                cfg.sourceRect = w.frame.offsetBy(dx: -d.frame.minX, dy: -d.frame.minY)
                cfg.width = Int(w.frame.width * 2); cfg.height = Int(w.frame.height * 2)
                cfg.showsCursor = false
                let sink = FrameSink(path: p[2])
                let stream = SCStream(filter: SCContentFilter(display: d, including: [w]), configuration: cfg, delegate: nil)
                try? stream.addStreamOutput(sink, type: .screen, sampleHandlerQueue: .main)
                stream.startCapture { err in
                    if let err { return reply("start failed \(err)") }
                    DispatchQueue.main.asyncAfter(deadline: .now() + 2) {
                        sink.save()
                        stream.stopCapture { _ in reply("ok (frames \(sink.count))") }
                        _ = sink
                    }
                }
            }
        case "scroll" where p.count == 3:
            guard let s = session, let x = Double(p[1]), let y = Double(p[2]) else { return reply("usage: scroll x y") }
            s.debugScroll(x: x, y: y, reply: reply)
        case "pause" where p.count == 2:
            // Preview the locked look: pause on|off.
            session?.debugPause(p[1] != "off", placeholder: p[1] == "placeholder")
            reply("ok")
        case "close" where p.count == 2:
            guard let s = session, let wid = UInt32(p[1]) else { return reply("no session") }
            s.queue.async {
                let r = s.debugClose(wid)
                DispatchQueue.main.async { reply(r) }
            }
        case "blind":
            // Uncapturable windows as the tracker has them, and what the window list says now.
            guard let s = session else { return reply("no session") }
            s.queue.async {
                let r = s.debugBlind()
                DispatchQueue.main.async { reply(r) }
            }
        case "windows":
            guard let s = session else { return reply("no session") }
            s.queue.async {
                let r = s.debugWindows()
                DispatchQueue.main.async { reply(r) }
            }
        default:
            reply("unknown command")
        }
    }

    private static func capture(_ filter: SCContentFilter, _ cfg: SCStreamConfiguration, to path: String,
                                reply: @escaping (String) -> Void) {
        SCScreenshotManager.captureImage(contentFilter: filter, configuration: cfg) { image, err in
            guard let image else { return reply("capture failed: \(String(describing: err))") }
            let rep = NSBitmapImageRep(cgImage: image)
            do {
                try rep.representation(using: .png, properties: [:])?.write(to: URL(fileURLWithPath: path))
                reply("ok \(image.width)x\(image.height)")
            } catch { reply("write failed: \(error)") }
        }
    }
}

/// Keeps the newest frame of a test stream.
final class FrameSink: NSObject, SCStreamOutput {
    let path: String
    var last: CVPixelBuffer?
    var count = 0
    init(path: String) { self.path = path }
    func stream(_ s: SCStream, didOutputSampleBuffer sb: CMSampleBuffer, of type: SCStreamOutputType) {
        if let pb = CMSampleBufferGetImageBuffer(sb) { last = pb; count += 1 }
    }
    func save() {
        guard let pb = last else { return }
        let ci = CIImage(cvPixelBuffer: pb)
        let rep = NSBitmapImageRep(ciImage: ci)
        try? rep.representation(using: .png, properties: [:])?.write(to: URL(fileURLWithPath: path))
    }
}
