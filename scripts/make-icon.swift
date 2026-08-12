#!/usr/bin/env swift
// ==================== 生成 kun 应用图标 ====================
// 渐变紫圆角方块 + "K" 字母，输出 iconset 所需的多尺寸 PNG。
// 用法：swift scripts/make-icon.swift <输出目录>
// 之后用 iconutil 合并为 icns。

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

// 渐变紫：顶部 #8b5cf6 → 底部 #22d3ee（呼应应用 accent 渐变）。
let topColor = NSColor(calibratedRed: 0x8b / 255.0, green: 0x5c / 255.0, blue: 0xf6 / 255.0, alpha: 1.0)
let bottomColor = NSColor(calibratedRed: 0x22 / 255.0, green: 0xd3 / 255.0, blue: 0xee / 255.0, alpha: 1.0)

for (point, scale) in sizes {
    let px = point * scale
    let image = NSImage(size: NSSize(width: px, height: px))
    image.lockFocus()

    // 背景：圆角矩形 + 垂直渐变。
    let rect = NSRect(x: 0, y: 0, width: px, height: px)
    let radius = CGFloat(px) * 0.22
    let path = NSBezierPath(roundedRect: rect, xRadius: radius, yRadius: radius)
    let gradient = NSGradient(colors: [topColor, bottomColor])!
    gradient.draw(in: path, angle: -90)

    // 字母 K：白色粗体，居中。
    let fontSize = CGFloat(px) * 0.55
    let font = NSFont.boldSystemFont(ofSize: fontSize)
    let attrs: [NSAttributedString.Key: Any] = [
        .font: font,
        .foregroundColor: NSColor.white,
    ]
    let text = NSAttributedString(string: "K", attributes: attrs)
    let textSize = text.size()
    let textPoint = NSPoint(
        x: (CGFloat(px) - textSize.width) / 2,
        y: (CGFloat(px) - textSize.height) / 2 - fontSize * 0.02
    )
    text.draw(at: textPoint)

    image.unlockFocus()

    // 写 PNG。
    guard let tiff = image.tiffRepresentation,
          let rep = NSBitmapImageRep(data: tiff),
          let png = rep.representation(using: .png, properties: [:]) else {
        fputs("生成 PNG 失败: \(px)x\(px)\n", stderr)
        exit(1)
    }
    let name = "icon_\(point)x\(point)\(scale == 2 ? "@2x" : "").png"
    try png.write(to: URL(fileURLWithPath: "\(iconsetDir)/\(name)"))
    print("生成 \(name)（\(px)x\(px)）")
}

print("iconset 完成：\(iconsetDir)")
