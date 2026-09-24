import AppKit
import CoreGraphics
import Carbon.HIToolbox

/// Injects pointer and keyboard events into the Mac session.
final class InputInjector {
    private let source = CGEventSource(stateID: .privateState)
    private var buttonsDown: Set<Int> = []
    private var keysDown: Set<UInt32> = []
    private var flags: CGEventFlags = []
    private(set) var lastPoint = CGPoint.zero

    // Click-count tracking so double/triple clicks register.
    private var lastClickAt: TimeInterval = 0
    private var lastClickPoint = CGPoint.zero
    private var lastClickButton = -1
    private var clickCount = 1

    init() {
        // Remote events should not be suppressed by (absent) local hardware state.
        source?.localEventsSuppressionInterval = 0
    }

    func move(to p: CGPoint) {
        lastPoint = p
        let type: CGEventType
        let button: CGMouseButton
        if buttonsDown.contains(0) { type = .leftMouseDragged; button = .left }
        else if buttonsDown.contains(1) { type = .rightMouseDragged; button = .right }
        else if let b = buttonsDown.first { type = .otherMouseDragged; button = CGMouseButton(rawValue: UInt32(b))! }
        else { type = .mouseMoved; button = .left }
        post(CGEvent(mouseEventSource: source, mouseType: type, mouseCursorPosition: p, mouseButton: button))
    }

    func button(_ b: Int, pressed: Bool, at p: CGPoint) {
        lastPoint = p
        let (down, up, cgButton): (CGEventType, CGEventType, CGMouseButton) = switch b {
        case 0: (.leftMouseDown, .leftMouseUp, .left)
        case 1: (.rightMouseDown, .rightMouseUp, .right)
        default: (.otherMouseDown, .otherMouseUp, CGMouseButton(rawValue: UInt32(b)) ?? .center)
        }
        if pressed {
            let now = ProcessInfo.processInfo.systemUptime
            if b == lastClickButton, now - lastClickAt < NSEvent.doubleClickInterval,
               hypot(p.x - lastClickPoint.x, p.y - lastClickPoint.y) < 5 {
                clickCount += 1
            } else {
                clickCount = 1
            }
            lastClickAt = now; lastClickPoint = p; lastClickButton = b
            buttonsDown.insert(b)
        } else {
            guard buttonsDown.remove(b) != nil else { return }
        }
        guard let e = CGEvent(mouseEventSource: source, mouseType: pressed ? down : up,
                              mouseCursorPosition: p, mouseButton: cgButton) else { return }
        e.setIntegerValueField(.mouseEventClickState, value: Int64(clickCount))
        post(e)
    }

    /// `unit` 0 = wheel lines, 1 = pixels (touchpad); `phase` 0 none, 1 began, 2 changed, 3 ended.
    func scroll(dx: CGFloat, dy: CGFloat, unit: UInt8, phase: UInt8, at p: CGPoint) {
        if p != lastPoint { move(to: p) }
        if unit == 0 {
            // Mouse wheel (in notches, fractional for high-resolution wheels): glide.
            wheel.add(dx: dx * Self.pointsPerNotch, dy: dy * Self.pointsPerNotch)
            return
        }
        trackpad.feed(dx: dx, dy: dy, phase: phase)
    }

    private lazy var trackpad = TrackpadScroller(
        post: { [weak self] dx, dy, phase in self?.postPixelScroll(dx: dx, dy: dy, phase: phase) },
        momentum: { [weak self] dx, dy, phase in self?.postMomentum(dx: dx, dy: dy, phase: phase) })

    /// Glide after the fingers lift: momentumPhase 1 begin, 2 continue, 3 end (no scroll phase).
    private func postMomentum(dx: CGFloat, dy: CGFloat, phase: Int64) {
        guard let e = CGEvent(scrollWheelEvent2Source: source, units: .pixel, wheelCount: 2,
                              wheel1: Int32(dy.rounded()), wheel2: Int32(dx.rounded()), wheel3: 0) else { return }
        e.setIntegerValueField(.scrollWheelEventIsContinuous, value: 1)
        e.setDoubleValueField(.scrollWheelEventFixedPtDeltaAxis1, value: Double(dy))
        e.setDoubleValueField(.scrollWheelEventFixedPtDeltaAxis2, value: Double(dx))
        e.setDoubleValueField(.scrollWheelEventPointDeltaAxis1, value: Double(dy))
        e.setDoubleValueField(.scrollWheelEventPointDeltaAxis2, value: Double(dx))
        e.setIntegerValueField(.scrollWheelEventScrollPhase, value: 0)
        e.setIntegerValueField(.scrollWheelEventMomentumPhase, value: phase)
        post(e)
    }

    /// How far one wheel notch scrolls, in points (about three lines of text).
    static let pointsPerNotch: CGFloat = 60

    private lazy var wheel = SmoothWheel { [weak self] dx, dy, phase in self?.postPixelScroll(dx: dx, dy: dy, phase: phase) }

    /// A continuous (trackpad-style) scroll event: phase 1 began, 2 changed, 3 ended.
    private func postPixelScroll(dx: CGFloat, dy: CGFloat, phase: UInt8) {
        guard let e = CGEvent(scrollWheelEvent2Source: source, units: .pixel, wheelCount: 2,
                              wheel1: Int32(dy.rounded()), wheel2: Int32(dx.rounded()), wheel3: 0) else { return }
        e.setIntegerValueField(.scrollWheelEventIsContinuous, value: 1)
        // Fixed-point deltas keep sub-pixel precision.
        e.setDoubleValueField(.scrollWheelEventFixedPtDeltaAxis1, value: Double(dy))
        e.setDoubleValueField(.scrollWheelEventFixedPtDeltaAxis2, value: Double(dx))
        e.setDoubleValueField(.scrollWheelEventPointDeltaAxis1, value: Double(dy))
        e.setDoubleValueField(.scrollWheelEventPointDeltaAxis2, value: Double(dx))
        let phaseValue: Int64 = switch phase { case 1: 1; case 2: 2; case 3: 4; default: 0 }
        e.setIntegerValueField(.scrollWheelEventScrollPhase, value: phaseValue)
        post(e)
    }

    func key(evdev code: UInt32, pressed: Bool, isRepeat: Bool = false) {
        guard let vk = evdevToMac[code] else { log("unmapped evdev key \(code)"); return }
        if pressed { keysDown.insert(code) } else { keysDown.remove(code) }
        if modifierFlag[vk] != nil {
            // Modifiers are flagsChanged events; recompute from all held modifier keys.
            flags = []
            for k in keysDown { if let m = evdevToMac[k], let f = modifierFlag[m] { flags.insert(f) } }
            if vk == kVK_CapsLock, pressed { flags.insert(.maskAlphaShift) }
            guard let e = CGEvent(keyboardEventSource: source, virtualKey: CGKeyCode(vk), keyDown: pressed) else { return }
            e.type = .flagsChanged
            e.flags = flags
            post(e)
            return
        }
        guard let e = CGEvent(keyboardEventSource: source, virtualKey: CGKeyCode(vk), keyDown: pressed) else { return }
        var f = flags
        if functionKeys.contains(vk) { f.insert(.maskSecondaryFn) }
        if numpadKeys.contains(vk) { f.insert(.maskNumericPad) }
        e.flags = f
        if isRepeat { e.setIntegerValueField(.keyboardEventAutorepeat, value: 1) }
        post(e)
    }

    /// Forget held buttons without posting releases (the drag bridge already released them).
    func forgetButtons() { buttonsDown.removeAll() }

    /// Releases everything we believe is held (client lost focus or disconnected).
    func releaseAll() {
        for code in keysDown { key(evdev: code, pressed: false) }
        for b in buttonsDown { button(b, pressed: false, at: lastPoint) }
        keysDown.removeAll(); buttonsDown.removeAll(); flags = []
    }

    private func post(_ e: CGEvent?) { e?.post(tap: .cghidEventTap) }
}

private let modifierFlag: [Int: CGEventFlags] = [
    kVK_Shift: .maskShift, kVK_RightShift: .maskShift,
    kVK_Control: .maskControl, kVK_RightControl: .maskControl,
    kVK_Option: .maskAlternate, kVK_RightOption: .maskAlternate,
    kVK_Command: .maskCommand, kVK_RightCommand: .maskCommand,
    kVK_CapsLock: .maskAlphaShift, kVK_Function: .maskSecondaryFn,
]

private let functionKeys: Set<Int> = [
    kVK_F1, kVK_F2, kVK_F3, kVK_F4, kVK_F5, kVK_F6, kVK_F7, kVK_F8, kVK_F9, kVK_F10, kVK_F11, kVK_F12,
    kVK_Home, kVK_End, kVK_PageUp, kVK_PageDown, kVK_ForwardDelete, kVK_Help,
    kVK_LeftArrow, kVK_RightArrow, kVK_UpArrow, kVK_DownArrow,
]

private let numpadKeys: Set<Int> = [
    kVK_ANSI_Keypad0, kVK_ANSI_Keypad1, kVK_ANSI_Keypad2, kVK_ANSI_Keypad3, kVK_ANSI_Keypad4, kVK_ANSI_Keypad5,
    kVK_ANSI_Keypad6, kVK_ANSI_Keypad7, kVK_ANSI_Keypad8, kVK_ANSI_Keypad9, kVK_ANSI_KeypadDecimal,
    kVK_ANSI_KeypadMultiply, kVK_ANSI_KeypadPlus, kVK_ANSI_KeypadClear, kVK_ANSI_KeypadDivide,
    kVK_ANSI_KeypadEnter, kVK_ANSI_KeypadMinus, kVK_ANSI_KeypadEquals,
    kVK_LeftArrow, kVK_RightArrow, kVK_UpArrow, kVK_DownArrow,
]

/// Linux evdev keycodes (linux/input-event-codes.h) → macOS virtual keycodes. Positional:
/// the Mac's own keyboard layout decides the character, so set it to match (e.g. British).
let evdevToMac: [UInt32: Int] = [
    1: kVK_Escape,
    2: kVK_ANSI_1, 3: kVK_ANSI_2, 4: kVK_ANSI_3, 5: kVK_ANSI_4, 6: kVK_ANSI_5,
    7: kVK_ANSI_6, 8: kVK_ANSI_7, 9: kVK_ANSI_8, 10: kVK_ANSI_9, 11: kVK_ANSI_0,
    12: kVK_ANSI_Minus, 13: kVK_ANSI_Equal, 14: kVK_Delete, 15: kVK_Tab,
    16: kVK_ANSI_Q, 17: kVK_ANSI_W, 18: kVK_ANSI_E, 19: kVK_ANSI_R, 20: kVK_ANSI_T,
    21: kVK_ANSI_Y, 22: kVK_ANSI_U, 23: kVK_ANSI_I, 24: kVK_ANSI_O, 25: kVK_ANSI_P,
    26: kVK_ANSI_LeftBracket, 27: kVK_ANSI_RightBracket, 28: kVK_Return, 29: kVK_Control,
    30: kVK_ANSI_A, 31: kVK_ANSI_S, 32: kVK_ANSI_D, 33: kVK_ANSI_F, 34: kVK_ANSI_G,
    35: kVK_ANSI_H, 36: kVK_ANSI_J, 37: kVK_ANSI_K, 38: kVK_ANSI_L, 39: kVK_ANSI_Semicolon,
    40: kVK_ANSI_Quote, 41: kVK_ANSI_Grave, 42: kVK_Shift, 43: kVK_ANSI_Backslash,
    44: kVK_ANSI_Z, 45: kVK_ANSI_X, 46: kVK_ANSI_C, 47: kVK_ANSI_V, 48: kVK_ANSI_B,
    49: kVK_ANSI_N, 50: kVK_ANSI_M, 51: kVK_ANSI_Comma, 52: kVK_ANSI_Period, 53: kVK_ANSI_Slash,
    54: kVK_RightShift, 55: kVK_ANSI_KeypadMultiply, 56: kVK_Option, 57: kVK_Space, 58: kVK_CapsLock,
    59: kVK_F1, 60: kVK_F2, 61: kVK_F3, 62: kVK_F4, 63: kVK_F5, 64: kVK_F6, 65: kVK_F7,
    66: kVK_F8, 67: kVK_F9, 68: kVK_F10, 87: kVK_F11, 88: kVK_F12,
    183: kVK_F13, 184: kVK_F14, 185: kVK_F15, 186: kVK_F16, 187: kVK_F17, 188: kVK_F18, 189: kVK_F19, 190: kVK_F20,
    69: kVK_ANSI_KeypadClear, 71: kVK_ANSI_Keypad7, 72: kVK_ANSI_Keypad8, 73: kVK_ANSI_Keypad9,
    74: kVK_ANSI_KeypadMinus, 75: kVK_ANSI_Keypad4, 76: kVK_ANSI_Keypad5, 77: kVK_ANSI_Keypad6,
    78: kVK_ANSI_KeypadPlus, 79: kVK_ANSI_Keypad1, 80: kVK_ANSI_Keypad2, 81: kVK_ANSI_Keypad3,
    82: kVK_ANSI_Keypad0, 83: kVK_ANSI_KeypadDecimal, 96: kVK_ANSI_KeypadEnter, 98: kVK_ANSI_KeypadDivide,
    117: kVK_ANSI_KeypadEquals,
    86: kVK_ISO_Section,
    97: kVK_RightControl, 100: kVK_RightOption, 125: kVK_Command, 126: kVK_RightCommand,
    102: kVK_Home, 103: kVK_UpArrow, 104: kVK_PageUp, 105: kVK_LeftArrow, 106: kVK_RightArrow,
    107: kVK_End, 108: kVK_DownArrow, 109: kVK_PageDown, 110: kVK_Help, 111: kVK_ForwardDelete,
    113: kVK_Mute, 114: kVK_VolumeDown, 115: kVK_VolumeUp,
    464: kVK_Function,
    // Apple keyboards' language keys on JIS/Korean layouts.
    122: kVK_JIS_Kana, 123: kVK_JIS_Eisu,
]

/// Turns wheel notches into a short eased glide of pixel scroll events (like smooth scrolling on
/// Linux), animated here on the Mac so network timing can't make it stutter. Notches arriving
/// mid-glide extend it.
final class SmoothWheel {
    private let post: (CGFloat, CGFloat, UInt8) -> Void
    private let queue = DispatchQueue(label: "smooth-wheel", qos: .userInteractive)
    private var remaining = CGVector.zero
    private var timer: DispatchSourceTimer?
    /// Share of the remaining distance covered per 120 Hz tick: ~95 % in about 130 ms.
    private let step: CGFloat = 0.2

    init(post: @escaping (CGFloat, CGFloat, UInt8) -> Void) { self.post = post }

    func add(dx: CGFloat, dy: CGFloat) {
        queue.async {
            // Reversing direction drops what's left of the old glide.
            if self.remaining.dy * dy < 0 { self.remaining.dy = 0 }
            if self.remaining.dx * dx < 0 { self.remaining.dx = 0 }
            self.remaining.dx += dx
            self.remaining.dy += dy
            guard self.timer == nil else { return }
            self.post(0, 0, 1)
            let t = DispatchSource.makeTimerSource(queue: self.queue)
            t.schedule(deadline: .now(), repeating: .nanoseconds(1_000_000_000 / 120), leeway: .microseconds(500))
            t.setEventHandler { [weak self] in self?.tick() }
            t.resume()
            self.timer = t
        }
    }

    private func tick() {
        var dx = remaining.dx * step, dy = remaining.dy * step
        // Don't crawl at the tail: at least half a point per tick, then finish.
        if abs(dx) < 0.5 { dx = abs(remaining.dx) < 0.5 ? remaining.dx : (remaining.dx < 0 ? -0.5 : 0.5) }
        if abs(dy) < 0.5 { dy = abs(remaining.dy) < 0.5 ? remaining.dy : (remaining.dy < 0 ? -0.5 : 0.5) }
        remaining.dx -= dx
        remaining.dy -= dy
        if dx != 0 || dy != 0 { post(dx, dy, 2) }
        if abs(remaining.dx) < 0.01 && abs(remaining.dy) < 0.01 {
            remaining = .zero
            post(0, 0, 3)
            timer?.cancel()
            timer = nil
        }
    }
}

/// Trackpad scrolling from Linux, made to feel like a Mac trackpad: deltas that arrive in
/// network bursts are spread over 120 Hz ticks, and lifting the fingers mid-swipe carries on
/// with momentum (Linux leaves inertia to the app; Mac apps expect it from the system).
final class TrackpadScroller {
    private let post: (CGFloat, CGFloat, UInt8) -> Void
    private let momentum: (CGFloat, CGFloat, Int64) -> Void
    private let queue = DispatchQueue(label: "trackpad-scroll", qos: .userInteractive)
    private var timer: DispatchSourceTimer?
    private var pending = CGVector.zero
    private var touching = false
    private var ending = false
    /// Recent per-tick output, for the release velocity.
    private var recent: [CGVector] = []
    private var velocity = CGVector.zero
    private var coasting = false

    /// Share of the backlog sent per tick (~one tick of smoothing).
    private let drain: CGFloat = 0.55
    /// Momentum kept per 120 Hz tick (close to macOS's own deceleration).
    private let friction: CGFloat = 0.972

    init(post: @escaping (CGFloat, CGFloat, UInt8) -> Void, momentum: @escaping (CGFloat, CGFloat, Int64) -> Void) {
        self.post = post
        self.momentum = momentum
    }

    func feed(dx: CGFloat, dy: CGFloat, phase: UInt8) {
        queue.async {
            if phase == 1 || !self.touching && phase != 3 {
                // Fingers down: stop any glide, start a new gesture.
                if self.coasting { self.momentum(0, 0, 3); self.coasting = false }
                self.touching = true
                self.ending = false
                self.pending = .zero
                self.recent = []
                self.post(0, 0, 1)
            }
            // macOS accelerates trackpad scrolling: slow movements stay precise, quick swipes
            // travel much further. Linux sends raw finger movement, so apply a similar curve
            // (by the size of each ~8 ms step).
            let gain = min(1 + 0.09 * hypot(dx, dy), 3.5)
            self.pending.dx += dx * gain
            self.pending.dy += dy * gain
            if phase == 3 { self.ending = true }
            self.startTimer()
        }
    }

    private func startTimer() {
        guard timer == nil else { return }
        let t = DispatchSource.makeTimerSource(queue: queue)
        t.schedule(deadline: .now(), repeating: .nanoseconds(1_000_000_000 / 120), leeway: .microseconds(500))
        t.setEventHandler { [weak self] in self?.tick() }
        t.resume()
        timer = t
    }

    private func tick() {
        if touching {
            var out = CGVector(dx: pending.dx * drain, dy: pending.dy * drain)
            if abs(pending.dx) < 0.5 { out.dx = pending.dx }
            if abs(pending.dy) < 0.5 { out.dy = pending.dy }
            pending.dx -= out.dx
            pending.dy -= out.dy
            if out.dx != 0 || out.dy != 0 { post(out.dx, out.dy, 2) }
            recent.append(out)
            if recent.count > 6 { recent.removeFirst() }
            if ending && pending == .zero {
                touching = false
                ending = false
                post(0, 0, 3)
                // Release velocity: the average of the last few ticks (~50 ms).
                let n = CGFloat(max(recent.count, 1))
                velocity = CGVector(dx: recent.reduce(0) { $0 + $1.dx } / n, dy: recent.reduce(0) { $0 + $1.dy } / n)
                if hypot(velocity.dx, velocity.dy) > 1.5 {
                    coasting = true
                    momentum(velocity.dx, velocity.dy, 1)
                } else {
                    stopTimer()
                }
            }
            return
        }
        if coasting {
            velocity.dx *= friction
            velocity.dy *= friction
            if hypot(velocity.dx, velocity.dy) < 0.2 {
                momentum(0, 0, 3)
                coasting = false
                stopTimer()
            } else {
                momentum(velocity.dx, velocity.dy, 2)
            }
            return
        }
        stopTimer()
    }

    private func stopTimer() {
        timer?.cancel()
        timer = nil
    }
}
