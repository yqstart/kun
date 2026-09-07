//! 终端视图：cell 渲染、键盘输入转发、滚动。
//!
//! 渲染为**行级增量**：每帧用 `Term::damage()` 拿到终端损坏行集合（行号即显示行号），
//! 只对损坏/新出现/滚入的行重建文本段与 Galley（已布局文本），其余行直接复用缓存
//! Galley 绘制（零扫描、零 layout）。内容未变的帧（PTY 空转、光标闪烁、无输入）仅
//! 绘制已有 Galley。

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::cell::{Flags, LineLength};
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::TermDamage;
use alacritty_terminal::vte::ansi::{Color as AColor, CursorShape, NamedColor, Rgb};
use egui::text::LayoutJob;
use egui::{Color32, FontId, Rect, Stroke, TextFormat, Ui, Vec2};
use mino_core::terminal::keys::{self, Key, Mods, MouseWheelDirection};
use mino_core::terminal::{Session, SessionEvent, TermMode};

/// 行缓存：内容 hash 未变时复用已布局文本（Galley），避免每帧重建。
/// `galley` 与 pixels_per_point 绑定，窗口缩放后需全量失效（见 `show`）。
#[derive(Clone)]
struct RowCache {
    /// 内容指纹（fg+bg+样式+字符；不含光标效果，光标移动不触发重建）。
    hash: u64,
    /// 已布局文本（绘制直接使用，无需 layout_job）。
    galley: std::sync::Arc<egui::Galley>,
    /// 背景段（合并相邻相同背景色，含起止列）。
    backgrounds: Vec<BgRect>,
}

/// 文本段（合并相邻相同前景样式的 cell）。
struct Segment {
    text: String,
    fg: Color32,
    bold: bool,
    italic: bool,
    underline: bool,
    strikeout: bool,
}

/// 背景矩形（合并相邻相同背景色的 cell，含起止列）。
#[derive(Clone)]
struct BgRect {
    start: usize,
    end: usize,
    color: Color32,
}

/// 单行渲染数据（锁内构建，锁外 layout）。
struct LineData {
    hash: u64,
    segments: Vec<Segment>,
    backgrounds: Vec<BgRect>,
}

/// 终端选区中的一个 cell 坐标。
///
/// 行使用 alacritty 的网格坐标而不是当前视口行号，因此用户滚动 scrollback
/// 时，选区仍然绑定在原来的输出内容上。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SelectionPoint {
    grid_line: i32,
    col: usize,
}

impl Ord for SelectionPoint {
    fn cmp(&self, other: &Self) -> Ordering {
        self.grid_line
            .cmp(&other.grid_line)
            .then_with(|| self.col.cmp(&other.col))
    }
}

impl PartialOrd for SelectionPoint {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// 终端鼠标选区。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TerminalSelection {
    anchor: SelectionPoint,
    focus: SelectionPoint,
}

impl TerminalSelection {
    /// 返回当前行应绘制的选区列范围（右端为 exclusive）。
    fn columns_for_line(self, grid_line: i32, cols: usize) -> Option<(usize, usize)> {
        let (start, end) = if self.anchor <= self.focus {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        };
        if grid_line < start.grid_line || grid_line > end.grid_line {
            return None;
        }
        let (from, to) = if start.grid_line == end.grid_line {
            (start.col, end.col.saturating_add(1))
        } else if grid_line == start.grid_line {
            (start.col, cols)
        } else if grid_line == end.grid_line {
            (0, end.col.saturating_add(1))
        } else {
            (0, cols)
        };
        let from = from.min(cols);
        let to = to.min(cols);
        (from < to).then_some((from, to))
    }
}

/// 终端内容内边距（文本与面板边缘的间距，参照 Terminal.app 观感）。
const PADDING: f32 = 10.0;

/// SSH 的 `window_change` 是异步发送的。连接刚建立或终端刚改变布局时，
/// 远端 shell 可能在收到第一次尺寸通知前就开始输出动态内容（例如 npm 的
/// 进度条）。在尺寸稳定后的几帧内重复发送，避免远端仍按旧宽度换行。
const REMOTE_RESIZE_SYNC_FRAMES: u8 = 8;

/// 终端视图。
pub struct TerminalView {
    session: Session,
    /// 行缓存：网格行号 → 渲染数据（Galley + 背景段 + hash）。
    /// 按网格行号索引：滚动后同一网格行直接命中，无需重建。
    rows_cache: HashMap<i32, RowCache>,
    font_size: f32,
    cell_width: f32,
    cell_height: f32,
    cols: u16,
    rows: u16,
    /// 上次渲染时的 pixels_per_point（Galley 与其绑定，变化需全量失效）。
    last_ppp: f32,
    /// 上次渲染时的主题修订号（主题切换后 Galley/背景均需失效）。
    last_theme_revision: u64,
    focus_id: egui::Id,
    initialized: bool,
    last_mode: TermMode,
    /// 退格/删除键按下后，下一帧的"空白类" Text 事件应丢弃。
    /// （某些输入法（如微信输入法）退格时会伴随发送空格类文本，
    /// 写入终端表现为"删除键插入空格"；正常字符不受影响）
    suppress_blank_frames: u8,
    /// 当前工作目录跟踪器（供 SFTP 面板快捷定位使用）。
    workdir: crate::workdir::WorkdirTracker,
    /// 执行 `pwd` 前的终端可见行，用于从后续屏幕变化中提取实际目录。
    pwd_output_rows: Option<Vec<String>>,
    /// 远程会话的初始目录（由 SFTP realpath(".") 提供）。
    remote_home: Option<std::path::PathBuf>,
    /// 上一帧终端是否持有焦点（焦点自动恢复用）。
    had_focus: bool,
    /// 分段耗时打点（性能 HUD 读数；默认不共享，仅本视图内部使用）。
    last_build_ms: f32,
    last_layout_ms: f32,
    last_paint_ms: f32,
    /// 会话标题缓存（`SessionEvent::Title` 时更新，避免每帧 Mutex + String clone）。
    cached_title: String,
    /// 远程 PTY 尺寸同步重试次数（`window_change` 由 SSH 后台异步发送）。
    remote_resize_sync_frames: u8,
    /// 当前终端选区（⌘C / Ctrl+Shift+C 复制）。
    selection: Option<TerminalSelection>,
    /// 是否正在进行鼠标拖选。
    selecting: bool,
    /// 复制后的短暂反馈 chip 到期时间。
    copy_flash_until: Option<f64>,
}

impl TerminalView {
    /// 创建终端视图并启动本地会话。
    pub fn new(session: Session) -> Self {
        let is_remote = session.is_remote();
        // 本地会话初始工作目录：会话启动目录（HOME）。
        let cwd = if is_remote {
            std::path::PathBuf::from("/")
        } else {
            std::env::var("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_default()
        };
        // 会话标题初值（一次 Mutex；后续由 Title 事件增量更新）。
        let cached_title = session.title();
        Self {
            session,
            rows_cache: HashMap::new(),
            font_size: 13.0,
            cell_width: 8.0,
            cell_height: 16.0,
            // 真实尺寸要等到首帧布局后才能从 egui 区域计算出来；不要把
            // SSH 建连时的 80x24 初始值误当成已经同步的窗口尺寸。
            cols: 0,
            rows: 0,
            last_ppp: 0.0,
            last_theme_revision: crate::theme::theme_revision(),
            focus_id: egui::Id::new("terminal_view"),
            initialized: false,
            last_mode: TermMode::NONE,
            suppress_blank_frames: 0,
            workdir: crate::workdir::WorkdirTracker::new(cwd),
            pwd_output_rows: None,
            remote_home: None,
            had_focus: false,
            last_build_ms: 0.0,
            last_layout_ms: 0.0,
            last_paint_ms: 0.0,
            cached_title,
            remote_resize_sync_frames: if is_remote {
                REMOTE_RESIZE_SYNC_FRAMES
            } else {
                0
            },
            selection: None,
            selecting: false,
            copy_flash_until: None,
        }
    }

    /// 本帧终端渲染分段耗时（性能 HUD 读取；未渲染时均为 0）。
    pub fn last_timing(&self) -> (f32, f32, f32) {
        (self.last_build_ms, self.last_layout_ms, self.last_paint_ms)
    }

    /// 会话引用（供状态栏等读取标题）。
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// 会话标题（缓存，`Title` 事件时更新；避免每帧 Mutex + String clone）。
    pub fn session_title(&self) -> &str {
        &self.cached_title
    }

    /// 当前终端已知的工作目录（供 SFTP 快捷定位使用）。
    pub fn current_directory(&self) -> Option<String> {
        if self.session.is_remote() && self.remote_home.is_none() {
            return None;
        }
        Some(self.workdir.cwd().to_string_lossy().into_owned())
    }

    /// SFTP 定位前调用：当前输入行为空时向 shell 注入一条 `pwd` 并等待输出。
    ///
    /// 返回 true 表示已注入 `pwd`（调用方应等待若干帧后的定位结果，不要
    /// 立即用旧的推测目录导航）；false 表示此刻不适合自动探测（已有未
    /// 完成的探测/手输 pwd、全屏应用占用终端、当前有未执行的输入），
    /// 调用方应直接回退到已知目录。
    pub fn request_fresh_pwd(&mut self) -> bool {
        if self.workdir.awaiting_pwd_output() || !self.workdir.begin_auto_pwd() {
            return false;
        }
        if !self.workdir_input_is_idle() || self.is_fullscreen_app() {
            self.workdir.cancel_auto_pwd();
            return false;
        }
        // 与用户在空提示符下手输 `pwd\r` 的字节流一致；终端随后走已有的
        // `awaiting_*_pwd_output` 输出校正管线把目录更新到真实值。
        self.session.write(b"pwd\n");
        self.workdir.reset();
        self.workdir.begin_auto_pwd();
        true
    }

    /// 自动 `pwd` 探测是否已拿到终端输出（SFTP 定位轮询用）。
    pub fn auto_pwd_ready(&self) -> bool {
        !self.workdir.awaiting_auto_pwd_output()
    }

    /// 取消未完成的自动 `pwd` 探测（定位超时 / 面板切到无终端上下文时）。
    pub fn cancel_fresh_pwd(&mut self) {
        self.workdir.cancel_auto_pwd();
    }

    /// 测试用：向工作目录跟踪器注入可见文本（模拟用户正在编辑命令行）。
    #[cfg(test)]
    pub fn push_workdir_text_for_test(&mut self, text: &str) {
        self.workdir.push_text(text);
    }

    /// 当前输入行是否为空（没有任何等待执行的字符）。
    ///
    /// 定位注入 `pwd` 必须在空提示符下进行，否则会污染用户正在编辑的
    /// 命令行。跟踪器只记录“可观察到的输入”，Tab/粘贴/方向键等已让它
    /// 失效——失效本身不代表输入行为空，这里只能做保守判断：
    /// 跟踪器有效且文本为空时才认为空闲。
    fn workdir_input_is_idle(&self) -> bool {
        self.workdir.input_is_idle()
    }

    /// 终端是否被全屏应用占用（vim/less/top 等）。
    ///
    /// 此时注入 `pwd` 会变成应用的按键而不是 shell 命令；SFTP 定位应
    /// 直接回退到已知目录，等用户退出全屏应用后再定位。
    fn is_fullscreen_app(&self) -> bool {
        let term_arc = self.session.term();
        let guard = term_arc.lock();
        guard.mode().contains(TermMode::ALT_SCREEN)
    }

    /// 设置远程会话的初始工作目录，不覆盖已经由终端输入跟踪到的目录。
    pub fn set_remote_current_directory(&mut self, path: &str) {
        if path.is_empty() {
            return;
        }
        let cwd = std::path::PathBuf::from(path);
        self.remote_home = Some(cwd.clone());
        self.workdir.set_cwd_if_unmodified(cwd);
    }

    /// 轮询后台事件但不渲染终端。
    ///
    /// 应用层会对所有存活标签调用它，避免非活动标签长期不消费事件；
    /// 当前活动标签随后进入 `show` 时会再次轮询但不会重复处理。
    pub fn drain_background_events(&mut self) {
        for event in self.session.drain_events() {
            match event {
                SessionEvent::PtyWrite(text) => self.session.write(text.as_bytes()),
                SessionEvent::Title(title) => self.cached_title = title,
                _ => {}
            }
        }
    }

    /// 每帧渲染入口。
    pub fn show(&mut self, ui: &mut Ui) {
        self.show_with_input(ui, true);
    }

    /// 渲染终端，并按需禁用键盘、鼠标与滚轮输入。
    ///
    /// 前台弹窗打开时终端仍需持续渲染后台输出，但不能直接读取
    /// egui 全局输入事件，否则位于终端坐标范围内的弹窗滚轮会穿透。
    pub fn show_with_input(&mut self, ui: &mut Ui, input_enabled: bool) {
        let ctx = ui.ctx().clone();
        let term_arc = self.session.term();

        // 主题会改变默认前景、基本色和终端背景；旧 Galley 与背景段不能跨主题复用。
        let theme_revision = crate::theme::theme_revision();
        if self.last_theme_revision != theme_revision {
            self.rows_cache.clear();
            self.last_theme_revision = theme_revision;
        }

        // 终端区域背景（当前主题的终端色）。
        // 注意：用 max_rect（布局分配区域）而非 min_rect（已用内容包围盒，
        // 无子项时为 0x0，会导致背景画不出来）。
        let theme = crate::theme::current_theme();
        let term_bg = theme.term_bg;
        let outer = ui.max_rect();
        ui.painter().rect_filled(
            outer,
            0.0,
            Color32::from_rgb(term_bg.r, term_bg.g, term_bg.b),
        );
        // 低对比网格：提供科技感的空间层次，但不干扰终端文本。
        let grid_step = 32.0;
        let grid_color = crate::theme::tokens::GRID_LINE;
        let first_x = outer.left() - outer.left().rem_euclid(grid_step);
        let first_y = outer.top() - outer.top().rem_euclid(grid_step);
        for x in (0..=((outer.width() / grid_step).ceil() as usize + 1))
            .map(|i| first_x + i as f32 * grid_step)
        {
            ui.painter().line_segment(
                [egui::pos2(x, outer.top()), egui::pos2(x, outer.bottom())],
                egui::Stroke::new(1.0, grid_color),
            );
        }
        for y in (0..=((outer.height() / grid_step).ceil() as usize + 1))
            .map(|i| first_y + i as f32 * grid_step)
        {
            ui.painter().line_segment(
                [egui::pos2(outer.left(), y), egui::pos2(outer.right(), y)],
                egui::Stroke::new(1.0, grid_color),
            );
        }
        // 终端内容区域：背景铺满面板，文本/光标在内边距内绘制。
        let inner = outer.shrink(PADDING);

        // ==================== 事件泵 ====================
        // 诊断：PTY 读取线程退出会导致输入写入失效。
        if self.session.pty_thread_finished() {
            log::warn!("PTY 读取线程已退出！输入将无法写入终端。");
        }
        // 注意：Wakeup 不再在此处二次 request_repaint——mino-core 的
        // `Listener::send_event` 已在事件到达时直接调过 on_event
        // （app.rs 的 `ctx.request_repaint()`），此处仅处理 PtyWrite 回写
        // 与标题缓存更新。
        self.drain_background_events();

        // ==================== 工作目录校正 ====================
        // 目录跟踪通常只需处理键盘输入；只有执行 pwd、等待其输出时才读取
        // 可见网格，避免为了一个低频兜底路径让每个空闲帧都扫描整个终端。
        let has_enter = input_enabled
            && ui.input(|input| {
                input.events.iter().any(|event| {
                    matches!(
                        event,
                        egui::Event::Key {
                            key: egui::Key::Enter,
                            pressed: true,
                            ..
                        }
                    )
                })
            });
        let should_observe_output = self.workdir.awaiting_any_pwd_output() || has_enter;
        let output_rows = should_observe_output.then(|| visible_terminal_rows(&self.session));
        if self.workdir.awaiting_any_pwd_output() {
            if let Some(current) = output_rows.as_deref() {
                if let Some(previous) = self.pwd_output_rows.take() {
                    let corrected = if self.session.is_remote() {
                        self.workdir.observe_remote_output(&previous, current)
                    } else {
                        self.workdir.observe_local_output(&previous, current)
                    };
                    if !corrected && self.workdir.awaiting_any_pwd_output() {
                        // 命令回显和命令输出可能跨多个帧到达；每次继续
                        // 以前一帧作为基线，避免漏掉后续被改写的行。
                        self.pwd_output_rows = Some(current.to_vec());
                    }
                } else {
                    self.pwd_output_rows = Some(current.to_vec());
                }
            }
        }

        // ==================== 尺寸计算与 resize ====================
        // cell 尺寸只依赖字体（启动时加载），缓存到字段避免每帧 fonts_mut。
        let ppp = ui.ctx().pixels_per_point();
        if self.cell_width == 0.0 || ppp != self.last_ppp {
            self.last_ppp = ppp;
            let (cell_width, cell_height) = ui.fonts_mut(|f| {
                let font = FontId::monospace(self.font_size);
                // 空格在部分等宽字体中比数字窄；终端列宽按真实等宽
                // 字符测量，否则本地列数会偏大，远端动态输出会错位换行。
                (f.glyph_width(&font, '0'), f.row_height(&font))
            });
            self.cell_width = cell_width;
            self.cell_height = cell_height;
            // Galley 与 pixels_per_point 绑定：缩放变化后旧布局失效，全量重建。
            self.rows_cache.clear();
        }
        let cell_width = self.cell_width;
        let cell_height = self.cell_height;

        let avail = inner.size();
        let cols = ((avail.x / cell_width).floor() as usize).max(2);
        let rows = ((avail.y / cell_height).floor() as usize).max(1);
        let size_changed = cols as u16 != self.cols || rows as u16 != self.rows;
        if size_changed {
            self.cols = cols as u16;
            self.rows = rows as u16;
            // 通知 PTY 并同步终端状态机网格（Session::resize 内部完成锁内 resize）。
            self.session.resize(self.cols, self.rows);
            if self.session.is_remote() {
                // 布局变化后重新开始短暂重试窗口，确保 SSH 的异步
                // window_change 在远端下一次动态渲染前到达。
                self.remote_resize_sync_frames = REMOTE_RESIZE_SYNC_FRAMES;
            }
            self.rows_cache.clear();
            // 选区的 grid_line 是建立时的快照，resize 重排网格后可能悬空
            // （复制时越界索引在 release 下会 panic），尺寸变化即放弃选区。
            self.selection = None;
        }
        if !size_changed && self.remote_resize_sync_frames > 0 {
            // 连接初始布局可能与 SSH request_pty 的 80x24 不同；即使本帧
            // 本地尺寸未变，也要把当前尺寸再送几次给远端，覆盖建连/输出
            // 并发时第一次 window_change 被延后的情况。
            self.session.resize(self.cols, self.rows);
            self.remote_resize_sync_frames -= 1;
        }

        // ==================== 构建渲染数据（锁内，行级增量） ====================
        // 只处理损坏行（`Term::damage`，行号 = 显示行号）与尚未缓存的行：
        // 内容未变的帧零遍历、零 layout；滚动只重建滚入的新行。
        let mut lines_data: Vec<(i32, LineData)> = Vec::new();
        let mut cursor_rect: Option<Rect> = None;
        let mut cursor_color: Option<Color32> = None;
        let cursor_shape: CursorShape;
        let display_offset: usize;
        let build_start = std::time::Instant::now();

        {
            let mut guard = term_arc.lock();
            // damage 收集（行号 = 网格行号 + display_offset = 显示行号），
            // 必须在同一持锁内 reset，否则下帧重复返回旧损伤。
            let (full_damage, damaged) = match guard.damage() {
                TermDamage::Full => (true, Vec::new()),
                TermDamage::Partial(iter) => (false, iter.map(|b| b.line).collect::<Vec<_>>()),
            };
            guard.reset_damage();

            let content = guard.renderable_content();
            let colors = content.colors;
            display_offset = content.display_offset;
            let default_fg =
                colors[NamedColor::Foreground].unwrap_or(crate::theme::current_theme().term_fg);
            // 背景色强制跟随主题（忽略 OSC 背景覆盖——zsh 主题常设置深色背景，
            // 会导致浅色主题下终端仍为深色）。
            let default_bg = crate::theme::current_theme().term_bg;
            self.last_mode = content.mode;
            let mode = content.mode;
            let cursor = content.cursor;
            let cursor_style = guard.cursor_style();
            cursor_shape = cursor_style.shape;

            // 光标可见性（含闪烁）。
            let time = ctx.input(|i| i.time);
            let blinking = cursor_style.blinking;
            let cursor_visible = mode.contains(TermMode::SHOW_CURSOR)
                && (!blinking || ((time * 2.0) as u64).is_multiple_of(2));
            if blinking {
                ctx.request_repaint_after(Duration::from_millis(500));
            }

            // 逐显示行：缓存命中（未损坏且 hash 一致）则跳过，否则锁内构建段。
            // 显示行 v ↔ 网格行 Line(v - display_offset)（display_iter 同语义：
            // 每个网格行一个显示行，wrap 续行独立成行）。
            let default_bg_egui = to_egui(default_bg);
            let grid = guard.grid();
            for v in 0..self.rows as usize {
                let grid_line = v as i32 - display_offset as i32;
                let cached = self.rows_cache.get(&grid_line);
                // 未损坏且已缓存：直接复用（不再遍历该行 cell）。
                if !full_damage && !damaged.contains(&v) && cached.is_some() {
                    continue;
                }
                // 锁内读取该网格行构建段与 hash。
                let data = build_line_data(
                    grid,
                    grid_line,
                    self.cols as usize,
                    colors,
                    default_fg,
                    default_bg,
                    default_bg_egui,
                );
                // 内容未变（如光标行被标记损伤但文本没变）：跳过 layout 复用旧 Galley。
                if let Some(c) = cached {
                    if c.hash == data.hash {
                        continue;
                    }
                }
                lines_data.push((grid_line, data));
            }

            // 光标矩形（Block 之外的光标形状）。
            if cursor_visible && cursor.shape != CursorShape::Hidden {
                let (line, col) = (cursor.point.line.0 as usize, cursor.point.column.0);
                // 滚动（查看 scrollback）时视口向上偏移 display_offset 行，
                // 光标网格行号需换算为显示行号；滚出视口则不绘制。
                let disp_line = line.saturating_add(display_offset);
                if disp_line < self.rows as usize && col < self.cols as usize {
                    let color = colors[NamedColor::Cursor]
                        .unwrap_or(crate::theme::current_theme().term_cursor);
                    cursor_color = Some(to_egui(color));
                    cursor_rect = Some(Rect::from_min_size(
                        inner.min
                            + Vec2::new(col as f32 * cell_width, disp_line as f32 * cell_height),
                        Vec2::new(cell_width, cell_height),
                    ));
                }
            }
        }
        self.last_build_ms = build_start.elapsed().as_secs_f32() * 1000.0;

        // ==================== 绘制（锁外） ====================
        let layout_start = std::time::Instant::now();
        // 先为新构建的行做文本布局并写缓存（命中行不进入此循环）。
        for (grid_line, data) in &lines_data {
            let job = build_job(&data.segments, self.font_size);
            let galley = ui.fonts_mut(|f| f.layout_job(job));
            self.rows_cache.insert(
                *grid_line,
                RowCache {
                    hash: data.hash,
                    galley,
                    backgrounds: data.backgrounds.clone(),
                },
            );
        }
        self.last_layout_ms = layout_start.elapsed().as_secs_f32() * 1000.0;

        let paint_start = std::time::Instant::now();
        let painter = ui.painter();
        let origin = inner.min;
        let selection_bg = Color32::from_rgba_unmultiplied(
            theme.accent.r(),
            theme.accent.g(),
            theme.accent.b(),
            92,
        );
        for v in 0..self.rows as usize {
            let grid_line = v as i32 - display_offset as i32;
            let Some(cache) = self.rows_cache.get(&grid_line) else {
                continue;
            };
            // 背景矩形（行内连续背景段）。
            for bg in &cache.backgrounds {
                let rect = Rect::from_min_size(
                    origin + Vec2::new(bg.start as f32 * cell_width, v as f32 * cell_height),
                    Vec2::new((bg.end - bg.start) as f32 * cell_width, cell_height),
                );
                painter.rect_filled(rect, 0.0, bg.color);
            }
            if let Some(selection) = self.selection {
                if let Some((start, end)) =
                    selection.columns_for_line(grid_line, self.cols as usize)
                {
                    let rect = Rect::from_min_size(
                        origin + Vec2::new(start as f32 * cell_width, v as f32 * cell_height),
                        Vec2::new((end - start) as f32 * cell_width, cell_height),
                    );
                    painter.rect_filled(rect, 2.0, selection_bg);
                }
            }
            // 文本（直接绘制缓存的 Galley，Arc clone 零成本；不再 layout_job）。
            let pos = origin + Vec2::new(0.0, v as f32 * cell_height);
            painter.galley(pos, cache.galley.clone(), Color32::WHITE);
        }

        // 光标形状绘制（shape 已在锁内读取，无需二次上锁）。
        if let (Some(rect), Some(color)) = (cursor_rect, cursor_color) {
            match cursor_shape {
                CursorShape::Block => {
                    painter.rect_filled(rect, 0.0, color);
                }
                CursorShape::Underline => {
                    painter.line_segment(
                        [
                            rect.left_bottom() + Vec2::new(0.0, -1.0),
                            rect.right_bottom() + Vec2::new(0.0, -1.0),
                        ],
                        Stroke::new(1.5, color),
                    );
                }
                CursorShape::Beam => {
                    painter.line_segment(
                        [rect.left_top(), rect.left_bottom()],
                        Stroke::new(1.5, color),
                    );
                }
                CursorShape::HollowBlock => {
                    painter.rect_stroke(
                        rect,
                        0.0,
                        Stroke::new(1.0, color),
                        egui::StrokeKind::Middle,
                    );
                }
                CursorShape::Hidden => {}
            }
        }
        // 绘制耗时（背景 rect + 文本 galley + 光标形状）。
        self.last_paint_ms = paint_start.elapsed().as_secs_f32() * 1000.0;

        // 缓存上限：滚动浏览大量历史时防止无限增长，超限只保留当前可见行。
        if self.rows_cache.len() > (self.rows as usize).saturating_mul(4).max(64) {
            let visible: std::collections::HashSet<i32> = (0..self.rows as usize)
                .map(|v| v as i32 - display_offset as i32)
                .collect();
            self.rows_cache.retain(|g, _| visible.contains(g));
        }

        // ==================== 焦点与输入 ====================
        if !self.initialized {
            ui.memory_mut(|m| m.request_focus(self.focus_id));
            self.initialized = true;
        }
        // 焦点自动恢复：egui 0.36 在 Text/Key 事件帧后可能清除焦点
        // （kittest 与部分平台；无事件时保持）。终端曾聚焦且当前无其他
        // 焦点（对话框/输入框等）时恢复，保证输入连续性。
        let has_focus_now = ui.memory(|m| m.has_focus(self.focus_id));
        if !has_focus_now && self.had_focus && ui.memory(|m| m.focused().is_none()) {
            ui.memory_mut(|m| m.request_focus(self.focus_id));
        }
        // 终端是一个整体的键盘控件，Tab/方向键/Esc 都应交给 shell 处理，
        // 不能触发 egui 的控件焦点导航。否则 shell 执行 Tab 后，终端会失去焦点，
        // 紧接的 Ctrl+C 可能被 UI 吞掉。
        let has_terminal_focus = ui.memory(|m| m.has_focus(self.focus_id));
        if has_terminal_focus {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    self.focus_id,
                    egui::EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: true,
                    },
                );
            });
        }
        self.had_focus = has_terminal_focus;
        // 点击/拖拽区域覆盖整个面板：终端文字不是 egui Label，必须自己维护
        // cell 选区，才能实现 Warp/Terminal.app 习惯的拖选后 ⌘C。
        let surface_rect = ui.max_rect();
        self.handle_dropped_files(ui, surface_rect, input_enabled);
        let response = ui.interact(surface_rect, self.focus_id, egui::Sense::click_and_drag());
        if response.clicked() {
            ui.memory_mut(|m| m.request_focus(self.focus_id));
            // 单击空白处清除旧选区；拖选会在 drag_started 时重新建立选区。
            self.selection = None;
        }
        if response.drag_started() {
            let start_pos = ui
                .input(|i| i.pointer.press_origin())
                .or_else(|| response.interact_pointer_pos());
            if let Some(pos) = start_pos {
                let point = selection_point_from_screen(
                    pos,
                    inner,
                    cell_width,
                    cell_height,
                    display_offset,
                    self.cols as usize,
                    self.rows as usize,
                );
                self.selection = Some(TerminalSelection {
                    anchor: point,
                    focus: point,
                });
                self.selecting = true;
                ui.memory_mut(|m| m.request_focus(self.focus_id));
            }
        }
        if self.selecting && response.dragged() {
            if let Some(pos) = response.interact_pointer_pos() {
                let point = selection_point_from_screen(
                    pos,
                    inner,
                    cell_width,
                    cell_height,
                    display_offset,
                    self.cols as usize,
                    self.rows as usize,
                );
                if let Some(selection) = &mut self.selection {
                    selection.focus = point;
                }
                ui.ctx().request_repaint();
            }
        }
        if response.drag_stopped() {
            self.selecting = false;
        }
        if input_enabled && ui.memory(|m| m.has_focus(self.focus_id)) {
            self.handle_input(ui, inner, output_rows);
        }
    }

    /// 将拖入终端区域的本地文件、目录或应用路径写入当前会话。
    ///
    /// egui 的原生后端把这三类对象统一表示为 `dropped_files`；有坐标时
    /// 沿用本帧的鼠标位置判断是否落在终端区域。部分 macOS 跨窗口拖放
    /// 不会提供坐标，此时当前窗口的终端是唯一的文本输入区，应接收该路径。
    fn handle_dropped_files(&mut self, ui: &Ui, surface_rect: Rect, input_enabled: bool) {
        if !input_enabled {
            return;
        }

        let paths = ui.input(|input| {
            if let Some(pointer) = input.pointer.hover_pos() {
                if !surface_rect.contains(pointer) {
                    return Vec::new();
                }
            }
            input
                .raw
                .dropped_files
                .iter()
                .map(|file| file.path().to_path_buf())
                .collect::<Vec<_>>()
        });
        let Some(text) = dropped_paths_text(&paths) else {
            return;
        };

        // 路径已经按 shell 语法转义，不需要执行回车；用户可以继续编辑
        // 命令，或在需要时手动按 Enter 执行。
        self.session.write(text.as_bytes());
        self.workdir.push_text(&text);
        self.selection = None;
        ui.memory_mut(|memory| memory.request_focus(self.focus_id));
        // 某些后端的 PTY 回显不会同步触发下一帧，主动安排一次重绘以便
        // 拖放后的路径尽快显示出来。
        ui.ctx().request_repaint();
    }

    /// 处理键盘与鼠标输入（转发到 PTY / 网格滚动）。
    fn handle_input(&mut self, ui: &Ui, inner: Rect, output_rows: Option<Vec<String>>) {
        let session = &self.session;
        let mode = self.last_mode;
        let cell_height = self.cell_height;
        let ctx = ui.ctx().clone();
        // 滚动后需要重绘；不能在 ui.input 闭包内调用 request_repaint
        // （Context 锁已被 input 持有，会自死锁 10 秒后 panic），用 flag 延后。
        let mut need_repaint = false;
        // 本帧输入动作（闭包内只读 self 写入 PTY，闭包外统一更新工作目录跟踪器）。
        let mut actions: Vec<InputAction> = Vec::new();

        // 检测本帧是否有退格/删除键按下（含上一帧的抑制状态）。
        // 某些输入法（如微信输入法）在退格时会伴随发送"空格类" Text 事件，
        // 写入终端会表现为"删除键插入空格"。
        let backspace_this_frame = ui.input(|i| {
            i.events.iter().any(|e| {
                matches!(
                    e,
                    egui::Event::Key {
                        key: egui::Key::Backspace | egui::Key::Delete,
                        pressed: true,
                        ..
                    }
                )
            })
        });
        // 正常的空格也会同时产生 Key::Space + Text(" ")。退格后若用户立刻
        // 输入空格，不能因为抑制输入法伪事件而把这个真实空格吞掉。
        let explicit_space_this_frame = ui.input(|i| {
            i.events.iter().any(|e| {
                matches!(
                    e,
                    egui::Event::Key {
                        key: egui::Key::Space,
                        pressed: true,
                        ..
                    }
                )
            })
        });
        let suppress_blank_text =
            (self.suppress_blank_frames > 0 || backspace_this_frame) && !explicit_space_this_frame;
        self.suppress_blank_frames = if backspace_this_frame {
            2
        } else {
            self.suppress_blank_frames.saturating_sub(1)
        };

        ui.input(|i| {
            for event in &i.events {
                match event {
                    egui::Event::Key {
                        key,
                        modifiers,
                        pressed,
                        ..
                    } => {
                        if !*pressed {
                            continue;
                        }
                        // Linux/Windows 终端惯用 Ctrl+Shift+C 复制选区；保留
                        // macOS 的 ⌘C，同时避免把组合键继续送进 shell。
                        if modifiers.ctrl && modifiers.shift && *key == egui::Key::C {
                            actions.push(InputAction::CopySelection);
                            continue;
                        }
                        // ⌘C 是终端复制；没有选区时不向 shell 发送任何字符。
                        if modifiers.command {
                            if *key == egui::Key::C {
                                actions.push(InputAction::CopySelection);
                            }
                            continue;
                        }
                        let mods = Mods {
                            shift: modifiers.shift,
                            alt: modifiers.alt,
                            ctrl: modifiers.ctrl,
                            super_: false,
                        };
                        if let Some(scroll) = scrollback_key(key, *modifiers) {
                            // Shift+PageUp/PageDown 不应发送给 shell，而是作为
                            // 终端窗口的本地 scrollback 翻页。编码层为这两个
                            // 组合返回 None；这里必须真正执行滚动，否则按键
                            // 会变成“既不发数据也不滚动”的无操作。
                            let term_arc = session.term();
                            let mut guard = term_arc.lock();
                            guard.scroll_display(scroll);
                            need_repaint = true;
                            continue;
                        }
                        // Ctrl/Alt 修饰的字母与符号键：直接编码为控制字符/转义前缀
                        // （egui 0.36 的 Text 事件与 Key 事件独立，这里处理并让 Text 事件跳过）。
                        if mods.ctrl || mods.alt {
                            if let Some(k) = map_char_key(key, modifiers.shift) {
                                if let Some(bytes) = keys::encode_key(k, mods, mode) {
                                    session.write(&bytes);
                                    actions.push(InputAction::Bytes(bytes));
                                }
                                continue;
                            }
                        }
                        if let Some(k) = map_special_key(key) {
                            if let Some(bytes) = keys::encode_key(k, mods, mode) {
                                session.write(&bytes);
                                actions.push(InputAction::Bytes(bytes));
                            }
                        }
                    }
                    egui::Event::Text(text) => {
                        // 退格/删除键伴随的"空白类"文本（输入法产物）丢弃，
                        // 只影响空格/零宽等空白字符，正常输入不受影响。
                        if suppress_blank_text
                            && text.chars().all(|c| c == ' ' || !is_printable_text_char(c))
                        {
                            continue;
                        }
                        // Ctrl/Alt 组合已在 Key 事件处理，跳过避免重复写入。
                        let mods = i.modifiers;
                        if mods.ctrl || mods.alt {
                            continue;
                        }
                        // 过滤不可打印字符（控制符/私有区/零宽字符等）。
                        // 某些输入法或平台在退格等按键时会产生零宽空格（\u{200b}），
                        // 直接写入会在终端插入空格。
                        if !text.chars().all(is_printable_text_char) {
                            continue;
                        }
                        session.write(text.as_bytes());
                        actions.push(InputAction::Text(text.clone()));
                    }
                    egui::Event::Paste(text) => {
                        // 括号粘贴模式（bracketed paste）下包装转义序列。
                        let payload = if mode.contains(TermMode::BRACKETED_PASTE) {
                            bracketed_paste_payload(text)
                        } else {
                            text.clone()
                        };
                        session.write(payload.as_bytes());
                        // 粘贴内容不可逐字节信任（可能含控制序列），模型失效。
                        actions.push(InputAction::Paste);
                    }
                    egui::Event::Copy => {
                        actions.push(InputAction::CopySelection);
                    }
                    egui::Event::MouseWheel {
                        unit,
                        delta,
                        modifiers,
                        ..
                    } => {
                        let Some(pointer) =
                            i.pointer.hover_pos().filter(|pos| inner.contains(*pos))
                        else {
                            continue;
                        };
                        let Some(direction) = mouse_wheel_direction(delta.y) else {
                            continue;
                        };
                        let steps = mouse_wheel_steps(*unit, delta.y);
                        if steps == 0 {
                            continue;
                        }

                        // 终端应用（如 Vim）先于本地 scrollback 取得滚轮：应用打开
                        // DECSET 鼠标上报后，必须收到 xterm 鼠标按键序列才能处理滚动。
                        match wheel_target(mode) {
                            WheelTarget::ApplicationMouse => {
                                let (column, row) = terminal_cell_from_screen(
                                    pointer,
                                    inner,
                                    self.cell_width,
                                    cell_height,
                                    self.cols as usize,
                                    self.rows as usize,
                                );
                                let mods = Mods {
                                    shift: modifiers.shift,
                                    alt: modifiers.alt,
                                    ctrl: modifiers.ctrl,
                                    super_: false,
                                };
                                let Some(bytes) =
                                    keys::encode_mouse_wheel(direction, mods, mode, column, row)
                                else {
                                    continue;
                                };
                                for _ in 0..steps {
                                    session.write(&bytes);
                                    actions.push(InputAction::Bytes(bytes.clone()));
                                }
                            }
                            // 未启用鼠标上报的替代屏应用仍应遵循终端惯例，将滚轮
                            // 映射为方向键（例如未设 mouse=a 的 Vim 或 less）。
                            WheelTarget::AlternateScroll => {
                                let key = match direction {
                                    MouseWheelDirection::Up => Key::Up,
                                    MouseWheelDirection::Down => Key::Down,
                                };
                                let Some(bytes) = keys::encode_key(key, Mods::default(), mode)
                                else {
                                    continue;
                                };
                                for _ in 0..steps {
                                    session.write(&bytes);
                                    actions.push(InputAction::Bytes(bytes.clone()));
                                }
                            }
                            WheelTarget::Scrollback => {
                                let lines = match unit {
                                    egui::MouseWheelUnit::Point | egui::MouseWheelUnit::Line => {
                                        let steps = steps as i32;
                                        if delta.y > 0.0 {
                                            steps
                                        } else {
                                            -steps
                                        }
                                    }
                                    egui::MouseWheelUnit::Page => {
                                        let term_arc = session.term();
                                        let mut guard = term_arc.lock();
                                        if delta.y > 0.0 {
                                            guard.scroll_display(Scroll::PageUp);
                                        } else {
                                            guard.scroll_display(Scroll::PageDown);
                                        }
                                        need_repaint = true;
                                        0
                                    }
                                };
                                if lines != 0 {
                                    let term_arc = session.term();
                                    let mut guard = term_arc.lock();
                                    if modifiers.alt {
                                        if lines > 0 {
                                            guard.scroll_display(Scroll::PageUp);
                                        } else {
                                            guard.scroll_display(Scroll::PageDown);
                                        }
                                    } else {
                                        guard.scroll_display(Scroll::Delta(lines));
                                    }
                                    need_repaint = true;
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        });

        if need_repaint {
            ctx.request_repaint();
        }

        // 闭包外统一应用输入动作，更新工作目录跟踪器。
        for action in actions {
            self.apply_input_action(action, &ctx);
        }
        if self.workdir.awaiting_any_pwd_output() {
            // `output_rows` 是处理本帧 Enter 之前的屏幕快照，正好作为
            // pwd 输出的基线；这样即使 shell 很快返回，也不会把新结果
            // 误当成旧输出。
            self.pwd_output_rows =
                Some(output_rows.unwrap_or_else(|| visible_terminal_rows(&self.session)));
        } else {
            self.pwd_output_rows = None;
        }
        self.render_copy_feedback(ui);
    }

    /// 应用一帧内的输入动作（写入 PTY 的字节与拦截的按键）。
    fn apply_input_action(&mut self, action: InputAction, ctx: &egui::Context) {
        match action {
            InputAction::Bytes(bytes) => self.track_input_bytes(&bytes),
            InputAction::Text(text) => {
                self.workdir.push_text(&text);
            }
            InputAction::Paste => {
                // 粘贴内容不可逐字节信任（可能包含控制序列），暂停目录跟踪。
                self.workdir.invalidate();
            }
            InputAction::CopySelection => self.copy_selection(ctx),
        }
    }

    /// 将当前终端选区交给 egui 平台层写入系统剪贴板。
    fn copy_selection(&mut self, ctx: &egui::Context) {
        let Some(selection) = self.selection else {
            return;
        };
        let term_arc = self.session.term();
        let text = {
            let guard = term_arc.lock();
            selection_to_text(guard.grid(), selection, self.cols as usize)
        };
        if text.is_empty() {
            return;
        }
        ctx.copy_text(text);
        let now = ctx.input(|i| i.time);
        self.copy_flash_until = Some(now + 1.2);
        ctx.request_repaint_after(Duration::from_millis(1200));
    }

    /// 分析写入 PTY 的字节并同步工作目录跟踪器（本地与远程会话）。
    fn track_input_bytes(&mut self, bytes: &[u8]) {
        match bytes {
            // 回车：执行命令并尝试解析 cd，清空当前输入跟踪。
            b"\r" | b"\n" => {
                if self.session.is_remote() {
                    self.workdir.execute_remote(self.remote_home.as_deref());
                } else {
                    self.workdir.execute();
                }
            }
            // Ctrl+C：重置当前行。
            b"\x03" => {
                self.workdir.reset();
            }
            // 退格/删除。
            b"\x7f" | b"\x08" => {
                self.workdir.backspace();
            }
            // Tab（shell 自身补全/移动光标）：输入行可能被 shell 改写，暂停目录跟踪。
            b"\t" => {
                self.workdir.invalidate();
            }
            _ => {
                // 可见文本（ASCII 可打印 / 空格 / 非 ASCII）。
                if let Ok(s) = std::str::from_utf8(bytes) {
                    if s.chars()
                        .all(|c| c.is_ascii_graphic() || c == ' ' || !c.is_ascii())
                    {
                        self.workdir.push_text(s);
                        return;
                    }
                }
                // 控制序列/编辑键（箭头、Ctrl+U/W 等）：光标位置不可追踪，模型失效。
                self.workdir.invalidate();
            }
        }
    }

    /// 复制成功后的非侵入式反馈，不抢终端焦点。
    fn render_copy_feedback(&mut self, ui: &Ui) {
        let Some(until) = self.copy_flash_until else {
            return;
        };
        let now = ui.ctx().input(|i| i.time);
        if now >= until {
            self.copy_flash_until = None;
            return;
        }
        ui.ctx()
            .request_repaint_after(Duration::from_secs_f64((until - now).min(0.2)));
        let theme = crate::theme::current_theme();
        egui::Area::new(egui::Id::new("copy_feedback"))
            .order(egui::Order::Foreground)
            .interactable(false)
            .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-16.0, 16.0))
            .show(ui.ctx(), |ui| {
                egui::Frame::new()
                    .fill(theme.bg_elevated.gamma_multiply(0.96))
                    .stroke(egui::Stroke::new(1.0, theme.accent))
                    .corner_radius(7.0)
                    .inner_margin(egui::Margin::symmetric(10, 6))
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new("COPIED")
                                .monospace()
                                .size(10.0)
                                .color(theme.accent),
                        );
                    });
            });
    }
}

/// 一帧内的终端输入动作（闭包内收集，闭包外统一应用到工作目录跟踪器）。
enum InputAction {
    /// 已写入 PTY 的字节。
    Bytes(Vec<u8>),
    /// 已写入的可见文本。
    Text(String),
    /// 粘贴（工作目录跟踪器失效）。
    Paste,
    /// 复制当前终端选区。
    CopySelection,
}

// ==================== 辅助函数 ====================

/// 读取终端当前视口的纯文本行（去除 VT 属性与尾随空格）。
///
/// 这里只在 `pwd` 输出校正期间调用；正常渲染仍使用行级损坏缓存，避免
/// 每帧遍历全部 cell。
fn visible_terminal_rows(session: &Session) -> Vec<String> {
    let term_arc = session.term();
    let guard = term_arc.lock();
    let content = guard.renderable_content();
    let mut rows = Vec::new();
    let mut current_line: Option<i32> = None;
    let mut current = String::new();

    for item in content.display_iter {
        let line = item.point.line.0;
        if current_line != Some(line) {
            if current_line.is_some() {
                rows.push(current.trim_end().to_string());
            }
            current_line = Some(line);
            current.clear();
        }
        let cell = item.cell;
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) || cell.flags.contains(Flags::HIDDEN) {
            continue;
        }
        current.push(cell.c);
    }
    if current_line.is_some() {
        rows.push(current.trim_end().to_string());
    }
    rows
}

/// 滚轮事件的优先目标：全屏应用的鼠标协议优先于本地 scrollback。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WheelTarget {
    ApplicationMouse,
    AlternateScroll,
    Scrollback,
}

fn wheel_target(mode: TermMode) -> WheelTarget {
    if mode.intersects(TermMode::MOUSE_MODE) {
        WheelTarget::ApplicationMouse
    } else if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
        WheelTarget::AlternateScroll
    } else {
        WheelTarget::Scrollback
    }
}

fn mouse_wheel_direction(delta_y: f32) -> Option<MouseWheelDirection> {
    if delta_y > 0.0 {
        Some(MouseWheelDirection::Up)
    } else if delta_y < 0.0 {
        Some(MouseWheelDirection::Down)
    } else {
        None
    }
}

/// 一次 egui 滚轮事件应转换成多少个离散 xterm 滚轮按键。
///
/// 滚轮是“意图”而非距离：一次滚轮手势应只上报少量按键（如 1)，而不是按
/// 滚动像素距离折算成多次。按像素折算会让 macOS 触控板的 Point 事件
/// （单帧几十像素）一次上报十几次，Vim 里直接翻过几屏、定位不到想看的行。
fn mouse_wheel_steps(unit: egui::MouseWheelUnit, delta_y: f32) -> usize {
    let magnitude = delta_y.abs();
    if !magnitude.is_finite() || magnitude == 0.0 {
        return 0;
    }

    match unit {
        // 离散滚轮刻度：直接按刻度数上报（通常为 1）。
        egui::MouseWheelUnit::Line => magnitude.ceil().clamp(1.0, 3.0) as usize,
        // 触控板/高精度滚轮：一律视为一次手势、只发 1 次，避免惯性滚动刷屏。
        egui::MouseWheelUnit::Point => 1,
        // 整页滚动：只发 1 次，由应用自己决定翻多少。
        egui::MouseWheelUnit::Page => 1,
    }
}

/// 屏幕坐标 → 当前终端视口 cell 坐标（从零开始）。
fn terminal_cell_from_screen(
    pos: egui::Pos2,
    inner: Rect,
    cell_width: f32,
    cell_height: f32,
    cols: usize,
    rows: usize,
) -> (usize, usize) {
    let x = (pos.x - inner.left()).clamp(0.0, inner.width().max(0.0));
    let y = (pos.y - inner.top()).clamp(0.0, inner.height().max(0.0));
    let col = (x / cell_width.max(1.0)).floor() as usize;
    let row = (y / cell_height.max(1.0)).floor() as usize;
    (
        col.min(cols.saturating_sub(1)),
        row.min(rows.saturating_sub(1)),
    )
}

/// 屏幕坐标 → 当前视口对应的网格坐标。
fn selection_point_from_screen(
    pos: egui::Pos2,
    inner: Rect,
    cell_width: f32,
    cell_height: f32,
    display_offset: usize,
    cols: usize,
    rows: usize,
) -> SelectionPoint {
    let x = (pos.x - inner.left()).clamp(0.0, inner.width().max(0.0));
    let y = (pos.y - inner.top()).clamp(0.0, inner.height().max(0.0));
    let col = (x / cell_width.max(1.0)).floor() as usize;
    let row = (y / cell_height.max(1.0)).floor() as usize;
    SelectionPoint {
        grid_line: row.min(rows.saturating_sub(1)) as i32 - display_offset as i32,
        col: col.min(cols.saturating_sub(1)),
    }
}

/// 从网格中提取选区文本，遵循 alacritty 的软换行、宽字符和组合字符语义。
fn selection_to_text(
    grid: &alacritty_terminal::grid::Grid<alacritty_terminal::term::cell::Cell>,
    selection: TerminalSelection,
    cols: usize,
) -> String {
    let (start, end) = if selection.anchor <= selection.focus {
        (selection.anchor, selection.focus)
    } else {
        (selection.focus, selection.anchor)
    };
    // 选区是建立时的 grid_line 快照，网格可能因 resize/scrollback 裁剪而缩小；
    // alacritty 的 Storage 越界防护仅 debug_assert，release 下会直接 panic——
    // 复制前校验范围（有效网格行号 = [-history_size, screen_lines)），越界放弃复制。
    let history = grid.history_size() as i32;
    let screen = grid.screen_lines() as i32;
    // cols 来自渲染器的快照；在 resize 或测试/异常调用下可能为 0，或大于当前网格宽度。
    // 后续会访问 Column(cols - 1)，因此必须在索引前拒绝不一致的快照。
    if cols == 0 || cols > grid.columns() || start.grid_line < -history || end.grid_line >= screen {
        return String::new();
    }
    let mut output = String::new();
    for grid_line in start.grid_line..=end.grid_line {
        let Some((mut from, to)) = selection.columns_for_line(grid_line, cols) else {
            continue;
        };
        let row = &grid[if grid_line >= 0 {
            alacritty_terminal::index::Line::from(grid_line as usize)
        } else {
            alacritty_terminal::index::Line::from(0) - grid_line.unsigned_abs() as usize
        }];
        if from < to
            && row[alacritty_terminal::index::Column(from)]
                .flags
                .contains(Flags::WIDE_CHAR_SPACER)
            && from > 0
        {
            // 选中宽字符的第二个 cell 时，把主字符一并纳入复制。
            from -= 1;
        }
        let line_length = row.line_length().0.min(to);
        let mut line = String::new();
        for col in from..line_length {
            let cell = &row[alacritty_terminal::index::Column(col)];
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            if cell.flags.contains(Flags::HIDDEN) {
                line.push(' ');
            } else {
                line.push(cell.c);
                if let Some(zero_width) = cell.zerowidth() {
                    line.extend(zero_width.iter().copied());
                }
            }
        }
        output.push_str(&line);
        if grid_line != end.grid_line
            && !row[alacritty_terminal::index::Column(cols - 1)]
                .flags
                .contains(Flags::WRAPLINE)
        {
            output.push('\n');
        }
    }
    output
}

/// 锁内构建单个网格行的渲染数据（段 + 背景 + hash）。
///
/// `grid_line` 为网格行号（滚动到 scrollback 时为负）。复用 `display_iter` 的
/// 单行语义：占位格跳过、颜色解析、背景段合并、文本段合并。
/// 注意：不再对光标 cell 做反色——Block 光标最终由光标色实心矩形覆盖，反色不可见，
/// 剔除后光标行内容 hash 稳定，光标移动不触发行重建。
#[allow(clippy::too_many_arguments)]
fn build_line_data(
    grid: &alacritty_terminal::grid::Grid<alacritty_terminal::term::cell::Cell>,
    grid_line: i32,
    cols: usize,
    colors: &Colors,
    default_fg: Rgb,
    default_bg: Rgb,
    default_bg_egui: Color32,
) -> LineData {
    let mut segments: Vec<Segment> = Vec::new();
    let mut backgrounds: Vec<BgRect> = Vec::new();
    let mut hash: u64 = 0;
    // `Line` 的 tuple 构造器不公开，负行号（scrollback）用 `Line(0) - n` 构造。
    let row = &grid[if grid_line >= 0 {
        alacritty_terminal::index::Line::from(grid_line as usize)
    } else {
        alacritty_terminal::index::Line::from(0) - grid_line.unsigned_abs() as usize
    }];

    for (col, cell) in row.into_iter().enumerate().take(cols) {
        // 解析颜色（含粗体 → 亮色映射）。
        let mut fg = resolve_color(
            cell.fg,
            colors,
            default_fg,
            cell.flags.contains(Flags::BOLD),
        );
        let mut bg = resolve_color(cell.bg, colors, default_bg, false);
        let bold = cell.flags.contains(Flags::BOLD);
        let italic = cell.flags.contains(Flags::ITALIC);
        let underline = cell.flags.contains(Flags::UNDERLINE);
        let strikeout = cell.flags.contains(Flags::STRIKEOUT);

        // INVERSE 反色。
        if cell.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        // DIM 减暗（粗体不减）。
        if cell.flags.contains(Flags::DIM) && !bold {
            fg = Color32::from_rgb(fg.r() / 2, fg.g() / 2, fg.b() / 2);
        }

        // 背景段合并（默认背景不绘制）。
        push_background(&mut backgrounds, col, bg, default_bg_egui);

        let leading_spacer = cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER);
        let wide_spacer = cell.flags.contains(Flags::WIDE_CHAR_SPACER);
        if wide_spacer && !leading_spacer {
            // 普通宽字符占位格由前一个宽字符的 glyph 提供视觉宽度，
            // 不再追加文本，但必须进入哈希以跟踪其背景/属性变化。
            mix_cell_hash(
                &mut hash,
                cell.c,
                CellStyle {
                    fg,
                    bg,
                    bold,
                    italic,
                    underline,
                    strikeout,
                },
                cell.zerowidth(),
            );
            continue;
        }

        // 文本段合并。
        let text = if cell.flags.contains(Flags::HIDDEN) || leading_spacer {
            ' '
        } else {
            cell.c
        };
        let zero_width = if cell.flags.contains(Flags::HIDDEN) || leading_spacer {
            None
        } else {
            cell.zerowidth()
        };
        push_or_merge(
            &mut segments,
            text,
            zero_width,
            CellStyle {
                fg,
                bg,
                bold,
                italic,
                underline,
                strikeout,
            },
            &mut hash,
        );
    }

    LineData {
        hash,
        segments,
        backgrounds,
    }
}

/// 追加一个背景 cell；只有颜色相同且列号紧邻时才允许合并。
fn push_background(backgrounds: &mut Vec<BgRect>, col: usize, color: Color32, default_bg: Color32) {
    if color == default_bg {
        return;
    }
    if let Some(last) = backgrounds.last_mut() {
        if last.color == color && last.end == col {
            last.end = col + 1;
            return;
        }
    }
    backgrounds.push(BgRect {
        start: col,
        end: col + 1,
        color,
    });
}

/// 解析终端颜色为 egui 颜色（Catppuccin 调色板 + xterm 256 色表）。
///
/// 优先级：程序直接指定颜色（Spec）> OSC 动态覆盖（term.colors）> 内置调色板。
fn resolve_color(color: AColor, colors: &Colors, default: Rgb, bold: bool) -> Color32 {
    match color {
        AColor::Spec(rgb) => to_egui(rgb),
        AColor::Named(n) => {
            // 背景始终用主题色（OSC 11 背景覆盖不生效——zsh 主题常设深色背景，
            // 否则浅色主题下终端仍为深色）。
            if n == NamedColor::Background {
                return to_egui(crate::theme::current_theme().term_bg);
            }
            // OSC 覆盖优先（其余颜色仍尊重终端程序动态改色）。
            if let Some(rgb) = colors[n as usize] {
                return to_egui(rgb);
            }
            match n {
                NamedColor::Foreground => to_egui(default),
                NamedColor::Background => unreachable!(),
                NamedColor::Cursor => to_egui(crate::theme::current_theme().term_cursor),
                _ => {
                    let mut idx = n as usize;
                    // 粗体时将基本色映射到亮色（参照 Alacritty 默认行为）。
                    if bold && idx < 8 {
                        idx += 8;
                    }
                    if idx < 16 {
                        to_egui(crate::theme::current_theme().term_palette[idx])
                    } else {
                        // 其余命名色（Dim 系等）用 256 色表兜底。
                        to_egui(crate::theme::xterm256(
                            idx as u8,
                            crate::theme::current_theme().term_palette,
                        ))
                    }
                }
            }
        }
        AColor::Indexed(i) => {
            // OSC 覆盖优先。
            if let Some(rgb) = colors[i as usize] {
                return to_egui(rgb);
            }
            to_egui(crate::theme::xterm256(
                i,
                crate::theme::current_theme().term_palette,
            ))
        }
    }
}

/// alacritty Rgb → egui Color32。
fn to_egui(rgb: Rgb) -> Color32 {
    Color32::from_rgb(rgb.r, rgb.g, rgb.b)
}

/// cell 的文本样式（用于段合并判断与哈希）。
#[derive(Clone, Copy)]
struct CellStyle {
    fg: Color32,
    bg: Color32,
    bold: bool,
    italic: bool,
    underline: bool,
    strikeout: bool,
}

impl CellStyle {
    fn key(self) -> u64 {
        u64::from(self.fg.r())
            ^ (u64::from(self.fg.g()) << 8)
            ^ (u64::from(self.fg.b()) << 16)
            ^ (u64::from(self.bg.r()) << 32)
            ^ (u64::from(self.bg.g()) << 40)
            ^ (u64::from(self.bg.b()) << 48)
            ^ (u64::from(self.bold) << 24)
            ^ (u64::from(self.italic) << 25)
            ^ (u64::from(self.underline) << 26)
            ^ (u64::from(self.strikeout) << 27)
    }
}

/// 合并或追加一个 cell 到段列表（相同样式则追加字符）。
fn push_or_merge(
    segments: &mut Vec<Segment>,
    c: char,
    zero_width: Option<&[char]>,
    style: CellStyle,
    hash: &mut u64,
) {
    if let Some(last) = segments.last_mut() {
        if last.fg == style.fg
            && last.bold == style.bold
            && last.italic == style.italic
            && last.underline == style.underline
            && last.strikeout == style.strikeout
        {
            last.text.push(c);
            if let Some(zero_width) = zero_width {
                last.text.extend(zero_width.iter().copied());
            }
            mix_cell_hash(hash, c, style, zero_width);
            return;
        }
    }
    segments.push(Segment {
        text: c.to_string(),
        fg: style.fg,
        bold: style.bold,
        italic: style.italic,
        underline: style.underline,
        strikeout: style.strikeout,
    });
    if let Some(zero_width) = zero_width {
        if let Some(last) = segments.last_mut() {
            last.text.extend(zero_width.iter().copied());
        }
    }
    mix_cell_hash(hash, c, style, zero_width);
}

/// 将影响行绘制的 cell 属性加入缓存指纹。
fn mix_cell_hash(hash: &mut u64, c: char, style: CellStyle, zero_width: Option<&[char]>) {
    *hash = hash.wrapping_mul(131).wrapping_add(style.key());
    *hash = hash.wrapping_mul(131).wrapping_add(c as u64);
    if let Some(zero_width) = zero_width {
        for c in zero_width {
            *hash = hash.wrapping_mul(131).wrapping_add(*c as u64);
        }
    }
}

/// 样式 → 哈希键。
#[allow(dead_code)]
fn style_key(
    fg: Color32,
    bg: Color32,
    bold: bool,
    italic: bool,
    underline: bool,
    strikeout: bool,
) -> u64 {
    CellStyle {
        fg,
        bg,
        bold,
        italic,
        underline,
        strikeout,
    }
    .key()
}

/// 将段列表构建为 egui LayoutJob。
fn build_job(segments: &[Segment], font_size: f32) -> LayoutJob {
    let mut job = LayoutJob::default();
    for seg in segments {
        let format = TextFormat {
            font_id: FontId::monospace(font_size),
            color: seg.fg,
            italics: seg.italic,
            underline: if seg.underline {
                Stroke::new(1.0, seg.fg)
            } else {
                Stroke::NONE
            },
            strikethrough: if seg.strikeout {
                Stroke::new(1.0, seg.fg)
            } else {
                Stroke::NONE
            },
            ..Default::default()
        };
        job.append(&seg.text, 0.0, format);
    }
    job
}

/// 判断字符是否可安全写入终端。
///
/// Text 事件本身已经是用户输入文本；只过滤 ASCII 控制字符，以及输入法
/// 在退格等按键中偶尔附带的零宽空格/BOM。不能把整个 Unicode 格式字符区
/// 都丢掉：变体选择符和零宽连接符是 emoji、部分文字系统的有效组成部分。
fn is_printable_text_char(c: char) -> bool {
    !c.is_ascii_control() && c != '\u{200b}' && c != '\u{feff}'
}

/// egui 键 → 终端字符键（仅无文本时兜底使用）。
fn map_char_key(key: &egui::Key, shift: bool) -> Option<Key> {
    use egui::Key as E;
    let v = *key as u8;
    // 字母与数字键（枚举判别值连续，按声明顺序）。
    if (E::A as u8..=E::Z as u8).contains(&v) {
        let c = (v - E::A as u8 + b'a') as char;
        return Some(Key::Char(if shift { c.to_ascii_uppercase() } else { c }));
    }
    if (E::Num0 as u8..=E::Num9 as u8).contains(&v) {
        let c = if shift {
            match key {
                E::Num0 => ')',
                E::Num1 => '!',
                E::Num2 => '@',
                E::Num3 => '#',
                E::Num4 => '$',
                E::Num5 => '%',
                E::Num6 => '^',
                E::Num7 => '&',
                E::Num8 => '*',
                E::Num9 => '(',
                _ => unreachable!("数字键范围内只能出现 Num0..Num9"),
            }
        } else {
            (v - E::Num0 as u8 + b'0') as char
        };
        return Some(Key::Char(c));
    }
    let c = match key {
        E::Space => ' ',
        E::Minus => {
            if shift {
                '_'
            } else {
                '-'
            }
        }
        E::Equals => {
            if shift {
                '+'
            } else {
                '='
            }
        }
        E::Comma => {
            if shift {
                '<'
            } else {
                ','
            }
        }
        E::Period => {
            if shift {
                '>'
            } else {
                '.'
            }
        }
        E::Slash => {
            if shift {
                '?'
            } else {
                '/'
            }
        }
        E::Semicolon => {
            if shift {
                ':'
            } else {
                ';'
            }
        }
        E::Quote => {
            if shift {
                '"'
            } else {
                '\''
            }
        }
        E::Backtick => {
            if shift {
                '~'
            } else {
                '`'
            }
        }
        E::Backslash => {
            if shift {
                '|'
            } else {
                '\\'
            }
        }
        E::OpenBracket => {
            if shift {
                '{'
            } else {
                '['
            }
        }
        E::CloseBracket => {
            if shift {
                '}'
            } else {
                ']'
            }
        }
        E::Colon => ':',
        E::Plus => '+',
        E::Pipe => '|',
        E::Questionmark => '?',
        E::Exclamationmark => '!',
        E::OpenCurlyBracket => '{',
        E::CloseCurlyBracket => '}',
        _ => return None,
    };
    Some(Key::Char(c))
}

/// 构造括号粘贴载荷。
///
/// 粘贴内容属于不可信输入；若保留其中的 ESC，文本内的
/// `ESC[201~` 可以提前关闭括号粘贴，让后续换行或控制序列脱离编辑缓冲区。
/// 删除 ESC 后，原始序列会变成普通文本，协议边界只由这里追加的结束标记提供。
fn bracketed_paste_payload(text: &str) -> String {
    let sanitized: String = text.chars().filter(|&c| c != '\x1b').collect();
    format!("\x1b[200~{sanitized}\x1b[201~")
}

/// 将拖入的路径转换为可直接交给 POSIX shell 的文本。
///
/// 常见路径保持原样，包含空格、引号或 shell 特殊字符的路径使用单引号；
/// 单引号本身用 shell 的 `'\''` 组合拆分，确保拖入路径只会作为一个参数，
/// 不会因为文件名内容被解释成额外的命令或重定向。
fn shell_escape_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    if text.chars().all(is_unquoted_shell_path_char) {
        return text.into_owned();
    }

    let mut escaped = String::with_capacity(text.len() + 2);
    escaped.push('\'');
    for character in text.chars() {
        if character == '\'' {
            escaped.push_str("'\\''");
        } else {
            escaped.push(character);
        }
    }
    escaped.push('\'');
    escaped
}

/// 不需要引号时允许出现在路径中的字符。
fn is_unquoted_shell_path_char(character: char) -> bool {
    character.is_alphanumeric()
        || matches!(
            character,
            '/' | '_' | '-' | '.' | '~' | '@' | '%' | '+' | '=' | ':' | ','
        )
}

/// 将同一拖放操作中的多个路径拼接为一段终端输入。
fn dropped_paths_text(paths: &[std::path::PathBuf]) -> Option<String> {
    let text = paths
        .iter()
        .map(|path| shell_escape_path(path))
        .collect::<Vec<_>>()
        .join(" ");
    (!text.is_empty()).then_some(text)
}

/// egui 键 → 终端特殊键。
fn map_special_key(key: &egui::Key) -> Option<Key> {
    use egui::Key as E;
    match key {
        E::Enter => Some(Key::Enter),
        E::Tab => Some(Key::Tab),
        E::Backspace => Some(Key::Backspace),
        E::Escape => Some(Key::Escape),
        E::ArrowUp => Some(Key::Up),
        E::ArrowDown => Some(Key::Down),
        E::ArrowLeft => Some(Key::Left),
        E::ArrowRight => Some(Key::Right),
        E::Home => Some(Key::Home),
        E::End => Some(Key::End),
        E::PageUp => Some(Key::PageUp),
        E::PageDown => Some(Key::PageDown),
        E::Insert => Some(Key::Insert),
        E::Delete => Some(Key::Delete),
        _ => {
            // F 键（F1-F35 判别值连续）。
            let v = *key as u8;
            if v >= E::F1 as u8 && v <= E::F35 as u8 {
                Some(Key::F(v - E::F1 as u8 + 1))
            } else {
                None
            }
        }
    }
}

/// 终端窗口本地处理的翻页组合。
///
/// 只接受纯 Shift，避免拦截 Shift+Alt/Ctrl 等应继续交给终端程序的
/// 修饰键序列；Command 组合也留给应用级快捷键处理。
fn scrollback_key(key: &egui::Key, modifiers: egui::Modifiers) -> Option<Scroll> {
    if !modifiers.shift || modifiers.alt || modifiers.ctrl || modifiers.command {
        return None;
    }
    match key {
        egui::Key::PageUp => Some(Scroll::PageUp),
        egui::Key::PageDown => Some(Scroll::PageDown),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mino_core::terminal::{Session, SessionOptions};
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[derive(Debug)]
    struct TestDroppedFile {
        path: PathBuf,
    }

    impl egui::DroppedFile for TestDroppedFile {
        fn path(&self) -> &std::path::Path {
            &self.path
        }

        fn bytes(&self) -> Result<Vec<u8>, String> {
            Ok(Vec::new())
        }
    }

    /// 将终端可见区域转为文本。
    fn grid_text(session: &Session) -> String {
        use alacritty_terminal::term::cell::Flags;
        let term_arc = session.term();
        let guard = term_arc.lock();
        let content = guard.renderable_content();
        let mut lines: Vec<String> = Vec::new();
        let mut current = String::new();
        let mut started = false;
        let mut prev_grid_line: i32 = i32::MIN;
        for item in content.display_iter {
            let cell = item.cell;
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) || cell.flags.contains(Flags::HIDDEN) {
                continue;
            }
            if item.point.line.0 != prev_grid_line {
                if started {
                    lines.push(current.trim_end().to_string());
                }
                current = String::new();
                started = true;
                prev_grid_line = item.point.line.0;
            }
            current.push(cell.c);
        }
        if started {
            lines.push(current.trim_end().to_string());
        }
        lines.join("\n")
    }

    /// 等待终端文本包含子串。
    fn wait_text(
        view: &Rc<RefCell<TerminalView>>,
        harness: &mut egui_kittest::Harness,
        needle: &str,
    ) -> bool {
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            harness.step();
            let text = grid_text(view.borrow().session());
            if text.contains(needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        false
    }

    /// 模拟真实按键：Key 事件 + Text 事件（与 egui-winit 行为一致）。
    fn send_key(harness: &mut egui_kittest::Harness, key: egui::Key, text: Option<&str>) {
        harness.event(egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        });
        if let Some(t) = text {
            harness.event(egui::Event::Text(t.to_string()));
        }
    }

    #[test]
    fn 拖入路径按shell安全格式化() {
        assert_eq!(
            shell_escape_path(Path::new("/tmp/report.txt")),
            "/tmp/report.txt"
        );
        assert_eq!(
            shell_escape_path(Path::new("/tmp/Project Files/app's.app")),
            "'/tmp/Project Files/app'\\''s.app'"
        );
        assert_eq!(
            dropped_paths_text(&[
                PathBuf::from("/tmp/report.txt"),
                PathBuf::from("/tmp/Project Files"),
                PathBuf::from("/Applications/Mino.app"),
            ])
            .as_deref(),
            Some("/tmp/report.txt '/tmp/Project Files' /Applications/Mino.app")
        );
    }

    /// 回归：Finder 拖入文件、目录或应用时，应把路径写到当前终端，
    /// 不自动回车执行，并正确处理包含空格的路径。
    #[test]
    fn 拖入文件目录应用路径写入终端() {
        let session = Session::spawn_local(
            SessionOptions::default(),
            80,
            24,
            Arc::new(|_ev: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let view = Rc::new(RefCell::new(TerminalView::new(session)));
        let view_show = view.clone();
        let mut harness = egui_kittest::Harness::new_ui(move |ui| {
            view_show.borrow_mut().show(ui);
        });
        assert!(wait_text(&view, &mut harness, "mino"), "zsh 未就绪");

        // 先让 egui 记录拖放结束时的鼠标位置；原生 dropped_files 本身不携带坐标。
        let drop_pos = egui::pos2(120.0, 120.0);
        harness.event(egui::Event::PointerMoved(drop_pos));
        harness.step();
        for path in [
            "/tmp/report.txt",
            "/tmp/Project Files",
            "/Applications/Mino.app",
        ] {
            harness
                .input_mut()
                .dropped_files
                .push(Arc::new(TestDroppedFile {
                    path: PathBuf::from(path),
                }));
        }
        harness.step();

        assert!(
            wait_text(&view, &mut harness, "'/tmp/Project Files'")
                && grid_text(view.borrow().session()).contains("/Applications/Mino.app"),
            "拖入路径未写入终端，终端内容：\n{}",
            grid_text(view.borrow().session())
        );
        // 没有发送回车：路径仍在当前输入行中，后续可继续编辑或手动执行。
        let text = grid_text(view.borrow().session());
        assert!(
            text.lines().any(|line| line.contains("/tmp/report.txt")),
            "拖放不应自动执行命令，终端内容：\n{text}"
        );
    }

    /// 鼠标拖动终端网格应建立稳定的选区（回归：终端曾只有键盘焦点，
    /// 任何拖动都不会产生可复制文本）。
    #[test]
    fn 鼠标拖选建立终端选区() {
        let session = Session::spawn_local(
            SessionOptions::default(),
            80,
            24,
            Arc::new(|_ev: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let view = Rc::new(RefCell::new(TerminalView::new(session)));
        let view_show = view.clone();
        let mut harness = egui_kittest::Harness::new_ui(move |ui| {
            view_show.borrow_mut().show(ui);
        });
        assert!(wait_text(&view, &mut harness, "mino"), "zsh 未就绪");

        let start = egui::pos2(12.0, 14.0);
        let end = egui::pos2(150.0, 14.0);
        harness.event(egui::Event::PointerMoved(start));
        harness.event(egui::Event::PointerButton {
            pos: start,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        });
        harness.step();
        harness.event(egui::Event::PointerMoved(end));
        harness.step();
        harness.event(egui::Event::PointerButton {
            pos: end,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        });
        harness.step();

        assert!(view.borrow().selection.is_some(), "拖选后应存在终端选区");
    }

    /// 退格键应删除已输入字符（回归测试：曾出现删除键异常）。
    #[test]
    fn 退格键删除输入字符() {
        let session = Session::spawn_local(
            SessionOptions::default(),
            80,
            24,
            Arc::new(|_ev: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let view = Rc::new(RefCell::new(TerminalView::new(session)));
        let view_show = view.clone();
        let mut harness = egui_kittest::Harness::new_ui(move |ui| {
            view_show.borrow_mut().show(ui);
        });

        // 等待 zsh 提示符出现。
        assert!(
            wait_text(&view, &mut harness, "mino"),
            "zsh 未就绪，终端内容：\n{}",
            grid_text(view.borrow().session())
        );

        // 输入 abc。
        send_key(&mut harness, egui::Key::A, Some("a"));
        send_key(&mut harness, egui::Key::B, Some("b"));
        send_key(&mut harness, egui::Key::C, Some("c"));
        assert!(
            wait_text(&view, &mut harness, "abc"),
            "输入 abc 失败，终端内容：\n{}",
            grid_text(view.borrow().session())
        );

        // 按退格：模拟真实环境的 Key 事件 + 输入法产生的零宽空格 Text 事件。
        send_key(&mut harness, egui::Key::Backspace, Some("\u{200b}"));
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut deleted = false;
        while Instant::now() < deadline {
            harness.step();
            let text = grid_text(view.borrow().session());
            // zsh 回显行应变为 "ab"（末尾 abc → ab），且不应出现多余空格。
            if let Some(line) = text.lines().find(|l| l.ends_with("ab")) {
                if !line.ends_with("abc") {
                    deleted = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        assert!(
            deleted,
            "退格未删除字符（或插入了异常字符），终端内容：\n{}",
            grid_text(view.borrow().session())
        );
    }

    /// 普通字符键不应产生重复或异常字节。
    #[test]
    fn 普通字符单次写入() {
        let session = Session::spawn_local(
            SessionOptions::default(),
            80,
            24,
            Arc::new(|_ev: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let view = Rc::new(RefCell::new(TerminalView::new(session)));
        let view_show = view.clone();
        let mut harness = egui_kittest::Harness::new_ui(move |ui| {
            view_show.borrow_mut().show(ui);
        });
        assert!(wait_text(&view, &mut harness, "mino"), "zsh 未就绪");

        send_key(&mut harness, egui::Key::A, Some("a"));
        assert!(
            wait_text(&view, &mut harness, "a"),
            "字符 a 未显示，终端内容：\n{}",
            grid_text(view.borrow().session())
        );
        // 不应有重复 "aa"：只检查当前输入行（最后一行），
        // 避免被提示符中的主机名（CI 为随机 UUID，可能含 "aa"）误报。
        let text = grid_text(view.borrow().session());
        let last_line = text.lines().last().unwrap_or("");
        assert!(
            !last_line.contains("aa"),
            "字符重复写入，最后一行：{last_line:?}，终端内容：\n{text}"
        );
    }

    /// Tab 属于终端输入，不应被 egui 当作焦点导航键；否则 shell 处理 Tab 后
    /// 终端会短暂失去焦点，紧接着的 Ctrl+C 可能被吞掉。
    #[test]
    fn tab保持终端焦点() {
        let session = Session::spawn_local(
            SessionOptions::default(),
            80,
            24,
            Arc::new(|_ev: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let view = Rc::new(RefCell::new(TerminalView::new(session)));
        let view_show = view.clone();
        let mut harness = egui_kittest::Harness::new_ui(move |ui| {
            view_show.borrow_mut().show(ui);
            // SSH 标签页的终端后面还有悬浮 SFTP 按钮；它是可聚焦控件，
            // 正是远端 Tab 被 egui 焦点导航抢走的实际布局。
            let _ = ui.button("after-terminal");
        });
        assert!(wait_text(&view, &mut harness, "mino"), "zsh 未就绪");

        assert_eq!(
            harness.ctx.memory(|m| m.focused()),
            Some(egui::Id::new("terminal_view")),
            "终端初始应持有焦点"
        );
        harness.event(egui::Event::Key {
            key: egui::Key::Tab,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        harness.step();

        assert_eq!(
            harness.ctx.memory(|m| m.focused()),
            Some(egui::Id::new("terminal_view")),
            "Tab 发送给 shell 后终端焦点不应被 egui 转移"
        );
    }

    /// 向上滚动查看 scrollback 后渲染不得崩溃（回归测试：display_iter 的
    /// scrollback 行是负网格行号，曾 cast 成 usize 触发 capacity overflow 闪退）。
    #[test]
    fn 滚动scrollback后渲染不崩溃() {
        use alacritty_terminal::grid::Scroll;

        let session = Session::spawn_local(
            SessionOptions::default(),
            80,
            24,
            Arc::new(|_ev: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let view = Rc::new(RefCell::new(TerminalView::new(session)));
        let view_show = view.clone();
        let mut harness = egui_kittest::Harness::new_ui(move |ui| {
            view_show.borrow_mut().show(ui);
        });
        assert!(wait_text(&view, &mut harness, "mino"), "zsh 未就绪");

        // 执行 `seq 40` 输出 40 行，超过 24 行视口，产生 scrollback。
        view.borrow().session().write(b"seq 40\r");
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut done = false;
        while Instant::now() < deadline {
            harness.step();
            let text = grid_text(view.borrow().session());
            if text.lines().any(|l| l.trim_end() == "40") {
                done = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        assert!(
            done,
            "seq 40 输出未就绪，终端内容：\n{}",
            grid_text(view.borrow().session())
        );

        // 滚动前视口顶行（seq 输出靠近末尾的数字）。
        let top_before: u32 = grid_text(view.borrow().session())
            .lines()
            .next()
            .and_then(|l| l.trim().parse().ok())
            .unwrap_or(0);

        // 向上滚动 5 行（进入 scrollback，出现负网格行号）。
        {
            let term = view.borrow().session().term();
            let mut guard = term.lock();
            guard.grid_mut().scroll_display(Scroll::Delta(5));
        }

        // 渲染若干帧：修复前负行号 cast 成 usize 后 resize 行缓存会
        // capacity overflow panic（本测试直接失败）。
        for _ in 0..6 {
            harness.step();
        }

        // 滚动后视口顶行应显示更早的输出（数字更小），验证显示行号换算正确。
        let top_after: u32 = grid_text(view.borrow().session())
            .lines()
            .next()
            .and_then(|l| l.trim().parse().ok())
            .unwrap_or(0);
        assert!(
            top_after < top_before,
            "滚动后视口应显示更早的输出行（{top_before} → {top_after}）"
        );
    }
}

#[cfg(test)]
mod deadlock_tests {
    use super::*;
    use mino_core::terminal::{Session, SessionOptions};
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Arc;

    /// 回归测试：滚轮事件不应在 ui.input 闭包内触发 request_repaint（会自死锁 panic）。
    #[test]
    fn 滚轮滚动不死锁() {
        let session = Session::spawn_local(
            SessionOptions::default(),
            80,
            24,
            Arc::new(|_ev: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let view = Rc::new(RefCell::new(TerminalView::new(session)));
        let view_show = view.clone();
        let mut harness = egui_kittest::Harness::new_ui(move |ui| {
            view_show.borrow_mut().show(ui);
        });
        // 跑几帧让 zsh 就绪。
        for _ in 0..6 {
            harness.step();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        // 注入滚轮事件（Point/Line/Page 三种单位）。滚轮只在指针位于终端时处理。
        harness.event(egui::Event::PointerMoved(egui::pos2(100.0, 100.0)));
        harness.step();
        for unit in [
            egui::MouseWheelUnit::Point,
            egui::MouseWheelUnit::Line,
            egui::MouseWheelUnit::Page,
        ] {
            harness.event(egui::Event::MouseWheel {
                unit,
                delta: egui::Vec2::new(0.0, 3.0),
                modifiers: egui::Modifiers::default(),
                phase: egui::TouchPhase::Move,
            });
            harness.step();
            harness.step();
        }
        // 若修复失效，此处会在 10 秒死锁后 panic；到达这里说明通过。
    }
}

#[cfg(test)]
mod mouse_wheel_tests {
    use super::*;

    #[test]
    fn 鼠标上报优先于替代屏和scrollback() {
        assert_eq!(
            wheel_target(
                TermMode::MOUSE_REPORT_CLICK
                    | TermMode::SGR_MOUSE
                    | TermMode::ALT_SCREEN
                    | TermMode::ALTERNATE_SCROLL,
            ),
            WheelTarget::ApplicationMouse
        );
        assert_eq!(
            wheel_target(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL),
            WheelTarget::AlternateScroll
        );
        assert_eq!(wheel_target(TermMode::NONE), WheelTarget::Scrollback);
    }

    #[test]
    fn 小幅point滚轮不会被截断() {
        // 滚轮是“意图”而非距离：任何有效的滚轮事件都只发一次，由 Vim 自己
        // 决定滚动行数；大 delta 不再按像素折算成多次，避免一次手势翻过几屏。
        assert_eq!(mouse_wheel_steps(egui::MouseWheelUnit::Point, 0.25), 1);
        assert_eq!(mouse_wheel_steps(egui::MouseWheelUnit::Point, 80.0), 1);
        assert_eq!(mouse_wheel_steps(egui::MouseWheelUnit::Line, -3.0), 3);
        assert_eq!(mouse_wheel_steps(egui::MouseWheelUnit::Line, -30.0), 3);
        assert_eq!(mouse_wheel_steps(egui::MouseWheelUnit::Page, 1.0), 1);
        assert_eq!(mouse_wheel_steps(egui::MouseWheelUnit::Point, 0.0), 0);
    }
}

#[cfg(test)]
mod enter_tests {
    use super::*;
    use mino_core::terminal::{Session, SessionOptions};
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn grid_text(session: &Session) -> String {
        use alacritty_terminal::term::cell::Flags;
        let term_arc = session.term();
        let guard = term_arc.lock();
        let content = guard.renderable_content();
        let mut lines: Vec<String> = Vec::new();
        let mut current = String::new();
        let mut started = false;
        let mut prev_grid_line: i32 = i32::MIN;
        for item in content.display_iter {
            let cell = item.cell;
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) || cell.flags.contains(Flags::HIDDEN) {
                continue;
            }
            if item.point.line.0 != prev_grid_line {
                if started {
                    lines.push(current.trim_end().to_string());
                }
                current = String::new();
                started = true;
                prev_grid_line = item.point.line.0;
            }
            current.push(cell.c);
        }
        if started {
            lines.push(current.trim_end().to_string());
        }
        lines.join("\n")
    }

    /// 回车应执行已输入的命令（回归测试：用户报告回车不执行）。
    #[test]
    fn 回车执行命令() {
        let session = Session::spawn_local(
            SessionOptions::default(),
            80,
            24,
            Arc::new(|_ev: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let view = Rc::new(RefCell::new(TerminalView::new(session)));
        let view_show = view.clone();
        let mut harness = egui_kittest::Harness::new_ui(move |ui| {
            view_show.borrow_mut().show(ui);
        });

        // 等待 zsh 就绪。
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut ready = false;
        while Instant::now() < deadline {
            harness.step();
            if grid_text(view.borrow().session()).contains("mino") {
                ready = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        assert!(ready, "zsh 未就绪");

        // 输入 echo HELLO。
        for (key, ch) in [
            (egui::Key::E, "e"),
            (egui::Key::C, "c"),
            (egui::Key::H, "h"),
            (egui::Key::O, "o"),
        ] {
            harness.event(egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::default(),
            });
            harness.event(egui::Event::Text(ch.to_string()));
        }
        harness.event(egui::Event::Text(" ".to_string()));
        for (key, ch) in [
            (egui::Key::H, "h"),
            (egui::Key::E, "e"),
            (egui::Key::L, "l"),
            (egui::Key::L, "l"),
            (egui::Key::O, "o"),
        ] {
            harness.event(egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::default(),
            });
            harness.event(egui::Event::Text(ch.to_string()));
        }

        // 按回车。
        harness.event(egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        });

        // 等待 HELLO 输出出现（命令被执行）。
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut executed = false;
        while Instant::now() < deadline {
            harness.step();
            if grid_text(view.borrow().session()).contains("hello") {
                executed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        assert!(
            executed,
            "回车未执行命令，终端内容：\n{}",
            grid_text(view.borrow().session())
        );
    }

    /// 回归：粘贴 cd 会让输入模型失效，随后执行 pwd 仍应以终端实际输出
    /// 校正当前目录，不能继续把 SFTP 定位在启动目录。
    #[test]
    fn pwd输出校正粘贴cd后的目录() {
        let base =
            std::env::temp_dir().join(format!("mino-terminal-pwd-test-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let session = Session::spawn_local(
            SessionOptions {
                working_directory: Some(std::env::temp_dir()),
                ..Default::default()
            },
            80,
            24,
            Arc::new(|_ev: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let view = Rc::new(RefCell::new(TerminalView::new(session)));
        let view_show = view.clone();
        let mut harness = egui_kittest::Harness::new_ui(move |ui| {
            view_show.borrow_mut().show(ui);
        });
        harness.run_steps(12);

        // 用独立输出确认 shell 已经可以接收输入，不能用目录名中的
        // “mino”作为就绪条件（测试临时目录本身也可能含有该字符串）。
        harness.event(egui::Event::Text("printf __MINO_TERMINAL_READY__".into()));
        harness.event(egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        });
        let ready_deadline = Instant::now() + Duration::from_secs(8);
        let mut ready = false;
        while Instant::now() < ready_deadline {
            harness.step();
            if grid_text(view.borrow().session()).contains("__MINO_TERMINAL_READY__") {
                ready = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        assert!(ready, "zsh 未就绪");

        // 粘贴 cd，模拟截图中的“跟踪器此前已经失效”场景。
        harness.event(egui::Event::Paste(format!("cd {}", base.display())));
        harness.event(egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        });
        harness.run_steps(6);

        // 只依赖 pwd 输出恢复，不依赖输入模型重新推导 cd。
        harness.event(egui::Event::Text("pwd".into()));
        harness.event(egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        });
        let expected = std::fs::canonicalize(&base).unwrap();
        let expected_text = expected.to_string_lossy().into_owned();
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut corrected = false;
        while Instant::now() < deadline {
            harness.step();
            if view.borrow().current_directory().as_deref() == Some(expected_text.as_str()) {
                corrected = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        assert!(
            corrected,
            "pwd 输出后目录未校正，当前目录：{:?}",
            view.borrow().current_directory()
        );
        std::fs::remove_dir_all(base).ok();
    }

    /// 回归（用户报告“定位只有 pwd 后才好用”）：输入跟踪失效后，
    /// `request_fresh_pwd` 应自动注入 `pwd` 并把目录校正到真实值，
    /// 不需要用户先手输一次 `pwd`。
    #[test]
    fn 定位自动pwd探测校正目录() {
        let base =
            std::env::temp_dir().join(format!("mino-terminal-auto-pwd-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let session = Session::spawn_local(
            SessionOptions {
                working_directory: Some(std::env::temp_dir()),
                ..Default::default()
            },
            80,
            24,
            Arc::new(|_ev: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let view = Rc::new(RefCell::new(TerminalView::new(session)));
        let view_show = view.clone();
        let mut harness = egui_kittest::Harness::new_ui(move |ui| {
            view_show.borrow_mut().show(ui);
        });
        harness.run_steps(12);

        harness.event(egui::Event::Text("printf __MINO_TERMINAL_READY__".into()));
        harness.event(egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        });
        let ready_deadline = Instant::now() + Duration::from_secs(8);
        let mut ready = false;
        while Instant::now() < ready_deadline {
            harness.step();
            if grid_text(view.borrow().session()).contains("__MINO_TERMINAL_READY__") {
                ready = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        assert!(ready, "zsh 未就绪");

        // 粘贴 cd 让跟踪器失效（与线上“别名/函数/补全后定位不准”同根因）。
        harness.event(egui::Event::Paste(format!("cd {}", base.display())));
        harness.event(egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        });
        harness.run_steps(6);

        // 此时跟踪器仍停在旧目录；定位探测应自动注入 pwd 并校正。
        let before = view.borrow().current_directory();
        let expected = std::fs::canonicalize(&base).unwrap();
        let expected_text = expected.to_string_lossy().into_owned();
        assert_ne!(before.as_deref(), Some(expected_text.as_str()));

        // 探测注入需要经过一帧终端渲染（输出校正管线在 show 内）。
        assert!(view.borrow_mut().request_fresh_pwd());
        assert!(!view.borrow().auto_pwd_ready());
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut corrected = false;
        while Instant::now() < deadline {
            harness.step();
            if view.borrow().auto_pwd_ready()
                && view.borrow().current_directory().as_deref() == Some(expected_text.as_str())
            {
                corrected = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        assert!(
            corrected,
            "自动 pwd 探测后目录未校正，当前目录：{:?}",
            view.borrow().current_directory()
        );
        std::fs::remove_dir_all(base).ok();
    }

    /// 有未执行输入时不得注入 `pwd`（避免污染用户正在编辑的命令行）。
    #[test]
    fn 定位有输入时不注入pwd() {
        use mino_core::terminal::{Session, SessionOptions};
        use std::sync::Arc;

        let session = Session::spawn_local(
            SessionOptions::default(),
            80,
            24,
            Arc::new(|_ev: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let mut view = TerminalView::new(session);
        view.workdir.push_text("echo hi");
        assert!(!view.request_fresh_pwd(), "输入行非空时不应注入 pwd");
        assert!(view.auto_pwd_ready());
    }
}

#[cfg(test)]
mod osc_tests {
    use super::*;

    /// 浅色主题兼容：终端程序（如 zsh 主题）通过 OSC 11 设置深色背景时，
    /// 背景应强制跟随主题色（否则浅色主题下终端仍为深色）。
    #[test]
    #[allow(non_snake_case)]
    fn 背景色忽略OSC覆盖() {
        use alacritty_terminal::term::color::Colors as TermColors;
        use alacritty_terminal::vte::ansi::{Color as TermColor, NamedColor as Named};

        let mut colors = TermColors::default();
        // 模拟 zsh 主题发送 OSC 11 设置深色背景。
        colors[Named::Background] = Some(Rgb {
            r: 0x1a,
            g: 0x1a,
            b: 0x1a,
        });

        // 任意主题下：背景应为主题色，而非 OSC 深色。
        let theme_bg = crate::theme::current_theme().term_bg;
        let resolved = resolve_color(
            TermColor::Named(Named::Background),
            &colors,
            theme_bg,
            false,
        );
        assert_eq!(
            resolved,
            Color32::from_rgb(theme_bg.r, theme_bg.g, theme_bg.b),
            "背景应跟随主题，忽略 OSC 覆盖"
        );

        // 前景仍尊重 OSC（程序控制文字颜色是合理行为）。
        colors[Named::Foreground] = Some(Rgb {
            r: 0x00,
            g: 0xff,
            b: 0x00,
        });
        let resolved_fg = resolve_color(
            TermColor::Named(Named::Foreground),
            &colors,
            theme_bg,
            false,
        );
        assert_eq!(resolved_fg, Color32::from_rgb(0x00, 0xff, 0x00));
    }
}

#[cfg(test)]
mod paste_tests {
    use super::*;

    /// 括号粘贴内容中的伪造结束序列不能提前关闭协议边界。
    #[test]
    fn 括号粘贴移除内嵌转义字符() {
        let payload = bracketed_paste_payload("echo safe\x1b[201~\n下一行");
        assert_eq!(
            payload, "\x1b[200~echo safe[201~\n下一行\x1b[201~",
            "内嵌 ESC 应被移除，换行仍保留"
        );
        assert_eq!(
            payload.matches("\x1b[201~").count(),
            1,
            "载荷中只能保留由终端生成的结束标记"
        );
    }
}

#[cfg(test)]
mod background_tests {
    use super::*;

    /// 同色背景被默认背景隔开时不能跨越中间列合并。
    #[test]
    fn 背景段只合并相邻列() {
        let default = Color32::BLACK;
        let accent = Color32::from_rgb(10, 20, 30);
        let mut backgrounds = Vec::new();
        push_background(&mut backgrounds, 0, accent, default);
        push_background(&mut backgrounds, 1, default, default);
        push_background(&mut backgrounds, 2, accent, default);

        assert_eq!(backgrounds.len(), 2);
        assert_eq!((backgrounds[0].start, backgrounds[0].end), (0, 1));
        assert_eq!((backgrounds[1].start, backgrounds[1].end), (2, 3));
    }

    /// 显式背景变化必须使行缓存指纹变化，即使文本和前景完全相同。
    #[test]
    fn 背景色参与行指纹() {
        let fg = Color32::WHITE;
        let first = style_key(fg, Color32::BLACK, false, false, false, false);
        let second = style_key(fg, Color32::from_rgb(1, 2, 3), false, false, false, false);
        assert_ne!(first, second);
    }
}

#[cfg(test)]
mod cell_semantics_tests {
    use super::*;
    use alacritty_terminal::grid::Grid;
    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::term::cell::Cell;

    fn selection(start: (i32, usize), end: (i32, usize)) -> TerminalSelection {
        TerminalSelection {
            anchor: SelectionPoint {
                grid_line: start.0,
                col: start.1,
            },
            focus: SelectionPoint {
                grid_line: end.0,
                col: end.1,
            },
        }
    }

    #[test]
    fn 组合字符随主字符渲染和复制() {
        let mut grid = Grid::<Cell>::new(2, 6, 0);
        grid[Line(0)][Column(0)].c = 'e';
        grid[Line(0)][Column(0)].push_zerowidth('\u{301}');
        grid[Line(0)][Column(1)].c = 'x';

        let text = selection_to_text(&grid, selection((0, 0), (0, 1)), 6);
        assert_eq!(text, "e\u{301}x");
    }

    #[test]
    fn 隐藏字符保留等宽空白() {
        let mut grid = Grid::<Cell>::new(2, 6, 0);
        grid[Line(0)][Column(0)].c = 'a';
        grid[Line(0)][Column(1)].c = 'x';
        grid[Line(0)][Column(1)].flags.insert(Flags::HIDDEN);
        grid[Line(0)][Column(2)].c = 'b';

        let text = selection_to_text(&grid, selection((0, 0), (0, 2)), 6);
        assert_eq!(text, "a b");
    }

    #[test]
    fn 软换行不插入额外换行符() {
        let mut grid = Grid::<Cell>::new(2, 4, 0);
        for (column, c) in "abcd".chars().enumerate() {
            grid[Line(0)][Column(column)].c = c;
        }
        grid[Line(0)][Column(3)].flags.insert(Flags::WRAPLINE);
        grid[Line(1)][Column(0)].c = 'e';

        let text = selection_to_text(&grid, selection((0, 0), (1, 0)), 4);
        assert_eq!(text, "abcde");
    }

    #[test]
    fn 局部选择保留有意义的尾随空格() {
        let mut grid = Grid::<Cell>::new(2, 6, 0);
        grid[Line(0)][Column(0)].c = 'a';
        grid[Line(0)][Column(1)].c = ' ';
        grid[Line(0)][Column(2)].c = 'b';

        let text = selection_to_text(&grid, selection((0, 0), (0, 1)), 6);
        assert_eq!(text, "a ");
    }

    #[test]
    fn 过期选区越界时安全返回空串() {
        // 选区是建立时的 grid_line 快照；resize/scrollback 裁剪后网格缩小，
        // 快照可能悬空。alacritty Storage 越界防护仅 debug_assert，
        // release 下索引越界会 panic——越界时须安全返回空串（放弃复制）。
        let grid = Grid::<Cell>::new(2, 6, 0);
        // 网格只有 2 行可视 + 0 行 scrollback，快照却引用第 5 行。
        assert_eq!(selection_to_text(&grid, selection((5, 0), (5, 1)), 6), "");
        // 快照引用 scrollback 深处（history=0 时负行号同样越界）。
        assert_eq!(selection_to_text(&grid, selection((-3, 0), (-3, 1)), 6), "");
        // 列快照为空或大于当前网格宽度时，同样放弃复制，避免 Column(cols - 1) 越界。
        assert_eq!(selection_to_text(&grid, selection((0, 0), (0, 1)), 0), "");
        assert_eq!(selection_to_text(&grid, selection((0, 0), (0, 1)), 7), "");
    }

    #[test]
    fn 渲染器保留隐藏与跨行宽字符列位() {
        let theme = crate::theme::current_theme();
        let mut grid = Grid::<Cell>::new(1, 4, 0);
        grid[Line(0)][Column(0)].c = 'a';
        grid[Line(0)][Column(1)].c = 'x';
        grid[Line(0)][Column(1)].flags.insert(Flags::HIDDEN);
        grid[Line(0)][Column(2)].c = 'b';
        grid[Line(0)][Column(3)]
            .flags
            .insert(Flags::LEADING_WIDE_CHAR_SPACER);

        let data = build_line_data(
            &grid,
            0,
            4,
            &Colors::default(),
            theme.term_fg,
            theme.term_bg,
            to_egui(theme.term_bg),
        );
        let text: String = data
            .segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect();
        assert_eq!(text, "a b ");
    }
}

#[cfg(test)]
mod ime_backspace_tests {
    use super::*;
    use mino_core::terminal::{Session, SessionOptions};
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn grid_text(session: &Session) -> String {
        use alacritty_terminal::term::cell::Flags;
        let term_arc = session.term();
        let guard = term_arc.lock();
        let content = guard.renderable_content();
        let mut lines: Vec<String> = Vec::new();
        let mut current = String::new();
        let mut started = false;
        let mut prev_grid_line: i32 = i32::MIN;
        for item in content.display_iter {
            let cell = item.cell;
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) || cell.flags.contains(Flags::HIDDEN) {
                continue;
            }
            if item.point.line.0 != prev_grid_line {
                if started {
                    lines.push(current.trim_end().to_string());
                }
                current = String::new();
                started = true;
                prev_grid_line = item.point.line.0;
            }
            current.push(cell.c);
        }
        if started {
            lines.push(current.trim_end().to_string());
        }
        lines.join("\n")
    }

    fn send_key(harness: &mut egui_kittest::Harness, key: egui::Key, text: Option<&str>) {
        harness.event(egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        });
        if let Some(t) = text {
            harness.event(egui::Event::Text(t.to_string()));
        }
    }

    /// 回归测试（用户报告"删除键插入空格"）：输入法（如微信输入法）在退格时
    /// 伴随发送"空格" Text 事件，不应插入空格。
    #[test]
    fn 退格伴随空格文本不插入() {
        let session = Session::spawn_local(
            SessionOptions::default(),
            80,
            24,
            Arc::new(|_ev: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let view = Rc::new(RefCell::new(TerminalView::new(session)));
        let view_show = view.clone();
        let mut harness = egui_kittest::Harness::new_ui(move |ui| {
            view_show.borrow_mut().show(ui);
        });

        // 等 zsh 就绪。
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut ready = false;
        while Instant::now() < deadline {
            harness.step();
            if grid_text(view.borrow().session()).contains("mino") {
                ready = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        assert!(ready, "zsh 未就绪");

        // 输入 abc。
        send_key(&mut harness, egui::Key::A, Some("a"));
        send_key(&mut harness, egui::Key::B, Some("b"));
        send_key(&mut harness, egui::Key::C, Some("c"));
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut typed = false;
        while Instant::now() < deadline {
            harness.step();
            if grid_text(view.borrow().session()).contains("abc") {
                typed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        assert!(typed, "输入 abc 失败");

        // 退格（伴随空格 Text——输入法产物）。
        send_key(&mut harness, egui::Key::Backspace, Some(" "));
        // 等待 zsh 回显更新为 ab。
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut deleted = false;
        while Instant::now() < deadline {
            harness.step();
            let text = grid_text(view.borrow().session());
            if text.lines().any(|l| l.ends_with("ab")) {
                deleted = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        assert!(
            deleted,
            "退格后应为 ab，终端内容：\n{}",
            grid_text(view.borrow().session())
        );

        // 不得出现"ab "（退格伴随的空格被丢弃）。
        let text = grid_text(view.borrow().session());
        let last_line = text.lines().last().unwrap_or("");
        assert!(
            !last_line.contains("ab "),
            "退格不应插入空格，最后一行：{last_line:?}"
        );
    }

    #[test]
    fn 修饰键回退映射保留移位符号() {
        assert_eq!(map_char_key(&egui::Key::Num1, true), Some(Key::Char('!')));
        assert_eq!(map_char_key(&egui::Key::Minus, true), Some(Key::Char('_')));
        assert_eq!(
            map_char_key(&egui::Key::OpenBracket, true),
            Some(Key::Char('{'))
        );
        assert_eq!(map_char_key(&egui::Key::Slash, true), Some(Key::Char('?')));
        assert_eq!(map_char_key(&egui::Key::Num1, false), Some(Key::Char('1')));
    }
}
