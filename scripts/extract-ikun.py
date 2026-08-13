#!/usr/bin/env python3
# ==================== 从 ikun 参考图抠出脸部（透明背景） ====================
# 输入：参考图（白底水彩笔触）
# 处理：自动裁切到非白内容包围盒 + 输出正方形带 8% 透明边距（下方多留 10%
# 给篮球弹跳空间），白/近白像素 → 完全透明
# 输出：assets/ikun_face.png（透明背景 RGBA）
#
# 用法：python3 scripts/extract-ikun.py [输入图] [输出路径]

import sys
from PIL import Image

src_path = sys.argv[1] if len(sys.argv) > 1 else (
    "/Users/yanqi/.zcode/cli/image-cache/sess_63e78d85-a5af-48c5-aed7-ae221b266d97/"
    "image-c07cfde0ce383a8bdcba22e15ce69b2c.png"
)
out_path = sys.argv[2] if len(sys.argv) > 2 else (
    "/Users/yanqi/Desktop/kun/crates/kun-app/assets/ikun_face.png"
)

# 加载原图 → RGBA。
src = Image.open(src_path).convert("RGBA")
w, h = src.size
print(f"源图: {w} x {h}")

# 自动裁切到非白包围盒。
# "白"阈值：RGB 任一通道 < 240 即视为非白（保留笔触里的浅色高光，去掉
# 纯白纸面与浅灰过渡带 → 输出干净 alpha）。
px = src.load()
white_th = 240
min_x, min_y, max_x, max_y = w, h, 0, 0
for y in range(h):
    for x in range(w):
        r, g, b, _ = px[x, y]
        if r < white_th or g < white_th or b < white_th:
            if x < min_x: min_x = x
            if y < min_y: min_y = y
            if x > max_x: max_x = x
            if y > max_y: max_y = y
crop_w = max_x - min_x + 1
crop_h = max_y - min_y + 1
print(f"裁切: ({min_x},{min_y}) - ({max_x},{max_y}) = {crop_w} x {crop_h}")

# 输出正方形 = max(crop_w, crop_h) + 8% 透明边距 + 10% 底距（给篮球空间）。
side = max(crop_w, crop_h)
pad = int(side * 0.08)
bottom_extra = int(side * 0.10)
out_w = side + 2 * pad
out_h = side + 2 * pad + bottom_extra

# 新建**全透明**画布，再把裁切内容贴上去。
out = Image.new("RGBA", (out_w, out_h), (0, 0, 0, 0))
cropped = src.crop((min_x, min_y, min_x + crop_w, min_y + crop_h))
# 裁切内容在 out 内的位置：水平居中，垂直靠顶（pad）。
paste_x = (out_w - crop_w) // 2
paste_y = pad
out.paste(cropped, (paste_x, paste_y), cropped)  # 用 cropped 自己的 alpha 作为 mask

# 把"白"像素强制 alpha=0：使用更激进的阈值（220），吃掉所有灰白过渡带。
out_px = out.load()
final_th = 220
for y in range(out_h):
    for x in range(out_w):
        r, g, b, a = out_px[x, y]
        if a == 0:
            continue  # 已透明，跳过
        if r > final_th and g > final_th and b > final_th:
            out_px[x, y] = (r, g, b, 0)

# 保存。
out.save(out_path, "PNG")
print(f"输出: {out_path} ({out_w} x {out_h})")

# 统计：透明像素比例。
total = out_w * out_h
transparent = sum(1 for y in range(out_h) for x in range(out_w) if out_px[x, y][3] == 0)
print(f"透明像素: {transparent}/{total} = {transparent / total:.1%}")
