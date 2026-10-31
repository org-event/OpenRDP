// Background for the drag-and-drop disk image.
// The empty band above the icons tells the user how to leave quarantine.
//
//   swift scripts/dmg-background.swift <output.png>
//
// The picture is 760×560 points. scripts/make-dmg.sh uses the same window
// and places the icons on the arrow.

import AppKit

guard CommandLine.arguments.count == 2 else {
    fputs("usage: dmg-background.swift <output.png>\n", stderr)
    exit(2)
}

let width: CGFloat = 760
let height: CGFloat = 560
let arrowY: CGFloat = 360
let arrowStart: CGFloat = 292
let arrowEnd: CGFloat = 468

let image = NSImage(size: NSSize(width: width, height: height), flipped: true) { _ in
    NSColor(srgbRed: 0.961, green: 0.961, blue: 0.969, alpha: 1).setFill()
    NSRect(x: 0, y: 0, width: width, height: height).fill()

    func draw(_ text: String, font: NSFont, color: NSColor, y: CGFloat) {
        let line = NSAttributedString(string: text, attributes: [
            .font: font,
            .foregroundColor: color,
        ])
        let size = line.size()
        line.draw(at: NSPoint(x: (width - size.width) / 2, y: y))
    }

    let ink = NSColor(srgbRed: 0.11, green: 0.11, blue: 0.12, alpha: 1)
    let secondary = NSColor(srgbRed: 0.32, green: 0.32, blue: 0.34, alpha: 1)
    draw(
        "Please unlock OpenRDP",
        font: .systemFont(ofSize: 26, weight: .semibold),
        color: ink,
        y: 36)
    draw(
        "Drag it to Applications, then open Terminal and paste:",
        font: .systemFont(ofSize: 16, weight: .regular),
        color: secondary,
        y: 78)

    let command = "xattr -dr com.apple.quarantine /Applications/OpenRDP.app"
    let commandLine = NSAttributedString(string: command, attributes: [
        .font: NSFont.monospacedSystemFont(ofSize: 13, weight: .medium),
        .foregroundColor: ink,
    ])
    let commandSize = commandLine.size()
    let chip = NSRect(
        x: ((width - commandSize.width) / 2) - 16,
        y: 118,
        width: commandSize.width + 32,
        height: commandSize.height + 16)
    NSColor.white.setFill()
    NSBezierPath(roundedRect: chip, xRadius: 8, yRadius: 8).fill()
    NSColor(srgbRed: 0.82, green: 0.82, blue: 0.84, alpha: 1).setStroke()
    let border = NSBezierPath(roundedRect: chip, xRadius: 8, yRadius: 8)
    border.lineWidth = 1
    border.stroke()
    commandLine.draw(at: NSPoint(x: chip.minX + 16, y: chip.minY + 8))

    draw(
        "Then open OpenRDP.",
        font: .systemFont(ofSize: 16, weight: .regular),
        color: secondary,
        y: 176)

    let shaft = NSRect(x: arrowStart, y: arrowY - 8, width: arrowEnd - arrowStart - 36, height: 16)
    let head = NSBezierPath()
    head.move(to: NSPoint(x: arrowEnd - 42, y: arrowY - 22))
    head.line(to: NSPoint(x: arrowEnd, y: arrowY))
    head.line(to: NSPoint(x: arrowEnd - 42, y: arrowY + 22))
    head.close()
    NSColor(srgbRed: 0.557, green: 0.557, blue: 0.576, alpha: 1).setFill()
    NSBezierPath(roundedRect: shaft, xRadius: 2, yRadius: 2).fill()
    head.fill()
    return true
}

guard let tiff = image.tiffRepresentation,
      let rep = NSBitmapImageRep(data: tiff),
      let png = rep.representation(using: .png, properties: [:])
else {
    fputs("could not draw the disk-image background\n", stderr)
    exit(1)
}
try png.write(to: URL(fileURLWithPath: CommandLine.arguments[1]))
