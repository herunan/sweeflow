import AppKit
import Carbon.HIToolbox

/// Drag and drop across the two machines.
///
/// Linux → Mac: files dropped on a streamed window are handed to `drop(files:at:)`, which starts
/// a real macOS drag from a tiny invisible window at the drop point and releases it there, so the
/// app underneath (Finder, a chat app, …) receives an ordinary drop.
///
/// Mac → Linux: while a mouse button is held in a streamed window, `pollDragStart()` watches the
/// system drag pasteboard; when a file drag begins it reports the files, and `cancelDrag()` ends
/// the Mac side once Linux has taken the drag over.
final class DragBridge: NSObject, NSDraggingSource {
    private var panel: NSPanel?
    /// Something dropped from Linux.
    enum Item: CustomStringConvertible {
        case file(URL), link(URL), text(String)
        var description: String {
            switch self {
            case .file(let u): return u.lastPathComponent
            case .link(let u): return u.absoluteString
            case .text(let t): return "text (\(t.count) chars)"
            }
        }
    }

    private var pending: [Item] = []

    /// True while a drop is being replayed (pointer input from Linux is held off meanwhile).
    static var replaying: Bool { replayLock.lock(); defer { replayLock.unlock() }; return DispatchTime.now() < replayUntil }
    private static let replayLock = NSLock()
    private static var replayUntil = DispatchTime.now()
    private static func replay(for seconds: Double) {
        replayLock.lock(); replayUntil = .now() + seconds; replayLock.unlock()
    }
    private var dragChangeCount = NSPasteboard(name: .drag).changeCount
    /// Files of the Mac drag in progress (for Linux to fetch on drop).
    private(set) var draggedFiles: [URL] = []

    // MARK: Linux → Mac

    /// Drops `items` at `point` (global CG coordinates, top-left origin). Main thread only.
    /// Pace of a replayed drop (seconds): quick, but slow enough that apps see the drag enter
    /// before the release. Tunable from the debug port while finding the edge.
    static var timing = (press: 0.02, afterPress: 0.06, step: 0.012, rests: 2, rest: 0.03)

    func drop(_ items: [Item], at point: CGPoint) {
        guard !items.isEmpty else { return }
        pending = items
        // NSScreen coordinates are bottom-left based.
        let primaryHeight = NSScreen.screens.first?.frame.maxY ?? 0
        let size: CGFloat = 6
        let rect = NSRect(x: point.x - size / 2, y: primaryHeight - point.y - size / 2, width: size, height: size)
        let p = NSPanel(contentRect: rect, styleMask: [.borderless, .nonactivatingPanel], backing: .buffered, defer: false)
        p.level = .popUpMenu
        p.isOpaque = false
        p.backgroundColor = NSColor.white.withAlphaComponent(0.01)   // must be hit-testable
        p.ignoresMouseEvents = false
        p.contentView = DragStartView(bridge: self)
        p.orderFrontRegardless()
        panel = p

        // Press on our panel (starts the drag), then move around the drop point like a hand
        // would, one event per frame, so the app underneath sees the drag enter and settle
        // before the release. Too fast and some apps miss the drop.
        let src = CGEventSource(stateID: .hidSystemState)
        func post(_ type: CGEventType, _ at: CGPoint) {
            CGEvent(mouseEventSource: src, mouseType: type, mouseCursorPosition: at, mouseButton: .left)?.post(tap: .cghidEventTap)
        }
        let path: [CGVector] = [.init(dx: 2, dy: 0), .init(dx: 6, dy: 1), .init(dx: 10, dy: 2), .init(dx: 12, dy: 3),
                                .init(dx: 8, dy: 2), .init(dx: 4, dy: 1), .init(dx: 1, dy: 0), .init(dx: 0, dy: 0)]
        post(.mouseMoved, point)
        var t = Self.timing.press
        DispatchQueue.main.asyncAfter(deadline: .now() + t) { post(.leftMouseDown, point) }
        t += Self.timing.afterPress
        for v in path {
            DispatchQueue.main.asyncAfter(deadline: .now() + t) { post(.leftMouseDragged, CGPoint(x: point.x + v.dx, y: point.y + v.dy)) }
            t += Self.timing.step
        }
        // Rest on the target a moment (drop highlights), then release.
        for _ in 0..<Self.timing.rests {
            DispatchQueue.main.asyncAfter(deadline: .now() + t) { post(.leftMouseDragged, point) }
            t += Self.timing.rest
        }
        Self.replay(for: t + 0.1)
        DispatchQueue.main.asyncAfter(deadline: .now() + t) { post(.leftMouseUp, point) }
        DispatchQueue.main.asyncAfter(deadline: .now() + t + 0.5) { self.panel?.orderOut(nil); self.panel = nil }
    }

    fileprivate func beginDrag(from view: NSView, event: NSEvent) {
        let items = pending.map { it -> NSDraggingItem in
            let writer: NSPasteboardWriting, icon: NSImage
            switch it {
            case .file(let url):
                writer = url as NSURL
                icon = NSWorkspace.shared.icon(forFile: url.path)
            case .link(let url):
                writer = url as NSURL
                icon = NSImage(systemSymbolName: "link", accessibilityDescription: nil) ?? NSImage()
            case .text(let text):
                writer = text as NSString
                icon = NSImage(systemSymbolName: "doc.plaintext", accessibilityDescription: nil) ?? NSImage()
            }
            let item = NSDraggingItem(pasteboardWriter: writer)
            item.setDraggingFrame(NSRect(x: 0, y: 0, width: 32, height: 32), contents: icon)
            return item
        }
        view.beginDraggingSession(with: items, event: event, source: self)
        // The drop target is whatever is under the pointer: get out of its way.
        panel?.ignoresMouseEvents = true
        panel?.alphaValue = 0
    }

    func draggingSession(_ session: NSDraggingSession, endedAt screenPoint: NSPoint, operation: NSDragOperation) {
        log("drop replay: ended at \(screenPoint) with operation \(operation.rawValue)")
    }

    func draggingSession(_ session: NSDraggingSession, sourceOperationMaskFor context: NSDraggingContext) -> NSDragOperation {
        .copy
    }

    // MARK: Mac → Linux

    /// Call when a button goes down in a streamed window (resets drag detection).
    func armDragDetection() {
        dragChangeCount = NSPasteboard(name: .drag).changeCount
        draggedFiles = []
    }

    /// Returns the files of a drag that started since `armDragDetection`, once.
    func pollDragStart() -> [URL]? {
        let pb = NSPasteboard(name: .drag)
        guard pb.changeCount != dragChangeCount else { return nil }
        dragChangeCount = pb.changeCount
        let urls = pb.readObjects(forClasses: [NSURL.self], options: [.urlReadingFileURLsOnly: true]) as? [URL] ?? []
        guard !urls.isEmpty else { return nil }
        draggedFiles = urls
        return urls
    }

    /// Linux took the drag over: cancel it on the Mac (Escape, then release the button).
    func cancelDrag(at point: CGPoint) {
        let src = CGEventSource(stateID: .hidSystemState)
        CGEvent(keyboardEventSource: src, virtualKey: CGKeyCode(kVK_Escape), keyDown: true)?.post(tap: .cghidEventTap)
        CGEvent(keyboardEventSource: src, virtualKey: CGKeyCode(kVK_Escape), keyDown: false)?.post(tap: .cghidEventTap)
        CGEvent(mouseEventSource: src, mouseType: .leftMouseUp, mouseCursorPosition: point, mouseButton: .left)?.post(tap: .cghidEventTap)
    }
}

/// The invisible view that receives the synthetic press and starts the drag.
private final class DragStartView: NSView {
    weak var bridge: DragBridge?
    init(bridge: DragBridge) {
        self.bridge = bridge
        super.init(frame: .zero)
    }
    required init?(coder: NSCoder) { nil }
    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }
    override func mouseDown(with event: NSEvent) {
        log("drop replay: press received")
        bridge?.beginDrag(from: self, event: event)
    }
}
