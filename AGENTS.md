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
    ├── src/main.rs           # 入口 + 字体加载（SF Mono + CJK fallback）+ 圆角注入
    ├── src/native.rs         # macOS 原生窗口定制（无边框窗口整体圆角，AppKit layer）
    ├── src/app.rs            # KunApp：tabs 列表 + 设置弹窗（show_settings）+ 布局/连接管理/对话框/快捷键
    ├── src/theme.rs          # Dracula 深色主题
    └── src/views/
        ├── terminal_view.rs  # cell 渲染（增量 hash 缓存）、输入转发、滚动
        └── sftp_view.rs      # 文件面板：列表/导航/传输进度/确认对话框
```

## 核心数据流

### 终端会话（本地与远程统一）

- 本地：`tty::new` 建 PTY → alacritty `EventLoop` 线程读 → vte 解析 → `Term` 网格
- 远程：后台线程 tokio runtime → SSH channel 读 → `Processor::advance` → 同一 `Term`
- 统一接口：`Session { term, writer, resizer, shuttor, is_remote }`（Writer/Resizer/Shuttor 闭包抽象）；`Session::is_remote()` 决定本地能力（补全）是否可用
- 写操作：本地走 `EventLoopSender`，远程走命令队列（UI 线程非阻塞）
- 渲染：UI 锁 `FairMutex<Term>` → `renderable_content()` → 逐行 hash 缓存 LayoutJob（增量重建）

### 基础补全（`kun-app/src/completion.rs`，Warp 风格）

- 仅本地会话启用（远程无本机文件系统对应）；`InputModel` 跟踪输入行（与写入 PTY 的字节同步：可见文本追加、退格删尾、`\r` 执行并解析 `cd` 更新 cwd、Ctrl+C 重置、箭头/编辑键使模型失效）
- 候选：命令位置匹配 PATH 可执行文件（`command_index()` OnceLock 懒扫描，只收有执行位的文件）；其余匹配 cwd 文件/目录（含 `~/` 展开与子路径），目录补 `/` 后缀
- 浮层（`terminal_view.rs::render_completion_popup`）：光标行上方 Area 列表（命令 accent2/目录 accent/文件次要色），Tab 确认（按字符退格替换 word 后写 PTY）、↑/↓ 选择、Esc 关闭；输入变化即时重算（最多 8 条）。**定位用 `completion_popup_pos` 纯函数：`cursor_pos` 为光标行底部，上方空间充足时浮层底边 = 光标行顶上方 4px（完全不遮输入行），不足时顶边 = 光标行底下方 4px 紧贴输入行下方（回归测试：曾向下偏移一整行盖住输入行底部）**
- 键盘拦截在 `handle_input` 的 Key 分支（菜单打开时 Tab/↑/↓/Esc 不转发 shell）；输入动作用 `InputAction` 枚举在 ui.input 闭包内收集、闭包外统一应用（闭包内 `session` 不可变借用，无法直接改 self）
- **egui 0.36 焦点坑：Text/Key 事件帧后终端焦点可能被清除（`Memory::end_pass` dead-man-switch，kittest 必现）**——`TerminalView` 维护 `had_focus`，曾聚焦且当前 `focused().is_none()` 时每帧 `request_focus` 恢复（对话框输入框后渲染覆盖，不冲突）

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
- 终端背景在 TerminalView 里绘制 `BG_TERMINAL`；SFTP 面板与设置弹窗内主机列表行 hover/选中用 `ACCENT_SOFT` 圆角底
- **动效**（`anim.rs`，无第三方依赖）：`smooth/smooth_bool` 指数平滑（状态存 ctx 临时数据，收敛中自动 request_repaint）、`paint_shimmer_line` 扫光 Mesh、`paint_rounded_gradient` 圆角渐变三角扇、`paint_glow` 辉光；工具栏底部扫光仅在 hover/更新活跃时持续重绘（省电）
- **品牌 logo**（`draw_logo_mark`）：紫金渐变圆底 + **白色粗体 "K" 字**（居中，与应用图标同构图）；hover 时 accent2 辉光；无动画、无持续重绘（省电）。**kittest 注意：持续重绘会让 `Harness::run()` 超 max_steps panic——测试统一用 `harness.run_steps(6)` 显式步进**
- **hover 光标**：`apply_theme` 设 `visuals.interact_cursor = Some(CursorIcon::PointingHand)`，所有标准控件（Button/SelectableLabel/ComboBox）hover 自动变小手（TextEdit 内部显式设 I-beam 覆盖）；自定义 `ui.interact` 区（主机行/删除/logo/toast）需手动 `.on_hover_cursor`
- 终端内容内边距 `PADDING=10`（`terminal_view.rs`）：背景铺满 `ui.max_rect()`，文本/光标在内边距内绘制。**注意 egui 陷阱：`ui.min_rect()` 是"已用内容"包围盒，无子项时为 0x0**——背景/点击区域必须用 `ui.max_rect()`（布局分配区域），否则背景画不出来、点击无法重新聚焦
- **display_iter 行号是网格坐标**（alacritty `grid/mod.rs::display_iter`）：viewport 顶行为 0，向上滚动后可见的 scrollback 行是**负行号**（`Line(-(display_offset)-1)`）。渲染循环必须用相对视口顶行的显示行号（换行时计数），**严禁 `point.line.0 as usize`**——负行号 cast 成 usize::MAX 级巨值，`rows_cache.resize` 直接 `capacity overflow` 闪退（offset=1 时则是 index 越界）；光标定位同样需换算：显示行 = 光标网格行 + `display_offset`。回归测试：`滚动scrollback后渲染不崩溃`
- 主机条目（设置弹窗 → 主机管理卡片内）：单击选中、双击连接；样式：**accent 圆形头像（主机名首字符）+ 名称（超长截断不换行）+ 🗑 删除图标**（行右缘，hover 红底）；**行点击区横向扩展到面板可用宽度**（`row_rect`），短名称主机也能整行点击
- **egui 0.36 交互坑：`Response::interact()`（scope_builder(...).response.interact(...)）的点击无法命中**（响应链问题，kittest 实测 clicked/hovered 恒 false）——必须用 `ui.interact(rect, id, sense)` 显式注册交互区，删除按钮等行内控件最后注册以覆盖行点击区
- 终端调色板（Catppuccin Mocha 16 色 + xterm 256 色表）在渲染层解析（`theme.rs::TERM_PALETTE_16`/`xterm256`），优先级：Spec > OSC 覆盖（term.colors）> 内置调色板；`Term.colors` 默认全 None，不设调色板则全部渲染为白色
- 选中主机条目左侧 2px accent 竖条；标签页底部有选中指示条（宽度动画）
- **标签页（tab）无双边框**：tab 内容用无边框透明 `Button`（`selectable_label` 选中自带边框，与手绘高亮叠加会成"双重框"），高亮只有一层手绘 `accent_soft` 圆角底 + 底部指示条
- **三套深色主题**（`theme.rs::THEMES`）：深色（紫金）/ 深蓝 / 霓虹；每套含 UI token + 终端调色板（16 色 + fg/bg/cursor）；`current_theme()` 静态读取，**设置弹窗 → 外观卡片**切换（`set_theme`）——ComboBox 选当前主题名（**勿用 ◐ 等 unicode 符号作为图标，SF 字体缺字形渲染为方块**）
- 主题切换后需重新渲染终端（terminal_view 每帧读 `current_theme()`）
- 终端输入/回车功能验证正常；**"回车不执行"两大真根因**：①TERM=dumb（GUI 启动继承，见下条 env 注入）②zsh 启用应用键盘模式（APP_KEYPAD）后主键盘 Enter 曾被误编码成 `ESC O M`——已修复为永远发 `\r`（见 keys.rs）；此外中文输入法（IME）激活时回车被输入法消费（确认拼音候选）是所有终端应用共性，切英文输入法即可
- **"删除键插入空格"**：有两类独立根因——①**微信输入法 wetype**：退格/删除键按下时输入法伴随发送"空格类" Text 事件（ASCII 空格/零宽）→ 已修复：`suppress_next_text` 跨帧标记 + 退格后一帧内的空白类 Text 丢弃（只影响空白字符，不误伤正常输入）；回归测试 `退格伴随空格文本不插入` ②**TERM=dumb**：zle 判定非交互终端，删除回显走「原地空格覆盖」（只发空格缺 `\b`）→ 已修复：env 注入 TERM（见下条）
- **布局重构（v0.5/v0.6）**：移除了左侧固定主机栏（`Panel::left("hosts")`）与顶部工具栏（`Panel::top("toolbar")`），所有原工具栏入口（主题/更新/品牌 logo）搬入**设置弹窗**（不再用 tab）；标签栏最右侧 `settings_gear_button` **22×22 纯图标齿轮**（无文字 label，自绘渐变圆底 + ⚙ 符号，hover accent2 辉光，包装为 `egui::Button` 以便被 kittest `Role::Button` 找到），click → `show_settings = true`；`⌘,` toggle 设置弹窗（macOS 标准"应用偏好设置"快捷键，取代原 `⌘B` 折叠侧栏）。**`KunApp::new` 启动仅创建本地终端 tab**（`Vec<Box<TerminalTab>>`），`active_tab = 0`（不打扰用户进入终端）；设置弹窗默认关闭。标签栏最右侧**只有齿轮一个图标按钮**（复制 tab「⎘」按钮已移除，相关 `duplicate_active_tab`/`new_local_tab_with_label` 一并删除）
- **窗口整体圆角**（`native.rs`，仅 macOS）：`with_decorations(false)` 无边框窗口是方角，通过 AppKit 给 contentView 的 backing layer 设 `cornerRadius=12` + `masksToBounds`，并 `setOpaque(false)` + `backgroundColor=clearColor` 让圆角外侧透明露出桌面（像素验证：四角 RGBA=(0,0,0,0) 全透明）。入口在 `main.rs` 的 app_creator 闭包 `native::apply_rounded_window(cc)`——用 `cc.window_handle()` 拿 `RawWindowHandle::AppKit` → `ns_view` 指针转 `&NSView`（同 eframe 内部写法，指针窗口生命周期内有效）；kittest 测试环境无真实 AppKit 句柄（`window_handle()` 失败/非 AppKit 变体）时静默返回，不 panic。**依赖**：kun-app 直接声明 `objc2-app-kit`（补 `NSColor`/`NSWindow`/`NSView` + `objc2-quartz-core` feature 使 CALayer 可用，features 全局合并不影响 eframe/winit）+ `objc2` + `raw-window-handle`（版本与 eframe 一致 0.6.2）；NSWindow 为 `MainThreadOnly`，必须在 eframe 主线程创建期调用
- **无边框窗口拖拽**（`tab_bar`）：无系统标题栏，`tab_bar` 开头对**整行**（`ui.max_rect()`，Panel 分配区域非 0x0）注册底层 `Sense::drag()` 背景（id `tab_bar_drag`），`drag_started()` 时发 `ViewportCommand::StartDrag`（egui-winit 转 winit `drag_window()`，需窗口有焦点）。**关键交互顺序**：egui 命中规则是**后注册 widget 在顶层**（`WidgetRects::get_layer` back-to-front，`hit_test` 中 `drag_idx < click_idx` 判定 click 在 drag 之上）——拖拽背景**先注册**（底层），tabs/红绿灯/齿轮**后注册**（顶层），故控件点击/拖拽优先，仅标签栏空白处按下拖动才移窗；`Sense::drag` 的大背景 + 附近小控件会被 hit_test 的 `contains_rect` 帮助逻辑优先给小控件。**实测验证**：CGEvent 模拟标签栏空白处拖拽 → 窗口位置精确移动对应距离；点击齿轮 → 设置弹窗正常打开（kittest 齿轮测试亦通过）；`ui.interact` 不占布局空间不移动 cursor，horizontal 布局不受影响
- **设置弹窗**（`fn settings_panel(ctx)`，`egui::Window` 居中，标题"设置"）：卡片化三组（`Self::settings_card` helper 渲染 `bg_elevated` 底 + 边框 + 圆角 + 紧凑内边距）：**主机管理**（复用 `host_sidebar`）/ **外观**（主题 ComboBox）/ **关于**（版本号 + 检查更新按钮）。`ScrollArea` 垂直滚动，max_height 420 防止超出视口。
- **应用内更新**（`kun-core::updater` + `app.rs` 状态机）：版本检查走 `releases.atom`（不受 API 限流），资产直链按 `kun-{版本}-macos-{arm64|x64}.dmg` 构造并流式下载（进度回调）；安装由后台 shell 脚本完成——`hdiutil attach` 挂载 → `pgrep -x kun-app` 等待主进程退出 → `ditto` 替换 `/Applications/kun.app`（失败回退 `~/Applications`）→ `open` 重启；`installed` 状态延时 0.9s 后 `ViewportCommand::Close`；入口在设置弹窗 → 关于卡片
- 主机行双击连接为**自实现检测**（`last_row_click`：0.3s 内同行的第二次点击）：egui 多击计数会把无关点击（如工具栏其他按钮）计入序列导致 count=3 而 `double_clicked`（count==2）失效
- **多标签页**（warp 风格）：`KunApp` 持 `tabs: Vec<Box<TerminalTab>>`（`pub type Tab = Box<TerminalTab>`），SFTP 按 TerminalTab 挂载（远程 tab 独享）；**`Box` 包裹 TerminalTab**——避免 `Vec` 各槽位按最大元素对齐造成内存浪费（原 enum 形式触发的 `large_enum_variant` 警告在去 enum 化后自动消失）；标签栏在顶栏（`Panel::top("tabs")`，合并了原工具栏区域），当前标签 accent-soft 高亮，支持点击切换/×关闭/＋新建；最右侧为设置齿轮；快捷键 ⌘T 新建本地、⌘W 关闭当前、⌘1-9 切换、⌘N 新建连接、⌘, 切换设置弹窗、⌥1-3 主题
- 本地终端默认工作目录为 **home**（`local_session_options`：`working_directory = $HOME`）——Finder/Dock 启动时进程 cwd 为 `/`，不指定会导致终端落在根目录；远程会话不受影响（由 sshd 决定）
- **终端 `ls` 颜色与 TERM**：渲染层已支持 ANSI 色，但 macOS `ls` 默认无颜色输出（无 `CLICOLOR`）→ `local_session_options` 注入 `CLICOLOR=1` + 深色优化 `LSCOLORS=Gxfxcxdxbxegedabagacad`（目录亮青/链接紫红/可执行红/socket 绿/管道黄）+ **`TERM=xterm-256color`**（必须注入：GUI/Finder/Dock 启动继承 `TERM=dumb`，alacritty 的 `setup_env()` 只在 alacritty 主应用入口调用、kun 未调用，`TERM=dumb` 会致 zsh zle 删除回显异常/回车异常——同 Miro Code 根因；**勿注入 locale**，会引发回车不执行）；经 `SessionOptions.env`（alacritty 为覆盖语义，`builder.env` 追加继承）传给 PTY；与 Terminal.app/iTerm2 行为一致，不篡改 shell
- **键盘编码**（`kun-core/src/terminal/keys.rs`）：主键盘 **Enter 永远发 `\r`**（应用键盘模式 APP_KEYPAD 只影响数字小键盘 Enter = `ESC O M`，主键盘 Enter 不受影响）——曾误按 APP_KEYPAD 把主键盘 Enter 编码成 `\x1bOM`，而 zsh 在 TERM=xterm-256color 下 zle 自动开启应用键盘模式 → 回车不执行（命令停在命令行）；回归测试 `enter应用键盘模式` 断言 Enter 在 APP_KEYPAD 下仍发 `\r`。方向键按 APP_CURSOR 用 SS3（`ESC O A` 等），Backspace=`\x7f`，Delete=`\x1b[3~`，Shift+Tab=`\x1b[Z`
- 终端内容颜色：prompt 彩色来自 shell 主题（如 oh-my-zsh robbyrussell 的绿/青/蓝/黄）
- 主题背景跟随当前深色主题；`apply_theme` 同步系统深色主题（`ctx.set_theme`）
- 主题快捷键：⌥1-⌥3 快速切换三套主题
- **新建连接表单**（`connect_dialog`）：默认用户名 `root`、端口 `22`（`ConnectForm` 手写 `Default`，可修改）；输入框统一走 `form_input` helper——圆角深色底（`bg_elevated`）+ 焦点 accent 边框 + `vertical_align(Center)`（**TextEdit 默认 `Align2::LEFT_TOP` 文字偏上，单行输入框必须居中**）
- **状态栏快捷键提示**（右下角）：每个提示为圆角深色块（`bg_elevated` 底 + 边框），按键 accent 色 + 动作次要色；快捷键 ⌥1-3（勿写 ⌥1-4）
- **SFTP 面板**（tabby 形式）：`Panel::right("sftp_panel")` **必须是顶层面板（先于 CentralPanel 注册）**——嵌套在 CentralPanel 内时面板状态会被顶层布局污染，面板错位覆盖终端（表现为"面板打不开"）；**默认收起只显示终端，终端右上角悬浮 `sftp_floating_button`（app.rs 顶层函数，"SFTP" 圆角按钮，点击切换 `TerminalTab.sftp_open`）**；展开用官方 `show_collapsible`（滑动动画，收起后右缘保留细拖拽把手可拖开）；尺寸 `default_size(340)/min_size(260)/max_size(窗口 45%)`——曾 resizable 无上限拖到 ~70% 窗口宽把终端压成窄条，max_size 每帧按 `viewport_rect().width()*0.45` 钳制；`poll_sftp` 处理 `Closed` 事件（连接中断时不能停在"连接中…"），失败写入 `sftp_error` 字段由状态栏**持久显示**（toast 一闪而过易忽略），下次 `start_connect` 清空；**文件列表列布局**（`sftp_view.rs`）：名称列左对齐、大小/时间列右对齐（均 `.halign(egui::Align::LEFT/RIGHT)` 显式声明，避免依赖默认对齐），`new_child` 子 Ui 需 `spacing_mut().item_spacing.x = 0.0` 归零自动间距（否则子项间默认 8px 叠加导致列位错乱），列间距用 add_space 精确控制，名称 `Label::truncate()` 截断；**`cell_width` 必须用 `'0'` 字符宽（数字等宽字体的真实字宽）而非空格宽**——空格 ≈ 0.25em、数字 ≈ 0.6em，用空格宽估算列宽会导致时间列 "2026-08-14" 被截到 "2026-01"（曾用空格宽 + 11 字符列宽只能容 7 字符数字）；大小/时间列宽 12 字符等宽（足够显示 10 字符日期 + 缓冲），名称列 = 总宽 - 12×2 - 12；回归测试 `文件列表列对齐`、`sftp面板默认收起悬浮按钮切换`；kittest 截图布局断言：上传按钮 left > 400（面板在右半侧）、齿轮按钮 right > 400（标签栏最右侧）

## 关键约定

- 集成测试依赖本地测试 sshd：`/usr/sbin/sshd -f /tmp/kun-test-sshd/sshd_config`（端口 2222、公钥认证、含 sftp subsystem）
- 配置文件：`~/.config/kun/hosts.toml`（toml 中 enum 用内部标记：`[hosts.auth.Key]`）
- 应用图标：`assets/icon.png`（**渐变紫圆角底 + 白色粗体 "K" 字（居中），四周留 10% 透明边距**，make-icon.swift 绘制，与应用内动态 logo 同构图）→ `load_icon()` 解码为 IconData → `ViewportBuilder::with_icon`——**eframe 在 macOS 上通过 NSApp 运行时设置 Dock 图标**，无 .app bundle 的 debug 构建也能生效；`.app` 安装版的 Dock 图标由 package-macos.sh 的 kun.icns 提供（同一设计）。**图标必须留透明边距：占满画布的无边距图标会被 macOS Dock 放大显示（比邻图标大一圈）**。**make-icon.swift 必须用 `NSBitmapImageRep` 位图上下文渲染（`NSImage.lockFocus` 在 Retina 屏按 2x 渲染导致输出尺寸翻倍）**
- 字体：Monospace 族 = SF Mono（主）+ **Menlo（符号 fallback）** + STHeiti（CJK）+ egui 默认；Proportional 族 = SF 主 + STHeiti。**SF Mono 缺 `➜`(U+279C)/`❯`(U+276F)/`⚡` 等常用 zsh 提示符符号**，缺字形会被 egui 渲染为 `?` 替换符；Menlo 同为等宽且完整覆盖（宽度一致不漂移），必须排在 CJK fallback 之前。**禁止加载 Apple Color Emoji.ttc**（192MB 彩色位图字体，ab_glyph 无法解析 → egui panic）
- 提示符 `?➜` 中的 `?` 是 oh-my-zsh robbyrussell 主题 `%1{➜%}` 语法在 zsh 5.9 的真实输出（script 捕获字节流验证：`0x3F E2 9E 9C`），Terminal.app 同样显示，**非 kun 渲染问题，勿尝试"修复"**
- egui 0.36 API 注意：`App::ui` 替代 `update`、`Panel::top/left` 替代 `TopBottomPanel`、`Fonts` 需要 `fonts_mut`、`Event::Key` 无 `text` 字段（Text 独立事件）
- 终端视图使用固定 `focus_id` 管理键盘焦点；对话框打开时自动聚焦首个输入框

## 验证

```bash
cargo test --workspace         # 63 个测试（单元 + ssh 集成 + sftp 集成 + UI 渲染 + 字体链 + 标签页 + 双击交互 + 表单默认值 + 补全模型/候选 + 补全浮层交互 + 设置弹窗 + scrollback + 完整应用回车）
cargo clippy --workspace --all-targets   # 零警告
cargo fmt --all
```

窗口圆角为运行时原生效果（kittest 无真实窗口句柄，无法单测断言）；验证方式：`cargo run -p kun-app` 后 `screencapture -l <CGWindowID>` 截窗，四角像素应全透明（RGBA alpha=0）。
