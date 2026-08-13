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

## 样式体系（Warp 风格）

- 设计 token 在 `theme.rs::tokens`：分层背景 `#0e0e11`（应用）/`#151519`（标题栏）/`#1a1a20`（面板）/`#24242c`（浮层）/`#0b0b0f`（终端，最深）
- 文字三阶 `#f4f4f6/#bcbcc4/#80808a`；accent `#8b5cf6` + `ACCENT_SOFT`（15% 透明）做 hover/选中底；品牌渐变紫→青（`accent2 #22d3ee`）
- 边框统一半透明白（`BORDER_SUBTLE`）；圆角 8px（按钮/输入）/7px（列表项/标签页）
- 面板用 `Panel::frame(Frame)` 指定背景与边框；主按钮用 accent 填充
- 终端背景在 TerminalView 里绘制 `BG_TERMINAL`；SFTP/主机列表行 hover/选中用 `ACCENT_SOFT` 圆角底
- **动效**（`anim.rs`，无第三方依赖）：`smooth/smooth_bool` 指数平滑（状态存 ctx 临时数据，收敛中自动 request_repaint）、`paint_shimmer_line` 扫光 Mesh、`paint_rounded_gradient` 圆角渐变三角扇、`paint_glow` 辉光；工具栏底部扫光仅在 hover/更新活跃时持续重绘（省电）
- 终端内容内边距 `PADDING=10`（`terminal_view.rs`）：背景铺满 `ui.max_rect()`，文本/光标在内边距内绘制。**注意 egui 陷阱：`ui.min_rect()` 是"已用内容"包围盒，无子项时为 0x0**——背景/点击区域必须用 `ui.max_rect()`（布局分配区域），否则背景画不出来、点击无法重新聚焦
- 主机条目支持单击选中、双击连接；条目样式：**accent 圆形头像（主机名首字符）+ 名称 + 🗑 删除图标**（行右侧，hover 红底）
- **egui 0.36 交互坑：`Response::interact()`（scope_builder(...).response.interact(...)）的点击无法命中**（响应链问题，kittest 实测 clicked/hovered 恒 false）——必须用 `ui.interact(rect, id, sense)` 显式注册交互区，删除按钮等行内控件最后注册以覆盖行点击区
- 终端调色板（Catppuccin Mocha 16 色 + xterm 256 色表）在渲染层解析（`theme.rs::TERM_PALETTE_16`/`xterm256`），优先级：Spec > OSC 覆盖（term.colors）> 内置调色板；`Term.colors` 默认全 None，不设调色板则全部渲染为白色
- 选中主机条目左侧 2px accent 竖条；标签页底部有选中指示条（宽度动画）
- **三套深色主题**（`theme.rs::THEMES`）：深色（紫金）/ 深蓝 / 霓虹；每套含 UI token + 终端调色板（16 色 + fg/bg/cursor）；`current_theme()` 静态读取，工具栏 ComboBox 切换（`set_theme`）
- 主题切换后需重新渲染终端（terminal_view 每帧读 `current_theme()`）
- 终端输入/回车功能验证正常；若用户"回车不执行"多为中文输入法（IME）激活时回车被输入法消费（确认拼音候选），切英文输入法即可（所有终端应用共性）
- **"删除键插入空格"（微信输入法 wetype 等）**：退格/删除键按下时输入法会伴随发送"空格类" Text 事件（ASCII 空格/零宽），写入终端表现为插入空格——已修复：`suppress_next_text` 跨帧标记 + 退格后一帧内的空白类 Text 丢弃（只影响空白字符，不误伤正常输入）；回归测试 `退格伴随空格文本不插入`
- 顶部工具栏：左侧品牌区（ikun logo + kun + 版本号，点击 logo 折叠主机列表）（默认**收起**，启动直接进终端，⌘B 切换）、右侧主题 ComboBox + 更新状态圆点/检查更新按钮；新建连接走左侧栏按钮/⌘N，新建本地终端走标签栏 ＋/⌘T
- **应用内更新**（`kun-core::updater` + `app.rs` 状态机）：版本检查走 `releases.atom`（不受 API 限流），资产直链按 `kun-{版本}-macos-{arm64|x64}.dmg` 构造并流式下载（进度回调）；安装由后台 shell 脚本完成——`hdiutil attach` 挂载 → `pgrep -x kun-app` 等待主进程退出 → `ditto` 替换 `/Applications/kun.app`（失败回退 `~/Applications`）→ `open` 重启；`installed` 状态延时 0.9s 后 `ViewportCommand::Close`
- 主机行双击连接为**自实现检测**（`last_row_click`：0.3s 内同行的第二次点击）：egui 多击计数会把无关点击（如工具栏其他按钮）计入序列导致 count=3 而 `double_clicked`（count==2）失效
- **多标签页**（warp 风格）：`KunApp` 持 `tabs: Vec<TerminalTab>`（`label + TerminalView + sftp`），SFTP 按标签页挂载（远程 tab 独享）；标签栏在工具栏下方（`Panel::top("tabs")`），当前标签 accent-soft 高亮，支持点击切换/×关闭/＋新建；快捷键 ⌘T 新建本地、⌘W 关闭当前、⌘1-9 切换、⌘N 新建连接、⌥1-3 主题
- 本地终端默认工作目录为 **home**（`local_session_options`：`working_directory = $HOME`）——Finder/Dock 启动时进程 cwd 为 `/`，不指定会导致终端落在根目录；远程会话不受影响（由 sshd 决定）
- 终端内容颜色：prompt 彩色来自 shell 主题（如 oh-my-zsh robbyrussell 的绿/青/蓝/黄）；`ls` 无色是 macOS 默认行为（需 `alias ls='ls -G'`），kun 不篡改 shell 输出
- 主题背景跟随当前深色主题；`apply_theme` 同步系统深色主题（`ctx.set_theme`）
- 主题快捷键：⌥1-⌥3 快速切换三套主题

## 关键约定

- 集成测试依赖本地测试 sshd：`/usr/sbin/sshd -f /tmp/kun-test-sshd/sshd_config`（端口 2222、公钥认证、含 sftp subsystem）
- 配置文件：`~/.config/kun/hosts.toml`（toml 中 enum 用内部标记：`[hosts.auth.Key]`）
- 应用图标：`assets/icon.png`（渐变紫 K，make-icon.swift 生成）→ `load_icon()` 解码为 IconData → `ViewportBuilder::with_icon`——**eframe 在 macOS 上通过 NSApp 运行时设置 Dock 图标**，无 .app bundle 的 debug 构建也能生效；`.app` 安装版的 Dock 图标由 package-macos.sh 的 kun.icns 提供（同一设计）
- 字体：Monospace 族 = SF Mono（主）+ **Menlo（符号 fallback）** + STHeiti（CJK）+ egui 默认；Proportional 族 = SF 主 + STHeiti。**SF Mono 缺 `➜`(U+279C)/`❯`(U+276F)/`⚡` 等常用 zsh 提示符符号**，缺字形会被 egui 渲染为 `?` 替换符；Menlo 同为等宽且完整覆盖（宽度一致不漂移），必须排在 CJK fallback 之前。**禁止加载 Apple Color Emoji.ttc**（192MB 彩色位图字体，ab_glyph 无法解析 → egui panic）
- 提示符 `?➜` 中的 `?` 是 oh-my-zsh robbyrussell 主题 `%1{➜%}` 语法在 zsh 5.9 的真实输出（script 捕获字节流验证：`0x3F E2 9E 9C`），Terminal.app 同样显示，**非 kun 渲染问题，勿尝试"修复"**
- egui 0.36 API 注意：`App::ui` 替代 `update`、`Panel::top/left` 替代 `TopBottomPanel`、`Fonts` 需要 `fonts_mut`、`Event::Key` 无 `text` 字段（Text 独立事件）
- 终端视图使用固定 `focus_id` 管理键盘焦点；对话框打开时自动聚焦首个输入框

## 验证

```bash
cargo test --workspace         # 36 个测试（单元 + ssh 集成 + sftp 集成 + UI 渲染 + 字体链 + 标签页 + 双击交互）
cargo clippy --workspace --all-targets   # 零警告
cargo fmt --all
```
