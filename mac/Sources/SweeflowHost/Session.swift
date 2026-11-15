import SystemConfiguration
import AppKit
import CPrivate
import IOKit
import Network

/// State for one connected client: the control stream, the video streams and the window
/// tracker feeding them. Everything runs on the server's serial queue.
final class Session {
    private let group: NWConnectionGroup
    let queue: DispatchQueue
    private var control: MsgStream?
    private var clientScale: CGFloat = 1
    private let tracker: WindowTracker
    private let control_ = WindowControl()
    private let input = InputInjector()
    private var streamers: [ObjectIdentifier: WindowStreamer] = [:]
    private var audio: (SoundSource, ObjectIdentifier)?
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

    /// The Linux machine on the other end (its certificate's fingerprint).
    var clientFingerprint: String?
    /// It said goodbye: leaving on purpose, so no grace period for it to come back.
    private var leaving = false

    /// What the client calls itself (its Hello; empty from older clients).
    private(set) var clientName = ""
    /// Asked on hello, before anything is served: may this client have the session? False: it
    /// has been answered already (Busy).
    var onHello: ((Session) -> Bool)?
    /// Hello was answered with the session (and it hasn't been ended since).
    var serving: Bool { started && !ended }
    /// Ended (turned away, refused): anything more from the client is ignored while the last
    /// message gets there.
    private var ended = false
    /// When the client last said anything (it pings every 2 s): a connection that went quiet
    /// is taken to be gone.
    private var lastHeard = DispatchTime.now()
    var silentFor: TimeInterval { Double(DispatchTime.now().uptimeNanoseconds - lastHeard.uptimeNanoseconds) / 1e9 }

    /// Says something and ends the connection (a moment later, so it gets there).
    func sendAndClose(_ msg: Msg) {
        sendAndClose(MsgWriter(msg))
    }

    func sendAndClose(_ w: MsgWriter) {
        ended = true
        control?.send(w)
        queue.asyncAfter(deadline: .now() + 0.5) { [weak self] in self?.shutdown() }
    }


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
            let leaving = leaving
            DispatchQueue.main.async { host.client = nil; host.sessionEnded(leaving: leaving) }
        }
        tracker.stop()
        if let o = screensObserver { NotificationCenter.default.removeObserver(o) }
        if let o = settingsObserver { NotificationCenter.default.removeObserver(o) }
        cursor?.stop()
        gestureTimer?.cancel()
        runningTimer?.cancel()
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
        lastHeard = .now()
        guard !ended else { return }
        // Input that comes while a press waits for its window to come to the front (see
        // holdInput) waits too, so it all lands in order.
        if heldInput != nil, [.pointerMove, .pointerButton, .scroll, .key, .releaseAll].contains(m.type) {
            let held = m
            heldInput?.append { [weak self] in
                var m = held
                do { try self?.handle(&m, on: s) } catch { log("held input: \(error)") }
            }
            return
        }
        switch m.type {
        case .goodbye:
            log("client leaving (switching Macs, disconnecting or quitting)")
            leaving = true
        case .hello:
            let v = try m.u16()
            clientScale = try m.f32()
            log("client hello v\(v) scale \(clientScale)")
            if let fp = Server.peerFingerprint(s.conn), fp != clientFingerprint {
                log("client is \(fp.prefix(8)), not \(clientFingerprint?.prefix(8) ?? "unknown") (the latest handshake was another computer's)")
                clientFingerprint = fp
            }
            // A stuck capture service, once restarted, takes every window's capture afresh.
            CaptureHealth.restartAll = { [weak self] in
                self?.queue.async { self?.streamers.values.forEach { $0.restartAfterServiceReset() } }
            }
            control = s
            // Forgotten here: told once, so it forgets this Mac too. Disconnected here: refused.
            if let fp = clientFingerprint, Identity.forgottenClients().contains(fp) {
                log("client \(fp.prefix(8)) was forgotten here: telling it")
                Identity.setForgotten(fp, false)
                return sendAndClose(.forgotten)
            }
            if let fp = clientFingerprint, Identity.disconnectedClients().contains(fp) {
                log("client \(fp.prefix(8)) was disconnected here: refused")
                return sendAndClose(.macDisconnected)
            }
            // Newer clients say their name. One linked without a name (the development setup)
            // takes it, so the menu lists it by name rather than a code from its certificate.
            clientName = (try? m.str()) ?? ""
            if let fp = clientFingerprint, !clientName.isEmpty,
               Identity.pairedClients().first(where: { $0.fingerprint == fp })?.name.isEmpty == true {
                Identity.renameClient(fp, to: clientName)
            }
            if let claim = onHello, !claim(self) { return }
            ScrollAcceleration.reload()   // the scroll settings may have changed meanwhile
            DebugPort.session = self
            if !started {
                started = true
                let fp = clientFingerprint, name = clientName
                DispatchQueue.main.async { host.clientSaysName = name; host.client = fp; host.sessionStarted() }
            }
            let key = ObjectIdentifier(s)
            if let st = streamers.removeValue(forKey: key) { st.stop() }
            var w = MsgWriter(.welcome)
            w.u16(protocolVersion)
            // The Mac's name as set in its settings, from configd (Host.current() can wait on name lookups).
            w.str(SCDynamicStoreCopyComputerName(nil, nil) as String? ?? macName)
            w.str(Self.macKind)
            s.send(w)
            var a = MsgWriter(.addresses)
            let all = Self.ownAddresses()
            a.u16(UInt16(all.count))
            all.forEach { a.str($0) }
            s.send(a)
            // Accessibility calls wait at most a second for an app (not the default 6 s): a
            // hung app must not hold its queue, and everything behind it, for long.
            AXUIElementSetMessagingTimeout(AXUIElementCreateSystemWide(), 1.0)
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
            watchRunningApps()
            probeQueue()
            openDatagrams()
            if !watchingLock {
                watchingLock = true
                ScreenLock.onChange { [weak self] in self?.queue.async { self?.sendLockState() } }
            }
            sendLockState()
        case .ping:
            // Echoed, plus when it got here (Mac clock), for one-way delays on the client.
            var w = MsgWriter(.pong); w.u64(try m.u64()); w.u64(hostMicros()); s.send(w)

        case .subscribe:
            let wid = try m.u32()
            if wid == displayViewWid {
                guard let dv = displayView else { s.close(); return }
                log("stream \(wid): subscribed (Mac display)")
                // At 1 pixel per point: it's for looking around, and a 2x ultrawide is a lot of pixels.
                let st = WindowStreamer(wid: wid, pointSize: dv.bounds.size, scale: 1, lossless: false,
                                        out: s, queue: queue, displayID: dv.id)
                st.active = wid == clientFocus
                if let flow = datagrams {
                    st.sendDatagram = { d in flow.send(content: d, completion: .idempotent) }
                }
                streamers[ObjectIdentifier(s)] = st
                st.start()
                if ScreenLock.isLocked { st.setScreenLocked(true) }
                return
            }
            guard let win = tracker.windows[wid] else { log("subscribe to unknown window \(wid)"); s.close(); return }
            log("stream \(wid): subscribed")
            // A new window is about to move onto the virtual display (see placeOnVirtualDisplay):
            // capture at its scale from the start, not at wherever it opened and then again.
            let vdScale = vdArea.flatMap { win.kind == .popup ? nil : displayScale(at: CGPoint(x: $0.midX, y: $0.midY)) }
            let scale = captureScale(vdScale ?? backingScale(of: win.frame))
            let st = WindowStreamer(wid: wid, pointSize: win.frame.size, scale: scale, lossless: win.kind == .popup,
                                    out: s, queue: queue)
            st.buttons = sentButtons[wid] ?? []
            st.badgeRect = { [weak tracker] in tracker?.badgeRect(wid) }
            st.badgeAbsent = { [weak tracker] in tracker?.badgeAbsent(wid) ?? false }
            st.displayScale = { [weak self, weak tracker] in
                guard let self, let f = tracker?.windows[wid]?.frame,
                      let d = displayScale(at: CGPoint(x: f.midX, y: f.minY + min(f.height / 2, 40))) else { return nil }
                return self.captureScale(d)
            }
            if let app = NSRunningApplication(processIdentifier: win.pid) {
                var r = CGRect(x: 0, y: 0, width: 256, height: 256)
                st.appIcon = app.icon?.cgImage(forProposedRect: &r, context: nil, hints: nil)
            }
            st.onDetectedButtons = { [weak self] rects in
                guard let self else { return }
                self.sentButtons[wid] = rects
                self.sendWindowButtons(wid, rects)
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
                        out.items(items)
                        s.send(out)
                        log("drag to Linux: \(items.map(\.name))")
                    }
                }
            }
        case .clipFetch:
            let number = try m.u32()
            guard let sync = clipboard?.0 else { return }
            sync.files(number) { [weak self] items in
                self?.queue.async {
                    var out = MsgWriter(.clipFiles)
                    out.u32(number)
                    out.items(items ?? [])
                    s.send(out)
                    log("clipboard: pasted on Linux: \(items.map { $0.map(\.name) } ?? ["(gone)"])")
                }
            }
        case .dragDropLocal:
            // A drag out of one Mac window dropped on another (via Linux): drop the same files.
            let wid = try m.u32(), x = try m.f32(), y = try m.f32()
            guard settings.config.dragDrop, let win = tracker.windows[wid] else { return }
            let point = CGPoint(x: win.frame.minX + x, y: win.frame.minY + y)
            // The drop lands on whatever is in front at that point: the target first.
            raiseThen(win) {
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
                self?.queue.async { self?.raiseThen(win) { self?.drag.drop(objects, at: point) } }
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
            clientResized(wid, to: CGSize(width: w, height: h))
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
                ax(pid: w.pid, burstFor: 1) { $0.unminimize(wid, pid: w.pid); $0.focus(wid, pid: w.pid) }
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
            // Off the session queue: a prompt that has gone, or a busy app, answers only when
            // the Accessibility timeout runs out, and input waited that long behind each press.
            if index == 0xFFFF, let menu = blindMenus[wid] {
                // Cancelled on Linux: close the menu on the Mac too.
                DispatchQueue.global(qos: .userInitiated).async { AXUIElementPerformAction(menu, kAXCancelAction as CFString) }
            } else if let b = blindButtons[wid], index < b.count {
                let button = b[index]
                DispatchQueue.global(qos: .userInitiated).async {
                    let r = AXUIElementPerformAction(button, kAXPressAction as CFString)
                    log("uncapturable window \(wid): pressed button \(index) (\(r == .success ? "ok" : "failed"))")
                }
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
            clientFocused(try m.u32())
        case .quitApp:
            // As ⌘Q: the app may still ask about unsaved changes (on the Mac, via a stand-in).
            let bid = try m.str()
            log("quitting \(bid)")
            DispatchQueue.main.async {
                NSRunningApplication.runningApplications(withBundleIdentifier: bid).forEach { $0.terminate() }
            }
        case .showDisplay:
            if try m.u8() != 0 { openDisplayView() } else { closeDisplayView() }
        case .close:
            let wid = try m.u32()
            if wid == displayViewWid { closeDisplayView(); return }
            // The window takes a moment to go after its close button is pressed: watch for it.
            if let win = tracker.windows[wid] { ax(pid: win.pid, burstFor: 2) { $0.close(wid, pid: win.pid) } }
            log("close \(wid)")

        case .pointerMove:
            let (wid, p) = try pointerTarget(&m)
            // A drop from Linux is being replayed with the Mac's own pointer: don't pull it away.
            // Only onto another window: looking at the window list costs a few ms, and motion
            // comes hundreds of times a second (a press still checks every time). And not here:
            // with many windows open the window server answers late, and every move waited.
            if let p, !DragBridge.replaying {
                if wid != raisedWid { ensureFrontSoon(wid, at: p) }
                input.move(to: p)
                probeSpot(wid, at: p)
            }
            userDidSomething(click: false)
        case .pointerButton:
            let (wid, p) = try pointerTarget(&m)
            let b = Int(try m.u8()), pressed = try m.u8() != 0
            let pressId = (try? m.u32()) ?? 0   // newer clients number their presses
            log("click \(pressed ? "down" : "up") on \(wid) at \(p.map { "\(Int($0.x)),\(Int($0.y))" } ?? "nowhere (unknown window)")")
            if pressed { spotProbe.last = nil }   // the hover after this looks afresh (see probeSpot)
            if let p {
                let post = { [self] in
                    input.button(b, pressed: pressed, at: p)
                    userDidSomething(click: true)
                    if b == 0 {
                        watchGesture(wid, buttonDown: pressed)
                        watchDrag(wid, buttonDown: pressed)
                        if pressed, pressId != 0 { classifyPress(wid, at: p, id: pressId) }
                    }
                }
                if pressed { press(wid, at: p, then: post) } else {
                    post()
                    // A capture gone quiet shows up as a click with no new picture.
                    for st in streamers.values where st.wid == wid { st.expectFrameAfterInput() }
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
            // Keys that open, close or switch things (menus, dialogs, fields, shortcuts) look
            // closely; plain typing only as closely as pointer motion.
            if v == 1 { userDidSomething(click: Self.actionKeys.contains(code) || input.commandOrControlHeld) }
            // Keys go to the focused window: its capture should show what they did.
            if v == 1 { for st in streamers.values where st.active { st.expectFrameAfterInput() } }
        case .releaseAll:
            input.releaseAll()
            setClientFocus(0)
        case .moveTaken:
            // Linux may start its move from a spot it knew from the hover (dragSpot), right after
            // the press: in line behind that press if it's being held for a raise (holdInput).
            let wid = try m.u32()
            afterHeld { [weak self] in self?.moveTaken(wid) }

        default:
            log("unhandled message \(m.type)")
        }
    }

    /// Which apps are running and how (RunningApps): looked at every second, sent on change.
    private var runningTimer: DispatchSourceTimer?
    private var lastRunning: [String: UInt8] = [:]

    /// How long work waits for the session queue (input and frame acks go through it), from
    /// a probe every 50 ms on a queue of its own, logged every 10 s while anything waited.
    private func probeQueue() {
        let probe = DispatchQueue(label: "sweeflow.probe")
        var n = 0, sum = 0.0, worst = 0.0, since = DispatchTime.now(), announced = false
        func tick() {
            let t0 = DispatchTime.now()
            queue.async { [weak self] in
                guard let self, self.control != nil else { return }
                let ms = Double(DispatchTime.now().uptimeNanoseconds - t0.uptimeNanoseconds) / 1e6
                probe.async {
                    if !announced { announced = true; log("session queue probe running") }
                    n += 1; sum += ms; worst = max(worst, ms)
                    if DispatchTime.now().uptimeNanoseconds - since.uptimeNanoseconds > 10_000_000_000 {
                        if worst > 2 { log(String(format: "session queue: work waits %.1f ms on average, worst %.1f ms", sum / Double(n), worst)) }
                        n = 0; sum = 0; worst = 0; since = .now()
                    }
                    probe.asyncAfter(deadline: .now() + .milliseconds(50)) { tick() }
                }
            }
        }
        probe.async { tick() }
    }

    private func watchRunningApps() {
        runningTimer?.cancel()
        lastRunning = [:]
        let t = DispatchSource.makeTimerSource(queue: queue)
        t.schedule(deadline: .now() + 0.3, repeating: 1.0)
        t.setEventHandler { [weak self] in self?.sendRunningApps() }
        t.resume()
        runningTimer = t
    }

    private let appsQueue = DispatchQueue(label: "sweeflow.apps", qos: .utility)
    private var scanningApps = false

    /// 3 = has a window showing here, 2 = only minimised ones, 1 = running with no window.
    private func sendRunningApps() {
        // Asking each app what kind it is goes to the system, one round trip per app, and takes
        // over a second when the Mac is busy: not on the main thread (where it held up the
        // cursor, the clipboard and the rest), and never two looks at once.
        guard !scanningApps else { return }
        scanningApps = true
        appsQueue.async { [weak self] in
            let own = Bundle.main.bundleIdentifier
            let bids = NSWorkspace.shared.runningApplications
                .filter { $0.activationPolicy == .regular }
                .compactMap(\.bundleIdentifier)
                .filter { $0 != own }
            self?.queue.async {
                guard let self else { return }
                self.scanningApps = false
                var now: [String: UInt8] = [:]
                for bid in bids {
                    let open = self.tracker.windows.values.contains { $0.bundleId == bid && $0.kind != .popup }
                    let mini = self.minimized.values.contains { $0.bundleId == bid }
                    now[bid] = open ? 3 : mini ? 2 : 1
                }
                guard now != self.lastRunning else { return }
                self.lastRunning = now
                var m = MsgWriter(.runningApps)
                m.u16(UInt16(now.count))
                for (bid, state) in now.sorted(by: { $0.key < $1.key }) { m.str(bid); m.u8(state) }
                self.control?.send(m)
            }
        }
    }

    /// The display shown as a window ("Show Mac display"), while it's open.
    private var displayView: (id: CGDirectDisplayID, bounds: CGRect)?

    /// Announces the whole virtual display (or, without one, the main display) as a window.
    private func openDisplayView() {
        DispatchQueue.main.async { [weak self] in
            let id = host.display.displayID ?? CGMainDisplayID()
            let b = CGDisplayBounds(id)
            self?.queue.async {
                guard let self else { return }
                self.displayView = (id, b)
                log("display view: display \(id) at \(b)")
                var m = MsgWriter(.windowAdded)
                m.u32(displayViewWid); m.i32(0)
                m.str("dev.sweeflow.display"); m.str("Mac display"); m.str(macName)
                m.f32(b.width); m.f32(b.height)
                m.u32(0); m.f32(Float(0)); m.f32(Float(0))
                m.u8(3)
                self.control?.send(m)
            }
        }
    }

    private func closeDisplayView() {
        guard displayView != nil else { return }
        displayView = nil
        for st in streamers.values where st.wid == displayViewWid { st.stop() }
        var m = MsgWriter(.windowRemoved); m.u32(displayViewWid); control?.send(m)
        log("display view closed")
    }

    /// Reads `u32 wid, f32 x, f32 y` and converts to a global Mac point.
    private func pointerTarget(_ m: inout MsgReader) throws -> (UInt32, CGPoint?) {
        let wid = try m.u32(), x = try m.f32(), y = try m.f32()
        // The display view: points on the display, whatever window is there.
        if wid == displayViewWid, let dv = displayView { return (wid, CGPoint(x: dv.bounds.minX + x, y: dv.bounds.minY + y)) }
        if let win = tracker.windows[wid] { return (wid, CGPoint(x: win.frame.minX + x, y: win.frame.minY + y)) }
        // Just gone on the Mac but still shown on Linux a moment longer (a tooltip vanishes as
        // the button under it is pressed): act where it was, on whatever is there now.
        if let gone = recentlyGone[wid], DispatchTime.now().uptimeNanoseconds - gone.at.uptimeNanoseconds < 2_000_000_000 {
            return (gone.parent, CGPoint(x: gone.frame.minX + x, y: gone.frame.minY + y))
        }
        return (wid, nil)
    }

    /// Where windows that just went away were (see pointerTarget).
    private var recentlyGone: [UInt32: (frame: CGRect, parent: UInt32, at: DispatchTime)] = [:]

    /// Windows overlap on the Mac, so the one under the pointer must be on top before we
    /// inject events at its location. A press never waits for the window server on this queue
    /// (it answers late with many windows open, and all input waited with it):
    /// - the tracker's last look at the window list is recent, and nothing has been raised
    ///   since: it says whether the window is in front. In front (the usual case: the pointer
    ///   was already over it), the press goes straight through;
    /// - covered, or not known: the press and what follows it wait (holdInput) while the
    ///   window server is asked off this queue, and the window is raised if it's covered.
    private func press(_ wid: UInt32, at p: CGPoint, then post: @escaping () -> Void) {
        // Menus and floating panels are already on top.
        guard let win = tracker.windows[wid], win.kind == .toplevel else { return post() }
        raisedWid = wid
        let raises = raiseState
        let known = raises.pending ? nil : tracker.frontmostAtLastLook(wid, at: p, maxAgeMs: 2000, since: raises.doneAt)
        if known == true { return post() }
        if known == false { raise(win) }
        let why = known == false ? "for its window to come to the front"
            : raises.pending ? "for the window server's answer (a raise was under way)"
            : String(format: "for the window server's answer (last look %.0f ms ago)", tracker.lastLookAgeMs)
        holdInput(until: wid, at: p, raised: known == false, why: why, then: post)
    }

    /// Whether a raise asked of an app may still change what's in front (it's under way, or
    /// the newest one hasn't had its turn), and when the last one finished: a look at the
    /// window list from before then doesn't say what's in front now. Raises skipped because a
    /// newer one came don't count: they wait their turn behind an app's slow resizes for
    /// seconds, and change nothing. (All under raiseLock.)
    private let raiseNowQueue = DispatchQueue(label: "sweeflow.raise", qos: .userInteractive)
    private var raisesExecuting = 0
    private var raiseDoneGeneration = 0
    private var raiseDoneAt = DispatchTime(uptimeNanoseconds: 0)
    private var raiseState: (pending: Bool, doneAt: DispatchTime) {
        raiseLock.withLock { (raisesExecuting > 0 || raiseDoneGeneration != raiseGeneration, raiseDoneAt) }
    }

    /// Asks the window server which window is in front, off the session queue: for the window
    /// the pointer moves onto (frontQueue, newest question only), and for a press and the
    /// window a held button may be dragging (pressQueue, so they never wait behind the former).
    private let frontQueue = DispatchQueue(label: "sweeflow.front", qos: .userInteractive)
    private let pressQueue = DispatchQueue(label: "sweeflow.press", qos: .userInteractive)
    private var frontGeneration = 0
    private let frontLock = NSLock()

    /// As ensureFront, for pointer motion, which never waits for the answer: the window server
    /// can take long to give it with many windows open, and input queued up behind it for
    /// seconds. The window counts as raised at once, so the moves that follow don't ask again.
    private func ensureFrontSoon(_ wid: UInt32, at p: CGPoint) {
        guard let win = tracker.windows[wid], win.kind == .toplevel else { return }
        raisedWid = wid
        // What isFrontmost needs from the tracker, read here: its state belongs to this queue.
        let pid = win.pid
        let snapshot = tracker.windows
        // Only the newest question matters: the ones still waiting when the pointer has gone on
        // to another window are dropped (asked one after another, they queued up for seconds
        // when the window server answered late).
        let gen = frontLock.withLock { frontGeneration += 1; return frontGeneration }
        frontQueue.async { [weak self] in
            guard let self, self.frontLock.withLock({ self.frontGeneration == gen }) else { return }
            let front = WindowTracker.isFrontmost(wid: wid, pid: pid, point: p, kindOf: { snapshot[$0]?.kind })
            guard !front else { return }
            self.queue.async {
                // Not if the pointer has gone on to another window meanwhile, or this one has gone.
                guard self.raisedWid == wid, let now = self.tracker.windows[wid] else { return }
                self.raise(now)
            }
        }
    }

    /// Input waiting for a window to come to the front (each to be handled in turn).
    private var heldInput: [() -> Void]?

    /// Does `f` now, or after the press that input is waiting behind.
    private func afterHeld(_ f: @escaping () -> Void) {
        if heldInput != nil { heldInput?.append(f) } else { f() }
    }

    /// A press on a window that another one covers on the Mac (windows Linux shows side by side
    /// can overlap there, big tiled ones especially) waits until the raise has taken effect, and
    /// what follows it with it. Raising goes through Accessibility, which a busy app answers
    /// from a few ms to most of a second later; sent straight away, the press landed on the
    /// window in front. Sent anyway after 0.6 s. The window server is asked off this queue, one
    /// question at a time; `raised`: the window has been asked to the front already (if not,
    /// the first answer decides whether it needs to be).
    private func holdInput(until wid: UInt32, at p: CGPoint, raised: Bool, why: String, then post: @escaping () -> Void) {
        heldInput = []
        let start = DispatchTime.now()
        var raised = raised, released = false
        func release(front: Bool) {
            guard !released else { return }
            released = true
            let waited = Double(DispatchTime.now().uptimeNanoseconds - start.uptimeNanoseconds) / 1e6
            if raised || waited > 20 {
                log(String(format: "click on %u held %.0f ms %@%@", wid, waited, why, front ? "" : ": still covered, sent anyway"))
            }
            post()
            var rest = heldInput ?? []
            heldInput = nil
            while !rest.isEmpty {
                let next = rest.removeFirst()
                // A press in there may wait for a raise of its own: the rest waits behind it.
                if heldInput != nil { heldInput?.append(next); continue }
                next()
            }
        }
        func look() {
            guard let win = tracker.windows[wid] else { return release(front: false) }
            // What isFrontmost needs from the tracker, read here: its state belongs to this queue.
            let pid = win.pid, snapshot = tracker.windows
            pressQueue.async { [weak self] in
                let front = WindowTracker.isFrontmost(wid: wid, pid: pid, point: p, kindOf: { snapshot[$0]?.kind })
                self?.queue.async {
                    guard let self, !released else { return }
                    if front { return release(front: true) }
                    if !raised, let win = self.tracker.windows[wid] { raised = true; self.raise(win) }
                    self.queue.asyncAfter(deadline: .now() + .milliseconds(16)) { if !released { look() } }
                }
            }
        }
        // Sent anyway after 0.6 s, whether or not the window server has answered by then.
        queue.asyncAfter(deadline: .now() + .milliseconds(600)) { release(front: false) }
        look()
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

    // MARK: window moves straight from the press

    /// What's under the held press, as classifyPress saw it: kept to learn from if the press
    /// turns out to drag the window the slow way (checkGesture).
    private var pressSpot: (wid: UInt32, id: UInt32, spot: String)?
    /// Spots ("bundle id|role|subrole|identifier") that have dragged their window before: a
    /// press there moves it at once next time. Kept across restarts.
    private var dragSpots = Set(UserDefaults.standard.stringArray(forKey: "dragSpots") ?? [])
    private let hitQueue = DispatchQueue(label: "sweeflow.hit", qos: .userInteractive)

    /// The app moves its window itself when its title bar is dragged, and we only see that
    /// once the window has moved (tens of ms for native windows, a few hundred for web-based
    /// ones): KWin's move then takes hold of the window wherever the pointer has got to by
    /// then, the window lagging behind it. So at the press, look at what's under it: the window
    /// itself or its toolbar within the title-bar strip (above the controls), or a spot that
    /// dragged this app's window before, tells Linux to move the window on the first motion.
    private func classifyPress(_ wid: UInt32, at p: CGPoint, id: UInt32) {
        guard inTitleStrip(wid, at: p) else { return }
        hitSpot(wid, at: p) { [weak self] spot, bare in
            guard let self, let spot, let g = self.gesture, g.wid == wid, g.buttonDown else { return }
            self.pressSpot = (wid, id, spot)
            guard bare || self.dragSpots.contains(spot) else { return }
            var m = MsgWriter(.moveReady); m.u32(wid); m.u32(id); self.control?.send(m)
        }
    }

    /// Whether `p` is in the window's title-bar strip: the band the traffic lights sit centred
    /// in (the title bar, or the toolbar merged with it). A floating panel without them (a
    /// reminder, Quick Look) is moved by any bare part of it. Not a menu (see movable).
    private func inTitleStrip(_ wid: UInt32, at p: CGPoint) -> Bool {
        guard let win = tracker.windows[wid], Self.movable(win) else { return false }
        let strip: CGFloat
        if let button = sentButtons[wid]?.first(where: { $0 != .zero }) {
            strip = button.minY + button.maxY + 2
        } else if win.kind == .dialog || win.kind == .popup {
            strip = win.frame.height
        } else {
            return false
        }
        let y = p.y - win.frame.minY
        return y >= 0 && y <= strip
    }

    /// What's under `p` in window `wid`, by an Accessibility hit test off this queue (the app
    /// answers in a few ms, a busy one later: capped at 250 ms): `then`, on this queue, gets the
    /// spot ("bundle id|role|subrole|identifier") and whether it's bare window or toolbar, or
    /// nil when it isn't this window's (one in front of it, or no answer).
    private func hitSpot(_ wid: UInt32, at p: CGPoint, then: @escaping (_ spot: String?, _ bare: Bool) -> Void) {
        guard let win = tracker.windows[wid] else { return then(nil, false) }
        let bundle = win.bundleId
        hitQueue.async { [weak self] in
            var found: (String, Bool)?
            defer { self?.queue.async { then(found?.0, found?.1 ?? false) } }
            let system = AXUIElementCreateSystemWide()
            AXUIElementSetMessagingTimeout(system, 0.25)
            // The system-wide timeout holds for the whole process: back to the usual second after.
            defer { AXUIElementSetMessagingTimeout(system, 1.0) }
            var hit: AXUIElement?
            guard AXUIElementCopyElementAtPosition(system, Float(p.x), Float(p.y), &hit) == .success, let el = hit else { return }
            func attr(_ e: AXUIElement, _ name: String) -> String {
                var v: CFTypeRef?
                AXUIElementCopyAttributeValue(e, name as CFString, &v)
                return v as? String ?? ""
            }
            // It must be this window's (not one in front of it).
            var owner: AXUIElement? = el
            let role = attr(el, kAXRoleAttribute)
            if role != kAXWindowRole {
                var w: CFTypeRef?
                owner = AXUIElementCopyAttributeValue(el, kAXWindowAttribute as CFString, &w) == .success ? (w as! AXUIElement) : nil
            }
            var ownerId: CGWindowID = 0
            guard let owner, _AXUIElementGetWindow(owner, &ownerId) == .success, ownerId == wid else { return }
            let spot = [bundle, role, attr(el, kAXSubroleAttribute), attr(el, kAXIdentifierAttribute)].joined(separator: "|")
            found = (spot, role == kAXWindowRole || role == kAXToolbarRole)
        }
    }

    /// The hover's hit tests (probeSpot): one at a time, the newest point wanted meanwhile
    /// (so the last place the pointer stops is always looked at), and the last point looked at.
    private var spotProbe: (busy: Bool, want: (wid: UInt32, p: CGPoint)?, last: (wid: UInt32, p: CGPoint, at: DispatchTime)?, retries: Int) = (false, nil, nil, 0)
    /// The last answer sent (window, drags): a change is logged.
    private var spotAnswer: (wid: UInt32, drags: Bool)?

    /// Looks under the pointer while it hovers a window's title-bar strip, before any press, and
    /// tells Linux what's there (dragSpot): with that, a press there starts the window's move
    /// on its first motion with no wait for an answer, so the window is taken hold of exactly
    /// where it was pressed (classifyPress's answer comes a round trip and a hit test later,
    /// by when a quick drag has moved the pointer tens of points: the hand slid over the window
    /// before it caught). A new look every 3 points of travel; an answer "no" matters too (a
    /// tab or toolbar button next to a bare spot gets its own press).
    private func probeSpot(_ wid: UInt32, at p: CGPoint) {
        guard inTitleStrip(wid, at: p) else { spotProbe.last = nil; return }
        // The same spot again (within 3 points, in the last 2 s) needs no new look. Not across a
        // press (pointerButton forgets the last look): the window may have moved under the
        // pointer, on Linux, with it still at the same point of the Mac's screen.
        if let last = spotProbe.last, last.wid == wid, abs(last.p.x - p.x) < 3, abs(last.p.y - p.y) < 3,
           DispatchTime.now().uptimeNanoseconds - last.at.uptimeNanoseconds < 2_000_000_000 { return }
        spotProbe.want = (wid, p)
        spotProbe.retries = 0
        runSpotProbe()
    }

    private func runSpotProbe() {
        guard !spotProbe.busy, let want = spotProbe.want else { return }
        let (wid, p) = (want.wid, want.p)
        spotProbe.want = nil
        guard let win = tracker.windows[wid] else { return }
        spotProbe.busy = true
        spotProbe.last = (wid, p, .now())
        let origin = win.frame.origin
        hitSpot(wid, at: p) { [weak self] spot, bare in
            guard let self else { return }
            self.spotProbe.busy = false
            let drags = bare || spot.map { self.dragSpots.contains($0) } == true
            var m = MsgWriter(.dragSpot)
            m.u32(wid); m.f32(Float(p.x - origin.x)); m.f32(Float(p.y - origin.y)); m.u8(drags ? 1 : 0)
            self.control?.send(m)
            if self.spotAnswer.map({ $0.wid != wid || $0.drags != drags }) ?? true {
                self.spotAnswer = (wid, drags)
                let what = spot.map { String($0.split(separator: "|", omittingEmptySubsequences: false).dropFirst().joined(separator: "|")) } ?? "not this window's"
                log("window \(wid): under the pointer at \(Int(p.x - origin.x)),\(Int(p.y - origin.y)): \(what) → \(drags ? "a press there moves the window" : "no move from there")")
            }
            // Not this window's: the one over it on the Mac is being moved out of the way
            // (ensureFrontSoon, asked as the pointer came). Looked at again shortly, while the
            // pointer stays, a few times.
            if spot == nil, self.spotProbe.want == nil, self.spotProbe.retries < 4 {
                self.spotProbe.retries += 1
                self.queue.asyncAfter(deadline: .now() + .milliseconds(150)) { [weak self] in
                    guard let self, self.spotProbe.want == nil, let last = self.spotProbe.last, last.wid == wid, last.p == p else { return }
                    self.spotProbe.want = (wid, p)
                    self.runSpotProbe()
                }
            }
            self.runSpotProbe()
        }
    }

    /// The press dragged its window the slow way: next time, a press there moves it at once.
    private func learnDragSpot(_ wid: UInt32) {
        guard let s = pressSpot, s.wid == wid else { return }
        pressSpot = nil
        guard dragSpots.insert(s.spot).inserted else { return }
        log("learned a window-drag spot: \(s.spot)")
        UserDefaults.standard.set(Array(dragSpots), forKey: "dragSpots")
    }

    /// Linux has started moving the window itself (after moveReady): let go of the press here,
    /// and undo any move the app had begun meanwhile.
    private func moveTaken(_ wid: UInt32) {
        guard let g = gesture, g.wid == wid, g.buttonDown, let win = tracker.windows[wid] else { return }
        gesture = nil; gestureTimer?.cancel(); gestureTimer = nil
        pressSpot = nil
        input.button(0, pressed: false, at: input.lastPoint)
        watchDrag(wid, buttonDown: false)
        // Back where it was if the app had begun moving it (asked off this queue).
        let origin = g.frame.origin, pid = win.pid, tracker = tracker
        pressQueue.async { [weak self] in
            guard let now = tracker.liveFrame(wid), now.origin != origin else { return }
            self?.queue.async { self?.ax(pid: pid) { $0.setPosition(wid, pid: pid, origin) } }
        }
        log("window \(wid): moved by Linux from the press")
    }

    /// Whether a press can move the window: not a menu, but a floating panel shown attached on
    /// Linux (Quick Look), which Linux moves itself there (a popup to its desktop).
    static func movable(_ win: TrackedWindow) -> Bool {
        win.kind != .popup || (1..<24).contains(win.layer)
    }

    private func watchGesture(_ wid: UInt32, buttonDown: Bool) {
        guard let win = tracker.windows[wid], Self.movable(win) else { return }
        if buttonDown || gesture?.wid != wid {
            // Where the window is as the press lands: the tracker's frame when its last look is
            // recent, else the first look from here (see checkGesture) says. Not asked on this
            // queue, where every press waited for the window server's answer.
            gesture = (wid, win.frame, .now() + .milliseconds(buttonDown ? 10_000 : 400), buttonDown)
            gestureNeedsStart = tracker.lastLookAgeMs > 150
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

    /// A look at the window's frame is on its way (see checkGesture).
    private var gestureLooking = false
    /// The gesture's starting frame isn't known yet: the first look gives it.
    private var gestureNeedsStart = false

    /// Where the window is now is asked off the session queue, one look at a time: the window
    /// server can take far longer than the 8 ms between looks when many windows are open, and
    /// asked here the looks filled the queue for as long as the button was held (up to 10 s),
    /// the button's own release waiting behind them.
    private func checkGesture() {
        guard let g = gesture, tracker.windows[g.wid] != nil, DispatchTime.now() < g.until else {
            gesture = nil; gestureTimer?.cancel(); gestureTimer = nil
            return
        }
        guard !gestureLooking else { return }
        gestureLooking = true
        let wid = g.wid, tracker = tracker
        pressQueue.async { [weak self] in
            let now = tracker.liveFrame(wid)
            self?.queue.async {
                self?.gestureLooking = false
                self?.gestureLooked(wid, now)
            }
        }
    }

    private func gestureLooked(_ wid: UInt32, _ now: CGRect?) {
        // Still the same press, and the window still there.
        guard let g = gesture, g.wid == wid, let win = tracker.windows[wid] else { return }
        guard let now else {
            gesture = nil; gestureTimer?.cancel(); gestureTimer = nil
            return
        }
        if gestureNeedsStart { gestureNeedsStart = false; gesture?.frame = now; return }
        if now == g.frame { return }
        gesture = nil; gestureTimer?.cancel(); gestureTimer = nil
        if now.size == g.frame.size && g.buttonDown {
            // The app started dragging its window: end the drag on the Mac, put the window back,
            // and let KWin move the Linux window instead (the button is still held there).
            input.button(0, pressed: false, at: input.lastPoint)
            ax(pid: win.pid) { $0.setPosition(g.wid, pid: win.pid, g.frame.origin) }
            log("window \(g.wid): title-bar drag → handing move to Linux")
            learnDragSpot(g.wid)
            var m = MsgWriter(.moveStarted); m.u32(g.wid); control?.send(m)
        } else if now.size != g.frame.size && !g.buttonDown {
            // Double-click zoom: undo it on the Mac and maximise the Linux window instead.
            ax(pid: win.pid) { $0.setPosition(g.wid, pid: win.pid, g.frame.origin); $0.setSize(g.wid, pid: win.pid, g.frame.size) }
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
        // Already that size where it is (a relayout sends every window's size again): nothing
        // to ask the app. Not while a resize is under way, which may yet change it.
        if !moved, !resizing.contains(win.wid),
           abs(win.frame.width - size.width) <= 1, abs(win.frame.height - size.height) <= 1 { return }
        resizeTargets[win.wid] = (win.pid, moved ? origin : nil, size)
        applyResize(win.wid)
    }

    /// The latest size (and position) asked for each window. A tiling drag lays out every
    /// window again many times a second, and an Accessibility resize takes 100–500 ms: applied
    /// one by one, the stale sizes queued up for seconds. Now one resize per window runs at a
    /// time and the next takes only the newest target; each app has its own queue, so a slow
    /// app (Electron) doesn't hold the others up.
    private var resizeTargets: [UInt32: (pid: pid_t, origin: CGPoint?, size: CGSize)] = [:]
    private var resizing: Set<UInt32> = []
    private var appAXQueues: [pid_t: DispatchQueue] = [:]

    /// Accessibility calls to one app, in order (a slow app doesn't hold the others up).
    private func appQueue(_ pid: pid_t) -> DispatchQueue {
        if let q = appAXQueues[pid] { return q }
        let q = DispatchQueue(label: "sweeflow.ax.\(pid)", qos: .userInteractive)
        appAXQueues[pid] = q
        return q
    }

    private func applyResize(_ wid: UInt32) {
        guard !resizing.contains(wid), let t = resizeTargets.removeValue(forKey: wid) else { return }
        resizing.insert(wid)
        let control = control_
        appQueue(t.pid).async { [weak self] in
            let t0 = Date()
            if let o = t.origin { control.setPosition(wid, pid: t.pid, o) }
            control.setSize(wid, pid: t.pid, t.size)
            // Moved first so it could grow: an app may size it against where it was, its move not
            // landed yet (TextEdit stopped at the screen's edge from there). Once it's there, again.
            if let o = t.origin, let f = self?.tracker.liveFrame(wid),
               f.width < t.size.width - 2 || f.height < t.size.height - 2 {
                for _ in 0..<10 where self?.tracker.liveFrame(wid)?.origin != o { usleep(20_000) }
                control.setSize(wid, pid: t.pid, t.size)
            }
            let ms = Date().timeIntervalSince(t0) * 1000
            if ms > 100 { log(String(format: "resize of %u took %.0f ms", wid, ms)) }
            // What the app made of it: some keep a size of their own (a minimum, a step).
            let got = self?.tracker.liveFrame(wid)
            // Capture at the new size as soon as the app has resized (not on the next slow look).
            self?.tracker.burst(everyMs: 16, for: 0.5)
            self?.queue.async {
                guard let self else { return }
                if let got, abs(got.width - t.size.width) > 2 || abs(got.height - t.size.height) > 2 {
                    self.refusedResize(wid, t.size, at: t.origin ?? got.origin, got: got.size)
                }
                self.resizing.remove(wid)
                self.applyResize(wid)
            }
        }
    }

    /// The size each window last refused (logged once, not on every try).
    private var refusedSizes: [UInt32: CGSize] = [:]

    private func refusedResize(_ wid: UInt32, _ size: CGSize, at origin: CGPoint, got: CGSize) {
        guard refusedSizes[wid] != size else { return }
        refusedSizes[wid] = size
        log("resize of \(wid): asked \(Int(size.width))x\(Int(size.height)) at \(Int(origin.x)),\(Int(origin.y)), the app made it \(Int(got.width))x\(Int(got.height))")
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
        // Moved less than 1.5 s ago (see justMoved): it may not have got there yet, so what
        // covers it where it seems to be says nothing.
        let recentlyMoved = justMoved[w.wid].map { DispatchTime.now().uptimeNanoseconds - $0.at.uptimeNanoseconds < 1_500_000_000 } ?? false
        if w.kind == .toplevel && !recentlyMoved { origin = uncovered(w, at: origin, in: area) }
        if origin != w.frame.origin { ax(pid: w.pid) { $0.setPosition(w.wid, pid: w.pid, origin) } }
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

    /// Checks whether closed windows' Accessibility elements are gone, or the windows only
    /// minimised (any app's; several at a time, so one slow app doesn't hold the others up).
    private let axQueue = DispatchQueue(label: "sweeflow.ax", qos: .userInteractive, attributes: .concurrent)

    /// Accessibility actions wait on the app (Finder can take a second to close a window): run
    /// them off the session's queue so frames, input and window checks carry on meanwhile, then
    /// look for the change they make straight away. On the app's own queue: one slow app
    /// doesn't hold up raises and moves for the others.
    private func ax(pid: pid_t, burstFor seconds: Double = 0.5, _ action: @escaping (WindowControl) -> Void) {
        let control = control_
        appQueue(pid).async { [weak self] in
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
        // As the client's own messages (the reply doesn't wait for a held press to land).
        if !click, wid != raisedWid { ensureFrontSoon(wid, at: p) }
        input.move(to: p)
        if click {
            // In line behind a press that's being held, as the client's messages are.
            let b = right ? 1 : 0
            afterHeld { [self] in
                press(wid, at: p) { [self] in input.button(b, pressed: true, at: p); userDidSomething(click: true) }
                afterHeld { [self] in input.button(b, pressed: false, at: p) }
            }
        }
        return "ok \(p)"
    }

    func debugDrop(_ wid: UInt32, x: Double, y: Double, file: URL) -> String {
        guard let win = tracker.windows[wid] else { return "no window \(wid)" }
        let point = CGPoint(x: win.frame.minX + x, y: win.frame.minY + y)
        raiseThen(win) { self.drag.drop([.file(file)], at: point) }
        return "dropping at \(point)"
    }

    func debugClose(_ wid: UInt32) -> String {
        guard let w = tracker.windows[wid] else { return "no window \(wid)" }
        ax(pid: w.pid, burstFor: 2) { $0.close(wid, pid: w.pid) }
        return "ok"
    }

    /// What the two ways of telling whether a window is in front say at a point in it, and how
    /// long the window server took to answer.
    func debugFront(wid: UInt32, x: Double, y: Double, reply: @escaping (String) -> Void) {
        guard let win = tracker.windows[wid] else { return reply("no window \(wid)") }
        let p = CGPoint(x: win.frame.minX + x, y: win.frame.minY + y)
        let last = tracker.frontmostAtLastLook(wid, at: p, maxAgeMs: .infinity, since: DispatchTime(uptimeNanoseconds: 0))
        let age = tracker.lastLookAgeMs, flying = raiseState.pending ? 1 : 0, pid = win.pid, snapshot = tracker.windows
        pressQueue.async {
            let t0 = DispatchTime.now()
            let asked = WindowTracker.isFrontmost(wid: wid, pid: pid, point: p, kindOf: { snapshot[$0]?.kind })
            let ms = Double(DispatchTime.now().uptimeNanoseconds - t0.uptimeNanoseconds) / 1e6
            reply(String(format: "last look (%.0f ms old): %@; asked now: %@ in %.0f ms; %d raises under way; ", age,
                         last.map { $0 ? "front" : "covered" } ?? "unknown", asked ? "front" : "covered", ms, flying)
                  + WindowTracker.describeAbove(wid: wid, point: p))
        }
    }

    /// As the client resizing a window (points).
    func debugResize(wid: UInt32, w: Double, h: Double) -> String {
        clientResized(wid, to: CGSize(width: w, height: h)) ? "ok" : "no window \(wid)"
    }

    /// As the client focusing a window.
    func debugFocus(wid: UInt32) -> String {
        clientFocused(wid) ? "ok" : "no window \(wid)"
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

    /// The client focused one of the windows (`focus`): it comes to the front here too.
    /// Whether the window is known.
    @discardableResult
    private func clientFocused(_ wid: UInt32) -> Bool {
        setClientFocus(wid)
        guard let win = tracker.windows[wid] else { return false }
        raise(win)   // which focuses it too
        return true
    }

    /// The client shows a window at a new size (`resize`): the Mac window and its capture
    /// follow. Whether the window is known.
    @discardableResult
    private func clientResized(_ wid: UInt32, to size: CGSize) -> Bool {
        guard let win = tracker.windows[wid] else { return false }
        requestedSizes[wid] = size
        resize(win, to: size)
        return true
    }

    private func sendButtons(_ w: TrackedWindow) {
        guard w.kind == .toplevel else { return }
        // Looked up on the app's own queue: an app busy resizing answers late, and on the
        // session's queue every window's frames waited for it (a tiling relayout resizes them
        // all at once). One look per window at a time; changes meanwhile look once more after.
        guard buttonLookups.insert(w.wid).inserted else { buttonsAgain.insert(w.wid); return }
        let control = control_
        appQueue(w.pid).async { [weak self] in
            // Traffic lights always sit in the top-left corner. Apps that hide the native ones
            // (e.g. a browser, which draws its own) report them elsewhere — outside the window, or
            // inside it once it's big enough; those are ignored and the drawn ones found in the
            // pixels.
            let corner = CGRect(x: 0, y: 0, width: min(160, w.frame.width), height: min(80, w.frame.height))
            let rects = control.buttonRects(w.wid, pid: w.pid).map { corner.contains($0) ? $0 : .zero }
            self?.queue.async {
                guard let self else { return }
                self.buttonLookups.remove(w.wid)
                if self.buttonsAgain.remove(w.wid) != nil, let now = self.tracker.windows[w.wid] {
                    self.sendButtons(now)
                }
                self.applyButtons(w, rects)
            }
        }
    }

    private var buttonLookups: Set<UInt32> = []
    private var buttonsAgain: Set<UInt32> = []

    /// Apps that place their buttons themselves (Tauri- and Electron-style title bars) get them
    /// put back at the standard place by AppKit whenever it lays the title bar out again, and
    /// move them down again with their next drawing. Asked in between, such a window gives the
    /// standard place, higher up, and can stay there while nothing redraws it: Linux then drew
    /// the buttons too high. So a window whose buttons went to a place and straight back (same
    /// sizes and spacing, within a few seconds) keeps them at the lower of the two from then
    /// on, which is where the app puts them; and the app is remembered for it (across
    /// restarts), so its windows get the lower place straight away next time.
    private var buttonHistory: [UInt32: [(at: DispatchTime, rects: [CGRect])]] = [:]
    private var buttonsKept: [UInt32: [CGRect]] = [:]
    /// "bundle id|the standard place as reported" → how far down and right the app really has
    /// its buttons ("dx,dy").
    private var buttonShifts = UserDefaults.standard.dictionary(forKey: "buttonShifts") as? [String: String] ?? [:]

    private func settledButtons(_ w: TrackedWindow, _ rects: [CGRect]) -> [CGRect] {
        let wid = w.wid
        guard rects.count == 3, rects.allSatisfy({ $0.width > 0 }) else { return rects }
        func sameShape(_ a: [CGRect], _ b: [CGRect]) -> Bool {
            a.count == 3 && b.count == 3 && zip(a, b).allSatisfy { $0.size == $1.size }
                && abs((a[1].minX - a[0].minX) - (b[1].minX - b[0].minX)) < 0.5
        }
        func key(_ r: [CGRect]) -> String {
            w.bundleId + "|" + r.map { "\(Int($0.minX)),\(Int($0.minY)) \(Int($0.width))x\(Int($0.height))" }.joined(separator: " ")
        }
        // macOS's capture badge sits over the real buttons: when it covers them where they're
        // reported and not where a shift would put them, they are where reported. (TextEdit has
        // two real places, by its toolbar's height: going from one to the other and back had it
        // taken for an app that moves its own buttons, and Linux drew them 26 points too low,
        // over its formatting bar.)
        func badgeSaysAsReported(_ shifted: [CGRect]) -> Bool {
            guard let badge = tracker.badgeRect(wid)?.insetBy(dx: -2, dy: -2) else { return false }
            let mid = { (r: [CGRect]) in CGPoint(x: r[1].midX, y: r[1].midY) }
            return badge.contains(mid(rects)) && !badge.contains(mid(shifted))
        }
        // Known from before: this app's buttons, reported at the standard place.
        if let shift = buttonShifts[key(rects)]?.split(separator: ",").compactMap({ Double($0) }), shift.count == 2 {
            let shifted = rects.map { $0.offsetBy(dx: shift[0], dy: shift[1]) }
            if !badgeSaysAsReported(shifted) { return shifted }
            buttonShifts[key(rects)] = nil
            UserDefaults.standard.set(buttonShifts, forKey: "buttonShifts")
            log("window \(wid) (\(w.appName)): the badge is over its buttons where reported: no longer shifted for the app")
            return rects
        }
        if let kept = buttonsKept[wid] {
            if sameShape(kept, rects) {
                if rects[0].minY > kept[0].minY { buttonsKept[wid] = rects; return rects }
                if badgeSaysAsReported(kept) { buttonsKept[wid] = nil; buttonHistory[wid] = nil; return rects }
                return kept
            }
            // Other buttons altogether (another title bar style): start over.
            buttonsKept[wid] = nil
            buttonHistory[wid] = nil
        }
        var history = buttonHistory[wid] ?? []
        if history.last?.rects != rects { history.append((.now(), rects)) }
        history = Array(history.suffix(3))
        buttonHistory[wid] = history
        if history.count == 3, history[0].rects == history[2].rects, sameShape(history[0].rects, history[1].rects),
           history[2].at.uptimeNanoseconds - history[0].at.uptimeNanoseconds < 5_000_000_000 {
            let (a, b) = (history[0].rects, history[1].rects)
            let (lower, higher) = a[0].minY >= b[0].minY ? (a, b) : (b, a)
            // Both places real (the badge is over the buttons where they are now, the higher
            // one): nothing to keep or remember.
            if rects == higher, badgeSaysAsReported(lower) { return rects }
            buttonsKept[wid] = lower
            buttonShifts[key(higher)] = "\(lower[0].minX - higher[0].minX),\(lower[0].minY - higher[0].minY)"
            UserDefaults.standard.set(buttonShifts, forKey: "buttonShifts")
            log("window \(wid) (\(w.appName)): its buttons went to a place and straight back: kept at the lower one, and remembered for the app")
            return lower
        }
        return rects
    }

    private func applyButtons(_ w: TrackedWindow, _ rects: [CGRect]) {
        guard tracker.windows[w.wid] != nil else { return }
        // A transient failure (window mid-move) must not wipe known-good positions.
        if rects.allSatisfy({ $0 == .zero }), sentButtons[w.wid]?.contains(where: { $0 != .zero }) == true {
            return
        }
        let rects = settledButtons(w, rects)
        guard sentButtons[w.wid] != rects else { return }
        sentButtons[w.wid] = rects
        for st in streamers.values where st.wid == w.wid { st.buttons = rects }
        log("window \(w.wid) (\(w.appName)) buttons \(rects.map { "\(Int($0.minX)),\(Int($0.minY)) \(Int($0.width))x\(Int($0.height))" })")
        sendWindowButtons(w.wid, rects)
    }

    /// Where a window's close, minimise and zoom buttons are (window points).
    private func sendWindowButtons(_ wid: UInt32, _ rects: [CGRect]) {
        var m = MsgWriter(.windowButtons)
        m.u32(wid)
        for r in rects { m.f32(r.minX); m.f32(r.minY); m.f32(r.width); m.f32(r.height) }
        control?.send(m)
    }

    /// Brings a window to the front, then (on the main thread) does what needs it there.
    private func raiseThen(_ win: TrackedWindow, _ then: @escaping () -> Void) {
        if tracker.isKnownSheet(win.wid), let parent = tracker.windows[win.parent], parent.kind == .toplevel {
            return raiseThen(parent, then)
        }
        guard win.kind == .toplevel else { return DispatchQueue.main.async(execute: then) }
        let control = control_
        raisedWid = win.wid
        appQueue(win.pid).async {
            control.focus(win.wid, pid: win.pid)
            // A moment for the window server to restack before the drop is replayed.
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.08, execute: then)
        }
    }

    /// The latest raise asked for: raises still queued behind a slow one are skipped (only the
    /// window last pointed at matters).
    private var raiseGeneration = 0
    private let raiseLock = NSLock()

    private func raise(_ win: TrackedWindow) {
        // A sheet comes forward with its window, and is raised by raising that: put in front on
        // its own (its window number to the window server), the app took no more clicks on it
        // until its window was clicked.
        if tracker.isKnownSheet(win.wid), let parent = tracker.windows[win.parent], parent.kind == .toplevel {
            return raise(parent)
        }
        log("raising \(win.wid) (\(win.appName))")
        let gen = raiseLock.withLock { raiseGeneration += 1; return raiseGeneration }
        // To the front in the window server at once, in the order asked: the rest (below) goes
        // through the app, behind whatever else it has been asked (a resize can take a second
        // or two), and a press on the window waited all that time for it to come forward.
        let control = control_
        raiseNowQueue.async { control.frontNow(win.wid, pid: win.pid) }
        ax(pid: win.pid) { [weak self] control in
            guard let self else { return }
            let newest = self.raiseLock.withLock { () -> Bool in
                guard self.raiseGeneration == gen else { return false }
                self.raisesExecuting += 1
                return true
            }
            guard newest else { return }
            control.focus(win.wid, pid: win.pid)
            self.raiseLock.withLock {
                self.raisesExecuting -= 1
                self.raiseDoneGeneration = max(self.raiseDoneGeneration, gen)
                self.raiseDoneAt = .now()
            }
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
            self.fixedKnown[w.wid] = nil
            self.fixedSpots[w.wid] = nil
            let now = DispatchTime.now()
            self.recentlyGone = self.recentlyGone.filter { now.uptimeNanoseconds - $0.value.at.uptimeNanoseconds < 2_000_000_000 }
            self.recentlyGone[w.wid] = (w.frame, w.parent == 0 ? w.wid : w.parent, now)
            let wid = w.wid
            self.announced.remove(wid)
            // Its popups waiting for it: told now, with whatever window they have by then.
            for c in self.awaitingParent.removeValue(forKey: wid) ?? [] {
                if let now = self.tracker.windows[c] { self.sendAdded(now) }
            }
            self.lastFrames[wid] = nil
            self.refusedSizes[wid] = nil
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
            // (Asked off this queue: an app closing its windows answers late, and with several
            // closing at once input waited a second behind each.)
            self.axQueue.asyncAfter(deadline: .now() + 0.7) { [weak self] in
                let minimised = control.isMinimized(wid, pid: w.pid)
                self?.queue.async {
                    guard let self, self.tracker.windows[wid] == nil else { return }
                    if minimised {
                        log("window \(wid) minimised on the Mac → minimised on Linux too")
                        self.minimized[wid] = w
                        var m = MsgWriter(.minimize); m.u32(wid); self.control?.send(m)
                    } else {
                        self.windowGone(wid)
                    }
                }
            }
        }
        tracker.isAppWindow = { [weak self] wid, pid in self?.control_.element(wid, pid: pid) != nil }
        tracker.isSheet = { [weak self] wid, pid in
            // A child of the window it hangs from, or listed as a window with the role of one.
            self?.control_.isSheet(wid, pid: pid) ?? false
        }
        tracker.hasNoButtons = { [weak self] wid, pid in
            guard let self, self.control_.element(wid, pid: pid) != nil else { return false }
            return self.control_.buttonRects(wid, pid: pid).allSatisfy { $0.width == 0 }
        }
        // Which window has focus changes with what the user does or with app switches, which
        // both start a burst of window checks.
        tracker.onBurstPoll = { [weak self] in self?.checkFocus() }
        tracker.onBadgeAppeared = { [weak self] wid in
            guard let self, let w = self.tracker.windows[wid] else { return }
            self.sendButtons(w)
        }
        tracker.onBadgeFound = { [weak self] wid in
            self?.streamers.values.filter { $0.wid == wid }.forEach { $0.requestRepaint() }
        }
        tracker.onBlindAdded = { [weak self] b in self?.blindAdded(b) }
        tracker.onScreenSelect = { [weak self] on in
            log("screenshot selection \(on ? "started" : "ended")")
            var m = MsgWriter(.screenSelect); m.u8(on ? 1 : 0)
            self?.control?.send(m)
        }
        tracker.onBlindRemoved = { [weak self] b in
            guard let self else { return }
            self.blindButtons[b.wid] = nil
            self.blindMenus[b.wid] = nil
            var m = MsgWriter(.blindGone); m.u32(b.wid)
            self.control?.send(m)
        }
        tracker.start()
        // Which window is in front here, told as the client starts showing them: it focuses
        // that one, not whichever its desktop focuses last as they arrive (linux arrival.rs).
        tracker.burst(everyMs: 50, for: 3)
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
    /// open and close), pointer movement less so. Motion comes hundreds of times a second:
    /// it starts a look at most every 50 ms, and the clipboard is looked at at most every
    /// 250 ms, so they don't flood the queues.
    private func userDidSomething(click: Bool) {
        let now = DispatchTime.now()
        if click || now.uptimeNanoseconds - lastActivity.uptimeNanoseconds >= 50_000_000 {
            lastActivity = now
            tracker.burst(everyMs: click ? 8 : 33)
            cursor?.activity()
        }
        if click, now.uptimeNanoseconds - lastClipboardCheck.uptimeNanoseconds >= 250_000_000 {
            lastClipboardCheck = now
            clipboard?.0.checkSoon()
        }
    }

    private var lastActivity = DispatchTime(uptimeNanoseconds: 0)
    private var lastClipboardCheck = DispatchTime(uptimeNanoseconds: 0)
    /// Return, keypad Enter, Escape, Tab and Space (evdev codes): keys that open, close or
    /// switch things.
    private static let actionKeys: Set<UInt32> = [28, 96, 1, 15, 57]

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

    /// What to capture a window at, drawn at `display` pixels per point on the Mac. High
    /// quality: as the Mac draws it (2× on its Retina virtual display), even for a 1× Linux
    /// screen: shrunk there by the compositor it reads sharper than captured at 1×, which the
    /// Mac's window server shrinks softly. Low: 1×.
    private func captureScale(_ display: CGFloat) -> CGFloat {
        settings.config.quality == "light" ? 1 : display
    }

    private func updateAudio() {
        let c = settings.config
        guard let s = audioStream, c.enabled, c.soundOnLinux else {
            audio?.0.stop()
            audio = nil
            return
        }
        guard audio == nil else { return }
        let onPCM: (Data, UInt32) -> Void = { [weak self, weak s] pcm, ageUs in
            self?.queue.async {
                // Drop rather than queue: late audio is worse than a tiny gap.
                guard let s, s.pendingBytes < 64_000 else { return }
                var m = MsgWriter(.audio)
                m.bytes(pcm)
                m.u32(ageUs)   // how long after it played it was captured (older clients stop at the sound)
                s.send(m)
            }
        }
        // A Core Audio tap (~10 ms behind the sound); ScreenCaptureKit's (~55 ms) when there's none.
        let allowed: (String) -> Bool = { settings.config.allows($0) }
        let tap = AudioTap(allowed: allowed)
        tap.onPCM = onPCM
        audio = (tap, ObjectIdentifier(s))
        tap.start { [weak self] ok in
            guard !ok else { return }
            self?.queue.async {
                guard let self, let a = self.audio, a.0 === tap else { return }
                let cap = AudioCapture(allowed: allowed)
                cap.onPCM = onPCM
                cap.start()
                self.audio = (cap, a.1)
            }
        }
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
                m.items(items)
                s.send(m)
                log("clipboard to Linux: \(items.map { $0.kind == .fileRef ? $0.name : $0.kind.rawValue })")
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
        buttonHistory[wid] = nil
        buttonsKept[wid] = nil
        var m = MsgWriter(.windowRemoved); m.u32(wid); control?.send(m)
    }

    private var lastFocusCheck = DispatchTime(uptimeNanoseconds: 0)
    private var focusChecking = false
    private let focusQueue = DispatchQueue(label: "sweeflow.focus", qos: .userInteractive)

    private func checkFocus() {
        // An Accessibility round trip to the frontmost app: not on every 8 ms look, and not on
        // the session's queue (an app busy resizing answers late, and every window's frames
        // waited meanwhile).
        let now = DispatchTime.now()
        guard !focusChecking, now.uptimeNanoseconds - lastFocusCheck.uptimeNanoseconds > 50_000_000 else { return }
        lastFocusCheck = now
        focusChecking = true
        let control = control_
        focusQueue.async { [weak self] in
            let pid = NSWorkspace.shared.frontmostApplication?.processIdentifier
            let focused = pid.flatMap { control.focusedWindowID(of: $0) }
            self?.queue.async {
                guard let self else { return }
                self.focusChecking = false
                // Not said (Finder with Quick Look up has no focused window for Accessibility):
                // the app's frontmost window, from the window server's order.
                var wid = focused ?? self.tracker.zOrder.first { self.tracker.windows[$0].map { $0.pid == pid && $0.kind == .toplevel } ?? false }
                // A panel shown as a popup on Linux (Quick Look, when Accessibility names it)
                // can't have the focus there: the window it's shown on.
                if let w = wid.flatMap({ self.tracker.windows[$0] }), w.kind == .popup, w.parent != 0 { wid = w.parent }
                guard let wid, wid != self.lastFocused, self.tracker.windows[wid] != nil else { return }
                self.lastFocused = wid
                var m = MsgWriter(.focused); m.u32(wid); self.control?.send(m)
            }
        }
    }

    /// This Mac's IPv4 addresses on interfaces that are up (not loopback), and its .local name.
    static func ownAddresses() -> [String] {
        var out: [String] = []
        var list: UnsafeMutablePointer<ifaddrs>?
        if getifaddrs(&list) == 0, let first = list {
            var p: UnsafeMutablePointer<ifaddrs>? = first
            while let i = p {
                let flags = Int32(i.pointee.ifa_flags)
                if let a = i.pointee.ifa_addr, a.pointee.sa_family == UInt8(AF_INET),
                   flags & IFF_UP != 0, flags & IFF_LOOPBACK == 0 {
                    var host = [CChar](repeating: 0, count: Int(NI_MAXHOST))
                    if getnameinfo(a, socklen_t(a.pointee.sa_len), &host, socklen_t(host.count), nil, 0, NI_NUMERICHOST) == 0 {
                        let s = String(cString: host)
                        if !s.hasPrefix("169.254."), !out.contains(s) { out.append(s) }
                    }
                }
                p = i.pointee.ifa_next
            }
            freeifaddrs(first)
        }
        if let local = SCDynamicStoreCopyLocalHostName(nil) as String? { out.append(local + ".local") }
        return out
    }

    /// Whether each window's app lets it be resized (looked at once per window).
    private var fixedKnown: [UInt32: Bool] = [:]
    /// Where each window first appeared on its screen (see spot(of:)).
    private var fixedSpots: [UInt32: (Float, Float)] = [:]

    /// The window is fixed-size, and the spot its app put it at: Linux shows it there too.
    private func sendFixed(_ wid: UInt32, fixed: Bool = true) {
        let (fx, fy) = fixedSpots[wid] ?? (0.5, 0.5)
        var f = MsgWriter(.windowFixed); f.u32(wid); f.f32(fx); f.f32(fy); f.u8(fixed ? 1 : 0); control?.send(f)
    }

    /// Where a window is on its screen, as fractions of the room it has there: 0 flush with the
    /// left (top) edge, 1 flush with the right (bottom), 0.5 centred. A reminder an app puts in
    /// the top-right corner is (1, 0) on any screen size.
    static func spot(of frame: CGRect) -> (Float, Float) {
        var id: CGDirectDisplayID = 0, n: UInt32 = 0
        guard CGGetDisplaysWithPoint(CGPoint(x: frame.midX, y: frame.midY), 1, &id, &n) == .success, n > 0 else { return (0.5, 0.5) }
        let b = CGDisplayBounds(id)
        let f = { (at: CGFloat, start: CGFloat, room: CGFloat) -> Float in room <= 1 ? 0.5 : Float(max(0, min(1, (at - start) / room))) }
        return (f(frame.minX, b.minX, b.width - frame.width), f(frame.minY, b.minY, b.height - frame.height))
    }

    /// Windows told to the client, and popups waiting for theirs to be told first, by window.
    /// A window is told once its app has said whether it can be resized (a moment), a popup at
    /// once: after a reconnect, Quick Look (open over its Finder window) got to the client
    /// before that window, and was shown as a window of its own there.
    private var announced: Set<UInt32> = []
    private var awaitingParent: [UInt32: [UInt32]] = [:]

    /// Tells Linux about a new window. Whether its app lets it be resized is looked at first
    /// (Accessibility, off this queue), so a fixed one (a reminder, a dialog) is said to be
    /// fixed before it's added: Linux then shows it as a dialog of its own size from the start,
    /// which desktops and tilers (Karousel) float instead of tiling, and they decide that only
    /// when a window appears. An app slow to answer doesn't hold the window up: after 60 ms it's
    /// added anyway, and said to be fixed when the answer comes.
    private func sendAdded(_ w: TrackedWindow) {
        if w.kind == .popup {
            if w.parent != 0, !announced.contains(w.parent), tracker.windows[w.parent] != nil {
                awaitingParent[w.parent, default: []].append(w.wid)
                return
            }
            return announce(w, fixed: false)
        }
        // Where the app put it, before it's moved onto the virtual display.
        let spot = Self.spot(of: w.frame)
        fixedSpots[w.wid] = fixedSpots[w.wid] ?? spot
        if let fixed = fixedKnown[w.wid] { return announce(w, fixed: fixed) }
        final class Once { var sent = false }
        // A sheet keeps the size its app gives it (its window is resized, not it).
        let once = Once(), control = control_, wid = w.wid, pid = w.pid, sheet = tracker.isKnownSheet(w.wid)
        appQueue(pid).async { [weak self] in
            let fixed = sheet || control.fixedSize(wid, pid: pid) == true
            self?.queue.async {
                guard let self, let now = self.tracker.windows[wid] else { return }
                self.fixedKnown[wid] = fixed
                if !once.sent {
                    once.sent = true
                    self.announce(now, fixed: fixed)
                } else if fixed {
                    self.sendFixed(wid)
                }
            }
        }
        queue.asyncAfter(deadline: .now() + 0.06) { [weak self] in
            guard let self, !once.sent, let now = self.tracker.windows[wid] else { return }
            once.sent = true
            self.announce(now, fixed: false)
        }
    }

    private func announce(_ w: TrackedWindow, fixed: Bool) {
        sendIcon(w)
        // A floating panel (picture in picture, a palette) floats on Linux too, resizable.
        if fixed || w.kind == .dialog { sendFixed(w.wid, fixed: fixed) }
        var m = MsgWriter(.windowAdded)
        m.u32(w.wid); m.i32(w.pid)
        m.str(w.bundleId); m.str(w.appName); m.str(w.title)
        m.f32(w.frame.width); m.f32(w.frame.height)
        m.u32(w.parent); m.f32(w.rel.x); m.f32(w.rel.y)
        m.u8(w.kind.rawValue)
        control?.send(m)
        lastFrames[w.wid] = w.frame
        sendButtonsNowAndSettled(w)
        announced.insert(w.wid)
        for c in awaitingParent.removeValue(forKey: w.wid) ?? [] {
            if let now = tracker.windows[c] { sendAdded(now) }
        }
    }

    /// The window's buttons now, and again shortly. An app that places them itself (an
    /// Electron-style title bar) puts them back a moment after its window opens or changes
    /// size; asked right then, it still gives the standard place, higher up, and that was
    /// where Linux drew them from then on. (A look that finds nothing new sends nothing.)
    private func sendButtonsNowAndSettled(_ w: TrackedWindow) {
        sendButtons(w)
        // (3 s: macOS's capture badge, which says where the buttons really are, is up by then.)
        for delay in [0.4, 1.5, 3] {
            queue.asyncAfter(deadline: .now() + delay) { [weak self] in
                guard let self, let now = self.tracker.windows[w.wid] else { return }
                self.sendButtons(now)
            }
        }
    }

    /// Each window's frame as last sent (sendAdded, sendChanged).
    private var lastFrames: [UInt32: CGRect] = [:]

    private func sendChanged(_ w: TrackedWindow) {
        let last = lastFrames[w.wid]
        lastFrames[w.wid] = w.frame
        // Only when it moved or changed size: a title change alone doesn't place it again (with
        // many windows, every change re-placing them kept them moving round and round).
        if last != w.frame { placeOnVirtualDisplay(w) }
        for st in streamers.values where st.wid == w.wid {
            st.windowResized(to: w.frame.size)
            st.checkScale()   // it may have moved to another display
        }
        var m = MsgWriter(.windowChanged)
        m.u32(w.wid); m.str(w.title)
        m.f32(w.frame.width); m.f32(w.frame.height)
        m.f32(w.rel.x); m.f32(w.rel.y)
        control?.send(m)
        // The buttons sit where they did unless the size changed (each look asks the app).
        // (Once, not the three looks a new window gets: every look asks the app, on the queue its
        // resizes wait on, and a relayout changes every window's size at once. Where the app
        // really keeps its buttons is remembered per app by now, see settledButtons.)
        if last?.size != w.frame.size { sendButtons(w) }
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

extension Session {
    /// What sort of Mac this is, for its icon on Linux: "laptop" (MacBook), "imac" or "desktop"
    /// (Mac mini, Studio, Pro). From the model name, else from whether it has a built-in screen.
    static let macKind: String = {
        let svc = IOServiceGetMatchingService(kIOMainPortDefault, IOServiceMatching("IOPlatformExpertDevice"))
        defer { IOObjectRelease(svc) }
        var name = ""
        for key in ["product-name", "model"] {
            if let d = IORegistryEntryCreateCFProperty(svc, key as CFString, kCFAllocatorDefault, 0)?.takeRetainedValue() as? Data {
                name += String(decoding: d.prefix { $0 != 0 }, as: UTF8.self) + " "
            }
        }
        if name.contains("MacBook") { return "laptop" }
        if name.contains("iMac") { return "imac" }
        if name.contains("Mac mini") || name.contains("Macmini") || name.contains("Mac Studio") || name.contains("Mac Pro") { return "desktop" }
        return BuiltInScreen.screen != nil ? "laptop" : "desktop"
    }()
}
