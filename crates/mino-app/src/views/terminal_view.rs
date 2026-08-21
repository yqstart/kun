//! 终端视图：cell 渲染、键盘输入转发、滚动。
//!
//! 渲染为**行级增量**：每帧用 `Term::damage()` 拿到终端损坏行集合（行号即显示行号），
//! 只对损坏/新出现/滚入的行重建文本段与 Galley（已布局文本），其余行直接复用缓存
//! Galley 绘制（零扫描、零 layout）。内容未变的帧（PTY 空转、光标闪烁、无输入）仅
//! 绘制已有 Galley。

use std::cmp::Ordering;
use std::collections::HashMap;
use std::time::Duration;

use alacritty_terminal::grid::Scroll;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::TermDamage;
use alacritty_terminal::vte::ansi::{Color as AColor, CursorShape, NamedColor, Rgb};
use egui::text::LayoutJob;
use egui::{Color32, FontId, Rect, Stroke, TextFormat, Ui, Vec2};
use mino_core::terminal::keys::{self, Key, Mods};
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
    /// 补全输入模型（本地会话启用；远程会话恒失效）。
    input: crate::completion::InputModel,
    /// 远程会话的初始目录（由 SFTP realpath(".") 提供）。
    remote_home: Option<std::path::PathBuf>,
    /// 当前补全候选。
    candidates: Vec<crate::completion::Candidate>,
    /// 候选选中索引。
    candidate_selected: usize,
    /// 光标屏幕坐标（补全浮层定位）。
    cursor_pos: Option<egui::Pos2>,
    /// 上一帧终端是否持有焦点（焦点自动恢复用）。
    had_focus: bool,
    /// 分段耗时打点（性能 HUD 读数；默认不共享，仅本视图内部使用）。
    last_build_ms: f32,
    last_layout_ms: f32,
    last_paint_ms: f32,
    /// 上次补全候选重算时刻（去抖：连续打字不重复 read_dir/metadata）。
    last_recompute: std::time::Instant,
    /// 去抖期间挂起的重算请求（到点后由 show() 执行）。
    recompute_pending: bool,
    /// 上次重算时的输入快照（同快照不重算，退格后恢复场景）。
    last_recompute_text: String,
    /// 会话标题缓存（`SessionEvent::Title` 时更新，避免每帧 Mutex + String clone）。
    cached_title: String,
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
        // 本地会话初始工作目录：会话启动目录（HOME）；远程不启用补全。
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
            cols: 80,
            rows: 24,
            last_ppp: 0.0,
            last_theme_revision: crate::theme::theme_revision(),
            focus_id: egui::Id::new("terminal_view"),
            initialized: false,
            last_mode: TermMode::NONE,
            suppress_blank_frames: 0,
            input: crate::completion::InputModel::new(cwd),
            remote_home: None,
            candidates: Vec::new(),
            candidate_selected: 0,
            cursor_pos: None,
            had_focus: false,
            last_build_ms: 0.0,
            last_layout_ms: 0.0,
            last_paint_ms: 0.0,
            last_recompute: std::time::Instant::now(),
            recompute_pending: false,
            last_recompute_text: String::new(),
            cached_title,
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
        Some(self.input.cwd.to_string_lossy().into_owned())
    }

    /// 设置远程会话的初始工作目录。
    pub fn set_remote_current_directory(&mut self, path: &str) {
        if path.is_empty() {
            return;
        }
        let cwd = std::path::PathBuf::from(path);
        self.remote_home = Some(cwd.clone());
        self.input.set_cwd(cwd);
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
        // 低对比网格与右上角柔光：提供科技感的空间层次，但不干扰终端文本。
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
        crate::anim::paint_glow(
            ui.painter(),
            egui::pos2(outer.right() - 24.0, outer.top() + 20.0),
            150.0,
            theme.accent2.gamma_multiply(0.35),
        );
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

        // 补全去抖：挂起的重算到点后执行（需请求一帧重绘驱动）。
        if self.recompute_pending {
            const DEBOUNCE: Duration = Duration::from_millis(120);
            if self.last_recompute.elapsed() >= DEBOUNCE {
                self.recompute_candidates();
                // 重算可能产生候选，浮层渲染依赖本帧，无需额外 repaint；
                // 若结果为空则关闭浮层，本帧已完成。
            } else {
                ctx.request_repaint_after(DEBOUNCE - self.last_recompute.elapsed().min(DEBOUNCE));
            }
        }

        // ==================== 尺寸计算与 resize ====================
        // cell 尺寸只依赖字体（启动时加载），缓存到字段避免每帧 fonts_mut。
        let ppp = ui.ctx().pixels_per_point();
        if self.cell_width == 0.0 || ppp != self.last_ppp {
            self.last_ppp = ppp;
            let (cell_width, cell_height) = ui.fonts_mut(|f| {
                let font = FontId::monospace(self.font_size);
                (f.glyph_width(&font, ' '), f.row_height(&font))
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
        if cols as u16 != self.cols || rows as u16 != self.rows {
            self.cols = cols as u16;
            self.rows = rows as u16;
            // 通知 PTY 并同步终端状态机网格（Session::resize 内部完成锁内 resize）。
            self.session.resize(self.cols, self.rows);
            self.rows_cache.clear();
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
                // 光标屏幕位置（补全浮层定位：光标行底部）。
                self.cursor_pos = Some(egui::pos2(
                    inner.min.x + cursor.point.column.0 as f32 * cell_width,
                    inner.min.y + (disp_line as f32 + 1.0) * cell_height,
                ));
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
        // 终端是一个整体的键盘控件，Tab/方向键/Esc 都应交给 shell 或补全
        // 状态处理，不能触发 egui 的控件焦点导航。否则远程 shell 执行
        // Tab 补全后，终端会失去焦点，紧接的 Ctrl+C 可能被 UI 吞掉。
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
        if ui.memory(|m| m.has_focus(self.focus_id)) {
            self.handle_input(ui);
        }
    }

    /// 处理键盘与鼠标输入（转发到 PTY / 网格滚动）。
    fn handle_input(&mut self, ui: &Ui) {
        let session = &self.session;
        let mode = self.last_mode;
        let cell_height = self.cell_height;
        let ctx = ui.ctx().clone();
        // 滚动后需要重绘；不能在 ui.input 闭包内调用 request_repaint
        // （Context 锁已被 input 持有，会自死锁 10 秒后 panic），用 flag 延后。
        let mut need_repaint = false;
        // 本帧输入动作（闭包内只读 self 写入 PTY，闭包外统一同步补全模型）。
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
        let suppress_blank_text = self.suppress_blank_frames > 0 || backspace_this_frame;
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
                        // 补全菜单打开时：Tab 确认、↑/↓ 选择、Esc 关闭（不转发给 shell）。
                        if !self.candidates.is_empty() {
                            match key {
                                egui::Key::Tab => {
                                    actions.push(InputAction::AcceptCompletion);
                                    continue;
                                }
                                egui::Key::ArrowUp => {
                                    actions.push(InputAction::SelectUp);
                                    continue;
                                }
                                egui::Key::ArrowDown => {
                                    actions.push(InputAction::SelectDown);
                                    continue;
                                }
                                egui::Key::Escape => {
                                    actions.push(InputAction::CloseMenu);
                                    continue;
                                }
                                _ => {}
                            }
                        }
                        let mods = Mods {
                            shift: modifiers.shift,
                            alt: modifiers.alt,
                            ctrl: modifiers.ctrl,
                            super_: false,
                        };
                        // Ctrl/Alt 修饰的字母与符号键：直接编码为控制字符/转义前缀
                        // （egui 0.36 的 Text 事件与 Key 事件独立，这里处理并让 Text 事件跳过）。
                        if mods.ctrl || mods.alt {
                            if let Some(k) = map_char_key(key) {
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
                        let lines = match unit {
                            egui::MouseWheelUnit::Point => (delta.y / (cell_height * 3.0)) as i32,
                            egui::MouseWheelUnit::Line => delta.y as i32,
                            egui::MouseWheelUnit::Page => {
                                let term_arc = session.term();
                                let mut guard = term_arc.lock();
                                if delta.y > 0.0 {
                                    guard.grid_mut().scroll_display(Scroll::PageUp);
                                } else {
                                    guard.grid_mut().scroll_display(Scroll::PageDown);
                                }
                                need_repaint = true;
                                0
                            }
                        };
                        if lines != 0 {
                            let term_arc = session.term();
                            let mut guard = term_arc.lock();
                            let grid = guard.grid_mut();
                            if modifiers.alt {
                                if lines > 0 {
                                    grid.scroll_display(Scroll::PageUp);
                                } else {
                                    grid.scroll_display(Scroll::PageDown);
                                }
                            } else {
                                grid.scroll_display(Scroll::Delta(lines));
                            }
                            need_repaint = true;
                        }
                    }
                    _ => {}
                }
            }
        });

        if need_repaint {
            ctx.request_repaint();
        }

        // 闭包外统一应用输入动作，同步补全模型。
        for action in actions {
            self.apply_input_action(action, &ctx);
        }
        // 渲染补全浮层（本地会话）。
        if !self.candidates.is_empty() {
            self.render_completion_ghost(ui);
            self.render_completion_popup(ui);
        }
        self.render_copy_feedback(ui);
    }

    /// 应用一帧内的输入动作（写入 PTY 的字节与拦截的按键）。
    fn apply_input_action(&mut self, action: InputAction, ctx: &egui::Context) {
        match action {
            InputAction::Bytes(bytes) => self.track_input_bytes(&bytes),
            InputAction::Text(text) => {
                self.input.push_text(&text);
                self.request_recompute();
            }
            InputAction::Paste => {
                // 粘贴内容不可逐字节信任，模型失效禁用补全直到回车。
                self.input.invalidate();
                self.candidates.clear();
            }
            InputAction::AcceptCompletion => self.accept_completion(),
            InputAction::SelectUp => {
                self.candidate_selected = self.candidate_selected.saturating_sub(1);
            }
            InputAction::SelectDown => {
                let n = self.candidates.len().saturating_sub(1);
                self.candidate_selected = (self.candidate_selected + 1).min(n);
            }
            InputAction::CloseMenu => {
                self.candidates.clear();
                self.candidate_selected = 0;
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

    /// 分析写入 PTY 的字节并同步输入模型（本地与远程会话）。
    fn track_input_bytes(&mut self, bytes: &[u8]) {
        match bytes {
            // 回车：执行命令（解析 cd），清空输入，模型恢复可靠。
            b"\r" | b"\n" => {
                if self.session.is_remote() {
                    self.input.execute_remote(self.remote_home.as_deref());
                } else {
                    self.input.execute();
                }
                self.candidates.clear();
            }
            // Ctrl+C：重置当前行。
            b"\x03" => {
                self.input.reset();
                self.candidates.clear();
            }
            // 退格/删除。
            b"\x7f" | b"\x08" => {
                self.input.backspace();
                self.request_recompute();
            }
            // Tab（shell 自身补全/移动光标）：输入行已被 shell 改写（zsh 菜单
            // 补全会原地扩展命令），模型无法追踪，失效禁用浮层避免给出错候选。
            b"\t" => {
                self.input.invalidate();
                self.candidates.clear();
            }
            _ => {
                // 可见文本（ASCII 可打印 / 空格 / 非 ASCII）。
                if let Ok(s) = std::str::from_utf8(bytes) {
                    if s.chars()
                        .all(|c| c.is_ascii_graphic() || c == ' ' || !c.is_ascii())
                    {
                        self.input.push_text(s);
                        self.request_recompute();
                        return;
                    }
                }
                // 控制序列/编辑键（箭头、Ctrl+U/W 等）：光标位置不可追踪，模型失效。
                self.input.invalidate();
                self.candidates.clear();
            }
        }
    }

    /// 请求重新计算补全候选（路径候选去抖）。
    ///
    /// 命令候选是内存索引扫描（`command_index` 进程级缓存），毫秒级，立即算；
    /// 路径候选每次都要 `read_dir` + 逐项 `metadata()` 同步 syscall
    /// （大目录下卡顿），连续打字时去抖 120ms 只重算一次，去抖期间挂起
    /// 由 show() 到点执行。
    fn request_recompute(&mut self) {
        if self.session.is_remote() {
            self.candidates.clear();
            return;
        }
        let (word_start, word) = crate::completion::last_word(&self.input.text);
        let is_command_pos = word_start == 0 && !word.contains('/');
        if is_command_pos && !word.is_empty() {
            self.recompute_candidates();
            return;
        }
        const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(120);
        if self.last_recompute.elapsed() >= DEBOUNCE {
            self.recompute_candidates();
        } else {
            self.recompute_pending = true;
        }
    }

    /// 实际执行候选重算（含同输入快照跳过：退格后恢复原文本等场景不重复 read_dir）。
    fn recompute_candidates(&mut self) {
        if self.session.is_remote() {
            self.candidates.clear();
            return;
        }
        self.last_recompute = std::time::Instant::now();
        self.recompute_pending = false;
        if self.input.text == self.last_recompute_text {
            return;
        }
        self.last_recompute_text = self.input.text.clone();
        self.candidates = crate::completion::compute_candidates(
            &self.input,
            crate::completion::command_index(),
            8,
        );
        self.candidate_selected = 0;
    }

    /// 用选中候选替换输入中的当前 word（发送退格 + 候选文本到 PTY）。
    fn accept_completion(&mut self) {
        let Some(c) = self.candidates.get(self.candidate_selected).cloned() else {
            return;
        };
        let (word_start, word) = crate::completion::last_word(&self.input.text);
        // shell 中删除旧 word（按字符退格）再写入补全文本。
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend(std::iter::repeat_n(b'\x7f', word.chars().count()));
        bytes.extend_from_slice(c.text.as_bytes());
        self.session.write(&bytes);
        // 同步模型。
        self.input.text.truncate(word_start);
        self.input.text.push_str(&c.text);
        self.candidates.clear();
        self.candidate_selected = 0;
    }

    /// 渲染补全浮层（输入行上方，Warp 风格候选列表）。
    fn render_completion_popup(&mut self, ui: &Ui) {
        let theme = crate::theme::current_theme();
        let Some(cursor_pos) = self.cursor_pos else {
            return;
        };
        let inner = ui.max_rect().shrink(PADDING);
        let popup_w = 326.0;
        let row_h = 30.0;
        let rows = self.candidates.len().min(8) as f32;
        // 估算高度与实际 header / footer / row 节奏一致，避免浮层定位和真实
        // 尺寸不一致时出现遮挡输入行或上下跳动。
        let popup_h = 42.0 + rows * row_h + 30.0 + 20.0;
        // 锚定：上方空间充足时浮层底边固定在光标行顶上方 4px（实际渲染多高都
        // 向上生长，绝不遮输入行）；不足时顶边固定在光标行底下方 4px。
        let (anchor, offset) = completion_popup_anchor(
            cursor_pos,
            self.cell_height,
            egui::vec2(popup_w, popup_h),
            inner,
            ui.ctx().content_rect(),
        );
        egui::Area::new(egui::Id::new("completion_popup"))
            .order(egui::Order::Foreground)
            // Warp 风格浮层允许鼠标点选；接受后重新把焦点交还终端。
            .interactable(true)
            .anchor(anchor, offset)
            .show(ui.ctx(), |ui| {
                egui::Frame::new()
                    .fill(theme.bg_panel)
                    .stroke(egui::Stroke::new(1.0, theme.accent2.gamma_multiply(0.65)))
                    .corner_radius(10.0)
                    .inner_margin(egui::Margin::symmetric(10, 10))
                    .show(ui, |ui| {
                        ui.set_width(popup_w - 20.0);
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 7.0;
                            ui.label(
                                egui::RichText::new("SUGGESTIONS")
                                    .monospace()
                                    .size(9.0)
                                    .color(theme.accent),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "{:02} MATCHES",
                                            self.candidates.len().min(8)
                                        ))
                                        .monospace()
                                        .size(9.0)
                                        .color(theme.text_muted),
                                    );
                                },
                            );
                        });
                        ui.add_space(8.0);
                        let mut clicked: Option<usize> = None;
                        for (i, c) in self.candidates.iter().enumerate().take(8) {
                            let selected = i == self.candidate_selected;
                            let (color, marker) = match c.kind {
                                // 标记用 ASCII/常用字符（Proportional 字体无 Menlo fallback，
                                // ⚙ 等符号有缺字形风险）。
                                crate::completion::CandidateKind::Command => (theme.accent2, "$"),
                                crate::completion::CandidateKind::Dir => (theme.accent, ">"),
                                crate::completion::CandidateKind::File => {
                                    (theme.text_secondary, "·")
                                }
                            };
                            let (row_rect, row) = ui.allocate_exact_size(
                                egui::vec2(popup_w - 20.0, row_h),
                                egui::Sense::click(),
                            );
                            if row.hovered() {
                                self.candidate_selected = i;
                            }
                            if selected || row.hovered() {
                                ui.painter().rect_filled(
                                    row_rect,
                                    crate::theme::tokens::RADIUS_ITEM,
                                    if selected {
                                        theme.accent_soft
                                    } else {
                                        theme.bg_elevated.gamma_multiply(0.85)
                                    },
                                );
                            }
                            if selected {
                                ui.painter().rect_filled(
                                    egui::Rect::from_min_max(
                                        egui::pos2(row_rect.left(), row_rect.top()),
                                        egui::pos2(row_rect.left() + 2.0, row_rect.bottom()),
                                    ),
                                    1.0,
                                    theme.accent,
                                );
                            }
                            let mut row_ui = ui.new_child(
                                egui::UiBuilder::new()
                                    .max_rect(row_rect.shrink2(egui::vec2(10.0, 0.0)))
                                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
                            );
                            row_ui.spacing_mut().item_spacing.x = 9.0;
                            row_ui.label(
                                egui::RichText::new(marker).size(12.0).color(if selected {
                                    theme.text_primary
                                } else {
                                    color
                                }),
                            );
                            row_ui.label(
                                egui::RichText::new(&c.display)
                                    .size(13.0)
                                    .color(if selected { theme.text_primary } else { color }),
                            );
                            row_ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(
                                        egui::RichText::new(match c.kind {
                                            crate::completion::CandidateKind::Command => "CMD",
                                            crate::completion::CandidateKind::Dir => "DIR",
                                            crate::completion::CandidateKind::File => "FILE",
                                        })
                                        .monospace()
                                        .size(8.0)
                                        .color(theme.text_muted),
                                    );
                                },
                            );
                            if row.clicked() {
                                clicked = Some(i);
                            }
                        }
                        ui.add_space(7.0);
                        ui.painter().line_segment(
                            [
                                ui.cursor().left_top(),
                                egui::pos2(ui.max_rect().right(), ui.cursor().top()),
                            ],
                            egui::Stroke::new(1.0, theme.border),
                        );
                        ui.add_space(6.0);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                egui::RichText::new("↑ ↓ 选择   Tab 补全   Esc 关闭")
                                    .monospace()
                                    .size(8.5)
                                    .color(theme.text_muted),
                            );
                        });
                        if let Some(i) = clicked {
                            self.candidate_selected = i;
                            self.accept_completion();
                            ui.memory_mut(|m| m.request_focus(self.focus_id));
                        }
                    });
            });
    }

    /// 在真实 shell 光标右侧绘制第一候选的幽灵后缀，减少“只弹一个列表”的
    /// 鸡肋感；Tab 仍然是明确提交，Enter 继续执行 shell 当前命令。
    fn render_completion_ghost(&self, ui: &Ui) {
        let Some(cursor_pos) = self.cursor_pos else {
            return;
        };
        let Some(candidate) = self.candidates.first() else {
            return;
        };
        if candidate.kind != crate::completion::CandidateKind::Command {
            return;
        }
        let (_, word) = crate::completion::last_word(&self.input.text);
        let Some(suffix) = candidate.text.strip_prefix(word) else {
            return;
        };
        if suffix.is_empty() {
            return;
        }
        ui.painter().text(
            egui::pos2(cursor_pos.x, cursor_pos.y - self.cell_height),
            egui::Align2::LEFT_TOP,
            suffix,
            FontId::monospace(self.font_size),
            crate::theme::current_theme()
                .text_muted
                .gamma_multiply(0.62),
        );
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

/// 计算补全浮层的锚定方式（独立函数便于单测）。
///
/// `cursor` 为光标行**底部**坐标。返回 `(anchor, offset)`，供 `Area::anchor` 使用
/// （offset 相对屏幕内容区 `content` 的对应锚角）：
/// - 上方空间充足时返回 `LEFT_BOTTOM` 锚定：浮层底边固定在光标行顶上方 4px，
///   实际渲染多高都向上生长，**与浮层真实尺寸无关，绝不遮挡输入行**；
/// - 不足时返回 `LEFT_TOP` 锚定：顶边固定在光标行底下方 4px，向下生长。
///
/// x 方向以光标为轴向左偏移并限制在终端内容区内。
fn completion_popup_anchor(
    cursor: egui::Pos2,
    cell_height: f32,
    popup_size: egui::Vec2,
    inner: egui::Rect,
    content: egui::Rect,
) -> (egui::Align2, egui::Vec2) {
    let x = (cursor.x - popup_size.x * 0.35).clamp(
        inner.left(),
        (inner.right() - popup_size.x - 4.0).max(inner.left()),
    );
    if cursor.y - cell_height - popup_size.y - 4.0 >= inner.top() {
        // 上方模式：浮层底边锚定在光标行顶上方 4px。
        let offset = egui::vec2(
            x - content.left(),
            (cursor.y - cell_height - 4.0) - content.bottom(),
        );
        (egui::Align2::LEFT_BOTTOM, offset)
    } else {
        // 下方模式：浮层顶边锚定在光标行底下方 4px。
        let offset = egui::vec2(x - content.left(), (cursor.y + 4.0) - content.top());
        (egui::Align2::LEFT_TOP, offset)
    }
}

/// 一帧内的终端输入动作（闭包内收集，闭包外统一应用到补全模型）。
enum InputAction {
    /// 已写入 PTY 的字节。
    Bytes(Vec<u8>),
    /// 已写入的可见文本。
    Text(String),
    /// 粘贴（模型失效）。
    Paste,
    /// Tab 确认补全（拦截，不转发 shell）。
    AcceptCompletion,
    /// 候选上移。
    SelectUp,
    /// 候选下移。
    SelectDown,
    /// 关闭补全浮层。
    CloseMenu,
    /// 复制当前终端选区。
    CopySelection,
}

// ==================== 辅助函数 ====================

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

/// 从网格中提取选区文本，去掉每行末尾用于填充终端宽度的空格。
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
    let mut output = String::new();
    for grid_line in start.grid_line..=end.grid_line {
        let Some((from, to)) = selection.columns_for_line(grid_line, cols) else {
            continue;
        };
        let row = &grid[if grid_line >= 0 {
            alacritty_terminal::index::Line::from(grid_line as usize)
        } else {
            alacritty_terminal::index::Line::from(0) - grid_line.unsigned_abs() as usize
        }];
        let mut line = String::new();
        for (col, cell) in row.into_iter().enumerate().take(to).skip(from) {
            if col >= cols
                || cell.flags.contains(Flags::WIDE_CHAR_SPACER)
                || cell.flags.contains(Flags::HIDDEN)
            {
                continue;
            }
            line.push(cell.c);
        }
        output.push_str(line.trim_end());
        if grid_line != end.grid_line {
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
        // 占位格只参与背景合并，不进入文本。
        let is_spacer =
            cell.flags.contains(Flags::WIDE_CHAR_SPACER) || cell.flags.contains(Flags::HIDDEN);
        if is_spacer {
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) && !cell.flags.contains(Flags::HIDDEN) {
                // 宽字符占位格继承前一格的背景，合并背景段。
                let bg = resolve_color(cell.bg, colors, default_bg, false);
                push_background(&mut backgrounds, col, bg, default_bg_egui);
                mix_cell_hash(
                    &mut hash,
                    cell.c,
                    CellStyle {
                        fg: resolve_color(cell.fg, colors, default_fg, false),
                        bg,
                        bold: false,
                        italic: false,
                        underline: false,
                        strikeout: false,
                    },
                );
            }
            continue;
        }

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

        // 文本段合并。
        push_or_merge(
            &mut segments,
            cell.c,
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
fn push_or_merge(segments: &mut Vec<Segment>, c: char, style: CellStyle, hash: &mut u64) {
    if let Some(last) = segments.last_mut() {
        if last.fg == style.fg
            && last.bold == style.bold
            && last.italic == style.italic
            && last.underline == style.underline
            && last.strikeout == style.strikeout
        {
            last.text.push(c);
            mix_cell_hash(hash, c, style);
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
    mix_cell_hash(hash, c, style);
}

/// 将影响行绘制的 cell 属性加入缓存指纹。
fn mix_cell_hash(hash: &mut u64, c: char, style: CellStyle) {
    *hash = hash.wrapping_mul(131).wrapping_add(style.key());
    *hash = hash.wrapping_mul(131).wrapping_add(c as u64);
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

/// 判断字符是否可安全写入终端（过滤控制符/私有区/零宽字符）。
fn is_printable_text_char(c: char) -> bool {
    !c.is_ascii_control()
        && !('\u{e000}'..='\u{f8ff}').contains(&c) // 私有使用区
        && !('\u{200b}'..='\u{200f}').contains(&c) // 零宽空格/左右连接符
        && !('\u{2060}'..='\u{2064}').contains(&c) // 单词连接符等
        && !('\u{fe00}'..='\u{fe0f}').contains(&c) // 变体选择符
        && c != '\u{feff}' // BOM/零宽不换行空格
}

/// egui 键 → 终端字符键（仅无文本时兜底使用）。
fn map_char_key(key: &egui::Key) -> Option<Key> {
    use egui::Key as E;
    let v = *key as u8;
    // 字母与数字键（枚举判别值连续，按声明顺序）。
    if (E::A as u8..=E::Z as u8).contains(&v) {
        return Some(Key::Char((v - E::A as u8 + b'a') as char));
    }
    if (E::Num0 as u8..=E::Num9 as u8).contains(&v) {
        return Some(Key::Char((v - E::Num0 as u8 + b'0') as char));
    }
    Some(Key::Char(match key {
        E::Space => ' ',
        E::Minus => '-',
        E::Equals => '=',
        E::Comma => ',',
        E::Period => '.',
        E::Slash => '/',
        E::Semicolon => ';',
        E::Quote => '\'',
        E::Backtick => '`',
        E::Backslash => '\\',
        E::OpenBracket => '[',
        E::CloseBracket => ']',
        E::Colon => ':',
        E::Plus => '+',
        E::Pipe => '|',
        E::Questionmark => '?',
        E::Exclamationmark => '!',
        E::OpenCurlyBracket => '{',
        E::CloseCurlyBracket => '}',
        _ => return None,
    }))
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

#[cfg(test)]
mod tests {
    use super::*;
    use mino_core::terminal::{Session, SessionOptions};
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

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

    /// 渲染级回归：光标在视口底行输入时，浮层实际渲染矩形不得遮挡输入行
    /// （曾因浮层高度估算漏算行间距，底边侵入输入行约 15px）。
    #[test]
    fn 浮层实际渲染不遮输入行() {
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
        assert!(
            wait_text(&view, &mut harness, "mino"),
            "zsh 未就绪，终端内容：\n{}",
            grid_text(view.borrow().session())
        );

        // 输出足够多行让光标到视口底行。
        view.borrow().session().write(b"seq 40\r");
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            harness.step();
            if grid_text(view.borrow().session())
                .lines()
                .any(|l| l.trim_end() == "40")
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }

        // 输入 "l" 触发补全候选（最多 8 条）。
        send_key(&mut harness, egui::Key::L, Some("l"));
        for _ in 0..6 {
            harness.step();
        }

        let (n, cursor_pos, cell_height) = {
            let v = view.borrow();
            (v.candidates.len(), v.cursor_pos, v.cell_height)
        };
        assert!(n > 0, "输入 l 后应有补全候选");

        let popup_rect = harness
            .ctx
            .memory(|m| m.area_rect(egui::Id::new("completion_popup")));
        let cursor = cursor_pos.expect("光标位置未记录");
        let input_row = egui::Rect::from_min_max(
            egui::pos2(0.0, cursor.y - cell_height),
            egui::pos2(5000.0, cursor.y),
        );
        let pr = popup_rect.expect("浮层未渲染（area_rect 为 None）");
        assert!(
            !pr.intersects(input_row),
            "浮层盖住输入行：浮层 {pr:?} 与输入行 {input_row:?} 相交"
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

    /// Tab 属于终端输入，不应被 egui 当作焦点导航键；否则 SSH 远端补全后
    /// 终端会短暂失去焦点，紧接着的 Ctrl+C 可能被吞掉。
    #[test]
    fn tab补全保持终端焦点() {
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

        // 注入滚轮事件（Point/Line/Page 三种单位）。
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
}

#[cfg(test)]
mod completion_popup_tests {
    use super::completion_popup_anchor;

    /// 内边距后的终端内容区（800x600 面板去 PADDING）。
    fn inner() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 600.0))
    }

    /// 屏幕内容区（含标签栏高度，与 ctx.content_rect 同构）。
    fn content() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(0.0, 40.0), egui::vec2(800.0, 560.0))
    }

    /// 3 条候选的浮层尺寸（3 * 22 + 2 * 3 + 12 + 8）。
    fn popup() -> egui::Vec2 {
        egui::vec2(260.0, 86.0)
    }

    /// 输入行在视口中部：上方模式，浮层底边锚定在光标行顶上方 4px。
    #[test]
    fn 输入行在中部浮层在上方() {
        let cursor = egui::pos2(120.0, 160.0); // 光标行底部（第 10 行）
        let (anchor, offset) = completion_popup_anchor(cursor, 16.0, popup(), inner(), content());
        assert_eq!(anchor, egui::Align2::LEFT_BOTTOM, "空间充足应用上方模式");
        assert_eq!(
            content().bottom() + offset.y,
            160.0 - 16.0 - 4.0,
            "浮层底边应锚定在光标行顶上方 4px"
        );
    }

    /// 输入行在视口顶部（上方放不下）：下方模式，顶边锚定在光标行底下方 4px。
    #[test]
    fn 输入行在顶部浮层在下方() {
        let cursor = egui::pos2(120.0, 42.0); // 内容区第 1 行底
        let (anchor, offset) = completion_popup_anchor(cursor, 16.0, popup(), inner(), content());
        assert_eq!(anchor, egui::Align2::LEFT_TOP, "上方放不下应用下方模式");
        assert_eq!(
            content().top() + offset.y,
            42.0 + 4.0,
            "浮层顶边应锚定在光标行底下方 4px"
        );
    }

    /// 上方空间恰好放得下（浮层顶边 == 终端顶）时仍用上方模式。
    #[test]
    fn 上方空间恰好放得下() {
        // 106 - 16 - 86 - 4 = 0 == inner.top()
        let cursor = egui::pos2(120.0, 106.0);
        let (anchor, _) = completion_popup_anchor(cursor, 16.0, popup(), inner(), content());
        assert_eq!(
            anchor,
            egui::Align2::LEFT_BOTTOM,
            "恰好放得下时应保持上方模式"
        );
    }

    /// x 方向限制在终端内容区内（光标靠边时浮层不越界）。
    #[test]
    fn 浮层x方向不越界() {
        let left =
            completion_popup_anchor(egui::pos2(0.0, 160.0), 16.0, popup(), inner(), content());
        assert_eq!(content().left() + left.1.x, 0.0, "浮层不得超出内容区左缘");
        let right =
            completion_popup_anchor(egui::pos2(800.0, 160.0), 16.0, popup(), inner(), content());
        assert_eq!(
            content().left() + right.1.x,
            800.0 - 260.0 - 4.0,
            "浮层不得超出内容区右缘"
        );
    }
}
