import AppKit
import Foundation
import ScreenCaptureKit
import VideoToolbox
import CoreMedia
import Compression

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
    private let maxInFlight = 2
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
    /// Called when traffic lights drawn by the app itself were found in the pixels.
    var onDetectedButtons: (([CGRect]) -> Void)?
    private var detectCountdown = 0
    var active = false
    private var darkTitleBar = true
    /// Last corner tiles sent (hash), and the ones to send with the next encoded frame.
    private var cornersHash: UInt64 = 0
    /// Largest corner radius seen (px); stable across frames where the app paints its edge.
    private var cornerRadiusPx = 0
    /// Radius measured on any window: standard windows share it, so apps that paint their own
    /// edge (a browser) still get the system's rounded corners.
    static var sharedRadiusPt: CGFloat = 0
    private var pendingCorners: (Int, Data)?

    // Stats
    private var statFrames = 0, statBytes = 0, statLatencySum: UInt64 = 0, statLatencyN = 0
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
        config.minimumFrameInterval = CMTime(value: 1, timescale: 120)
        config.queueDepth = 4
        config.showsCursor = false
        config.ignoreShadowsSingleWindow = true
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
        queue.async { [weak self] in
            guard let self, !self.stopped else { return }
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
                if let r = measureCornerRadius(pb, scale: self.scale), r > self.cornerRadiusPx {
                    self.cornerRadiusPx = r
                    WindowStreamer.sharedRadiusPt = max(WindowStreamer.sharedRadiusPt, CGFloat(r) / self.scale)
                    log("stream \(self.wid): corner radius \(r)px")
                }
                let radius = self.cornerRadiusPx > 0 ? self.cornerRadiusPx
                    : Int((WindowStreamer.sharedRadiusPt * self.scale).rounded())
                if let (t, tiles, hash) = cornerTiles(pb, radius: radius, scale: self.scale),
                   hash != self.cornersHash {
                    self.cornersHash = hash
                    self.pendingCorners = (t, tiles)
                }
                fillCorners(pb, radiusPx: Int(28 * self.scale))
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
        let idleMs = (DispatchTime.now().uptimeNanoseconds - lastEncodeAt.uptimeNanoseconds) / 1_000_000
        if refinementsLeft > 0, idleMs >= 120, pendingBuffer == nil, let b = lastBuffer {
            refinementsLeft -= 1
            pendingBuffer = b
            pump()
        }
        let el = Double(DispatchTime.now().uptimeNanoseconds - statSince.uptimeNanoseconds) / 1e9
        if el >= 5, statFrames > 0 {
            let lat = statLatencyN > 0 ? Double(statLatencySum) / Double(statLatencyN) / 1000 : -1
            log(String(format: "stream %u: %.1f fps, %.1f Mbit/s, capture→present %.1f ms", wid,
                       Double(statFrames) / el, Double(statBytes) * 8 / el / 1e6, lat))
            statFrames = 0; statBytes = 0; statLatencySum = 0; statLatencyN = 0; statSince = .now()
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
            self.queue.async { self.sendFrame(captureUs: captureUs, w: w, h: h, key: key, data: data) }
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

    private func sendFrame(captureUs: UInt64, w: Int32, h: Int32, key: Bool, data: Data) {
        guard !stopped else { return }
        var m = MsgWriter(.frame)
        m.u64(captureUs)
        m.u16(UInt16(w)); m.u16(UInt16(h))
        m.u8((key ? 1 : 0) | (darkTitleBar ? 4 : 0) | (UInt8(min(15, max(1, scale.rounded()))) << 4))
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
func cornerTiles(_ pb: CVPixelBuffer, radius r: Int, scale: CGFloat) -> (Int, Data, UInt64)? {
    guard r > 0, CVPixelBufferGetPixelFormatType(pb) == kCVPixelFormatType_32BGRA,
          CVPixelBufferLockBaseAddress(pb, .readOnly) == kCVReturnSuccess else { return nil }
    defer { CVPixelBufferUnlockBaseAddress(pb, .readOnly) }
    guard let base = CVPixelBufferGetBaseAddress(pb)?.assumingMemoryBound(to: UInt8.self) else { return nil }
    let w = CVPixelBufferGetWidth(pb), h = CVPixelBufferGetHeight(pb), stride = CVPixelBufferGetBytesPerRow(pb)
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
                    if src0[3] < 250 || cover < 1 {
                        let ddx = Double(fx) + 0.5 - cx, ddy = Double(fy) + 0.5 - cy
                        let d = max(1e-6, (ddx * ddx + ddy * ddy).squareRoot())
                        let k = min(1, max(0, (rf - 2) / d))
                        sxp = min(w - 1, max(0, Int(cx + ddx * k)))
                        syp = min(h - 1, max(0, Int(cy + ddy * k)))
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

/// Writes the top-left corner tile as a PNG in the support directory (diagnostics).
func debugDumpTile(_ tiles: Data, t: Int, name: String) {
    guard let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: t, pixelsHigh: t, bitsPerSample: 8,
                                     samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
                                     bytesPerRow: t * 4, bitsPerPixel: 32), let dst = rep.bitmapData else { return }
    tiles.withUnsafeBytes { src in
        let p = src.bindMemory(to: UInt8.self)
        for i in 0..<(t * t) {   // BGRA premultiplied → RGBA premultiplied
            dst[i * 4] = p[i * 4 + 2]; dst[i * 4 + 1] = p[i * 4 + 1]; dst[i * 4 + 2] = p[i * 4]; dst[i * 4 + 3] = p[i * 4 + 3]
        }
    }
    try? rep.representation(using: .png, properties: [:])?.write(to: Identity.dir.appendingPathComponent(name))
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
