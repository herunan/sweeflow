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
    /// A window went and another of the same app came at the very same frame, in one look at
    /// the window list: (old, new). A tab switched: each tab is a window of its own, and macOS
    /// swaps one for the other in a single step. Without this they are a removal and an
    /// addition.
    var onReplaced: ((TrackedWindow, TrackedWindow) -> Void)?

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
    /// macOS's screenshot selection (Cmd+Shift+4) is up, and a callback as it starts and ends.
    private(set) var screenSelecting = false
    var onScreenSelect: ((Bool) -> Void)?
    /// macOS's screenshot tools: screencapture runs the selection, screencaptureui ("Screenshot")
    /// the toolbar and thumbnails.
    static let captureOwners: Set<String> = ["screencapture", "screencaptureui", "Screenshot"]
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
    /// Whether a window is a sheet ("Save changes?"), hanging from one of its app's windows: set
    /// by the session, asked with hasNoButtons.
    var isSheet: ((UInt32, pid_t) -> Bool)?
    private var sheetSeen: [UInt32: Bool] = [:]
    /// Whether the window is a sheet (one attached to its parent window, as a save sheet is).
    func isKnownSheet(_ wid: UInt32) -> Bool { sheetSeen[wid] == true }
    /// Untitled windows' answers, looked at for their first second (the list can lag a new
    /// window), then kept.
    private var appWindowSeen: [UInt32: (isApp: Bool?, first: DispatchTime)] = [:]
    /// Both questions go to the app through Accessibility, which answers when the app gets to
    /// it (a second, with an app busy opening its windows): asked off this queue, where every
    /// look at the window list, and all input behind it, waited for the answer. One question
    /// per window at a time; the look after the answer uses it.
    private let askQueue = DispatchQueue(label: "sweeflow.windowax", qos: .userInteractive, attributes: .concurrent)
    private var askingApp: Set<UInt32> = []
    private var askingButtons: Set<UInt32> = []

    /// nil while the first answer is on its way.
    private func looksLikeAppWindow(_ wid: UInt32, pid: pid_t) -> Bool? {
        let now = DispatchTime.now()
        if let c = appWindowSeen[wid], let isApp = c.isApp,
           now.uptimeNanoseconds - c.first.uptimeNanoseconds > 1_000_000_000 { return isApp }
        if appWindowSeen[wid] == nil { appWindowSeen[wid] = (nil, now) }
        if askingApp.insert(wid).inserted {
            let ask = isAppWindow
            askQueue.async { [weak self] in
                let isApp = ask?(wid, pid) ?? true
                self?.queue.async {
                    guard let self else { return }
                    self.askingApp.remove(wid)
                    // Kept even when the entry is gone: the look that asked drops it again at its
                    // end (a window still waiting for its answer isn't among the live ones), and
                    // with the answer thrown away for that, no untitled window ever got one: a
                    // sheet on an app's window (a prompt in the TV app) was never shown, the app
                    // left dimmed behind it and deaf to clicks. The next look uses the answer;
                    // if the window has gone meanwhile, that look drops it.
                    let was = self.appWindowSeen[wid]
                    self.appWindowSeen[wid] = (isApp, was?.first ?? .now())
                    if was?.isApp != isApp { self.poll() }
                }
            }
        }
        return appWindowSeen[wid]?.isApp
    }

    private func askNoButtons(_ wid: UInt32, pid: pid_t) {
        guard askingButtons.insert(wid).inserted else { return }
        let ask = hasNoButtons, sheet = isSheet
        askQueue.async { [weak self] in
            let isSheet = sheet?(wid, pid) ?? false
            let none = isSheet || (ask?(wid, pid) ?? false)
            self?.queue.async {
                guard let self else { return }
                self.askingButtons.remove(wid)
                self.noButtonsSeen[wid] = none
                self.sheetSeen[wid] = isSheet
                self.poll()
            }
        }
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
            nc.addObserver(forName: name, object: nil, queue: nil) { [weak self] _ in
                self?.queue.async { self?.lastAppChange = .now() }
                self?.burst(everyMs: 20)
            }
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
                self.poll(then: self.onBurstPoll)
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

    /// Whether a window covers at least one whole display.
    static func coversDisplay(_ r: CGRect) -> Bool {
        var ids = [CGDirectDisplayID](repeating: 0, count: 8)
        var n: UInt32 = 0
        guard CGGetDisplaysWithRect(r, 8, &ids, &n) == .success else { return false }
        return ids.prefix(Int(n)).contains { r.insetBy(dx: -4, dy: -4).contains(CGDisplayBounds($0)) }
    }

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

    /// The window server's list takes a few ms to fetch (and more to bridge): fetched on a
    /// queue of its own, so input on the session queue doesn't wait behind it (during a burst
    /// it otherwise took half the queue's time). At most one fetch at a time.
    private let listQueue = DispatchQueue(label: "sweeflow.windowlist", qos: .userInteractive)
    private var listing = false

    func poll(then done: (() -> Void)? = nil) {
        guard !listing else { return }   // the fetch under way is fresh enough
        listing = true
        listQueue.async { [weak self] in
            let began = DispatchTime.now()
            let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID)
                as? [[String: Any]]
            self?.queue.async {
                guard let self else { return }
                self.listing = false
                if let list { self.lastList = list; self.lastListAt = began; self.update(list) }
                done?()
            }
        }
    }

    private func update(_ list: [[String: Any]]) {
        var seen: [UInt32: TrackedWindow] = [:]
        var order: [UInt32] = []
        // Frontmost toplevel per pid, to parent popups/menus onto.
        var frontToplevel: [pid_t: TrackedWindow] = [:]
        var popups: [TrackedWindow] = []
        // Windows at the window level too small to be windows of their own (sorted out below).
        var small: [TrackedWindow] = []
        var badges: [pid_t: [CGRect]] = [:]
        var blindNow: [UInt32: BlindWindow] = [:]
        var helperPopups: [(wid: UInt32, pid: pid_t, frame: CGRect, layer: Int)] = []
        var selecting = false

        for info in list {
            // The screenshot tools' windows over whole displays: screencapture's selection layer,
            // and one screencaptureui puts over a display (and sometimes leaves there). Neither
            // can be captured; shown, they'd only cover the Linux screen, invisible, and take its
            // clicks. Linux draws the selection itself instead.
            if let owner = info[kCGWindowOwnerName as String] as? String, Self.captureOwners.contains(owner),
               let bd = info[kCGWindowBounds as String] as? NSDictionary, let b = CGRect(dictionaryRepresentation: bd),
               Self.coversDisplay(b) {
                if owner == "screencapture" { selecting = true }
                continue
            }
            // Windows that refuse capture (security prompts, sign-in verifiers): can't be streamed, but
            // Linux can show a stand-in for them (see BlindWindows). Any app, system ones too.
            if let owner = info[kCGWindowOwnerName as String] as? String,
               (info[kCGWindowSharingState as String] as? Int) == 0 || Self.promptOwners.contains(owner),
               let wid = info[kCGWindowNumber as String] as? UInt32,
               let pid = info[kCGWindowOwnerPID as String] as? pid_t, pid != getpid(),
               // The administrator password prompt (SecurityAgent) sits on layer 1000.
               let layer = info[kCGWindowLayer as String] as? Int, (0..<1000).contains(layer) || (layer == 1000 && Self.promptOwners.contains(owner)),
               let bd = info[kCGWindowBounds as String] as? NSDictionary, let b = CGRect(dictionaryRepresentation: bd),
               !Self.notBlind.contains(owner) {
                // An open pop-up menu (layer 101) is small; anything else that small is no dialog.
                let menu = layer == 101 && (info[kCGWindowSharingState as String] as? Int) == 0
                if menu ? (b.width >= 30 && b.height >= 16) : (b.width >= 150 && b.height >= 80) {
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
            // Whether an untitled window is one of the app's own isn't known until the app has
            // answered: a new one waits for that (a moment), one already shown stays as it is.
            // The badge as macOS lists it to the capturing app itself: an ordinary, shareable
            // window titled "Window", of its fixed size (other processes see it unshareable,
            // untitled). Taken for a window too small to stream, it was dropped, and the badge
            // never known: erased blind, and an app's buttons misplaced (see settledButtons).
            // Decided once the app's windows are all known (below): a field the app floats over
            // its window (Finder's rename box under an icon, one line high) can be just as small.
            if layer == 0, (60...72).contains(bounds.width), (17...23).contains(bounds.height) {
                small.append(w)
                continue
            }
            var realWindow = true
            if layer == 0, title.isEmpty {
                if let isApp = looksLikeAppWindow(wid, pid: pid) { realWindow = isApp }
                else if windows[wid] == nil { continue }
            }
            if layer == 0, title.isEmpty, !realWindow {
                if Self.spansDisplays(bounds) { continue }   // a drag image layer, see spansDisplays
                // Not a real window of the app but a transient one drawn at window level (a calendar app's
                // event card): shown attached to the app window it sits on, like a popover.
                w.kind = .popup
                popups.append(w)
            } else if layer == 0 {
                guard bounds.width >= 40, bounds.height >= 30 else {
                    if bounds.width >= 12, bounds.height >= 12 { small.append(w) }
                    continue
                }
                if frontToplevel[pid] == nil { frontToplevel[pid] = w }
                seen[wid] = w
                order.append(wid)
            } else if layer < 1000, bounds.width >= 4, bounds.height >= 4, !Self.spansDisplays(bounds) {
                // Below the menu-bar level (24) are floating panels, utility and modal panel
                // windows (reminders, palettes, inspectors): real windows of their own, attached
                // to the app. Above are menus, popovers and tooltips.
                w.kind = layer < 24 ? .dialog : .popup
                popups.append(w)
            }
        }
        // Small windows at the window level: the capture badge (its fixed size, at the top-left
        // of another window of the app), or a field floated over one of the app's windows
        // (Finder's rename box in icon view, a line high: dropped, the name vanished while it
        // was typed), shown attached to it like a popover. Others are too small to stream.
        for s in small {
            let mine = seen.values.filter { $0.pid == s.pid && $0.wid != s.wid }
            let badgeSized = (60...72).contains(s.frame.width) && (17...23).contains(s.frame.height)
            if badgeSized, mine.contains(where: { abs($0.frame.minX - s.frame.minX) < 30 && abs($0.frame.minY - s.frame.minY) < 30 }) {
                badges[s.pid, default: []].append(s.frame)
            } else if mine.contains(where: { $0.frame.contains(s.frame) }) {
                var p = s
                p.kind = .popup
                popups.append(p)
            } else if badgeSized {
                // Its window not listed (yet): as before, a badge.
                badges[s.pid, default: []].append(s.frame)
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
            // Its top-left corner on it, or the middle of its top edge: a sheet hangs from the
            // middle of its window's top, and sticks out both sides of a window narrower than it
            // (a tile on Linux), taken for a window of its own then.
            let corner = CGPoint(x: w.frame.minX + 1, y: w.frame.minY + 1)
            let top = CGPoint(x: w.frame.midX, y: w.frame.minY + 1)
            let host = seen.values.contains {
                $0.wid != wid && $0.pid == w.pid && $0.kind == .toplevel && ($0.frame.contains(corner) || $0.frame.contains(top))
                    && $0.frame.width * $0.frame.height > w.frame.width * w.frame.height
            }
            guard host else { continue }
            guard let none = noButtonsSeen[wid] else {
                // Not known until the app has answered: a new window waits for that (a moment),
                // one already shown stays a window.
                askNoButtons(wid, pid: w.pid)
                if windows[wid] == nil {
                    seen[wid] = nil
                    order.removeAll { $0 == wid }
                    if frontToplevel[w.pid]?.wid == wid {
                        frontToplevel[w.pid] = order.lazy.compactMap { seen[$0] }.first { $0.pid == w.pid }
                    }
                }
                continue
            }
            guard none else { continue }
            var p = w
            // A sheet (a save sheet) too: attached to its window on Linux, as on the Mac, it
            // moves with it (a tiler scrolling, the window dragged) and keeps its size. Shown
            // as a dialog of its own instead, it stayed behind on Plasma and could be resized.
            p.kind = .popup
            popups.append(p)
            seen[wid] = nil
            order.removeAll { $0 == wid }
            if frontToplevel[w.pid]?.wid == wid {
                frontToplevel[w.pid] = order.lazy.compactMap { seen[$0] }.first { $0.pid == w.pid }
            }
        }
        let appWindows = Array(seen.values)
        for i in popups.indices where popups[i].kind == .dialog && popups[i].title.isEmpty && sheetSeen[popups[i].wid] != true {
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
            guard let parent = owners.first(where: { $0.frame.insetBy(dx: -40, dy: -40).intersects(h.frame) }) else { continue }
            popups.append(TrackedWindow(wid: h.wid, pid: parent.pid, bundleId: parent.bundleId, appName: parent.appName, title: "",
                                        frame: h.frame, layer: h.layer, parent: 0, kind: .popup))
        }
        for var p in popups where p.kind == .popup {
            // Closing: macOS shrinks popovers (a calendar app's event card) toward where they opened.
            // Shrinking in both directions at once only happens then, so it's gone from now on
            // (a list that gets shorter shrinks in height only).
            if closing.contains(p.wid) { continue }
            let big = popupMax[p.wid] ?? p.frame.size
            // Not a floating panel (layers 1–23): Quick Look's opens over its window and then
            // settles to its picture's size, smaller in both directions (on macOS 26 in a VM,
            // 810×543 to 370×399), and was taken down as closing while it stayed up.
            if !(1..<24).contains(p.layer), p.frame.width < big.width * 0.9 && p.frame.height < big.height * 0.9 {
                log("popup \(p.wid) (\(p.appName)) shrank from \(big) to \(p.frame.size): taken as closing")
                closing.insert(p.wid)
                continue
            }
            popupMax[p.wid] = CGSize(width: max(big.width, p.frame.width), height: max(big.height, p.frame.height))
            let near = owners.filter { $0.pid == p.pid && $0.frame.insetBy(dx: -40, dy: -40).intersects(p.frame) }
            // Prefer a panel (drawn above its app's windows), then the frontmost window.
            // A floating panel (Quick Look's) belongs to the app's frontmost window, the one it
            // was opened from: by overlap on this display it went to whichever window it happened
            // to lie over there, a column away on Linux. Not lying wholly on that window (macOS
            // centres the panel on the display), it's shown centred on it.
            let panel = (1..<24).contains(p.layer)
            guard let parent = (panel ? frontToplevel[p.pid] : nil) ?? near.first(where: { $0.kind == .dialog }) ?? near.first ?? frontToplevel[p.pid] else { continue }
            p.parent = parent.wid
            p.rel = CGPoint(x: p.frame.minX - parent.frame.minX, y: p.frame.minY - parent.frame.minY)
            if panel, !parent.frame.contains(p.frame) {
                p.rel = CGPoint(x: ((parent.frame.width - p.frame.width) / 2).rounded(),
                                y: max(0, ((parent.frame.height - p.frame.height) / 3).rounded()))
            }
            seen[p.wid] = p
            order.append(p.wid)
        }

        let live = Set(seen.keys).union(popups.map(\.wid))
        popupMax = popupMax.filter { live.contains($0.key) }
        closing = closing.filter { id in popups.contains { $0.wid == id } }
        appWindowSeen = appWindowSeen.filter { live.contains($0.key) }
        noButtonsSeen = noButtonsSeen.filter { live.contains($0.key) }
        sheetSeen = sheetSeen.filter { live.contains($0.key) }
        // New windows that took a gone one's place (see onReplaced): new wid → the old window.
        var replaced: [UInt32: TrackedWindow] = [:]
        if onReplaced != nil {
            var gone = windows.values.filter { seen[$0.wid] == nil && $0.kind == .toplevel }
            for wid in order {
                guard windows[wid] == nil, let w = seen[wid], w.kind == .toplevel,
                      let i = gone.firstIndex(where: { $0.pid == w.pid && $0.frame == w.frame }) else { continue }
                replaced[wid] = gone.remove(at: i)
            }
        }
        let replacedOld = Set(replaced.values.map(\.wid))
        for (wid, old) in windows where seen[wid] == nil {
            windows[wid] = nil
            if !replacedOld.contains(wid) { onRemoved?(old) }
        }
        // Parents before children so the client can attach popups.
        for wid in order {
            let w = seen[wid]!
            if let old = windows[wid] {
                if old != w { windows[wid] = w; onChanged?(w) }
            } else {
                windows[wid] = w
                if let old = replaced[wid] { onReplaced?(old, w) } else { onAdded?(w) }
            }
        }
        zOrder = order
        self.badges = badges
        // macOS puts the capture badge up some seconds after a capture starts: once it's there,
        // it says where a window's buttons really are (see Session.settledButtons).
        for (wid, w) in windows where !badged.contains(wid) && Self.badge(among: badges[w.pid] ?? [], over: w.frame) != nil {
            badged.insert(wid)
            onBadgeAppeared?(wid)
        }
        badged = badged.filter { windows[$0] != nil }
        for (wid, b) in blind where blindNow[wid] == nil { onBlindRemoved?(b) }
        for (wid, b) in blindNow where blind[wid] == nil { onBlindAdded?(b) }
        blind = blindNow
        if selecting != screenSelecting {
            screenSelecting = selecting
            onScreenSelect?(selecting)
        }
    }

    /// Called once per window when its capture badge is first seen.
    var onBadgeAppeared: ((UInt32) -> Void)?
    private var badged: Set<UInt32> = []

    /// Where macOS's capture badge overlaps `wid`, relative to the window (points).
    func badgeRect(_ wid: UInt32) -> CGRect? {
        guard let w = windows[wid] else { return nil }
        if let r = Self.badge(among: badges[w.pid] ?? [], over: w.frame) { return r }
        // Not there at the last look at the window list. macOS puts the badge up a moment after
        // a capture starts, and that changes the picture: the capture with the badge in it gets
        // here before the next look, and for a window that then stays as it is, that picture
        // (badge and all) was the one Linux kept showing. So the window server is asked again,
        // every two seconds at most per window: off this queue (input goes through it, and the
        // answer took tens of ms with many windows), and a badge found is said (onBadgeFound),
        // so the window's last picture is painted again without it.
        let now = DispatchTime.now().uptimeNanoseconds
        if let asked = badgeAsked[wid], now - asked < 2_000_000_000 { return nil }
        badgeAsked[wid] = now
        let pid = w.pid, frame = w.frame
        listQueue.async { [weak self] in
            guard let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID)
                    as? [[String: Any]] else { return }
            var frame = frame
            var found: [CGRect] = []
            for info in list {
                guard (info[kCGWindowOwnerPID as String] as? pid_t) == pid, (info[kCGWindowLayer as String] as? Int) == 0,
                      let bd = info[kCGWindowBounds as String] as? NSDictionary, let b = CGRect(dictionaryRepresentation: bd) else { continue }
                if (info[kCGWindowNumber as String] as? UInt32) == wid {
                    frame = b
                } else if (60...72).contains(b.width), (17...23).contains(b.height) {
                    found.append(b)   // the badge's fixed size (see update)
                }
            }
            self?.queue.async {
                guard let self, self.windows[wid] != nil else { return }
                guard Self.badge(among: found, over: frame) != nil else {
                    self.badgeAbsentAt[wid] = DispatchTime.now().uptimeNanoseconds   // asked just now, and it isn't there
                    return
                }
                self.badges[pid, default: []].append(contentsOf: found)   // until the next look, which lists them itself
                self.onBadgeFound?(wid)
            }
        }
        return nil
    }

    /// A badge found on a window by asking the window server (badgeRect), on the tracker's queue.
    var onBadgeFound: ((UInt32) -> Void)?

    /// Whether the window server said, within the last moment, that `wid` has no badge on it
    /// (as right after its capture starts): what is captured then is the window as it is.
    func badgeAbsent(_ wid: UInt32) -> Bool {
        guard let at = badgeAbsentAt[wid] else { return false }
        return DispatchTime.now().uptimeNanoseconds - at < 150_000_000
    }
    private var badgeAbsentAt: [UInt32: UInt64] = [:]

    /// When the window server was last asked for a window's badge (badgeRect).
    private var badgeAsked: [UInt32: UInt64] = [:]

    private static func badge(among badges: [CGRect], over frame: CGRect) -> CGRect? {
        for b in badges {
            let i = b.intersection(frame)
            // The badge sits at the window's top-left corner.
            guard !i.isNull, i.width > 0, i.height > 0, b.minX - frame.minX < 40, b.minY - frame.minY < 40 else { continue }
            return i.offsetBy(dx: -frame.minX, dy: -frame.minY)
        }
        return nil
    }

    /// Current frame of one window straight from the window server (bypasses the poll interval).
    func liveFrame(_ wid: UInt32) -> CGRect? {
        guard let arr = CGWindowListCopyWindowInfo(.optionIncludingWindow, wid) as? [[String: Any]],
              let d = arr.first?[kCGWindowBounds as String] as? NSDictionary else { return nil }
        return CGRect(dictionaryRepresentation: d)
    }

    /// The window list as last fetched (front to back), and when that fetch began.
    private var lastList: [[String: Any]] = []
    private var lastListAt = DispatchTime(uptimeNanoseconds: 0)

    /// How long ago the last look at the window list began.
    var lastLookAgeMs: Double { Double(DispatchTime.now().uptimeNanoseconds - lastListAt.uptimeNanoseconds) / 1e6 }

    /// When an app last came to the front, started, quit, hid or unhid: what's in front may
    /// have changed then.
    private var lastAppChange = DispatchTime(uptimeNanoseconds: 0)

    /// Whether `wid` was in front at `point` at the last look at the window list, without
    /// asking the window server again. nil when what's in front may have changed since that
    /// look began: something was raised (`since`) or an app came or went; when the look is
    /// older than `maxAgeMs`; or when the window isn't known.
    func frontmostAtLastLook(_ wid: UInt32, at point: CGPoint, maxAgeMs: Double, since: DispatchTime) -> Bool? {
        guard let pid = windows[wid]?.pid, lastLookAgeMs <= maxAgeMs, lastListAt > since, lastListAt > lastAppChange else { return nil }
        // Not in the list at all: not on screen, so not in front.
        return Self.frontmost(in: lastList, wid: wid, pid: pid, point: point, kindOf: { windows[$0]?.kind }) ?? false
    }

    /// Whether `wid` is what's in front at `point` (global coords), i.e. a click there would
    /// land on it — or only its own app's menu, tooltip or popover is (raising the window then
    /// would dismiss that menu: a raise counts as a click). Asks the window server, so not for
    /// the session queue: what it needs from the tracker is passed in. Only the windows in
    /// front of `wid` are listed: with many windows open the whole list kept the window server
    /// (and whoever waited on it) busy for long.
    static func isFrontmost(wid: UInt32, pid: pid_t, point: CGPoint, kindOf: (UInt32) -> TrackedWindow.Kind?) -> Bool {
        guard let list = CGWindowListCopyWindowInfo([.optionOnScreenAboveWindow, .excludeDesktopElements], wid)
                as? [[String: Any]] else { return false }
        // Nothing in front of it there.
        return frontmost(in: list, wid: wid, pid: pid, point: point, kindOf: kindOf) ?? true
    }

    /// For the debug port: what the window server lists in front of `wid` at `point`.
    static func describeAbove(wid: UInt32, point: CGPoint) -> String {
        let list = CGWindowListCopyWindowInfo([.optionOnScreenAboveWindow, .excludeDesktopElements], wid) as? [[String: Any]] ?? []
        let at = list.filter { info in
            (info[kCGWindowBounds as String] as? NSDictionary).flatMap { CGRect(dictionaryRepresentation: $0) }?.contains(point) ?? false
        }
        return "\(list.count) windows above, \(at.count) at the point: " + at.prefix(8).map {
            "\($0[kCGWindowNumber as String] ?? 0) \($0[kCGWindowOwnerName as String] ?? "?") layer \($0[kCGWindowLayer as String] ?? "?") alpha \($0[kCGWindowAlpha as String] ?? "?")"
        }.joined(separator: "; ")
    }

    /// Going through `list` front to back: true at `wid` itself or its own app's popup, false
    /// at another window covering `point`, nil when the list has neither.
    private static func frontmost(in list: [[String: Any]], wid: UInt32, pid: pid_t, point: CGPoint,
                                  kindOf: (UInt32) -> TrackedWindow.Kind?) -> Bool? {
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
            return owner == pid && (layer > 0 || number.flatMap { kindOf($0) } != .toplevel)
        }
        return nil
    }
}
