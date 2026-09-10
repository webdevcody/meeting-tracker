// swift-tools-version: 6.2
import PackageDescription

let package = Package(
    name: "meet",
    platforms: [.macOS(.v26)],
    products: [
        // The recording engine. The user-facing `meet` binary is the Rust TUI in crates/meet,
        // which spawns this one with `record --json`.
        .executable(name: "meet-rec", targets: ["meet"]),
    ],
    dependencies: [
        .package(url: "https://github.com/apple/swift-argument-parser", from: "1.5.0"),
    ],
    targets: [
        .executableTarget(
            name: "meet",
            dependencies: [
                .product(name: "ArgumentParser", package: "swift-argument-parser"),
            ],
            path: "Sources/meet",
            swiftSettings: [.swiftLanguageMode(.v5)]
        ),
    ]
)
