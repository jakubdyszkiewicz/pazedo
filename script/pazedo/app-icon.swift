// Makes Pazedo's app icon from the concept image in docs/pazedo/design/.
//
//   swift script/pazedo/app-icon.swift
//
// The concept is a generated preview: its tile sits on a pale backdrop with a presentation
// shadow and no transparency. This cuts the tile out and places it on macOS's icon grid (an
// 824px tile in a 1024px canvas) with a transparent background and the usual drop shadow, and
// writes crates/zed/resources/app-icon-dev@2x.png and app-icon-dev.png. Pazedo builds the dev
// channel, so those are the files its bundle, About window and Linux window icon use.
import AppKit

let repository = URL(fileURLWithPath: #filePath)
    .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
let conceptURL = repository.appendingPathComponent("docs/pazedo/design/app-icon-white-on-charcoal.png")
let resources = repository.appendingPathComponent("crates/zed/resources")

func fail(_ message: String) -> Never {
    FileHandle.standardError.write((message + "\n").data(using: .utf8)!)
    exit(1)
}

guard let concept = NSImage(contentsOf: conceptURL)?
    .cgImage(forProposedRect: nil, context: nil, hints: nil)
else { fail("cannot read \(conceptURL.path)") }

let conceptWidth = concept.width
let conceptHeight = concept.height
var pixels = [UInt8](repeating: 0, count: conceptWidth * conceptHeight * 4)
guard let reader = CGContext(
    data: &pixels, width: conceptWidth, height: conceptHeight, bitsPerComponent: 8,
    bytesPerRow: conceptWidth * 4, space: CGColorSpaceCreateDeviceRGB(),
    bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)
else { fail("cannot read pixels") }
reader.draw(concept, in: CGRect(x: 0, y: 0, width: conceptWidth, height: conceptHeight))

// Rows are top-down in `pixels`. The tile is charcoal and the backdrop and its shadow are light,
// so the tile's edges are where a scan from outside first meets a dark pixel. Scan away from
// the white P: the middle row passes left of its stem, and the top and bottom scans run left
// of center, through the stem only between the tile's edges.
func isTile(_ x: Int, _ y: Int) -> Bool {
    let index = (y * conceptWidth + x) * 4
    let luminance = (Int(pixels[index]) * 3 + Int(pixels[index + 1]) * 6 + Int(pixels[index + 2])) / 10
    return luminance < 80
}
let middleRow = conceptHeight / 2
let scanColumn = conceptWidth / 2 - conceptWidth / 6
guard
    let left = (0..<conceptWidth).first(where: { isTile($0, middleRow) }),
    let right = (0..<conceptWidth).reversed().first(where: { isTile($0, middleRow) }),
    let top = (0..<conceptHeight).first(where: { isTile(scanColumn, $0) }),
    let bottom = (0..<conceptHeight).reversed().first(where: { isTile(scanColumn, $0) })
else { fail("no tile found in the concept") }

// A few pixels inside the edge, so the backdrop's antialiased fringe does not show as a light rim.
let inset = 3
let tileBounds = CGRect(
    x: left + inset, y: top + inset,
    width: right - left + 1 - 2 * inset, height: bottom - top + 1 - 2 * inset)
guard let tile = concept.cropping(to: tileBounds) else { fail("cannot crop the tile") }
// Measured on the concept: its corners are round with a radius of about 26% of the tile.
let cornerFraction = 0.26

func renderIcon(size: Int) -> Data {
    let scale = CGFloat(size) / 1024
    let tileRect = CGRect(x: 100 * scale, y: 100 * scale, width: 824 * scale, height: 824 * scale)
    let corner = tileRect.width * cornerFraction
    let tilePath = CGPath(roundedRect: tileRect, cornerWidth: corner, cornerHeight: corner, transform: nil)

    guard let context = CGContext(
        data: nil, width: size, height: size, bitsPerComponent: 8, bytesPerRow: 0,
        space: CGColorSpace(name: CGColorSpace.sRGB)!,
        bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)
    else { fail("cannot create a \(size)px canvas") }
    context.interpolationQuality = .high

    context.saveGState()
    context.setShadow(
        offset: CGSize(width: 0, height: -12 * scale), blur: 28 * scale,
        color: CGColor(gray: 0, alpha: 0.35))
    context.addPath(tilePath)
    context.setFillColor(CGColor(gray: 0.13, alpha: 1))
    context.fillPath()
    context.restoreGState()

    context.addPath(tilePath)
    context.clip()
    context.draw(tile, in: tileRect)

    guard let image = context.makeImage(),
        let png = NSBitmapImageRep(cgImage: image).representation(using: .png, properties: [:])
    else { fail("cannot encode the \(size)px icon") }
    return png
}

for (name, size) in [("app-icon-dev@2x.png", 1024), ("app-icon-dev.png", 512)] {
    let url = resources.appendingPathComponent(name)
    do {
        try renderIcon(size: size).write(to: url)
    } catch {
        fail("cannot write \(url.path): \(error)")
    }
    print("wrote \(url.path)")
}
