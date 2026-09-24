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
            if layer == 0 {
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
        for var p in popups {
            // Menus from the menu bar and similar have no toplevel of their own to attach to.
            guard let parent = frontToplevel[p.pid] else { continue }
            p.parent = parent.wid
            p.rel = CGPoint(x: p.frame.minX - parent.frame.minX, y: p.frame.minY - parent.frame.minY)
            seen[p.wid] = p
            order.append(p.wid)
        }

        for (wid, old) in windows where seen[wid] == nil {
            windows[wid] = nil
            onRemoved?(old)
        }
        // Parents before children so the client can attach popups.
        for wid in order {
            let w = seen[wid]!
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
