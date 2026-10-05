import AppKit
import ApplicationServices
import CPrivate

/// Moves, resizes, raises and closes real Mac windows through the Accessibility API.
/// Used from the session's queue and its Accessibility queue (the cache is locked).
final class WindowControl {
    private var cache: [UInt32: AXUIElement] = [:]
    private let lock = NSLock()

    func element(_ wid: UInt32, pid: pid_t) -> AXUIElement? {
        if let e = lock.withLock({ cache[wid] }) { return e }
        let app = AXUIElementCreateApplication(pid)
        var value: CFTypeRef?
        guard AXUIElementCopyAttributeValue(app, kAXWindowsAttribute as CFString, &value) == .success,
              let wins = value as? [AXUIElement] else { return nil }
        return lock.withLock {
            for w in wins {
                var id: CGWindowID = 0
                if _AXUIElementGetWindow(w, &id) == .success {
                    cache[id] = w
                }
            }
            return cache[wid]
        }
    }

    /// Whether the window still exists for Accessibility: a closed one's element goes invalid
    /// at once, while a minimising one stays valid.
    func exists(_ wid: UInt32, pid: pid_t) -> Bool {
        guard let e = element(wid, pid: pid) else { return false }
        var role: CFTypeRef?
        if AXUIElementCopyAttributeValue(e, kAXRoleAttribute as CFString, &role) == .invalidUIElement {
            forget(wid)
            return false
        }
        return true
    }

    func forget(_ wid: UInt32) { lock.withLock { cache[wid] = nil } }

    @discardableResult
    func setSize(_ wid: UInt32, pid: pid_t, _ size: CGSize) -> Bool {
        guard let e = element(wid, pid: pid) else { return false }
        var s = size
        let v = AXValueCreate(.cgSize, &s)!
        return AXUIElementSetAttributeValue(e, kAXSizeAttribute as CFString, v) == .success
    }

    @discardableResult
    func setPosition(_ wid: UInt32, pid: pid_t, _ origin: CGPoint) -> Bool {
        guard let e = element(wid, pid: pid) else { return false }
        var p = origin
        let v = AXValueCreate(.cgPoint, &p)!
        return AXUIElementSetAttributeValue(e, kAXPositionAttribute as CFString, v) == .success
    }

    /// Whether the window is minimised to the Dock (and so off-screen, out of the stream).
    func isMinimized(_ wid: UInt32, pid: pid_t) -> Bool {
        guard let e = element(wid, pid: pid) else { return false }
        var v: CFTypeRef?
        return AXUIElementCopyAttributeValue(e, kAXMinimizedAttribute as CFString, &v) == .success
            && (v as? Bool) == true
    }

    func unminimize(_ wid: UInt32, pid: pid_t) {
        guard let e = element(wid, pid: pid) else { return }
        AXUIElementSetAttributeValue(e, kAXMinimizedAttribute as CFString, kCFBooleanFalse)
    }

    /// Brings the window to the front and makes its app active so it receives keystrokes.
    func focus(_ wid: UInt32, pid: pid_t) {
        if let e = element(wid, pid: pid) {
            AXUIElementPerformAction(e, kAXRaiseAction as CFString)
            AXUIElementSetAttributeValue(e, kAXMainAttribute as CFString, kCFBooleanTrue)
        }
        NSRunningApplication(processIdentifier: pid)?.activate(options: [])
    }

    func close(_ wid: UInt32, pid: pid_t) {
        guard let e = element(wid, pid: pid) else { return }
        var button: CFTypeRef?
        if AXUIElementCopyAttributeValue(e, kAXCloseButtonAttribute as CFString, &button) == .success, let b = button {
            AXUIElementPerformAction(b as! AXUIElement, kAXPressAction as CFString)
        }
    }

    /// Frames of the close / minimise / zoom buttons relative to the window origin, in points
    /// (`.zero` when a button is absent).
    func buttonRects(_ wid: UInt32, pid: pid_t) -> [CGRect] {
        guard let e = element(wid, pid: pid) else { return [.zero, .zero, .zero] }
        // Measure against the window's own AX position, read now: the window-server frame we
        // track can be stale while the window is moving (e.g. onto the virtual display).
        var posRef: CFTypeRef?
        var windowOrigin = CGPoint.zero
        guard AXUIElementCopyAttributeValue(e, kAXPositionAttribute as CFString, &posRef) == .success, let pr = posRef,
              AXValueGetValue(pr as! AXValue, .cgPoint, &windowOrigin) else { return [.zero, .zero, .zero] }
        return [kAXCloseButtonAttribute, kAXMinimizeButtonAttribute, kAXZoomButtonAttribute].map { attr in
            var button: CFTypeRef?
            guard AXUIElementCopyAttributeValue(e, attr as CFString, &button) == .success, let b = button else { return .zero }
            let el = b as! AXUIElement
            var pos: CFTypeRef?, size: CFTypeRef?
            guard AXUIElementCopyAttributeValue(el, kAXPositionAttribute as CFString, &pos) == .success,
                  AXUIElementCopyAttributeValue(el, kAXSizeAttribute as CFString, &size) == .success else { return .zero }
            var p = CGPoint.zero, sz = CGSize.zero
            AXValueGetValue(pos as! AXValue, .cgPoint, &p)
            AXValueGetValue(size as! AXValue, .cgSize, &sz)
            return CGRect(x: p.x - windowOrigin.x, y: p.y - windowOrigin.y, width: sz.width, height: sz.height)
        }
    }

    /// Diagnostics: the window's AX frame and every button-like child with its frame.
    func debugDescribe(_ wid: UInt32, pid: pid_t) -> String {
        guard let e = element(wid, pid: pid) else { return "no AX element" }
        func frame(_ el: AXUIElement) -> CGRect {
            var pos: CFTypeRef?, size: CFTypeRef?
            var p = CGPoint.zero, s = CGSize.zero
            if AXUIElementCopyAttributeValue(el, kAXPositionAttribute as CFString, &pos) == .success { AXValueGetValue(pos as! AXValue, .cgPoint, &p) }
            if AXUIElementCopyAttributeValue(el, kAXSizeAttribute as CFString, &size) == .success { AXValueGetValue(size as! AXValue, .cgSize, &s) }
            return CGRect(origin: p, size: s)
        }
        var out = "ax window \(frame(e))\n"
        for attr in [kAXCloseButtonAttribute, kAXMinimizeButtonAttribute, kAXZoomButtonAttribute, kAXFullScreenButtonAttribute] {
            var v: CFTypeRef?
            if AXUIElementCopyAttributeValue(e, attr as CFString, &v) == .success, let b = v {
                out += "\(attr): \(frame(b as! AXUIElement))\n"
            } else { out += "\(attr): none\n" }
        }
        return out
    }

    /// The window the user is typing into on the Mac, if any.
    func focusedWindowID() -> UInt32? {
        guard let app = NSWorkspace.shared.frontmostApplication else { return nil }
        let ax = AXUIElementCreateApplication(app.processIdentifier)
        var value: CFTypeRef?
        guard AXUIElementCopyAttributeValue(ax, kAXFocusedWindowAttribute as CFString, &value) == .success,
              let v = value else { return nil }
        var id: CGWindowID = 0
        return _AXUIElementGetWindow((v as! AXUIElement), &id) == .success ? id : nil
    }
}
