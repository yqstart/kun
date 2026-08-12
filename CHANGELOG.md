# 更新日志

本文件为发布正文提供内容（`scripts/release-notes.sh` 提取最新版本段落）。

## [0.1.0] - 2026-08-12

### 新增

- **本地终端**：基于 alacritty_terminal 生产级内核，支持 256 色、宽字符、滚动、括号粘贴、闪烁光标
- **SSH 远程终端**：密码 / 私钥认证（支持口令），xterm-256color PTY
- **SFTP 可视化**：双栏分屏（终端 | SFTP 可拖拽），目录导航、上传/下载带进度条、删除/重命名/新建目录（带确认对话框）
- **主机管理**：主机配置持久化（`~/.config/kun/hosts.toml`），单击选中、双击连接

### 样式

- **四套主题**（对齐 MiroCode）：Miro 深色（紫）/ Dawn 浅色 / Midnight 深蓝 / Cyberpunk 霓虹，每套含独立终端调色板
- 工具栏主题下拉切换（⌥1-⌥4 快捷键）
- 终端调色板：Catppuccin Mocha 16 色 + xterm 256 色表
- 浅色主题兼容：终端背景跟随主题、macOS 标题栏同步系统主题

### 修复

- 修复点击"新建连接"后崩溃（ui.input 闭包内 request_repaint 死锁）
- 修复输入法产生的零宽字符插入终端（退格键异常）
- 修复中文显示豆腐块（CJK fallback 字体）

### 安装

- **macOS**：下载 `kun-<版本>-macos.dmg`，打开并拖入"应用程序"；首次打开若提示未认证，右键 → 打开
- **Linux**：下载 `kun-<版本>-linux-x64.tar.gz`，解压后运行 `kun-app`
- **Windows**：下载 `kun-<版本>-windows-x64.zip`，解压后运行 `kun-app.exe`

> 注：当前版本未签名/未公证，macOS 首次打开需在"系统设置 → 隐私与安全性"中允许；Windows 可能出现 SmartScreen 提示。
