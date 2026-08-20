#!/usr/bin/env swift
// ==================== 生成 Mino 应用图标（极简 >_） ====================
// 紫青渐变圆角底 + 白色终端提示符 `>_`；
// 四周留 10% 透明边距——macOS Dock 对占满画布的无边距图标会自动放大显示
// （视觉"大一圈"），透明边距让 Dock 按标准尺寸渲染。
//
// 与应用内动态 logo（app.rs::draw_logo_mark）同构图：圆底 + `>_`。
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

// 品牌渐变：顶部 #8b5cf6 → 底部 #22d3ee（呼应应用 accent 渐变）。
let topColor = NSColor(calibratedRed: 0x8b / 255.0, green: 0x5c / 255.0, blue: 0xf6 / 255.0, alpha: 1.0)
let bottomColor = NSColor(calibratedRed: 0x22 / 255.0, green: 0xd3 / 255.0, blue: 0xee / 255.0, alpha: 1.0)

for (point, scale) in sizes {
    let px = point * scale
    // 用位图 rep 精确控制像素尺寸（NSImage.lockFocus 在 Retina 屏会按 2x
    // 渲染导致输出翻倍，旧脚本生成 1024px 图标实际是 2048px）。
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

    // 背景：圆角矩形 + 垂直渐变，四周留 10% 透明边距（Dock 标准观感）。
    let margin = CGFloat(px) * 0.10
    let rect = NSRect(x: margin, y: margin, width: CGFloat(px) - margin * 2, height: CGFloat(px) - margin * 2)
    let radius = rect.width * 0.22
    let path = NSBezierPath(roundedRect: rect, xRadius: radius, yRadius: radius)
    let gradient = NSGradient(colors: [topColor, bottomColor])!
    gradient.draw(in: path, angle: -90)

    // `>_`：用圆头线条绘制，避免依赖字体，缩放后仍保持清晰。
    let cg = NSGraphicsContext.current!.cgContext
    let strokeWidth = max(CGFloat(px) * 0.075, 2.0)
    let chevronLeft = rect.minX + rect.width * 0.29
    let chevronTip = rect.minX + rect.width * 0.44
    let chevronHalfHeight = rect.height * 0.18
    let centerY = rect.midY
    cg.setStrokeColor(NSColor.white.withAlphaComponent(0.96).cgColor)
    cg.setLineWidth(strokeWidth)
    cg.setLineCap(.round)
    cg.setLineJoin(.round)
    cg.move(to: CGPoint(x: chevronLeft, y: centerY - chevronHalfHeight))
    cg.addLine(to: CGPoint(x: chevronTip, y: centerY))
    cg.addLine(to: CGPoint(x: chevronLeft, y: centerY + chevronHalfHeight))
    cg.move(to: CGPoint(x: rect.minX + rect.width * 0.54, y: centerY + rect.height * 0.18))
    cg.addLine(to: CGPoint(x: rect.minX + rect.width * 0.73, y: centerY + rect.height * 0.18))
    cg.strokePath()

    NSGraphicsContext.restoreGraphicsState()

    // 写 PNG。
    guard let png = rep.representation(using: .png, properties: [:]) else {
        fputs("生成 PNG 失败: \(px)x\(px)\n", stderr)
        exit(1)
    }
    let name = "icon_\(point)x\(point)\(scale == 2 ? "@2x" : "").png"
    try png.write(to: URL(fileURLWithPath: "\(iconsetDir)/\(name)"))
    print("生成 \(name)（\(px)x\(px)）")
}

print("iconset 完成：\(iconsetDir)")
