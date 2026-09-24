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
    // Enough for 100–120 fps over a ~25 ms pipeline; more only queues frames in the encoder.
    private let maxInFlight = 3
    private var pendingBuffer: CVPixelBuffer?   // newest capture not yet encoded (throttled)
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
    /// The size Linux shows this window at (points). When the Mac window can't shrink that far
    /// (app minimum size), Linux crops it, so the bottom/right corners must be cut at the
    /// visible edge rather than the window's real edge.
    var visibleSize: CGSize?
    /// The app's icon, shown on the placeholder when the Mac is locked before the first frame.
    var appIcon: CGImage?
    /// Pixels per point of the display the window is on (looked up from its position).
    var displayScale: (() -> CGFloat?)?
    private var scaleCheckCountdown = 0
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

    private func applyFrameRate() {
        config.minimumFrameInterval = CMTime(value: 1, timescale: active ? Self.activeFps : Self.backgroundFps)
        stream?.updateConfiguration(config) { err in if let err { log("updateConfiguration: \(err)") } }
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
    private var statEncodeSum: UInt64 = 0, statEncodeN = 0, statCaptureLagSum: UInt64 = 0, statCaptureLagN = 0
    private var statSince = DispatchTime.now()

    /// Menus, tooltips and other popups: sent as lossless BGRA with alpha (LZ4) instead of HEVC,
    /// so their rounded corners and translucency survive. They are small and short-lived.
    let lossless: Bool

    init(wid: UInt32, pointSize: CGSize, scale: CGFloat, lossless: Bool, out: MsgStream, queue: DispatchQueue) {
        self.wid = wid
        self.lossless = lossless
        self.pointSize = pointSize
        self.scale = scale
        self.out = out
        self.queue = queue
    }

    // MARK: lifecycle

    func start() {
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
        let t = DispatchSource.makeTimerSource(queue: queue)
        t.schedule(deadline: .now() + .milliseconds(100), repeating: .milliseconds(100))
        t.setEventHandler { [weak self] in self?.tick() }
        t.resume()
        refineTimer = t
    }

    private func startCapture(_ win: SCWindow) {
        scWindow = win
        let filter = SCContentFilter(desktopIndependentWindow: win)
        applySize(to: config)
        // BGRA with alpha so the rounded window corners can be filled in (see fillCorners);
        // the encoder converts to 4:2:0 in hardware.
        config.pixelFormat = kCVPixelFormatType_32BGRA
        config.colorSpaceName = CGColorSpace.sRGB
        config.minimumFrameInterval = CMTime(value: 1, timescale: active ? Self.activeFps : Self.backgroundFps)
        config.queueDepth = 4
        config.showsCursor = false
        config.ignoreShadowsSingleWindow = true
        // A shared window's capture otherwise gets its menus/popups composited in (scaled and
        // misplaced); we stream those as their own windows.
        config.includeChildWindows = false
        config.shouldBeOpaque = false
        config.capturesAudio = false
        let s = SCStream(filter: filter, configuration: config, delegate: self)
        do {
            try s.addStreamOutput(self, type: .screen, sampleHandlerQueue: captureQueue)
        } catch {
            log("stream \(wid): addStreamOutput failed: \(error)")
            out.close(); return
        }
        stream = s
        s.startCapture { [weak self] err in
            if let err { log("stream \(self?.wid ?? 0): startCapture failed: \(err)"); self?.queue.async { self?.out.close() } }
            else { log("stream \(self?.wid ?? 0): capturing") }
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
    }

    func setPaused(_ p: Bool) {
        paused = p
        if !p { forceKeyframe = true; if let b = lastBuffer { pendingBuffer = pendingBuffer ?? b; pump() } }
    }

    /// Re-encode the last capture with fresh overlays (e.g. focus changed the traffic lights).
    /// The capture buffer still holds the previously painted lights, so this only runs the paint.
    func requestRepaint() {
        guard pendingBuffer == nil, let b = lastBuffer else { return }
        eraseCaptureBadge(b, buttons: buttons, badge: badgeRect(), scale: scale)
        pendingBuffer = b
        refinementsLeft = 2
        pump()
    }

    /// Corner tiles cut from `pb` (sent ahead of its frame), so they match what it shows.
    private func updateCorners(from pb: CVPixelBuffer) {
        let radius = Int((CGFloat(WindowStreamer.consensusRadiusPt) * scale).rounded())
        if let (t, tiles, hash) = cornerTiles(pb, radius: radius, scale: scale, visible: visiblePx()), hash != cornersHash {
            cornersHash = hash
            pendingCorners = (t, tiles)
        }
    }

    private func visiblePx() -> CGSize? {
        visibleSize.map { CGSize(width: $0.width * scale, height: $0.height * scale) }
    }

    /// The colour frame shown before the Mac locked, put back when it unlocks (a static window
    /// sends no new frame by itself).
    private var colourBeforeLock: CVPixelBuffer?
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
                    guard let grey = pausedCopy(of: b, scale: self.scale, visible: self.visiblePx()) else { return }
                    self.colourBeforeLock = b
                    self.updateCorners(from: grey)
                    self.pendingBuffer = grey
                } else {
                    // Locked before the first frame: a dark placeholder with the pause symbol.
                    let (w, h) = self.pixelSize()
                    guard let blank = solidBuffer(width: w, height: h, gray: 0x3A) else { return }
                    if let icon = self.appIcon { drawCentred(icon, in: blank, size: 72 * self.scale, visible: self.visiblePx()) }
                    guard let grey = pausedCopy(of: blank, scale: self.scale, visible: self.visiblePx()) else { return }
                    self.updateCorners(from: grey)
                    self.pendingBuffer = grey
                }
            } else {
                guard self.showingPause else { return }
                self.showingPause = false
                guard let b = self.colourBeforeLock else {
                    self.pendingBuffer = nil
                    // Only a placeholder was shown: a fresh capture starts with a real frame.
                    if let win = self.scWindow, let stream = self.stream {
                        stream.stopCapture { _ in }
                        self.stream = nil
                        self.startCapture(win)
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
            if now > captureUs { statLatencySum += now - captureUs; statLatencyN += 1 }
        }
        pump()
    }

    func stop() {
        guard !stopped else { return }
        stopped = true
        refineTimer?.cancel()
        stream?.stopCapture { _ in }
        stream = nil
        if let e = encoder { VTCompressionSessionInvalidate(e) }
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
            guard let self, !self.stopped, !self.showingPause else { return }
            if lagUs > 0 { self.statCaptureLagSum += lagUs; self.statCaptureLagN += 1 }
            if !self.loggedAttachments {
                self.loggedAttachments = true
                let a = CVBufferCopyAttachments(pb, .shouldPropagate) as? [String: Any] ?? [:]
                log("stream \(self.wid): buffer colour tags \(a.filter { $0.key.contains("Color") || $0.key.contains("Transfer") || $0.key.contains("Matrix") || $0.key.contains("Primaries") })")
            }
            if let r = contentRect {
                let bw = CGFloat(CVPixelBufferGetWidth(pb)), bh = CGFloat(CVPixelBufferGetHeight(pb))
                let cw = r.width * scaleFactor, ch = r.height * scaleFactor
                if abs(cw - bw) > 2 || abs(ch - bh) > 2 {
                    self.mismatchLogs += 1
                    if self.mismatchLogs <= 5 {
                        log(String(format: "stream %u: content %.0fx%.0f (rect %@ contentScale %.2f scaleFactor %.1f) in buffer %.0fx%.0f, pointSize %@",
                                   self.wid, cw, ch, NSStringFromRect(r), contentScale, scaleFactor, bw, bh, NSStringFromSize(self.pointSize)))
                    }
                }
            }
            if !self.lossless {
                // Apps that hide the standard buttons and draw their own: find them in the pixels
                // (a few times a second until found).
                if !self.buttons.contains(where: { $0.width > 0 }), self.onDetectedButtons != nil {
                    self.detectCountdown -= 1
                    if self.detectCountdown <= 0 {
                        self.detectCountdown = 20
                        if let found = detectTrafficLights(pb, scale: self.scale) {
                            log("stream \(self.wid): found app-drawn traffic lights at \(found.map { "\(Int($0.minX)),\(Int($0.minY))" })")
                            self.buttons = found
                            self.onDetectedButtons?(found)
                        }
                    }
                }
                // Order matters: erase the capture badge first (it overlaps the top-left
                // corner), cut the transparent corner tiles, then flatten the corners for HEVC.
                if let dark = eraseCaptureBadge(pb, buttons: self.buttons, badge: self.badgeRect(), scale: self.scale) { self.darkTitleBar = dark }
                let t0 = DispatchTime.now().uptimeNanoseconds
                // A few radius measurements per window (not every frame) feed the shared vote.
                self.measureCountdown -= 1
                if self.radiusSamples < 5, self.measureCountdown <= 0 {
                    self.measureCountdown = 15
                    if let r = measureCornerRadius(pb, scale: self.scale) {
                        self.radiusSamples += 1
                        let pt = Int((CGFloat(r) / self.scale).rounded())
                        WindowStreamer.radiusVotes[pt, default: 0] += 1
                        log("stream \(self.wid): corner radius sample \(pt)pt → consensus \(WindowStreamer.consensusRadiusPt)pt")
                    }
                }
                let radius = Int((CGFloat(WindowStreamer.consensusRadiusPt) * self.scale).rounded())
                let visiblePx = self.visibleSize.map { CGSize(width: $0.width * self.scale, height: $0.height * self.scale) }
                if let (t, tiles, hash) = cornerTiles(pb, radius: radius, scale: self.scale, visible: visiblePx),
                   hash != self.cornersHash {
                    self.cornersHash = hash
                    self.pendingCorners = (t, tiles)
                }
                fillCorners(pb, radiusPx: Int(28 * self.scale))
                fillEdges(pb, ring: max(2, Int(self.scale)))
                // Translucent areas (vibrancy sidebars etc.) arrive premultiplied; the encoder
                // drops alpha, which would leave them darkened. Restore their real colour.
                unpremultiply(pb)
                let ms = Double(DispatchTime.now().uptimeNanoseconds - t0) / 1e6
                if ms > 3 { log(String(format: "stream %u: frame prep took %.1f ms", self.wid, ms)) }
            }
            self.pendingBuffer = pb
            self.refinementsLeft = 2
            self.pump()
        }
    }

    func stream(_ stream: SCStream, didStopWithError error: Error) {
        log("stream \(wid) stopped: \(error)")
        queue.async { [weak self] in self?.out.close() }
    }

    // MARK: encode

    /// Encodes the pending capture if flow control allows.
    private func pump() {
        guard !stopped, !paused, inFlight.count < maxInFlight, let pb = pendingBuffer else { return }
        pendingBuffer = nil
        if lossless { sendLossless(pb) } else { encode(pb) }
    }

    /// When the window goes idle, re-encode the last frame a couple of times: the rate controller
    /// spends the spare bits refining it, so static content converges to near-lossless.
    private func tick() {
        // The window moved to a display with another scale (e.g. the virtual HiDPI display came
        // or went): capture at that scale rather than padding with black or losing detail.
        scaleCheckCountdown -= 1
        if scaleCheckCountdown <= 0 {
            scaleCheckCountdown = 5
            if let want = displayScale?(), want > 0, abs(want - scale) > 0.01, let stream, !stopped {
                log("stream \(wid): display scale \(scale) → \(want)")
                scale = want
                if lossless, let win = scWindow {
                    stream.stopCapture { _ in }
                    self.stream = nil
                    startCapture(win)
                } else {
                    applySize(to: config)
                    stream.updateConfiguration(config) { err in if let err { log("updateConfiguration: \(err)") } }
                }
            }
        }
        let idleMs = (DispatchTime.now().uptimeNanoseconds - lastEncodeAt.uptimeNanoseconds) / 1_000_000
        if refinementsLeft > 0, idleMs >= 120, pendingBuffer == nil, let b = lastBuffer {
            refinementsLeft -= 1
            pendingBuffer = b
            pump()
        }
        let el = Double(DispatchTime.now().uptimeNanoseconds - statSince.uptimeNanoseconds) / 1e9
        if el >= 5, statFrames > 0 {
            let lat = statLatencyN > 0 ? Double(statLatencySum) / Double(statLatencyN) / 1000 : -1
            let enc = statEncodeN > 0 ? Double(statEncodeSum) / Double(statEncodeN) / 1000 : -1
            let lag = statCaptureLagN > 0 ? Double(statCaptureLagSum) / Double(statCaptureLagN) / 1000 : -1
            log(String(format: "stream %u: %.1f fps, %.1f Mbit/s, capture→present %.1f ms (capture lag %.1f, encode %.1f, in flight %d)", wid,
                       Double(statFrames) / el, Double(statBytes) * 8 / el / 1e6, lat, lag, enc, inFlight.count))
            statFrames = 0; statBytes = 0; statLatencySum = 0; statLatencyN = 0; statSince = .now()
            statEncodeSum = 0; statEncodeN = 0; statCaptureLagSum = 0; statCaptureLagN = 0
        }
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

    private func encode(_ pb: CVPixelBuffer) {
        flushCorners()
        let w = Int32(CVPixelBufferGetWidth(pb)), h = Int32(CVPixelBufferGetHeight(pb))
        if encoder == nil || encoderSize != (w, h) {
            if let e = encoder { VTCompressionSessionInvalidate(e) }
            encoder = makeEncoder(width: w, height: h)
            encoderSize = (w, h)
            forceKeyframe = true
        }
        guard let enc = encoder else { return }
        lastBuffer = pb
        lastEncodeAt = .now()
        let captureUs = hostMicros()
        var props: CFDictionary?
        if forceKeyframe {
            props = [kVTEncodeFrameOptionKey_ForceKeyFrame: true] as CFDictionary
            forceKeyframe = false
        }
        inFlight.append(captureUs)
        let pts = CMTime(value: CMTimeValue(captureUs), timescale: 1_000_000)
        VTCompressionSessionEncodeFrame(enc, imageBuffer: pb, presentationTimeStamp: pts, duration: .invalid,
                                        frameProperties: props, infoFlagsOut: nil) { [weak self] status, _, sb in
            guard let self else { return }
            guard status == noErr, let sb else {
                log("encode error \(status)")
                self.queue.async { self.inFlight.removeAll { $0 == captureUs }; self.forceKeyframe = true }
                return
            }
            let (data, key) = annexB(sb)
            // The low-latency encoder emits full-range YUV whatever the input; tell the client.
            let fullRange = CMSampleBufferGetFormatDescription(sb).flatMap {
                CMFormatDescriptionGetExtension($0, extensionKey: kCMFormatDescriptionExtension_FullRangeVideo) as? Bool
            } ?? false
            let encodeUs = hostMicros() - captureUs
            self.queue.async {
                self.statEncodeSum += encodeUs; self.statEncodeN += 1
                self.sendFrame(captureUs: captureUs, w: w, h: h, key: key, fullRange: fullRange, data: data)
            }
        }
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
        var m = MsgWriter(.frame)
        m.u64(captureUs)
        m.u16(UInt16(w)); m.u16(UInt16(h))
        m.u8(0b11 | (UInt8(min(15, max(1, scale.rounded()))) << 4))   // keyframe | lossless | px/pt
        m.bytes(Data(out[0..<n]))
        out_send(m, bytes: n)
    }

    private func out_send(_ m: MsgWriter, bytes: Int) {
        out.send(m)
        statFrames += 1
        statBytes += bytes
    }

    private func sendFrame(captureUs: UInt64, w: Int32, h: Int32, key: Bool, fullRange: Bool, data: Data) {
        guard !stopped else { return }
        var m = MsgWriter(.frame)
        m.u64(captureUs)
        m.u16(UInt16(w)); m.u16(UInt16(h))
        m.u8((key ? 1 : 0) | (darkTitleBar ? 4 : 0) | (fullRange ? 8 : 0) | (UInt8(min(15, max(1, scale.rounded()))) << 4))
        m.bytes(data)
        out.send(m)
        statFrames += 1
        statBytes += data.count
    }

    private func makeEncoder(width: Int32, height: Int32) -> VTCompressionSession? {
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
            kVTCompressionPropertyKey_ProfileLevel: kVTProfileLevel_HEVC_Main_AutoLevel,
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
/// square edge) can't leak through. Returns (tile px, tiles, hash).
func cornerTiles(_ pb: CVPixelBuffer, radius r: Int, scale: CGFloat, visible: CGSize? = nil) -> (Int, Data, UInt64)? {
    guard r > 0, CVPixelBufferGetPixelFormatType(pb) == kCVPixelFormatType_32BGRA,
          CVPixelBufferLockBaseAddress(pb, .readOnly) == kCVReturnSuccess else { return nil }
    defer { CVPixelBufferUnlockBaseAddress(pb, .readOnly) }
    guard let base = CVPixelBufferGetBaseAddress(pb)?.assumingMemoryBound(to: UInt8.self) else { return nil }
    let bufW = CVPixelBufferGetWidth(pb), bufH = CVPixelBufferGetHeight(pb), stride = CVPixelBufferGetBytesPerRow(pb)
    // The corners go where Linux's view of the window ends (it crops windows it can't shrink).
    let w = min(bufW, visible.map { Int($0.width.rounded()) } ?? bufW)
    let h = min(bufH, visible.map { Int($0.height.rounded()) } ?? bufH)
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

/// While a window is captured, macOS draws a purple "being shared" badge where its traffic lights
/// are. Erase it to the title-bar colour; the client composites the real buttons over the spot.
/// Returns whether the title bar is dark (nil when there are no buttons), for the client's
/// inactive traffic-light colour.
@discardableResult
func eraseCaptureBadge(_ pb: CVPixelBuffer, buttons: [CGRect], badge: CGRect?, scale: CGFloat) -> Bool? {
    let rects = buttons.filter { $0.width > 0 }
    // Erase where the badge is, plus a margin around the buttons it may cover.
    var region: CGRect?
    if rects.count == 3 { region = rects.reduce(rects[0]) { $0.union($1) }.insetBy(dx: -6, dy: -5) }
    if let badge { region = region.map { $0.union(badge.insetBy(dx: -1, dy: -1)) } ?? badge.insetBy(dx: -1, dy: -1) }
    guard let union = region, CVPixelBufferGetPixelFormatType(pb) == kCVPixelFormatType_32BGRA,
          CVPixelBufferLockBaseAddress(pb, []) == kCVReturnSuccess else { return nil }
    defer { CVPixelBufferUnlockBaseAddress(pb, []) }
    guard let base = CVPixelBufferGetBaseAddress(pb)?.assumingMemoryBound(to: UInt8.self) else { return nil }
    let w = CVPixelBufferGetWidth(pb), h = CVPixelBufferGetHeight(pb), stride = CVPixelBufferGetBytesPerRow(pb)

    // The badge is a pill around all three buttons; clear it to the surrounding title bar.
    let x0 = max(0, Int(union.minX * scale)), x1 = min(w, Int(union.maxX * scale))
    let y0 = max(0, Int(union.minY * scale)), y1 = min(h, Int(union.maxY * scale))
    guard x1 > x0, y1 > y0 else { return nil }
    // Fill colour: median of the pixels just outside the region (robust to a logo or text next
    // to the buttons, which a single sample would pick up).
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
        for x in x0..<x1 { let p = row + x * 4; p[0] = bb; p[1] = bgc; p[2] = br; p[3] = 255 }
    }
    let luminance = (Int(br) * 3 + Int(bgc) * 6 + Int(bb)) / 10
    return luminance < 128
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

func solidBuffer(width w: Int, height h: Int, gray: UInt8) -> CVPixelBuffer? {
    var out: CVPixelBuffer?
    let attrs = [kCVPixelBufferIOSurfacePropertiesKey: [:]] as CFDictionary
    guard CVPixelBufferCreate(nil, w, h, kCVPixelFormatType_32BGRA, attrs, &out) == kCVReturnSuccess, let out else { return nil }
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
