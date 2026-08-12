# kun —— 轻量终端 + SFTP 可视化工具设计方案

## 一、产品定位
原生 GUI 桌面应用（macOS 优先，架构预留跨平台）。目标指标：release 单二进制 ~15MB、内存 <150MB、启动 <300ms。
交互参照：Termius（终端+SFTP 双栏分屏）、WindTerm（一体化会话）、lazygit（轻量现代美学）。

## 二、技术选型（复用成熟方案，不重复造轮子）

| 组件 | 选型 | 理由 |
|---|---|---|
| 语言 | Rust (2021) | 轻量、安全、跨平台 |
| GUI | eframe/egui + wgpu | macOS 走 Metal；立即模式、无 async 框架负担；MIT |
| 终端内核 | alacritty_terminal crate | 生产级 VT 仿真 + PTY 管理（vte 解析），Apache-2.0/MIT，业界验证（Alacritty 自用） |
| SSH/SFTP | russh + russh-sftp | 纯 Rust 无 C 依赖、Tokio 异步；Yazi 文件管理器生产先例 |
| 异步 | tokio | russh 原生生态 |
| 配置 | serde + toml | 主机配置持久化 |

## 三、目录结构（workspace，内核与 UI 分离便于测试）

```
kun/
├── Cargo.toml              # workspace
├── crates/
│   ├── kun-core/           # 纯引擎，无 UI 依赖
│   │   ├── terminal/       # 终端会话封装（PTY + alacritty Terminal + 事件通道）
│   │   ├── sftp/           # SSH 连接 + SFTP 客户端（列表/上传/下载/删除/重命名/mkdir，带进度回调）
│   │   └── config/         # HostProfile 模型 + hosts.toml 读写
│   └── kun-app/            # egui 应用壳
│       ├── app.rs          # 布局：侧栏 + 主区双栏 + 状态栏
│       ├── views/
│       │   ├── terminal_view.rs   # cell 渲染（增量 diff）+ 键盘/鼠标输入转发 + 滚动
│       │   ├── sftp_view.rs       # 远程文件列表/导航/工具栏/传输进度
│       │   └── host_list.rs       # 主机列表 + 连接对话框
│       └── theme.rs        # 深/浅主题 + 终端配色（Dracula 系）
```

## 四、核心数据流
- **终端**：alacritty EventLoop 读 PTY → Terminal 屏幕状态 → 每帧只重绘 dirty lines（egui LayoutJob 缓存）→ 输入（按键/粘贴/IME）→ Terminal → PTY 写
- **SFTP**：后台 tokio runtime 执行操作 → `mpsc` 通道向 UI 发消息（目录列表/传输进度/完成/错误）
- **会话**：HostProfile（名称/主机/端口/用户/认证：密码或私钥）→ SSH 连接 → 终端 Channel + SFTP 客户端共享会话

## 五、UI 设计（参照 Termius 交互）
- 左侧栏：主机/会话列表，连接按钮
- 主区：终端与 SFTP 面板水平分屏（可拖拽比例、可单栏全屏切换）
- 状态栏：连接信息、传输进度、路径
- 深色主题默认（Dracula 配色）+ 浅色切换；系统等宽字体（SF Mono → Menlo fallback）

## 六、里程碑（每步可运行验证）
1. **M1 本地终端**：窗口骨架 + TerminalView + 本地 zsh 会话（可打字、颜色、滚动）
2. **M2 SSH 远程终端**：主机表单 + 密码/私钥认证 + 远程终端会话
3. **M3 SFTP 双栏**：文件列表/目录导航 + 上传/下载（带进度条）
4. **M4 文件管理**：删除/重命名/新建目录/刷新 + 错误提示
5. **M5 打磨**：主机配置持久化（~/.config/kun/hosts.toml）、快捷键（⌘T 新会话、⌘1/2 切换）、主题、状态栏

## 七、验证方式
- `cargo build --release`、`cargo test`、`cargo clippy`、`cargo fmt` 全绿
- 单元测试：配置解析、路径拼接、消息通道
- 手动冒烟：本地终端输入回显、`ssh localhost` 连接、SFTP 上传下载实际文件、传输进度显示
- 性能检查：记录 release 二进制大小与运行内存

## 八、风险与对策
- alacritty_terminal API 属 beta → 锁定具体版本号
- egui 逐 cell 文本渲染性能 → 增量 diff + LayoutJob 缓存，必要时改纹理渲染
- russh API 变动期 → 锁定版本，仅使用其稳定子集（connect/auth/sftp）
- 密码存储 MVP 先明文 toml 并注释警告，后续接入 macOS Keychain