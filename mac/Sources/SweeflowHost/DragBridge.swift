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
    private var pendingFiles: [URL] = []
    private var dragChangeCount = NSPasteboard(name: .drag).changeCount
    /// Files of the Mac drag in progress (for Linux to fetch on drop).
    private(set) var draggedFiles: [URL] = []

    // MARK: Linux → Mac

    /// Drops `files` at `point` (global CG coordinates, top-left origin). Main thread only.
    func drop(files: [URL], at point: CGPoint) {
        guard !files.isEmpty else { return }
        pendingFiles = files
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

        // Press on our panel (starts the drag), wiggle so AppKit registers dragging, release
        // at the drop point once the panel is out of the way.
        let src = CGEventSource(stateID: .hidSystemState)
        func post(_ type: CGEventType, _ at: CGPoint) {
            CGEvent(mouseEventSource: src, mouseType: type, mouseCursorPosition: at, mouseButton: .left)?.post(tap: .cghidEventTap)
        }
        post(.mouseMoved, point)
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) {
            post(.leftMouseDown, point)
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.08) {
                for dx in [3.0, 6.0, 9.0, 6.0, 3.0] { post(.leftMouseDragged, CGPoint(x: point.x + dx, y: point.y)) }
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.08) {
                    post(.leftMouseDragged, point)
                    DispatchQueue.main.asyncAfter(deadline: .now() + 0.12) {
                        post(.leftMouseUp, point)
                        DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) { self.panel?.orderOut(nil); self.panel = nil }
                    }
                }
            }
        }
    }

    fileprivate func beginDrag(from view: NSView, event: NSEvent) {
        let items = pendingFiles.map { url -> NSDraggingItem in
            let item = NSDraggingItem(pasteboardWriter: url as NSURL)
            item.setDraggingFrame(NSRect(x: 0, y: 0, width: 32, height: 32), contents: NSWorkspace.shared.icon(forFile: url.path))
            return item
        }
        view.beginDraggingSession(with: items, event: event, source: self)
        // The drop target is whatever is under the pointer: get out of its way.
        panel?.ignoresMouseEvents = true
        panel?.alphaValue = 0
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
    override func mouseDown(with event: NSEvent) { bridge?.beginDrag(from: self, event: event) }
}
