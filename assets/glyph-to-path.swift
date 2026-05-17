import CoreText
import CoreGraphics
import Foundation

// Output the SVG path data for a single glyph rendered in a specific font.
// Usage: swift glyph-to-path.swift <PostScript-font-name> <character> <font-size>

guard CommandLine.arguments.count == 4,
      let fontSize = Double(CommandLine.arguments[3]) else {
    FileHandle.standardError.write("usage: glyph-to-path.swift <font-ps-name> <char> <font-size>\n".data(using: .utf8)!)
    exit(2)
}
let fontName = CommandLine.arguments[1]
let ch = CommandLine.arguments[2]

let font = CTFontCreateWithName(fontName as CFString, CGFloat(fontSize), nil)
let actualName = CTFontCopyPostScriptName(font) as String
FileHandle.standardError.write("resolved font: \(actualName)\n".data(using: .utf8)!)

let scalar = ch.unicodeScalars.first!
var unichars: [UniChar] = Array(String(scalar).utf16)
var glyphs = [CGGlyph](repeating: 0, count: unichars.count)
let ok = CTFontGetGlyphsForCharacters(font, unichars, &glyphs, unichars.count)
guard ok, let cgpath = CTFontCreatePathForGlyph(font, glyphs[0], nil) else {
    FileHandle.standardError.write("no path for glyph\n".data(using: .utf8)!)
    exit(1)
}

let bbox = cgpath.boundingBox
FileHandle.standardError.write("bbox: x=\(bbox.minX) y=\(bbox.minY) w=\(bbox.width) h=\(bbox.height)\n".data(using: .utf8)!)
FileHandle.standardError.write("ascent=\(CTFontGetAscent(font)) descent=\(CTFontGetDescent(font)) capHeight=\(CTFontGetCapHeight(font))\n".data(using: .utf8)!)

var d = ""
class Carrier { var s = "" }
let carrier = Carrier()
let ptr = Unmanaged.passUnretained(carrier).toOpaque()

cgpath.apply(info: ptr) { (info, elPtr) in
    let carrier = Unmanaged<Carrier>.fromOpaque(info!).takeUnretainedValue()
    let el = elPtr.pointee
    func fmt(_ p: CGPoint) -> String { return "\(p.x) \(p.y)" }
    switch el.type {
    case .moveToPoint:
        carrier.s += "M\(fmt(el.points[0])) "
    case .addLineToPoint:
        carrier.s += "L\(fmt(el.points[0])) "
    case .addQuadCurveToPoint:
        carrier.s += "Q\(fmt(el.points[0])) \(fmt(el.points[1])) "
    case .addCurveToPoint:
        carrier.s += "C\(fmt(el.points[0])) \(fmt(el.points[1])) \(fmt(el.points[2])) "
    case .closeSubpath:
        carrier.s += "Z "
    @unknown default:
        break
    }
}
d = carrier.s
print(d)
