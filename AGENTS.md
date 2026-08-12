# kun 项目架构

轻量终端 + SFTP 可视化工具。Rust workspace，两个 crate。

## 技术选型

| 组件 | 选型 | 理由 |
|---|---|---|
| GUI | eframe/egui 0.36 + wgpu | 原生轻量、macOS 走 Metal |
| 终端内核 | alacritty_terminal 0.26 | 生产级 VT 仿真 + PTY（Alacritty 自用） |
| SSH/SFTP | russh 0.62 + russh-sftp 2.4 | 纯 Rust 无 C 依赖 |
| 配置 | serde + toml | hosts.toml 持久化 |
| UI 测试 | egui_kittest | 渲染级断言 |

## 目录结构

```
crates/
├── kun-core/                 # 纯引擎，禁止依赖 egui
│   ├── src/lib.rs
│   ├── src/terminal/mod.rs   # Session 统一封装（本地/远程）、TermSize、Listener
│   ├── src/terminal/keys.rs  # 键盘 → 字节序列编码（XTerm 修饰键）
│   ├── src/ssh/mod.rs        # connect_remote（后台线程 + tokio）、authenticate
│   ├── src/ssh/sftp.rs       # connect_sftp、SftpHandle（命令队列）、SftpEvent（事件流）
│   ├── src/config/mod.rs     # HostProfile/HostConfig + hosts.toml
│   └── tests/                # 集成测试（需本地测试 sshd: 127.0.0.1:2222）
└── kun-app/                  # egui 应用
    ├── src/main.rs           # 入口 + 字体加载（SF Mono + CJK fallback）
    ├── src/app.rs            # KunApp：布局/连接管理/对话框/快捷键
    ├── src/theme.rs          # Dracula 深色主题
    └── src/views/
        ├── terminal_view.rs  # cell 渲染（增量 hash 缓存）、输入转发、滚动
        └── sftp_view.rs      # 文件面板：列表/导航/传输进度/确认对话框
```

## 核心数据流

### 终端会话（本地与远程统一）

- 本地：`tty::new` 建 PTY → alacritty `EventLoop` 线程读 → vte 解析 → `Term` 网格
- 远程：后台线程 tokio runtime → SSH channel 读 → `Processor::advance` → 同一 `Term`
- 统一接口：`Session { term, writer, resizer, shuttor }`（Writer/Resizer/Shuttor 闭包抽象）
- 写操作：本地走 `EventLoopSender`，远程走命令队列（UI 线程非阻塞）
- 渲染：UI 锁 `FairMutex<Term>` → `renderable_content()` → 逐行 hash 缓存 LayoutJob（增量重建）

### SFTP

- `connect_sftp` 建立独立 SSH 连接（后台线程 runtime 存活到 `Shutdown` 命令）
- UI 持 `SftpHandle`（克隆发送端）发命令；后台执行后经 `SftpEvent` 通道回报
- 事件流：`Listed / Progress / Done / Error / Closed`

### 连接生命周期

- 远程连接后台线程必须持有 runtime 直到会话关闭（`Notify` 等待 remote_loop 结束），否则 tokio::spawn 的任务被取消
- `Session` 实现 `Drop` → 发 Shutdown 优雅关闭

## 样式体系（借鉴 MiroCode / Warp）

- 设计 token 在 `theme.rs::miro`：分层背景 `#0a0a0d`（应用）/`#141418`（标题栏）/`#1c1c22`（面板）/`#28282f`（浮层）/`#06060a`（终端，最深）
- 文字三阶 `#f5f5f7/#c7c7cc/#8e8e93`；accent `#8b5cf6` + `ACCENT_SOFT`（16% 透明）做 hover/选中底
- 边框统一 5% 半透明白（`BORDER_SUBTLE`）；圆角 10px（按钮/输入）/6px（列表项）
- 面板用 `Panel::frame(Frame)` 指定背景与边框；主按钮用 accent 填充
- 终端背景在 TerminalView 里绘制 `BG_TERMINAL`；SFTP/主机列表行 hover/选中用 `ACCENT_SOFT` 圆角底
- 主机条目支持单击选中、双击连接
- 终端调色板（Catppuccin Mocha 16 色 + xterm 256 色表）在渲染层解析（`theme.rs::TERM_PALETTE_16`/`xterm256`），优先级：Spec > OSC 覆盖（term.colors）> 内置调色板；`Term.colors` 默认全 None，不设调色板则全部渲染为白色
- 工具栏底部有紫→青渐变指示线（`draw_gradient_line`）；选中主机条目左侧 2px accent 竖条
- **四套主题**（`theme.rs::THEMES`，对齐 MiroCode）：Miro 深色（紫）/ Dawn 浅色 / Midnight 深蓝 / Cyberpunk 霓虹；每套含 UI token + 终端调色板（16 色 + fg/bg/cursor）；`current_theme()` 静态读取，工具栏 ComboBox 切换（`set_theme`）
- 主题切换后需重新渲染终端（terminal_view 每帧读 `current_theme()`）
- 终端输入/回车功能验证正常；若用户"回车不执行"多为中文输入法（IME）激活时回车被输入法消费（确认拼音候选），切英文输入法即可（所有终端应用共性）
- 浅色主题兼容：终端背景强制跟随主题（忽略 OSC 11 背景覆盖——zsh 主题常设深色背景会破坏浅色主题）；`apply_theme` 同步系统主题（`ctx.set_theme`）使 macOS 标题栏跟随浅/深色
- 主题快捷键：⌥1-⌥4 快速切换四套主题

## 关键约定

- 集成测试依赖本地测试 sshd：`/usr/sbin/sshd -f /tmp/kun-test-sshd/sshd_config`（端口 2222、公钥认证、含 sftp subsystem）
- 配置文件：`~/.config/kun/hosts.toml`（toml 中 enum 用内部标记：`[hosts.auth.Key]`）
- 字体：仅 Monospace/Proportional 族加载 CJK fallback（STHeiti），否则中文显示豆腐块
- egui 0.36 API 注意：`App::ui` 替代 `update`、`Panel::top/left` 替代 `TopBottomPanel`、`Fonts` 需要 `fonts_mut`、`Event::Key` 无 `text` 字段（Text 独立事件）
- 终端视图使用固定 `focus_id` 管理键盘焦点；对话框打开时自动聚焦首个输入框

## 验证

```bash
cargo test --workspace         # 17 个测试（单元 + ssh 集成 + sftp 集成 + UI 渲染）
cargo clippy --workspace --all-targets   # 零警告
cargo fmt --all
```
