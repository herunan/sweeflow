import AppKit
import CoreGraphics

struct TrackedWindow: Equatable {
    enum Kind: UInt8 { case toplevel = 0, popup = 1, dialog = 2 }

    let wid: UInt32
    let pid: pid_t
    let bundleId: String
    let appName: String
    var title: String
    var frame: CGRect          // global CG coordinates (top-left origin), points
    var layer: Int
    var parent: UInt32         // 0 for toplevels
    var kind: Kind

    var rel: CGPoint = .zero   // offset from parent's origin, for popups
}

/// Polls the window server for on-screen windows of the streamed apps and reports diffs.
/// CGWindowList is used instead of SCShareableContent because it is cheap enough to poll
/// every few frames and returns windows in front-to-back order.
final class WindowTracker {
    var onAdded: ((TrackedWindow) -> Void)?
    var onChanged: ((TrackedWindow) -> Void)?
    var onRemoved: ((TrackedWindow) -> Void)?

    private(set) var windows: [UInt32: TrackedWindow] = [:]
    /// Window ids in front-to-back order from the last poll.
    private(set) var zOrder: [UInt32] = []
    private var timer: DispatchSourceTimer?
    private let queue: DispatchQueue
    private let allowed: (String) -> Bool
    private var bundleCache: [pid_t: (String, String)] = [:]
    /// macOS's "being captured" badges (small windows it attaches to captured windows), by pid.
    private(set) var badges: [pid_t: [CGRect]] = [:]
    /// Whether a window is one of its app's real windows (in its Accessibility window list).
    /// Set by the session; transient popups (a calendar app's event card) aren't.
    var isAppWindow: ((UInt32, pid_t) -> Bool)?
    /// Untitled windows' answers, looked at for their first second (the list can lag a new
    /// window), then kept.
    private var appWindowSeen: [UInt32: (isApp: Bool, first: DispatchTime)] = [:]

    private func looksLikeAppWindow(_ wid: UInt32, pid: pid_t) -> Bool {
        let now = DispatchTime.now()
        if let c = appWindowSeen[wid], now.uptimeNanoseconds - c.first.uptimeNanoseconds > 1_000_000_000 { return c.isApp }
        let isApp = isAppWindow?(wid, pid) ?? true
        appWindowSeen[wid] = (isApp, appWindowSeen[wid]?.first ?? now)
        return isApp
    }

    init(queue: DispatchQueue, allowed: @escaping (String) -> Bool) {
        self.queue = queue
        self.allowed = allowed
    }

    func start(interval: DispatchTimeInterval = .milliseconds(50)) {
        let t = DispatchSource.makeTimerSource(queue: queue)
        t.schedule(deadline: .now(), repeating: interval, leeway: .milliseconds(5))
        t.setEventHandler { [weak self] in self?.poll() }
        t.resume()
        timer = t
    }

    /// Clicks open and close menus and popovers: look closely for a moment (every 8 ms for
    /// half a second), so one that closes vanishes on Linux almost at once.
    func pollFast() {
        fastUntil = .now() + 0.5
        guard fastTimer == nil else { return }
        let t = DispatchSource.makeTimerSource(queue: queue)
        t.schedule(deadline: .now(), repeating: .milliseconds(8), leeway: .milliseconds(1))
        t.setEventHandler { [weak self] in
            guard let self else { return }
            self.poll()
            if DispatchTime.now() > self.fastUntil {
                self.fastTimer?.cancel()
                self.fastTimer = nil
            }
        }
        t.resume()
        fastTimer = t
    }
    private var fastTimer: DispatchSourceTimer?
    private var fastUntil = DispatchTime.now()
    /// Diagnostics: popups' frames during their first moments (to see open/close animations).
    private var popupBorn: [UInt32: DispatchTime] = [:]
    /// Popups that open with a zoom (a calendar app's event card grows from a dot at the click over
    /// half a second, with a bounce): announced once they've stopped changing, at their final
    /// size, rather than followed step by step. Frame and when it last changed.
    private var growing: [UInt32: (frame: CGRect, since: DispatchTime)] = [:]

    /// A new popup still animating open: held until its frame has stayed put for 40 ms (a menu
    /// that opens at full size waits just that; a zooming card waits until it has settled).
    private func stillOpening(_ p: TrackedWindow) -> Bool {
        let now = DispatchTime.now()
        guard var g = growing[p.wid] else {
            growing[p.wid] = (p.frame, now)
            pollFast()   // watch it closely until it settles
            return true
        }
        if g.frame != p.frame {
            g = (p.frame, now)
            growing[p.wid] = g
            pollFast()
            return true
        }
        if now.uptimeNanoseconds - g.since.uptimeNanoseconds < 40_000_000 { return true }
        growing[p.wid] = nil
        return false
    }

    func stop() { timer?.cancel(); timer = nil }

    private func appInfo(_ pid: pid_t) -> (String, String)? {
        if let c = bundleCache[pid] { return c }
        guard let app = NSRunningApplication(processIdentifier: pid), let bid = app.bundleIdentifier else { return nil }
        let info = (bid, app.localizedName ?? bid)
        bundleCache[pid] = info
        return info
    }

    func poll() {
        guard let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID)
                as? [[String: Any]] else { return }
        var seen: [UInt32: TrackedWindow] = [:]
        var order: [UInt32] = []
        // Frontmost toplevel per pid, to parent popups/menus onto.
        var frontToplevel: [pid_t: TrackedWindow] = [:]
        var popups: [TrackedWindow] = []
        var badges: [pid_t: [CGRect]] = [:]

        for info in list {
            guard let wid = info[kCGWindowNumber as String] as? UInt32,
                  let pid = info[kCGWindowOwnerPID as String] as? pid_t,
                  let layer = info[kCGWindowLayer as String] as? Int,
                  let boundsDict = info[kCGWindowBounds as String] as? NSDictionary,
                  let bounds = CGRect(dictionaryRepresentation: boundsDict),
                  (info[kCGWindowAlpha as String] as? Double ?? 1) > 0.01,
                  let (bid, appName) = appInfo(pid), allowed(bid) else { continue }
            // Windows macOS injects into a shared app — the purple "being captured" badge and the
            // live preview it pops up on hover — are not shareable (sharing state 0). Never
            // stream them; remember the badge (layer 0) so it can be erased from captures.
            if (info[kCGWindowSharingState as String] as? Int) == 0 {
                if layer == 0 { badges[pid, default: []].append(bounds) }
                continue
            }
            let title = info[kCGWindowName as String] as? String ?? ""
            var w = TrackedWindow(wid: wid, pid: pid, bundleId: bid, appName: appName, title: title,
                                  frame: bounds, layer: layer, parent: 0, kind: .toplevel)
            if layer == 0, title.isEmpty, !looksLikeAppWindow(wid, pid: pid) {
                // Not a real window of the app but a transient one drawn at window level (a calendar app's
                // event card): shown attached to the app window it sits on, like a popover.
                w.kind = .popup
                popups.append(w)
            } else if layer == 0 {
                guard bounds.width >= 40, bounds.height >= 30 else { continue }
                if frontToplevel[pid] == nil { frontToplevel[pid] = w }
                seen[wid] = w
                order.append(wid)
            } else if layer < 1000, bounds.width >= 4, bounds.height >= 4 {
                // Below the menu-bar level (24) are floating panels, utility and modal panel
                // windows (reminders, palettes, inspectors): real windows of their own, attached
                // to the app. Above are menus, popovers and tooltips.
                w.kind = layer < 24 ? .dialog : .popup
                popups.append(w)
            }
        }
        // Capture badges on floating windows aren't always marked unshareable: recognise them by
        // their fixed size, sitting at the top-left of another window of the same app.
        let candidates = popups
        popups.removeAll { p in
            guard (60...72).contains(p.frame.width), (17...23).contains(p.frame.height) else { return false }
            let others = candidates.filter { $0.wid != p.wid && $0.pid == p.pid } + Array(seen.values.filter { $0.pid == p.pid })
            guard others.contains(where: { abs($0.frame.minX - p.frame.minX) < 30 && abs($0.frame.minY - p.frame.minY) < 30 }) else {
                return false
            }
            badges[p.pid, default: []].append(p.frame)
            return true
        }
        // Untitled floating windows over one of their app's windows belong to it: a search
        // field's suggestions, an event's hover card (a calendar app). Shown attached, like menus.
        // Titled ones (a reminder, an inspector) stay windows of their own.
        let appWindows = Array(seen.values)
        for i in popups.indices where popups[i].kind == .dialog && popups[i].title.isEmpty {
            let p = popups[i]
            if appWindows.contains(where: { $0.pid == p.pid && $0.frame.intersects(p.frame) }) {
                popups[i].kind = .popup
            }
        }
        // Floating panels (dialogs) are windows in their own right; menus go last.
        for d in popups where d.kind == .dialog {
            var d = d
            if let parent = frontToplevel[d.pid] {
                d.parent = parent.wid
                d.rel = CGPoint(x: d.frame.minX - parent.frame.minX, y: d.frame.minY - parent.frame.minY)
            }
            seen[d.wid] = d
            order.append(d.wid)
        }
        // Each menu belongs to the frontmost window of its app that it touches (a panel's own
        // dropdown must not be positioned relative to the app's main window).
        let owners = order.compactMap { seen[$0] }   // front-to-back: toplevels, then panels
        for var p in popups where p.kind == .popup {
            if windows[p.wid] == nil, stillOpening(p) { continue }
            let near = owners.filter { $0.pid == p.pid && $0.frame.insetBy(dx: -40, dy: -40).intersects(p.frame) }
            // Prefer a panel (drawn above its app's windows), then the frontmost window.
            guard let parent = near.first(where: { $0.kind == .dialog }) ?? near.first ?? frontToplevel[p.pid] else { continue }
            p.parent = parent.wid
            p.rel = CGPoint(x: p.frame.minX - parent.frame.minX, y: p.frame.minY - parent.frame.minY)
            seen[p.wid] = p
            order.append(p.wid)
        }

        let live = Set(seen.keys).union(popups.map(\.wid))
        growing = growing.filter { live.contains($0.key) }
        appWindowSeen = appWindowSeen.filter { live.contains($0.key) }
        for (wid, old) in windows where seen[wid] == nil {
            if old.kind == .popup, let born = popupBorn.removeValue(forKey: wid) {
                log("popup \(wid) gone after \((DispatchTime.now().uptimeNanoseconds - born.uptimeNanoseconds) / 1_000_000) ms")
            }
            windows[wid] = nil
            onRemoved?(old)
        }
        // Parents before children so the client can attach popups.
        for wid in order {
            let w = seen[wid]!
            if w.kind == .popup {
                let born = popupBorn[wid] ?? .now()
                popupBorn[wid] = born
                let ms = (DispatchTime.now().uptimeNanoseconds - born.uptimeNanoseconds) / 1_000_000
                if ms < 800, windows[wid]?.frame != w.frame {
                    log("popup \(wid) +\(ms) ms: \(w.frame)")
                }
            }
            if let old = windows[wid] {
                if old != w { windows[wid] = w; onChanged?(w) }
            } else {
                windows[wid] = w
                onAdded?(w)
            }
        }
        zOrder = order
        self.badges = badges
    }

    /// Where macOS's capture badge overlaps `wid`, relative to the window (points).
    func badgeRect(_ wid: UInt32) -> CGRect? {
        guard let w = windows[wid] else { return nil }
        for b in badges[w.pid] ?? [] {
            let i = b.intersection(w.frame)
            // The badge sits at the window's top-left corner.
            guard !i.isNull, i.width > 0, i.height > 0, b.minX - w.frame.minX < 40, b.minY - w.frame.minY < 40 else { continue }
            return i.offsetBy(dx: -w.frame.minX, dy: -w.frame.minY)
        }
        return nil
    }

    /// Current frame of one window straight from the window server (bypasses the poll interval).
    func liveFrame(_ wid: UInt32) -> CGRect? {
        guard let arr = CGWindowListCopyWindowInfo(.optionIncludingWindow, wid) as? [[String: Any]],
              let d = arr.first?[kCGWindowBounds as String] as? NSDictionary else { return nil }
        return CGRect(dictionaryRepresentation: d)
    }

    /// Returns true when `wid` is the topmost window covering `point` (global coords),
    /// i.e. a click there would land on it.
    func isFrontmost(_ wid: UInt32, at point: CGPoint) -> Bool {
        guard let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID)
                as? [[String: Any]] else { return false }
        for info in list {
            guard let layer = info[kCGWindowLayer as String] as? Int, layer < 1000,
                  let boundsDict = info[kCGWindowBounds as String] as? NSDictionary,
                  let bounds = CGRect(dictionaryRepresentation: boundsDict), bounds.contains(point),
                  (info[kCGWindowAlpha as String] as? Double ?? 1) > 0.01 else { continue }
            return (info[kCGWindowNumber as String] as? UInt32) == wid
        }
        return false
    }
}
