import Foundation
import IOKit

/// The Mac's own scroll acceleration, reproduced from IOHIDFamily (IOHIDScrollAccelerator with
/// IOHIDParametricAcceleration, the algorithm macOS uses for Apple trackpads and mice): the
/// device's curve set (`HIDScrollAccelCurves` in the IORegistry) interpolated at the user's
/// scroll speed setting, applied to the average of the last few movements. So scrolling from
/// Linux accelerates the way the Mac would, with the user's own settings.
struct ScrollAcceleration {
    struct Curve {
        var index = 0.0, gainLinear = 0.0, gainParabolic = 0.0, gainCubic = 0.0, gainQuartic = 0.0
        var tangentLinear = 0.0, tangentRoot = 0.0
    }
    private let c: Curve
    private var m0 = 0.0, b0 = 0.0, m1 = 0.0, b1 = 0.0
    private var t0 = Double.greatestFiniteMagnitude, t1 = Double.greatestFiniteMagnitude
    /// IOHIDFamily's device scale: scroll resolution (units per inch) over report rate.
    private let deviceScale = 400.0 / 67.0

    private static func poly(_ c: Curve, _ x: Double) -> Double {
        c.gainLinear * x + pow(c.gainParabolic * x, 2) + pow(c.gainCubic * x, 3) + pow(c.gainQuartic * x, 4)
    }

    init(_ curve: Curve) {
        let c = curve
        self.c = c
        func poly(_ x: Double) -> Double { Self.poly(c, x) }
        // As IOHIDParametricAcceleration::CreateWithParameters: a polynomial up to the first
        // tangent speed, then its tangent line, then a square-root tail (it never flattens).
        if c.tangentLinear != 0 {
            let x = c.tangentLinear
            m0 = c.gainLinear + 2 * x * pow(c.gainParabolic, 2) + 3 * x * x * pow(c.gainCubic, 3) + 4 * pow(x, 3) * pow(c.gainQuartic, 4)
            b0 = poly(x) - m0 * x
            t0 = x
            if c.tangentRoot != 0 {
                let y1 = m0 * c.tangentRoot + b0
                m1 = 2 * y1 * m0
                b1 = y1 * y1 - m1 * c.tangentRoot
                t1 = c.tangentRoot
            }
        }
    }

    /// IOHIDParametricAcceleration::multiplier (without its constant cursor scale).
    private func output(_ velocity: Double) -> Double {
        let x = velocity / deviceScale
        if x <= t0 { return Self.poly(c, x) }
        if x <= t1 { return m0 * x + b0 }
        return sqrt(max(m1 * x + b1, 0))
    }

    /// How much to scale a movement at `velocity` (device units), relative to slow movement,
    /// so the slowest scrolling stays 1:1 and faster scrolling is multiplied the Mac's way.
    func gain(_ velocity: Double) -> Double {
        let v = max(velocity, 0.01)
        let slope = c.gainLinear / deviceScale
        return slope > 0 ? output(v) / v / slope : 1
    }

    // MARK: loading

    enum Device { case trackpad, mouse }

    /// The curves as last loaded, for all sessions. Loading walks the whole I/O registry, which
    /// took seconds under load; done as each session started, on the queue every session shares
    /// (input included), a computer asking every few seconds whether the Mac was free (one
    /// linked but not streaming) held up clicks for that long. So it's done off that queue,
    /// when Sweeflow starts and again as a computer connects (the setting may have changed).
    private static let lock = NSLock()
    private static var loaded: [Device: ScrollAcceleration] = [:]
    private static var loading = false

    static func current(_ device: Device) -> ScrollAcceleration {
        lock.withLock { loaded[device] } ?? ScrollAcceleration(device == .trackpad ? defaultTrackpad : defaultMouse)
    }

    static func reload() {
        guard lock.withLock({ let was = loading; loading = true; return !was }) else { return }
        DispatchQueue.global(qos: .utility).async {
            let both = [Device.trackpad, .mouse].map { ($0, load($0)) }
            lock.withLock {
                for (d, a) in both { loaded[d] = a }
                loading = false
            }
        }
    }

    private static func load(_ device: Device) -> ScrollAcceleration {
        let (typeName, prefKey, fallback) = device == .trackpad
            ? ("HIDTrackpadScrollAcceleration", "com.apple.trackpad.scrolling", defaultTrackpad)
            : ("HIDMouseScrollAcceleration", "com.apple.scrollwheel.scaling", defaultMouse)
        let setting = (CFPreferencesCopyAppValue(prefKey as CFString, kCFPreferencesAnyApplication) as? NSNumber)?.doubleValue ?? 0.3125
        guard let curves = registryCurves(type: typeName), let curve = interpolate(curves, at: setting) else {
            log("scroll acceleration (\(device)): built-in default curve")
            return ScrollAcceleration(fallback)
        }
        log(String(format: "scroll acceleration (%@): setting %.4f, linear %.3f parabolic %.3f tangents %.2f/%.2f",
                   "\(device)", setting, curve.gainLinear, curve.gainParabolic, curve.tangentLinear, curve.tangentRoot))
        return ScrollAcceleration(curve)
    }

    /// The `HIDScrollAccelCurves` of a service whose scroll acceleration type is `type`.
    private static func registryCurves(type: String) -> [Curve]? {
        var it: io_iterator_t = 0
        let root = IORegistryGetRootEntry(kIOMainPortDefault)
        defer { IOObjectRelease(root) }
        guard IORegistryEntryCreateIterator(root, kIOServicePlane, IOOptionBits(kIORegistryIterateRecursively), &it) == KERN_SUCCESS else { return nil }
        defer { IOObjectRelease(it) }
        while case let e = IOIteratorNext(it), e != 0 {
            defer { IOObjectRelease(e) }
            guard let t = IORegistryEntryCreateCFProperty(e, "HIDScrollAccelerationType" as CFString, nil, 0)?.takeRetainedValue() as? String,
                  t == type,
                  let arr = IORegistryEntryCreateCFProperty(e, "HIDScrollAccelCurves" as CFString, nil, 0)?.takeRetainedValue() as? [[String: Any]]
            else { continue }
            func f(_ d: [String: Any], _ k: String) -> Double { ((d[k] as? NSNumber)?.doubleValue ?? 0) / 65536 }
            let curves = arr.map { d in
                Curve(index: f(d, "HIDAccelIndex"), gainLinear: f(d, "HIDAccelGainLinear"), gainParabolic: f(d, "HIDAccelGainParabolic"),
                      gainCubic: f(d, "HIDAccelGainCubic"), gainQuartic: f(d, "HIDAccelGainQuartic"),
                      tangentLinear: f(d, "HIDAccelTangentSpeedLinear"), tangentRoot: f(d, "HIDAccelTangentSpeedParabolicRoot"))
            }
            if !curves.isEmpty { return curves }
        }
        return nil
    }

    /// The curve for a setting between two defined ones is blended linearly, as IOHIDFamily does.
    private static func interpolate(_ curves: [Curve], at setting: Double) -> Curve? {
        guard !curves.isEmpty else { return nil }
        var i = 0
        for (n, c) in curves.enumerated() where setting >= c.index { i = n }
        let a = curves[i]
        guard a.index < setting, i + 1 < curves.count else { return a }
        let b = curves[i + 1]
        let r = (setting - a.index) / (b.index - a.index)
        func mix(_ x: Double, _ y: Double) -> Double { x + r * (y - x) }
        return Curve(index: setting, gainLinear: mix(a.gainLinear, b.gainLinear), gainParabolic: mix(a.gainParabolic, b.gainParabolic),
                     gainCubic: mix(a.gainCubic, b.gainCubic), gainQuartic: mix(a.gainQuartic, b.gainQuartic),
                     tangentLinear: mix(a.tangentLinear, b.tangentLinear), tangentRoot: mix(a.tangentRoot, b.tangentRoot))
    }

    /// macOS's curves at the default speed (0.3125), in case the registry can't be read.
    static let defaultTrackpad = Curve(index: 0.3125, gainLinear: 0.925, gainParabolic: 0.75, tangentLinear: 6.3, tangentRoot: 12)
    static let defaultMouse = Curve(index: 0.3125, gainLinear: 1.6, gainParabolic: 0.6, tangentLinear: 11, tangentRoot: 18)
}

/// IOHIDScrollAccelerator's velocity: the average movement of the last events (up to 9, within
/// half a second, restarting on a change of direction or a pause), times IOHIDFamily's timing
/// polynomial (≈1.36 at trackpad report rates, so taken as constant here: the Linux events'
/// arrival times carry network jitter).
struct ScrollVelocity {
    private var events: [(t: CFTimeInterval, size: Double)] = []
    private var sign = 0.0

    mutating func add(_ size: Double, signed: Double, at t: CFTimeInterval) -> Double {
        let s: Double = signed > 0 ? 1 : -1
        if s != sign || (events.last.map { t - $0.t > 0.15 } ?? true) { events.removeAll() }
        sign = s
        events.append((t, size))
        events.removeAll { t - $0.t > 0.5 }
        if events.count > 9 { events.removeFirst(events.count - 9) }
        let avg = events.reduce(0) { $0 + $1.size } / Double(events.count)
        return 1.36 * avg
    }

    mutating func reset() { events.removeAll() }
}
