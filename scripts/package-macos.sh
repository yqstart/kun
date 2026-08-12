#!/bin/bash
# ==================== macOS 打包：生成 .app bundle + dmg ====================
# 用法：scripts/package-macos.sh [版本号] [架构: arm64|x64]
# 依赖：已执行 cargo build --release；iconset 由 make-icon.swift 生成。
set -e

VERSION="${1:-0.1.0}"
ARCH="${2:-arm64}"
BIN="target/release/kun-app"
APP_NAME="kun"
APP_DIR="target/release/${APP_NAME}.app"

if [ ! -f "$BIN" ]; then
    echo "错误：未找到 $BIN，请先 cargo build --release" >&2
    exit 1
fi

# ==================== 1. 生成图标（iconset → icns） ====================
ICONSET_DIR="target/release/kun.iconset"
swift scripts/make-icon.swift target/release > /dev/null
iconutil -c icns "$ICONSET_DIR" -o target/release/kun.icns
rm -rf "$ICONSET_DIR"
echo "图标生成完成"

# ==================== 2. 组装 .app bundle ====================
rm -rf "$APP_DIR"
mkdir -p "$APP_DIR/Contents/MacOS" "$APP_DIR/Contents/Resources"
cp "$BIN" "$APP_DIR/Contents/MacOS/${APP_NAME}-app"
cp target/release/kun.icns "$APP_DIR/Contents/Resources/kun.icns"

cat > "$APP_DIR/Contents/Info.plist" << EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>kun</string>
    <key>CFBundleDisplayName</key>
    <string>kun</string>
    <key>CFBundleIdentifier</key>
    <string>dev.kun.terminal</string>
    <key>CFBundleVersion</key>
    <string>${VERSION}</string>
    <key>CFBundleShortVersionString</key>
    <string>${VERSION}</string>
    <key>CFBundleExecutable</key>
    <string>kun-app</string>
    <key>CFBundleIconFile</key>
    <string>kun</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>LSMinimumSystemVersion</key>
    <string>12.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSPrincipalClass</key>
    <string>NSApplication</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.developer-tools</string>
</dict>
</plist>
EOF

echo ".app bundle 完成：$APP_DIR"

# ==================== 3. 制作 dmg（文件名含架构，避免 ARM/Intel 产物互相覆盖） ====================
DMG="target/release/kun-${VERSION}-macos-${ARCH}.dmg"
rm -f "$DMG"
hdiutil create -volname "$APP_NAME" -srcfolder "$APP_DIR" -ov -format UDZO "$DMG" > /dev/null
echo "dmg 完成：$DMG"
