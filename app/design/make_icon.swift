// Draws the Eihwaz (yew rune) app icon at 1024x1024 in the macOS icon style.
// usage: swift make_icon.swift <out.png> [berry]
import AppKit
import CoreGraphics

let args = CommandLine.arguments
let out = args[1]
let berry = args.count > 2 && args[2] == "berry"
let paletteName = args.count > 3 ? args[3] : "evergreen"
struct Pal { let b0, b1, b2, glow, r0, r1, r2, rshadow: UInt32; let glowA: CGFloat }
let palettes: [String: Pal] = [
  "evergreen": Pal(b0: 0x2F6E55, b1: 0x1B4636, b2: 0x0D251C, glow: 0xBFE8D2, r0: 0xFFFBF2, r1: 0xEDE3CF, r2: 0xD9CBB0, rshadow: 0x051510, glowA: 0.22),
  "graphite":  Pal(b0: 0x3A3D45, b1: 0x23252B, b2: 0x111216, glow: 0xFFFFFF, r0: 0xFFFBF2, r1: 0xEDE3CF, r2: 0xD9CBB0, rshadow: 0x000000, glowA: 0.14),
  "indigo":    Pal(b0: 0x4B5FD6, b1: 0x2C3A9E, b2: 0x161E5C, glow: 0xC9D4FF, r0: 0xFFFFFF, r1: 0xF1F3FA, r2: 0xD9DEEE, rshadow: 0x0A0F33, glowA: 0.25),
  "heartwood": Pal(b0: 0xD9783F, b1: 0xB0502A, b2: 0x6E2A16, glow: 0xFFD7B8, r0: 0xFFFBF2, r1: 0xF5EAD8, r2: 0xE6D3B6, rshadow: 0x3A1206, glowA: 0.25),
  "silver":    Pal(b0: 0xFFFFFF, b1: 0xEEF0F3, b2: 0xD5D9E0, glow: 0xFFFFFF, r0: 0x2A2E36, r1: 0x1D2027, r2: 0x0F1115, rshadow: 0x6B7280, glowA: 0.0),
]
let P = palettes[paletteName]!
let S: CGFloat = 1024

let cs = CGColorSpace(name: CGColorSpace.displayP3)!
let ctx = CGContext(data: nil, width: Int(S), height: Int(S), bitsPerComponent: 8, bytesPerRow: 0, space: cs, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
// flip to top-left origin
ctx.translateBy(x: 0, y: S); ctx.scaleBy(x: 1, y: -1)

func c(_ hex: UInt32, _ a: CGFloat = 1) -> CGColor {
    CGColor(colorSpace: cs, components: [CGFloat((hex >> 16) & 255) / 255, CGFloat((hex >> 8) & 255) / 255, CGFloat(hex & 255) / 255, a])!
}

// Apple's icon grid: 824pt body inside 1024, superellipse corners.
func squircle(_ r: CGRect, n: CGFloat = 5) -> CGPath {
    let p = CGMutablePath()
    let a = r.width / 2, b = r.height / 2, cx = r.midX, cy = r.midY
    let steps = 720
    for i in 0...steps {
        let t = CGFloat(i) / CGFloat(steps) * 2 * .pi
        let ct = cos(t), st = sin(t)
        let x = cx + a * (ct < 0 ? -1 : 1) * pow(abs(ct), 2 / n)
        let y = cy + b * (st < 0 ? -1 : 1) * pow(abs(st), 2 / n)
        i == 0 ? p.move(to: CGPoint(x: x, y: y)) : p.addLine(to: CGPoint(x: x, y: y))
    }
    p.closeSubpath()
    return p
}

let body = CGRect(x: 100, y: 100, width: 824, height: 824)
// Apple's macOS shape: straight sides with smooth corners about 22.5% of the width.
let shape = CGPath(roundedRect: body, cornerWidth: 185, cornerHeight: 185, transform: nil)

// drop shadow
ctx.saveGState()
ctx.setShadow(offset: CGSize(width: 0, height: 12), blur: 28, color: c(0x000000, 0.45))
ctx.addPath(shape); ctx.setFillColor(c(P.b1)); ctx.fillPath()
ctx.restoreGState()

// evergreen body gradient
ctx.saveGState()
ctx.addPath(shape); ctx.clip()
let g = CGGradient(colorsSpace: cs, colors: [c(P.b0), c(P.b1), c(P.b2)] as CFArray, locations: [0, 0.55, 1])!
ctx.drawLinearGradient(g, start: CGPoint(x: 512, y: 100), end: CGPoint(x: 512, y: 924), options: [])
// soft light from the top
let glow = CGGradient(colorsSpace: cs, colors: [c(P.glow, P.glowA), c(P.glow, 0)] as CFArray, locations: [0, 1])!
ctx.drawRadialGradient(glow, startCenter: CGPoint(x: 512, y: 160), startRadius: 0, endCenter: CGPoint(x: 512, y: 160), endRadius: 560, options: [])
// faint inner rim
ctx.addPath(shape); ctx.setStrokeColor(paletteName == "silver" ? c(0x000000, 0.08) : c(0xFFFFFF, 0.10)); ctx.setLineWidth(3); ctx.strokePath()
ctx.restoreGState()

// Eihwaz: one upright, a hook down-right at the top, a hook up-left at the bottom.
let x: CGFloat = 512, top: CGFloat = 262, bottom: CGFloat = 762
let hx: CGFloat = 150, hy: CGFloat = 82
let rune = CGMutablePath()
rune.move(to: CGPoint(x: x + hx, y: top + hy))
rune.addLine(to: CGPoint(x: x, y: top))
rune.addLine(to: CGPoint(x: x, y: bottom))
rune.addLine(to: CGPoint(x: x - hx, y: bottom - hy))
let stroke = rune.copy(strokingWithWidth: 66, lineCap: .round, lineJoin: .round, miterLimit: 10)

// shadow under the rune
ctx.saveGState()
ctx.setShadow(offset: CGSize(width: 0, height: 10), blur: 22, color: c(P.rshadow, paletteName == "silver" ? 0.25 : 0.55))
ctx.addPath(stroke); ctx.setFillColor(c(P.r1)); ctx.fillPath()
ctx.restoreGState()
// ivory gradient on the rune
ctx.saveGState()
ctx.addPath(stroke); ctx.clip()
let ivory = CGGradient(colorsSpace: cs, colors: [c(P.r0), c(P.r1), c(P.r2)] as CFArray, locations: [0, 0.6, 1])!
ctx.drawLinearGradient(ivory, start: CGPoint(x: 512, y: top - 40), end: CGPoint(x: 512, y: bottom + 40), options: [])
ctx.restoreGState()

// a single red yew berry (aril), hanging below the top hook
if berry {
    let center = CGPoint(x: x + hx + 6, y: top + hy + 96)
    let r: CGFloat = 40
    ctx.saveGState()
    ctx.setShadow(offset: CGSize(width: 0, height: 8), blur: 16, color: c(0x2A0505, 0.55))
    ctx.addEllipse(in: CGRect(x: center.x - r, y: center.y - r, width: 2 * r, height: 2 * r)); ctx.setFillColor(c(0xC8312B)); ctx.fillPath()
    ctx.restoreGState()
    ctx.saveGState()
    ctx.addEllipse(in: CGRect(x: center.x - r, y: center.y - r, width: 2 * r, height: 2 * r)); ctx.clip()
    let red = CGGradient(colorsSpace: cs, colors: [c(0xF0675B), c(0xC8312B), c(0x8E1C19)] as CFArray, locations: [0, 0.55, 1])!
    ctx.drawRadialGradient(red, startCenter: CGPoint(x: center.x - 14, y: center.y - 16), startRadius: 2, endCenter: center, endRadius: r, options: [.drawsAfterEndLocation])
    // the aril's open cup, a dark seed showing at the bottom
    ctx.setFillColor(c(0x3A1410, 0.9))
    ctx.addEllipse(in: CGRect(x: center.x - 11, y: center.y + 12, width: 22, height: 16)); ctx.fillPath()
    ctx.restoreGState()
    // highlight
    ctx.setFillColor(c(0xFFFFFF, 0.55))
    ctx.addEllipse(in: CGRect(x: center.x - 22, y: center.y - 26, width: 16, height: 11)); ctx.fillPath()
}

let img = ctx.makeImage()!
let rep = NSBitmapImageRep(cgImage: img)
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: out))
print("wrote \(out)")
