import AppKit
import Network

/// State for one connected client: the control stream, the video streams and the window
/// tracker feeding them. Everything runs on the server's serial queue.
final class Session {
    private let group: NWConnectionGroup
    let queue: DispatchQueue
    private var control: MsgStream?
    private var clientScale: CGFloat = 1
    /// Pixels per point to capture at; 0 = the window's native (Retina) backing scale.
    private var captureScale: CGFloat = 0
    private let tracker: WindowTracker
    private let control_ = WindowControl()
    private let input = InputInjector()
    private var streamers: [ObjectIdentifier: WindowStreamer] = [:]
    private var audio: (AudioCapture, ObjectIdentifier)?
    private var clipboard: (ClipboardSync, MsgStream)?
    /// Main-thread only.
    private let drag = DragBridge()
    private var dragTimer: Timer?
    private var videoStreams: [ObjectIdentifier: MsgStream] = [:]
    private var sentIcons: Set<String> = []
    private var raisedWid: UInt32 = 0
    private var lastFocused: UInt32 = 0
    private var screensObserver: NSObjectProtocol?
    private var cursor: CursorWatcher?
    /// Watches a window right after a click in it, to catch the app starting a window drag
    /// (title bar) or zoom (double-click), which we hand over to the Linux window manager.
    private var gesture: (wid: UInt32, frame: CGRect, until: DispatchTime, buttonDown: Bool)?
    private var gestureTimer: DispatchSourceTimer?
    private var sentButtons: [UInt32: [CGRect]] = [:]
    /// Buttons of the uncapturable windows shown as stand-ins on Linux.
    private var blindButtons: [UInt32: [AXUIElement]] = [:]
    /// Usable area of the virtual display (global CG coords), once it is online.
    private var vdArea: CGRect?
    private var started = false
    /// Sizes the client shows windows at (from its resize requests).
    private var requestedSizes: [UInt32: CGSize] = [:]
    /// Windows minimised on the Mac: kept (not reported removed) so the Linux window stays in the
    /// taskbar, minimised, until the user restores it there.
    private var minimized: [UInt32: TrackedWindow] = [:]
    private let menuQueue = DispatchQueue(label: "sweeflow.menus", qos: .userInitiated)
    /// The window with keyboard focus on the Linux side (0 = none of ours).
    private var clientFocus: UInt32 = 0

    /// The Linux screen's size (points), for (re)creating the virtual display.
    private var clientScreen: CGSize?
    /// The streams audio and the clipboard go out on, kept so they can be switched back on.
    private var audioStream: MsgStream?
    private var clipboardStream: MsgStream?
    private var settingsObserver: NSObjectProtocol?

    init(group: NWConnectionGroup, queue: DispatchQueue) {
        self.group = group
        self.queue = queue
        tracker = WindowTracker(queue: queue, allowed: { let c = settings.config; return c.enabled && c.allows($0) })
        settingsObserver = NotificationCenter.default.addObserver(forName: .sweeflowSettingsChanged, object: nil, queue: nil) {
            [weak self] _ in self?.queue.async { self?.settingsChanged() }
        }
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
            if let (a, k) = self.audio, k == key { a.stop(); self.audio = nil }
            if self.audioStream.map({ ObjectIdentifier($0) }) == key { self.audioStream = nil }
            if let (c, cs) = self.clipboard, ObjectIdentifier(cs) == key { c.stop(); self.clipboard = nil }
            if self.clipboardStream.map({ ObjectIdentifier($0) }) == key { self.clipboardStream = nil }
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
        if let o = screensObserver { NotificationCenter.default.removeObserver(o) }
        if let o = settingsObserver { NotificationCenter.default.removeObserver(o) }
        cursor?.stop()
        gestureTimer?.cancel()
        input.releaseAll()
        streamers.values.forEach { $0.stop() }
        streamers.removeAll()
        audio?.0.stop()
        audio = nil
        clipboard?.0.stop()
        clipboard = nil
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
            sendAppList()
            sendSettings()
            openDatagrams()
            if !watchingLock {
                watchingLock = true
                ScreenLock.onChange { [weak self] in self?.queue.async { self?.sendLockState() } }
            }
            sendLockState()
        case .ping:
            var w = MsgWriter(.pong); w.u64(try m.u64()); s.send(w)

        case .subscribe:
            let wid = try m.u32()
            guard let win = tracker.windows[wid] else { log("subscribe to unknown window \(wid)"); s.close(); return }
            log("stream \(wid): subscribed")
            // A new window is about to move onto the virtual display (see placeOnVirtualDisplay):
            // capture at its scale from the start, not at wherever it opened and then again.
            let vdScale = vdArea.flatMap { win.kind == .popup ? nil : displayScale(at: CGPoint(x: $0.midX, y: $0.midY)) }
            let scale = settings.config.quality == "light" ? 1 : captureScale > 0 ? captureScale : vdScale ?? backingScale(of: win.frame)
            let st = WindowStreamer(wid: wid, pointSize: win.frame.size, scale: scale, lossless: win.kind == .popup,
                                    out: s, queue: queue)
            st.buttons = sentButtons[wid] ?? []
            st.badgeRect = { [weak tracker] in tracker?.badgeRect(wid) }
            st.visibleSize = requestedSizes[wid]
            st.displayScale = { [weak tracker] in
                if settings.config.quality == "light" { return 1 }
                guard let f = tracker?.windows[wid]?.frame else { return nil }
                return displayScale(at: CGPoint(x: f.midX, y: f.minY + min(f.height / 2, 40)))
            }
            if let app = NSRunningApplication(processIdentifier: win.pid) {
                var r = CGRect(x: 0, y: 0, width: 256, height: 256)
                st.appIcon = app.icon?.cgImage(forProposedRect: &r, context: nil, hints: nil)
            }
            st.onDetectedButtons = { [weak self] rects in
                guard let self else { return }
                self.sentButtons[wid] = rects
                var m = MsgWriter(.windowButtons)
                m.u32(wid)
                for r in rects { m.f32(r.minX); m.f32(r.minY); m.f32(r.width); m.f32(r.height) }
                self.control?.send(m)
            }
            st.active = wid == clientFocus
            if let flow = datagrams {
                st.sendDatagram = { d in flow.send(content: d, completion: .idempotent) }
            }
            streamers[ObjectIdentifier(s)] = st
            st.start()
            queue.asyncAfter(deadline: .now() + 0.6) { st.checkScale() }
            if ScreenLock.isLocked { st.setScreenLocked(true) }
        case .clipboardSubscribe:
            clipboardStream = s
            updateClipboard()
        case .dragTaken:
            _ = try m.u32()
            let p = input.lastPoint
            DispatchQueue.main.async { self.dragTimer?.invalidate(); self.drag.cancelDrag(at: p) }
            input.forgetButtons()
        case .dragFetch:
            guard settings.config.dragDrop else { return }
            DispatchQueue.main.async { [weak self] in
                guard let self else { return }
                let urls = self.drag.draggedFiles
                DispatchQueue.global(qos: .userInitiated).async {
                    let items = urls.compactMap { url -> ClipItem? in
                        guard let data = try? Data(contentsOf: url, options: .mappedIfSafe) else { return nil }
                        return ClipItem(kind: .file, name: url.lastPathComponent, data: data)
                    }
                    self.queue.async {
                        var out = MsgWriter(.dragData)
                        out.u16(UInt16(items.count))
                        for it in items { out.str(it.kind.rawValue); out.str(it.name); out.bytes(it.data) }
                        s.send(out)
                        log("drag to Linux: \(items.map(\.name))")
                    }
                }
            }
        case .dragDropLocal:
            // A drag out of one Mac window dropped on another (via Linux): drop the same files.
            let wid = try m.u32(), x = try m.f32(), y = try m.f32()
            guard settings.config.dragDrop, let win = tracker.windows[wid] else { return }
            let point = CGPoint(x: win.frame.minX + x, y: win.frame.minY + y)
            if win.kind == .toplevel { raise(win) }
            DispatchQueue.main.async {
                let files = self.drag.draggedFiles
                log("drag between Mac windows: \(files.map(\.lastPathComponent)) → \(wid)")
                self.drag.drop(files.map { .file($0) }, at: point)
            }
        case .dropFiles:
            let wid = try m.u32(), x = try m.f32(), y = try m.f32()
            let n = Int(try m.u16())
            var items: [(kind: String, name: String, data: Data)] = []
            for _ in 0..<n { items.append((try m.str(), try m.str(), try m.bytes())) }
            guard settings.config.dragDrop, let win = tracker.windows[wid] else { return }
            let point = CGPoint(x: win.frame.minX + x, y: win.frame.minY + y)
            if win.kind == .toplevel { raise(win) }
            DispatchQueue.global(qos: .userInitiated).async { [weak self] in
                let dir = FileManager.default.homeDirectoryForCurrentUser
                    .appendingPathComponent("Library/Caches/sweeflow/drops/\(Int(Date().timeIntervalSince1970 * 1000))")
                var objects: [DragBridge.Item] = []
                for it in items {
                    switch it.kind {
                    case "file":
                        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
                        let name = (it.name as NSString).lastPathComponent
                        let url = dir.appendingPathComponent(name.isEmpty ? "file" : name)
                        if (try? it.data.write(to: url)) != nil { objects.append(.file(url)) }
                    case "url":
                        if let url = URL(string: String(decoding: it.data, as: UTF8.self)) { objects.append(.link(url)) }
                    default:
                        objects.append(.text(String(decoding: it.data, as: UTF8.self)))
                    }
                }
                log("drop from Linux: \(objects) at \(point)")
                DispatchQueue.main.async { self?.drag.drop(objects, at: point) }
            }
        case .clipboard:
            let n = Int(try m.u16())
            var items: [ClipItem] = []
            for _ in 0..<n {
                let kind = ClipItem.Kind(rawValue: try m.str())
                let name = try m.str(), data = try m.bytes()
                if let kind { items.append(ClipItem(kind: kind, name: name, data: data)) }
            }
            clipboard?.0.apply(items)
        case .audioSubscribe:
            audioStream = s
            updateAudio()
        case .requestKeyframe: streamers[ObjectIdentifier(s)]?.requestKeyframe()
        case .pause: streamers[ObjectIdentifier(s)]?.setPaused(try m.u8() != 0)
        case .frameAck: streamers[ObjectIdentifier(s)]?.ack(try m.u64())

        case .resize:
            let wid = try m.u32(), w = try m.f32(), h = try m.f32()
            guard let win = tracker.windows[wid] else { return }
            requestedSizes[wid] = CGSize(width: w, height: h)
            for st in streamers.values where st.wid == wid { st.visibleSize = CGSize(width: w, height: h) }
            resize(win, to: CGSize(width: w, height: h))
        case .launchApp:
            let bid = try m.str()
            log("launching \(bid)")
            // `open -b` also reopens a running app that has no windows (like clicking its Dock icon).
            DispatchQueue.global().async { _ = try? run("/usr/bin/open", ["-b", bid]) }
            // Its window can take a few seconds to appear (a cold launch): look out for it.
            tracker.burst(everyMs: 50, for: 5)
        case .restore:
            // Restored from the Linux taskbar: bring the Mac window back; the tracker then
            // re-announces it and the client resubscribes to its video.
            let wid = try m.u32()
            if let w = minimized.removeValue(forKey: wid) {
                ax(burstFor: 1) { $0.unminimize(wid, pid: w.pid); $0.focus(wid, pid: w.pid) }
            }
        case .menuRefresh:
            sendMenus(pid: pid_t(try m.i32()))
        case .menuActivate:
            let pid = pid_t(try m.i32())
            let n = Int(try m.u16())
            var path: [Int] = []
            for _ in 0..<n { path.append(Int(try m.u16())) }
            menuQueue.async { [weak self] in
                let ok = MenuReader.press(pid: pid, path: path)
                self?.tracker.burst(everyMs: 16, for: 1)
                log("menu \(path) in \(pid): \(ok ? "pressed" : "not found")")
                // Items change state after use (checkmarks, enabled): resend shortly after.
                DispatchQueue.global().asyncAfter(deadline: .now() + 0.4) { self?.sendMenus(pid: pid) }
            }
        case .setSettings:
            settings.apply(wire: try m.bytes())
        case .blindPress:
            let wid = try m.u32(), index = Int(try m.u16())
            if wid == 1 { log("stand-in test: button \(index) chosen") }
            if index == 0xFFFF, let menu = blindMenus[wid] {
                // Cancelled on Linux: close the menu on the Mac too.
                AXUIElementPerformAction(menu, kAXCancelAction as CFString)
            } else if let b = blindButtons[wid], index < b.count {
                let r = AXUIElementPerformAction(b[index], kAXPressAction as CFString)
                log("uncapturable window \(wid): pressed button \(index) (\(r == .success ? "ok" : "failed"))")
            }
        case .screenSize:
            let w = CGFloat(try m.u16()), h = CGFloat(try m.u16())
            clientScreen = CGSize(width: w, height: h)
            let c = settings.config
            guard c.virtualDisplay, c.enabled else { return }
            log("client screen \(Int(w))x\(Int(h))")
            DispatchQueue.main.async { host.display.ensure(points: CGSize(width: w, height: h)) }
            refreshVirtualDisplayArea(attempt: 0)
        case .focus:
            let wid = try m.u32()
            setClientFocus(wid)
            guard let win = tracker.windows[wid] else { return }
            raise(win)
            ax { $0.focus(wid, pid: win.pid) }
        case .close:
            let wid = try m.u32()
            // The window takes a moment to go after its close button is pressed: watch for it.
            if let win = tracker.windows[wid] { ax(burstFor: 2) { $0.close(wid, pid: win.pid) } }

        case .pointerMove:
            let (wid, p) = try pointerTarget(&m)
            // A drop from Linux is being replayed with the Mac's own pointer: don't pull it away.
            if let p, !DragBridge.replaying { ensureFront(wid, at: p); input.move(to: p) }
            userDidSomething(click: false)
        case .pointerButton:
            let (wid, p) = try pointerTarget(&m)
            let b = Int(try m.u8()), pressed = try m.u8() != 0
            if let p {
                if pressed { ensureFront(wid, at: p) }
                input.button(b, pressed: pressed, at: p)
                userDidSomething(click: true)
                if b == 0 {
                    watchGesture(wid, buttonDown: pressed)
                    watchDrag(wid, buttonDown: pressed)
                }
            }
        case .scroll:
            let (_, p) = try pointerTarget(&m)
            let dx = try m.f32(), dy = try m.f32(), unit = try m.u8(), phase = try m.u8()
            if let p { input.scroll(dx: dx, dy: dy, unit: unit, phase: phase, at: p) }
            userDidSomething(click: false)
        case .key:
            let code = try m.u32(), v = try m.u8()   // 0 up, 1 down, 2 repeat
            input.key(evdev: code, pressed: v != 0, isRepeat: v == 2)
            if v == 1 { userDidSomething(click: true) }
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
        if win.kind != .toplevel { return }   // menus and floating panels are already on top
        if raisedWid == wid && tracker.isFrontmost(wid, at: p) { return }
        if !tracker.isFrontmost(wid, at: p) { raise(win) }
        raisedWid = wid
    }

    /// While the left button is held in a streamed window, watch for a file drag starting there
    /// (reported to Linux, which takes it over when the pointer leaves the window).
    private func watchDrag(_ wid: UInt32, buttonDown: Bool) {
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            self.dragTimer?.invalidate()
            self.dragTimer = nil
            guard buttonDown, settings.config.dragDrop else { return }
            self.drag.armDragDetection()
            let t = Timer(timeInterval: 0.03, repeats: true) { [weak self] timer in
                guard let self, let urls = self.drag.pollDragStart() else { return }
                timer.invalidate()
                self.queue.async {
                    var m = MsgWriter(.dragBegan)
                    m.u32(wid)
                    m.u16(UInt16(urls.count))
                    for u in urls { m.str(u.lastPathComponent) }
                    self.control?.send(m)
                    log("drag began in \(wid): \(urls.map(\.lastPathComponent))")
                }
            }
            RunLoop.main.add(t, forMode: .common)
            self.dragTimer = t
        }
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
            ax { $0.setPosition(g.wid, pid: win.pid, g.frame.origin) }
            log("window \(g.wid): title-bar drag → handing move to Linux")
            var m = MsgWriter(.moveStarted); m.u32(g.wid); control?.send(m)
        } else if now.size != g.frame.size && !g.buttonDown {
            // Double-click zoom: undo it on the Mac and maximise the Linux window instead.
            ax { $0.setPosition(g.wid, pid: win.pid, g.frame.origin); $0.setSize(g.wid, pid: win.pid, g.frame.size) }
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
                    if area == self.vdArea { self.streamers.values.forEach { $0.checkScale() }; return }
                    self.vdArea = area
                    // Windows move onto the (new) virtual display: capture at its scale.
                    self.streamers.values.forEach { $0.checkScale() }
                    log("virtual display area \(area)")
                    for w in self.tracker.windows.values { self.placeOnVirtualDisplay(w) }
                    // Sizes asked for before the display was known may have been cut short.
                    for w in self.tracker.windows.values where w.kind == .toplevel {
                        if let want = self.requestedSizes[w.wid], abs(want.width - w.frame.width) > 2 || abs(want.height - w.frame.height) > 2 {
                            self.resize(w, to: want)
                        }
                    }
                } else if attempt < 10 {
                    self.refreshVirtualDisplayArea(attempt: attempt + 1)
                }
            }
        }
    }

    /// Gives a window the size it has on Linux. macOS won't let a window grow past the bottom or
    /// right of its screen, so one placed low (see placeOnVirtualDisplay) is moved up/left first.
    private func resize(_ win: TrackedWindow, to size: CGSize) {
        var origin = win.frame.origin
        if let area = vdArea, win.kind == .toplevel {
            origin.x = max(area.minX, min(origin.x, area.maxX - size.width))
            origin.y = max(area.minY, min(origin.y, area.maxY - size.height))
        }
        let moved = origin != win.frame.origin
        // Capture at the new size as soon as the app has resized (not on the next slow look).
        ax {
            if moved { $0.setPosition(win.wid, pid: win.pid, origin) }
            $0.setSize(win.wid, pid: win.pid, size)
        }
    }

    /// Keeps a streamed window (or floating panel) fully on the virtual display: input is injected
    /// at its real position on the Mac, so any part off-screen there can't be clicked.
    private func placeOnVirtualDisplay(_ w: TrackedWindow) {
        guard w.kind != .popup, let area = vdArea else { return }
        var origin = w.frame.origin
        if w.kind == .toplevel && !area.contains(CGPoint(x: w.frame.minX + 1, y: w.frame.minY + 1)) {
            origin = area.origin   // not on the display at all yet: top-left
        }
        // Clamp so the whole window is visible (as far as it fits).
        origin.x = min(max(origin.x, area.minX), max(area.minX, area.maxX - w.frame.width))
        origin.y = min(max(origin.y, area.minY), max(area.minY, area.maxY - w.frame.height))
        if w.kind == .toplevel { origin = uncovered(w, at: origin, in: area) }
        if origin != w.frame.origin { ax { $0.setPosition(w.wid, pid: w.pid, origin) } }
    }

    /// Nobody sees the virtual display, but macOS still works out which windows are hidden
    /// behind others, and apps (Chromium/Electron ones especially) all but stop drawing those:
    /// hovering one on Linux then lags while the Mac wakes it. So windows mostly under another
    /// move to where they overlap the rest least. An app's own overlay inside its window (a
    /// search panel) stays where it is.
    /// Where windows were just moved (the tracker sees it on its next look), so several moved
    /// at once don't all pick the same free spot.
    private var justMoved: [UInt32: (frame: CGRect, at: DispatchTime)] = [:]

    private func uncovered(_ w: TrackedWindow, at origin: CGPoint, in area: CGRect) -> CGPoint {
        let now = DispatchTime.now()
        justMoved = justMoved.filter { now.uptimeNanoseconds - $0.value.at.uptimeNanoseconds < 1_500_000_000 }
        let others = tracker.windows.values
            .filter { $0.wid != w.wid && $0.kind != .popup }
            .map { o -> TrackedWindow in var o = o; if let m = justMoved[o.wid] { o.frame = m.frame }; return o }
            .filter { area.intersects($0.frame) }
            // An app's own overlay inside its window (a search panel) doesn't count.
            .filter { o in !(o.pid == w.pid && o.frame.contains(w.frame) && o.frame.width * o.frame.height > w.frame.width * w.frame.height) }
            .map(\.frame)
        let best = Placement.uncovered(CGRect(origin: origin, size: w.frame.size), others: others, in: area)
        if best != origin {
            log("window \(w.wid) (\(w.appName)): moved out from under another window")
            justMoved[w.wid] = (CGRect(origin: best, size: w.frame.size), now)
        }
        return best
    }

    /// Accessibility actions wait on the app (Finder can take a second to close a window): run
    /// them off the session's queue so frames, input and window checks carry on meanwhile, then
    /// look for the change they make straight away.
    private let axQueue = DispatchQueue(label: "sweeflow.ax", qos: .userInteractive)

    private func ax(burstFor seconds: Double = 0.5, _ action: @escaping (WindowControl) -> Void) {
        let control = control_
        axQueue.async { [weak self] in
            let t0 = Date()
            action(control)
            let ms = Date().timeIntervalSince(t0) * 1000
            if ms > 100 { log(String(format: "accessibility action took %.0f ms", ms)) }
            self?.tracker.burst(everyMs: 16, for: seconds)
        }
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

    func debugClose(_ wid: UInt32) -> String {
        guard let w = tracker.windows[wid] else { return "no window \(wid)" }
        ax(burstFor: 2) { $0.close(wid, pid: w.pid) }
        return "ok"
    }

    func debugBlind() -> String {
        let tracked = tracker.blind.values.map { "tracked \($0.wid) \($0.owner) menu=\($0.menu) \($0.frame)" }
        let list = (CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]]) ?? []
        let raw = list.filter { ($0[kCGWindowSharingState as String] as? Int) == 0 || ($0[kCGWindowLayer as String] as? Int) == 101 }.map {
            "list \($0[kCGWindowNumber as String] ?? 0) \($0[kCGWindowOwnerName as String] ?? "") pid \($0[kCGWindowOwnerPID as String] ?? 0) layer \($0[kCGWindowLayer as String] ?? 0) sharing \($0[kCGWindowSharingState as String] ?? -1)"
        }
        return (tracked + raw).joined(separator: "\n")
    }

    func debugAX(_ wid: UInt32) -> String {
        guard let w = tracker.windows[wid] else { return "no window" }
        return "cg frame \(w.frame)\n" + control_.debugDescribe(wid, pid: w.pid)
    }

    /// A two-finger flick at global point (x, y), reporting the scroll position under it before,
    /// right after the fingers lift and once the glide is over.
    func debugScroll(x: Double, y: Double, reply: @escaping (String) -> Void) {
        let p = CGPoint(x: x, y: y)
        func position() -> String {
            var el: AXUIElement?
            guard AXUIElementCopyElementAtPosition(AXUIElementCreateSystemWide(), Float(x), Float(y), &el) == .success, var e = el else { return "?" }
            for _ in 0..<12 {
                var role: CFTypeRef?
                AXUIElementCopyAttributeValue(e, kAXRoleAttribute as CFString, &role)
                if (role as? String) == kAXScrollAreaRole {
                    var bar: CFTypeRef?
                    AXUIElementCopyAttributeValue(e, kAXVerticalScrollBarAttribute as CFString, &bar)
                    var v: CFTypeRef?
                    if let bar { AXUIElementCopyAttributeValue(bar as! AXUIElement, kAXValueAttribute as CFString, &v) }
                    return "\(v ?? "none" as CFTypeRef)"
                }
                var parent: CFTypeRef?
                guard AXUIElementCopyAttributeValue(e, kAXParentAttribute as CFString, &parent) == .success, let par = parent else { break }
                e = par as! AXUIElement
            }
            return "no scroll area"
        }
        let before = position()
        input.scroll(dx: 0, dy: 0, unit: 1, phase: 1, at: p)
        for i in 0..<8 {
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.011 * Double(i + 1)) { self.input.scroll(dx: 0, dy: -25, unit: 1, phase: 2, at: p) }
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.1) {
            self.input.scroll(dx: 0, dy: 0, unit: 1, phase: 3, at: p)
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.15) {
                let lifted = position()
                DispatchQueue.main.asyncAfter(deadline: .now() + 2.5) {
                    let after = position()
                    // Scroll back where it was.
                    reply("before \(before), at lift \(lifted), after glide \(after)")
                }
            }
        }
    }

    func debugPause(_ on: Bool, placeholder: Bool = false) {
        queue.async { self.streamers.values.forEach { $0.setScreenLocked(on, placeholder: placeholder) } }
    }

    func debugBlindTest() {
        queue.async {
            var m = MsgWriter(.blindShow)
            m.u32(1); m.str("Sweeflow"); m.str("Stand-in test")
            m.u16(1); m.str("This is how a Mac window that can't be captured (a security prompt) shows up here. Pick one — nothing happens on the Mac.")
            m.u16(2); m.str("Allow"); m.str("Deny")
            self.control?.send(m)
        }
    }

    func debugWindows() -> String {
        tracker.windows.values.sorted { $0.wid < $1.wid }.map {
            "\($0.wid) \($0.appName) kind=\($0.kind) parent=\($0.parent) layer=\($0.layer) frame=\($0.frame) rel=\($0.rel)"
        }.joined(separator: "\n")
    }

    /// Reads an app's menu bar (off the network queue: big menus take a while) and sends it.
    private func sendMenus(pid: pid_t) {
        menuQueue.async { [weak self] in
            guard let items = MenuReader.read(pid: pid),
                  let json = try? JSONEncoder().encode(items) else { return }
            self?.queue.async {
                var m = MsgWriter(.menuTree)
                m.i32(pid)
                m.bytes(json)
                self?.control?.send(m)
            }
        }
    }

    private func setClientFocus(_ wid: UInt32) {
        guard wid != clientFocus else { return }
        clientFocus = wid
        if let w = tracker.windows[wid] { sendMenus(pid: w.pid) }
        for st in streamers.values {
            let active = st.wid == wid
            if st.active != active { st.active = active; st.requestRepaint() }
        }
    }

    private func sendButtons(_ w: TrackedWindow) {
        guard w.kind == .toplevel else { return }
        // Traffic lights always sit in the top-left corner. Apps that hide the native ones
        // (e.g. a browser, which draws its own) report them elsewhere — outside the window, or inside
        // it once it's big enough; those are ignored and the drawn ones found in the pixels.
        let corner = CGRect(x: 0, y: 0, width: min(160, w.frame.width), height: min(80, w.frame.height))
        let rects = control_.buttonRects(w.wid, pid: w.pid).map { corner.contains($0) ? $0 : .zero }
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

    /// The latest raise asked for: raises still queued behind a slow one are skipped (only the
    /// window last pointed at matters).
    private var raiseGeneration = 0
    private let raiseLock = NSLock()

    private func raise(_ win: TrackedWindow) {
        let gen = raiseLock.withLock { raiseGeneration += 1; return raiseGeneration }
        ax { [weak self] control in
            guard let self, self.raiseLock.withLock({ self.raiseGeneration == gen }) else { return }
            control.focus(win.wid, pid: win.pid)
        }
        raisedWid = win.wid
    }

    // MARK: window list

    private func startTracking() {
        tracker.onAdded = { [weak self] w in
            WindowStreamer.prefetch(w.wid)
            self?.goneWindows.remove(w.wid)
            self?.minimized[w.wid] = nil
            self?.placeOnVirtualDisplay(w)
            self?.sendAdded(w)
        }
        tracker.onChanged = { [weak self] w in self?.sendChanged(w) }
        tracker.onRemoved = { [weak self] w in
            guard let self else { return }
            let wid = w.wid
            if self.minimized[wid] != nil { return }
            guard w.kind == .toplevel else { self.windowGone(wid); return }
            // Closed: its Accessibility element is gone already, so say so straight away.
            // (It can take macOS a moment to invalidate it: look again shortly.)
            let control = self.control_
            for delay in [0.0, 0.12, 0.3] {
                self.axQueue.asyncAfter(deadline: .now() + delay) { [weak self] in
                    let exists = control.exists(wid, pid: w.pid)
                    self?.queue.async { [weak self] in
                        guard let self, !exists, self.tracker.windows[wid] == nil, self.minimized[wid] == nil,
                              !self.goneWindows.contains(wid) else { return }
                        self.windowGone(wid)
                    }
                }
            }
            // A minimising window leaves the screen before the minimise animation finishes and
            // AX reports it minimised, so decide a moment later.
            self.queue.asyncAfter(deadline: .now() + 0.7) { [weak self] in
                guard let self, self.tracker.windows[wid] == nil else { return }
                if self.control_.isMinimized(wid, pid: w.pid) {
                    log("window \(wid) minimised on the Mac → minimised on Linux too")
                    self.minimized[wid] = w
                    var m = MsgWriter(.minimize); m.u32(wid); self.control?.send(m)
                } else {
                    self.windowGone(wid)
                }
            }
        }
        tracker.isAppWindow = { [weak self] wid, pid in self?.control_.element(wid, pid: pid) != nil }
        tracker.hasNoButtons = { [weak self] wid, pid in
            guard let self, self.control_.element(wid, pid: pid) != nil else { return false }
            return self.control_.buttonRects(wid, pid: pid).allSatisfy { $0.width == 0 }
        }
        // Which window has focus changes with what the user does or with app switches, which
        // both start a burst of window checks.
        tracker.onBurstPoll = { [weak self] in self?.checkFocus() }
        tracker.onBlindAdded = { [weak self] b in self?.blindAdded(b) }
        tracker.onBlindRemoved = { [weak self] b in
            guard let self else { return }
            self.blindButtons[b.wid] = nil
            self.blindMenus[b.wid] = nil
            var m = MsgWriter(.blindGone); m.u32(b.wid)
            self.control?.send(m)
        }
        tracker.start()
        // Displays come and go (the virtual one, the lid, a real monitor plugged in): macOS can
        // rearrange them and move the virtual display, so look up where it is now, bring the
        // windows back onto it, and capture at its scale.
        screensObserver = NotificationCenter.default.addObserver(forName: NSApplication.didChangeScreenParametersNotification,
                                                                 object: nil, queue: nil) { [weak self] _ in
            self?.refreshVirtualDisplayArea(attempt: 0)
        }
    }

    /// What the user does is when windows, menus, the cursor and the clipboard change: check
    /// those then (just in time) rather than on a timer. A click or key looks closely (menus
    /// open and close), pointer movement less so.
    private func userDidSomething(click: Bool) {
        tracker.burst(everyMs: click ? 8 : 33)
        cursor?.activity()
        if click { clipboard?.0.checkSoon() }
    }

    /// A window that refuses capture appeared: read its text and buttons (a moment later, once
    /// it has drawn) and have Linux show a stand-in.
    /// Open menus shown as stand-ins, to close them when the choice is cancelled on Linux.
    private var blindMenus: [UInt32: AXUIElement] = [:]

    private func blindAdded(_ b: WindowTracker.BlindWindow) {
        if b.menu { return blindMenuAdded(b) }
        DispatchQueue.global(qos: .userInitiated).asyncAfter(deadline: .now() + 0.3) { [weak self] in
            let content = BlindWindows.read(pid: b.pid, frame: b.frame)
            self?.queue.async {
                guard let self, self.tracker.blind[b.wid] != nil, settings.config.enabled else { return }
                let title = content?.title ?? ""
                let texts = (content?.texts ?? []).filter { $0 != title && $0 != b.owner }
                let buttons = content?.buttons ?? []
                self.blindButtons[b.wid] = buttons.map(\.element)
                var m = MsgWriter(.blindShow)
                m.u32(b.wid); m.str(b.owner); m.str(title)
                m.u16(UInt16(texts.count)); texts.forEach { m.str($0) }
                m.u16(UInt16(buttons.count)); buttons.forEach { m.str($0.label) }
                self.control?.send(m)
                log("uncapturable window \(b.wid) (\(b.owner)): stand-in with \(buttons.count) buttons")
            }
        }
    }

    /// A menu that can't be captured (System Settings' pages run in helper processes that
    /// refuse capture): its items as a pick-list on Linux.
    private func blindMenuAdded(_ b: WindowTracker.BlindWindow) {
        log("uncapturable menu \(b.wid) (\(b.owner)): reading its items")
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            let t0 = Date()
            let read = BlindWindows.readMenu(pid: b.pid)
            log(String(format: "uncapturable menu %u: read in %.2fs", b.wid, Date().timeIntervalSince(t0)))
            self?.queue.async {
                guard let self, self.tracker.blind[b.wid] != nil, settings.config.enabled else { return }
                guard let read, !read.items.isEmpty else {
                    return log("uncapturable menu \(b.wid) (\(b.owner)): no items readable (\(read == nil ? "no menu found" : "empty"))")
                }
                self.blindButtons[b.wid] = read.items.map(\.element)
                self.blindMenus[b.wid] = read.menu
                var m = MsgWriter(.blindShow)
                m.u32(b.wid); m.str(b.owner); m.str("")
                m.u16(0)
                m.u16(UInt16(read.items.count)); read.items.forEach { m.str($0.label) }
                m.u8(1)   // a menu
                self.control?.send(m)
                log("uncapturable menu \(b.wid) (\(b.owner)): stand-in with \(read.items.count) items")
            }
        }
    }

    private var watchingLock = false

    /// The connection's QUIC datagram flow (for video), opened on hello.
    private var datagrams: NWConnection?

    private func openDatagrams() {
        guard datagrams == nil, settings.config.videoDatagrams else { return }
        let opts = NWProtocolQUIC.Options()
        opts.isDatagram = true
        guard let flow = group.extract(connectionTo: nil, using: opts) else { return log("datagrams: unavailable, video on streams") }
        flow.stateUpdateHandler = { [weak self] state in
            guard case .failed = state, let self else { return }
            log("datagrams: \(state), video on streams")
            self.datagrams = nil
            self.streamers.values.forEach { $0.sendDatagram = nil }
        }
        flow.start(queue: queue)
        datagrams = flow
    }

    // MARK: settings

    private func sendSettings() {
        var m = MsgWriter(.settings)
        m.bytes(settings.wire())
        control?.send(m)
    }

    /// Changed here (menu-bar item) or on Linux (tray): bring everything in line.
    private func settingsChanged() {
        guard control != nil else { return }
        let c = settings.config
        sendSettings()
        // Windows of apps just switched off (or all of them) go, switched-on ones come.
        tracker.burst(everyMs: 20)
        if c.enabled && c.virtualDisplay, let size = clientScreen {
            DispatchQueue.main.async { host.display.ensure(points: size) }
            refreshVirtualDisplayArea(attempt: 0)
        }
        updateAudio()
        updateClipboard()
        // Quality: captures move to its scale (and encoders to its bit depth).
        streamers.values.forEach { $0.checkScale() }
    }

    private func updateAudio() {
        let c = settings.config
        guard let s = audioStream, c.enabled, c.soundOnLinux else {
            audio?.0.stop()
            audio = nil
            return
        }
        guard audio == nil else { return }
        let cap = AudioCapture(allowed: { settings.config.allows($0) })
        cap.onPCM = { [weak self, weak s] pcm in
            self?.queue.async {
                // Drop rather than queue: late audio is worse than a tiny gap.
                guard let s, s.pendingBytes < 64_000 else { return }
                var m = MsgWriter(.audio)
                m.bytes(pcm)
                s.send(m)
            }
        }
        cap.start()
        audio = (cap, ObjectIdentifier(s))
    }

    private func updateClipboard() {
        let c = settings.config
        guard let s = clipboardStream, c.enabled, c.clipboard else {
            clipboard?.0.stop()
            clipboard = nil
            return
        }
        guard clipboard == nil else { return }
        let sync = ClipboardSync()
        sync.onChange = { [weak self, weak s] items in
            self?.queue.async {
                guard let s else { return }
                var m = MsgWriter(.clipboard)
                m.u16(UInt16(items.count))
                for it in items { m.str(it.kind.rawValue); m.str(it.name); m.bytes(it.data) }
                s.send(m)
                log("clipboard to Linux: \(items.map { $0.kind == .file ? $0.name : $0.kind.rawValue })")
            }
        }
        sync.start()
        clipboard = (sync, s)
    }

    private func sendLockState() {
        let locked = ScreenLock.isLocked
        var m = MsgWriter(.screenLocked)
        m.u8(locked ? 1 : 0)
        control?.send(m)
        // Fresh pictures right away once it's unlocked.
        streamers.values.forEach { $0.setScreenLocked(locked) }
        if !locked && settings.config.virtualDisplay && settings.config.enabled {
            DispatchQueue.main.async { host.display.retry() }
            refreshVirtualDisplayArea(attempt: 0)
        }
    }

    private func sendAppList() {
        DispatchQueue.global(qos: .utility).async { [weak self] in
            let apps = AppList.scan()
            self?.queue.async {
                var m = MsgWriter(.appList)
                m.u16(UInt16(apps.count))
                for a in apps { m.str(a.bundleId); m.str(a.name); m.bytes(a.iconPNG) }
                self?.control?.send(m)
                log("sent \(apps.count) installed apps")
            }
        }
    }

    /// Windows already reported gone (several checks can conclude it).
    private var goneWindows: Set<UInt32> = []

    private func windowGone(_ wid: UInt32) {
        guard goneWindows.insert(wid).inserted else { return }
        control_.forget(wid)
        sentButtons[wid] = nil
        var m = MsgWriter(.windowRemoved); m.u32(wid); control?.send(m)
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
        placeOnVirtualDisplay(w)
        for st in streamers.values where st.wid == w.wid {
            st.windowResized(to: w.frame.size)
            st.checkScale()   // it may have moved to another display
        }
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

/// Pixels per point of the display at `p` (global CG coordinates).
func displayScale(at p: CGPoint) -> CGFloat? {
    var id = CGDirectDisplayID(0)
    var n: UInt32 = 0
    guard CGGetDisplaysWithPoint(p, 1, &id, &n) == .success, n > 0, let mode = CGDisplayCopyDisplayMode(id), mode.width > 0 else {
        return nil
    }
    return CGFloat(mode.pixelWidth) / CGFloat(mode.width)
}
