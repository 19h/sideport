// swift-tools-version: 6.2

import PackageDescription

let package = Package(
    name: "SideportAnisetteBridge",
    platforms: [.macOS(.v26)],
    products: [
        .library(name: "SideportAnisetteBridge", type: .dynamic, targets: ["SideportAnisetteBridge"])
    ],
    dependencies: [
        .package(
            url: "https://github.com/altstoreio/AnisetteKit.git",
            revision: "1f5a7e36553cc865b873f222b87a6486c0bcc7bf"
        )
    ],
    targets: [
        .target(
            name: "SideportAnisetteBridge",
            dependencies: [.product(name: "AnisetteKit", package: "AnisetteKit")]
        )
    ]
)
