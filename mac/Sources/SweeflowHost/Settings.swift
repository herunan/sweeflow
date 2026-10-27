import Foundation

extension Notification.Name {
    static let sweeflowSettingsChanged = Notification.Name("sweeflowSettingsChanged")
}

/// The switches in the Mac menu-bar item and the Linux tray menu (the same ones: changing one
/// side changes the other). Saved in config.json with the rest of the configuration.
/// Readable from any thread.
final class Settings {
    private let lock = NSLock()
    private var c: Config

    init(_ c: Config) { self.c = c }

    var config: Config { lock.withLock { c } }

    /// Changes the settings, saves them and tells everyone (sessions, the menu-bar item).
    func update(_ change: (inout Config) -> Void) {
        let now: Config = lock.withLock { change(&c); return c }
        now.save()
        DispatchQueue.main.async { NotificationCenter.default.post(name: .sweeflowSettingsChanged, object: nil) }
    }

    /// The part the client sees and changes, as JSON.
    struct Wire: Codable {
        var enabled: Bool?
        var exclude: [String]?
        var sound: Bool?
        var dim: Bool?
        var clipboard: Bool?
        var dragDrop: Bool?
        var launcher: Bool?
        var quality: String?
        var desktop: Bool?
        var backdrop: String?
    }

    func wire() -> Data {
        let c = config
        let w = Wire(enabled: c.enabled, exclude: c.excludeApps, sound: c.soundOnLinux, dim: c.dimMacScreenWhileStreaming,
                     clipboard: c.clipboard, dragDrop: c.dragDrop, launcher: c.launcherMenu, quality: c.quality,
                     desktop: c.ownDesktop, backdrop: c.backdrop)
        return (try? JSONEncoder().encode(w)) ?? Data()
    }

    /// A change from the client: only the fields it sent.
    func apply(wire data: Data) {
        guard let w = try? JSONDecoder().decode(Wire.self, from: data) else { return log("bad settings from client") }
        update { Self.merge(w, into: &$0) }
    }

    /// Applies the fields the client sent; anything absent or invalid stays as it is.
    static func merge(_ w: Wire, into c: inout Config) {
        if let v = w.enabled { c.enabled = v }
        if let v = w.exclude { c.excludeApps = v }
        if let v = w.sound { c.soundOnLinux = v }
        if let v = w.dim { c.dimMacScreenWhileStreaming = v }
        if let v = w.clipboard { c.clipboard = v }
        if let v = w.dragDrop { c.dragDrop = v }
        if let v = w.launcher { c.launcherMenu = v }
        if let v = w.quality, ["sharp", "light"].contains(v) { c.quality = v }
        if let v = w.desktop { c.ownDesktop = v }
        if let v = w.backdrop, ["dark", "light"].contains(v) { c.backdrop = v }
    }
}
