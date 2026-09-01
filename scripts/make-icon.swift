#!/usr/bin/env swift
// ==================== 生成 Mino 应用图标（极简黑色终端） ====================
// 黑色哑光圆角底 + 极细轮廓 + 干净的 `>_` 几何符号。
// 保留 10% 透明边距，让 Dock 按标准尺寸显示，不显得过大。
//
// 与应用内品牌标记（app.rs::draw_logo_mark）同构图。
// 用法：swift scripts/make-icon.swift <输出目录>

import AppKit
import CoreGraphics
import Foundation

let outDir = CommandLine.arguments.count > 1
    ? CommandLine.arguments[1]
    : "/tmp/mino-iconset"
let iconsetDir = "\(outDir)/mino.iconset"
try? FileManager.default.createDirectory(atPath: iconsetDir, withIntermediateDirectories: true)

// 尺寸列表：iconset 要求的 (pointSize, scale)。
let sizes: [(Int, Int)] = [
    (16, 1), (16, 2),
    (32, 1), (32, 2),
    (128, 1), (128, 2),
    (256, 1), (256, 2),
    (512, 1), (512, 2),
]

// 黑色占主导，符号只使用柔和白与一处终端绿，保持高级克制。
let topColor = NSColor(calibratedRed: 0x16 / 255.0, green: 0x18 / 255.0, blue: 0x19 / 255.0, alpha: 1.0)
let bottomColor = NSColor(calibratedRed: 0x05 / 255.0, green: 0x06 / 255.0, blue: 0x07 / 255.0, alpha: 1.0)
let primaryColor = NSColor(calibratedRed: 0xe8 / 255.0, green: 0xef / 255.0, blue: 0xeb / 255.0, alpha: 1.0)
let accentColor = NSColor(calibratedRed: 0xb8 / 255.0, green: 0xf3 / 255.0, blue: 0x4c / 255.0, alpha: 1.0)

func drawStroke(
    _ path: CGPath,
    in context: CGContext,
    color: NSColor,
    width: CGFloat,
    cap: CGLineCap = .round,
    join: CGLineJoin = .round
) {
    context.saveGState()
    context.setStrokeColor(color.cgColor)
    context.setLineWidth(width)
    context.setLineCap(cap)
    context.setLineJoin(join)
    context.addPath(path)
    context.strokePath()
    context.restoreGState()
}

for (point, scale) in sizes {
    let px = point * scale
    // 用位图 rep 精确控制像素尺寸，避免 Retina 环境下输出翻倍。
    guard let rep = NSBitmapImageRep(
        bitmapDataPlanes: nil,
        pixelsWide: px,
        pixelsHigh: px,
        bitsPerSample: 8,
        samplesPerPixel: 4,
        hasAlpha: true,
        isPlanar: false,
        colorSpaceName: .deviceRGB,
        bytesPerRow: 0,
        bitsPerPixel: 0
    ) else {
        fputs("创建位图失败: \(px)x\(px)\n", stderr)
        exit(1)
    }
    rep.size = NSSize(width: px, height: px)
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)

    let cg = NSGraphicsContext.current!.cgContext
    cg.setAllowsAntialiasing(true)

    // 黑色哑光圆角底：保留非常轻的上下明暗变化，避免纯黑在 Dock 上失去轮廓。
    let margin = CGFloat(px) * 0.10
    let rect = NSRect(
        x: margin,
        y: margin,
        width: CGFloat(px) - margin * 2,
        height: CGFloat(px) - margin * 2
    )
    let radius = rect.width * 0.23
    let tile = NSBezierPath(roundedRect: rect, xRadius: radius, yRadius: radius)
    NSGradient(colors: [topColor, bottomColor])!.draw(in: tile, angle: -90)

    // 极细边缘：只给黑色图标一个精致的实体轮廓。
    NSColor(calibratedWhite: 0.78, alpha: 0.24).setStroke()
    tile.lineWidth = max(CGFloat(px) * 0.008, 1.0)
    tile.stroke()

    // `>_`：不依赖字体，缩放到 16px 仍然保持稳定的几何形状。
    let strokeWidth = max(CGFloat(px) * 0.075, 1.8)
    let centerY = rect.minY + rect.height * 0.47
    let chevron = CGMutablePath()
    chevron.move(to: CGPoint(x: rect.minX + rect.width * 0.30, y: centerY - rect.height * 0.14))
    chevron.addLine(to: CGPoint(x: rect.minX + rect.width * 0.45, y: centerY))
    chevron.addLine(to: CGPoint(x: rect.minX + rect.width * 0.30, y: centerY + rect.height * 0.14))
    let cursor = CGMutablePath()
    cursor.move(to: CGPoint(x: rect.minX + rect.width * 0.56, y: centerY - rect.height * 0.14))
    cursor.addLine(to: CGPoint(x: rect.minX + rect.width * 0.75, y: centerY - rect.height * 0.14))

    drawStroke(chevron, in: cg, color: primaryColor, width: strokeWidth)
    drawStroke(cursor, in: cg, color: accentColor, width: strokeWidth, cap: .butt)

    NSGraphicsContext.restoreGraphicsState()

    guard let png = rep.representation(using: .png, properties: [:]) else {
        fputs("生成 PNG 失败: \(px)x\(px)\n", stderr)
        exit(1)
    }
    let name = "icon_\(point)x\(point)\(scale == 2 ? "@2x" : "").png"
    try png.write(to: URL(fileURLWithPath: "\(iconsetDir)/\(name)"))
    print("生成 \(name)（\(px)x\(px)）")
}

print("iconset 完成：\(iconsetDir)")
