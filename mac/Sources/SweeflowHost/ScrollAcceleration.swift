import Foundation
import IOKit

/// The Mac's own scroll acceleration: the curve its HID system applies for the user's scroll
/// speed setting, read from the IORegistry table (`HIDScrollAccelerationTable`) so scrolling
/// from Linux travels as far as it would on the Mac itself.
struct ScrollAcceleration {
    /// (input speed, output) points; the output is also scaled by `scale`.
    private let points: [(x: Double, y: Double)]
    private let scale: Double

    /// Gain for a scroll speed `x` (points per 1/120 s): how much to multiply the movement by.
    func gain(_ x: Double) -> Double {
        guard x > 0.01, let first = points.first, let last = points.last else { return 1 }
        let y: Double
        if x <= first.x {
            y = first.y * x / first.x
        } else if x >= last.x {
            y = last.y   // the curve flattens out: very fast flicks are capped
        } else {
            let i = points.firstIndex { $0.x >= x }!
            let a = points[i - 1], b = points[i]
            y = a.y + (b.y - a.y) * (x - a.x) / (b.x - a.x)
        }
        return y * scale / x
    }

    /// The trackpad curve for the current setting (System Settings → Trackpad → scroll speed).
    static func trackpad() -> ScrollAcceleration {
        let setting = preference("com.apple.trackpad.scrolling") ?? 0.3125
        if let table = registryTable(), let accel = parse(table, setting: setting) { return accel }
        log("scroll acceleration: using the built-in default curve")
        return fallback
    }

    private static func preference(_ key: String) -> Double? {
        (CFPreferencesCopyAppValue(key as CFString, kCFPreferencesAnyApplication) as? NSNumber)?.doubleValue
    }

    private static func registryTable() -> Data? {
        let root = IORegistryGetRootEntry(kIOMainPortDefault)
        defer { IOObjectRelease(root) }
        return IORegistryEntrySearchCFProperty(root, kIOServicePlane, "HIDScrollAccelerationTable" as CFString, nil,
                                               IOOptionBits(kIORegistryIterateRecursively)) as? Data
    }

    /// Table layout (big-endian): fixed scale, 'USB*', u16 curve count, then per curve a fixed
    /// acceleration setting, u16 point count and (fixed x, fixed y) points. The curve for a
    /// setting between two is blended from its neighbours, as the HID system does.
    static func parse(_ d: Data, setting: Double) -> ScrollAcceleration? {
        let b = [UInt8](d)
        func u16(_ o: Int) -> Int { Int(b[o]) << 8 | Int(b[o + 1]) }
        func fixed(_ o: Int) -> Double { Double(UInt32(b[o]) << 24 | UInt32(b[o + 1]) << 16 | UInt32(b[o + 2]) << 8 | UInt32(b[o + 3])) / 65536 }
        guard b.count >= 10 else { return nil }
        let scale = fixed(0)
        var o = 10
        var curves: [(accel: Double, pts: [(x: Double, y: Double)])] = []
        for _ in 0..<u16(8) {
            guard o + 6 <= b.count else { return nil }
            let accel = fixed(o), n = u16(o + 4)
            o += 6
            guard o + 8 * n <= b.count else { return nil }
            curves.append((accel, (0..<n).map { (fixed(o + 8 * $0), fixed(o + 8 * $0 + 4)) }))
            o += 8 * n
        }
        curves = curves.filter { $0.pts.count > 1 }.sorted { $0.accel < $1.accel }
        guard let lo = curves.last(where: { $0.accel <= setting }) ?? curves.first,
              let hi = curves.first(where: { $0.accel >= setting }) ?? curves.last else { return nil }
        if lo.accel == hi.accel { return ScrollAcceleration(points: lo.pts, scale: scale) }
        // Blend: sample both curves at the lower curve's x positions.
        let t = (setting - lo.accel) / (hi.accel - lo.accel)
        let hiCurve = ScrollAcceleration(points: hi.pts, scale: 1)
        let pts = lo.pts.map { p in (x: p.x, y: p.y * (1 - t) + hiCurve.gain(p.x) * p.x * t) }
        return ScrollAcceleration(points: pts, scale: scale)
    }

    /// macOS's default trackpad curve (scroll speed 0.3125), in case the table can't be read.
    static let fallback = ScrollAcceleration(points: [
        (0.44, 0.30), (1.00, 2.38), (2.00, 6.88), (3.00, 12.38), (4.75, 23.00), (6.75, 36.00), (9.00, 50.00),
        (11.24, 65.40), (13.44, 80.76), (15.67, 96.29), (17.66, 110.30), (19.31, 119.83), (20.81, 126.86),
        (23.48, 134.85), (26.51, 142.62), (29.54, 147.74), (32.87, 151.36), (36.23, 153.55), (39.63, 154.44),
    ], scale: 0.5)
}
