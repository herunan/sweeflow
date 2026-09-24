// swift-tools-version:6.0
import PackageDescription

let package = Package(
    name: "SweeflowHost",
    platforms: [.macOS(.v15)],
    targets: [
        // Declarations for private CoreGraphics / HIServices SPI (virtual displays, AX window ids).
        .target(name: "CPrivate"),
        .executableTarget(
            name: "SweeflowHost",
            dependencies: ["CPrivate"],
            swiftSettings: [.swiftLanguageMode(.v5)],
            linkerSettings: [
                .linkedFramework("ScreenCaptureKit"),
                .linkedFramework("VideoToolbox"),
                .linkedFramework("CoreMedia"),
                .linkedFramework("Network"),
                .linkedFramework("AppKit"),
                .linkedFramework("ApplicationServices"),
                .linkedFramework("Accelerate"),
            ]
        ),
    ]
)
