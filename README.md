# kun —— 轻量终端 + SFTP 可视化工具

原生 GUI 终端工具：本地/远程（SSH）终端 + 可视化 SFTP 文件管理，参照 Termius/WindTerm 的双栏分屏交互。

## 特性

- **本地终端**：基于 alacritty_terminal 内核（生产级 VT 仿真），支持 256 色、宽字符、滚动、括号粘贴、闪烁光标
- **SSH 远程终端**：密码 / 私钥认证（支持口令），xterm-256color PTY
- **SFTP 可视化**：双栏分屏（终端 | SFTP 可拖拽），目录导航、上传/下载带进度条、删除/重命名/新建目录（带确认对话框）
- **主机管理**：主机配置持久化（`~/.config/kun/hosts.toml`）
- **界面**：Dracula 系深色主题（可切浅色）、SF Mono + 中文 fallback 字体、macOS 原生窗口

## 快捷键

| 快捷键 | 功能 |
|---|---|
| ⌘N | 新建连接 |
| ⌘1 | 切回本地终端 |

## 构建与运行

```bash
cargo build --release
./target/release/kun-app
```

## 测试

```bash
cargo test --workspace          # 单元 + 集成测试
cargo clippy --workspace --all-targets
cargo fmt --all
```

集成测试（`crates/kun-core/tests/`）连接本地测试 sshd（127.0.0.1:2222），可用环境变量覆盖：

```bash
KUN_TEST_HOST=127.0.0.1 KUN_TEST_PORT=2222 KUN_TEST_USER=yanqi KUN_TEST_KEY=~/.ssh/id_ed25519
```

## 架构

```
crates/
├── kun-core/       # 纯引擎（无 UI 依赖）
│   ├── terminal/   # 会话封装（alacritty_terminal 内核）+ 键盘编码
│   ├── ssh/        # 远程终端会话 + SFTP 客户端（russh）
│   └── config/     # 主机配置模型与持久化
└── kun-app/        # egui 应用
    ├── views/      # terminal_view（cell 渲染）/ sftp_view（文件面板）
    └── theme.rs    # Dracula 主题
```

详细设计见 [AGENTS.md](AGENTS.md)。

## 性能（macOS arm64, release）

| 指标 | 数值 | 说明 |
|---|---|---|
| 二进制大小 | 21 MB | 单二进制 |
| 启动时间 | < 300 ms | 冷启动 |
| 运行内存 | ~220 MB | wgpu + 终端缓存（可优化） |

## 已知限制（Roadmap）

- 密码明文存于 hosts.toml（后续接入 macOS Keychain）
- 主机密钥未校验（后续支持 known_hosts）
- SFTP 单连接（后续与终端共用 SSH 连接）
- 多标签页 / 分屏会话（后续版本）
