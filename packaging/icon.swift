// App icon generator: a terminal in muxterm's default palette (iterm-dark),
// split into three panes with a prompt chevron in each - the split tree is
// the product. Renders a 1024x1024 PNG; the Makefile's `icon` target scales
// it into the .iconset sizes and packs assets/muxterm.icns. Run via:
//   xcrun swift packaging/icon.swift <out.png>
import AppKit

let out = CommandLine.arguments.count > 1
    ? CommandLine.arguments[1] : "icon-1024.png"

func srgb(_ hex: UInt32, _ alpha: CGFloat = 1) -> NSColor {
    NSColor(
        srgbRed: CGFloat((hex >> 16) & 0xff) / 255,
        green: CGFloat((hex >> 8) & 0xff) / 255,
        blue: CGFloat(hex & 0xff) / 255,
        alpha: alpha
    )
}

// Draw into an explicit 1024px bitmap: NSImage.lockFocus() would render at
// the screen's backing scale and emit 2048px on retina.
let rep = NSBitmapImageRep(
    bitmapDataPlanes: nil, pixelsWide: 1024, pixelsHigh: 1024,
    bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
    colorSpaceName: .calibratedRGB, bytesPerRow: 0, bitsPerPixel: 0
)!
rep.size = NSSize(width: 1024, height: 1024)
NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)

// Apple's icon grid: content in a rounded rect inset ~10%, radius ~22.5%.
let tile = NSRect(x: 100, y: 100, width: 824, height: 824)
let shape = NSBezierPath(roundedRect: tile, xRadius: 185, yRadius: 185)
NSGradient(starting: srgb(0x26272e), ending: srgb(0x15161a))!
    .draw(in: shape, angle: -90)

// Pane dividers (left pane, right column split in two), clipped to the tile.
NSGraphicsContext.saveGraphicsState()
shape.addClip()
let divX: CGFloat = 100 + 824 * 0.56
srgb(0xffffff, 0.08).set()
NSRect(x: divX - 6, y: 100, width: 12, height: 824).fill()
NSRect(x: divX + 6, y: 506, width: 924 - divX - 6, height: 12).fill()
NSGraphicsContext.restoreGraphicsState()

func chevron(x: CGFloat, y: CGFloat, pt: CGFloat, color: NSColor) {
    let font = NSFont(name: "Menlo-Bold", size: pt)
        ?? NSFont.boldSystemFont(ofSize: pt)
    NSAttributedString(
        string: "\u{276F}",
        attributes: [.font: font, .foregroundColor: color]
    ).draw(at: NSPoint(x: x, y: y))
}

func bar(x: CGFloat, y: CGFloat, w: CGFloat, alpha: CGFloat) {
    srgb(0xc7c7c7, alpha).set()
    NSBezierPath(
        roundedRect: NSRect(x: x, y: y, width: w, height: 36),
        xRadius: 18, yRadius: 18
    ).fill()
}

// The user's prompt - accent chevron, block cursor, scrolled output...
let accent = srgb(0x4a90d9)
chevron(x: 176, y: 656, pt: 200, color: accent)
accent.withAlphaComponent(0.5).set()
NSRect(x: 348, y: 668, width: 104, height: 200).fill()
bar(x: 176, y: 540, w: 320, alpha: 0.12)
bar(x: 176, y: 452, w: 230, alpha: 0.12)
bar(x: 176, y: 364, w: 284, alpha: 0.12)
// ...and two teammate panes, dimmer, mid-work.
let dim = srgb(0xc7c7c7, 0.55)
chevron(x: divX + 56, y: 756, pt: 96, color: dim)
bar(x: divX + 56, y: 668, w: 204, alpha: 0.09)
chevron(x: divX + 56, y: 340, pt: 96, color: dim)
bar(x: divX + 56, y: 252, w: 172, alpha: 0.09)

NSGraphicsContext.restoreGraphicsState()
try! rep.representation(using: .png, properties: [:])!
    .write(to: URL(fileURLWithPath: out))
print("wrote \(out)")
