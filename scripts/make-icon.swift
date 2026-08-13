#!/usr/bin/env swift
// ==================== 生成 kun 应用图标（K 字 + 篮球） ====================
// 紫金渐变圆角底 + 白色粗体 "K" + 篮球贴 K 字下方（落地压扁姿态）；
// 四周留 10% 透明边距——macOS Dock 对占满画布的无边距图标会自动放大显示
// （视觉"大一圈"），透明边距让 Dock 按标准尺寸渲染。
//
// 与应用内动态 logo（app.rs::draw_logo_mark）同构图：圆底 + K + 篮球。
// 用法：swift scripts/make-icon.swift <输出目录>

import AppKit
import CoreGraphics
import Foundation

let outDir = CommandLine.arguments.count > 1
    ? CommandLine.arguments[1]
    : "/tmp/kun-iconset"
let iconsetDir = "\(outDir)/kun.iconset"
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
// 篮球橙（ikun 招牌色）。
let ballColor = NSColor(calibratedRed: 0xff / 255.0, green: 0x9e / 255.0, blue: 0x2c / 255.0, alpha: 1.0)
let lineColor = NSColor(calibratedRed: 0x2b / 255.0, green: 0x20 / 255.0, blue: 0x33 / 255.0, alpha: 1.0)

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

    // K 字：白色粗体，圆底中央偏上。
    let fontSize = rect.width * 0.46
    let font = NSFont.boldSystemFont(ofSize: fontSize)
    let attrs: [NSAttributedString.Key: Any] = [
        .font: font,
        .foregroundColor: NSColor.white,
    ]
    let kText = NSAttributedString(string: "K", attributes: attrs)
    let kSize = kText.size()
    let kPoint = NSPoint(
        x: rect.midX - kSize.width / 2,
        y: rect.midY - kSize.height / 2 + rect.height * 0.04
    )
    kText.draw(at: kPoint)

    // 篮球：贴 K 字下方，落地压扁姿态（与动态版 squash 状态一致）。
    let ballRadius = rect.width * 0.14
    let ballCenter = NSPoint(x: rect.midX, y: rect.minY + rect.height * 0.24)
    let sx: CGFloat = 1.32 // 横向压扁
    let sy: CGFloat = 0.74 // 纵向收缩
    let bw = ballRadius * sx
    let bh = ballRadius * sy
    let ball = NSBezierPath(ovalIn: NSRect(
        x: ballCenter.x - bw, y: ballCenter.y - bh,
        width: bw * 2, height: bh * 2))
    ballColor.setFill()
    ball.fill()
    lineColor.setStroke()
    ball.lineWidth = max(1.0, ballRadius * 0.17)
    ball.stroke()
    // 竖弧。
    let vert = NSBezierPath()
    vert.move(to: NSPoint(x: ballCenter.x, y: ballCenter.y - bh))
    vert.line(to: NSPoint(x: ballCenter.x, y: ballCenter.y + bh))
    vert.lineWidth = max(0.9, ballRadius * 0.15)
    vert.stroke()
    // 左右侧弧（经典篮球纹理）。
    for sign: CGFloat in [-1.0, 1.0] {
        let arc = NSBezierPath()
        let ax = ballCenter.x + bw * 0.62 * sign
        arc.move(to: NSPoint(x: ax, y: ballCenter.y - bh * 0.82))
        arc.curve(
            to: NSPoint(x: ax, y: ballCenter.y + bh * 0.82),
            controlPoint1: NSPoint(x: ax + ballRadius * 1.5 * sign, y: ballCenter.y),
            controlPoint2: NSPoint(x: ax + ballRadius * 1.5 * sign, y: ballCenter.y))
        arc.lineWidth = max(0.9, ballRadius * 0.15)
        arc.stroke()
    }

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
