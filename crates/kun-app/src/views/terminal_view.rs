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
}

impl TerminalView {
    /// 创建终端视图并启动本地会话。
    pub fn new(session: Session) -> Self {
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

        // ==================== 事件泵 ====================
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

        let avail = ui.available_size();
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
            let default_fg = colors[NamedColor::Foreground].unwrap_or(Rgb {
                r: 0xf8,
                g: 0xf8,
                b: 0xf2,
            });
            let default_bg = colors[NamedColor::Background].unwrap_or(Rgb {
                r: 0x28,
                g: 0x2a,
                b: 0x36,
            });
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

                if point.line != current_line {
                    if current_line != usize::MAX {
                        lines_data.push(LineData {
                            line: current_line,
                            hash,
                            segments: std::mem::take(&mut segments),
                            backgrounds: std::mem::take(&mut backgrounds),
                        });
                    }
                    hash = 0;
                    current_line = point.line.0 as usize;
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
                    && point.line.0 as usize == cursor.point.line.0 as usize
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
                if line < self.rows as usize && col < self.cols as usize {
                    let color = colors[NamedColor::Cursor].unwrap_or(default_fg);
                    cursor_color = Some(to_egui(color));
                    cursor_rect = Some(Rect::from_min_size(
                        ui.min_rect().min
                            + Vec2::new(col as f32 * cell_width, line as f32 * cell_height),
                        Vec2::new(cell_width, cell_height),
                    ));
                }
            }
        }

        // ==================== 绘制（锁外） ====================
        let painter = ui.painter();
        let origin = ui.min_rect().min;
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
        let response = ui.interact(ui.min_rect(), self.focus_id, egui::Sense::click());
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
                                }
                                continue;
                            }
                        }
                        if let Some(k) = map_special_key(key) {
                            if let Some(bytes) = keys::encode_key(k, mods, mode) {
                                session.write(&bytes);
                            }
                        }
                    }
                    egui::Event::Text(text) => {
                        // Ctrl/Alt 组合已在 Key 事件处理，跳过避免重复写入。
                        let mods = i.modifiers;
                        if mods.ctrl || mods.alt {
                            continue;
                        }
                        session.write(text.as_bytes());
                    }
                    egui::Event::Paste(text) => {
                        // 括号粘贴模式（bracketed paste）下包装转义序列。
                        let payload = if mode.contains(TermMode::BRACKETED_PASTE) {
                            format!("\x1b[200~{text}\x1b[201~")
                        } else {
                            text.clone()
                        };
                        session.write(payload.as_bytes());
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
                                ctx.request_repaint();
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
                            ctx.request_repaint();
                        }
                    }
                    _ => {}
                }
            }
        });
    }
}

// ==================== 辅助函数 ====================

/// 解析终端颜色为 egui 颜色（含粗体 → 亮色映射）。
fn resolve_color(color: AColor, colors: &Colors, default: Rgb, bold: bool) -> Color32 {
    match color {
        AColor::Spec(rgb) => to_egui(rgb),
        AColor::Named(n) => {
            let mut idx = n as usize;
            // 粗体时将基本色映射到亮色（参照 Alacritty 默认行为）。
            if bold && idx < 8 {
                idx += 8;
            }
            colors[idx].map(to_egui).unwrap_or(to_egui(default))
        }
        AColor::Indexed(i) => colors[i as usize].map(to_egui).unwrap_or(to_egui(default)),
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
