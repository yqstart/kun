//! SFTP 面板：远程文件浏览、传输进度与文件操作。

use egui::{RichText, Ui};
use mino_core::ssh::sftp::{RemoteEntry, SftpEvent, SftpHandle};
use tokio::sync::mpsc::Receiver;

/// 传输任务（进度条显示）。
#[derive(Clone)]
struct Transfer {
    id: u64,
    label: String,
    done: u64,
    total: u64,
    finished: bool,
    failed: bool,
}

/// 文件操作确认对话框。
enum ConfirmDialog {
    Delete {
        items: Vec<DeleteTarget>,
    },
    Rename {
        from: String,
        path: String,
        input: String,
    },
    Mkdir {
        path: String,
        input: String,
    },
}

/// 待确认删除的远程条目。
#[derive(Clone)]
struct DeleteTarget {
    name: String,
    path: String,
    is_dir: bool,
}

/// 列表右键菜单产生的动作，统一在 `show` 结束后执行，避免菜单闭包
/// 与当前列表的可变借用互相冲突。
enum ContextAction {
    Open(String),
    Refresh,
    Upload,
    Download(Vec<String>),
    Rename(String),
    Delete(Vec<String>),
    Mkdir,
    Locate(String),
}

/// SFTP 面板状态。
pub struct SftpView {
    /// 主机名称（显示用）。
    host_name: String,
    handle: SftpHandle,
    rx: Receiver<SftpEvent>,
    /// 当前远程路径。
    current_path: String,
    /// 当前目录条目。
    entries: Vec<RemoteEntry>,
    /// 选中条目（按当前目录顺序保存，支持 Shift/Cmd(Ctrl) 多选）。
    selected: Vec<String>,
    /// Shift 范围选择的锚点。
    selection_anchor: Option<String>,
    /// 加载中。
    loading: bool,
    /// 传输任务。
    transfers: Vec<Transfer>,
    /// 确认对话框。
    dialog: Option<ConfirmDialog>,
    /// 行内错误。
    error: Option<String>,
    /// 连接已关闭。
    closed: bool,
    /// 等宽字符宽缓存（'0' 字符，数字等宽字体的真实字宽；字体启动时加载后不变）。
    cell_width: f32,
}

impl SftpView {
    /// 创建 SFTP 面板（连接就绪后）。
    pub fn new(host_name: &str, handle: SftpHandle, rx: Receiver<SftpEvent>) -> Self {
        Self::new_at_path(host_name, handle, rx, "/")
    }

    /// 创建 SFTP 面板并从指定的远程初始目录开始浏览。
    pub fn new_at_path(
        host_name: &str,
        handle: SftpHandle,
        rx: Receiver<SftpEvent>,
        initial_path: &str,
    ) -> Self {
        let initial_path = if initial_path.is_empty() {
            "/"
        } else {
            initial_path
        };
        let view = Self {
            host_name: host_name.to_string(),
            handle,
            rx,
            current_path: initial_path.to_string(),
            entries: Vec::new(),
            selected: Vec::new(),
            selection_anchor: None,
            loading: true,
            transfers: Vec::new(),
            dialog: None,
            error: None,
            closed: false,
            cell_width: 0.0,
        };
        // 初始列出 SFTP 会话的目录（通常与远程终端登录目录一致）。
        view.handle.list(initial_path);
        view
    }

    /// 会话引用（供状态栏等读取标题）。
    pub fn host_name(&self) -> &str {
        &self.host_name
    }

    /// 请求列出目录并清理已失效的选择状态。
    fn navigate_to(&mut self, path: &str) {
        let path = if path.is_empty() { "/" } else { path };
        self.handle.list(path);
        self.loading = true;
        self.error = None;
        self.selected.clear();
        self.selection_anchor = None;
    }

    /// 按文件管理器习惯更新选择：普通点击单选，Shift 选择范围，
    /// Cmd(macOS)/Ctrl 追加或取消单项。
    fn update_selection(&mut self, idx: usize, name: &str, modifiers: egui::Modifiers) {
        if modifiers.shift {
            let anchor_idx = self
                .selection_anchor
                .as_deref()
                .and_then(|anchor| self.entries.iter().position(|e| e.name == anchor))
                .unwrap_or(idx.saturating_sub(1));
            let clicked_idx = idx.saturating_sub(1);
            let (start, end) = if anchor_idx <= clicked_idx {
                (anchor_idx, clicked_idx)
            } else {
                (clicked_idx, anchor_idx)
            };
            self.selected = self.entries[start..=end]
                .iter()
                .map(|entry| entry.name.clone())
                .collect();
            return;
        }

        if modifiers.command || modifiers.ctrl {
            if let Some(position) = self.selected.iter().position(|item| item == name) {
                self.selected.remove(position);
            } else {
                self.selected.push(name.to_string());
            }
            self.selection_anchor = Some(name.to_string());
            return;
        }

        self.selected.clear();
        self.selected.push(name.to_string());
        self.selection_anchor = Some(name.to_string());
    }

    /// 右键已选中条目时保留整个多选集合；右键未选中条目则先按当前修饰键
    /// 更新选择，再打开该条目的菜单。
    fn update_secondary_selection(&mut self, idx: usize, name: &str, modifiers: egui::Modifiers) {
        let already_selected = self.selected.iter().any(|item| item == name);
        if !already_selected || modifiers.shift || modifiers.command || modifiers.ctrl {
            self.update_selection(idx, name, modifiers);
        }
    }

    /// 记录一项传输，先于后台事件进入队列，保证上传区域立即出现。
    fn begin_transfer(&mut self, id: u64, label: String, total: u64) {
        self.transfers.push(Transfer {
            id,
            label,
            done: 0,
            total,
            finished: false,
            failed: false,
        });
    }

    /// 处理后台事件。返回本帧是否收到新事件（调用方据此请求重绘——
    /// 传输进度/列表到达后若无其它重绘源，进度条不会自行刷新）。
    pub(crate) fn poll_events(&mut self) -> bool {
        let mut any = false;
        while let Ok(event) = self.rx.try_recv() {
            any = true;
            match event {
                SftpEvent::Listed { path, entries } => {
                    self.current_path = path;
                    self.entries = entries;
                    self.selected
                        .retain(|name| self.entries.iter().any(|entry| entry.name == *name));
                    if self.selected.is_empty() {
                        self.selection_anchor = None;
                    }
                    self.loading = false;
                }
                SftpEvent::Progress {
                    id,
                    label,
                    done,
                    total,
                } => {
                    if let Some(t) = self.transfers.iter_mut().find(|t| t.id == id) {
                        t.done = done;
                        t.total = total;
                    } else {
                        self.transfers.push(Transfer {
                            id,
                            label,
                            done,
                            total,
                            finished: false,
                            failed: false,
                        });
                    }
                }
                SftpEvent::Done { id, label } => {
                    if let Some(id) = id {
                        if let Some(t) = self.transfers.iter_mut().find(|t| t.id == id) {
                            t.finished = true;
                        } else {
                            self.transfers.push(Transfer {
                                id,
                                label,
                                done: 1,
                                total: 1,
                                finished: true,
                                failed: false,
                            });
                        }
                    }
                    // 操作完成后刷新目录。
                    self.handle.list(&self.current_path);
                }
                SftpEvent::Error { id, label, message } => {
                    self.error = Some(format!("{label}：{message}"));
                    if let Some(id) = id {
                        if let Some(t) = self.transfers.iter_mut().find(|t| t.id == id) {
                            t.failed = true;
                        } else {
                            self.transfers.push(Transfer {
                                id,
                                label,
                                done: 0,
                                total: 0,
                                finished: false,
                                failed: true,
                            });
                        }
                    }
                    self.loading = false;
                }
                SftpEvent::Closed => {
                    self.closed = true;
                }
                _ => {}
            }
        }
        any
    }

    /// 远程路径拼接。
    fn join(&self, name: &str) -> String {
        let path = &self.current_path;
        if path.ends_with('/') {
            format!("{path}{name}")
        } else {
            format!("{path}/{name}")
        }
    }

    /// 上级目录。
    fn parent_of(path: &str) -> String {
        let trimmed = path.trim_end_matches('/');
        if trimmed.is_empty() {
            return "/".to_string();
        }
        match trimmed.rfind('/') {
            Some(0) => "/".to_string(),
            Some(i) => trimmed[..i].to_string(),
            None => "/".to_string(),
        }
    }

    /// 格式化大小。
    fn format_size(size: u64) -> String {
        const KB: f64 = 1024.0;
        const MB: f64 = KB * 1024.0;
        const GB: f64 = MB * 1024.0;
        let size = size as f64;
        if size >= GB {
            format!("{:.1} GB", size / GB)
        } else if size >= MB {
            format!("{:.1} MB", size / MB)
        } else if size >= KB {
            format!("{:.1} KB", size / KB)
        } else {
            format!("{size:.0} B")
        }
    }

    /// 渲染列表中的一行（虚拟化：`show_rows` 只对可见行调用本方法）。
    ///
    /// `idx == 0` 为 ".." 上级行，其余对应 `entries[idx - 1]`。
    /// 导航/选中动作不直接改 self（闭包内借用冲突），写入 `open_dir`/`select`
    /// 由调用方在闭包外统一应用。
    #[allow(clippy::too_many_arguments)]
    fn render_list_row(
        &mut self,
        ui: &mut Ui,
        idx: usize,
        table_width: f32,
        name_col: f32,
        size_col: f32,
        time_col: f32,
        icon_pad: f32,
        row_h: f32,
        theme: &'static crate::theme::Theme,
        open_dir: &mut Option<String>,
        context_action: &mut Option<ContextAction>,
        terminal_cwd: Option<&str>,
    ) {
        let (row_rect, _) =
            ui.allocate_exact_size(egui::vec2(table_width, row_h), egui::Sense::hover());
        // hover 用指针位置判定（子 Ui 会抢走 response.hovered()）。
        let pointer_in_row =
            ui.input(|i| i.pointer.hover_pos().is_some_and(|p| row_rect.contains(p)));
        let row_id = if idx == 0 {
            egui::Id::new(("sftp_row", ".."))
        } else {
            egui::Id::new(("sftp_row", self.entries[idx - 1].name.as_str()))
        };

        // ".." 行：返回上级目录（文件管理器通用习惯，导航更直观）。
        if idx == 0 {
            if pointer_in_row {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                ui.painter().rect_filled(
                    row_rect,
                    crate::theme::tokens::RADIUS_ITEM,
                    egui::Color32::from_rgba_unmultiplied(255, 255, 255, 12),
                );
            }
            let icon = egui::Rect::from_center_size(
                egui::pos2(row_rect.left() + 6.0 + 7.0, row_rect.center().y),
                egui::vec2(14.0, 14.0),
            );
            paint_entry_icon(ui.painter(), icon, true);
            let mut inner = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(row_rect)
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
            );
            inner.spacing_mut().item_spacing.x = 0.0;
            inner.add_space(icon_pad);
            inner.add_sized(
                [name_col - icon_pad, row_h],
                egui::Label::new(RichText::new("..").monospace().color(theme.text_primary))
                    .halign(egui::Align::LEFT)
                    .truncate(),
            );
            inner.add_space(6.0);
            inner.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.set_min_size(egui::vec2(size_col + time_col + 6.0, row_h));
                ui.add_sized(
                    [time_col, row_h],
                    egui::Label::new(RichText::new("—").color(theme.text_muted))
                        .halign(egui::Align::RIGHT),
                );
                ui.add_space(6.0);
                ui.add_sized(
                    [size_col, row_h],
                    egui::Label::new(RichText::new("—").color(theme.text_muted))
                        .halign(egui::Align::RIGHT),
                );
            });
            // 整行点击区：显式 ui.interact + 稳定 Id，且必须在列内容
            // 之后注册（后注册 widget 在顶层；见条目行注释）。
            let response = ui.interact(row_rect, row_id, egui::Sense::click());
            if response.clicked() {
                let parent = Self::parent_of(&self.current_path);
                *open_dir = Some(parent);
            }
            let parent = Self::parent_of(&self.current_path);
            response.context_menu(|ui| {
                ui.set_min_width(180.0);
                if ui.button("打开上级目录").clicked() {
                    *context_action = Some(ContextAction::Open(parent.clone()));
                    ui.close();
                }
                if ui.button("刷新").clicked() {
                    *context_action = Some(ContextAction::Refresh);
                    ui.close();
                }
            });
            return;
        }

        // 克隆一份可见条目，后续右键菜单闭包需要持有它，避免借用整个 entries。
        let entry = self.entries[idx - 1].clone();
        let selected = self.selected.iter().any(|name| name == &entry.name);
        let label = if entry.is_dir {
            format!("{}/", entry.name)
        } else {
            entry.name.clone()
        };
        // 整行点击区：单击选中，再次单击已选中的目录进入下级目录
        //（不用 double_clicked——egui 多击计数会被无关点击污染，
        // 双击时灵时不灵）。
        // 选中：accent 软底 + 左侧 accent 竖条；hover：白色低透明度
        // 叠加（Tabby 风，先画背景，列内容绘制在其上）。
        if selected {
            ui.painter().rect_filled(
                row_rect,
                crate::theme::tokens::RADIUS_ITEM,
                theme.accent_soft,
            );
            ui.painter().rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(row_rect.left() + 1.0, row_rect.top() + 4.0),
                    egui::pos2(row_rect.left() + 3.0, row_rect.bottom() - 4.0),
                ),
                1.5,
                theme.accent,
            );
        } else if pointer_in_row {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            ui.painter().rect_filled(
                row_rect,
                crate::theme::tokens::RADIUS_ITEM,
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 12),
            );
        }
        // 行首矢量图标：文件夹 accent2 填充 / 文件描边轮廓。
        let icon = egui::Rect::from_center_size(
            egui::pos2(row_rect.left() + 6.0 + 7.0, row_rect.center().y),
            egui::vec2(14.0, 14.0),
        );
        paint_entry_icon(ui.painter(), icon, entry.is_dir);
        // 三列内容：名称（目录主色/文件次要色）+ 大小/时间定宽右排。
        let name_text = RichText::new(label)
            .monospace()
            .color(if entry.is_dir || selected {
                theme.text_primary
            } else {
                theme.text_secondary
            });
        let weak_color = if selected {
            theme.text_secondary
        } else {
            theme.text_muted
        };
        let mut inner = ui.new_child(
            egui::UiBuilder::new()
                .id_salt(("sftp_cols", entry.name.as_str()))
                .max_rect(row_rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        // 归零自动间距（见表头注释）。
        inner.spacing_mut().item_spacing.x = 0.0;
        inner.add_space(icon_pad);
        inner.add_sized(
            [name_col - icon_pad, row_h],
            egui::Label::new(name_text)
                .halign(egui::Align::LEFT)
                .truncate(),
        );
        inner.add_space(6.0);
        inner.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.set_min_size(egui::vec2(size_col + time_col + 6.0, row_h));
            // 时间缺失时以 "—" 占位，保持列位稳定。
            let time_text = entry
                .modified
                .map(Self::format_time)
                .unwrap_or_else(|| "—".to_string());
            ui.add_sized(
                [time_col, row_h],
                egui::Label::new(RichText::new(time_text).color(weak_color))
                    .halign(egui::Align::RIGHT)
                    .truncate(),
            );
            ui.add_space(6.0);
            let size_text = if entry.is_dir {
                "—".to_string()
            } else {
                Self::format_size(entry.size)
            };
            ui.add_sized(
                [size_col, row_h],
                egui::Label::new(RichText::new(size_text).color(weak_color))
                    .halign(egui::Align::RIGHT)
                    .truncate(),
            );
        });
        // 整行点击区：显式 ui.interact + 稳定 Id，且必须在列内容
        // 之后注册（后注册 widget 在顶层）——allocate_exact_size
        // 的自动 Id 帧间漂移、new_child 子 Ui 叠加，行点击从未
        // 生效（"点击文件夹进不去"的根因）。
        let response = ui.interact(row_rect, row_id, egui::Sense::click());
        let modifiers = response.ctx.input(|input| input.modifiers);
        if response.secondary_clicked() {
            self.update_secondary_selection(idx, &entry.name, modifiers);
        } else if response.clicked() {
            let was_only_selected = self.selected.len() == 1
                && self
                    .selected
                    .first()
                    .is_some_and(|name| name == &entry.name);
            if entry.is_dir
                && was_only_selected
                && !modifiers.shift
                && !modifiers.command
                && !modifiers.ctrl
            {
                *open_dir = Some(self.join(&entry.name));
            } else {
                self.update_selection(idx, &entry.name, modifiers);
            }
        }

        let selected_names = self.selected.clone();
        let selected_count = selected_names.len();
        let entry_name = entry.name.clone();
        let entry_path = self.join(&entry_name);
        let terminal_cwd = terminal_cwd.map(str::to_string);
        response.context_menu(|ui| {
            ui.set_min_width(190.0);
            if entry.is_dir && selected_count == 1 && ui.button("打开目录").clicked() {
                *context_action = Some(ContextAction::Open(entry_path.clone()));
                ui.close();
            }
            if selected_count > 0 {
                let label = if selected_count == 1 {
                    "下载"
                } else {
                    "下载选中项"
                };
                if ui.button(label).clicked() {
                    *context_action = Some(ContextAction::Download(selected_names.clone()));
                    ui.close();
                }
            }
            if selected_count == 1 && ui.button("重命名").clicked() {
                *context_action = Some(ContextAction::Rename(entry_name.clone()));
                ui.close();
            }
            if selected_count > 0 {
                let label = if selected_count == 1 {
                    "删除"
                } else {
                    "删除选中项"
                };
                if ui.button(label).clicked() {
                    *context_action = Some(ContextAction::Delete(selected_names.clone()));
                    ui.close();
                }
            }
            ui.separator();
            if ui.button("新建文件夹").clicked() {
                *context_action = Some(ContextAction::Mkdir);
                ui.close();
            }
            if ui.button("上传文件").clicked() {
                *context_action = Some(ContextAction::Upload);
                ui.close();
            }
            if let Some(path) = terminal_cwd.as_deref() {
                if ui
                    .button("定位到当前终端目录  ⌘⇧L")
                    .on_hover_text(path)
                    .clicked()
                {
                    *context_action = Some(ContextAction::Locate(path.to_string()));
                    ui.close();
                }
            }
            if ui.button("刷新").clicked() {
                *context_action = Some(ContextAction::Refresh);
                ui.close();
            }
        });
    }

    /// 每帧渲染（无终端目录上下文时的测试/嵌入入口）。
    pub fn show(&mut self, ui: &mut Ui) {
        self.show_with_terminal_cwd(ui, None);
    }

    /// 每帧渲染，并接收当前终端已知的远程目录。
    pub fn show_with_terminal_cwd(&mut self, ui: &mut Ui, terminal_cwd: Option<&str>) {
        // 后台事件到达后请求重绘（传输进度/列表刷新不依赖其它重绘源）。
        if self.poll_events() {
            ui.ctx().request_repaint();
        }
        let theme = crate::theme::current_theme();

        // ⌘⇧L（Windows/Linux 为 Ctrl+Shift+L）定位到当前终端目录。
        let locate_shortcut = ui.input_mut(|input| {
            input.consume_key(
                egui::Modifiers {
                    shift: true,
                    command: true,
                    ..egui::Modifiers::NONE
                },
                egui::Key::L,
            )
        });
        let mut context_action = if locate_shortcut {
            terminal_cwd.map(|path| ContextAction::Locate(path.to_string()))
        } else {
            None
        };

        // ==================== 标题行：状态点 + 主机名 ====================
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let (dot, _) = ui.allocate_exact_size(egui::vec2(9.0, 9.0), egui::Sense::hover());
            ui.painter().circle_filled(dot.center(), 3.2, theme.accent2);
            ui.add_sized(
                [ui.available_width().max(0.0), 18.0],
                egui::Label::new(
                    RichText::new(format!("SFTP · {}", self.host_name))
                        .strong()
                        .size(12.5)
                        .color(theme.text_primary),
                )
                .truncate(),
            );
        });
        ui.add_space(8.0);

        // ==================== 路径栏（圆角地址条） ====================
        // 路径本身不再放置操作按钮；上级目录通过 `..` 行或空白处右键菜单进入。
        let bar_h = 22.0;
        let avail = ui.available_width().max(0.0);
        let (bar_rect, _) = ui.allocate_exact_size(egui::vec2(avail, bar_h), egui::Sense::hover());
        ui.painter().rect_filled(
            bar_rect,
            crate::theme::tokens::RADIUS_ITEM,
            theme.bg_elevated,
        );
        ui.painter().rect_stroke(
            bar_rect,
            crate::theme::tokens::RADIUS_ITEM,
            egui::Stroke::new(1.0, theme.border),
            egui::StrokeKind::Inside,
        );
        let mut inner = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(bar_rect.shrink2(egui::vec2(8.0, 0.0)))
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        inner.add_sized(
            [(bar_rect.width() - 16.0).max(0.0), bar_h],
            egui::Label::new(
                RichText::new(&self.current_path)
                    .monospace()
                    .size(11.5)
                    .color(theme.text_muted),
            )
            .truncate(),
        );

        // ==================== 错误提示 ====================
        if let Some(err) = &self.error {
            ui.colored_label(theme.danger, RichText::new(err).size(11.5));
        }
        if self.closed {
            ui.colored_label(theme.danger, RichText::new("连接已关闭").size(11.5));
        }

        // ==================== 文件列表 ====================
        // 用 '0' 字符宽（数字等宽字体的真实字宽）计算列宽：
        // 此前用 ' '（空格 ≈ 0.25em）低估了数字宽度（'0' ≈ 0.6em），
        // 导致时间列 "2026-08-14" 被截到 "2026-01"。
        // 字体启动时加载后不变，缓存到字段避免每帧 fonts_mut（Context 写锁）。
        if self.cell_width == 0.0 {
            self.cell_width = ui.fonts_mut(|f| {
                let font = egui::FontId::monospace(13.0);
                f.glyph_width(&font, '0')
            });
        }
        let cell_width = self.cell_width;
        // 行首图标占位宽度（16px 图标 + 两侧留白），表头与行共用基准。
        let icon_pad = 22.0;
        let row_h = 22.0;
        // 固定列宽：名称列占剩余宽度、大小/修改时间右对齐定宽。
        // 曾用"总宽 - 固定字符数"的 add_space 定位：长文件名会把
        // 大小/时间列挤出面板右缘，且各行列位随名称长度漂移错位。
        let size_col = cell_width * 12.0;
        let time_col = cell_width * 12.0;
        let table_width = ui.available_width();
        // 名称列让出两个 6px 间隔（名称|大小|时间），大小/时间定宽。
        let name_col = (table_width - size_col - time_col - 12.0).max(40.0);

        // 表头（固定不滚动，与行同列基准：名称左对齐、大小/时间定宽右排）。
        let (header_rect, _) =
            ui.allocate_exact_size(egui::vec2(table_width, row_h), egui::Sense::hover());
        let mut header = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(header_rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        // 归零自动间距：列间距完全由 add_space 精确控制
        // （否则子项间默认 8px item_spacing 会叠加，列位错乱）。
        header.spacing_mut().item_spacing.x = 0.0;
        header.add_space(icon_pad);
        header.add_sized(
            [name_col - icon_pad, row_h],
            egui::Label::new(
                RichText::new("名称")
                    .strong()
                    .size(11.0)
                    .color(theme.text_muted),
            )
            .halign(egui::Align::LEFT),
        );
        header.add_space(6.0);
        header.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.set_min_size(egui::vec2(size_col + time_col + 6.0, row_h));
            ui.add_sized(
                [time_col, row_h],
                egui::Label::new(
                    RichText::new("修改时间")
                        .strong()
                        .size(11.0)
                        .color(theme.text_muted),
                )
                .halign(egui::Align::RIGHT),
            );
            ui.add_space(6.0);
            ui.add_sized(
                [size_col, row_h],
                egui::Label::new(
                    RichText::new("大小")
                        .strong()
                        .size(11.0)
                        .color(theme.text_muted),
                )
                .halign(egui::Align::RIGHT),
            );
        });
        ui.separator();

        // 先注册列表背景，再注册具体行；后注册的行会覆盖背景的命中，
        // 因而空白处可右键而不会抢走文件行的点击。空目录提示也包含在
        // 这块背景里，用户不必精确点到列表下方才可打开菜单。
        let blank_rect = ui.available_rect_before_wrap();
        let blank_response = ui.interact(
            blank_rect,
            egui::Id::new("sftp_list_blank"),
            egui::Sense::click(),
        );
        if blank_response.clicked() {
            self.selected.clear();
            self.selection_anchor = None;
        }
        let blank_path = self.current_path.clone();
        let blank_terminal_cwd = terminal_cwd.map(str::to_string);
        blank_response.context_menu(|ui| {
            render_blank_context_menu(
                ui,
                &blank_path,
                blank_terminal_cwd.as_deref(),
                &mut context_action,
            );
        });

        // 加载中 / 空目录提示（滚动区外，随面板固定）。
        if self.loading {
            ui.horizontal(|ui| {
                // 静态指示点（不滚动动画）：egui `Spinner` 每帧 request_repaint
                // 强制 60fps 全帧重绘（含终端全量扫描），静态点零重绘。
                let (dot, _) = ui.allocate_exact_size(egui::vec2(9.0, 9.0), egui::Sense::hover());
                ui.painter().circle_filled(dot.center(), 3.2, theme.accent2);
                ui.label(RichText::new("加载中…").size(12.0).color(theme.text_muted));
            });
        } else if self.entries.is_empty() {
            ui.add_space(14.0);
            ui.vertical_centered(|ui| {
                let response = ui.add(
                    egui::Label::new(RichText::new("空目录").size(12.0).color(theme.text_muted))
                        .sense(egui::Sense::click()),
                );
                let empty_path = self.current_path.clone();
                let empty_terminal_cwd = terminal_cwd.map(str::to_string);
                response.context_menu(|ui| {
                    render_blank_context_menu(
                        ui,
                        &empty_path,
                        empty_terminal_cwd.as_deref(),
                        &mut context_action,
                    );
                });
            });
        }

        // 虚拟化列表：`show_rows` 只构建可见行（大目录不再每帧全量构建
        // String/RichText/Label/子 Ui）。index 0 = ".." 上级行，其余 = entries。
        let total_rows = self.entries.len() + 1;
        let mut open_dir: Option<String> = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_rows(ui, row_h, total_rows, |ui, row_range| {
                // 行间距归零：行高由 show_rows 精确分配，避免叠加默认间距错位。
                ui.spacing_mut().item_spacing.y = 0.0;
                for idx in row_range {
                    self.render_list_row(
                        ui,
                        idx,
                        table_width,
                        name_col,
                        size_col,
                        time_col,
                        icon_pad,
                        row_h,
                        theme,
                        &mut open_dir,
                        &mut context_action,
                        terminal_cwd,
                    );
                }
            });
        if let Some(path) = open_dir {
            self.navigate_to(&path);
        }

        if let Some(action) = context_action.take() {
            self.apply_context_action(action);
            ui.ctx().request_repaint();
        }

        // ==================== 上传进度区域 ====================
        // 只保留最近的传输记录（上限 12 条），上传单独放在带边框区域中，
        // 避免进度条被目录列表淹没。
        if self.transfers.len() > 12 {
            self.transfers.drain(..self.transfers.len() - 12);
        }
        let has_upload = self
            .transfers
            .iter()
            .any(|transfer| transfer.label.starts_with("上传 "));
        if has_upload {
            egui::Frame::new()
                .fill(theme.bg_elevated)
                .stroke(egui::Stroke::new(1.0, theme.border))
                .corner_radius(crate::theme::tokens::RADIUS_ITEM)
                .inner_margin(egui::Margin::same(8))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new("上传进度")
                            .strong()
                            .size(11.5)
                            .color(theme.text_primary),
                    );
                    for transfer in self
                        .transfers
                        .iter()
                        .filter(|transfer| transfer.label.starts_with("上传 "))
                    {
                        render_transfer_row(ui, transfer, theme);
                    }
                });
        }
        let has_other_transfer = self
            .transfers
            .iter()
            .any(|transfer| !transfer.label.starts_with("上传 "));
        if has_other_transfer {
            ui.add_space(6.0);
            egui::Frame::new()
                .fill(theme.bg_elevated)
                .stroke(egui::Stroke::new(1.0, theme.border))
                .corner_radius(crate::theme::tokens::RADIUS_ITEM)
                .inner_margin(egui::Margin::same(8))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new("传输记录")
                            .strong()
                            .size(11.5)
                            .color(theme.text_secondary),
                    );
                    for transfer in self
                        .transfers
                        .iter()
                        .filter(|transfer| !transfer.label.starts_with("上传 "))
                    {
                        render_transfer_row(ui, transfer, theme);
                    }
                });
        }
    }

    /// 执行列表右键菜单动作。
    fn apply_context_action(&mut self, action: ContextAction) {
        match action {
            ContextAction::Open(path) | ContextAction::Locate(path) => self.navigate_to(&path),
            ContextAction::Refresh => {
                let path = self.current_path.clone();
                self.handle.list(&path);
                self.loading = true;
            }
            ContextAction::Upload => self.upload_dialog(),
            ContextAction::Download(names) => self.download_selected(&names),
            ContextAction::Rename(name) => {
                self.dialog = Some(ConfirmDialog::Rename {
                    from: name.clone(),
                    path: self.join(&name),
                    input: name,
                });
            }
            ContextAction::Delete(names) => {
                let items = names
                    .iter()
                    .filter_map(|name| {
                        self.entries
                            .iter()
                            .find(|entry| entry.name == *name)
                            .map(|entry| DeleteTarget {
                                name: entry.name.clone(),
                                path: self.join(&entry.name),
                                is_dir: entry.is_dir,
                            })
                    })
                    .collect::<Vec<_>>();
                if !items.is_empty() {
                    self.dialog = Some(ConfirmDialog::Delete { items });
                }
            }
            ContextAction::Mkdir => {
                self.dialog = Some(ConfirmDialog::Mkdir {
                    path: self.current_path.clone(),
                    input: String::new(),
                });
            }
        }
    }

    /// 格式化时间戳为 UTC 日期（`YYYY-MM-DD`）。
    ///
    /// 用 Howard Hinnant `civil_from_days` 公版算法精确换算（无第三方依赖）——
    /// 曾用 `天/365 + 天%365/30` 近似：月长不均与闰年导致日期错位
    /// （如 1800000000 显示为 2027-01-29，实际应为 2027-01-15）。
    fn format_time(ts: u64) -> String {
        // Unix 纪元到公历纪元的偏移（1970-01-01 = 第 719468 天）。
        let z = (ts / 86400) as i64 + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = if m <= 2 { y + 1 } else { y };
        format!("{y}-{m:02}-{d:02}")
    }

    /// 上传：选择本地文件。
    fn upload_dialog(&mut self) {
        if let Some(path) = rfd::FileDialog::new().pick_file() {
            let name = path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".to_string());
            let label = format!("上传 {name}");
            let total = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
            let remote = self.join(&name);
            let id = self.handle.upload(&path, &remote);
            self.begin_transfer(id, label, total);
        }
    }

    /// 下载选中文件。单项选择保存文件，多项选择保存目录；目录本身暂不
    /// 递归下载，只会跳过并保留在远程列表中。
    fn download_selected(&mut self, names: &[String]) {
        if names.is_empty() {
            return;
        }
        let files = names
            .iter()
            .filter_map(|name| {
                self.entries
                    .iter()
                    .find(|entry| entry.name == *name && !entry.is_dir)
                    .map(|entry| (entry.name.clone(), entry.size))
            })
            .collect::<Vec<_>>();
        if files.is_empty() {
            self.error = Some("选中的项目没有可下载的文件".to_string());
            return;
        }

        if names.len() == 1 && files.len() == 1 {
            let (name, size) = &files[0];
            let Some(path) = rfd::FileDialog::new().set_file_name(name).save_file() else {
                return;
            };
            let id = self.handle.download(&self.join(name), &path);
            self.begin_transfer(id, format!("下载 {name}"), *size);
            return;
        }

        let Some(folder) = rfd::FileDialog::new().pick_folder() else {
            return;
        };
        for (name, size) in files {
            let id = self.handle.download(&self.join(&name), &folder.join(&name));
            self.begin_transfer(id, format!("下载 {name}"), size);
        }
    }

    /// 确认对话框渲染。
    pub fn show_dialog(&mut self, ctx: &egui::Context) {
        let mut close = false;
        let mut action: Option<ConfirmDialog> = None;
        if let Some(dialog) = &mut self.dialog {
            egui::Window::new(match dialog {
                ConfirmDialog::Delete { .. } => "确认删除",
                ConfirmDialog::Rename { .. } => "重命名",
                ConfirmDialog::Mkdir { .. } => "新建目录",
            })
            .resizable(false)
            .collapsible(false)
            .show(ctx, |ui| match dialog {
                ConfirmDialog::Delete { items } => {
                    if items.len() == 1 {
                        let item = &items[0];
                        ui.label(format!(
                            "确定删除{} {}？此操作不可恢复。",
                            if item.is_dir { "目录" } else { "文件" },
                            item.name
                        ));
                    } else {
                        ui.label(format!("确定删除 {} 个项目？此操作不可恢复。", items.len()));
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("删除").clicked() {
                            action = Some(dialog.clone());
                            close = true;
                        }
                        if ui.button("取消").clicked() {
                            close = true;
                        }
                    });
                }
                ConfirmDialog::Rename { input, .. } => {
                    ui.label("新名称：");
                    ui.text_edit_singleline(input);
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("确定").clicked() {
                            action = Some(dialog.clone());
                            close = true;
                        }
                        if ui.button("取消").clicked() {
                            close = true;
                        }
                    });
                }
                ConfirmDialog::Mkdir { input, .. } => {
                    ui.label("目录名称：");
                    ui.text_edit_singleline(input);
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("确定").clicked() {
                            action = Some(dialog.clone());
                            close = true;
                        }
                        if ui.button("取消").clicked() {
                            close = true;
                        }
                    });
                }
            });
        }
        if close {
            self.dialog = None;
        }
        if let Some(dialog) = action {
            match dialog {
                ConfirmDialog::Delete { items } => {
                    for item in items {
                        self.handle.remove(&item.path, item.is_dir);
                    }
                }
                ConfirmDialog::Rename { path, input, .. } => {
                    let new_path = self.join(&input);
                    if new_path != path && !input.trim().is_empty() {
                        self.handle.rename(&path, &new_path);
                    }
                }
                ConfirmDialog::Mkdir { input, .. } => {
                    if !input.trim().is_empty() {
                        self.handle.mkdir(&self.join(input.trim()));
                    }
                }
            }
        }
    }

    /// 会话关闭时调用。
    pub fn close(&self) {
        self.handle.close();
    }
}

/// 绘制空白处的统一右键菜单。
fn render_blank_context_menu(
    ui: &mut egui::Ui,
    current_path: &str,
    terminal_cwd: Option<&str>,
    action: &mut Option<ContextAction>,
) {
    ui.set_min_width(190.0);
    if ui.button("上传文件").clicked() {
        *action = Some(ContextAction::Upload);
        ui.close();
    }
    if ui.button("新建文件夹").clicked() {
        *action = Some(ContextAction::Mkdir);
        ui.close();
    }
    if ui.button("刷新").clicked() {
        *action = Some(ContextAction::Refresh);
        ui.close();
    }
    let parent = SftpView::parent_of(current_path);
    if ui.button("打开上级目录").clicked() {
        *action = Some(ContextAction::Open(parent));
        ui.close();
    }
    if let Some(path) = terminal_cwd {
        if ui
            .button("定位到当前终端目录  ⌘⇧L")
            .on_hover_text(path)
            .clicked()
        {
            *action = Some(ContextAction::Locate(path.to_string()));
            ui.close();
        }
    }
}

/// 渲染一条带状态、文件名与字节数的传输记录。
fn render_transfer_row(
    ui: &mut egui::Ui,
    transfer: &Transfer,
    theme: &'static crate::theme::Theme,
) {
    let (color, status) = if transfer.failed {
        (theme.danger, "失败")
    } else if transfer.finished {
        (theme.success, "完成")
    } else {
        (theme.accent2, "进行中")
    };
    ui.colored_label(color, RichText::new(status).size(11.5));
    ui.add(
        egui::Label::new(
            RichText::new(&transfer.label)
                .size(11.5)
                .color(theme.text_secondary),
        )
        .truncate(),
    );
    let progress = if transfer.total == 0 {
        if transfer.finished {
            1.0
        } else {
            0.0
        }
    } else {
        (transfer.done as f32 / transfer.total as f32).clamp(0.0, 1.0)
    };
    ui.add(
        egui::ProgressBar::new(progress)
            .desired_width(ui.available_width().max(80.0))
            .text(format!(
                "{} / {}",
                SftpView::format_size(transfer.done),
                SftpView::format_size(transfer.total)
            )),
    );
}

/// 行首文件/目录矢量图标：文件夹为 accent2 填充（提手 + 圆角主体），
/// 文件为细描边轮廓。矢量绘制避免 emoji 字形随字体变化。
fn paint_entry_icon(painter: &egui::Painter, rect: egui::Rect, is_dir: bool) {
    let theme = crate::theme::current_theme();
    if is_dir {
        let h = rect.height();
        let tab = egui::Rect::from_min_size(
            egui::pos2(rect.left(), rect.top() + h * 0.2),
            egui::vec2(rect.width() * 0.52, h * 0.26),
        );
        let body = egui::Rect::from_min_max(
            egui::pos2(rect.left(), rect.top() + h * 0.3),
            egui::pos2(rect.right(), rect.bottom()),
        );
        painter.rect_filled(tab, 1.5, theme.accent2);
        painter.rect_filled(body, 2.0, theme.accent2);
    } else {
        painter.rect_stroke(
            rect.shrink(1.0),
            2.0,
            egui::Stroke::new(1.0, theme.text_muted),
            egui::StrokeKind::Inside,
        );
    }
}

impl Clone for ConfirmDialog {
    fn clone(&self) -> Self {
        match self {
            ConfirmDialog::Delete { items } => ConfirmDialog::Delete {
                items: items.clone(),
            },
            ConfirmDialog::Rename { from, path, input } => ConfirmDialog::Rename {
                from: from.clone(),
                path: path.clone(),
                input: input.clone(),
            },
            ConfirmDialog::Mkdir { path, input } => ConfirmDialog::Mkdir {
                path: path.clone(),
                input: input.clone(),
            },
        }
    }
}

/// 路径工具（供测试使用）。
pub fn parent_of(path: &str) -> String {
    SftpView::parent_of(path)
}

/// 路径拼接（供测试使用）。
pub fn join_path(parent: &str, name: &str) -> String {
    if parent.ends_with('/') {
        format!("{parent}{name}")
    } else {
        format!("{parent}/{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试 sshd 配置（与 mino-core 集成测试一致）。
    fn test_profile() -> mino_core::config::HostProfile {
        use mino_core::config::Auth;
        let key_path = std::env::var("MINO_TEST_KEY").unwrap_or_else(|_| {
            format!(
                "{}/.ssh/id_ed25519",
                std::env::var("HOME").unwrap_or_default()
            )
        });
        mino_core::config::HostProfile {
            name: "UI 测试".into(),
            host: std::env::var("MINO_TEST_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
            port: std::env::var("MINO_TEST_PORT")
                .unwrap_or_else(|_| "2222".into())
                .parse()
                .unwrap(),
            user: std::env::var("MINO_TEST_USER")
                .unwrap_or_else(|_| std::env::var("USER").unwrap_or_else(|_| "root".into())),
            auth: Auth::Key {
                path: key_path.into(),
                passphrase: None,
            },
        }
    }

    /// SFTP 面板真实连接并渲染文件列表。
    #[test]
    fn sftp_面板真实连接渲染() {
        use mino_core::ssh::sftp::connect_sftp;
        use std::time::{Duration, Instant};

        // 测试 sshd 的 known_hosts 与 hostkey 同目录（/tmp/mino-test-sshd），
        // hostkey 重建时指纹记录一并消失，不会旧指纹不匹配导致测试失败。
        // call_once：测试并行运行时不重复设置环境变量。
        static KNOWN_HOSTS_INIT: std::sync::Once = std::sync::Once::new();
        KNOWN_HOSTS_INIT.call_once(|| {
            std::env::set_var("MINO_KNOWN_HOSTS", "/tmp/mino-test-sshd/known_hosts.toml");
        });

        // 测试 sshd 不可达时跳过（CI 无测试 sshd）。
        if std::net::TcpStream::connect_timeout(
            &"127.0.0.1:2222".parse().unwrap(),
            Duration::from_millis(500),
        )
        .is_err()
        {
            eprintln!("跳过：测试 sshd 未运行");
            return;
        }

        let profile = test_profile();
        if let mino_core::config::Auth::Key { path, .. } = &profile.auth {
            if !path.exists() {
                eprintln!("跳过：测试私钥不存在");
                return;
            }
        }
        let (_thread, handle, rx) = connect_sftp(&profile);
        let mut view = SftpView::new("UI 测试主机", handle, rx);

        // 轮询等待目录列表加载完成。
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            view.poll_events();
            if !view.entries.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            !view.entries.is_empty(),
            "SFTP 目录列表应为空目录列表（加载完成）"
        );

        // 渲染并断言条目可见。
        use kittest::Queryable;
        let mut harness = egui_kittest::Harness::new_ui(|ui| {
            view.show(ui);
        });
        harness.run();
        harness.get_by_label("..");
        // 面板标题包含主机名。
        harness.get_by_label("SFTP · UI 测试主机");
    }

    /// 确认对话框流程：选中条目 → 点击删除 → 出现确认框。
    #[test]
    fn 删除确认对话框流程() {
        use kittest::Queryable;

        // 直接构造带条目的面板（不依赖网络）。
        let (_tx, rx) = tokio::sync::mpsc::channel(128);
        let (handle_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = SftpHandle::from_raw(handle_tx);
        let mut view = SftpView {
            host_name: "测试主机".into(),
            handle,
            rx,
            current_path: "/".into(),
            entries: vec![
                RemoteEntry {
                    name: "Desktop".into(),
                    is_dir: true,
                    size: 0,
                    modified: None,
                    permissions: 0,
                },
                RemoteEntry {
                    name: "readme.md".into(),
                    is_dir: false,
                    size: 100,
                    modified: None,
                    permissions: 0,
                },
            ],
            selected: vec!["readme.md".into()],
            selection_anchor: Some("readme.md".into()),
            loading: false,
            transfers: Vec::new(),
            dialog: None,
            error: None,
            closed: false,
            cell_width: 0.0,
        };

        let mut harness = egui_kittest::Harness::new_ui(|ui| {
            let ctx = ui.ctx().clone();
            view.show(ui);
            view.show_dialog(&ctx);
        });
        harness.run();

        // 右键条目 → 菜单中的删除 → 出现确认对话框。
        harness.get_by_label("readme.md").click_secondary();
        harness.run_steps(6);
        harness.get_by_label("删除").click();
        harness.run_steps(6);
        harness.get_by_label("确认删除");
        harness.get_by_label("readme.md");

        // 点击取消 → 对话框关闭。
        harness.get_by_label("取消").click();
        harness.run();
        assert!(
            harness.root().query_by_label("确认删除").is_none(),
            "取消后对话框应关闭"
        );
    }

    /// Shift 选择范围后，右键菜单应把多选操作显示为批量动作。
    #[test]
    fn shift多选右键批量菜单() {
        use kittest::Queryable;

        let (_tx, rx) = tokio::sync::mpsc::channel(128);
        let (handle_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = SftpHandle::from_raw(handle_tx);
        let mut view = SftpView {
            host_name: "测试主机".into(),
            handle,
            rx,
            current_path: "/".into(),
            entries: vec![
                RemoteEntry {
                    name: "a.txt".into(),
                    is_dir: false,
                    size: 1,
                    modified: None,
                    permissions: 0,
                },
                RemoteEntry {
                    name: "b.txt".into(),
                    is_dir: false,
                    size: 2,
                    modified: None,
                    permissions: 0,
                },
                RemoteEntry {
                    name: "folder".into(),
                    is_dir: true,
                    size: 0,
                    modified: None,
                    permissions: 0,
                },
            ],
            selected: vec![],
            selection_anchor: None,
            loading: false,
            transfers: Vec::new(),
            dialog: None,
            error: None,
            closed: false,
            cell_width: 0.0,
        };
        let mut harness = egui_kittest::Harness::new_ui(|ui| view.show(ui));
        harness.run();

        harness.get_by_label("a.txt").click();
        harness.run_steps(2);
        harness
            .get_by_label("folder/")
            .click_modifiers(egui::Modifiers::SHIFT);
        harness.run_steps(2);
        harness.get_by_label("b.txt").click_secondary();
        harness.run_steps(4);

        harness.get_by_label("删除选中项");
        harness.get_by_label("下载选中项");
    }

    /// 上传事件应落在独立的上传进度区域，而不是只显示一条状态文字。
    #[test]
    fn 上传进度独立区域() {
        use kittest::Queryable;

        let (event_tx, rx) = tokio::sync::mpsc::channel(128);
        let (handle_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = SftpHandle::from_raw(handle_tx);
        let mut view = SftpView {
            host_name: "测试主机".into(),
            handle,
            rx,
            current_path: "/home/test".into(),
            entries: Vec::new(),
            selected: vec![],
            selection_anchor: None,
            loading: false,
            transfers: Vec::new(),
            dialog: None,
            error: None,
            closed: false,
            cell_width: 0.0,
        };
        event_tx
            .try_send(SftpEvent::Progress {
                id: 1,
                label: "上传 demo.bin".into(),
                done: 512,
                total: 1024,
            })
            .unwrap();

        let mut harness = egui_kittest::Harness::new_ui(|ui| view.show(ui));
        harness.run_steps(2);
        harness.get_by_label("上传进度");
        harness.get_by_label("上传 demo.bin");
    }

    #[test]
    fn 相同文件名传输按标识隔离状态() {
        let (event_tx, rx) = tokio::sync::mpsc::channel(128);
        let (handle_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut view = SftpView::new("测试主机", SftpHandle::from_raw(handle_tx), rx);
        event_tx
            .try_send(SftpEvent::Progress {
                id: 1,
                label: "上传 same.bin".into(),
                done: 10,
                total: 100,
            })
            .unwrap();
        event_tx
            .try_send(SftpEvent::Progress {
                id: 2,
                label: "上传 same.bin".into(),
                done: 20,
                total: 200,
            })
            .unwrap();
        event_tx
            .try_send(SftpEvent::Done {
                id: Some(1),
                label: "上传 same.bin".into(),
            })
            .unwrap();
        event_tx
            .try_send(SftpEvent::Error {
                id: Some(2),
                label: "上传 same.bin".into(),
                message: "失败".into(),
            })
            .unwrap();

        assert!(view.poll_events());
        assert_eq!(view.transfers.len(), 2);
        let first = view
            .transfers
            .iter()
            .find(|transfer| transfer.id == 1)
            .unwrap();
        assert!(first.finished && !first.failed && first.done == 10);
        let second = view
            .transfers
            .iter()
            .find(|transfer| transfer.id == 2)
            .unwrap();
        assert!(!second.finished && second.failed && second.done == 20);
    }

    /// ⌘⇧L 应直接发出当前终端目录的列表请求。
    #[test]
    fn 快捷键定位终端目录() {
        use mino_core::ssh::sftp::SftpCmd;

        let (_tx, rx) = tokio::sync::mpsc::channel(128);
        let (handle_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = SftpHandle::from_raw(handle_tx);
        let mut view = SftpView {
            host_name: "测试主机".into(),
            handle,
            rx,
            current_path: "/".into(),
            entries: Vec::new(),
            selected: vec![],
            selection_anchor: None,
            loading: false,
            transfers: Vec::new(),
            dialog: None,
            error: None,
            closed: false,
            cell_width: 0.0,
        };
        let mut harness = egui_kittest::Harness::new_ui(|ui| {
            view.show_with_terminal_cwd(ui, Some("/srv/project"));
        });
        harness.run_steps(2);
        harness.event(egui::Event::Key {
            key: egui::Key::L,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers {
                command: true,
                shift: true,
                ..egui::Modifiers::NONE
            },
        });
        harness.run_steps(2);

        let cmd = cmd_rx.try_recv().expect("快捷键应发出目录定位请求");
        assert!(matches!(cmd, SftpCmd::List { path } if path == "/srv/project"));
    }

    /// 目录列表空白区域右键应提供新建文件夹等操作。
    #[test]
    fn 空白处右键新建文件夹菜单() {
        use kittest::Queryable;

        let (_tx, rx) = tokio::sync::mpsc::channel(128);
        let (handle_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = SftpHandle::from_raw(handle_tx);
        let mut view = SftpView {
            host_name: "测试主机".into(),
            handle,
            rx,
            current_path: "/".into(),
            entries: Vec::new(),
            selected: vec![],
            selection_anchor: None,
            loading: false,
            transfers: Vec::new(),
            dialog: None,
            error: None,
            closed: false,
            cell_width: 0.0,
        };
        let mut harness = egui_kittest::Harness::new_ui(|ui| view.show(ui));
        harness.run_steps(2);
        harness.get_by_label("空目录").click_secondary();
        harness.run_steps(4);
        harness.get_by_label("新建文件夹");
        harness.get_by_label("上传文件");
    }

    /// 渲染级回归：340 宽（比 40% 默认更窄的保守下界）下地址栏和标题
    /// 不越出面板边界；操作入口由右键菜单提供。
    #[test]
    fn sftp面板窄宽不截断() {
        use kittest::Queryable;

        let (_tx, rx) = tokio::sync::mpsc::channel(128);
        let (handle_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = SftpHandle::from_raw(handle_tx);
        let mut view = SftpView {
            host_name: "测试主机".into(),
            handle,
            rx,
            current_path: "/very/long/remote/path/that/overflows/".into(),
            entries: Vec::new(),
            selected: vec![],
            selection_anchor: None,
            loading: false,
            transfers: Vec::new(),
            dialog: None,
            error: None,
            closed: false,
            cell_width: 0.0,
        };

        // 模拟 340 宽面板（app.rs sftp_frame 左右内边距各 12；默认宽为
        // 窗口 40%，340 是更窄的保守回归值）。
        const PANEL_W: f32 = 340.0;
        const MARGIN_X: f32 = 12.0;
        let mut harness = egui_kittest::Harness::new_ui(move |ui| {
            ui.set_max_width(PANEL_W - MARGIN_X * 2.0);
            view.show(ui);
        });
        harness.run();

        let title = harness.get_by_label("SFTP · 测试主机").rect();
        assert!(title.right() <= PANEL_W - MARGIN_X + 0.5);
        harness.get_by_label("..");
    }

    /// 单击选中，再次单击已选中的目录进入下级目录（发 List 命令）。
    /// 回归：曾用 response.double_clicked()，egui 多击计数被无关点击
    /// 污染导致"点击文件夹进不去"。
    #[test]
    fn 单击选中再次单击进入目录() {
        use kittest::Queryable;
        use mino_core::ssh::sftp::SftpCmd;

        let (_tx, rx) = tokio::sync::mpsc::channel(128);
        let (handle_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = SftpHandle::from_raw(handle_tx);
        let mut view = SftpView {
            host_name: "测试主机".into(),
            handle,
            rx,
            current_path: "/".into(),
            entries: vec![
                RemoteEntry {
                    name: "workspace".into(),
                    is_dir: true,
                    size: 0,
                    modified: None,
                    permissions: 0,
                },
                RemoteEntry {
                    name: "notes.txt".into(),
                    is_dir: false,
                    size: 12,
                    modified: None,
                    permissions: 0,
                },
            ],
            selected: vec![],
            selection_anchor: None,
            loading: false,
            transfers: Vec::new(),
            dialog: None,
            error: None,
            closed: false,
            cell_width: 0.0,
        };

        let mut harness = egui_kittest::Harness::new_ui(|ui| {
            view.show(ui);
        });
        harness.run();

        // 第一次单击：只选中，不进入目录。
        harness.get_by_label("workspace/").click();
        harness.run();
        assert!(
            cmd_rx.try_recv().is_err(),
            "第一次单击只应选中，不应发出进入目录命令"
        );

        // 再次单击同一目录 → 进入下级目录。进入后 loading=true 出现
        // spinner 持续重绘，harness.run() 会超 max_steps，用显式步进。
        harness.get_by_label("workspace/").click();
        harness.run_steps(6);
        let cmd = cmd_rx.try_recv().expect("再次单击应发出进入目录命令");
        assert!(
            matches!(&cmd, SftpCmd::List { path } if path == "/workspace"),
            "进入的路径应为 /workspace，收到 {cmd:?}"
        );

        // 文件不受影响：单击两次也只是选中，不导航。
        //（进入目录后 loading 一直为 true，全程用显式步进。）
        harness.get_by_label("notes.txt").click();
        harness.run_steps(6);
        harness.get_by_label("notes.txt").click();
        harness.run_steps(6);
        assert!(
            cmd_rx.try_recv().is_err(),
            "文件不应发出任何命令（无目录可进）"
        );
    }

    #[test]
    fn 路径拼接与上级目录() {
        assert_eq!(join_path("/home/user", "file.txt"), "/home/user/file.txt");
        assert_eq!(join_path("/", "etc"), "/etc");
        assert_eq!(join_path("/home/user/", "a"), "/home/user/a");
        assert_eq!(parent_of("/home/user"), "/home");
        assert_eq!(parent_of("/home"), "/");
        assert_eq!(parent_of("/"), "/");
    }

    /// 文件列表列对齐：大小/时间列位置固定，不随名称长度漂移；
    /// 长文件名不越入大小列（回归测试：曾用"总宽-固定字符数"
    /// add_space 定位，长文件名把时间列挤出面板右缘且各行列位错乱）。
    #[test]
    fn 文件列表列对齐() {
        use kittest::Queryable;

        let (_tx, rx) = tokio::sync::mpsc::channel(128);
        let (handle_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = SftpHandle::from_raw(handle_tx);
        let mut view = SftpView {
            host_name: "测试主机".into(),
            handle,
            rx,
            current_path: "/".into(),
            entries: vec![
                RemoteEntry {
                    name: "defaultUploadFolder".into(),
                    is_dir: true,
                    size: 0,
                    modified: Some(1_800_000_000),
                    permissions: 0,
                },
                RemoteEntry {
                    name: "a".into(),
                    is_dir: false,
                    size: 5_000_000_000,
                    modified: Some(1_800_000_000),
                    permissions: 0,
                },
            ],
            selected: vec![],
            selection_anchor: None,
            loading: false,
            transfers: Vec::new(),
            dialog: None,
            error: None,
            closed: false,
            cell_width: 0.0,
        };

        let mut harness = egui_kittest::Harness::new_ui(|ui| {
            view.show(ui);
        });
        harness.run();

        // 时间列：两行（长名/短名）框位置一致，不随名称长度漂移。
        let times = harness
            .root()
            .query_all_by_label("2027-01-15")
            .collect::<Vec<_>>();
        assert_eq!(times.len(), 2, "两行都应显示修改时间");
        assert_eq!(
            times[0].rect().left(),
            times[1].rect().left(),
            "时间列左缘应一致"
        );
        assert_eq!(
            times[0].rect().right(),
            times[1].rect().right(),
            "时间列右缘应一致"
        );

        // ".." 行已渲染（返回上级目录入口）。
        harness.get_by_label("..");

        // 大小列：".." 行与目录各占一个 "—"（创建顺序：.. 时间、.. 大小、
        // 目录大小，最后一个即目录行的大小列），与文件 "4.7 GB" 框位置一致。
        let dashes: Vec<_> = harness.root().query_all_by_label("—").collect();
        assert_eq!(dashes.len(), 3, ".. 行与目录行的占位 — 共 3 个");
        let dir_size = dashes[2].rect();
        let file_size = harness.get_by_label("4.7 GB").rect();
        assert_eq!(dir_size.left(), file_size.left(), "大小列左缘应一致");

        // 长文件名不越入大小列（名称列与大小列之间留 6px 间隔）。
        let name = harness.get_by_label("defaultUploadFolder/").rect();
        assert!(
            name.right() + 6.0 <= file_size.left(),
            "长文件名不应越入大小列"
        );
        assert!(
            name.right() + 6.0 <= file_size.left(),
            "长文件名不应越入大小列"
        );
    }

    #[test]
    fn 大小格式化() {
        assert_eq!(SftpView::format_size(0), "0 B");
        assert_eq!(SftpView::format_size(1024), "1.0 KB");
        assert_eq!(SftpView::format_size(1024 * 1024), "1.0 MB");
        assert_eq!(SftpView::format_size(1024 * 1024 * 1024), "1.0 GB");
    }

    /// 时间戳 → UTC 日期精确换算（回归测试：曾用 365/30 近似致日期错位）。
    #[test]
    fn 时间戳日期精确换算() {
        assert_eq!(SftpView::format_time(0), "1970-01-01");
        assert_eq!(SftpView::format_time(86_400), "1970-01-02");
        // 闰日（2000-02-29 00:00:00 UTC）。
        assert_eq!(SftpView::format_time(951_782_400), "2000-02-29");
        assert_eq!(SftpView::format_time(1_800_000_000), "2027-01-15");
        // 闰年后一天（2000-03-01）。
        assert_eq!(SftpView::format_time(951_868_800), "2000-03-01");
    }
}
