//! 终端视图：cell 渲染、键盘输入转发、滚动。

use std::time::Duration;

use alacritty_terminal::grid::Scroll;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color as AColor, CursorShape, NamedColor, Rgb};
use egui::text::LayoutJob;
use egui::{Color32, FontId, Rect, Stroke, TextFormat, Ui, Vec2};
use kun_core::terminal::keys::{self, Key, Mods};
use kun_core::terminal::{Session, SessionEvent, TermMode, TermSize};

/// 行缓存：内容 hash 未变时复用 LayoutJob，避免每帧重建。
#[derive(Clone)]
struct RowCache {
    hash: u64,
    job: LayoutJob,
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
struct BgRect {
    start: usize,
    end: usize,
    color: Color32,
}

/// 单行渲染数据。
struct LineData {
    line: usize,
    hash: u64,
    segments: Vec<Segment>,
    backgrounds: Vec<BgRect>,
}

/// 终端内容内边距（文本与面板边缘的间距，参照 Terminal.app 观感）。
const PADDING: f32 = 10.0;

/// 终端视图。
pub struct TerminalView {
    session: Session,
    rows_cache: Vec<RowCache>,
    font_size: f32,
    cell_width: f32,
    cell_height: f32,
    cols: u16,
    rows: u16,
    focus_id: egui::Id,
    initialized: bool,
    last_mode: TermMode,
    /// 退格/删除键按下后，下一帧的"空白类" Text 事件应丢弃。
    /// （某些输入法（如微信输入法）退格时会伴随发送空格类文本，
    /// 写入终端表现为"删除键插入空格"；正常字符不受影响）
    suppress_blank_frames: u8,
    /// 补全输入模型（本地会话启用；远程会话恒失效）。
    input: crate::completion::InputModel,
    /// 当前补全候选。
    candidates: Vec<crate::completion::Candidate>,
    /// 候选选中索引。
    candidate_selected: usize,
    /// 光标屏幕坐标（补全浮层定位）。
    cursor_pos: Option<egui::Pos2>,
    /// 上一帧终端是否持有焦点（焦点自动恢复用）。
    had_focus: bool,
}

impl TerminalView {
    /// 创建终端视图并启动本地会话。
    pub fn new(session: Session) -> Self {
        let is_remote = session.is_remote();
        // 本地会话初始工作目录：会话启动目录（HOME）；远程不启用补全。
        let cwd = if is_remote {
            std::path::PathBuf::new()
        } else {
            std::env::var("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_default()
        };
        Self {
            session,
            rows_cache: Vec::new(),
            font_size: 13.0,
            cell_width: 8.0,
            cell_height: 16.0,
            cols: 80,
            rows: 24,
            focus_id: egui::Id::new("terminal_view"),
            initialized: false,
            last_mode: TermMode::NONE,
            suppress_blank_frames: 0,
            input: crate::completion::InputModel::new(cwd),
            candidates: Vec::new(),
            candidate_selected: 0,
            cursor_pos: None,
            had_focus: false,
        }
    }

    /// 会话引用（供状态栏等读取标题）。
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// 每帧渲染入口。
    pub fn show(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let term_arc = self.session.term();

        // 终端区域背景（当前主题的终端色）。
        // 注意：用 max_rect（布局分配区域）而非 min_rect（已用内容包围盒，
        // 无子项时为 0x0，会导致背景画不出来）。
        let term_bg = crate::theme::current_theme().term_bg;
        let outer = ui.max_rect();
        ui.painter().rect_filled(
            outer,
            0.0,
            Color32::from_rgb(term_bg.r, term_bg.g, term_bg.b),
        );
        // 终端内容区域：背景铺满面板，文本/光标在内边距内绘制。
        let inner = outer.shrink(PADDING);

        // ==================== 事件泵 ====================
        // 诊断：PTY 读取线程退出会导致输入写入失效。
        if self.session.pty_thread_finished() {
            log::warn!("PTY 读取线程已退出！输入将无法写入终端。");
        }
        for event in self.session.drain_events() {
            match event {
                SessionEvent::Wakeup => ctx.request_repaint(),
                SessionEvent::PtyWrite(text) => self.session.write(text.as_bytes()),
                _ => {}
            }
        }

        // ==================== 尺寸计算与 resize ====================
        let (cell_width, cell_height) = ui.fonts_mut(|f| {
            let font = FontId::monospace(self.font_size);
            (f.glyph_width(&font, ' '), f.row_height(&font))
        });
        self.cell_width = cell_width;
        self.cell_height = cell_height;

        let avail = inner.size();
        let cols = ((avail.x / cell_width).floor() as usize).max(2);
        let rows = ((avail.y / cell_height).floor() as usize).max(1);
        if cols as u16 != self.cols || rows as u16 != self.rows {
            self.cols = cols as u16;
            self.rows = rows as u16;
            // 通知 PTY 调整窗口尺寸。
            self.session.resize(self.cols, self.rows);
            // 同步更新终端状态机的网格。
            let mut guard = term_arc.lock();
            guard.resize(TermSize { rows, cols });
            self.rows_cache.clear();
        }

        // ==================== 构建渲染数据（锁内） ====================
        let mut lines_data: Vec<LineData> = Vec::with_capacity(self.rows as usize);
        let mut cursor_rect: Option<Rect> = None;
        let mut cursor_color: Option<Color32> = None;

        {
            let guard = term_arc.lock();
            let content = guard.renderable_content();
            let colors = content.colors;
            let display_offset = content.display_offset;
            let default_fg =
                colors[NamedColor::Foreground].unwrap_or(crate::theme::current_theme().term_fg);
            // 背景色强制跟随主题（忽略 OSC 背景覆盖——zsh 主题常设置深色背景，
            // 会导致浅色主题下终端仍为深色）。
            let default_bg = crate::theme::current_theme().term_bg;
            self.last_mode = content.mode;
            let mode = content.mode;
            let cursor = content.cursor;
            let cursor_style = guard.cursor_style();

            // 光标可见性（含闪烁）。
            let time = ctx.input(|i| i.time);
            let blinking = cursor_style.blinking;
            let cursor_visible = mode.contains(TermMode::SHOW_CURSOR)
                && (!blinking || ((time * 2.0) as u64).is_multiple_of(2));
            if blinking {
                ctx.request_repaint_after(Duration::from_millis(500));
            }

            // 逐行构建（跳过宽字符占位格）。
            let mut segments: Vec<Segment> = Vec::new();
            let mut backgrounds: Vec<BgRect> = Vec::new();
            let mut hash: u64 = 0;
            let mut current_line = usize::MAX;
            // display_iter 的行号是网格坐标（向上滚动后 scrollback 行为负），
            // 仅用于换行检测；渲染定位与行缓存索引用相对视口顶行的显示行号（0 起）。
            let mut prev_grid_line: i32 = i32::MIN;
            let mut display_line: usize = 0;
            let default_bg_egui = to_egui(default_bg);

            for item in content.display_iter {
                let point = item.point;
                let cell = item.cell;

                // 占位格只参与背景合并，不进入文本。
                let is_spacer = cell.flags.contains(Flags::WIDE_CHAR_SPACER)
                    || cell.flags.contains(Flags::HIDDEN);
                if is_spacer {
                    if cell.flags.contains(Flags::WIDE_CHAR_SPACER)
                        && !cell.flags.contains(Flags::HIDDEN)
                    {
                        // 宽字符占位格继承前一格的背景，合并背景段。
                        let bg = resolve_color(cell.bg, colors, default_bg, false);
                        if bg != default_bg_egui {
                            if let Some(last) = backgrounds.last_mut() {
                                if last.color == bg {
                                    last.end = (point.column + 1).0;
                                } else {
                                    backgrounds.push(BgRect {
                                        start: point.column.0,
                                        end: (point.column + 1).0,
                                        color: bg,
                                    });
                                }
                            } else {
                                backgrounds.push(BgRect {
                                    start: point.column.0,
                                    end: (point.column + 1).0,
                                    color: bg,
                                });
                            }
                        }
                    }
                    continue;
                }

                if point.line.0 != prev_grid_line {
                    if current_line != usize::MAX {
                        lines_data.push(LineData {
                            line: current_line,
                            hash,
                            segments: std::mem::take(&mut segments),
                            backgrounds: std::mem::take(&mut backgrounds),
                        });
                    }
                    hash = 0;
                    current_line = display_line;
                    display_line += 1;
                    prev_grid_line = point.line.0;
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

                // 光标 cell：Block 光标下文本反色，由光标矩形覆盖。
                let is_cursor = cursor_visible
                    && cursor.shape == CursorShape::Block
                    && point.line.0 == cursor.point.line.0
                    && point.column == cursor.point.column;
                if is_cursor {
                    std::mem::swap(&mut fg, &mut bg);
                }

                // 背景段合并（默认背景不绘制）。
                if bg != default_bg_egui {
                    if let Some(last) = backgrounds.last_mut() {
                        if last.color == bg {
                            last.end = (point.column + 1).0;
                        } else {
                            backgrounds.push(BgRect {
                                start: point.column.0,
                                end: (point.column + 1).0,
                                color: bg,
                            });
                        }
                    } else {
                        backgrounds.push(BgRect {
                            start: point.column.0,
                            end: (point.column + 1).0,
                            color: bg,
                        });
                    }
                }

                // 文本段合并。
                push_or_merge(
                    &mut segments,
                    cell.c,
                    CellStyle {
                        fg,
                        bold,
                        italic,
                        underline,
                        strikeout,
                    },
                    &mut hash,
                );
            }
            if current_line != usize::MAX {
                lines_data.push(LineData {
                    line: current_line,
                    hash,
                    segments: std::mem::take(&mut segments),
                    backgrounds: std::mem::take(&mut backgrounds),
                });
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

        // ==================== 绘制（锁外） ====================
        let painter = ui.painter();
        let origin = inner.min;
        for data in &lines_data {
            // 绘制背景矩形（行内连续背景段）。
            for bg in &data.backgrounds {
                let rect = Rect::from_min_size(
                    origin
                        + Vec2::new(bg.start as f32 * cell_width, data.line as f32 * cell_height),
                    Vec2::new((bg.end - bg.start) as f32 * cell_width, cell_height),
                );
                painter.rect_filled(rect, 0.0, bg.color);
            }
            // 文本（复用或重建 LayoutJob）。
            let job = self.job_for_line(data);
            let pos = origin + Vec2::new(0.0, data.line as f32 * cell_height);
            let galley = ui.fonts_mut(|f| f.layout_job(job));
            painter.galley(pos, galley, Color32::WHITE);
        }

        // 光标形状绘制。
        if let (Some(rect), Some(color)) = (cursor_rect, cursor_color) {
            let shape = {
                let guard = term_arc.lock();
                guard.cursor_style().shape
            };
            match shape {
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
        self.had_focus = has_focus_now;
        // 点击区域覆盖整个面板（min_rect 无子项时为 0x0，会导致点击无法重新聚焦）。
        let response = ui.interact(ui.max_rect(), self.focus_id, egui::Sense::click());
        if response.clicked() {
            ui.memory_mut(|m| m.request_focus(self.focus_id));
        }
        if ui.memory(|m| m.has_focus(self.focus_id)) {
            self.handle_input(ui);
        }
    }

    /// 获取（或重建）某一行的 LayoutJob。
    fn job_for_line(&mut self, data: &LineData) -> LayoutJob {
        if let Some(cache) = self.rows_cache.get_mut(data.line) {
            if cache.hash == data.hash {
                return cache.job.clone();
            }
            cache.hash = data.hash;
            cache.job = build_job(&data.segments, self.font_size);
            return cache.job.clone();
        }
        let job = build_job(&data.segments, self.font_size);
        if self.rows_cache.len() <= data.line {
            self.rows_cache.resize(
                data.line + 1,
                RowCache {
                    hash: 0,
                    job: LayoutJob::default(),
                },
            );
        }
        self.rows_cache[data.line].hash = data.hash;
        self.rows_cache[data.line].job = job;
        self.rows_cache[data.line].job.clone()
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
                        // Command 修饰为应用快捷键，不转发给终端。
                        if modifiers.command {
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
                            format!("\x1b[200~{text}\x1b[201~")
                        } else {
                            text.clone()
                        };
                        session.write(payload.as_bytes());
                        // 粘贴内容不可逐字节信任（可能含控制序列），模型失效。
                        actions.push(InputAction::Paste);
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
            self.apply_input_action(action);
        }
        // 渲染补全浮层（本地会话）。
        if !self.candidates.is_empty() {
            self.render_completion_popup(ui);
        }
    }

    /// 应用一帧内的输入动作（写入 PTY 的字节与拦截的按键）。
    fn apply_input_action(&mut self, action: InputAction) {
        match action {
            InputAction::Bytes(bytes) => self.track_input_bytes(&bytes),
            InputAction::Text(text) => {
                self.input.push_text(&text);
                self.recompute_candidates();
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
        }
    }

    /// 分析写入 PTY 的字节并同步输入模型（本地会话）。
    fn track_input_bytes(&mut self, bytes: &[u8]) {
        if self.session.is_remote() {
            return;
        }
        match bytes {
            // 回车：执行命令（解析 cd），清空输入，模型恢复可靠。
            b"\r" | b"\n" => {
                self.input.execute();
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
                self.recompute_candidates();
            }
            // Tab（shell 自身补全）：不改变输入内容。
            b"\t" => {}
            _ => {
                // 可见文本（ASCII 可打印 / 空格 / 非 ASCII）。
                if let Ok(s) = std::str::from_utf8(bytes) {
                    if s.chars()
                        .all(|c| c.is_ascii_graphic() || c == ' ' || !c.is_ascii())
                    {
                        self.input.push_text(s);
                        self.recompute_candidates();
                        return;
                    }
                }
                // 控制序列/编辑键（箭头、Ctrl+U/W 等）：光标位置不可追踪，模型失效。
                self.input.invalidate();
                self.candidates.clear();
            }
        }
    }

    /// 重新计算补全候选。
    fn recompute_candidates(&mut self) {
        if self.session.is_remote() {
            self.candidates.clear();
            return;
        }
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
        let popup_w = 260.0;
        let row_h = 22.0;
        let rows = self.candidates.len().min(8) as f32;
        let popup_h = rows * row_h + 12.0;
        // 显示在光标行上方；空间不足时移到光标行下方。
        // cursor_pos 是光标行底部：上方模式浮层底边 = 光标行顶上方 4px（完全不遮输入行），
        // 下方模式浮层顶边 = 光标行底下方 4px（紧贴输入行）。
        let pos = completion_popup_pos(
            cursor_pos,
            self.cell_height,
            egui::vec2(popup_w, popup_h),
            inner,
        );
        egui::Area::new(egui::Id::new("completion_popup"))
            .order(egui::Order::Foreground)
            // 不拦截鼠标交互（interactable 的 Area 会导致终端焦点被释放）。
            .interactable(false)
            .fixed_pos(pos)
            .show(ui.ctx(), |ui| {
                egui::Frame::new()
                    .fill(theme.bg_elevated)
                    .stroke(egui::Stroke::new(1.0, theme.border))
                    .corner_radius(crate::theme::tokens::RADIUS_SM)
                    .inner_margin(egui::Margin::symmetric(6, 4))
                    .show(ui, |ui| {
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
                            let row = ui
                                .horizontal(|ui| {
                                    ui.set_min_size(egui::vec2(popup_w, row_h));
                                    if selected {
                                        ui.painter().rect_filled(
                                            ui.max_rect(),
                                            crate::theme::tokens::RADIUS_ITEM,
                                            theme.accent_soft,
                                        );
                                    }
                                    ui.label(
                                        egui::RichText::new(marker).size(11.0).color(if selected {
                                            theme.text_primary
                                        } else {
                                            color
                                        }),
                                    );
                                    ui.add_space(4.0);
                                    ui.label(
                                        egui::RichText::new(&c.display).size(12.5).color(
                                            if selected { theme.text_primary } else { color },
                                        ),
                                    );
                                })
                                .response
                                .on_hover_cursor(egui::CursorIcon::PointingHand);
                            if row.hovered() {
                                self.candidate_selected = i;
                            }
                            if row.clicked() {
                                clicked = Some(i);
                            }
                        }
                        if let Some(i) = clicked {
                            self.candidate_selected = i;
                            self.accept_completion();
                        }
                    });
            });
    }
}

/// 计算补全浮层左上角位置（独立函数便于单测）。
///
/// `cursor` 为光标行**底部**坐标；上方空间充足时浮层显示在输入行上方
/// （底边 = 光标行顶上方 4px，完全不遮输入行），不足时显示在输入行下方
/// （顶边 = 光标行底下方 4px，紧贴输入行）。x 方向以光标为轴向左偏移并
/// 限制在终端内容区内。
fn completion_popup_pos(
    cursor: egui::Pos2,
    cell_height: f32,
    popup_size: egui::Vec2,
    inner: egui::Rect,
) -> egui::Pos2 {
    let mut pos = egui::pos2(
        (cursor.x - popup_size.x * 0.35).clamp(inner.left(), inner.right() - popup_size.x - 4.0),
        cursor.y - cell_height - popup_size.y - 4.0,
    );
    if pos.y < inner.top() {
        pos.y = cursor.y + 4.0;
    }
    pos
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
}

// ==================== 辅助函数 ====================

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
            *hash = hash.wrapping_mul(131).wrapping_add(style.key());
            *hash = hash.wrapping_mul(131).wrapping_add(c as u64);
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
    *hash = hash.wrapping_mul(131).wrapping_add(style.key());
    *hash = hash.wrapping_mul(131).wrapping_add(c as u64);
}

/// 样式 → 哈希键。
#[allow(dead_code)]
fn style_key(fg: Color32, bold: bool, italic: bool, underline: bool, strikeout: bool) -> u64 {
    CellStyle {
        fg,
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
    use kun_core::terminal::{Session, SessionOptions};
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
            wait_text(&view, &mut harness, "kun"),
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
        assert!(wait_text(&view, &mut harness, "kun"), "zsh 未就绪");

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
        assert!(wait_text(&view, &mut harness, "kun"), "zsh 未就绪");

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
    use kun_core::terminal::{Session, SessionOptions};
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
    use kun_core::terminal::{Session, SessionOptions};
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
            if grid_text(view.borrow().session()).contains("kun") {
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
mod ime_backspace_tests {
    use super::*;
    use kun_core::terminal::{Session, SessionOptions};
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
            if grid_text(view.borrow().session()).contains("kun") {
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
    use super::completion_popup_pos;

    /// 内边距后的终端内容区（800x600 面板去 PADDING）。
    fn inner() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 600.0))
    }

    /// 3 条候选的浮层尺寸（3 * 22 + 12）。
    fn popup() -> egui::Vec2 {
        egui::vec2(260.0, 78.0)
    }

    /// 输入行在视口中部：浮层显示在输入行上方，且完全不遮输入行
    /// （底边 ≤ 光标行顶上方 4px；回归测试：曾向下偏移一整行盖住输入行底部）。
    #[test]
    fn 输入行在中部浮层在上方() {
        let cursor = egui::pos2(120.0, 160.0); // 光标行底部（第 10 行）
        let pos = completion_popup_pos(cursor, 16.0, popup(), inner());
        assert_eq!(
            pos.y,
            160.0 - 16.0 - 78.0 - 4.0,
            "浮层底边应在光标行顶上方 4px"
        );
        assert!(
            pos.y + 78.0 <= cursor.y - 16.0 - 4.0,
            "浮层不得遮挡输入行（底边 {} > 输入行顶上方 4px {}）",
            pos.y + 78.0,
            cursor.y - 16.0 - 4.0
        );
    }

    /// 输入行在视口顶部（上方放不下）：浮层移到输入行下方紧贴，不遮输入行。
    #[test]
    fn 输入行在顶部浮层在下方() {
        let cursor = egui::pos2(120.0, 32.0); // 第 2 行底，上方仅 1 行
        let pos = completion_popup_pos(cursor, 16.0, popup(), inner());
        assert_eq!(
            pos.y,
            cursor.y + 4.0,
            "下方模式浮层顶边应紧贴输入行底下方 4px"
        );
        assert!(
            pos.y >= cursor.y + 4.0,
            "浮层不得遮挡输入行（顶边 {} < 光标行底 {}）",
            pos.y,
            cursor.y
        );
    }

    /// 上方空间恰好放得下（浮层顶边 == 终端顶）时仍用上方模式。
    #[test]
    fn 上方空间恰好放得下() {
        let cursor = egui::pos2(120.0, 98.0); // 98 - 16 - 78 - 4 = 0 == inner.top()
        let pos = completion_popup_pos(cursor, 16.0, popup(), inner());
        assert_eq!(pos.y, 0.0, "恰好放得下时应保持上方模式");
    }

    /// x 方向限制在终端内容区内（光标靠边时浮层不越界）。
    #[test]
    fn 浮层x方向不越界() {
        let left = completion_popup_pos(egui::pos2(0.0, 160.0), 16.0, popup(), inner());
        assert_eq!(left.x, 0.0, "浮层不得超出内容区左缘");
        let right = completion_popup_pos(egui::pos2(800.0, 160.0), 16.0, popup(), inner());
        assert_eq!(right.x, 800.0 - 260.0 - 4.0, "浮层不得超出内容区右缘");
    }
}
