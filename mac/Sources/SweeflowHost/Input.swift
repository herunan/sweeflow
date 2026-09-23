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
        let units: CGScrollEventUnit = unit == 0 ? .line : .pixel
        guard let e = CGEvent(scrollWheelEvent2Source: source, units: units, wheelCount: 2,
                              wheel1: Int32(dy.rounded()), wheel2: Int32(dx.rounded()), wheel3: 0) else { return }
        if unit == 1 {
            e.setIntegerValueField(.scrollWheelEventIsContinuous, value: 1)
            // Fixed-point deltas keep sub-pixel precision for smooth touchpad scrolling.
            e.setDoubleValueField(.scrollWheelEventFixedPtDeltaAxis1, value: Double(dy))
            e.setDoubleValueField(.scrollWheelEventFixedPtDeltaAxis2, value: Double(dx))
            e.setDoubleValueField(.scrollWheelEventPointDeltaAxis1, value: Double(dy))
            e.setDoubleValueField(.scrollWheelEventPointDeltaAxis2, value: Double(dx))
            let phaseValue: Int64 = switch phase { case 1: 1; case 2: 2; case 3: 4; default: 0 }
            e.setIntegerValueField(.scrollWheelEventScrollPhase, value: phaseValue)
        }
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
