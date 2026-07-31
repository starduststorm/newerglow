import AppKit
import CoreImage

// Usage: make_icon <source.png> <out_dir> <hex_bg> <inset_fraction>
//   hex_bg: "000000", "1a1a1a", or "clear" for transparent
//   inset_fraction: 0.0..0.5 (e.g. 0.10 means 10% padding on each side)
//
// When hex_bg is "clear", the source is treated as a luminance mask
// (black → transparent, white → opaque white) and a drop shadow is rendered
// behind the foreground silhouette.

let args = CommandLine.arguments
guard args.count == 5 else {
    FileHandle.standardError.write("usage: make_icon <src> <out_dir> <hex_bg|clear> <inset>\n".data(using: .utf8)!)
    exit(1)
}
let srcPath = args[1]
let outDir = args[2]
let bgArg = args[3]
let inset = Double(args[4]) ?? 0.10

guard let loadedImage = NSImage(contentsOfFile: srcPath) else {
    FileHandle.standardError.write("cannot load source image\n".data(using: .utf8)!)
    exit(1)
}

func parseColor(_ s: String) -> NSColor? {
    if s == "clear" { return nil }
    var hex = s
    if hex.hasPrefix("#") { hex.removeFirst() }
    guard hex.count == 6, let v = UInt32(hex, radix: 16) else { return nil }
    let r = CGFloat((v >> 16) & 0xff) / 255.0
    let g = CGFloat((v >> 8) & 0xff) / 255.0
    let b = CGFloat(v & 0xff) / 255.0
    return NSColor(srgbRed: r, green: g, blue: b, alpha: 1.0)
}

let bg = parseColor(bgArg)

// For transparent output, force-decode the source through CIMaskToAlpha so
// black pixels become transparent and luminance survives as alpha (preserves
// anti-aliased edges). Doing this once up front also detaches us from the
// source file on disk, so it's safe for the loop to later overwrite the input
// when it happens to share a path with one of the output sizes.
func keyOutBlackToAlpha(_ image: NSImage) -> NSImage {
    var rect = NSRect(origin: .zero, size: image.size)
    guard let cg = image.cgImage(forProposedRect: &rect, context: nil, hints: nil),
          let filter = CIFilter(name: "CIMaskToAlpha")
    else { return image }
    filter.setValue(CIImage(cgImage: cg), forKey: kCIInputImageKey)
    guard let output = filter.outputImage,
          let cgOut = CIContext(options: nil).createCGImage(output, from: output.extent)
    else { return image }
    return NSImage(cgImage: cgOut, size: image.size)
}

let srcImage: NSImage = (bg == nil) ? keyOutBlackToAlpha(loadedImage) : loadedImage

let sizes: [(name: String, px: Int)] = [
    ("icon_16x16",      16),
    ("icon_16x16@2x",   32),
    ("icon_32x32",      32),
    ("icon_32x32@2x",   64),
    ("icon_128x128",    128),
    ("icon_128x128@2x", 256),
    ("icon_256x256",    256),
    ("icon_256x256@2x", 512),
    ("icon_512x512",    512),
    ("icon_512x512@2x", 1024),
]

try? FileManager.default.createDirectory(atPath: outDir, withIntermediateDirectories: true)

for entry in sizes {
    let px = entry.px
    let bmp = NSBitmapImageRep(
        bitmapDataPlanes: nil,
        pixelsWide: px,
        pixelsHigh: px,
        bitsPerSample: 8,
        samplesPerPixel: 4,
        hasAlpha: true,
        isPlanar: false,
        colorSpaceName: .deviceRGB,
        bytesPerRow: 0,
        bitsPerPixel: 32
    )!
    bmp.size = NSSize(width: px, height: px)

    NSGraphicsContext.saveGraphicsState()
    let ctx = NSGraphicsContext(bitmapImageRep: bmp)!
    NSGraphicsContext.current = ctx
    ctx.imageInterpolation = .high

    let rect = NSRect(x: 0, y: 0, width: px, height: px)
    if let bg = bg {
        bg.setFill()
        rect.fill()
    } else {
        NSColor.clear.setFill()
        rect.fill(using: .copy)
    }

    let pad = CGFloat(Double(px) * inset)
    let logoRect = NSRect(x: pad, y: pad, width: CGFloat(px) - 2 * pad, height: CGFloat(px) - 2 * pad)

    if bg == nil {
        let shadow = NSShadow()
        shadow.shadowColor = NSColor(white: 0, alpha: 0.45)
        shadow.shadowOffset = NSSize(width: 0, height: -CGFloat(Double(px) * 0.015))
        shadow.shadowBlurRadius = CGFloat(Double(px) * 0.05)
        NSGraphicsContext.saveGraphicsState()
        shadow.set()
        srcImage.draw(in: logoRect, from: .zero, operation: .sourceOver, fraction: 1.0)
        NSGraphicsContext.restoreGraphicsState()
    } else {
        srcImage.draw(in: logoRect, from: .zero, operation: .sourceOver, fraction: 1.0)
    }

    NSGraphicsContext.restoreGraphicsState()

    guard let data = bmp.representation(using: .png, properties: [:]) else {
        FileHandle.standardError.write("png encode failed for \(entry.name)\n".data(using: .utf8)!)
        exit(1)
    }
    let outPath = "\(outDir)/\(entry.name).png"
    try! data.write(to: URL(fileURLWithPath: outPath))
    print("wrote \(outPath) (\(px)x\(px))")
}
