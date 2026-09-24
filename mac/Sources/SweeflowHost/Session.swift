import AppKit
import Network

/// State for one connected client: the control stream, the video streams and the window
/// tracker feeding them. Everything runs on the server's serial queue.
final class Session {
    private let group: NWConnectionGroup
    let queue: DispatchQueue
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
    private var cursor: CursorWatcher?
    /// Watches a window right after a click in it, to catch the app starting a window drag
    /// (title bar) or zoom (double-click), which we hand over to the Linux window manager.
    private var gesture: (wid: UInt32, frame: CGRect, until: DispatchTime, buttonDown: Bool)?
    private var gestureTimer: DispatchSourceTimer?
    private var sentButtons: [UInt32: [CGRect]] = [:]
    /// Usable area of the virtual display (global CG coords), once it is online.
    private var vdArea: CGRect?
    private var started = false
    /// The window with keyboard focus on the Linux side (0 = none of ours).
    private var clientFocus: UInt32 = 0

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
        if started {
            started = false
            DispatchQueue.main.async { host.sessionEnded() }
        }
        tracker.stop()
        focusTimer?.cancel()
        cursor?.stop()
        gestureTimer?.cancel()
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
            DebugPort.session = self
            if !started {
                started = true
                DispatchQueue.main.async { host.sessionStarted() }
            }
            let key = ObjectIdentifier(s)
            if let st = streamers.removeValue(forKey: key) { st.stop() }
            var w = MsgWriter(.welcome)
            w.u16(protocolVersion)
            w.str(Host.current().localizedName ?? "Mac")
            s.send(w)
            startTracking()
            let cw = CursorWatcher(scale: clientScale) { [weak self] png, hot, scale in
                self?.queue.async {
                    var m = MsgWriter(.cursor)
                    m.f32(hot.x); m.f32(hot.y); m.f32(scale); m.bytes(png)
                    self?.control?.send(m)
                }
            }
            cw.start()
            cursor = cw
        case .ping:
            var w = MsgWriter(.pong); w.u64(try m.u64()); s.send(w)

        case .subscribe:
            let wid = try m.u32()
            guard let win = tracker.windows[wid] else { log("subscribe to unknown window \(wid)"); s.close(); return }
            let scale = captureScale > 0 ? captureScale : backingScale(of: win.frame)
            let st = WindowStreamer(wid: wid, pointSize: win.frame.size, scale: scale, lossless: win.kind == .popup,
                                    out: s, queue: queue)
            st.buttons = sentButtons[wid] ?? []
            st.badgeRect = { [weak tracker] in tracker?.badgeRect(wid) }
            st.active = wid == clientFocus
            streamers[ObjectIdentifier(s)] = st
            st.start()
        case .requestKeyframe: streamers[ObjectIdentifier(s)]?.requestKeyframe()
        case .pause: streamers[ObjectIdentifier(s)]?.setPaused(try m.u8() != 0)
        case .frameAck: streamers[ObjectIdentifier(s)]?.ack(try m.u64())

        case .resize:
            let wid = try m.u32(), w = try m.f32(), h = try m.f32()
            guard let win = tracker.windows[wid] else { return }
            control_.setSize(wid, pid: win.pid, CGSize(width: w, height: h))
        case .screenSize:
            let w = CGFloat(try m.u16()), h = CGFloat(try m.u16())
            guard config.virtualDisplay else { return }
            log("client screen \(Int(w))x\(Int(h))")
            DispatchQueue.main.async { host.display.ensure(points: CGSize(width: w, height: h)) }
            refreshVirtualDisplayArea(attempt: 0)
        case .focus:
            let wid = try m.u32()
            setClientFocus(wid)
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
                if b == 0 { watchGesture(wid, buttonDown: pressed) }
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
            setClientFocus(0)

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

    private func watchGesture(_ wid: UInt32, buttonDown: Bool) {
        guard let win = tracker.windows[wid], win.kind == .toplevel else { return }
        if buttonDown || gesture?.wid != wid {
            gesture = (wid, tracker.liveFrame(wid) ?? win.frame, .now() + .milliseconds(buttonDown ? 10_000 : 400), buttonDown)
        } else {
            gesture?.buttonDown = false
            gesture?.until = .now() + .milliseconds(400)   // a zoom lands shortly after the release
        }
        if gestureTimer == nil {
            let t = DispatchSource.makeTimerSource(queue: queue)
            t.schedule(deadline: .now(), repeating: .milliseconds(8), leeway: .milliseconds(2))
            t.setEventHandler { [weak self] in self?.checkGesture() }
            t.resume()
            gestureTimer = t
        }
    }

    private func checkGesture() {
        guard let g = gesture, let win = tracker.windows[g.wid], DispatchTime.now() < g.until,
              let now = tracker.liveFrame(g.wid) else {
            gesture = nil; gestureTimer?.cancel(); gestureTimer = nil
            return
        }
        if now == g.frame { return }
        gesture = nil; gestureTimer?.cancel(); gestureTimer = nil
        if now.size == g.frame.size && g.buttonDown {
            // The app started dragging its window: end the drag on the Mac, put the window back,
            // and let KWin move the Linux window instead (the button is still held there).
            input.button(0, pressed: false, at: input.lastPoint)
            control_.setPosition(g.wid, pid: win.pid, g.frame.origin)
            log("window \(g.wid): title-bar drag → handing move to Linux")
            var m = MsgWriter(.moveStarted); m.u32(g.wid); control?.send(m)
        } else if now.size != g.frame.size && !g.buttonDown {
            // Double-click zoom: undo it on the Mac and maximise the Linux window instead.
            control_.setPosition(g.wid, pid: win.pid, g.frame.origin)
            control_.setSize(g.wid, pid: win.pid, g.frame.size)
            log("window \(g.wid): zoom → toggling maximise on Linux")
            var m = MsgWriter(.toggleMaximize); m.u32(g.wid); control?.send(m)
        }
    }

    /// The new display shows up in NSScreen asynchronously; poll briefly for its usable area.
    private func refreshVirtualDisplayArea(attempt: Int) {
        DispatchQueue.main.asyncAfter(deadline: .now() + (attempt == 0 ? 0.3 : 0.5)) { [weak self] in
            let area = host.display.usableBounds
            self?.queue.async {
                guard let self else { return }
                if let area, area.width > 0 {
                    self.vdArea = area
                    log("virtual display area \(area)")
                    for w in self.tracker.windows.values { self.placeOnVirtualDisplay(w) }
                } else if attempt < 10 {
                    self.refreshVirtualDisplayArea(attempt: attempt + 1)
                }
            }
        }
    }

    /// Moves a streamed window onto the virtual display (top-left, below its menu bar).
    private func placeOnVirtualDisplay(_ w: TrackedWindow) {
        guard w.kind == .toplevel, let area = vdArea else { return }
        if area.contains(CGPoint(x: w.frame.minX + 1, y: w.frame.minY + 1)) { return }
        control_.setPosition(w.wid, pid: w.pid, area.origin)
    }

    // MARK: debug hooks (DebugPort)

    func debugPointer(wid: UInt32, x: Double, y: Double, click: Bool, right: Bool) -> String {
        guard let win = tracker.windows[wid] else { return "no window \(wid)" }
        let p = CGPoint(x: win.frame.minX + x, y: win.frame.minY + y)
        ensureFront(wid, at: p)
        input.move(to: p)
        if click {
            input.button(right ? 1 : 0, pressed: true, at: p)
            input.button(right ? 1 : 0, pressed: false, at: p)
        }
        return "ok \(p)"
    }

    func debugWindows() -> String {
        tracker.windows.values.sorted { $0.wid < $1.wid }.map {
            "\($0.wid) \($0.appName) kind=\($0.kind) parent=\($0.parent) layer=\($0.layer) frame=\($0.frame) rel=\($0.rel)"
        }.joined(separator: "\n")
    }

    private func setClientFocus(_ wid: UInt32) {
        guard wid != clientFocus else { return }
        clientFocus = wid
        for st in streamers.values {
            let active = st.wid == wid
            if st.active != active { st.active = active; st.requestRepaint() }
        }
    }

    private func sendButtons(_ w: TrackedWindow) {
        guard w.kind == .toplevel else { return }
        // Apps that hide the native buttons (e.g. a browser) report them outside the window.
        let bounds = CGRect(origin: .zero, size: w.frame.size)
        let rects = control_.buttonRects(w.wid, pid: w.pid).map { bounds.contains($0) ? $0 : .zero }
        // A transient failure (window mid-move) must not wipe known-good positions.
        if rects.allSatisfy({ $0 == .zero }), sentButtons[w.wid]?.contains(where: { $0 != .zero }) == true {
            return
        }
        guard sentButtons[w.wid] != rects else { return }
        sentButtons[w.wid] = rects
        for st in streamers.values where st.wid == w.wid { st.buttons = rects }
        log("window \(w.wid) (\(w.appName)) buttons \(rects.map { "\(Int($0.minX)),\(Int($0.minY)) \(Int($0.width))x\(Int($0.height))" })")
        var m = MsgWriter(.windowButtons)
        m.u32(w.wid)
        for r in rects { m.f32(r.minX); m.f32(r.minY); m.f32(r.width); m.f32(r.height) }
        control?.send(m)
    }

    private func raise(_ win: TrackedWindow) {
        control_.focus(win.wid, pid: win.pid)
        raisedWid = win.wid
    }

    // MARK: window list

    private func startTracking() {
        tracker.onAdded = { [weak self] w in self?.placeOnVirtualDisplay(w); self?.sendAdded(w) }
        tracker.onChanged = { [weak self] w in self?.sendChanged(w) }
        tracker.onRemoved = { [weak self] w in
            guard let self else { return }
            let wid = w.wid
            // Minimised on the Mac (⌘M, or an app's own button): a minimised window can't be
            // captured, so restore it there and minimise the Linux window instead.
            if w.kind == .toplevel, self.control_.isMinimized(wid, pid: w.pid) {
                log("window \(wid) minimised on the Mac → minimising on Linux instead")
                self.control_.unminimize(wid, pid: w.pid)
                var m = MsgWriter(.minimize); m.u32(wid); self.control?.send(m)
                return
            }
            self.control_.forget(wid)
            self.sentButtons[wid] = nil
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
        sendButtons(w)
    }

    private func sendChanged(_ w: TrackedWindow) {
        for st in streamers.values where st.wid == w.wid { st.windowResized(to: w.frame.size) }
        var m = MsgWriter(.windowChanged)
        m.u32(w.wid); m.str(w.title)
        m.f32(w.frame.width); m.f32(w.frame.height)
        m.f32(w.rel.x); m.f32(w.rel.y)
        control?.send(m)
        sendButtons(w)
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
