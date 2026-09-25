import AppKit
import ApplicationServices

/// Windows that refuse to be captured (security prompts, sign-in verifiers: sharing type "none").
/// They can't be streamed, but their text and buttons can be read and pressed through the
/// Accessibility API, so Linux shows a native stand-in dialog for them.
enum BlindWindows {
    struct Content: CustomDebugStringConvertible {
        var title: String
        var texts: [String]
        var buttons: [(label: String, element: AXUIElement)]
        var debugDescription: String { "title: \(title)\ntexts: \(texts)\nbuttons: \(buttons.map(\.label))" }
    }

    private static func attr(_ e: AXUIElement, _ a: String) -> CFTypeRef? {
        var v: CFTypeRef?
        return AXUIElementCopyAttributeValue(e, a as CFString, &v) == .success ? v : nil
    }

    /// The window of `pid` at `frame` (global points), read through Accessibility.
    static func read(pid: pid_t, frame: CGRect) -> Content? {
        let app = AXUIElementCreateApplication(pid)
        guard let wins = attr(app, kAXWindowsAttribute) as? [AXUIElement], !wins.isEmpty else { return nil }
        func frameOf(_ e: AXUIElement) -> CGRect {
            var p = CGPoint.zero, s = CGSize.zero
            if let v = attr(e, kAXPositionAttribute) { AXValueGetValue(v as! AXValue, .cgPoint, &p) }
            if let v = attr(e, kAXSizeAttribute) { AXValueGetValue(v as! AXValue, .cgSize, &s) }
            return CGRect(origin: p, size: s)
        }
        let match = wins.first { let f = frameOf($0); return abs(f.minX - frame.minX) < 3 && abs(f.minY - frame.minY) < 3 } ?? wins[0]
        return read(match)
    }

    /// Every window of `pid`, read through Accessibility.
    static func describe(pid: pid_t) -> [Content] {
        let app = AXUIElementCreateApplication(pid)
        guard let wins = attr(app, kAXWindowsAttribute) as? [AXUIElement] else { return [] }
        return wins.map(read)
    }

    static func read(_ window: AXUIElement) -> Content {
        var c = Content(title: attr(window, kAXTitleAttribute) as? String ?? "", texts: [], buttons: [])
        func walk(_ e: AXUIElement, _ depth: Int) {
            guard depth < 14 else { return }
            let role = attr(e, kAXRoleAttribute) as? String ?? ""
            let sub = attr(e, kAXSubroleAttribute) as? String ?? ""
            if role == kAXButtonRole, !["AXCloseButton", "AXMinimizeButton", "AXZoomButton", "AXFullScreenButton"].contains(sub) {
                let label = [attr(e, kAXTitleAttribute), attr(e, kAXDescriptionAttribute)].compactMap { $0 as? String }.first { !$0.isEmpty }
                if let label { c.buttons.append((label, e)) }
            } else if role == kAXStaticTextRole || role == kAXTextAreaRole {
                if let v = attr(e, kAXValueAttribute) as? String, !v.trimmingCharacters(in: .whitespaces).isEmpty { c.texts.append(v) }
            }
            for child in (attr(e, kAXChildrenAttribute) as? [AXUIElement]) ?? [] { walk(child, depth + 1) }
        }
        walk(window, 0)
        return c
    }
}
