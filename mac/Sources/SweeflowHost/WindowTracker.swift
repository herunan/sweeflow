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
    /// A popup covering a whole display (see WindowTracker.coversDisplay): Linux shows it as an
    /// overlay that a click or Esc closes, instead of a popup taking the whole screen.
    var overlay = false
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
    struct BlindWindow { let wid: UInt32; let pid: pid_t; let owner: String; let frame: CGRect; var menu = false }
    /// Windows that can't be captured, and callbacks as they come and go.
    private(set) var blind: [UInt32: BlindWindow] = [:]
    var onBlindAdded: ((BlindWindow) -> Void)?
    var onBlindRemoved: ((BlindWindow) -> Void)?
    /// macOS's prompt helpers (not apps, so never streamed): "downloaded from the internet"
    /// checks, password and permission prompts, system alerts. Shown as stand-ins too.
    static let promptOwners: Set<String> = ["CoreServicesUIAgent", "SecurityAgent", "UserNotificationCenter",
                                            "universalAccessAuthWarn", "coreautha", "LocalAuthenticationRemoteService"]
    /// System UI that is unshareable but not a prompt for the user (the lock screen included).
    static let notBlind: Set<String> = ["Window Server", "Dock", "SystemUIServer", "Control Center", "Notification Center",
                                        "Spotlight", "loginwindow", "WindowManager", "Wallpaper", "TextInputMenuAgent"]

    /// Whether a window is one of its app's real windows (in its Accessibility window list).
    /// Set by the session; transient popups (a calendar app's event card) aren't.
    var isAppWindow: ((UInt32, pid_t) -> Bool)?
    /// Whether a window lacks working window buttons (all zero-size): set by the session.
    var hasNoButtons: ((UInt32, pid_t) -> Bool)?
    private var noButtonsSeen: [UInt32: Bool] = [:]
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

    /// Looks at the window list only when something may have changed: in short bursts after
    /// the user does something (see `burst`) and on app events, with a slow fallback for what
    /// happens by itself (a reminder popping up).
    func start() {
        let t = DispatchSource.makeTimerSource(queue: queue)
        t.schedule(deadline: .now(), repeating: .seconds(1), leeway: .milliseconds(100))
        t.setEventHandler { [weak self] in self?.poll() }
        t.resume()
        timer = t
        let nc = NSWorkspace.shared.notificationCenter
        for name in [NSWorkspace.didActivateApplicationNotification, NSWorkspace.didLaunchApplicationNotification,
                     NSWorkspace.didTerminateApplicationNotification, NSWorkspace.didHideApplicationNotification,
                     NSWorkspace.didUnhideApplicationNotification] {
            nc.addObserver(forName: name, object: nil, queue: nil) { [weak self] _ in self?.burst(everyMs: 20) }
        }
    }

    /// Checks closely for a moment (half a second): every 8 ms after a click or key (menus and
    /// popovers open and close then), every 33 ms while the pointer moves.
    func burst(everyMs interval: Int = 8, for seconds: Double = 0.5) {
        queue.async { [self] in
            burstUntil = max(burstUntil, .now() + seconds)
            if let current = burstInterval, current <= interval, burstTimer != nil { return }
            burstTimer?.cancel()
            burstInterval = interval
            let t = DispatchSource.makeTimerSource(queue: queue)
            t.schedule(deadline: .now(), repeating: .milliseconds(interval), leeway: .milliseconds(1))
            t.setEventHandler { [weak self] in
                guard let self else { return }
                self.poll()
                self.onBurstPoll?()
                if DispatchTime.now() > self.burstUntil {
                    self.burstTimer?.cancel()
                    self.burstTimer = nil
                    self.burstInterval = nil
                }
            }
            t.resume()
            burstTimer = t
        }
    }
    /// Also run during bursts (the session checks which window has focus).
    var onBurstPoll: (() -> Void)?
    private var burstTimer: DispatchSourceTimer?
    private var burstInterval: Int?
    private var burstUntil = DispatchTime.now()
    /// Popups' largest size so far, and those that have started their closing animation.
    private var popupMax: [UInt32: CGSize] = [:]
    private var closing: Set<UInt32> = []

    func stop() { timer?.cancel(); timer = nil }

    /// A window over more than one display (a drag's image layer, which covers the whole
    /// desktop while dragging in Finder) is no menu or panel: streamed, it would sit over the
    /// Linux desktop and take the pointer. It covers whole displays; a real window that merely
    /// straddles two (a video-call app's meeting window, left across a display edge) is no drag layer.
    static func spansDisplays(_ r: CGRect) -> Bool {
        var ids = [CGDirectDisplayID](repeating: 0, count: 8)
        var n: UInt32 = 0
        guard CGGetDisplaysWithRect(r, 8, &ids, &n) == .success, n > 1 else { return false }
        return ids.prefix(Int(n)).contains { r.contains(CGDisplayBounds($0)) }
    }

    /// A window covering one whole display (the screenshot UI's, Cmd-Shift-5) is no menu,
    /// popover or panel either: streamed as a plain popup it would cover the Linux screen,
    /// invisible, and take every click. It's sent as an overlay instead, which Linux lets the
    /// user close. A few points short of the display still counts. (Ones over more than one
    /// display are drag layers, see spansDisplays: not streamed at all.)
    static func coversDisplay(_ r: CGRect) -> Bool {
        var ids = [CGDirectDisplayID](repeating: 0, count: 8)
        var n: UInt32 = 0
        guard CGGetDisplaysWithRect(r, 8, &ids, &n) == .success else { return false }
        return ids.prefix(Int(n)).contains { r.insetBy(dx: -4, dy: -4).contains(CGDisplayBounds($0)) }
    }

    /// Processes known to have no app bundle (system helpers owning windows): not asked again.
    private var noBundle: Set<pid_t> = []

    private func appInfo(_ pid: pid_t) -> (String, String)? {
        if let c = bundleCache[pid] { return c }
        if noBundle.contains(pid) { return nil }
        guard let app = NSRunningApplication(processIdentifier: pid), let bid = app.bundleIdentifier else {
            noBundle.insert(pid)
            return nil
        }
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
        var blindNow: [UInt32: BlindWindow] = [:]
        var helperPopups: [(wid: UInt32, pid: pid_t, frame: CGRect, layer: Int)] = []

        for info in list {
            // Windows that refuse capture (security prompts, sign-in verifiers): can't be streamed, but
            // Linux can show a stand-in for them (see BlindWindows). Any app, system ones too.
            if let owner = info[kCGWindowOwnerName as String] as? String,
               (info[kCGWindowSharingState as String] as? Int) == 0 || Self.promptOwners.contains(owner),
               let wid = info[kCGWindowNumber as String] as? UInt32,
               let pid = info[kCGWindowOwnerPID as String] as? pid_t, pid != getpid(),
               let layer = info[kCGWindowLayer as String] as? Int, (0..<1000).contains(layer),
               let bd = info[kCGWindowBounds as String] as? NSDictionary, let b = CGRect(dictionaryRepresentation: bd),
               !Self.notBlind.contains(owner) {
                // An open pop-up menu (layer 101) is small; anything else that small is no dialog.
                let menu = layer == 101 && (info[kCGWindowSharingState as String] as? Int) == 0
                if !Self.promptOwners.contains(owner), Self.coversDisplay(b) {
                    // An overlay (streamed as one, see below), not a dialog: no stand-in.
                } else if menu ? (b.width >= 30 && b.height >= 16) : (b.width >= 150 && b.height >= 80) {
                    blindNow[wid] = BlindWindow(wid: wid, pid: pid, owner: owner, frame: b, menu: menu)
                }
            }
            guard let wid = info[kCGWindowNumber as String] as? UInt32,
                  let pid = info[kCGWindowOwnerPID as String] as? pid_t,
                  let layer = info[kCGWindowLayer as String] as? Int,
                  let boundsDict = info[kCGWindowBounds as String] as? NSDictionary,
                  let bounds = CGRect(dictionaryRepresentation: boundsDict),
                  (info[kCGWindowAlpha as String] as? Double ?? 1) > 0.01 else { continue }
            guard let (bid, appName) = appInfo(pid), allowed(bid) else {
                // A helper process's menu or popover (each System Settings page runs in a process
                // of its own): attached below to the streamed window it opens over.
                if (24..<1000).contains(layer), (info[kCGWindowSharingState as String] as? Int) != 0, pid != getpid(),
                   !Self.spansDisplays(bounds),
                   let owner = info[kCGWindowOwnerName as String] as? String, !Self.notBlind.contains(owner),
                   bounds.width >= 8, bounds.height >= 8 {
                    helperPopups.append((wid, pid, bounds, layer))
                }
                continue
            }
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
                if Self.spansDisplays(bounds) { continue }   // a drag image layer, see spansDisplays
                // Not a real window of the app but a transient one drawn at window level (a calendar app's
                // event card): shown attached to the app window it sits on, like a popover.
                w.kind = .popup
                w.overlay = Self.coversDisplay(bounds)
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
                if Self.spansDisplays(bounds) { continue }
                w.kind = layer < 24 ? .dialog : .popup
                if Self.coversDisplay(bounds), layer >= 24 || title.isEmpty {
                    w.kind = .popup
                    w.overlay = true
                }
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
        // An app's overlay opening from inside its own window (System Settings' search
        // suggestions, which can hang off the bottom): listed as a window, but with no window
        // buttons, starting within a bigger window of the app. Shown attached to it, like a popover.
        for (wid, w) in seen where w.kind == .toplevel {
            let corner = CGPoint(x: w.frame.minX + 1, y: w.frame.minY + 1)
            let host = seen.values.contains {
                $0.wid != wid && $0.pid == w.pid && $0.kind == .toplevel && $0.frame.contains(corner)
                    && $0.frame.width * $0.frame.height > w.frame.width * w.frame.height
            }
            guard host else { continue }
            let none = noButtonsSeen[wid] ?? (hasNoButtons?(wid, w.pid) ?? false)
            noButtonsSeen[wid] = none
            guard none else { continue }
            var p = w
            p.kind = .popup
            popups.append(p)
            seen[wid] = nil
            order.removeAll { $0 == wid }
            if frontToplevel[w.pid]?.wid == wid {
                frontToplevel[w.pid] = order.lazy.compactMap { seen[$0] }.first { $0.pid == w.pid }
            }
        }
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
        for h in helperPopups {
            // An overlay stays with the window it came up over (it covers them all, so whichever
            // is in front would do, and moving it from one to the next re-creates it on Linux).
            let kept = Self.coversDisplay(h.frame) ? windows[h.wid].flatMap { seen[$0.parent] } : nil
            guard let parent = kept ?? owners.first(where: { $0.frame.insetBy(dx: -40, dy: -40).intersects(h.frame) }) else { continue }
            popups.append(TrackedWindow(wid: h.wid, pid: parent.pid, bundleId: parent.bundleId, appName: parent.appName, title: "",
                                        frame: h.frame, layer: h.layer, parent: 0, kind: .popup, overlay: Self.coversDisplay(h.frame)))
        }
        for var p in popups where p.kind == .popup {
            // Closing: macOS shrinks popovers (a calendar app's event card) toward where they opened.
            // Shrinking in both directions at once only happens then, so it's gone from now on
            // (a list that gets shorter shrinks in height only).
            if closing.contains(p.wid) { continue }
            let big = popupMax[p.wid] ?? p.frame.size
            if p.frame.width < big.width * 0.9 && p.frame.height < big.height * 0.9 {
                log("popup \(p.wid) (\(p.appName)) shrank from \(big) to \(p.frame.size): taken as closing")
                closing.insert(p.wid)
                continue
            }
            popupMax[p.wid] = CGSize(width: max(big.width, p.frame.width), height: max(big.height, p.frame.height))
            let near = owners.filter { $0.pid == p.pid && $0.frame.insetBy(dx: -40, dy: -40).intersects(p.frame) }
            // Prefer a panel (drawn above its app's windows), then the frontmost window.
            let kept = p.overlay ? windows[p.wid].flatMap { seen[$0.parent] } : nil
            guard let parent = kept ?? near.first(where: { $0.kind == .dialog }) ?? near.first ?? frontToplevel[p.pid] else { continue }
            p.parent = parent.wid
            p.rel = CGPoint(x: p.frame.minX - parent.frame.minX, y: p.frame.minY - parent.frame.minY)
            seen[p.wid] = p
            order.append(p.wid)
        }

        let live = Set(seen.keys).union(popups.map(\.wid))
        popupMax = popupMax.filter { live.contains($0.key) }
        closing = closing.filter { id in popups.contains { $0.wid == id } }
        appWindowSeen = appWindowSeen.filter { live.contains($0.key) }
        noButtonsSeen = noButtonsSeen.filter { live.contains($0.key) }
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
        for (wid, b) in blind where blindNow[wid] == nil { onBlindRemoved?(b) }
        for (wid, b) in blindNow where blind[wid] == nil { onBlindAdded?(b) }
        blind = blindNow
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
    /// Whether `wid` is what's in front at `point` — or only its own app's menu, tooltip or
    /// popover is (raising the window then would dismiss that menu: a raise counts as a click).
    func isFrontmost(_ wid: UInt32, at point: CGPoint) -> Bool {
        guard let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID)
                as? [[String: Any]] else { return false }
        let pid = windows[wid]?.pid
        for info in list {
            guard let layer = info[kCGWindowLayer as String] as? Int, layer < 1000,
                  let boundsDict = info[kCGWindowBounds as String] as? NSDictionary,
                  let bounds = CGRect(dictionaryRepresentation: boundsDict), bounds.contains(point),
                  (info[kCGWindowAlpha as String] as? Double ?? 1) > 0.01 else { continue }
            let number = info[kCGWindowNumber as String] as? UInt32
            if number == wid { return true }
            let owner = info[kCGWindowOwnerPID as String] as? pid_t
            // Other apps' windows above the normal level (the Dock keeps a transparent one over
            // the whole display) stay on top whatever we raise: they don't count.
            if layer > 0 && owner != pid { continue }
            let isOwnPopup = owner != nil && owner == pid && (layer > 0 || number.flatMap { windows[$0]?.kind } != .toplevel)
            return isOwnPopup
        }
        return false
    }
}
