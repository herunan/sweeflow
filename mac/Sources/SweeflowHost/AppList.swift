import AppKit

/// Installed Mac apps (for launcher entries on Linux).
enum AppList {
    struct App {
        let bundleId: String
        let name: String
        let iconPNG: Data
    }

    static func scan() -> [App] {
        let fm = FileManager.default
        let home = fm.homeDirectoryForCurrentUser.path
        let dirs = ["/Applications", "/Applications/Utilities", "/System/Applications",
                    "/System/Applications/Utilities", home + "/Applications"]
        var seen = Set<String>()
        var out: [App] = []
        // Apps that live outside the Applications folders.
        let extras = ["/System/Library/CoreServices/Finder.app"]
        let paths = dirs.flatMap { dir in
            ((try? fm.contentsOfDirectory(atPath: dir)) ?? []).filter { $0.hasSuffix(".app") }.map { dir + "/" + $0 }
        } + extras
        for path in paths {
            let item = (path as NSString).lastPathComponent
            guard let bundle = Bundle(path: path), let bid = bundle.bundleIdentifier,
                  bid != Bundle.main.bundleIdentifier, seen.insert(bid).inserted else { continue }
            // Skip background-only and menu-bar agents: nothing to show on Linux.
            if bundle.object(forInfoDictionaryKey: "LSBackgroundOnly") as? Bool == true { continue }
            if bundle.object(forInfoDictionaryKey: "LSUIElement") as? Bool == true { continue }
            let name = (bundle.object(forInfoDictionaryKey: "CFBundleDisplayName") as? String)
                ?? (bundle.object(forInfoDictionaryKey: "CFBundleName") as? String)
                ?? String(item.dropLast(4))
            out.append(App(bundleId: bid, name: name, iconPNG: png(NSWorkspace.shared.icon(forFile: path), size: 128)))
        }
        return out.sorted { $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending }
    }

    static func png(_ image: NSImage, size: Int) -> Data {
        guard let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: size, pixelsHigh: size, bitsPerSample: 8,
                                         samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
                                         bytesPerRow: 0, bitsPerPixel: 0) else { return Data() }
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
        image.draw(in: NSRect(x: 0, y: 0, width: size, height: size))
        NSGraphicsContext.restoreGraphicsState()
        return rep.representation(using: .png, properties: [:]) ?? Data()
    }
}
