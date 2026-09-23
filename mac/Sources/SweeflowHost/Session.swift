import AppKit
import Network

/// State for one connected client: the control stream, the video streams and the window
/// tracker feeding them. Everything runs on the server's serial queue.
final class Session {
    private let group: NWConnectionGroup
    private let queue: DispatchQueue
    private let config: Config
    private var control: MsgStream?
    private var clientScale: CGFloat = 1
    /// Pixels per point to capture at; 0 = the window's native (Retina) backing scale.
    private var captureScale: CGFloat = 0
    private let tracker: WindowTracker
    private let control_ = WindowControl()
    private let input = InputInjector()
    private var streamers: [ObjectIdentifier: WindowStreamer] = [:]
    private var videoStreams: [ObjectIdentifier: MsgStream] = [:]
    private var sentIcons: Set<String> = []
    private var raisedWid: UInt32 = 0
    private var lastFocused: UInt32 = 0
    private var focusTimer: DispatchSourceTimer?

    init(group: NWConnectionGroup, queue: DispatchQueue, config: Config) {
        self.group = group
        self.queue = queue
        self.config = config
        tracker = WindowTracker(queue: queue, allowed: { config.allows($0) })
    }

    func addStream(_ conn: NWConnection) {
        let s = MsgStream(conn, queue: queue)
        let key = ObjectIdentifier(s)
        videoStreams[key] = s
        s.onMessage = { [weak self, weak s] m in
            guard let self, let s else { return }
            var m = m
            do { try self.handle(&m, on: s) } catch { log("bad message: \(error)") }
        }
        s.onClose = { [weak self] in
            guard let self else { return }
            self.videoStreams[key] = nil
            if let st = self.streamers.removeValue(forKey: key) { st.stop() }
            if self.control.map({ ObjectIdentifier($0) }) == key { self.shutdown() }
        }
        s.start()
    }

    func shutdown() {
        tracker.stop()
        focusTimer?.cancel()
        input.releaseAll()
        streamers.values.forEach { $0.stop() }
        streamers.removeAll()
        videoStreams.values.forEach { $0.close() }
        videoStreams.removeAll()
        control = nil
        group.cancel()
    }

    // MARK: messages

    private func handle(_ m: inout MsgReader, on s: MsgStream) throws {
        switch m.type {
        case .hello:
            let v = try m.u16()
            clientScale = try m.f32()
            captureScale = (try? m.f32()) ?? 0
            log("client hello v\(v) scale \(clientScale) capture scale \(captureScale)")
            control = s
            let key = ObjectIdentifier(s)
            if let st = streamers.removeValue(forKey: key) { st.stop() }
            var w = MsgWriter(.welcome)
            w.u16(protocolVersion)
            w.str(Host.current().localizedName ?? "Mac")
            s.send(w)
            startTracking()
        case .ping:
            var w = MsgWriter(.pong); w.u64(try m.u64()); s.send(w)

        case .subscribe:
            let wid = try m.u32()
            guard let win = tracker.windows[wid] else { log("subscribe to unknown window \(wid)"); s.close(); return }
            let scale = captureScale > 0 ? captureScale : backingScale(of: win.frame)
            let st = WindowStreamer(wid: wid, pointSize: win.frame.size, scale: scale, out: s, queue: queue)
            streamers[ObjectIdentifier(s)] = st
            st.start()
        case .requestKeyframe: streamers[ObjectIdentifier(s)]?.requestKeyframe()
        case .pause: streamers[ObjectIdentifier(s)]?.setPaused(try m.u8() != 0)
        case .frameAck: streamers[ObjectIdentifier(s)]?.ack(try m.u64())

        case .resize:
            let wid = try m.u32(), w = try m.f32(), h = try m.f32()
            guard let win = tracker.windows[wid] else { return }
            control_.setSize(wid, pid: win.pid, CGSize(width: w, height: h))
        case .focus:
            let wid = try m.u32()
            guard let win = tracker.windows[wid] else { return }
            raise(win)
            control_.focus(wid, pid: win.pid)
        case .close:
            let wid = try m.u32()
            if let win = tracker.windows[wid] { control_.close(wid, pid: win.pid) }

        case .pointerMove:
            let (wid, p) = try pointerTarget(&m)
            if let p { ensureFront(wid, at: p); input.move(to: p) }
        case .pointerButton:
            let (wid, p) = try pointerTarget(&m)
            let b = Int(try m.u8()), pressed = try m.u8() != 0
            if let p {
                if pressed { ensureFront(wid, at: p) }
                input.button(b, pressed: pressed, at: p)
            }
        case .scroll:
            let (_, p) = try pointerTarget(&m)
            let dx = try m.f32(), dy = try m.f32(), unit = try m.u8(), phase = try m.u8()
            if let p { input.scroll(dx: dx, dy: dy, unit: unit, phase: phase, at: p) }
        case .key:
            let code = try m.u32(), v = try m.u8()   // 0 up, 1 down, 2 repeat
            input.key(evdev: code, pressed: v != 0, isRepeat: v == 2)
        case .releaseAll:
            input.releaseAll()

        default:
            log("unhandled message \(m.type)")
        }
    }

    /// Reads `u32 wid, f32 x, f32 y` and converts to a global Mac point.
    private func pointerTarget(_ m: inout MsgReader) throws -> (UInt32, CGPoint?) {
        let wid = try m.u32(), x = try m.f32(), y = try m.f32()
        guard let win = tracker.windows[wid] else { return (wid, nil) }
        return (wid, CGPoint(x: win.frame.minX + x, y: win.frame.minY + y))
    }

    /// Windows overlap on the Mac, so the one under the pointer must be on top before we
    /// inject events at its location.
    private func ensureFront(_ wid: UInt32, at p: CGPoint) {
        guard let win = tracker.windows[wid] else { return }
        if win.kind == .popup { return }
        if raisedWid == wid && tracker.isFrontmost(wid, at: p) { return }
        if !tracker.isFrontmost(wid, at: p) { raise(win) }
        raisedWid = wid
    }

    private func raise(_ win: TrackedWindow) {
        control_.focus(win.wid, pid: win.pid)
        raisedWid = win.wid
    }

    // MARK: window list

    private func startTracking() {
        tracker.onAdded = { [weak self] w in self?.sendAdded(w) }
        tracker.onChanged = { [weak self] w in self?.sendChanged(w) }
        tracker.onRemoved = { [weak self] wid in
            guard let self else { return }
            self.control_.forget(wid)
            var m = MsgWriter(.windowRemoved); m.u32(wid); self.control?.send(m)
        }
        tracker.start()

        let t = DispatchSource.makeTimerSource(queue: queue)
        t.schedule(deadline: .now() + .milliseconds(250), repeating: .milliseconds(250))
        t.setEventHandler { [weak self] in self?.checkFocus() }
        t.resume()
        focusTimer = t
    }

    private func checkFocus() {
        guard let wid = control_.focusedWindowID(), wid != lastFocused, tracker.windows[wid] != nil else { return }
        lastFocused = wid
        var m = MsgWriter(.focused); m.u32(wid); control?.send(m)
    }

    private func sendAdded(_ w: TrackedWindow) {
        sendIcon(w)
        var m = MsgWriter(.windowAdded)
        m.u32(w.wid); m.i32(w.pid)
        m.str(w.bundleId); m.str(w.appName); m.str(w.title)
        m.f32(w.frame.width); m.f32(w.frame.height)
        m.u32(w.parent); m.f32(w.rel.x); m.f32(w.rel.y)
        m.u8(w.kind.rawValue)
        control?.send(m)
    }

    private func sendChanged(_ w: TrackedWindow) {
        for st in streamers.values where st.wid == w.wid { st.windowResized(to: w.frame.size) }
        var m = MsgWriter(.windowChanged)
        m.u32(w.wid); m.str(w.title)
        m.f32(w.frame.width); m.f32(w.frame.height)
        m.f32(w.rel.x); m.f32(w.rel.y)
        control?.send(m)
    }

    private func sendIcon(_ w: TrackedWindow) {
        guard !sentIcons.contains(w.bundleId),
              let app = NSRunningApplication(processIdentifier: w.pid), let icon = app.icon else { return }
        sentIcons.insert(w.bundleId)
        let size = NSSize(width: 256, height: 256)
        let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: 256, pixelsHigh: 256, bitsPerSample: 8,
                                   samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
                                   bytesPerRow: 0, bitsPerPixel: 0)!
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
        icon.draw(in: NSRect(origin: .zero, size: size))
        NSGraphicsContext.restoreGraphicsState()
        guard let png = rep.representation(using: .png, properties: [:]) else { return }
        var m = MsgWriter(.appIcon)
        m.str(w.bundleId); m.bytes(png)
        control?.send(m)
    }
}

/// Backing scale (2 on Retina) of the screen showing most of `frame` (CG, top-left origin).
func backingScale(of frame: CGRect) -> CGFloat {
    guard let primary = NSScreen.screens.first else { return 2 }
    var best: (CGFloat, CGFloat) = (0, primary.backingScaleFactor)
    for screen in NSScreen.screens {
        // NSScreen frames are bottom-left origin; flip into CG coordinates.
        var f = screen.frame
        f.origin.y = primary.frame.maxY - f.maxY
        let i = f.intersection(frame)
        let area = i.isNull ? 0 : i.width * i.height
        if area > best.0 { best = (area, screen.backingScaleFactor) }
    }
    return best.1
}
