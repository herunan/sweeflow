import AppKit
import Foundation
import ScreenCaptureKit
import VideoToolbox
import CoreMedia
import Compression
import Accelerate

/// Captures one window with ScreenCaptureKit, encodes it with the hardware HEVC encoder and
/// writes frames to a video stream. Flow control is end-to-end: at most `maxInFlight` frames may
/// be un-acknowledged by the client; newer captures replace older unsent ones, so latency never
/// builds up in queues when the network (Wi-Fi) hiccups.
final class WindowStreamer: NSObject, SCStreamOutput, SCStreamDelegate {
    let wid: UInt32
    private let out: MsgStream
    private let queue: DispatchQueue            // net/session queue; all mutable state lives here
    private let captureQueue = DispatchQueue(label: "sweeflow.capture", qos: .userInteractive)
    private var scale: CGFloat
    private var pointSize: CGSize

    private var stream: SCStream?
    private var scWindow: SCWindow?
    private var config = SCStreamConfiguration()
    private var encoder: VTCompressionSession?
    private var encoderSize: (Int32, Int32) = (0, 0)

    private var inFlight: [UInt64] = []
    /// Sends one QUIC datagram (set by the session): delta frames go this way, so a lost packet
    /// costs that frame rather than stalling the stream behind it.
    var sendDatagram: ((Data) -> Void)?
    /// Numbers HEVC frames, so the client can order them across the stream and datagrams and
    /// notice a lost one.
    private var frameSeq: UInt32 = 0
    // Enough for 100–120 fps over a ~25 ms pipeline; more only queues frames in the encoder.
    private let maxInFlight = 3
    private var pendingBuffer: CVPixelBuffer?   // newest capture not yet encoded (throttled)
    /// A frame is in the encoder. Only one at a time: a big Retina window takes 20–40 ms to
    /// encode, and frames handed over meanwhile queued inside it, each waiting for the ones
    /// ahead (70–240 ms behind under steady updates). Now the newest capture waits here
    /// instead, replacing older ones, and goes in as soon as the encoder is free.
    private var encoding = false
    /// Frames handed to the encoder whose call into it hasn't returned yet, and whether an
    /// encoder is being made: nothing new goes in meanwhile (both happen on `work`).
    private var inEncoder = 0
    /// How long the last frame took from its preparation to coming out of the encoder.
    private var lastEncodeUs: UInt64 = 0
    private var makingEncoder = false
    private var encodingSince = DispatchTime.now()
    /// The pending buffer has had its overlays painted already (a re-encode of the last one).
    private var pendingPrepared = false
    /// Pixel work on a frame (overlays, corners, the 10-bit conversion: 5–15 ms on a Retina
    /// window) runs here, off the session queue, so input never waits behind video.
    private let work = DispatchQueue(label: "sweeflow.frame", qos: .userInteractive)
    /// The focus changed while a frame was being prepared: repaint the lights once it's done.
    private var repaintAfter = false
    private var lastBuffer: CVPixelBuffer?      // last encoded capture, reused for refinement
    private var lastEncodeAt = DispatchTime.now()
    private var refinementsLeft = 0
    private var refineTimer: DispatchSourceTimer?
    private var forceKeyframe = true
    private var paused = false
    private var stopped = false
    /// Traffic-light frames (window points) to repaint over macOS's capture badge, and whether
    /// this window currently has focus on the Linux side.
    var buttons: [CGRect] = []
    /// Where macOS's capture badge overlaps this window (window points), looked up per frame.
    var badgeRect: () -> CGRect? = { nil }
    /// The window's top-left corner as it looks without the badge (see CleanCorner).
    private lazy var cleanCorner = CleanCorner.of(wid)
    /// Whether the window server has just said the window has no badge (WindowTracker.badgeAbsent).
    var badgeAbsent: () -> Bool = { false }
    /// Where the window's content ends in the last capture (px): all of it, unless the capture
    /// is of a stale size and macOS padded it (ScreenCaptureKit's content rect). The corner
    /// tiles are cut there, the window's real corners; Linux puts them on the edges it shows
    /// whole, and cuts the rest off straight.
    private var captureContent: CGSize?
    /// The app's icon, shown on the placeholder when the Mac is locked before the first frame.
    var appIcon: CGImage?
    /// Pixels per point of the display the window is on (looked up from its position).
    var displayScale: (() -> CGFloat?)?
    /// Called when traffic lights drawn by the app itself were found in the pixels.
    var onDetectedButtons: (([CGRect]) -> Void)?
    private var detectCountdown = 0
    /// Focused on Linux. The hardware encoder is shared by every window, so only the focused one
    /// gets the full frame rate; the rest are capped (they rarely need more, and at full rate
    /// they queue up in the encoder and add tens of milliseconds to everything).
    var active = false {
        didSet { if active != oldValue { applyFrameRate() } }
    }
    private static let activeFps: Int32 = 120, backgroundFps: Int32 = 30
    /// While nothing in the window has changed for a second (see differsFromLast): a look often
    /// enough to catch the first change soon, and the full rate again from then.
    private static let activeIdleFps: Int32 = 60, backgroundIdleFps: Int32 = 5
    private var unchanging = false

    private var frameRate: Int32 {
        active ? (unchanging ? Self.activeIdleFps : Self.activeFps) : (unchanging ? Self.backgroundIdleFps : Self.backgroundFps)
    }

    private func applyFrameRate() {
        config.minimumFrameInterval = CMTime(value: 1, timescale: frameRate)
        stream?.updateConfiguration(config) { err in if let err { log("updateConfiguration: \(err)") } }
    }

    // MARK: unchanged captures

    /// macOS hands over a "changed" frame of a window whenever anything changes on the part of
    /// its display the window occupies, whoever drew it: windows share the virtual display and
    /// overlap there, so one window that keeps drawing (a terminal, a video) made every window
    /// deliver frames at its full rate, each of them prepared and encoded. With twenty windows
    /// that kept the encoder, this app and the window server busy all the time, and everything
    /// that waits on them (clicks, raises, resizes) slow.
    /// So each capture is compared with the one before, by a hash of every row, and dropped if
    /// it's the same. When one differs the rows aren't all hashed (it stops at the first that
    /// differs), so what the last one looked like isn't known again until the hashes are
    /// rebuilt, at most every 250 ms: a window that keeps changing pays for one rebuild and
    /// one comparison in that time, one that doesn't for a comparison per capture, and those
    /// come less often after a second without a change. (All on captureQueue.)
    private var rowHashes: [UInt64] = []
    private var rowHashWidth = 0
    private var rowHashesValid = false
    private var rowHashesRebuiltAt: UInt64 = 0
    private var lastChangeAt: UInt64 = 0
    private var slowedForNoChange = false

    private func differsFromLast(_ pb: CVPixelBuffer) -> Bool {
        let differs = compareWithLast(pb)
        let now = DispatchTime.now().uptimeNanoseconds
        if differs { lastChangeAt = now }
        let slow = !differs && now - lastChangeAt > 1_000_000_000
        if slow != slowedForNoChange {
            slowedForNoChange = slow
            queue.async { [weak self] in
                guard let self, !self.stopped, self.unchanging != slow else { return }
                self.unchanging = slow
                self.applyFrameRate()
            }
        }
        return differs
    }

    private func compareWithLast(_ pb: CVPixelBuffer) -> Bool {
        guard CVPixelBufferGetPixelFormatType(pb) == kCVPixelFormatType_32BGRA,
              CVPixelBufferLockBaseAddress(pb, .readOnly) == kCVReturnSuccess else { return true }
        defer { CVPixelBufferUnlockBaseAddress(pb, .readOnly) }
        guard let base = CVPixelBufferGetBaseAddress(pb) else { return true }
        let w = CVPixelBufferGetWidth(pb), h = CVPixelBufferGetHeight(pb), stride = CVPixelBufferGetBytesPerRow(pb)
        let rowBytes = w * 4
        guard rowHashesValid, rowHashes.count == h, rowHashWidth == w else {
            let now = DispatchTime.now().uptimeNanoseconds
            guard now - rowHashesRebuiltAt > 250_000_000 else { return true }
            rowHashesRebuiltAt = now
            if rowHashes.count != h { rowHashes = [UInt64](repeating: 0, count: h) }
            rowHashes.withUnsafeMutableBufferPointer { out in
                for y in 0..<h { out[y] = Self.hashRow(base + y * stride, rowBytes) }
            }
            rowHashWidth = w
            rowHashesValid = true
            return true
        }
        let same = rowHashes.withUnsafeBufferPointer { last -> Bool in
            for y in 0..<h where Self.hashRow(base + y * stride, rowBytes) != last[y] { return false }
            return true
        }
        if !same { rowHashesValid = false }
        return !same
    }

    /// A fast hash of one row of pixels (two independent multiply chains, 16 bytes a step).
    @inline(__always)
    private static func hashRow(_ p: UnsafeRawPointer, _ n: Int) -> UInt64 {
        var a: UInt64 = 0x9E37_79B9_7F4A_7C15, b: UInt64 = 0xC2B2_AE3D_27D4_EB4F
        var i = 0
        while i + 16 <= n {
            a = (a ^ p.loadUnaligned(fromByteOffset: i, as: UInt64.self)) &* 0x0000_0100_0000_01B3
            b = (b ^ p.loadUnaligned(fromByteOffset: i + 8, as: UInt64.self)) &* 0x9E37_79B9_7F4A_7C15
            i += 16
        }
        while i + 4 <= n {
            a = (a ^ UInt64(p.loadUnaligned(fromByteOffset: i, as: UInt32.self))) &* 0x0000_0100_0000_01B3
            i += 4
        }
        return a ^ (b << 31 | b >> 33)
    }
    private var darkTitleBar = true
    /// Last corner tiles sent (hash), and the ones to send with the next encoded frame.
    private var cornersHash: UInt64 = 0
    private var mismatchLogs = 0
    private var loggedAttachments = false
    /// Corner radii measured across all windows (points → count). Standard macOS windows share
    /// one radius, so every window uses the most common measurement: individual measurements
    /// can be thrown off by what an app draws at its edge (Terminal, a browser).
    static var radiusVotes: [Int: Int] = [:]
    static var consensusRadiusPt: Int {
        radiusVotes.max { a, b in a.value == b.value ? a.key < b.key : a.value < b.value }?.key ?? 0
    }
    private var radiusSamples = 0
    private var measureCountdown = 0
    private var pendingCorners: (Int, Data)?

    // Stats
    private var statFrames = 0, statBytes = 0, statLatencySum: UInt64 = 0, statLatencyN = 0
    /// Every capture→present time this period (for percentiles).
    private var statLatencies: [UInt64] = []
    private var statEncodeSum: UInt64 = 0, statEncodeN = 0, statCaptureLagSum: UInt64 = 0, statCaptureLagN = 0
    private var statSince = DispatchTime.now()

    /// Menus, tooltips and other popups: sent as lossless BGRA with alpha (LZ4) instead of HEVC,
    /// so their rounded corners and translucency survive. They are small and short-lived.
    let lossless: Bool

    /// Set for the "Mac display" view: captures this whole display instead of a window, with
    /// none of the window treatment (corners, traffic lights, capture badge).
    let displayID: CGDirectDisplayID?
    private var scDisplay: SCDisplay?

    init(wid: UInt32, pointSize: CGSize, scale: CGFloat, lossless: Bool, out: MsgStream, queue: DispatchQueue,
         displayID: CGDirectDisplayID? = nil) {
        self.wid = wid
        self.displayID = displayID
        self.lossless = lossless
        self.pointSize = pointSize
        self.scale = scale
        self.out = out
        self.queue = queue
    }

    // MARK: lifecycle

    /// Windows looked up ahead of a subscription (see `prefetch`): the window list takes
    /// ScreenCaptureKit a few hundred milliseconds, which would otherwise delay a new window.
    private static var prefetched: [UInt32: (window: SCWindow, at: Date)] = [:]
    private static let prefetchLock = NSLock()

    /// Looks up a window that's about to be subscribed to (it just appeared).
    static func prefetch(_ wid: UInt32) {
        let t0 = Date()
        SCShareableContent.getExcludingDesktopWindows(false, onScreenWindowsOnly: true) { content, _ in
            guard let w = content?.windows.first(where: { $0.windowID == wid }) else { return }
            prefetchLock.withLock { prefetched[wid] = (w, Date()) }
            log(String(format: "window %u: looked up for capture in %.0f ms", wid, Date().timeIntervalSince(t0) * 1000))
        }
    }

    func start() {
        if displayID != nil { return startDisplay() }
        let cached = Self.prefetchLock.withLock { Self.prefetched.removeValue(forKey: wid) }
        if let cached, Date().timeIntervalSince(cached.at) < 5 {
            queue.async { [weak self] in
                guard let self, !self.stopped else { return }
                self.startCapture(cached.window)
            }
            return
        }
        SCShareableContent.getExcludingDesktopWindows(false, onScreenWindowsOnly: false) { [weak self] content, err in
            guard let self else { return }
            self.queue.async {
                guard !self.stopped else { return }
                guard let win = content?.windows.first(where: { $0.windowID == self.wid }) else {
                    log("stream \(self.wid): window not found (\(String(describing: err)))")
                    self.out.close()
                    return
                }
                // Keep pointSize: the tracker's size (from init or windowResized) is newer than
                // this snapshot, which can lag a resize we just requested.
                self.startCapture(win)
            }
        }
    }

    /// The display view: look the display up, then capture it.
    private func startDisplay() {
        SCShareableContent.getExcludingDesktopWindows(false, onScreenWindowsOnly: true) { [weak self] content, err in
            guard let self else { return }
            self.queue.async {
                guard !self.stopped, self.stream == nil else { return }
                guard let d = content?.displays.first(where: { $0.displayID == self.displayID }) else {
                    // No list at all: the capture service isn't back yet (see resumeAfterStop).
                    if content == nil { self.resumeAfterStop() } else { log("stream \(self.wid): display not found (\(String(describing: err)))"); self.out.close() }
                    return
                }
                self.scDisplay = d
                self.startCapture(nil, display: d)
            }
        }
    }

    /// Starts again from the same window or display (after a stop, or a capture that never
    /// delivered a frame).
    /// `still`: show a still of it while the capture starts (see startCapture).
    private func restartCapture(still: Bool = false) {
        if let d = scDisplay { startCapture(nil, display: d, still: still) } else if let w = scWindow { startCapture(w, still: still) }
    }

    /// `still`: send a still of the window while the capture starts (always, the first time).
    private func startCapture(_ win: SCWindow?, display: SCDisplay? = nil, still: Bool = false) {
        scWindow = win
        // Paused (not shown on Linux): no capture at all; resuming starts one with what's
        // remembered here. Every path that (re)starts a capture comes through here.
        guard !paused else { return }
        let filter: SCContentFilter
        if let display {
            filter = SCContentFilter(display: display, excludingWindows: [])
        } else if let win {
            filter = SCContentFilter(desktopIndependentWindow: win)
        } else { return }
        // A new capture's first frame always goes through (expectFirstFrame waits for it),
        // whatever the last capture's looked like.
        unchanging = false
        captureQueue.async { [weak self] in
            self?.rowHashesValid = false
            self?.rowHashesRebuiltAt = 0
            self?.slowedForNoChange = false
            self?.lastChangeAt = DispatchTime.now().uptimeNanoseconds
        }
        applySize(to: config)
        // BGRA with alpha so the rounded window corners can be filled in (see fillCorners);
        // the encoder converts to 4:2:0 in hardware.
        config.pixelFormat = kCVPixelFormatType_32BGRA
        config.colorSpaceName = CGColorSpace.sRGB
        config.minimumFrameInterval = CMTime(value: 1, timescale: frameRate)
        config.queueDepth = 4
        config.showsCursor = false
        config.ignoreShadowsSingleWindow = true
        // A shared window's capture otherwise gets its menus/popups composited in (scaled and
        // misplaced); we stream those as their own windows.
        config.includeChildWindows = false
        config.shouldBeOpaque = display != nil
        config.capturesAudio = false
        let s = SCStream(filter: filter, configuration: config, delegate: self)
        do {
            try s.addStreamOutput(self, type: .screen, sampleHandlerQueue: captureQueue)
        } catch {
            log("stream \(wid): addStreamOutput failed: \(error)")
            out.close(); return
        }
        stream = s
        // Counts this capture's frames (the still below only shows while there are none yet).
        framesSinceStart = 0
        // Starting a capture takes ScreenCaptureKit ~0.4 s; a still of the window takes a fraction
        // of that, so a new window shows straight away and goes live when the capture starts.
        // Not for popups: macOS fades them in, so a still catches one half-drawn and holds it
        // until the capture starts (it pops). Popups' captures start fast enough.
        if (still || !sentStill) && !lossless {
            sentStill = true
            let t0 = Date()
            SCScreenshotManager.captureSampleBuffer(contentFilter: filter, configuration: config) { [weak self] sb, _ in
                log(String(format: "stream %u: still taken in %.0f ms", self?.wid ?? 0, Date().timeIntervalSince(t0) * 1000))
                guard let self, let sb, let pb = CMSampleBufferGetImageBuffer(sb) else { return }
                self.queue.async {
                    guard !self.stopped, !self.showingPause, self.stream === s, self.framesSinceStart == 0, !ScreenLock.isLocked else { return }
                    self.process(pb, contentRect: nil, contentScale: 1, scaleFactor: self.scale, lagUs: 0, still: true)
                }
            }
        }
        if lossless {
            // Some popups get no frame at all from the live capture (one sitting still, a
            // "Save changes?" sheet; Safari's address-bar suggestions, even while they change):
            // after a moment with none, stills of it instead, taken ten times a second and sent
            // when they differ, for as long as the capture stays silent.
            queue.asyncAfter(deadline: .now() + 0.6) { [weak self] in
                self?.pollStills(s, filter: filter)
            }
        }
        s.startCapture { [weak self] err in
            guard let self else { return }
            self.queue.async {
                // Replaced while it was starting (a stop before the start completes doesn't
                // take): stop it now, or it keeps sending frames alongside its replacement.
                guard self.stream === s, !self.stopped else {
                    if err == nil { s.stopCapture { _ in } }
                    return
                }
                if let err {
                    log("stream \(self.wid): startCapture failed: \(err)")
                    self.stream = nil
                    self.resumeAfterStop()
                }
                else {
                    log("stream \(self.wid): capturing")
                    self.expectFirstFrame(from: s)
                }
            }
        }
    }

    private func pixelSize() -> (Int, Int) {
        // Even dimensions for 4:2:0.
        let w = Int((pointSize.width * scale).rounded()) & ~1
        let h = Int((pointSize.height * scale).rounded()) & ~1
        return (max(w, 16), max(h, 16))
    }

    private func applySize(to c: SCStreamConfiguration) {
        let (w, h) = pixelSize()
        c.width = w
        c.height = h
    }

    /// The window's size in points changed (either we resized it or the app did).
    func windowResized(to size: CGSize) {
        guard size != pointSize, let stream else { pointSize = size; return }
        pointSize = size
        if lossless, let win = scWindow {
            // Menus are static: after a reconfigure ScreenCaptureKit would not deliver a frame
            // until the content changes, leaving the old-size image. A fresh capture always
            // starts with a frame.
            stream.stopCapture { _ in }
            self.stream = nil
            startCapture(win)
            return
        }
        applySize(to: config)
        stream.updateConfiguration(config) { err in if let err { log("updateConfiguration: \(err)") } }
        expectFrameAfterResize()
    }

    /// A reconfigured capture sends nothing until the window changes. A window that had already
    /// redrawn at its new size by then (an idle one, resized by a tiling layout swapping tiles)
    /// stayed at its old picture on Linux, at the old size. So if no frame has come shortly
    /// after the last resize, the capture is started afresh, which always begins with one.
    private var resizeChecks = 0
    /// A shrunk capture was seen (and dropped) since the last proper frame.
    private var shrunkSeen = false

    private func expectFrameAfterResize() {
        resizeChecks += 1
        let check = resizeChecks, frames = framesSinceStart
        queue.asyncAfter(deadline: .now() + 0.35) { [weak self] in
            guard let self, !self.stopped, !self.paused, !self.showingPause, check == self.resizeChecks,
                  let s = self.stream, self.framesSinceStart == frames else { return }
            log("stream \(self.wid): no frame since its resize, starting the capture again")
            s.stopCapture { _ in }
            self.stream = nil
            self.forceKeyframe = true
            self.restartCapture(still: true)
        }
    }

    /// Captures ScreenCaptureKit delivered since the capture started, unchanged ones included.
    private var delivered = 0
    private var inputChecks = 0
    private var lastInputRestart: UInt64 = 0

    /// A capture can go quiet for good: after the virtual display was replaced (a computer
    /// switching in), several windows' captures delivered nothing again, ever, while clicks and
    /// keys still reached the apps, so the window looked frozen on Linux (Calculator read
    /// "7+278" on the Mac and "7+2" on Linux). Input almost always changes something on
    /// screen, so a click or key that is followed by no capture at all for 0.8 s means the
    /// capture is dead: it's started afresh (which always begins with a frame), at most once
    /// every 3 s per window. A live one delivers a frame for any change, so nothing restarts.
    func expectFrameAfterInput() {
        queue.async { [weak self] in
            guard let self, !self.stopped, !self.paused, !self.showingPause, self.stream != nil else { return }
            self.inputChecks += 1
            let check = self.inputChecks, before = self.delivered
            self.queue.asyncAfter(deadline: .now() + 0.8) { [weak self] in
                guard let self, !self.stopped, !self.paused, !self.showingPause, check == self.inputChecks,
                      let s = self.stream, self.delivered == before, !ScreenLock.isLocked else { return }
                let now = DispatchTime.now().uptimeNanoseconds
                guard now - self.lastInputRestart > 3_000_000_000 else { return }
                self.lastInputRestart = now
                log("stream \(self.wid): nothing captured after input, starting the capture again")
                s.stopCapture { _ in }
                self.stream = nil
                self.forceKeyframe = true
                self.startRetries = 0
                self.restartCapture(still: true)
            }
        }
    }

    /// The capture service was restarted (CaptureHealth): this capture belonged to the old one.
    func restartAfterServiceReset() {
        queue.async { [weak self] in
            guard let self, !self.stopped, !self.paused, !self.showingPause, let s = self.stream else { return }
            s.stopCapture { _ in }
            self.stream = nil
            self.forceKeyframe = true
            self.startRetries = 0
            self.restartCapture(still: true)
        }
    }

    /// When the last resume was asked for (host µs), until its first frame goes out (0: none due).
    private var resumedUs: UInt64 = 0

    /// Linux isn't showing the window (minimised, on another desktop): stop capturing it
    /// altogether. Only stopping encoding left ScreenCaptureKit delivering every change, each
    /// hashed and prepared for nothing: with many windows open, most of them hidden, that kept
    /// the host near full CPU. A stop rather than a slower capture: a capture started afresh
    /// always begins with a frame of the window as it is now, at its size and scale now, where
    /// a reconfigured one sends nothing until the window changes. Starting takes ~0.4 s, so a
    /// still (a fraction of that) is sent first. Not the Mac being locked: that's `showingPause`.
    func setPaused(_ p: Bool) {
        guard p != paused, !stopped else { return }
        paused = p
        if p {
            let unshown = resumedUs > 0 ? String(format: " (resumed %.0f ms ago, no frame sent yet)", Double(hostMicros() - resumedUs) / 1000) : ""
            resumedUs = 0
            refineTimer?.cancel()
            // A capture still starting stops itself when it finds it's been replaced.
            stream?.stopCapture { _ in }
            stream = nil
            // Out of date by the time it's shown again. The locked picture stays (it's current).
            if !showingPause { pendingBuffer = nil }
            log("stream \(wid): paused, capture stopped\(unshown)")
            return
        }
        // Logged with its first frame (sendFrame, sendLossless).
        forceKeyframe = true
        resumedUs = hostMicros()
        if showingPause {
            // Locked: captures show nothing, so the locked picture goes back up and the capture
            // starts at unlock (see setScreenLocked). The colour picture from before the lock
            // may be out of date by then: forget it, so unlocking starts a fresh capture.
            colourBeforeLock = nil
            if pendingBuffer == nil, let b = lastBuffer { pendingBuffer = b; pendingPrepared = true }
            pump()
            return
        }
        // Whatever was waiting is older than the still.
        pendingBuffer = nil
        resumeCapture()
    }

    /// Starts capturing again after a pause, with a still first, at the scale of the display
    /// the window is on now (it may have moved meanwhile; checkScale leaves a paused one be).
    /// Its size is already current: windowResized keeps `pointSize` while there's no capture.
    private func resumeCapture() {
        if let want = displayScale?(), want > 0, abs(want - scale) > 0.01 {
            log("stream \(wid): display scale \(scale) → \(want) (while paused)")
            scale = want
        }
        restartCapture(still: true)
    }

    /// Re-encode the last capture with fresh overlays (e.g. focus changed the traffic lights).
    /// The capture buffer still holds the previously painted lights, so this only runs the paint.
    func requestRepaint() {
        if encoding { repaintAfter = true; return }   // that buffer is being worked on
        guard pendingBuffer == nil, let b = lastBuffer else { return }
        eraseCaptureBadge(b, buttons: buttons, badge: badgeRect(), scale: scale, clean: cleanCorner)
        pendingBuffer = b
        pendingPrepared = true
        refinementsLeft = 2
        pump()
    }

    /// Corner tiles cut from `pb` (sent ahead of its frame), so they match what it shows.
    private func updateCorners(from pb: CVPixelBuffer) {
        let radius = Int((CGFloat(WindowStreamer.consensusRadiusPt) * scale).rounded())
        if let (t, tiles, hash) = cornerTiles(pb, radius: radius, scale: scale), hash != cornersHash {
            cornersHash = hash
            pendingCorners = (t, tiles)
        }
    }

    /// The colour frame shown before the Mac locked, put back when it unlocks (a static window
    /// sends no new frame by itself).
    private var colourBeforeLock: CVPixelBuffer?
    private var loggedFormat = false
    /// Encode 10-bit (smooth gradients), unless the "light" quality is chosen. Colours checked
    /// against the Mac with a known grey.
    static var tenBit: Bool { settings.config.quality != "light" }
    /// What the current encoder was made for.
    private var encoderTenBit = true

    /// The size to encode a w×h picture at. A new encoder's first frame takes ~30 ms longer
    /// than one from a running encoder (more with several windows at once on the shared hardware
    /// encoder), and a tiling relayout or a drag resizes windows over and over, often back and
    /// forth. So while a window is being resized its encoder stays as it is if the picture still
    /// fits (in at least half its area), and grows with room to spare. The picture goes in the
    /// top-left corner; the frame tells Linux its real size. Once the size has held for a few
    /// seconds the encoder fits it again (encoding the spare room costs a little every frame).
    /// (Only for 10-bit: that picture is converted into a buffer of our own anyway.)
    private func codedSize(_ w: Int32, _ h: Int32) -> (Int32, Int32) {
        guard Self.tenBit, displayID == nil else { return (w, h) }
        if (w, h) != pictureSize {
            pictureSize = (w, h)
            pictureSince = .now()
        }
        func up(_ v: Int32, room: Bool) -> Int32 { min(8192, (v + (room ? v / 8 : 0) + 63) / 64 * 64) }
        let fitted = (up(w, room: false), up(h, room: false))
        let (ew, eh) = encoderSize
        let settled = DispatchTime.now().uptimeNanoseconds - pictureSince.uptimeNanoseconds > 3_000_000_000
        if encoder != nil, encoderTenBit, w <= ew, h <= eh, Int64(w) * Int64(h) * 2 >= Int64(ew) * Int64(eh),
           !settled || encoderSize == fitted {
            return encoderSize
        }
        // A new window, or one whose size has settled, gets its own size (in the encoder's
        // 64-pixel blocks); one being resized more.
        return settled || encoder == nil ? fitted : (up(w, room: true), up(h, room: true))
    }
    private var pictureSize: (Int32, Int32) = (0, 0)
    private var pictureSince = DispatchTime.now()
    private var transfer: VTPixelTransferSession?
    private var yuvPool: CVPixelBufferPool?
    private var yuvPoolSize = (0, 0)
    /// Picture and buffer size the conversion is set up for.
    private var transferArea: [Int] = []

    /// BGRA → 10-bit 4:2:0 limited-range BT.709 YUV (tagged sRGB like the capture, so no
    /// brightness curve is applied on the way).
    private func toVideoRange10(_ pb: CVPixelBuffer, size: (Int32, Int32)) -> CVPixelBuffer? {
        let pw = CVPixelBufferGetWidth(pb), ph = CVPixelBufferGetHeight(pb)
        let (w, h) = (Int(size.0), Int(size.1))
        guard pw <= w, ph <= h else { return nil }
        if transfer == nil {
            var t: VTPixelTransferSession?
            guard VTPixelTransferSessionCreate(allocator: nil, pixelTransferSessionOut: &t) == noErr, let t else { return nil }
            VTSessionSetProperty(t, key: kVTPixelTransferPropertyKey_DestinationYCbCrMatrix, value: kCVImageBufferYCbCrMatrix_ITU_R_709_2)
            VTSessionSetProperty(t, key: kVTPixelTransferPropertyKey_DestinationColorPrimaries, value: kCVImageBufferColorPrimaries_ITU_R_709_2)
            VTSessionSetProperty(t, key: kVTPixelTransferPropertyKey_DestinationTransferFunction, value: kCVImageBufferTransferFunction_sRGB)
            VTSessionSetProperty(t, key: kVTPixelTransferPropertyKey_ScalingMode, value: kVTScalingMode_CropSourceToCleanAperture)
            transfer = t
        }
        if yuvPool == nil || yuvPoolSize != (w, h) {
            let attrs: [CFString: Any] = [
                kCVPixelBufferPixelFormatTypeKey: kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange,
                kCVPixelBufferWidthKey: w, kCVPixelBufferHeightKey: h,
                kCVPixelBufferIOSurfacePropertiesKey: [:] as CFDictionary,
            ]
            var pool: CVPixelBufferPool?
            CVPixelBufferPoolCreate(nil, nil, attrs as CFDictionary, &pool)
            yuvPool = pool
            yuvPoolSize = (w, h)
        }
        guard let pool = yuvPool, let t = transfer else { return nil }
        var out: CVPixelBuffer?
        guard CVPixelBufferPoolCreatePixelBuffer(nil, pool, &out) == kCVReturnSuccess, let out else { return nil }
        // The picture goes in the top-left corner, unscaled, and the conversion fills the rest
        // with black: a clean aperture that size there (its offsets are from the centre).
        if transferArea != [pw, ph, w, h] {
            transferArea = [pw, ph, w, h]
            let aperture: [CFString: Any] = [
                kCVImageBufferCleanApertureWidthKey: pw, kCVImageBufferCleanApertureHeightKey: ph,
                kCVImageBufferCleanApertureHorizontalOffsetKey: -(w - pw) / 2,
                kCVImageBufferCleanApertureVerticalOffsetKey: -(h - ph) / 2,
            ]
            VTSessionSetProperty(t, key: kVTPixelTransferPropertyKey_DestinationCleanAperture, value: aperture as CFDictionary)
        }
        CVBufferSetAttachment(out, kCVImageBufferYCbCrMatrixKey, kCVImageBufferYCbCrMatrix_ITU_R_709_2, .shouldPropagate)
        CVBufferSetAttachment(out, kCVImageBufferColorPrimariesKey, kCVImageBufferColorPrimaries_ITU_R_709_2, .shouldPropagate)
        CVBufferSetAttachment(out, kCVImageBufferTransferFunctionKey, kCVImageBufferTransferFunction_sRGB, .shouldPropagate)
        guard VTPixelTransferSessionTransferImage(t, from: pb, to: out) == noErr else { return nil }
        // The conversion tags the buffer with that aperture; the encoder must see the whole.
        CVBufferRemoveAttachment(out, kCVImageBufferCleanApertureKey)
        return out
    }
    /// The paused picture is up: captures are dropped until unlocked.
    private var showingPause = false

    /// The Mac locked or unlocked: show the last frame greyed out with a pause symbol meanwhile.
    func setScreenLocked(_ locked: Bool, placeholder: Bool = false) {
        queue.async { [weak self] in
            guard let self, !self.stopped, !self.lossless else { return }
            if locked {
                guard !self.showingPause else { return }
                self.showingPause = true
                if !placeholder, let b = self.lastBuffer {
                    guard let grey = pausedCopy(of: b, scale: self.scale, visible: self.captureContent) else { return }
                    self.colourBeforeLock = b
                    self.updateCorners(from: grey)
                    self.pendingBuffer = grey
                } else {
                    // Locked before the first frame: a dark placeholder with the pause symbol.
                    let (w, h) = self.pixelSize()
                    guard let blank = solidBuffer(width: w, height: h, gray: 0x3A) else { return }
                    if let icon = self.appIcon { drawCentred(icon, in: blank, size: 72 * self.scale, visible: self.captureContent) }
                    guard let grey = pausedCopy(of: blank, scale: self.scale, visible: self.captureContent) else { return }
                    self.updateCorners(from: grey)
                    self.pendingBuffer = grey
                }
            } else {
                guard self.showingPause else { return }
                self.showingPause = false
                guard let b = self.colourBeforeLock else {
                    self.pendingBuffer = nil
                    // Only a placeholder was shown: a fresh capture starts with a real frame.
                    if let stream = self.stream {
                        stream.stopCapture { _ in }
                        self.stream = nil
                        self.restartCapture()
                    } else if !self.paused {
                        // Resumed while locked (see setPaused): no capture was started then.
                        self.resumeCapture()
                    }
                    return
                }
                self.colourBeforeLock = nil
                self.updateCorners(from: b)
                // Covers windows that don't change (fresh captures replace it as they come).
                self.pendingBuffer = b
            }
            self.forceKeyframe = true
            self.refinementsLeft = 2
            self.pump()
        }
    }

    func requestKeyframe() {
        forceKeyframe = true
        if pendingBuffer == nil, let b = lastBuffer { pendingBuffer = b }
        pump()
    }

    func ack(_ captureUs: UInt64) {
        if let i = inFlight.firstIndex(of: captureUs) {
            inFlight.removeSubrange(...i)
            let now = hostMicros()
            if now > captureUs {
                statLatencySum += now - captureUs; statLatencyN += 1
                statLatencies.append(now - captureUs)
            }
        }
        pump()
    }

    func stop() {
        guard !stopped else { return }
        stopped = true
        refineTimer?.cancel()
        stream?.stopCapture { _ in }
        stream = nil
        // Dropped on the stream's own queue: it waits for a frame still inside the encoder.
        if let e = encoder { work.async { VTCompressionSessionInvalidate(e) } }
        encoder = nil
    }

    // MARK: capture

    func stream(_ stream: SCStream, didOutputSampleBuffer sb: CMSampleBuffer, of type: SCStreamOutputType) {
        guard type == .screen, sb.isValid,
              let attachments = CMSampleBufferGetSampleAttachmentsArray(sb, createIfNecessary: false) as? [[SCStreamFrameInfo: Any]],
              let raw = attachments.first?[.status] as? Int, SCFrameStatus(rawValue: raw) == .complete,
              let pb = CMSampleBufferGetImageBuffer(sb) else { return }
        // A locked Mac captures as blank: keep showing the last real frame instead.
        guard !ScreenLock.isLocked else { return }
        // Counted before the unchanged ones are dropped: the capture is alive (see
        // expectFrameAfterInput).
        queue.async { [weak self] in if let self, stream === self.stream { self.delivered &+= 1 } }
        guard differsFromLast(pb) else { return }
        // Where the window's content actually is in the buffer: when it doesn't fill it (the
        // configured size is stale), ScreenCaptureKit pads with black.
        let info = attachments.first ?? [:]
        let contentRect = (info[.contentRect] as? NSDictionary).flatMap { CGRect(dictionaryRepresentation: $0 as CFDictionary) }
        let contentScale = info[.contentScale] as? CGFloat ?? 1
        // How long ScreenCaptureKit took to hand us the frame (its timestamp is host time).
        let pts = CMSampleBufferGetPresentationTimeStamp(sb)
        let lagUs: UInt64 = {
            guard pts.isValid else { return 0 }
            let now = CMClockGetTime(CMClockGetHostTimeClock())
            let d = CMTimeGetSeconds(CMTimeSubtract(now, pts))
            return d > 0 && d < 1 ? UInt64(d * 1e6) : 0
        }()
        let scaleFactor = info[.scaleFactor] as? CGFloat ?? 1
        queue.async { [weak self] in
            // Only the current capture's frames (a replaced one can still deliver a few).
            guard let self, !self.stopped, !self.showingPause, stream === self.stream else { return }
            // The window outgrew the capture's size and macOS shrank it into it, with black
            // around (a TV window showed a black band at its bottom on Linux). Not sent: the
            // capture is started afresh at the window's size if no proper frame follows soon.
            if self.scDisplay == nil && contentScale < 0.98 {
                if !self.shrunkSeen {
                    self.shrunkSeen = true
                    log(String(format: "stream %u: capture shrunk to %.0f%% (window outgrew it): dropped", self.wid, contentScale * 100))
                    self.expectFrameAfterResize()
                }
                return
            }
            self.shrunkSeen = false
            self.process(pb, contentRect: contentRect, contentScale: contentScale, scaleFactor: scaleFactor, lagUs: lagUs)
        }
    }

    /// A captured frame (from the stream, or the still that stands in until it starts), on
    /// `queue`: prepared and handed to the encoder.
    private func process(_ pb: CVPixelBuffer, contentRect: CGRect?, contentScale: CGFloat, scaleFactor: CGFloat, lagUs: UInt64,
                         still: Bool = false) {
        if !still {
            framesSinceStart += 1
            stopRetries = 0
        }
        if lagUs > 0 { statCaptureLagSum += lagUs; statCaptureLagN += 1 }
        if !loggedAttachments {
            loggedAttachments = true
            let a = CVBufferCopyAttachments(pb, .shouldPropagate) as? [String: Any] ?? [:]
            log("stream \(wid): buffer colour tags \(a.filter { $0.key.contains("Color") || $0.key.contains("Transfer") || $0.key.contains("Matrix") || $0.key.contains("Primaries") })")
        }
        captureContent = contentRect.map { CGSize(width: $0.width * scaleFactor, height: $0.height * scaleFactor) }
        if let r = contentRect {
            let bw = CGFloat(CVPixelBufferGetWidth(pb)), bh = CGFloat(CVPixelBufferGetHeight(pb))
            let cw = r.width * scaleFactor, ch = r.height * scaleFactor
            if abs(cw - bw) > 2 || abs(ch - bh) > 2 {
                mismatchLogs += 1
                if mismatchLogs <= 5 {
                    log(String(format: "stream %u: content %.0fx%.0f (rect %@ contentScale %.2f scaleFactor %.1f) in buffer %.0fx%.0f, pointSize %@",
                               wid, cw, ch, NSStringFromRect(r), contentScale, scaleFactor, bw, bh, NSStringFromSize(pointSize)))
                }
            }
        }
        // Prepared only if it's encoded: a newer capture may replace it while the encoder is busy.
        pendingBuffer = pb
        pendingPrepared = false
        refinementsLeft = 2
        pump()
    }

    /// What preparing a frame needs from the streamer's state, taken on `queue`.
    private struct Prep {
        let buttons: [CGRect], badge: CGRect?, scale: CGFloat, content: CGSize?, radiusPx: Int
        let detect: Bool, measure: Bool
        let clean: CleanCorner
        let noBadge: Bool
    }

    /// And what it found, applied back on `queue`.
    private struct Prepared {
        var dark: Bool?
        var detected: [CGRect]?
        var radiusPx: Int?
        var corners: (Int, Data, UInt64)?
    }

    /// Paints the overlays over a capture and cuts its corners (on `work`): order matters:
    /// erase the capture badge first (it overlaps the top-left corner), cut the transparent
    /// corner tiles, then flatten the corners for HEVC.
    private static func prepare(_ pb: CVPixelBuffer, _ p: Prep) -> Prepared {
        var r = Prepared()
        var buttons = p.buttons
        // Apps that hide the standard buttons and draw their own: find them in the pixels
        // (a few times a second until found).
        if p.detect, let found = detectTrafficLights(pb, scale: p.scale) {
            r.detected = found
            buttons = found
        }
        r.dark = eraseCaptureBadge(pb, buttons: buttons, badge: p.badge, scale: p.scale, clean: p.clean, fresh: true,
                                   noBadge: p.noBadge)
        // A few radius measurements per window (not every frame) feed the shared vote.
        if p.measure { r.radiusPx = measureCornerRadius(pb, scale: p.scale) }
        r.corners = cornerTiles(pb, radius: p.radiusPx, scale: p.scale, content: p.content)
        fillCorners(pb, radiusPx: Int(28 * p.scale))
        fillEdges(pb, ring: max(2, Int(p.scale)))
        // Translucent areas (vibrancy sidebars etc.) arrive premultiplied; the encoder
        // drops alpha, which would leave them darkened. Restore their real colour.
        // Most windows are fully opaque: a sparse look (1 pixel in 64) decides whether
        // the full pass is needed.
        if hasTranslucency(pb, skip: Int(28 * p.scale)) { unpremultiply(pb) }
        return r
    }

    private func apply(_ r: Prepared) {
        if let found = r.detected, !buttons.contains(where: { $0.width > 0 }) {
            log("stream \(wid): found app-drawn traffic lights at \(found.map { "\(Int($0.minX)),\(Int($0.minY))" })")
            buttons = found
            onDetectedButtons?(found)
        }
        if let dark = r.dark { darkTitleBar = dark }
        if let px = r.radiusPx {
            radiusSamples += 1
            let pt = Int((CGFloat(px) / scale).rounded())
            WindowStreamer.radiusVotes[pt, default: 0] += 1
            log("stream \(wid): corner radius sample \(pt)pt → consensus \(WindowStreamer.consensusRadiusPt)pt")
        }
        if let (t, tiles, hash) = r.corners, hash != cornersHash {
            cornersHash = hash
            pendingCorners = (t, tiles)
        }
    }

    func stream(_ stream: SCStream, didStopWithError error: Error) {
        log("stream \(wid) stopped: \(error)")
        queue.async { [weak self] in
            guard let self, !self.stopped, stream === self.stream else { return }
            self.stream = nil
            self.resumeAfterStop()
        }
    }

    /// The capture died under us (typically macOS's capture service, replayd, crashing and
    /// taking every stream with it): start again once it's back, if the window still exists.
    private var stopRetries = 0

    private func resumeAfterStop() {
        guard stopRetries < 5 else { log("stream \(wid): giving up after repeated stops"); out.close(); return }
        stopRetries += 1
        if displayID != nil {
            queue.asyncAfter(deadline: .now() + Double(stopRetries)) { [weak self] in
                self?.forceKeyframe = true
                self?.startDisplay()
            }
            return
        }
        queue.asyncAfter(deadline: .now() + Double(stopRetries)) { [weak self] in
            SCShareableContent.getExcludingDesktopWindows(false, onScreenWindowsOnly: false) { content, _ in
                guard let self else { return }
                self.queue.async {
                    guard !self.stopped, self.stream == nil else { return }
                    guard let win = content?.windows.first(where: { $0.windowID == self.wid }) else {
                        // No list at all: the service isn't back yet. No window: it's gone.
                        if content == nil { self.resumeAfterStop() } else { self.out.close() }
                        return
                    }
                    log("stream \(self.wid): resuming capture")
                    self.forceKeyframe = true
                    self.startCapture(win)
                }
            }
        }
    }

    // MARK: encode

    /// The last still sent by pollStills (a sample of its pixels), to send only changes.
    private var lastStillHash: UInt64 = 0
    private var pollingStills = false

    /// Stills of a popup whose live capture delivers nothing (see startCapture), every 100 ms
    /// while that lasts; each one sent only when it differs from the last.
    private func pollStills(_ s: SCStream, filter: SCContentFilter) {
        guard !stopped, stream === s, framesSinceStart == 0 else { pollingStills = false; return }
        if !pollingStills {
            pollingStills = true
            lastStillHash = 0
            log("stream \(wid): no frame from the capture: stills of it instead")
        }
        SCScreenshotManager.captureSampleBuffer(contentFilter: filter, configuration: config) { [weak self] sb, _ in
            guard let self else { return }
            let pb = sb.flatMap { CMSampleBufferGetImageBuffer($0) }
            self.queue.async {
                guard !self.stopped, self.stream === s, self.framesSinceStart == 0 else { self.pollingStills = false; return }
                if let pb, !self.showingPause, !ScreenLock.isLocked {
                    let h = Self.sampleHash(pb)
                    if h != self.lastStillHash {
                        self.lastStillHash = h
                        self.process(pb, contentRect: nil, contentScale: 1, scaleFactor: self.scale, lagUs: 0, still: true)
                    }
                }
                self.queue.asyncAfter(deadline: .now() + 0.1) { [weak self] in
                    self?.pollStills(s, filter: filter)
                }
            }
        }
    }

    /// A quick fingerprint of a picture: every 7th byte of it (enough to tell a change).
    private static func sampleHash(_ pb: CVPixelBuffer) -> UInt64 {
        guard CVPixelBufferLockBaseAddress(pb, .readOnly) == kCVReturnSuccess else { return 0 }
        defer { CVPixelBufferUnlockBaseAddress(pb, .readOnly) }
        guard let base = CVPixelBufferGetBaseAddress(pb)?.assumingMemoryBound(to: UInt8.self) else { return 0 }
        let n = CVPixelBufferGetBytesPerRow(pb) * CVPixelBufferGetHeight(pb)
        var h: UInt64 = 0xcbf2_9ce4_8422_2325
        var i = 0
        while i < n { h = (h ^ UInt64(base[i])) &* 0x100_0000_01b3; i += 7 }
        return h ^ UInt64(n)
    }

    /// Encodes the pending capture if flow control allows.
    private func pump() {
        // A frame lost on the way (datagrams) is never acknowledged: don't wait for it forever.
        let now = hostMicros()
        inFlight.removeAll { now > $0 && now - $0 > 300_000 }
        // An encoder dropped mid-frame may never call back: don't wait on it for ever.
        if encoding, DispatchTime.now().uptimeNanoseconds - encodingSince.uptimeNanoseconds > 300_000_000 { encoding = false }
        guard !stopped, !paused, !encoding, !makingEncoder, inEncoder == 0, inFlight.count < maxInFlight,
              let pb = pendingBuffer else { return }
        pendingBuffer = nil
        if lossless { sendLossless(pb); return }
        let w = Int32(CVPixelBufferGetWidth(pb)), h = Int32(CVPixelBufferGetHeight(pb))
        let coded = codedSize(w, h)
        let tenBit = Self.tenBit
        if encoder == nil || encoderSize != coded || encoderTenBit != tenBit {
            // Made (and the old one dropped) on this stream's own queue: both wait for the
            // system's encoder service, which answers late when many windows are encoding, and
            // on the session queue all input waited with them. The frame goes in once it's there.
            let old = encoder
            encoder = nil
            encoderSize = coded
            encoderTenBit = tenBit
            makingEncoder = true
            pendingBuffer = pb
            work.async { [weak self] in
                if let old { VTCompressionSessionInvalidate(old) }
                let made = Self.makeEncoder(width: coded.0, height: coded.1, tenBit: tenBit)
                guard let self else { if let made { VTCompressionSessionInvalidate(made) }; return }
                self.queue.async {
                    self.makingEncoder = false
                    if self.stopped {
                        if let made { self.work.async { VTCompressionSessionInvalidate(made) } }
                        return
                    }
                    self.encoder = made
                    self.forceKeyframe = true
                    // Not made (the service refused): the next frame tries again.
                    if made != nil { self.pump() }
                }
            }
            return
        }
        let prepare = !pendingPrepared && displayID == nil
        var prep: Prep?
        if prepare {
            var detect = false
            if !buttons.contains(where: { $0.width > 0 }), onDetectedButtons != nil {
                detectCountdown -= 1
                if detectCountdown <= 0 { detectCountdown = 20; detect = true }
            }
            measureCountdown -= 1
            let measure = radiusSamples < 5 && measureCountdown <= 0
            if measure { measureCountdown = 15 }
            let badge = badgeRect()
            prep = Prep(buttons: buttons, badge: badge, scale: scale, content: captureContent,
                        radiusPx: Int((CGFloat(WindowStreamer.consensusRadiusPt) * scale).rounded()), detect: detect, measure: measure,
                        clean: cleanCorner, noBadge: badge == nil && badgeAbsent())
        }
        lastBuffer = pb
        lastEncodeAt = .now()
        encoding = true
        encodingSince = .now()
        let startUs = hostMicros()
        work.async { [weak self] in
            guard let self else { return }
            let t0 = DispatchTime.now().uptimeNanoseconds
            let result = prep.map { Self.prepare(pb, $0) }
            let ms = Double(DispatchTime.now().uptimeNanoseconds - t0) / 1e6
            if ms > 3 { log(String(format: "stream %u: frame prep took %.1f ms", self.wid, ms)) }
            // 10-bit goes in as limited-range YUV we convert ourselves: from BGRA the encoder
            // writes full-range 10-bit, which KWin shows as limited whatever it's tagged (too dark).
            let input = tenBit ? (self.toVideoRange10(pb, size: coded) ?? pb) : pb
            self.queue.async {
                if let result { self.apply(result) }
                self.encode(input, w: w, h: h, startUs: startUs)
            }
        }
    }

    /// When the window goes idle, re-encode the last frame a couple of times: the rate controller
    /// spends the spare bits refining it, so static content converges to near-lossless.
    /// A new capture normally starts with a frame, but not always (seen with a browser right after
    /// the virtual display came back): if none has come within a second, start again (at most
    /// three times). A one-off check, nothing keeps running.
    private var framesSinceStart = 0
    /// A still was sent ahead of the first capture (once per window).
    private var sentStill = false
    private var startRetries = 0

    private func expectFirstFrame(from s: SCStream) {
        let win = scWindow
        framesSinceStart = 0
        queue.asyncAfter(deadline: .now() + 1) { [weak self] in
            guard let self, !self.stopped, self.stream === s, self.framesSinceStart == 0 else {
                self?.startRetries = 0
                return
            }
            // A popup the capture won't feed is shown by stills (pollStills): restarting the
            // capture would only make it flicker.
            if self.lossless { return }
            guard self.startRetries < 3 else {
                log("stream \(self.wid): still no frame after 3 restarts")
                CaptureHealth.captureGaveNothing()
                return
            }
            self.startRetries += 1
            log("stream \(self.wid): no first frame, starting the capture again")
            s.stopCapture { _ in }
            self.stream = nil
            self.forceKeyframe = true
            guard let win else { return self.restartCapture() }
            // A fresh look-up: the window as ScreenCaptureKit saw it when it first appeared (an
            // app still opening it) can stay frameless however often it's restarted.
            SCShareableContent.getExcludingDesktopWindows(false, onScreenWindowsOnly: false) { [weak self] content, _ in
                guard let self else { return }
                self.queue.async {
                    guard !self.stopped, self.stream == nil else { return }
                    self.startCapture(content?.windows.first(where: { $0.windowID == self.wid }) ?? win)
                }
            }
        }
    }

    /// The window moved, or the displays changed: capture at the scale of the display it's on
    /// now (e.g. the virtual HiDPI display came or went), rather than padding with black or
    /// losing detail.
    func checkScale() {
        queue.async { [weak self] in
            // Paused, there's no capture: resumeCapture looks the scale up when it starts one
            // (until then the last picture, at the old scale, may still be sent again).
            guard let self, let want = self.displayScale?(), want > 0, abs(want - self.scale) > 0.01,
                  let stream = self.stream, !self.stopped else { return }
            log("stream \(self.wid): display scale \(self.scale) → \(want)")
            self.scale = want
            // A fresh capture, not a reconfigure: after a reconfigure ScreenCaptureKit sends
            // nothing until the window's content changes, so a window sitting still (native apps
            // don't redraw on moving display) would keep its old-scale picture.
            if let win = self.scWindow {
                stream.stopCapture { _ in }
                self.stream = nil
                self.forceKeyframe = true
                self.startCapture(win)
            }
        }
    }

    /// When the window goes idle, re-encode the last frame a couple of times: the rate controller
    /// spends the spare bits refining it, so static content converges to near-lossless. One
    /// timer after each frame, not a running one.
    private func scheduleRefine() {
        refineTimer?.cancel()
        guard refinementsLeft > 0, !lossless else { return }
        let t = DispatchSource.makeTimerSource(queue: queue)
        // Refining is for spare encoder time. When the last frame took long to encode (many
        // windows changing at once), wait a second instead: three encodes per change kept the
        // encoder full, and every window's frames came late.
        t.schedule(deadline: .now() + .milliseconds(lastEncodeUs > 60_000 ? 1000 : 130))
        t.setEventHandler { [weak self] in
            guard let self, !self.stopped, self.refinementsLeft > 0, self.pendingBuffer == nil, let b = self.lastBuffer else { return }
            self.refinementsLeft -= 1
            self.pendingBuffer = b
            self.pump()
        }
        t.resume()
        refineTimer = t
    }

    /// Stats every 5 s of streaming, written as frames go out.
    private func logStatsIfDue() {
        let el = Double(DispatchTime.now().uptimeNanoseconds - statSince.uptimeNanoseconds) / 1e9
        guard el >= 5, statFrames > 0 else { return }
        let lat = statLatencyN > 0 ? Double(statLatencySum) / Double(statLatencyN) / 1000 : -1
        let enc = statEncodeN > 0 ? Double(statEncodeSum) / Double(statEncodeN) / 1000 : -1
        let lag = statCaptureLagN > 0 ? Double(statCaptureLagSum) / Double(statCaptureLagN) / 1000 : -1
        let sorted = statLatencies.sorted()
        func pct(_ p: Double) -> Double { sorted.isEmpty ? -1 : Double(sorted[min(sorted.count - 1, Int(Double(sorted.count) * p))]) / 1000 }
        log(String(format: "stream %u: %.1f fps, %.1f Mbit/s, capture→present %.1f ms (p50 %.1f, p95 %.1f, max %.1f; capture lag %.1f, encode %.1f, in flight %d)", wid,
                   Double(statFrames) / el, Double(statBytes) * 8 / el / 1e6, lat, pct(0.5), pct(0.95), pct(1), lag, enc, inFlight.count))
        statLatencies.removeAll(keepingCapacity: true)
        statFrames = 0; statBytes = 0; statLatencySum = 0; statLatencyN = 0; statSince = .now()
        statEncodeSum = 0; statEncodeN = 0; statCaptureLagSum = 0; statCaptureLagN = 0
    }

    /// Sends the window's transparent corner tiles (ahead of the frame they belong to).
    private func flushCorners() {
        guard let (t, tiles) = pendingCorners else { return }
        pendingCorners = nil
        var out = [UInt8](repeating: 0, count: tiles.count + tiles.count / 255 + 64)
        let n = tiles.withUnsafeBytes { src in
            compression_encode_buffer(&out, out.count, src.bindMemory(to: UInt8.self).baseAddress!, tiles.count,
                                      nil, COMPRESSION_LZ4_RAW)
        }
        var m = MsgWriter(.corners)
        m.u16(UInt16(t))
        m.bytes(Data(out[0..<n]))
        out_send(m, bytes: n)
    }

    /// Hands a prepared frame to the encoder (on `queue`). `startUs`: when its preparation
    /// began, so the latency figures cover that too.
    private func encode(_ input: CVPixelBuffer, w: Int32, h: Int32, startUs: UInt64) {
        guard !stopped, let enc = encoder else { encoding = false; return }
        flushCorners()
        scheduleRefine()
        logStatsIfDue()
        let captureUs = startUs
        var props: CFDictionary?
        if forceKeyframe {
            props = [kVTEncodeFrameOptionKey_ForceKeyFrame: true] as CFDictionary
            forceKeyframe = false
        }
        inFlight.append(captureUs)
        let pts = CMTime(value: CMTimeValue(captureUs), timescale: 1_000_000)
        // The call itself waits for the system's encoder service (a round trip per frame, a
        // second or more when twenty windows are encoding): on the session queue every click
        // and pointer move waited behind every window's frames. On this stream's own queue only
        // this stream waits; `inEncoder` keeps further frames from piling up behind it.
        inEncoder += 1
        work.async { [weak self] in
            VTCompressionSessionEncodeFrame(enc, imageBuffer: input, presentationTimeStamp: pts, duration: .invalid,
                                            frameProperties: props, infoFlagsOut: nil) { [weak self] status, _, sb in
                self?.encoderOutput(status, sb, captureUs: captureUs, w: w, h: h)
            }
            self?.queue.async {
                guard let self else { return }
                self.inEncoder -= 1
                self.pump()
            }
        }
    }

    /// What the encoder made of a frame (called on one of its own threads).
    private func encoderOutput(_ status: OSStatus, _ sb: CMSampleBuffer?, captureUs: UInt64, w: Int32, h: Int32) {
        guard status == noErr, let sb else {
            // No frame and no error: the encoder dropped it (real-time mode) — the stream
            // stays intact. A real error needs a fresh start (keyframe).
            if status != noErr { log("encode error \(status)") }
            self.queue.async {
                self.inFlight.removeAll { $0 == captureUs }
                if status != noErr { self.forceKeyframe = true }
                self.encoded()
            }
            return
        }
        let (data, key) = annexB(sb)
        // The low-latency encoder emits full-range YUV whatever the input; tell the client.
        if !self.loggedFormat, let fd = CMSampleBufferGetFormatDescription(sb) {
            self.loggedFormat = true
            let ext = CMFormatDescriptionGetExtensions(fd) as? [String: Any] ?? [:]
            let keys = ["FullRangeVideo", "BitsPerComponent", "Depth", "CVImageBufferTransferFunction",
                        "CVImageBufferYCbCrMatrix", "CVImageBufferColorPrimaries"]
            log("stream \(self.wid): encoded format \(keys.compactMap { k in ext[k].map { "\(k)=\($0)" } })")
        }
        let fullRange = CMSampleBufferGetFormatDescription(sb).flatMap {
            CMFormatDescriptionGetExtension($0, extensionKey: kCMFormatDescriptionExtension_FullRangeVideo) as? Bool
        } ?? false
        let encodeUs = hostMicros() - captureUs
        self.queue.async {
            self.statEncodeSum += encodeUs; self.statEncodeN += 1
            self.lastEncodeUs = encodeUs
            self.sendFrame(captureUs: captureUs, w: w, h: h, key: key, fullRange: fullRange, data: data)
            self.encoded()
        }
    }

    /// The encoder is free again: the newest capture (or a wanted repaint) goes in.
    private func encoded() {
        encoding = false
        if repaintAfter { repaintAfter = false; requestRepaint() }
        pump()
    }

    private func sendLossless(_ pb: CVPixelBuffer) {
        lastBuffer = pb
        refinementsLeft = 0
        guard CVPixelBufferLockBaseAddress(pb, .readOnly) == kCVReturnSuccess else { return }
        defer { CVPixelBufferUnlockBaseAddress(pb, .readOnly) }
        guard let base = CVPixelBufferGetBaseAddress(pb)?.assumingMemoryBound(to: UInt8.self) else { return }
        let w = CVPixelBufferGetWidth(pb), h = CVPixelBufferGetHeight(pb), stride = CVPixelBufferGetBytesPerRow(pb)
        let rowBytes = w * 4
        var packed = [UInt8](repeating: 0, count: rowBytes * h)
        packed.withUnsafeMutableBufferPointer { dst in
            for y in 0..<h { memcpy(dst.baseAddress! + y * rowBytes, base + y * stride, rowBytes) }
        }
        var out = [UInt8](repeating: 0, count: packed.count + packed.count / 255 + 64)
        let n = compression_encode_buffer(&out, out.count, packed, packed.count, nil, COMPRESSION_LZ4_RAW)
        guard n > 0 else { log("lz4 failed for \(wid)"); return }
        let captureUs = hostMicros()
        inFlight.append(captureUs)
        lastEncodeAt = .now()
        scheduleRefine()
        logStatsIfDue()
        var m = MsgWriter(.frame)
        m.u64(captureUs)
        m.u16(UInt16(w)); m.u16(UInt16(h))
        m.u8(0b11 | (UInt8(min(15, max(1, scale.rounded()))) << 4))   // keyframe | lossless | px/pt
        m.bytes(Data(out[0..<n]))
        out_send(m, bytes: n)
        logResumedIfDue(captureUs: captureUs)
    }

    private func out_send(_ m: MsgWriter, bytes: Int) {
        out.send(m)
        statFrames += 1
        statBytes += bytes
    }

    /// The first frame since a resume went out (one prepared before the pause doesn't count).
    private func logResumedIfDue(captureUs: UInt64) {
        guard resumedUs > 0, captureUs >= resumedUs else { return }
        log(String(format: "stream %u: resumed, first frame sent %.0f ms after the resume", wid, Double(hostMicros() - resumedUs) / 1000))
        resumedUs = 0
    }

    private func sendFrame(captureUs: UInt64, w: Int32, h: Int32, key: Bool, fullRange: Bool, data: Data) {
        guard !stopped else { return }
        var m = MsgWriter(.frame)
        m.u64(captureUs)
        m.u16(UInt16(w)); m.u16(UInt16(h))
        m.u8((key ? 1 : 0) | (darkTitleBar ? 4 : 0) | (fullRange ? 8 : 0) | (UInt8(min(15, max(1, scale.rounded()))) << 4))
        m.bytes(data)
        frameSeq &+= 1
        m.u32(frameSeq)
        // Keyframes (big, and everything after depends on them) stay on the reliable stream.
        if !key, let send = sendDatagram {
            for d in Datagrams.split(m.framed, wid: wid, seq: frameSeq) { send(d) }
        } else {
            out.send(m)
        }
        statFrames += 1
        statBytes += data.count
        logResumedIfDue(captureUs: captureUs)
    }

    private static func makeEncoder(width: Int32, height: Int32, tenBit encoderTenBit: Bool) -> VTCompressionSession? {
        var session: VTCompressionSession?
        let spec: [CFString: Any] = [
            kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: true,
            kVTVideoEncoderSpecification_EnableLowLatencyRateControl: true,
        ]
        var st = VTCompressionSessionCreate(allocator: nil, width: width, height: height, codecType: kCMVideoCodecType_HEVC,
                                            encoderSpecification: spec as CFDictionary, imageBufferAttributes: nil,
                                            compressedDataAllocator: nil, outputCallback: nil, refcon: nil,
                                            compressionSessionOut: &session)
        if st != noErr {
            log("low-latency HEVC encoder unavailable (\(st)); falling back")
            st = VTCompressionSessionCreate(allocator: nil, width: width, height: height, codecType: kCMVideoCodecType_HEVC,
                                            encoderSpecification: nil, imageBufferAttributes: nil,
                                            compressedDataAllocator: nil, outputCallback: nil, refcon: nil,
                                            compressionSessionOut: &session)
        }
        guard st == noErr, let s = session else { log("VTCompressionSessionCreate failed \(st)"); return nil }
        // Bits scale with area; desktop content compresses well so this is generous headroom.
        let pixels = Double(width) * Double(height)
        let bitrate = min(max(pixels * 10, 20_000_000), 100_000_000)
        let props: [CFString: Any] = [
            kVTCompressionPropertyKey_RealTime: true,
            kVTCompressionPropertyKey_AllowFrameReordering: false,
            // 10-bit: subtle UI gradients (sidebars, glass) get four times the steps.
            kVTCompressionPropertyKey_ProfileLevel: encoderTenBit ? kVTProfileLevel_HEVC_Main10_AutoLevel : kVTProfileLevel_HEVC_Main_AutoLevel,
            kVTCompressionPropertyKey_MaxKeyFrameInterval: 100_000,
            kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration: 3600,
            kVTCompressionPropertyKey_ExpectedFrameRate: 120,
            kVTCompressionPropertyKey_AverageBitRate: bitrate,
            kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality: true,
            kVTCompressionPropertyKey_ColorPrimaries: kCVImageBufferColorPrimaries_ITU_R_709_2,
            // sRGB, not BT.709: the capture is sRGB and Linux compositors treat untagged video as
            // sRGB, so any other curve makes VideoToolbox shift every pixel's brightness.
            kVTCompressionPropertyKey_TransferFunction: kCVImageBufferTransferFunction_sRGB,
            kVTCompressionPropertyKey_YCbCrMatrix: kCVImageBufferYCbCrMatrix_ITU_R_709_2,
        ]
        for (k, v) in props {
            let r = VTSessionSetProperty(s, key: k, value: v as CFTypeRef)
            if r != noErr { log("encoder property \(k) rejected (\(r))") }
        }
        VTCompressionSessionPrepareToEncodeFrames(s)
        log("encoder \(width)x\(height) @ \(Int(bitrate / 1e6)) Mbit/s")
        return s
    }
}

/// Measures the window's corner radius (px) by fitting a circle to the anti-aliased top-left
/// edge (the soft fade along the top row would overestimate it). nil when the edge is opaque in
/// this frame (some apps paint it) or doesn't look like a rounded corner.
func measureCornerRadius(_ pb: CVPixelBuffer, scale: CGFloat) -> Int? {
    guard CVPixelBufferGetPixelFormatType(pb) == kCVPixelFormatType_32BGRA,
          CVPixelBufferLockBaseAddress(pb, .readOnly) == kCVReturnSuccess else { return nil }
    defer { CVPixelBufferUnlockBaseAddress(pb, .readOnly) }
    guard let base = CVPixelBufferGetBaseAddress(pb)?.assumingMemoryBound(to: UInt8.self) else { return nil }
    let w = CVPixelBufferGetWidth(pb), h = CVPixelBufferGetHeight(pb), rowBytes = CVPixelBufferGetBytesPerRow(pb)
    let n = min(Int(48 * scale), w / 3, h / 3)
    // Sub-pixel edge per row: how much of the row's first n pixels is transparent.
    var pts: [(y: Double, x: Double)] = []
    for y in 0..<n {
        let row = base + y * rowBytes
        var clear = 0.0
        for x in 0..<n { clear += 1 - Double(row[x * 4 + 3]) / 255 }
        if clear > 0.05 { pts.append((Double(y) + 0.5, clear)) }
    }
    guard pts.count >= Int(3 * scale), pts[0].x >= Double(2 * scale) else { return nil }
    var best: (r: Double, err: Double) = (0, .infinity)
    for r in Swift.stride(from: 4 * Double(scale), through: Double(n), by: 0.25) {
        var err = 0.0, count = 0
        for p in pts where p.y < r {
            let dy = r - p.y
            let x = r - (r * r - dy * dy).squareRoot()
            err += (x - p.x) * (x - p.x); count += 1
        }
        if count > 0, err / Double(count) < best.err { best = (r, err / Double(count)) }
    }
    return best.err < 4 ? Int(best.r.rounded()) : nil
}

/// The window's four corners (TL, TR, BL, BR) as square premultiplied BGRA tiles with an exact
/// anti-aliased rounded mask of radius `r` px. Colours come from the nearest pixel inside the
/// window, so whatever the capture has in the corner (transparency, an erased badge, an app's own
/// square edge) can't leak through. `content`: where the window ends in the buffer (px), when
/// macOS padded a capture of a stale size. Returns (tile px, tiles, hash).
func cornerTiles(_ pb: CVPixelBuffer, radius r: Int, scale: CGFloat, content: CGSize? = nil) -> (Int, Data, UInt64)? {
    guard r > 0, CVPixelBufferGetPixelFormatType(pb) == kCVPixelFormatType_32BGRA,
          CVPixelBufferLockBaseAddress(pb, .readOnly) == kCVReturnSuccess else { return nil }
    defer { CVPixelBufferUnlockBaseAddress(pb, .readOnly) }
    guard let base = CVPixelBufferGetBaseAddress(pb)?.assumingMemoryBound(to: UInt8.self) else { return nil }
    let bufW = CVPixelBufferGetWidth(pb), bufH = CVPixelBufferGetHeight(pb), stride = CVPixelBufferGetBytesPerRow(pb)
    // The window's real corners, wherever its content ends in the buffer.
    let w = max(1, min(bufW, content.map { Int($0.width.rounded()) } ?? bufW))
    let h = max(1, min(bufH, content.map { Int($0.height.rounded()) } ?? bufH))
    let s = max(1, Int(scale.rounded()))
    let t = ((r + s - 1) / s) * s          // whole points, so the client can place it exactly
    guard t <= w / 3, t <= h / 3 else { return nil }
    let rf = Double(r)
    var tiles = Data(count: t * t * 4 * 4)
    tiles.withUnsafeMutableBytes { raw in
        let dst = raw.bindMemory(to: UInt8.self).baseAddress!
        // Per corner: tile origin in the frame, and the corner circle's centre in frame pixels.
        let corners: [(Int, Int, Double, Double)] = [
            (0, 0, rf, rf), (w - t, 0, Double(w) - rf, rf),
            (0, h - t, rf, Double(h) - rf), (w - t, h - t, Double(w) - rf, Double(h) - rf),
        ]
        for (i, (ox, oy, cx, cy)) in corners.enumerated() {
            for ty in 0..<t {
                for tx in 0..<t {
                    let fx = ox + tx, fy = oy + ty
                    // Coverage of the rounded shape (4x4 supersampled) — only pixels beyond the
                    // centre in both axes are in the rounded part.
                    var cover = 0.0
                    for sy in 0..<4 {
                        for sx in 0..<4 {
                            let px = Double(fx) + (Double(sx) + 0.5) / 4, py = Double(fy) + (Double(sy) + 0.5) / 4
                            let dx = (px < cx) == (cx < Double(w) / 2) ? abs(px - cx) : 0
                            let dy = (py < cy) == (cy < Double(h) / 2) ? abs(py - cy) : 0
                            if dx * dx + dy * dy <= rf * rf { cover += 1 }
                        }
                    }
                    cover /= 16
                    // Colour: this pixel if it is solid window content, else the point 2px
                    // inside the curve along the ray from the circle centre.
                    var sxp = fx, syp = fy
                    let src0 = base + fy * stride + fx * 4
                    // Only genuinely transparent pixels (outside the app's own rounded corner)
                    // borrow colour from further in; translucent content keeps its own.
                    if src0[3] < 16 || cover < 1 {
                        // Walk inward along the ray from the circle centre until the capture has
                        // solid window pixels (the app's own corner may be rounder than ours).
                        let ddx = Double(fx) + 0.5 - cx, ddy = Double(fy) + 0.5 - cy
                        let d = max(1e-6, (ddx * ddx + ddy * ddy).squareRoot())
                        var dist = min(d, rf - 2)
                        while true {
                            let k = max(0, dist / d)
                            sxp = min(w - 1, max(0, Int(cx + ddx * k)))
                            syp = min(h - 1, max(0, Int(cy + ddy * k)))
                            if (base + syp * stride + sxp * 4)[3] >= 16 || dist <= 0 { break }
                            dist -= 1
                        }
                    }
                    let src = base + syp * stride + sxp * 4
                    let o = dst + (i * t * t + ty * t + tx) * 4
                    let a = Double(src[3]) / 255
                    for c in 0..<3 {
                        // Un-premultiply the source, then premultiply by the mask.
                        let v = a > 0 ? Double(src[c]) / a : 0
                        o[c] = UInt8(min(255, (v * cover).rounded()))
                    }
                    o[3] = UInt8((cover * 255).rounded())
                }
            }
        }
    }
    var hash: UInt64 = 0xcbf29ce484222325
    for b in tiles { hash = (hash ^ UInt64(b)) &* 0x100000001b3 }
    return (t, tiles, hash)
}

/// Finds traffic lights an app draws itself (hiding the standard ones, e.g. a browser): three equal
/// round shapes in a row near the top-left, at macOS size and spacing. Returns their frames in
/// window points (16x16, centred on each circle), or nil.
func detectTrafficLights(_ pb: CVPixelBuffer, scale: CGFloat) -> [CGRect]? {
    guard CVPixelBufferGetPixelFormatType(pb) == kCVPixelFormatType_32BGRA,
          CVPixelBufferLockBaseAddress(pb, .readOnly) == kCVReturnSuccess else { return nil }
    defer { CVPixelBufferUnlockBaseAddress(pb, .readOnly) }
    guard let base = CVPixelBufferGetBaseAddress(pb)?.assumingMemoryBound(to: UInt8.self) else { return nil }
    let w = CVPixelBufferGetWidth(pb), h = CVPixelBufferGetHeight(pb), rowBytes = CVPixelBufferGetBytesPerRow(pb)
    let sc = max(1, Int(scale.rounded()))
    // Work on a 1-point grid over the top-left 140x70 pt.
    let gw = min(140, w / sc), gh = min(70, h / sc)
    guard gw > 40, gh > 20 else { return nil }
    func px(_ x: Int, _ y: Int) -> (Int, Int, Int) {
        let p = base + (y * sc) * rowBytes + (x * sc) * 4
        return (Int(p[0]), Int(p[1]), Int(p[2]))
    }
    // Background: the median colour of the whole region (the title bar dominates it).
    var bgSamples: [(Int, Int, Int)] = []
    for y in Swift.stride(from: 0, to: gh, by: 2) { for x in Swift.stride(from: 0, to: gw, by: 2) { bgSamples.append(px(x, y)) } }
    let bg = (bgSamples.map { $0.0 }.sorted()[bgSamples.count / 2],
              bgSamples.map { $0.1 }.sorted()[bgSamples.count / 2],
              bgSamples.map { $0.2 }.sorted()[bgSamples.count / 2])
    var mask = [Bool](repeating: false, count: gw * gh)
    for y in 0..<gh {
        for x in 0..<gw {
            let c = px(x, y)
            mask[y * gw + x] = abs(c.0 - bg.0) + abs(c.1 - bg.1) + abs(c.2 - bg.2) > 30
        }
    }
    // Connected components → circle-like blobs about 10–16 pt across.
    var seen = [Bool](repeating: false, count: gw * gh)
    var blobs: [(cx: Double, cy: Double, d: Double)] = []
    for start in 0..<(gw * gh) where mask[start] && !seen[start] {
        var stack = [start], cells: [Int] = []
        seen[start] = true
        while let c = stack.popLast() {
            cells.append(c)
            let x = c % gw, y = c / gw
            for (nx, ny) in [(x - 1, y), (x + 1, y), (x, y - 1), (x, y + 1)] where nx >= 0 && ny >= 0 && nx < gw && ny < gh {
                let n = ny * gw + nx
                if mask[n] && !seen[n] { seen[n] = true; stack.append(n) }
            }
        }
        let xs = cells.map { $0 % gw }, ys = cells.map { $0 / gw }
        let bw = Double(xs.max()! - xs.min()! + 1), bh = Double(ys.max()! - ys.min()! + 1)
        let fill = Double(cells.count) / (bw * bh)
        guard (10...17).contains(bw), abs(bw - bh) <= 2, fill > 0.6, fill < 0.95 else { continue }
        blobs.append((Double(xs.min()!) + bw / 2, Double(ys.min()!) + bh / 2, bw))
    }
    // Three in a row, evenly spaced 18–28 pt apart.
    blobs.sort { $0.cx < $1.cx }
    for i in 0..<blobs.count {
        for j in (i + 1)..<max(i + 1, blobs.count) {
            for k in (j + 1)..<max(j + 1, blobs.count) {
                let (a, b, c) = (blobs[i], blobs[j], blobs[k])
                let s1 = b.cx - a.cx, s2 = c.cx - b.cx
                guard (18...28).contains(s1), abs(s1 - s2) <= 2, abs(a.cy - b.cy) <= 2, abs(b.cy - c.cy) <= 2,
                      abs(a.d - b.d) <= 2, abs(b.d - c.d) <= 2 else { continue }
                return [a, b, c].map { CGRect(x: $0.cx - 8, y: $0.cy - 8, width: 16, height: 16) }
            }
        }
    }
    return nil
}

/// The window's outermost pixels are a partly transparent border; with alpha dropped by the
/// encoder they'd show as black lines along the edges. Composite them (premultiplied) over the
/// colour just inside, keeping the border's highlight.
func fillEdges(_ pb: CVPixelBuffer, ring: Int) {
    guard CVPixelBufferGetPixelFormatType(pb) == kCVPixelFormatType_32BGRA,
          CVPixelBufferLockBaseAddress(pb, []) == kCVReturnSuccess else { return }
    defer { CVPixelBufferUnlockBaseAddress(pb, []) }
    guard let base = CVPixelBufferGetBaseAddress(pb)?.assumingMemoryBound(to: UInt8.self) else { return }
    let w = CVPixelBufferGetWidth(pb), h = CVPixelBufferGetHeight(pb), rowBytes = CVPixelBufferGetBytesPerRow(pb)
    guard w > 2 * ring + 2, h > 2 * ring + 2 else { return }
    func fix(_ x: Int, _ y: Int, _ ix: Int, _ iy: Int) {
        let p = base + y * rowBytes + x * 4
        let a = Int(p[3])
        guard a < 255 else { return }
        let q = base + iy * rowBytes + ix * 4
        for c in 0..<3 { p[c] = UInt8(min(255, Int(p[c]) + Int(q[c]) * (255 - a) / 255)) }
        p[3] = 255
    }
    for d in 0..<ring {
        for x in 0..<w { fix(x, d, x, ring); fix(x, h - 1 - d, x, h - 1 - ring) }
        for y in 0..<h { fix(d, y, ring, y); fix(w - 1 - d, y, w - 1 - ring, y) }
    }
}

/// Premultiplied → straight BGRA in place (SIMD), so dropping alpha doesn't darken translucent
/// pixels.
func unpremultiply(_ pb: CVPixelBuffer) {
    guard CVPixelBufferGetPixelFormatType(pb) == kCVPixelFormatType_32BGRA,
          CVPixelBufferLockBaseAddress(pb, []) == kCVReturnSuccess else { return }
    defer { CVPixelBufferUnlockBaseAddress(pb, []) }
    guard let base = CVPixelBufferGetBaseAddress(pb) else { return }
    var buf = vImage_Buffer(data: base, height: vImagePixelCount(CVPixelBufferGetHeight(pb)),
                            width: vImagePixelCount(CVPixelBufferGetWidth(pb)), rowBytes: CVPixelBufferGetBytesPerRow(pb))
    // Alpha is last in BGRA as in RGBA; the colour order does not matter here.
    vImageUnpremultiplyData_RGBA8888(&buf, &buf, vImage_Flags(kvImageNoFlags))
}

/// macOS windows have rounded, transparent corners. The Linux window is a plain rectangle, so
/// composite the transparent corner pixels over the window's own edge colour; the corners then
/// read as part of the window rather than as black specks.
func fillCorners(_ pb: CVPixelBuffer, radiusPx: Int) {
    guard CVPixelBufferGetPixelFormatType(pb) == kCVPixelFormatType_32BGRA,
          CVPixelBufferLockBaseAddress(pb, []) == kCVReturnSuccess else { return }
    defer { CVPixelBufferUnlockBaseAddress(pb, []) }
    guard let base = CVPixelBufferGetBaseAddress(pb)?.assumingMemoryBound(to: UInt8.self) else { return }
    let w = CVPixelBufferGetWidth(pb), h = CVPixelBufferGetHeight(pb), stride = CVPixelBufferGetBytesPerRow(pb)
    let k = min(radiusPx, w / 3, h / 3)
    guard k > 2 else { return }
    // (corner x range, y range, sample point just inside the straight edge)
    let corners = [
        (0..<k, 0..<k, (k, k / 2)),
        ((w - k)..<w, 0..<k, (w - 1 - k, k / 2)),
        (0..<k, (h - k)..<h, (k, h - 1 - k / 2)),
        ((w - k)..<w, (h - k)..<h, (w - 1 - k, h - 1 - k / 2)),
    ]
    for (xs, ys, (sx, sy)) in corners {
        let sp = base + sy * stride + sx * 4
        let (eb, eg, er) = (Int(sp[0]), Int(sp[1]), Int(sp[2]))
        for y in ys {
            let row = base + y * stride
            for x in xs {
                let p = row + x * 4
                let a = Int(p[3])
                if a == 255 { continue }
                // Premultiplied source over the opaque edge colour.
                p[0] = UInt8(min(255, Int(p[0]) + eb * (255 - a) / 255))
                p[1] = UInt8(min(255, Int(p[1]) + eg * (255 - a) / 255))
                p[2] = UInt8(min(255, Int(p[2]) + er * (255 - a) / 255))
                p[3] = 255
            }
        }
    }
}

/// What a window's top-left corner looks like without macOS's capture badge on it, learnt pixel
/// by pixel from the captures. In apps that move their own buttons the badge sits now in one
/// place, now in another, and at the lower one it lies over whatever comes right after the
/// buttons (an app's logo, a toolbar button): covered with plain background, that lost a bite
/// each time. The corner shows whole in the first captures after a capture starts (macOS puts
/// the badge up a moment later), and what one place hides shows while the badge sits at the
/// other: those pixels are kept here and put back under the badge instead. Kept per window
/// for as long as the host runs: across resizes (the corner doesn't move) and reconnects.
final class CleanCorner {
    private static var all: [UInt32: CleanCorner] = [:]
    private static let allLock = NSLock()
    static func of(_ wid: UInt32) -> CleanCorner {
        allLock.lock(); defer { allLock.unlock() }
        if let c = all[wid] { return c }
        let c = CleanCorner(); all[wid] = c; return c
    }

    fileprivate let lock = NSLock()
    fileprivate var shape = (0, 0)   // the corner's width and height in pixels
    fileprivate var px: [UInt8] = []
    fileprivate var known: [Bool] = []
    private var badge: CGRect?
    private var since: UInt64 = 0

    /// Whether the badge has been at `rect` for a second: only then is the rest of the corner
    /// taken as clear of it (while it moves, the capture and its position can disagree).
    fileprivate func settled(at rect: CGRect) -> Bool {
        let now = DispatchTime.now().uptimeNanoseconds
        if badge != rect { badge = rect; since = now; return false }
        return now - since > 1_000_000_000
    }
}

/// While a window is captured, macOS draws a purple "being shared" badge where its traffic lights
/// are. Erase it to the title-bar colour; the client composites the real buttons over the spot.
/// Returns whether the title bar is dark (nil when there are no buttons), for the client's
/// inactive traffic-light colour.
/// `clean`: the window's remembered corner (see CleanCorner); `fresh`: this is a new capture,
/// nothing painted on it yet, so the corner can be learnt from it; `noBadge`: the window
/// server has just said there is no badge on the window.
@discardableResult
func eraseCaptureBadge(_ pb: CVPixelBuffer, buttons: [CGRect], badge: CGRect?, scale: CGFloat,
                       clean: CleanCorner? = nil, fresh: Bool = false, noBadge: Bool = false) -> Bool? {
    let rects = buttons.filter { $0.width > 0 }
    // Two places, each covered with the background around it: where the badge is, and the
    // buttons it may cover (with a margin). Not one box around both: an app that keeps its
    // buttons away from the badge's place (its own title bar: a browser, an Electron-style
    // app) has other things between the two, and the colour around such a box isn't the
    // colour around the badge, so pieces of the badge stayed.
    var regions: [CGRect] = []
    // First the corner above and to the left of the buttons, whether or not the badge was
    // found there: that is where macOS puts it on a window with buttons (at 16,17 on a tall
    // toolbar, 12,4 or even partly above the window on others), and where pieces of a badge
    // missed for any reason showed. No further right or down than the buttons' own cover
    // below: an app's logo or toolbar can start right after them.
    if rects.count == 3, let right = rects.map(\.maxX).max(), let bottom = rects.map(\.maxY).max() {
        regions.append(CGRect(x: 0, y: 0, width: right + 6, height: bottom + 5))
    }
    // (Only when the whole badge is on the window: cut off by its edge, the piece that shows
    // isn't that shape, and is covered as a box.)
    let badgeRegion = badge.flatMap { $0.width >= 60 && $0.height >= 17 ? $0.insetBy(dx: -1, dy: -1) : nil }
    if let badge { regions.append(badgeRegion ?? badge.insetBy(dx: -1, dy: -1)) }
    if rects.count == 3 { regions.append(rects.reduce(rects[0]) { $0.union($1) }.insetBy(dx: -6, dy: -5)) }
    guard !regions.isEmpty, CVPixelBufferGetPixelFormatType(pb) == kCVPixelFormatType_32BGRA,
          CVPixelBufferLockBaseAddress(pb, []) == kCVReturnSuccess else { return nil }
    defer { CVPixelBufferUnlockBaseAddress(pb, []) }
    guard let base = CVPixelBufferGetBaseAddress(pb)?.assumingMemoryBound(to: UInt8.self) else { return nil }
    let w = CVPixelBufferGetWidth(pb), h = CVPixelBufferGetHeight(pb), stride = CVPixelBufferGetBytesPerRow(pb)

    /// Whether a pixel lies in the bar with round ends that fills `region` (the badge's shape).
    func inPill(_ region: CGRect, _ x: Int, _ y: Int) -> Bool {
        let radius = Double(region.height * scale) / 2
        let leftX = Double(region.minX * scale) + radius, rightX = Double(region.maxX * scale) - radius
        let px = Double(x) + 0.5, dy = Double(y) + 0.5 - Double(region.midY * scale)
        let dx = px < leftX ? leftX - px : px > rightX ? px - rightX : 0
        return dx * dx + dy * dy <= radius * radius
    }

    // The remembered corner: learnt from a fresh capture wherever the badge (with a margin)
    // isn't, once the badge has stayed put; and held for the covering below.
    let cornerW = min(w, Int(112 * scale)), cornerH = min(h, Int(50 * scale))
    clean?.lock.lock()
    defer { clean?.lock.unlock() }
    if let clean {
        if clean.shape != (cornerW, cornerH) {
            clean.shape = (cornerW, cornerH)
            clean.px = [UInt8](repeating: 0, count: cornerW * cornerH * 4)
            clean.known = [Bool](repeating: false, count: cornerW * cornerH)
        }
        // What is remembered must still be what the app shows there (its toolbar may have
        // changed): where the corner is in plain view now, away from the badge and the
        // buttons, it has to match, or the lot is forgotten and learnt again.
        let hidden = (badge.map { [$0.insetBy(dx: -3, dy: -3)] } ?? []) + rects.map { $0.insetBy(dx: -8, dy: -8) }
        if fresh {
            var seen = 0, differ = 0
            for y in Swift.stride(from: 0, to: cornerH, by: 2) {
                let row = base + y * stride
                for x in Swift.stride(from: 0, to: cornerW, by: 2) where clean.known[y * cornerW + x] {
                    let pt = CGPoint(x: CGFloat(x) / scale, y: CGFloat(y) / scale)
                    if hidden.contains(where: { $0.contains(pt) }) { continue }
                    seen += 1
                    let i = (y * cornerW + x) * 4
                    if abs(Int(row[x * 4]) - Int(clean.px[i])) > 40 || abs(Int(row[x * 4 + 1]) - Int(clean.px[i + 1])) > 40
                        || abs(Int(row[x * 4 + 2]) - Int(clean.px[i + 2])) > 40 { differ += 1 }
                }
            }
            if seen > 50, differ * 5 > seen {
                clean.known = [Bool](repeating: false, count: cornerW * cornerH)
            }
        }
        // Learnt from a fresh capture: all of the corner while there is no badge, and
        // otherwise wherever the badge (with a margin) isn't, once it has stayed put.
        let around = badge?.insetBy(dx: -3, dy: -3)
        if fresh, (badge == nil && noBadge) || (badge.map { clean.settled(at: $0) } ?? false) {
            for y in 0..<cornerH {
                let row = base + y * stride
                for x in 0..<cornerW where around.map({ !inPill($0, x, y) }) ?? true {
                    let i = y * cornerW + x
                    for c in 0..<4 { clean.px[i * 4 + c] = row[x * 4 + c] }
                    clean.known[i] = true
                }
            }
        }
    }

    /// Fills one region with the colour around it; whether that colour is dark, nil when
    /// there was nothing around it to go by. `pill`: only the badge's own shape within the
    /// region (a bar with round ends), so what sits beside those ends stays: as a plain box
    /// it took the edge off a logo right after the buttons.
    func cover(_ region: CGRect, pill: Bool = false) -> Bool? {
        let x0 = max(0, Int(region.minX * scale)), x1 = min(w, Int(region.maxX * scale))
        let y0 = max(0, Int(region.minY * scale)), y1 = min(h, Int(region.maxY * scale))
        guard x1 > x0, y1 > y0 else { return nil }

        // Fill colour: median of the pixels just outside the region (robust to a logo or text
        // next to it, which a single sample would pick up).
        var samples: [(UInt8, UInt8, UInt8)] = []
        let pad = max(2, Int(scale * 1.5))
        func sample(_ x: Int, _ y: Int) {
            guard x >= 0, x < w, y >= 0, y < h else { return }
            let p = base + y * stride + x * 4
            if p[3] > 250 { samples.append((p[0], p[1], p[2])) }
        }
        for x in Swift.stride(from: x0, to: x1, by: 2) { sample(x, y0 - pad); sample(x, y1 + pad) }
        for y in Swift.stride(from: y0, to: y1, by: 2) { sample(x0 - pad, y); sample(x1 + pad, y) }
        guard !samples.isEmpty else { return nil }
        let mid = samples.count / 2
        let bb = samples.map { $0.0 }.sorted()[mid]
        let bgc = samples.map { $0.1 }.sorted()[mid]
        let br = samples.map { $0.2 }.sorted()[mid]
        for y in y0..<y1 {
            let row = base + y * stride
            for x in x0..<x1 {
                if pill, !inPill(region, x, y) { continue }
                let p = row + x * 4
                // Under the badge: what was there when the badge wasn't, where that's known.
                if pill, let clean, x < cornerW, y < cornerH, clean.known[y * cornerW + x] {
                    let i = (y * cornerW + x) * 4
                    for c in 0..<4 { p[c] = clean.px[i + c] }
                    continue
                }
                // The window's own shape stays: where its rounded corner is see-through (the
                // badge can reach into it), the pixel keeps its alpha and takes the colour
                // through it (the buffer is premultiplied).
                let a = Int(p[3])
                if a == 255 {
                    p[0] = bb; p[1] = bgc; p[2] = br
                } else if a > 0 {
                    p[0] = UInt8(Int(bb) * a / 255); p[1] = UInt8(Int(bgc) * a / 255); p[2] = UInt8(Int(br) * a / 255)
                }
            }
        }
        return (Int(br) * 3 + Int(bgc) * 6 + Int(bb)) / 10 < 128
    }

    // The buttons' surroundings say whether the title bar is dark (the last region, when there).
    var dark: Bool?
    for region in regions { if let d = cover(region, pill: region == badgeRegion) { dark = d } }
    return dark
}

/// Converts a length-prefixed HEVC sample to Annex B, prepending VPS/SPS/PPS on keyframes.
func annexB(_ sb: CMSampleBuffer) -> (Data, Bool) {
    var key = true
    if let arr = CMSampleBufferGetSampleAttachmentsArray(sb, createIfNecessary: false) as? [[CFString: Any]],
       let a = arr.first, a[kCMSampleAttachmentKey_NotSync] as? Bool == true {
        key = false
    }
    var out = Data()
    let startCode: [UInt8] = [0, 0, 0, 1]
    if key, let fmt = CMSampleBufferGetFormatDescription(sb) {
        var count = 0
        CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(fmt, parameterSetIndex: 0, parameterSetPointerOut: nil,
                                                          parameterSetSizeOut: nil, parameterSetCountOut: &count,
                                                          nalUnitHeaderLengthOut: nil)
        for i in 0..<count {
            var ptr: UnsafePointer<UInt8>?
            var size = 0
            if CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(fmt, parameterSetIndex: i, parameterSetPointerOut: &ptr,
                                                                 parameterSetSizeOut: &size, parameterSetCountOut: nil,
                                                                 nalUnitHeaderLengthOut: nil) == noErr, let ptr {
                out.append(contentsOf: startCode)
                out.append(ptr, count: size)
            }
        }
    }
    guard let block = CMSampleBufferGetDataBuffer(sb) else { return (out, key) }
    var total = 0
    var base: UnsafeMutablePointer<CChar>?
    guard CMBlockBufferGetDataPointer(block, atOffset: 0, lengthAtOffsetOut: nil, totalLengthOut: &total,
                                      dataPointerOut: &base) == noErr, let base else { return (out, key) }
    base.withMemoryRebound(to: UInt8.self, capacity: total) { p in
        var off = 0
        while off + 4 <= total {
            let len = Int(UInt32(p[off]) << 24 | UInt32(p[off + 1]) << 16 | UInt32(p[off + 2]) << 8 | UInt32(p[off + 3]))
            off += 4
            guard off + len <= total else { break }
            out.append(contentsOf: startCode)
            out.append(p + off, count: len)
            off += len
        }
    }
    return (out, key)
}

func hostMicros() -> UInt64 {
    clock_gettime_nsec_np(CLOCK_UPTIME_RAW) / 1000
}

/// Tags a buffer we made ourselves as sRGB, like the captured frames (untagged ones can be
/// converted with a different curve on the way to 10-bit).
func tagSRGB(_ pb: CVPixelBuffer) {
    if let cs = CGColorSpace(name: CGColorSpace.sRGB) {
        CVBufferSetAttachment(pb, kCVImageBufferCGColorSpaceKey, cs, .shouldPropagate)
    }
    CVBufferSetAttachment(pb, kCVImageBufferColorPrimariesKey, kCVImageBufferColorPrimaries_ITU_R_709_2, .shouldPropagate)
    CVBufferSetAttachment(pb, kCVImageBufferTransferFunctionKey, kCVImageBufferTransferFunction_sRGB, .shouldPropagate)
}

func solidBuffer(width w: Int, height h: Int, gray: UInt8) -> CVPixelBuffer? {
    var out: CVPixelBuffer?
    let attrs = [kCVPixelBufferIOSurfacePropertiesKey: [:]] as CFDictionary
    guard CVPixelBufferCreate(nil, w, h, kCVPixelFormatType_32BGRA, attrs, &out) == kCVReturnSuccess, let out else { return nil }
    tagSRGB(out)
    CVPixelBufferLockBaseAddress(out, [])
    defer { CVPixelBufferUnlockBaseAddress(out, []) }
    guard let base = CVPixelBufferGetBaseAddress(out) else { return nil }
    let row = CVPixelBufferGetBytesPerRow(out)
    let px = base.bindMemory(to: UInt32.self, capacity: row / 4 * h)
    let v = UInt32(gray)
    let fill = 0xFF00_0000 | v << 16 | v << 8 | v
    for i in 0..<(row / 4 * h) { px[i] = fill }
    return out
}

/// Draws `image` (`size` px square) in the middle of the part of `pb` shown on Linux.
func drawCentred(_ image: CGImage, in pb: CVPixelBuffer, size: CGFloat, visible: CGSize?) {
    let w = CVPixelBufferGetWidth(pb), h = CVPixelBufferGetHeight(pb)
    CVPixelBufferLockBaseAddress(pb, [])
    defer { CVPixelBufferUnlockBaseAddress(pb, []) }
    guard let base = CVPixelBufferGetBaseAddress(pb),
          let ctx = CGContext(data: base, width: w, height: h, bitsPerComponent: 8, bytesPerRow: CVPixelBufferGetBytesPerRow(pb),
                              space: CGColorSpace(name: CGColorSpace.sRGB)!,
                              bitmapInfo: CGImageAlphaInfo.premultipliedFirst.rawValue | CGBitmapInfo.byteOrder32Little.rawValue)
    else { return }
    let vw = min(CGFloat(w), visible?.width ?? CGFloat(w)), vh = min(CGFloat(h), visible?.height ?? CGFloat(h))
    let side = min(size, vw * 0.5, vh * 0.5)
    ctx.interpolationQuality = .high
    ctx.draw(image, in: CGRect(x: vw / 2 - side / 2, y: CGFloat(h) - vh / 2 - side / 2, width: side, height: side))
}

/// A greyscale copy of `pb` with a flat pause symbol in the middle (shown while the Mac is locked).
func pausedCopy(of pb: CVPixelBuffer, scale: CGFloat, visible: CGSize? = nil) -> CVPixelBuffer? {
    let w = CVPixelBufferGetWidth(pb), h = CVPixelBufferGetHeight(pb)
    var out: CVPixelBuffer?
    let attrs = [kCVPixelBufferIOSurfacePropertiesKey: [:]] as CFDictionary
    guard CVPixelBufferCreate(nil, w, h, kCVPixelFormatType_32BGRA, attrs, &out) == kCVReturnSuccess, let out else { return nil }
    tagSRGB(out)
    CVPixelBufferLockBaseAddress(pb, .readOnly)
    CVPixelBufferLockBaseAddress(out, [])
    defer {
        CVPixelBufferUnlockBaseAddress(out, [])
        CVPixelBufferUnlockBaseAddress(pb, .readOnly)
    }
    guard let src = CVPixelBufferGetBaseAddress(pb), let dst = CVPixelBufferGetBaseAddress(out) else { return nil }
    var s = vImage_Buffer(data: src, height: vImagePixelCount(h), width: vImagePixelCount(w), rowBytes: CVPixelBufferGetBytesPerRow(pb))
    var d = vImage_Buffer(data: dst, height: vImagePixelCount(h), width: vImagePixelCount(w), rowBytes: CVPixelBufferGetBytesPerRow(out))
    // Rec. 709 luma into B, G and R (memory order BGRA), alpha kept; a little darker, to read as
    // inactive. dest[j] = Σ src[i] · m[i·4 + j] / divisor.
    let k: Double = 0.85 * 256
    let (b, g, r) = (Int16(0.0722 * k), Int16(0.7152 * k), Int16(0.2126 * k))
    let m: [Int16] = [b, b, b, 0,
                      g, g, g, 0,
                      r, r, r, 0,
                      0, 0, 0, 256]
    guard vImageMatrixMultiply_ARGB8888(&s, &d, m, 256, nil, nil, vImage_Flags(kvImageNoFlags)) == kvImageNoError else { return nil }

    guard let ctx = CGContext(data: dst, width: w, height: h, bitsPerComponent: 8, bytesPerRow: CVPixelBufferGetBytesPerRow(out),
                              space: CGColorSpace(name: CGColorSpace.sRGB)!,
                              bitmapInfo: CGImageAlphaInfo.premultipliedFirst.rawValue | CGBitmapInfo.byteOrder32Little.rawValue)
    else { return out }
    // A dark disc with two white bars, sized to the window within sensible limits.
    // A small badge in the top-right corner of the part shown on Linux (the top-left `visible`
    // pixels; CG's origin is bottom-left): a dark disc with two white bars, always the same size.
    let vw = min(CGFloat(w), visible?.width ?? CGFloat(w))
    let radius = 11 * scale, inset = 14 * scale
    let c = CGPoint(x: vw - inset - radius, y: CGFloat(h) - inset - radius)
    ctx.setFillColor(CGColor(gray: 0, alpha: 0.55))
    ctx.fillEllipse(in: CGRect(x: c.x - radius, y: c.y - radius, width: radius * 2, height: radius * 2))
    let barW = radius * 0.28, barH = radius * 0.9, gap = radius * 0.28
    ctx.setFillColor(CGColor(gray: 1, alpha: 0.95))
    for x in [c.x - gap / 2 - barW, c.x + gap / 2] {
        let bar = CGRect(x: x, y: c.y - barH / 2, width: barW, height: barH)
        ctx.addPath(CGPath(roundedRect: bar, cornerWidth: barW * 0.3, cornerHeight: barW * 0.3, transform: nil))
    }
    ctx.fillPath()
    return out
}

/// Whether any pixel away from the corners and edges is semi-transparent, looking at one pixel
/// in 64 (every 8th in both directions).
func hasTranslucency(_ pb: CVPixelBuffer, skip margin: Int) -> Bool {
    guard CVPixelBufferGetPixelFormatType(pb) == kCVPixelFormatType_32BGRA,
          CVPixelBufferLockBaseAddress(pb, .readOnly) == kCVReturnSuccess else { return true }
    defer { CVPixelBufferUnlockBaseAddress(pb, .readOnly) }
    guard let base = CVPixelBufferGetBaseAddress(pb)?.assumingMemoryBound(to: UInt8.self) else { return true }
    let w = CVPixelBufferGetWidth(pb), h = CVPixelBufferGetHeight(pb), stride = CVPixelBufferGetBytesPerRow(pb)
    guard w > 2 * margin, h > 2 * margin else { return true }
    for y in Swift.stride(from: margin, to: h - margin, by: 8) {
        let row = base + y * stride
        for x in Swift.stride(from: margin, to: w - margin, by: 8) where row[x * 4 + 3] < 250 {
            return true
        }
    }
    return false
}
