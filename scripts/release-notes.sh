#!/bin/bash
# ==================== 提取最新版本段落作为 Release 正文 ====================
# 用法：scripts/release-notes.sh [版本号]
# 输出：CHANGELOG.md 中对应版本的段落（找不到则输出整份 changelog）。

set -e

VERSION="${1:-}"
CHANGELOG="CHANGELOG.md"

if [ ! -f "$CHANGELOG" ]; then
    echo "错误：未找到 $CHANGELOG" >&2
    exit 1
fi

if [ -n "$VERSION" ]; then
    # 提取指定版本段落：## [x.y.z] ... 到下一个 ## 或文件尾。
    awk -v ver="$VERSION" '
        $0 ~ "^## \\[" ver "\\]" { found=1; print; next }
        found && /^## / { exit }
        found { print }
    ' "$CHANGELOG"
else
    # 提取第一个版本段落（最新）。
    awk '
        /^## / { if (started) exit; started=1 }
        started { print }
    ' "$CHANGELOG"
fi
