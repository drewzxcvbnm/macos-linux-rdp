// swift-tools-version:5.10
import PackageDescription

let package = Package(
    name: "macrd-server",
    platforms: [.macOS(.v14)],
    products: [
        .executable(name: "macrd-server", targets: ["macrd-server"]),
    ],
    targets: [
        .target(
            name: "CVirtualDisplay",
            linkerSettings: [.linkedFramework("CoreGraphics")]
        ),
        .target(
            name: "MacRDCore",
            dependencies: ["CVirtualDisplay"],
            linkerSettings: [
                .linkedFramework("ScreenCaptureKit"),
                .linkedFramework("VideoToolbox"),
                .linkedFramework("CoreMedia"),
                .linkedFramework("AppKit"),
                .linkedFramework("Network"),
                .linkedFramework("IOKit"),
                .linkedFramework("Carbon"),
            ]
        ),
        .executableTarget(name: "macrd-server", dependencies: ["MacRDCore"]),
        // XCTest/swift-testing are unavailable with Command Line Tools alone, so tests are a plain executable.
        .executableTarget(name: "macrd-selftest", dependencies: ["MacRDCore"]),
    ]
)
