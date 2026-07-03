// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "JaumKit",
    platforms: [.macOS(.v15), .iOS(.v18)],
    products: [
        .library(name: "JaumKit", targets: ["JaumKit"])
    ],
    targets: [
        .target(name: "JaumKit"),
        .testTarget(name: "JaumKitTests", dependencies: ["JaumKit"]),
    ]
)
