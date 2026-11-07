// swift-tools-version: 6.2
import PackageDescription

let package = Package(
    name: "CodeConnectKit",
    platforms: [.iOS(.v17), .macOS(.v26)],
    products: [
        .library(name: "CodeConnectKit", targets: ["CodeConnectKit"])
    ],
    targets: [
        .target(
            name: "CodeConnectKit",
            swiftSettings: [.enableUpcomingFeature("StrictConcurrency")]
        )
    ],
    swiftLanguageModes: [.v5]
)
