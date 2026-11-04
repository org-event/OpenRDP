// Turns the artwork in assets/icon (server-art.png, client-art.png: square,
// full-bleed) into macOS app icons: the art goes on the 824×824 rounded tile
// of the 1024 grid, with the system's drop shadow. Writes, for each of server
// and client, <name>.icns (every iconset size) and <name>-256.png for the
// README.
//
//   swift scripts/make-icon.swift

import AppKit

let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent()
let dir = root.appendingPathComponent("assets/icon")

/// Share of each edge of the art cut away, so the mark fills more of the tile.
let crop = 0.05

func render(_ art: NSImage, _ size: Int) -> NSBitmapImageRep {
    let rep = NSBitmapImageRep(
        bitmapDataPlanes: nil, pixelsWide: size, pixelsHigh: size, bitsPerSample: 8,
        samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
        bytesPerRow: 0, bitsPerPixel: 0)!
    rep.size = NSSize(width: size, height: size)
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    NSGraphicsContext.current?.imageInterpolation = .high

    let k = CGFloat(size) / 1024
    let tile = NSRect(x: 100 * k, y: 100 * k, width: 824 * k, height: 824 * k)
    let shape = NSBezierPath(roundedRect: tile, xRadius: 185 * k, yRadius: 185 * k)

    NSGraphicsContext.saveGraphicsState()
    let shadow = NSShadow()
    shadow.shadowOffset = NSSize(width: 0, height: -12 * k)
    shadow.shadowBlurRadius = 28 * k
    shadow.shadowColor = NSColor.black.withAlphaComponent(0.4)
    shadow.set()
    NSColor.black.setFill()
    shape.fill()
    NSGraphicsContext.restoreGraphicsState()

    shape.addClip()
    let w = art.size.width, h = art.size.height
    let from = NSRect(x: w * crop, y: h * crop, width: w * (1 - 2 * crop), height: h * (1 - 2 * crop))
    art.draw(in: tile, from: from, operation: .copy, fraction: 1)

    NSGraphicsContext.restoreGraphicsState()
    return rep
}

func write(_ rep: NSBitmapImageRep, to url: URL) throws {
    try rep.representation(using: .png, properties: [:])!.write(to: url)
}

for name in ["server", "client"] {
    guard let art = NSImage(contentsOf: dir.appendingPathComponent("\(name)-art.png")) else {
        fatalError("cannot read \(name)-art.png")
    }
    let iconset = FileManager.default.temporaryDirectory.appendingPathComponent("\(name).iconset")
    try? FileManager.default.removeItem(at: iconset)
    try FileManager.default.createDirectory(at: iconset, withIntermediateDirectories: true)
    for points in [16, 32, 128, 256, 512] {
        try write(render(art, points), to: iconset.appendingPathComponent("icon_\(points)x\(points).png"))
        try write(render(art, points * 2), to: iconset.appendingPathComponent("icon_\(points)x\(points)@2x.png"))
    }

    let iconutil = Process()
    iconutil.executableURL = URL(fileURLWithPath: "/usr/bin/iconutil")
    iconutil.arguments = ["-c", "icns", iconset.path, "-o", dir.appendingPathComponent("\(name).icns").path]
    try iconutil.run()
    iconutil.waitUntilExit()
    guard iconutil.terminationStatus == 0 else { fatalError("iconutil failed for \(name)") }

    try write(render(art, 256), to: dir.appendingPathComponent("\(name)-256.png"))
    print("wrote \(name).icns and \(name)-256.png")
}
