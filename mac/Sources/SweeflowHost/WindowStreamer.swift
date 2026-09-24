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
    var active = false
    private var darkTitleBar = true
    /// Last corner tiles sent (hash), and the ones to send with the next encoded frame.
    private var cornersHash: UInt64 = 0
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
                // Order matters: erase the capture badge first (it overlaps the top-left
                // corner), cut the transparent corner tiles, then flatten the corners for HEVC.
                if let dark = eraseCaptureBadge(pb, buttons: self.buttons, badge: self.badgeRect(), scale: self.scale) { self.darkTitleBar = dark }
                if let (t, tiles, hash) = cornerTiles(pb, scale: self.scale), hash != self.cornersHash {
                    log("stream \(self.wid): corner tiles \(t)px (buttons known: \(self.buttons.filter { $0.width > 0 }.count))")
                    debugDumpTile(tiles, t: t, name: "tile-\(self.wid).png")
                    self.cornersHash = hash
                    self.pendingCorners = (t, tiles)
                }
                fillCorners(pb, radiusPx: Int(28 * self.scale))
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

/// The window's rounded corners as four square BGRA tiles (TL, TR, BL, BR; premultiplied, with
/// alpha), sized to the corner radius rounded up to whole points. Returns (tile px, tiles, hash);
/// nil when the window has square corners.
func cornerTiles(_ pb: CVPixelBuffer, scale: CGFloat) -> (Int, Data, UInt64)? {
    guard CVPixelBufferGetPixelFormatType(pb) == kCVPixelFormatType_32BGRA,
          CVPixelBufferLockBaseAddress(pb, .readOnly) == kCVReturnSuccess else { return nil }
    defer { CVPixelBufferUnlockBaseAddress(pb, .readOnly) }
    guard let base = CVPixelBufferGetBaseAddress(pb)?.assumingMemoryBound(to: UInt8.self) else { return nil }
    let w = CVPixelBufferGetWidth(pb), h = CVPixelBufferGetHeight(pb), stride = CVPixelBufferGetBytesPerRow(pb)
    // Radius ≈ run of non-opaque pixels at the start of the top row.
    let limit = min(Int(40 * scale), w / 3, h / 3)
    var r = 0
    while r < limit && base[r * 4 + 3] < 250 { r += 1 }
    guard r >= 2 else { return nil }
    let s = max(1, Int(scale.rounded()))
    let t = ((r + s - 1) / s) * s
    guard t <= w / 2, t <= h / 2 else { return nil }
    var tiles = Data(count: t * t * 4 * 4)
    tiles.withUnsafeMutableBytes { raw in
        let dst = raw.bindMemory(to: UInt8.self).baseAddress!
        for (i, (ox, oy)) in [(0, 0), (w - t, 0), (0, h - t), (w - t, h - t)].enumerated() {
            for y in 0..<t {
                memcpy(dst + (i * t * t + y * t) * 4, base + (oy + y) * stride + ox * 4, t * 4)
            }
        }
    }
    var hash: UInt64 = 0xcbf29ce484222325
    for b in tiles { hash = (hash ^ UInt64(b)) &* 0x100000001b3 }
    return (t, tiles, hash)
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
    for x in stride(from: x0, to: x1, by: 2) { sample(x, y0 - pad); sample(x, y1 + pad) }
    for y in stride(from: y0, to: y1, by: 2) { sample(x0 - pad, y); sample(x1 + pad, y) }
    guard !samples.isEmpty else { return nil }
    func median(_ k: (UInt8, UInt8, UInt8) -> UInt8) -> UInt8 { samples.map(k).sorted()[samples.count / 2] }
    let (bb, bgc, br) = (median { $0.0 }, median { $0.1 }, median { $0.2 })
    for y in y0..<y1 {
        let row = base + y * stride
        for x in x0..<x1 { let p = row + x * 4; p[0] = bb; p[1] = bgc; p[2] = br; p[3] = 255 }
    }
    return (Int(br) * 3 + Int(bgc) * 6 + Int(bb)) / 10 < 128
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
