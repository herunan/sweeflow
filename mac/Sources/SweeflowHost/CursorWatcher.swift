import AppKit

/// Watches the system cursor image (I-beam, pointing hand, resize arrows...) and reports changes,
/// so the client can draw the Mac's cursor shape locally with zero latency.
final class CursorWatcher {
    private var timer: Timer?
    private var lastHash: UInt64 = 0
    private let scale: CGFloat
    private let onChange: (_ png: Data, _ hotspot: CGPoint, _ scale: CGFloat) -> Void

    init(scale: CGFloat, onChange: @escaping (Data, CGPoint, CGFloat) -> Void) {
        self.scale = max(1, scale.rounded())
        self.onChange = onChange
    }

    func start() {
        DispatchQueue.main.async {
            let t = Timer(timeInterval: 1.0 / 60, repeats: true) { [weak self] _ in self?.poll() }
            RunLoop.main.add(t, forMode: .common)
            self.timer = t
        }
    }

    func stop() {
        DispatchQueue.main.async { self.timer?.invalidate(); self.timer = nil }
    }

    private func poll() {
        guard let cursor = NSCursor.currentSystem else { return }
        let image = cursor.image
        let size = image.size
        guard size.width > 0, size.height > 0 else { return }
        let pw = Int((size.width * scale).rounded()), ph = Int((size.height * scale).rounded())
        guard let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: pw, pixelsHigh: ph, bitsPerSample: 8,
                                         samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
                                         bytesPerRow: pw * 4, bitsPerPixel: 32) else { return }
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
        image.draw(in: NSRect(x: 0, y: 0, width: pw, height: ph))
        NSGraphicsContext.restoreGraphicsState()
        guard let bytes = rep.bitmapData else { return }

        // FNV-1a over the pixels plus hotspot: cheap enough at 60 Hz for cursor-sized images.
        var h: UInt64 = 0xcbf29ce484222325
        let hot = cursor.hotSpot
        for v in [UInt64(hot.x * 100), UInt64(hot.y * 100), UInt64(pw), UInt64(ph)] { h = (h ^ v) &* 0x100000001b3 }
        for i in 0..<(pw * ph * 4) { h = (h ^ UInt64(bytes[i])) &* 0x100000001b3 }
        guard h != lastHash else { return }
        lastHash = h
        guard let png = rep.representation(using: .png, properties: [:]) else { return }
        onChange(png, hot, scale)
    }
}
