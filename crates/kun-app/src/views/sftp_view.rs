//! SFTP 面板：远程文件浏览、传输进度与文件操作。

use egui::{RichText, Ui};
use kun_core::ssh::sftp::{RemoteEntry, SftpEvent, SftpHandle};
use tokio::sync::mpsc::UnboundedReceiver;

/// 传输任务（进度条显示）。
#[derive(Clone)]
struct Transfer {
    label: String,
    done: u64,
    total: u64,
    finished: bool,
    failed: bool,
}

/// 文件操作确认对话框。
enum ConfirmDialog {
    Delete {
        name: String,
        path: String,
        is_dir: bool,
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

/// SFTP 面板状态。
pub struct SftpView {
    /// 主机名称（显示用）。
    host_name: String,
    handle: SftpHandle,
    rx: UnboundedReceiver<SftpEvent>,
    /// 当前远程路径。
    current_path: String,
    /// 当前目录条目。
    entries: Vec<RemoteEntry>,
    /// 选中条目。
    selected: Option<String>,
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
}

impl SftpView {
    /// 创建 SFTP 面板（连接就绪后）。
    pub fn new(host_name: &str, handle: SftpHandle, rx: UnboundedReceiver<SftpEvent>) -> Self {
        let view = Self {
            host_name: host_name.to_string(),
            handle,
            rx,
            current_path: "/".to_string(),
            entries: Vec::new(),
            selected: None,
            loading: true,
            transfers: Vec::new(),
            dialog: None,
            error: None,
            closed: false,
        };
        // 初始列出根目录。
        view.handle.list("/");
        view
    }

    /// 会话引用（供状态栏等读取标题）。
    pub fn host_name(&self) -> &str {
        &self.host_name
    }

    /// 处理后台事件。
    pub(crate) fn poll_events(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            match event {
                SftpEvent::Listed { path, entries } => {
                    self.current_path = path;
                    self.entries = entries;
                    self.loading = false;
                }
                SftpEvent::Progress { label, done, total } => {
                    if let Some(t) = self.transfers.iter_mut().find(|t| t.label == label) {
                        t.done = done;
                        t.total = total;
                    } else {
                        self.transfers.push(Transfer {
                            label,
                            done,
                            total,
                            finished: false,
                            failed: false,
                        });
                    }
                }
                SftpEvent::Done { label } => {
                    if let Some(t) = self.transfers.iter_mut().find(|t| t.label == label) {
                        t.finished = true;
                    } else {
                        self.transfers.push(Transfer {
                            label,
                            done: 1,
                            total: 1,
                            finished: true,
                            failed: false,
                        });
                    }
                    // 操作完成后刷新目录。
                    self.handle.list(&self.current_path);
                }
                SftpEvent::Error { label, message } => {
                    self.error = Some(format!("{label}：{message}"));
                    if let Some(t) = self.transfers.iter_mut().find(|t| t.label == label) {
                        t.failed = true;
                    }
                    self.loading = false;
                }
                SftpEvent::Closed => {
                    self.closed = true;
                }
                _ => {}
            }
        }
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

    /// 每帧渲染。
    pub fn show(&mut self, ui: &mut Ui) {
        self.poll_events();
        let theme = crate::theme::current_theme();

        // ==================== 标题行：状态点 + 主机名 + 刷新 ====================
        // 标题占剩余宽度超长截断、刷新按钮锚定右缘（分居两端不重叠）。
        let refresh_clicked = ui
            .horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                // accent2 状态点（与底部状态栏同款）。
                let (dot, _) = ui.allocate_exact_size(egui::vec2(9.0, 9.0), egui::Sense::hover());
                ui.painter().circle_filled(dot.center(), 3.2, theme.accent2);
                let mut clicked = false;
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    clicked = sftp_tool_button(ui, "刷新")
                        .on_hover_text("刷新目录")
                        .clicked();
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
                clicked
            })
            .inner;
        if refresh_clicked {
            self.handle.list(&self.current_path);
            self.loading = true;
        }
        ui.add_space(8.0);

        // ==================== 操作行 ====================
        // 空间不足时自动换行（horizontal_wrapped）：按钮不截断、不互相重叠。
        let mut upload = false;
        let mut download = false;
        let mut mkdir = false;
        let mut rename = false;
        let mut delete = false;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            upload = sftp_tool_button(ui, "上传")
                .on_hover_text("上传文件")
                .clicked();
            download = sftp_tool_button(ui, "下载")
                .on_hover_text("下载选中文件")
                .clicked();
            mkdir = sftp_tool_button(ui, "新建目录").clicked();
            rename = sftp_tool_button(ui, "重命名").clicked();
            delete = sftp_tool_button(ui, "删除").clicked();
        });
        if upload {
            self.upload_dialog();
        }
        if download {
            self.download_selected();
        }
        if mkdir {
            self.dialog = Some(ConfirmDialog::Mkdir {
                path: self.join(""),
                input: String::new(),
            });
        }
        if rename {
            if let Some(name) = self.selected.clone() {
                self.dialog = Some(ConfirmDialog::Rename {
                    from: name.clone(),
                    path: self.join(&name),
                    input: name,
                });
            }
        }
        if delete {
            if let Some(name) = self.selected.clone() {
                let entry = self.entries.iter().find(|e| e.name == name);
                self.dialog = Some(ConfirmDialog::Delete {
                    name: name.clone(),
                    path: self.join(&name),
                    is_dir: entry.map(|e| e.is_dir).unwrap_or(false),
                });
            }
        }
        ui.add_space(10.0);

        // ==================== 路径栏（圆角地址条） ====================
        // 路径放在 bg_elevated 圆角条里，超长截断（曾溢出面板右缘被裁剪）。
        let up_clicked = ui
            .horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let clicked = sftp_tool_button(ui, "上级")
                    .on_hover_text("返回上级目录")
                    .clicked();
                let bar_h = 22.0;
                let avail = ui.available_width().max(0.0);
                let (bar_rect, _) =
                    ui.allocate_exact_size(egui::vec2(avail, bar_h), egui::Sense::hover());
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
                clicked
            })
            .inner;
        if up_clicked {
            let parent = Self::parent_of(&self.current_path);
            self.handle.list(&parent);
            self.loading = true;
        }

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
        let cell_width = ui.fonts_mut(|f| {
            let font = egui::FontId::monospace(13.0);
            f.glyph_width(&font, '0')
        });
        // 行首图标占位宽度（16px 图标 + 两侧留白），表头与行共用基准。
        let icon_pad = 22.0;
        let row_h = 22.0;
        // 固定列宽：名称列占剩余宽度、大小/修改时间右对齐定宽。
        // 曾用"总宽 - 固定字符数"的 add_space 定位：长文件名会把
        // 大小/时间列挤出面板右缘，且各行列位随名称长度漂移错位。
        let size_col = cell_width * 12.0;
        let time_col = cell_width * 12.0;

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let table_width = ui.available_width();
                // 名称列让出两个 6px 间隔（名称|大小|时间），大小/时间定宽。
                let name_col = (table_width - size_col - time_col - 12.0).max(40.0);

                if self.loading {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new("加载中…").size(12.0).color(theme.text_muted));
                    });
                }
                // 表头（与行同列基准：名称左对齐、大小/时间定宽右排）。
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

                // ".." 行：返回上级目录（文件管理器通用习惯，导航更直观）。
                {
                    let (row_rect, _) = ui
                        .allocate_exact_size(egui::vec2(table_width, row_h), egui::Sense::hover());
                    // hover 用指针位置判定（子 Ui 会抢走 response.hovered()）。
                    let pointer_in_row =
                        ui.input(|i| i.pointer.hover_pos().is_some_and(|p| row_rect.contains(p)));
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
                    let response = ui.interact(
                        row_rect,
                        egui::Id::new(("sftp_row", "..")),
                        egui::Sense::click(),
                    );
                    if response.clicked() {
                        let parent = Self::parent_of(&self.current_path);
                        self.handle.list(&parent);
                        self.loading = true;
                        self.selected = None;
                    }
                }

                let mut open_dir: Option<String> = None;
                let mut select: Option<String> = None;
                for entry in &self.entries {
                    let selected = self.selected.as_deref() == Some(entry.name.as_str());
                    let label = if entry.is_dir {
                        format!("{}/", entry.name)
                    } else {
                        entry.name.clone()
                    };
                    // 整行点击区：单击选中，再次单击已选中的目录进入下级目录
                    //（不用 double_clicked——egui 多击计数会被无关点击污染，
                    // 双击时灵时不灵）。
                    let (row_rect, _) = ui
                        .allocate_exact_size(egui::vec2(table_width, row_h), egui::Sense::hover());
                    let row_id = egui::Id::new(("sftp_row", entry.name.as_str()));
                    // hover 用指针位置判定（子 Ui 会抢走 response.hovered()）。
                    let pointer_in_row =
                        ui.input(|i| i.pointer.hover_pos().is_some_and(|p| row_rect.contains(p)));
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
                    let name_text =
                        RichText::new(label)
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
                    if response.clicked() {
                        if entry.is_dir && self.selected.as_deref() == Some(entry.name.as_str()) {
                            open_dir = Some(entry.name.clone());
                        } else {
                            select = Some(entry.name.clone());
                        }
                    }
                }
                if let Some(name) = open_dir {
                    let path = self.join(&name);
                    self.handle.list(&path);
                    self.loading = true;
                    self.selected = None;
                }
                if let Some(name) = select {
                    self.selected = Some(name);
                }
                if self.entries.is_empty() && !self.loading {
                    ui.add_space(14.0);
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new("空目录").size(12.0).color(theme.text_muted));
                    });
                }
            });

        // ==================== 传输进度 ====================
        if !self.transfers.is_empty() {
            ui.separator();
            ui.add_space(6.0);
            ui.label(
                RichText::new("传输")
                    .strong()
                    .size(11.5)
                    .color(theme.text_secondary),
            );
            // 只保留最近的传输记录（上限 12 条）。
            if self.transfers.len() > 12 {
                self.transfers.drain(..self.transfers.len() - 12);
            }
            for transfer in &self.transfers {
                ui.horizontal(|ui| {
                    let (color, text) = if transfer.failed {
                        (theme.danger, "失败")
                    } else if transfer.finished {
                        (theme.success, "完成")
                    } else {
                        (theme.accent2, "进行中")
                    };
                    ui.colored_label(color, RichText::new(text).size(11.5));
                    ui.label(
                        RichText::new(&transfer.label)
                            .size(11.5)
                            .color(theme.text_secondary),
                    );
                    if !transfer.finished && !transfer.failed && transfer.total > 0 {
                        let progress = transfer.done as f32 / transfer.total as f32;
                        ui.add(egui::ProgressBar::new(progress).desired_width(120.0).text(
                            format!(
                                "{} / {}",
                                Self::format_size(transfer.done),
                                Self::format_size(transfer.total)
                            ),
                        ));
                    }
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
            self.handle.upload(&path, &self.join(&name));
        }
    }

    /// 下载选中文件。
    fn download_selected(&mut self) {
        let Some(name) = self.selected.clone() else {
            return;
        };
        let Some(path) = rfd::FileDialog::new().set_file_name(&name).save_file() else {
            return;
        };
        self.handle.download(&self.join(&name), &path);
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
                ConfirmDialog::Delete { name, is_dir, .. } => {
                    ui.label(format!(
                        "确定删除{} {}？此操作不可恢复。",
                        if *is_dir { "目录" } else { "文件" },
                        name
                    ));
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
                ConfirmDialog::Delete { path, is_dir, .. } => {
                    self.handle.remove(&path, is_dir);
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

/// SFTP 工具栏紧凑按钮：深色底 + 细边框 + 圆角（Tabby 风）。
fn sftp_tool_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    let theme = crate::theme::current_theme();
    ui.add(
        egui::Button::new(RichText::new(label).size(11.5).color(theme.text_primary))
            .fill(theme.bg_elevated)
            .stroke(egui::Stroke::new(1.0, theme.border))
            .corner_radius(crate::theme::tokens::RADIUS_ITEM),
    )
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
            ConfirmDialog::Delete { name, path, is_dir } => ConfirmDialog::Delete {
                name: name.clone(),
                path: path.clone(),
                is_dir: *is_dir,
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

    /// 测试 sshd 配置（与 kun-core 集成测试一致）。
    fn test_profile() -> kun_core::config::HostProfile {
        use kun_core::config::Auth;
        let key_path = std::env::var("KUN_TEST_KEY").unwrap_or_else(|_| {
            format!(
                "{}/.ssh/id_ed25519",
                std::env::var("HOME").unwrap_or_default()
            )
        });
        kun_core::config::HostProfile {
            name: "UI 测试".into(),
            host: std::env::var("KUN_TEST_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
            port: std::env::var("KUN_TEST_PORT")
                .unwrap_or_else(|_| "2222".into())
                .parse()
                .unwrap(),
            user: std::env::var("KUN_TEST_USER")
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
        use kun_core::ssh::sftp::connect_sftp;
        use std::time::{Duration, Instant};

        // 测试 sshd 的 known_hosts 与 hostkey 同目录（/tmp/kun-test-sshd），
        // hostkey 重建时指纹记录一并消失，不会旧指纹不匹配导致测试失败。
        // call_once：测试并行运行时不重复设置环境变量。
        static KNOWN_HOSTS_INIT: std::sync::Once = std::sync::Once::new();
        KNOWN_HOSTS_INIT.call_once(|| {
            std::env::set_var("KUN_KNOWN_HOSTS", "/tmp/kun-test-sshd/known_hosts.toml");
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
        if let kun_core::config::Auth::Key { path, .. } = &profile.auth {
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
        harness.get_by_label("刷新");
        harness.get_by_label("上传");
        harness.get_by_label("下载");
        harness.get_by_label("新建目录");
        // 面板标题包含主机名。
        harness.get_by_label("SFTP · UI 测试主机");
    }

    /// 确认对话框流程：选中条目 → 点击删除 → 出现确认框。
    #[test]
    fn 删除确认对话框流程() {
        use kittest::Queryable;

        // 直接构造带条目的面板（不依赖网络）。
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
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
            selected: Some("readme.md".into()),
            loading: false,
            transfers: Vec::new(),
            dialog: None,
            error: None,
            closed: false,
        };

        let mut harness = egui_kittest::Harness::new_ui(|ui| {
            let ctx = ui.ctx().clone();
            view.show(ui);
            view.show_dialog(&ctx);
        });
        harness.run();

        // 点击删除按钮 → 出现确认对话框。
        harness.get_by_label("删除").click();
        harness.run();
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

    /// 渲染级回归：340 宽（比 40% 默认更窄的保守下界）下工具栏按钮
    /// 不被右缘截断、互不重叠
    /// （曾把全部按钮塞一行：溢出被裁剪，且右对齐的"刷新"与"删除"重叠）。
    #[test]
    fn 工具栏按钮不截断不重叠() {
        use kittest::Queryable;

        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (handle_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = SftpHandle::from_raw(handle_tx);
        let mut view = SftpView {
            host_name: "测试主机".into(),
            handle,
            rx,
            current_path: "/very/long/remote/path/that/overflows/".into(),
            entries: Vec::new(),
            selected: None,
            loading: false,
            transfers: Vec::new(),
            dialog: None,
            error: None,
            closed: false,
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

        let labels = ["刷新", "上传", "下载", "新建目录", "重命名", "删除"];
        let rects: Vec<egui::Rect> = labels
            .iter()
            .map(|l| harness.get_by_label(l).rect())
            .collect();
        for (label, rect) in labels.iter().zip(&rects) {
            assert!(
                rect.right() <= PANEL_W - MARGIN_X + 0.5,
                "{label} 按钮右缘 {:.1} 超出面板内容区（被截断）",
                rect.right()
            );
        }
        for i in 0..rects.len() {
            for j in i + 1..rects.len() {
                assert!(
                    !rects[i].intersects(rects[j]),
                    "按钮 {} 与 {} 重叠：{:?} vs {:?}",
                    labels[i],
                    labels[j],
                    rects[i],
                    rects[j]
                );
            }
        }
    }

    /// 单击选中，再次单击已选中的目录进入下级目录（发 List 命令）。
    /// 回归：曾用 response.double_clicked()，egui 多击计数被无关点击
    /// 污染导致"点击文件夹进不去"。
    #[test]
    fn 单击选中再次单击进入目录() {
        use kittest::Queryable;
        use kun_core::ssh::sftp::SftpCmd;

        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
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
            selected: None,
            loading: false,
            transfers: Vec::new(),
            dialog: None,
            error: None,
            closed: false,
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

        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
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
            selected: None,
            loading: false,
            transfers: Vec::new(),
            dialog: None,
            error: None,
            closed: false,
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
