// Genera el fondo del .dmg (usado por el workflow release-macos.yml y para
// builds locales). Escribe background.png (600x400) y background@2x.png
// (1200x800) en el directorio pasado como argumento.
//
//   swift packaging/macos/make_bg.swift <dir-salida>
import AppKit

let W: CGFloat = 600, H: CGFloat = 400

func render(scale: CGFloat, to path: String) {
    let pw = Int(W * scale), ph = Int(H * scale)
    let cs = CGColorSpaceCreateDeviceRGB()
    guard let ctx = CGContext(data: nil, width: pw, height: ph, bitsPerComponent: 8,
                              bytesPerRow: 0, space: cs,
                              bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else {
        fatalError("ctx")
    }
    ctx.scaleBy(x: scale, y: scale)
    NSGraphicsContext.current = NSGraphicsContext(cgContext: ctx, flipped: false)

    // Fondo: degradado vertical (slate oscuro).
    let top = NSColor(calibratedRed: 0.13, green: 0.15, blue: 0.22, alpha: 1)
    let bot = NSColor(calibratedRed: 0.09, green: 0.10, blue: 0.15, alpha: 1)
    NSGradient(starting: bot, ending: top)!.draw(in: NSRect(x: 0, y: 0, width: W, height: H), angle: 90)

    // Título.
    let title = "Renombrador"
    let tAttrs: [NSAttributedString.Key: Any] = [
        .font: NSFont.systemFont(ofSize: 30, weight: .bold),
        .foregroundColor: NSColor.white,
    ]
    let tSize = title.size(withAttributes: tAttrs)
    title.draw(at: NSPoint(x: (W - tSize.width) / 2, y: H - 62), withAttributes: tAttrs)

    // Instrucción.
    let sub = "Arrastra la app a la carpeta Aplicaciones"
    let sAttrs: [NSAttributedString.Key: Any] = [
        .font: NSFont.systemFont(ofSize: 14, weight: .regular),
        .foregroundColor: NSColor(white: 0.72, alpha: 1),
    ]
    let sSize = sub.size(withAttributes: sAttrs)
    sub.draw(at: NSPoint(x: (W - sSize.width) / 2, y: 40), withAttributes: sAttrs)

    // Flecha app -> Aplicaciones (a la altura de los iconos, y=205 desde arriba).
    let ay: CGFloat = H - 205
    ctx.setStrokeColor(NSColor(white: 0.85, alpha: 0.9).cgColor)
    ctx.setLineWidth(3)
    ctx.setLineCap(.round)
    let ax0: CGFloat = 232, ax1: CGFloat = 360
    ctx.move(to: CGPoint(x: ax0, y: ay))
    ctx.addLine(to: CGPoint(x: ax1, y: ay))
    ctx.strokePath()
    ctx.setFillColor(NSColor(white: 0.85, alpha: 0.9).cgColor)
    ctx.move(to: CGPoint(x: ax1 + 14, y: ay))
    ctx.addLine(to: CGPoint(x: ax1, y: ay + 9))
    ctx.addLine(to: CGPoint(x: ax1, y: ay - 9))
    ctx.closePath()
    ctx.fillPath()

    guard let img = ctx.makeImage() else { fatalError("img") }
    let url = URL(fileURLWithPath: path)
    guard let dest = CGImageDestinationCreateWithURL(url as CFURL, "public.png" as CFString, 1, nil) else {
        fatalError("dest")
    }
    CGImageDestinationAddImage(dest, img, nil)
    CGImageDestinationFinalize(dest)
}

let outDir = CommandLine.arguments[1]
render(scale: 1, to: outDir + "/background.png")
render(scale: 2, to: outDir + "/background@2x.png")
print("wrote backgrounds to \(outDir)")
