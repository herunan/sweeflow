import AppKit
import ApplicationServices

/// One menu item as sent to the client (keys kept short: menus can have thousands of items).
struct MenuItemInfo: Codable {
    var t: String              // title
    var e: Bool = true         // enabled
    var sep: Bool? = nil       // separator
    var k: String? = nil       // key equivalent character
    var m: Int? = nil          // AX modifier mask: 1 shift, 2 option, 4 control, 8 = no ⌘
    var c: Bool? = nil         // checked
    var sub: [MenuItemInfo]? = nil
}

/// Reads an app's menu bar through Accessibility and presses menu items by path.
enum MenuReader {
    /// Limits keep huge dynamic menus (History, Bookmarks) from stalling the read.
    private static let maxDepth = 4
    private static let maxItems = 4000

    /// The app's menus (menu bar items after the Apple menu), or nil if unavailable.
    static func read(pid: pid_t) -> [MenuItemInfo]? {
        let app = AXUIElementCreateApplication(pid)
        AXUIElementSetMessagingTimeout(app, 0.5)
        guard let bar: AXUIElement = attr(app, kAXMenuBarAttribute) else { return nil }
        let tops: [AXUIElement] = attr(bar, kAXChildrenAttribute) ?? []
        var budget = maxItems
        // Index 0 is the Apple menu (About This Mac, Restart, Shut Down…): not the app's.
        return tops.dropFirst().compactMap { top in
            guard let title: String = attr(top, kAXTitleAttribute), !title.isEmpty else { return nil }
            var item = MenuItemInfo(t: title)
            item.e = attr(top, kAXEnabledAttribute) ?? true
            item.sub = submenu(of: top, depth: 1, budget: &budget)
            return item
        }
    }

    private static func submenu(of el: AXUIElement, depth: Int, budget: inout Int) -> [MenuItemInfo]? {
        guard depth <= maxDepth, budget > 0,
              let menus: [AXUIElement] = attr(el, kAXChildrenAttribute),
              let menu = menus.first(where: { (attr($0, kAXRoleAttribute) as String?) == (kAXMenuRole as String) }),
              let kids: [AXUIElement] = attr(menu, kAXChildrenAttribute) else { return nil }
        var out: [MenuItemInfo] = []
        for k in kids {
            guard budget > 0 else { break }
            budget -= 1
            let title: String = attr(k, kAXTitleAttribute) ?? ""
            let enabled: Bool = attr(k, kAXEnabledAttribute) ?? true
            if title.isEmpty {
                // Untitled, disabled items are separators; skip other untitled ones (custom views).
                if !enabled { out.append(MenuItemInfo(t: "", e: false, sep: true)) }
                continue
            }
            var item = MenuItemInfo(t: title, e: enabled)
            if let ch: String = attr(k, kAXMenuItemCmdCharAttribute), !ch.isEmpty {
                item.k = ch
                item.m = attr(k, kAXMenuItemCmdModifiersAttribute) ?? 0
            }
            if let mark: String = attr(k, kAXMenuItemMarkCharAttribute), !mark.isEmpty { item.c = true }
            item.sub = submenu(of: k, depth: depth + 1, budget: &budget)
            out.append(item)
        }
        return out
    }

    /// Presses the item at `path` (indices as sent by `read`: top-level index counts from the
    /// first menu after the Apple menu; separators and skipped items are not counted).
    static func press(pid: pid_t, path: [Int]) -> Bool {
        let app = AXUIElementCreateApplication(pid)
        AXUIElementSetMessagingTimeout(app, 0.5)
        guard let bar: AXUIElement = attr(app, kAXMenuBarAttribute), let first = path.first else { return false }
        let tops = Array((attr(bar, kAXChildrenAttribute) as [AXUIElement]? ?? []).dropFirst())
            .filter { !((attr($0, kAXTitleAttribute) as String?) ?? "").isEmpty }
        guard first < tops.count else { return false }
        var el = tops[first]
        for idx in path.dropFirst() {
            guard let menus: [AXUIElement] = attr(el, kAXChildrenAttribute),
                  let menu = menus.first(where: { (attr($0, kAXRoleAttribute) as String?) == (kAXMenuRole as String) }),
                  let kids: [AXUIElement] = attr(menu, kAXChildrenAttribute) else { return false }
            // Same filtering as `read`, so indices line up.
            let visible = kids.filter { k in
                let title: String = attr(k, kAXTitleAttribute) ?? ""
                let enabled: Bool = attr(k, kAXEnabledAttribute) ?? true
                return !title.isEmpty || !enabled
            }
            guard idx < visible.count else { return false }
            el = visible[idx]
        }
        return AXUIElementPerformAction(el, kAXPressAction as CFString) == .success
    }

    private static func attr<T>(_ el: AXUIElement, _ name: String) -> T? {
        var v: CFTypeRef?
        guard AXUIElementCopyAttributeValue(el, name as CFString, &v) == .success, let v else { return nil }
        return v as? T
    }
}
