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
    var onRemoved: ((UInt32) -> Void)?

    private(set) var windows: [UInt32: TrackedWindow] = [:]
    /// Window ids in front-to-back order from the last poll.
    private(set) var zOrder: [UInt32] = []
    private var timer: DispatchSourceTimer?
    private let queue: DispatchQueue
    private let allowed: (String) -> Bool
    private var bundleCache: [pid_t: (String, String)] = [:]

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

        for info in list {
            guard let wid = info[kCGWindowNumber as String] as? UInt32,
                  let pid = info[kCGWindowOwnerPID as String] as? pid_t,
                  let layer = info[kCGWindowLayer as String] as? Int,
                  let boundsDict = info[kCGWindowBounds as String] as? NSDictionary,
                  let bounds = CGRect(dictionaryRepresentation: boundsDict),
                  (info[kCGWindowAlpha as String] as? Double ?? 1) > 0.01,
                  let (bid, appName) = appInfo(pid), allowed(bid) else { continue }
            let title = info[kCGWindowName as String] as? String ?? ""
            var w = TrackedWindow(wid: wid, pid: pid, bundleId: bid, appName: appName, title: title,
                                  frame: bounds, layer: layer, parent: 0, kind: .toplevel)
            if layer == 0 {
                guard bounds.width >= 40, bounds.height >= 30 else { continue }
                if frontToplevel[pid] == nil { frontToplevel[pid] = w }
                seen[wid] = w
                order.append(wid)
            } else if layer < 1000, bounds.width >= 4, bounds.height >= 4 {
                w.kind = .popup
                popups.append(w)
            }
        }
        for var p in popups {
            // Menus from the menu bar and similar have no toplevel of their own to attach to.
            guard let parent = frontToplevel[p.pid] else { continue }
            p.parent = parent.wid
            p.rel = CGPoint(x: p.frame.minX - parent.frame.minX, y: p.frame.minY - parent.frame.minY)
            seen[p.wid] = p
            order.append(p.wid)
        }

        for (wid, _) in windows where seen[wid] == nil {
            windows[wid] = nil
            onRemoved?(wid)
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
