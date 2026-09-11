//! SFTP 面板：远程文件浏览、传输进度与文件操作。

use egui::{RichText, Ui};
use mino_core::ssh::sftp::{RemoteEntry, SftpEvent, SftpHandle};
use tokio::sync::mpsc::Receiver;

/// 传输聚合状态（标题行徽标用）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TransferState {
    /// 有进行中的传输（数量为未结束项数）。
    Uploading { active: usize },
    /// 无进行中，但有失败项（数量为失败项数）。
    Failed { failed: usize },
    /// 全部完成。
    Done,
}

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

/// 传输行的视觉尺寸：状态文字预留固定宽度，进度轨道保持纤细。
const TRANSFER_STATUS_WIDTH: f32 = 34.0;
const TRANSFER_PROGRESS_HEIGHT: f32 = 4.0;

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
    /// 定位到终端目录。面板只表达“用户想要定位”，真正的目录解析由
    /// 应用层在拿到最新终端目录后完成（见
    /// `SftpView::locate_terminal_directory`）：`show_with_terminal_cwd`
    /// 遇到它只向上传递定位请求，不在面板内部直接导航。
    LocateTerminal,
    /// 定位到一条已经解析好的绝对路径（`apply_context_action` 兜底用，
    /// 正常路径走 `locate_terminal_directory` 直接导航）。
    #[allow(dead_code)]
    LocatePath(String),
}

/// SFTP 面板状态。
pub struct SftpView {
    /// 主机名称（显示用）。
    host_name: String,
    handle: SftpHandle,
    rx: Receiver<SftpEvent>,
    /// 当前远程路径。
    current_path: String,
    /// SFTP 会话的初始目录，用于解析 `~` 和相对路径。
    ///
    /// SSH 终端和 SFTP 子系统可能由服务端映射到不同的当前目录；
    /// 保留 SFTP 自己的 home，避免把终端侧的显示路径直接当成 SFTP 路径。
    sftp_home: String,
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
    /// 传输详情是否展开。点击标题行徽标切换；新传输开始时自动展开、
    /// 全部结束后保持上一次展开态（用户可手动收起看结果角标）。
    transfers_expanded: bool,
    /// 确认对话框。
    dialog: Option<ConfirmDialog>,
    /// 行内错误。
    error: Option<String>,
    /// 面板内一次性提示（定位反馈等）：文本 + 展示开始时间。
    notice: Option<(String, std::time::Instant)>,
    /// 连接已关闭。
    closed: bool,
    /// 等宽字符宽缓存（'0' 字符，数字等宽字体的真实字宽；字体启动时加载后不变）。
    cell_width: f32,
    /// 上一次普通主键点击（行标识、时间），用于稳定识别双击。
    /// 不依赖 egui 全局 click_count，避免其它控件的点击污染目录行判断。
    last_primary_click: Option<(String, f64)>,
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
        let initial_path = normalize_remote_path(initial_path);
        let view = Self {
            host_name: host_name.to_string(),
            handle,
            rx,
            current_path: initial_path.clone(),
            sftp_home: initial_path.clone(),
            entries: Vec::new(),
            selected: Vec::new(),
            selection_anchor: None,
            loading: true,
            transfers: Vec::new(),
            transfers_expanded: true,
            dialog: None,
            error: None,
            notice: None,
            closed: false,
            cell_width: 0.0,
            last_primary_click: None,
        };
        // 初始列出 SFTP 会话的目录（通常与远程终端登录目录一致）。
        view.handle.list(&initial_path);
        view
    }

    /// 会话引用（供状态栏等读取标题）。
    pub fn host_name(&self) -> &str {
        &self.host_name
    }

    /// 请求列出目录并清理已失效的选择状态。
    fn navigate_to(&mut self, path: &str) {
        if self.closed {
            self.error = Some("连接已关闭，无法切换目录".to_string());
            self.loading = false;
            return;
        }
        let path = self.resolve_path(path);
        self.current_path = path.clone();
        self.handle.list(&path);
        self.loading = true;
        self.error = None;
        // 保留原条目数量作为稳定的布局占位；渲染层在加载期间会用静态
        // 骨架替换旧内容，避免双击目录后 ScrollArea 高度瞬间收缩再弹回。
        self.selected.clear();
        self.selection_anchor = None;
        self.last_primary_click = None;
    }

    /// 面板内一次性提示（定位反馈等），4 秒后自动消失。
    fn set_notice(&mut self, text: impl Into<String>) {
        self.notice = Some((text.into(), std::time::Instant::now()));
    }

    /// 将终端侧传来的路径转换成 SFTP 使用的规范 POSIX 路径。
    ///
    /// 终端目录来自输入模型，可能是 `~`、相对路径或包含 `.`/`..`；
    /// SFTP 请求应始终使用绝对、归一化后的路径，否则某些服务端会把
    /// 这类路径报成重复的 `No such file` 错误。
    fn resolve_path(&self, path: &str) -> String {
        let path = path.trim();
        let path = if path == "~" {
            self.sftp_home.as_str()
        } else if let Some(rest) = path.strip_prefix("~/") {
            return normalize_remote_path(&format!("{}/{rest}", self.sftp_home));
        } else if path.is_empty() {
            "/"
        } else {
            path
        };
        if path.starts_with('/') {
            normalize_remote_path(path)
        } else {
            normalize_remote_path(&format!("{}/{path}", self.current_path))
        }
    }

    /// 判断是否为同一行的普通双击。
    ///
    /// 使用面板自己的时间窗口，不使用 egui 的全局多击计数，避免用户在
    /// 两次目录点击之间点击其它控件后，目录仍被错误地当成双击。
    fn register_primary_click(
        &mut self,
        key: String,
        modifiers: egui::Modifiers,
        now: f64,
    ) -> bool {
        let plain_click = !modifiers.shift && !modifiers.command && !modifiers.ctrl;
        let double_clicked = plain_click
            && self
                .last_primary_click
                .as_ref()
                .is_some_and(|(last_key, last_time)| {
                    last_key == &key && now >= *last_time && now - *last_time <= 0.45
                });
        self.last_primary_click = if plain_click && !double_clicked {
            Some((key, now))
        } else {
            None
        };
        double_clicked
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

    /// 选中列表中的上级目录入口。`..` 不是远程条目，使用独立哨兵值保存
    /// 选中态；收到新目录列表后会和普通条目一样被自动清掉。
    fn select_parent(&mut self) {
        self.selected.clear();
        self.selected.push("..".to_string());
        self.selection_anchor = Some("..".to_string());
    }

    /// 记录一项传输，先于后台事件进入队列，保证上传徽标立即出现。
    ///
    /// 新传输恒展开详情：用户点「上传」后默认能看到进度条；手动收起
    /// 只在点击徽标后发生且不受后续进度事件影响。
    fn begin_transfer(&mut self, id: u64, label: String, total: u64) {
        self.transfers.push(Transfer {
            id,
            label,
            done: 0,
            total,
            finished: false,
            failed: false,
        });
        self.transfers_expanded = true;
    }

    /// 当前聚合传输状态（无传输时 `None`，标题行不画徽标）。
    fn transfer_state(&self) -> Option<TransferState> {
        if self.transfers.is_empty() {
            return None;
        }
        let active = self
            .transfers
            .iter()
            .filter(|transfer| !transfer.finished && !transfer.failed)
            .count();
        if active > 0 {
            return Some(TransferState::Uploading { active });
        }
        let failed = self.transfers.iter().filter(|t| t.failed).count();
        if failed > 0 {
            return Some(TransferState::Failed { failed });
        }
        Some(TransferState::Done)
    }

    /// 处理后台事件。返回本帧是否收到新事件（调用方据此请求重绘——
    /// 传输进度/列表到达后若无其它重绘源，进度条不会自行刷新）。
    pub(crate) fn poll_events(&mut self) -> bool {
        let mut any = false;
        while let Ok(event) = self.rx.try_recv() {
            any = true;
            match event {
                SftpEvent::Listed { path, entries } => {
                    let path = normalize_remote_path(&path);
                    // 列表是异步返回的。用户快速切换目录时，旧请求的结果
                    // 不能覆盖当前页面，否则文件内容会先跳回旧目录再跳回来。
                    if path != self.current_path {
                        continue;
                    }
                    self.entries = entries;
                    self.error = None;
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
                        // 后台事件到达的新传输同样默认展开（用户正在看文件
                        // 列表也应看到进度出现；手动收起不受影响）。
                        self.transfers_expanded = true;
                    }
                }
                SftpEvent::Done {
                    id,
                    label,
                    refresh,
                    path,
                } => {
                    if let Some(id) = id {
                        if let Some(t) = self.transfers.iter_mut().find(|t| t.id == id) {
                            t.finished = true;
                            // 进度事件是可丢弃的；队列拥塞时 Done 可能先于
                            // 最后一条 Progress 到达。完成态必须补齐进度，
                            // 不能显示“完成 0 / 总量”。
                            t.done = t.total;
                        } else {
                            self.transfers.push(Transfer {
                                id,
                                label,
                                done: 1,
                                total: 1,
                                finished: true,
                                failed: false,
                            });
                            self.transfers_expanded = true;
                        }
                    }
                    // 只有远程目录内容发生变化的操作才刷新列表；下载只写本地，
                    // 不应触发批量传输中的重复 read_dir 与 loading 闪烁。
                    let stale = path
                        .as_deref()
                        .is_some_and(|path| normalize_remote_path(path) != self.current_path);
                    if refresh && !stale {
                        let path = self.current_path.clone();
                        self.handle.list(&path);
                        self.loading = true;
                    }
                }
                SftpEvent::Error {
                    id,
                    label,
                    message,
                    path,
                } => {
                    let stale = path
                        .as_deref()
                        .is_some_and(|path| normalize_remote_path(path) != self.current_path);
                    if let Some(id) = id {
                        if let Some(t) = self.transfers.iter_mut().find(|t| t.id == id) {
                            t.failed = true;
                        } else {
                            self.transfers.push(Transfer {
                                id,
                                label: label.clone(),
                                done: 0,
                                total: 0,
                                finished: false,
                                failed: true,
                            });
                            self.transfers_expanded = true;
                        }
                    }
                    // 传输失败即使发生在旧目录，也必须结束对应进度条；
                    // 但旧目录的错误文本不能污染用户当前正在浏览的目录。
                    if stale {
                        continue;
                    }
                    if label == "列出目录" {
                        // 当前目录加载失败时清掉占位内容；传输失败则保留
                        // 当前列表，避免一条上传错误把文件面板清空。
                        self.entries.clear();
                        self.selected.clear();
                        self.selection_anchor = None;
                    }
                    self.error = Some(format!("{label}：{message}"));
                    self.loading = false;
                }
                SftpEvent::Closed => {
                    self.closed = true;
                    self.loading = false;
                    self.last_primary_click = None;
                    let interrupted = self
                        .transfers
                        .iter_mut()
                        .filter(|transfer| !transfer.finished && !transfer.failed)
                        .map(|transfer| {
                            transfer.failed = true;
                        })
                        .count();
                    if interrupted > 0 && self.error.is_none() {
                        self.error = Some("连接已关闭，未完成的传输已取消".to_string());
                    }
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
        // 行的点击区域保持完整高度，视觉底色上下收 2px，给相邻目录留出
        // 呼吸空间，避免图标、文字和选中底色挤在一起。
        let visual_rect = row_rect.shrink2(egui::vec2(0.0, 2.0));

        // 双击进入目录后，列表请求是异步的。保留旧行的高度，但先画静态
        // 骨架，不显示旧目录内容，避免列表瞬间收缩造成整块内容跳动。
        if self.loading && !self.entries.is_empty() {
            ui.painter().rect_filled(
                visual_rect,
                crate::theme::tokens::RADIUS_ITEM,
                theme.bg_panel.gamma_multiply(0.45),
            );
            let skeleton = theme.text_muted.gamma_multiply(0.22);
            let name_width = (name_col - icon_pad).clamp(1.0, 150.0);
            ui.painter().rect_filled(
                egui::Rect::from_min_size(
                    egui::pos2(row_rect.left() + icon_pad, row_rect.center().y - 2.0),
                    egui::vec2(name_width, 4.0),
                ),
                2.0,
                skeleton,
            );
            ui.painter().rect_filled(
                egui::Rect::from_min_size(
                    egui::pos2(row_rect.right() - time_col - 6.0, row_rect.center().y - 2.0),
                    egui::vec2(time_col.min(62.0), 4.0),
                ),
                2.0,
                skeleton,
            );
            return;
        }

        // hover 用指针位置判定（子 Ui 会抢走 response.hovered()）。
        let pointer_in_row =
            ui.input(|i| i.pointer.hover_pos().is_some_and(|p| row_rect.contains(p)));
        let row_id = if idx == 0 {
            egui::Id::new(("sftp_row", self.current_path.as_str(), ".."))
        } else {
            egui::Id::new((
                "sftp_row",
                self.current_path.as_str(),
                self.entries[idx - 1].name.as_str(),
            ))
        };
        let row_sense = if self.loading {
            egui::Sense::hover()
        } else {
            egui::Sense::click()
        };

        // ".." 行：返回上级目录（文件管理器通用习惯，导航更直观）。
        if idx == 0 {
            let selected = self.selected.iter().any(|name| name == "..");
            if selected {
                ui.painter().rect_filled(
                    visual_rect,
                    crate::theme::tokens::RADIUS_ITEM,
                    theme.accent_soft,
                );
                ui.painter().rect_filled(
                    egui::Rect::from_min_max(
                        egui::pos2(visual_rect.left() + 1.0, visual_rect.top() + 4.0),
                        egui::pos2(visual_rect.left() + 3.0, visual_rect.bottom() - 4.0),
                    ),
                    1.5,
                    theme.accent,
                );
            } else if pointer_in_row {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                ui.painter().rect_filled(
                    visual_rect,
                    crate::theme::tokens::RADIUS_ITEM,
                    egui::Color32::from_rgba_unmultiplied(255, 255, 255, 12),
                );
            }
            let icon = egui::Rect::from_center_size(
                egui::pos2(row_rect.left() + 12.0, row_rect.center().y),
                egui::vec2(18.0, 16.0),
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
            let response = ui.interact(row_rect, row_id, row_sense);
            // ".." 行双击返回上级，第一次点击只负责选中/反馈。
            if response.secondary_clicked() {
                self.last_primary_click = None;
            } else if response.clicked() {
                let modifiers = response.ctx.input(|input| input.modifiers);
                let now = response.ctx.input(|input| input.time);
                let double_clicked = self.register_primary_click(
                    format!("{}\0..", self.current_path),
                    modifiers,
                    now,
                );
                if double_clicked {
                    *open_dir = Some(Self::parent_of(&self.current_path));
                } else if !modifiers.shift && !modifiers.command && !modifiers.ctrl {
                    self.select_parent();
                }
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
        // 整行点击区：单击选中，双击目录进入下级目录。
        // 选中：accent 软底 + 左侧 accent 竖条；hover：白色低透明度
        // 叠加（Tabby 风，先画背景，列内容绘制在其上）。
        if selected {
            ui.painter().rect_filled(
                visual_rect,
                crate::theme::tokens::RADIUS_ITEM,
                theme.accent_soft,
            );
            ui.painter().rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(visual_rect.left() + 1.0, visual_rect.top() + 4.0),
                    egui::pos2(visual_rect.left() + 3.0, visual_rect.bottom() - 4.0),
                ),
                1.5,
                theme.accent,
            );
        } else if pointer_in_row {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            ui.painter().rect_filled(
                visual_rect,
                crate::theme::tokens::RADIUS_ITEM,
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 12),
            );
        }
        // 行首矢量图标：文件夹蓝色渐变 / 文件描边轮廓。
        let icon = egui::Rect::from_center_size(
            egui::pos2(row_rect.left() + 12.0, row_rect.center().y),
            egui::vec2(18.0, 16.0),
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
        let response = ui.interact(row_rect, row_id, row_sense);
        let modifiers = response.ctx.input(|input| input.modifiers);
        if response.secondary_clicked() {
            self.last_primary_click = None;
            self.update_secondary_selection(idx, &entry.name, modifiers);
        } else if response.clicked() {
            let now = response.ctx.input(|input| input.time);
            let double_clicked = self.register_primary_click(
                format!("{}\0{}", self.current_path, entry.name),
                modifiers,
                now,
            );
            // 目录：单击选中，普通双击进入；按住 Shift/Cmd/Ctrl 单击只
            // 选中（供批量下载/删除）。文件：单击或双击都只选中。
            if entry.is_dir && is_valid_entry_name(&entry.name) && double_clicked {
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
            if entry.is_dir
                && is_valid_entry_name(&entry.name)
                && selected_count == 1
                && ui.button("打开目录").clicked()
            {
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
            if selected_count == 1
                && is_valid_entry_name(&entry_name)
                && ui.button("重命名").clicked()
            {
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
            if terminal_cwd.is_some()
                && ui
                    .button("定位到当前终端目录  ⌘⇧L")
                    .on_hover_text(terminal_cwd.as_deref().unwrap_or(""))
                    .clicked()
            {
                *context_action = Some(ContextAction::LocateTerminal);
                ui.close();
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
    ///
    /// 返回值语义：`None` 表示本帧没有定位请求；`Some(true)` 表示用户按
    /// 下了定位（菜单/快捷键），应用层应先对终端做一次 `pwd` 探测再导航；
    /// `Some(false)` 保留给“探测失败，直接用已知目录回退”的内部路径。
    pub fn show_with_terminal_cwd(
        &mut self,
        ui: &mut Ui,
        terminal_cwd: Option<&str>,
    ) -> Option<bool> {
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
        let mut context_action = if locate_shortcut && terminal_cwd.is_some() {
            Some(ContextAction::LocateTerminal)
        } else {
            None
        };

        // ==================== 标题行：状态点 + 主机名 + 传输徽标 ====================
        // 上传进度平时只是一个徽标：标题行右缘的 26×26 方形图标按钮，
        // 有传输（进行中/已完成/失败）才出现；点击切换下方详情展开/收起。
        // 徽标形态按聚合状态区分：
        // - 进行中：主题进行色（accent2）圆底 + 白色向上箭头 + 进度弧环；
        // - 全部完成：success 色圆底 + 白色对勾；
        // - 有失败：danger 色圆底 + 白色感叹号。
        // 无障碍：纯图标按钮需显式 widget_info 覆盖 label（Button 的
        // WidgetInfo 取自文字 atoms，空文字会生成无 label 的 Button 节点，
        // kittest 按 label 找不到；齿轮按钮同理但按最右 Button 查找）。
        let transfer_state = self.transfer_state();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let (dot, _) = ui.allocate_exact_size(egui::vec2(9.0, 9.0), egui::Sense::hover());
            ui.painter().circle_filled(dot.center(), 3.2, theme.accent2);
            let title_avail =
                (ui.available_width() - transfer_state.map_or(0.0, |_| 26.0 + 6.0)).max(0.0);
            ui.add_sized(
                [title_avail, 18.0],
                egui::Label::new(
                    RichText::new(format!("SFTP · {}", self.host_name))
                        .strong()
                        .size(12.5)
                        .color(theme.text_primary),
                )
                .truncate(),
            );
            if let Some(state) = transfer_state {
                let badge_resp = ui.add_sized(
                    [26.0, 26.0],
                    egui::Button::new("")
                        .fill(egui::Color32::TRANSPARENT)
                        .stroke(egui::Stroke::NONE)
                        .min_size(egui::vec2(26.0, 26.0)),
                );
                let badge_resp = badge_resp
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text(if self.transfers_expanded {
                        "收起传输详情"
                    } else {
                        "展开传输详情"
                    });
                badge_resp.widget_info(|| {
                    // 只能通过闭包覆盖：Button::ui 内部已按自身 atoms 注册过
                    // WidgetInfo，后调用的 widget_info 会覆盖 accesskit 节点。
                    egui::WidgetInfo::labeled(
                        egui::WidgetType::Button,
                        ui.is_enabled(),
                        transfer_badge_label(state),
                    )
                });
                if badge_resp.clicked() {
                    self.transfers_expanded = !self.transfers_expanded;
                }
                if ui.is_rect_visible(badge_resp.rect) {
                    if badge_resp.hovered() {
                        ui.painter().rect_filled(
                            badge_resp.rect,
                            crate::theme::tokens::RADIUS_ITEM,
                            egui::Color32::from_rgba_unmultiplied(255, 255, 255, 18),
                        );
                    }
                    paint_transfer_badge(ui.painter(), badge_resp.rect, state, self, theme);
                }
            }
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

        // ==================== 一次性提示（定位反馈等） ====================
        // 定位到与当前相同的目录、跳转成功等场景给用户明确反馈，
        // 避免"点了没反应"的错觉；4 秒后自动消失。
        if let Some((text, at)) = &self.notice {
            if at.elapsed() > std::time::Duration::from_secs(4) {
                self.notice = None;
            } else {
                ui.colored_label(theme.accent2, RichText::new(text).size(11.5));
            }
        }

        // ==================== 传输详情（徽标展开时可见） ====================
        // 徽标是常驻入口：收起后标题行只剩状态徽标，文件列表获得全部
        // 纵向空间；展开后详情卡片落在列表上方（ScrollArea auto_shrink
        // false 会占满剩余高度，其后内容会被裁剪——上传进度曾因此不可见）。
        // 只保留最近的传输记录（上限 12 条）。
        if self.transfers.len() > 12 {
            self.transfers.drain(..self.transfers.len() - 12);
        }
        let transfers_expanded = self.transfers_expanded;
        let mut close_upload_progress = false;
        if transfers_expanded {
            self.render_transfer_details(ui, theme, &mut close_upload_progress);
        }
        if close_upload_progress {
            // 只移除已经结束的上传记录；按钮只在全部上传结束后出现，
            // 因此不会误清理进行中的任务。
            self.transfers
                .retain(|transfer| !transfer.label.starts_with("上传 "));
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
        // 行首图标占位宽度（18px 图标 + 两侧留白），表头与行共用基准。
        let icon_pad = 28.0;
        let row_h = 28.0;
        // 固定列宽：名称列占剩余宽度、大小/修改时间右对齐定宽。
        // 曾用"总宽 - 固定字符数"的 add_space 定位：长文件名会把
        // 大小/时间列挤出面板右缘，且各行列位随名称长度漂移错位。
        let size_col = cell_width * 12.0;
        let time_col = cell_width * 12.0;
        let table_width = ui.available_width();
        // 名称列让出两个 6px 间隔（名称|大小|时间），大小/时间定宽。
        // 名称列也必须服从总宽度；固定写死最小 40px 会在面板最小宽度
        // 下把大小/时间列推出右边界。正常面板仍保留至少一个图标后字符位。
        let name_col = (table_width - size_col - time_col - 12.0).max(icon_pad + 1.0);

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
            self.last_primary_click = None;
        }
        let blank_path = self.current_path.clone();
        let blank_terminal_cwd = terminal_cwd.map(str::to_string);
        if !self.loading {
            blank_response.context_menu(|ui| {
                render_blank_context_menu(
                    ui,
                    &blank_path,
                    blank_terminal_cwd.as_deref(),
                    &mut context_action,
                );
            });
        }

        // 加载提示占用固定高度：加载完成后只清空内容，不让文件列表整体
        // 因提示行的出现/消失而上下跳动。
        let loading_h = 24.0;
        let (loading_rect, _) =
            ui.allocate_exact_size(egui::vec2(table_width, loading_h), egui::Sense::hover());
        if self.loading {
            let mut loading_ui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(loading_rect)
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
            );
            // 静态指示点（不滚动动画）：egui `Spinner` 每帧 request_repaint
            // 强制 60fps 全帧重绘（含终端全量扫描），静态点零重绘。
            let (dot, _) =
                loading_ui.allocate_exact_size(egui::vec2(9.0, 9.0), egui::Sense::hover());
            loading_ui
                .painter()
                .circle_filled(dot.center(), 3.2, theme.accent2);
            loading_ui.add_space(6.0);
            loading_ui.label(RichText::new("加载中…").size(12.0).color(theme.text_muted));
        } else if self.entries.is_empty() {
            ui.add_space(14.0);
            ui.vertical_centered(|ui| {
                let response = ui.add(
                    egui::Label::new(RichText::new("空目录").size(12.0).color(theme.text_muted))
                        .sense(egui::Sense::click()),
                );
                if response.clicked() {
                    self.last_primary_click = None;
                }
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

        // 定位请求不在面板内部直接消费：终端目录可能是跟踪器里的旧
        // 推测值，应用层需要先触发 `pwd` 探测、等输出校正后再导航。
        // 这里只把"用户想要定位"向上传递，返回值见 `show_with_terminal_cwd`。
        let mut locate_requested = false;
        if let Some(action) = context_action.take() {
            match action {
                ContextAction::LocateTerminal => {
                    locate_requested = true;
                }
                action => {
                    self.apply_context_action(action);
                }
            }
            ui.ctx().request_repaint();
        }
        locate_requested.then_some(true)
    }

    /// 应用层在终端 `pwd` 探测完成后调用：导航到解析后的最新终端目录。
    ///
    /// 与旧的 `ContextAction::Locate(path)` 等价，但路径由应用层在探测
    /// 完成后提供，保证用的是 shell 真正所在的目录而不是跟踪旧值。
    pub fn locate_terminal_directory(&mut self, path: &str) {
        if self.closed {
            self.error = Some("连接已关闭，无法切换目录".to_string());
            self.loading = false;
            return;
        }
        // 定位到终端目录：目标与当前相同时不做无谓刷新，
        // 但必须给明确反馈——曾"点击后无反应"（实际是定位到
        // 了同一个目录，列表原地刷新看不出任何变化）。
        let resolved = self.resolve_path(path);
        if resolved == self.current_path {
            self.set_notice(format!("已在终端目录 {resolved}"));
        } else {
            self.navigate_to(path);
            self.set_notice(format!("定位到终端目录 {resolved}"));
        }
    }

    /// 执行列表右键菜单动作。
    fn apply_context_action(&mut self, action: ContextAction) {
        if self.closed {
            self.error = Some("连接已关闭，无法执行文件操作".to_string());
            return;
        }
        self.last_primary_click = None;
        match action {
            ContextAction::Open(path) => self.navigate_to(&path),
            // 面板内部不再直接消费定位请求（见 `show_with_terminal_cwd`
            // 尾部的 `locate_requested`）：`LocateTerminal` 由应用层在
            // `pwd` 探测完成后经 `locate_terminal_directory` 导航。
            // 这里保留兜底分支，避免该动作在其它路径被调用时静默丢失。
            ContextAction::LocateTerminal => {
                let path = self.current_path.clone();
                self.locate_terminal_directory(&path);
            }
            ContextAction::LocatePath(path) => {
                self.locate_terminal_directory(&path);
            }
            ContextAction::Refresh => {
                let path = self.current_path.clone();
                self.handle.list(&path);
                self.loading = true;
                self.error = None;
            }
            ContextAction::Upload => self.upload_dialog(),
            ContextAction::Download(names) => self.download_selected(&names),
            ContextAction::Rename(name) => {
                if !is_valid_entry_name(&name) {
                    self.error = Some("无法重命名异常的远程条目".to_string());
                    return;
                }
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
                            .find(|entry| entry.name == *name && is_valid_entry_name(&entry.name))
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

    /// 渲染展开态的传输详情（上传进度卡片 + 其它传输记录）。
    ///
    /// 调用方已确认 `transfers_expanded == true`；无传输时不画任何卡片
    /// （标题行徽标也不出现），避免空面板被一张空卡片占掉纵向空间。
    /// `close_upload_progress` 由调用方在渲染后消费：只清理已结束的上传。
    fn render_transfer_details(
        &self,
        ui: &mut Ui,
        theme: &'static crate::theme::Theme,
        close_upload_progress: &mut bool,
    ) {
        let has_upload = self
            .transfers
            .iter()
            .any(|transfer| transfer.label.starts_with("上传 "));
        if has_upload {
            let uploads_finished = self
                .transfers
                .iter()
                .filter(|transfer| transfer.label.starts_with("上传 "))
                .all(|transfer| transfer.finished || transfer.failed);
            egui::Frame::new()
                .fill(theme.bg_elevated)
                .stroke(egui::Stroke::new(1.0, theme.border))
                .corner_radius(crate::theme::tokens::RADIUS_ITEM)
                .inner_margin(egui::Margin::same(8))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("上传进度")
                                .strong()
                                .size(11.5)
                                .color(theme.text_primary),
                        );
                        if uploads_finished {
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .add(
                                            egui::Button::new(
                                                RichText::new("关闭")
                                                    .size(10.5)
                                                    .color(theme.text_secondary),
                                            )
                                            .min_size(egui::vec2(38.0, 20.0)),
                                        )
                                        .clicked()
                                    {
                                        *close_upload_progress = true;
                                    }
                                },
                            );
                        }
                    });
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
            if has_upload {
                ui.add_space(6.0);
            }
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
        if self.closed {
            self.error = Some("连接已关闭，无法上传文件".to_string());
            return;
        }
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
        if self.closed {
            self.error = Some("连接已关闭，无法下载文件".to_string());
            return;
        }
        if names.is_empty() {
            return;
        }
        let files = names
            .iter()
            .filter_map(|name| {
                self.entries
                    .iter()
                    .find(|entry| {
                        entry.name == *name && !entry.is_dir && is_valid_entry_name(&entry.name)
                    })
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

    /// 确认对话框渲染（与设置/新建连接/更新共用外壳与按钮）。
    pub fn show_dialog(&mut self, ctx: &egui::Context) {
        if self.closed {
            self.dialog = None;
            return;
        }
        let mut close = false;
        let mut action: Option<ConfirmDialog> = None;
        if let Some(dialog) = &mut self.dialog {
            let theme = crate::theme::current_theme();
            let title = match dialog {
                ConfirmDialog::Delete { .. } => "确认删除",
                ConfirmDialog::Rename { .. } => "重命名",
                ConfirmDialog::Mkdir { .. } => "新建目录",
            };
            let destructive = matches!(dialog, ConfirmDialog::Delete { .. });
            egui::Window::new(match dialog {
                ConfirmDialog::Delete { .. } => "确认删除",
                ConfirmDialog::Rename { .. } => "重命名",
                ConfirmDialog::Mkdir { .. } => "新建目录",
            })
            .resizable(false)
            .collapsible(false)
            .title_bar(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .frame(crate::dialog::confirm_frame(theme))
            .show(ctx, |ui| {
                // ==================== 自绘头部 ====================
                let (header_rect, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), crate::dialog::HEADER_H),
                    egui::Sense::hover(),
                );
                ui.painter().rect_filled(
                    header_rect,
                    egui::CornerRadius {
                        nw: 12,
                        ne: 12,
                        sw: 0,
                        se: 0,
                    },
                    theme.bg_header,
                );
                ui.painter().line_segment(
                    [header_rect.left_bottom(), header_rect.right_bottom()],
                    egui::Stroke::new(1.0, theme.border),
                );
                let mut header = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(header_rect)
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                );
                header.add_space(14.0);
                if destructive {
                    // 删除用 danger 实心圆 + 白色 ×，一眼可辨危险。
                    let (badge, _) =
                        header.allocate_exact_size(egui::vec2(24.0, 24.0), egui::Sense::hover());
                    header
                        .painter()
                        .circle_filled(badge.center(), 12.0, theme.danger);
                    let icon = badge.shrink(8.5);
                    let stroke = egui::Stroke::new(1.8, egui::Color32::WHITE);
                    header
                        .painter()
                        .line_segment([icon.left_top(), icon.right_bottom()], stroke);
                    header
                        .painter()
                        .line_segment([icon.right_top(), icon.left_bottom()], stroke);
                } else {
                    // 重命名/新建目录用单色 accent 圆徽标 + 首字。
                    let (badge, _) =
                        header.allocate_exact_size(egui::vec2(24.0, 24.0), egui::Sense::hover());
                    let initial = title.chars().next().unwrap_or('?');
                    crate::dialog::paint_avatar(header.painter(), badge, initial, theme, false);
                }
                header.add_space(10.0);
                header.label(
                    egui::RichText::new(title)
                        .strong()
                        .size(14.0)
                        .color(theme.text_primary),
                );
                header.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(12.0);
                    if crate::dialog::close_icon_button(ui, "关闭（Esc）") {
                        close = true;
                    }
                });

                egui::Frame::new()
                    .inner_margin(egui::Margin::symmetric(18, 14))
                    .show(ui, |ui| {
                        ui.set_min_width(320.0);
                        ui.spacing_mut().item_spacing.y = 0.0;
                        match dialog {
                            ConfirmDialog::Delete { items } => {
                                if items.len() == 1 {
                                    let item = &items[0];
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "确定删除{} {} {}？",
                                            if item.is_dir { "目录" } else { "文件" },
                                            item.name,
                                            if item.is_dir {
                                                "及其全部内容"
                                            } else {
                                                ""
                                            }
                                        ))
                                        .size(12.5)
                                        .color(theme.text_primary),
                                    );
                                } else {
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "确定删除 {} 个项目？",
                                            items.len()
                                        ))
                                        .size(12.5)
                                        .color(theme.text_primary),
                                    );
                                    ui.add_space(4.0);
                                    ui.label(
                                        egui::RichText::new("目录会连同其中内容一起删除。")
                                            .size(11.5)
                                            .color(theme.text_muted),
                                    );
                                }
                                ui.add_space(4.0);
                                ui.label(
                                    egui::RichText::new("此操作不可恢复。")
                                        .size(11.5)
                                        .color(theme.danger),
                                );
                                ui.add_space(14.0);
                                crate::dialog::hairline(ui);
                                ui.add_space(12.0);
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.spacing_mut().item_spacing.x = 8.0;
                                        if ui
                                            .add(crate::dialog::danger_button(theme, "删除"))
                                            .clicked()
                                        {
                                            action = Some(dialog.clone());
                                            close = true;
                                        }
                                        if ui
                                            .add(crate::dialog::secondary_button(theme, "取消"))
                                            .clicked()
                                        {
                                            close = true;
                                        }
                                    },
                                );
                            }
                            ConfirmDialog::Rename { input, .. } => {
                                crate::dialog::field_label(ui, "新名称");
                                ui.add_space(4.0);
                                crate::dialog::form_input(
                                    ui,
                                    egui::Id::new("sftp_rename_input"),
                                    input,
                                    "输入新名称",
                                    320.0,
                                    false,
                                    false,
                                );
                                ui.add_space(14.0);
                                crate::dialog::hairline(ui);
                                ui.add_space(12.0);
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.spacing_mut().item_spacing.x = 8.0;
                                        if ui
                                            .add(crate::dialog::primary_button(theme, "确定"))
                                            .clicked()
                                        {
                                            action = Some(dialog.clone());
                                            close = true;
                                        }
                                        if ui
                                            .add(crate::dialog::secondary_button(theme, "取消"))
                                            .clicked()
                                        {
                                            close = true;
                                        }
                                    },
                                );
                            }
                            ConfirmDialog::Mkdir { input, .. } => {
                                crate::dialog::field_label(ui, "目录名称");
                                ui.add_space(4.0);
                                crate::dialog::form_input(
                                    ui,
                                    egui::Id::new("sftp_mkdir_input"),
                                    input,
                                    "输入目录名称",
                                    320.0,
                                    false,
                                    false,
                                );
                                ui.add_space(14.0);
                                crate::dialog::hairline(ui);
                                ui.add_space(12.0);
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.spacing_mut().item_spacing.x = 8.0;
                                        if ui
                                            .add(crate::dialog::primary_button(theme, "确定"))
                                            .clicked()
                                        {
                                            action = Some(dialog.clone());
                                            close = true;
                                        }
                                        if ui
                                            .add(crate::dialog::secondary_button(theme, "取消"))
                                            .clicked()
                                        {
                                            close = true;
                                        }
                                    },
                                );
                            }
                        }
                    });
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
                    let new_name = input.trim();
                    if !is_valid_entry_name(new_name) {
                        self.error = Some("名称不能为空，且不能包含 / 或 ..".to_string());
                    } else {
                        // 对话框可能在用户切换目录后才提交；目标目录必须
                        // 使用打开对话框时保存的旧路径，不能被当前路径带偏。
                        let parent = Self::parent_of(&path);
                        let new_path = join_path(&parent, new_name);
                        if new_path != path {
                            self.handle.rename(&path, &new_path);
                        }
                    }
                }
                ConfirmDialog::Mkdir { path, input } => {
                    let name = input.trim();
                    if !is_valid_entry_name(name) {
                        self.error = Some("目录名不能为空，且不能包含 / 或 ..".to_string());
                    } else {
                        // 同样使用对话框创建时保存的当前目录。
                        self.handle.mkdir(&join_path(&path, name));
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
    if terminal_cwd.is_some()
        && ui
            .button("定位到当前终端目录  ⌘⇧L")
            .on_hover_text(terminal_cwd.unwrap_or(""))
            .clicked()
    {
        *action = Some(ContextAction::LocateTerminal);
        ui.close();
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
    let progress = if transfer.total == 0 {
        if transfer.finished {
            1.0
        } else {
            0.0
        }
    } else {
        (transfer.done as f32 / transfer.total as f32).clamp(0.0, 1.0)
    };

    // ==================== 传输标题 ====================
    // 使用状态点 + 右侧状态，避免默认 ProgressBar 把一整行撑成厚重的
    // 控件；文件名仍保留为可访问的 Label，并在窄面板中自动截断。
    let row_width = ui.available_width().max(0.0);
    let status_text = if transfer.failed || transfer.finished {
        status.to_string()
    } else {
        format!("{:.0}%", progress * 100.0)
    };
    let bytes_text = format!(
        "{} / {}",
        SftpView::format_size(transfer.done),
        SftpView::format_size(transfer.total)
    );
    // 固定预留比每帧重新测量字体更稳定，也避免在高频进度事件期间拿
    // egui 字体写锁；34px 足够容纳“进行中”和“100%”。
    let status_width = TRANSFER_STATUS_WIDTH.min(row_width);
    let left_width = (row_width - 8.0 - 6.0 - status_width - 8.0).max(0.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        let (dot_rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 16.0), egui::Sense::hover());
        ui.painter().circle_filled(dot_rect.center(), 3.0, color);
        ui.add_space(6.0);
        ui.add_sized(
            [left_width, 16.0],
            egui::Label::new(
                RichText::new(&transfer.label)
                    .size(11.5)
                    .color(theme.text_secondary),
            )
            .truncate(),
        );
        ui.add_space(8.0);
        ui.add_sized(
            [status_width, 16.0],
            egui::Label::new(RichText::new(status_text).size(10.5).color(color))
                .halign(egui::Align::RIGHT),
        );
    });

    // ==================== 纤细渐变进度条 ====================
    // 高度固定为 4px，轨道与填充都自绘，避免 egui 默认控件的内边距和
    // 文本布局使进度条看起来像一块厚按钮。
    ui.add_space(3.0);
    let (track_rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width().max(0.0), TRANSFER_PROGRESS_HEIGHT),
        egui::Sense::hover(),
    );
    let track_color = theme.bg_panel.gamma_multiply(0.92);
    ui.painter().rect_filled(track_rect, 2.0, track_color);
    ui.painter().rect_stroke(
        track_rect,
        2.0,
        egui::Stroke::new(0.6, theme.border),
        egui::StrokeKind::Inside,
    );
    if progress > 0.0 && track_rect.width() > 0.0 {
        let fill_color = if transfer.failed {
            theme.danger
        } else if transfer.finished {
            theme.success
        } else {
            // 上传/下载进行中使用 macOS 风格的蓝色渐变；完成和失败状态
            // 则切换为主题语义色，用户能快速区分结果。
            egui::Color32::from_rgb(0x3c, 0x9b, 0xf5)
        };
        let fill_end = track_rect.left() + track_rect.width() * progress;
        let fill_rect = egui::Rect::from_min_max(
            track_rect.left_top(),
            egui::pos2(
                fill_end
                    .max(track_rect.left() + 3.0)
                    .min(track_rect.right()),
                track_rect.bottom(),
            ),
        );
        let fill_top = if transfer.failed || transfer.finished {
            fill_color
        } else {
            egui::Color32::from_rgb(0x8b, 0xd5, 0xff)
        };
        crate::anim::paint_rounded_gradient(ui.painter(), fill_rect, 2.0, fill_top, fill_color);
    }

    // 字节数放在细条下方，既保留精确进度，又不会把文字压进进度轨道。
    ui.add_space(2.0);
    ui.allocate_ui_with_layout(
        egui::vec2(ui.available_width().max(0.0), 14.0),
        egui::Layout::right_to_left(egui::Align::Center),
        |ui| {
            ui.add(
                egui::Label::new(
                    RichText::new(bytes_text)
                        .monospace()
                        .size(10.0)
                        .color(theme.text_muted),
                )
                .truncate(),
            );
        },
    );
    ui.add_space(6.0);
}

/// 徽标的无障碍 label（kittest 按此查找并点击）。
///
/// - 进行中：`传输进度 · 进行中 N 项`（收起时也可读出数量）；
/// - 有失败：`传输进度 · 有 N 项失败`；
/// - 全部完成：`传输进度 · 全部完成`。
fn transfer_badge_label(state: TransferState) -> String {
    match state {
        TransferState::Uploading { active } => format!("传输进度 · 进行中 {active} 项"),
        TransferState::Failed { failed } => format!("传输进度 · 有 {failed} 项失败"),
        TransferState::Done => "传输进度 · 全部完成".to_string(),
    }
}

/// 绘制 26×26 方形点击区内的 16px 传输徽标：
///
/// - 16px 圆形底（直径）：进行中 accent2 / 完成 success / 失败 danger；
/// - 中央白色符号：向上箭头（上传语义）/ 对勾（完成）/ 感叹号（失败）；
/// - 进行中额外画进度弧环：总进度（各传输 done/total 求和）的圆环描边，
///   起点 12 点钟方向顺时针；0% 时只画轨道环（低透明白），避免空环。
///
/// 符号全部用线段/圆点手绘（矢量，避免 SF 缺字形渲染成方块；与齿轮
/// 按钮同策略）。
/// `view` 仅用于读取总进度，不产生借用写入。
fn paint_transfer_badge(
    painter: &egui::Painter,
    rect: egui::Rect,
    state: TransferState,
    view: &SftpView,
    theme: &'static crate::theme::Theme,
) {
    let center = rect.center();
    let radius = 8.0;
    let base = match state {
        TransferState::Uploading { .. } => theme.accent2,
        TransferState::Failed { .. } => theme.danger,
        TransferState::Done => theme.success,
    };
    painter.circle_filled(center, radius, base);
    // 极淡的深色描边，让圆形在 bg_elevated 卡片与深色面板上都有边界。
    painter.circle_stroke(
        center,
        radius - 0.5,
        egui::Stroke::new(1.0, egui::Color32::from_black_alpha(48)),
    );
    // 进行中：进度弧环（轨道 + 前景）。
    if matches!(state, TransferState::Uploading { .. }) {
        let done: u64 = view.transfers.iter().map(|t| t.done).sum();
        let total: u64 = view.transfers.iter().map(|t| t.total).sum();
        let progress = if total == 0 {
            0.0
        } else {
            (done as f32 / total as f32).clamp(0.0, 1.0)
        };
        let ring_r = radius + 2.5;
        let track = egui::Color32::from_white_alpha(56);
        let sweep = std::f32::consts::TAU * progress;
        paint_ring(painter, center, ring_r, 0.0, std::f32::consts::TAU, track);
        if sweep > 0.02 {
            paint_ring(painter, center, ring_r, 0.0, sweep, egui::Color32::WHITE);
        }
    }
    let fg = egui::Color32::WHITE;
    match state {
        TransferState::Uploading { .. } => {
            // 向上箭头：竖线 + 两翼（上传语义；字符 "↑" 依赖字体，改手绘）。
            let top = center + egui::vec2(0.0, -4.2);
            let bottom = center + egui::vec2(0.0, 4.2);
            painter.line_segment([bottom, top], egui::Stroke::new(1.8, fg));
            painter.line_segment(
                [top, top + egui::vec2(-3.0, 3.0)],
                egui::Stroke::new(1.8, fg),
            );
            painter.line_segment(
                [top, top + egui::vec2(3.0, 3.0)],
                egui::Stroke::new(1.8, fg),
            );
        }
        TransferState::Done => {
            // 对勾：短臂 + 长臂。
            let p0 = center + egui::vec2(-4.0, 0.2);
            let p1 = center + egui::vec2(-1.2, 3.0);
            let p2 = center + egui::vec2(4.2, -3.2);
            painter.line_segment([p0, p1], egui::Stroke::new(1.8, fg));
            painter.line_segment([p1, p2], egui::Stroke::new(1.8, fg));
        }
        TransferState::Failed { .. } => {
            // 感叹号：竖线 + 底部圆点。
            let top = center + egui::vec2(0.0, -4.0);
            let bottom = center + egui::vec2(0.0, 1.6);
            painter.line_segment([top, bottom], egui::Stroke::new(1.8, fg));
            painter.circle_filled(center + egui::vec2(0.0, 3.8), 1.2, fg);
        }
    }
}

/// 在 `center` 为圆心、`radius` 为半径的圆上画一段圆弧（起点 12 点钟，
/// 顺时针 `sweep` 弧度）。`sweep <= 0` 不画；整圆用多段折线逼近。
fn paint_ring(
    painter: &egui::Painter,
    center: egui::Pos2,
    radius: f32,
    start: f32,
    sweep: f32,
    color: egui::Color32,
) {
    if sweep <= 0.0 {
        return;
    }
    let sweep = sweep.min(std::f32::consts::TAU);
    // 每 ~10° 一段：16px 徽标上足够平滑，且形状数可控。
    let steps = ((sweep / 0.18).ceil() as usize).clamp(2, 40);
    let mut prev = center + egui::vec2(0.0, -radius);
    let stroke = egui::Stroke::new(1.6, color);
    for i in 1..=steps {
        let angle = start + sweep * (i as f32 / steps as f32);
        let point = center + egui::vec2(angle.sin() * radius, -angle.cos() * radius);
        painter.line_segment([prev, point], stroke);
        prev = point;
    }
}

/// 行首文件/目录矢量图标：文件夹采用 macOS 风格的蓝色渐变与高光，
/// 文件为细描边轮廓。矢量绘制避免 emoji 字形随字体变化。
fn paint_entry_icon(painter: &egui::Painter, rect: egui::Rect, is_dir: bool) {
    if is_dir {
        let h = rect.height();
        let w = rect.width();
        let folder_top = egui::Color32::from_rgb(0x9a, 0xd9, 0xff);
        let folder_mid = egui::Color32::from_rgb(0x5a, 0xb4, 0xf3);
        let folder_bottom = egui::Color32::from_rgb(0x2d, 0x7f, 0xd1);
        let folder_border = egui::Color32::from_rgba_unmultiplied(0x1a, 0x63, 0xa8, 0xd0);

        // 轻微底部阴影让小图标从深色列表背景中脱出来，同时保持
        // macOS Finder 图标那种柔和、偏亮的体积感。
        let shadow = egui::Rect::from_min_max(
            egui::pos2(rect.left() + 0.5, rect.top() + 1.0),
            egui::pos2(rect.right() + 0.5, rect.bottom() + 1.0),
        );
        painter.rect_filled(
            shadow,
            2.8,
            egui::Color32::from_rgba_unmultiplied(0x04, 0x2b, 0x56, 0x88),
        );

        // 文件夹提手先画在后面，主体覆盖提手下沿，形成干净的折角。
        let tab = egui::Rect::from_min_size(
            egui::pos2(rect.left() + 0.7, rect.top() + h * 0.08),
            egui::vec2(w * 0.56, h * 0.36),
        );
        let body = egui::Rect::from_min_max(
            egui::pos2(rect.left() + 0.3, rect.top() + h * 0.28),
            egui::pos2(rect.right() - 0.3, rect.bottom() - 0.5),
        );
        crate::anim::paint_rounded_gradient(painter, tab, 2.5, folder_top, folder_mid);
        crate::anim::paint_rounded_gradient(painter, body, 2.8, folder_top, folder_bottom);
        painter.rect_stroke(
            body.shrink(0.35),
            2.5,
            egui::Stroke::new(0.65, folder_border),
            egui::StrokeKind::Inside,
        );
        // 顶部一条半透明高光，尺寸很小时仍能读出蓝色文件夹的层次。
        painter.line_segment(
            [
                egui::pos2(body.left() + 2.0, body.top() + 1.0),
                egui::pos2(body.right() - 2.0, body.top() + 1.0),
            ],
            egui::Stroke::new(0.8, egui::Color32::from_white_alpha(92)),
        );
    } else {
        let theme = crate::theme::current_theme();
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

/// 判断新建/重命名使用的是否为单个远程目录项名称。
///
/// 这里不允许路径分隔符、NUL、`.` 和 `..`，避免用户借助输入框把操作
/// 指向当前目录之外；POSIX 允许反斜杠出现在文件名中，因此不将其误判为
/// 路径分隔符。
fn is_valid_entry_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/') && !name.contains('\0')
}

/// 归一化远程 POSIX 路径。
fn normalize_remote_path(path: &str) -> String {
    let path = path.trim();
    let absolute = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }

    if absolute {
        if parts.is_empty() {
            "/".to_string()
        } else {
            format!("/{}", parts.join("/"))
        }
    } else if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
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
        use kittest::{NodeT, Queryable};

        // 直接构造带条目的面板（不依赖网络）。
        let (_tx, rx) = tokio::sync::mpsc::channel(128);
        let (handle_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = SftpHandle::from_raw(handle_tx);
        let mut view = SftpView {
            host_name: "测试主机".into(),
            handle,
            rx,
            current_path: "/".into(),
            sftp_home: "/".into(),
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
            transfers_expanded: true,
            dialog: None,
            error: None,
            notice: None,
            closed: false,
            cell_width: 0.0,
            last_primary_click: None,
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
        // 头部标题与关闭按钮共用"确认删除"文案（Window + Button 各一个），
        // 仅断言标题节点唯一存在。
        assert!(
            harness
                .root()
                .query_all_by_role(accesskit::Role::Window)
                .any(|n| n.accesskit_node().label() == Some("确认删除".to_string())),
            "确认删除窗口应出现"
        );
        harness.get_by_label("readme.md");

        let dialog_rect = harness
            .root()
            .query_all_by_role(accesskit::Role::Window)
            .find(|n| n.accesskit_node().label() == Some("确认删除".to_string()))
            .expect("确认删除窗口应存在")
            .rect();
        let viewport = harness.ctx.content_rect();
        assert!(
            (dialog_rect.center().x - viewport.center().x).abs() < 1.0,
            "确认删除窗口应水平居中，窗口中心 x={}，视口中心 x={}",
            dialog_rect.center().x,
            viewport.center().x
        );
        assert!(
            (dialog_rect.center().y - viewport.center().y).abs() < 1.0,
            "确认删除窗口应垂直居中，窗口中心 y={}，视口中心 y={}",
            dialog_rect.center().y,
            viewport.center().y
        );

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
            sftp_home: "/".into(),
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
            transfers_expanded: true,
            dialog: None,
            error: None,
            notice: None,
            closed: false,
            cell_width: 0.0,
            last_primary_click: None,
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

    /// 上传事件应落在详情卡片（默认展开），而不是只显示一个徽标。
    #[test]
    fn 上传进度独立区域且列表上方可见() {
        use kittest::Queryable;

        let (event_tx, rx) = tokio::sync::mpsc::channel(128);
        let (handle_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = SftpHandle::from_raw(handle_tx);
        let mut view = SftpView {
            host_name: "测试主机".into(),
            handle,
            rx,
            current_path: "/home/test".into(),
            sftp_home: "/home/test".into(),
            entries: Vec::new(),
            selected: vec![],
            selection_anchor: None,
            loading: false,
            transfers: Vec::new(),
            transfers_expanded: true,
            dialog: None,
            error: None,
            notice: None,
            closed: false,
            cell_width: 0.0,
            last_primary_click: None,
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
        // 标题行徽标（进行中）与详情卡片同时出现。
        harness.get_by_label("传输进度 · 进行中 1 项");
        harness.get_by_label("上传进度");
        harness.get_by_label("上传 demo.bin");
        // 区域在列表内容（"空目录"提示）之前，处于面板可视范围内。
        // 回归：区域曾布局在 ScrollArea 之后，超出面板可视区被裁剪，
        // 上传开始后完全看不到进度。
        let progress = harness.get_by_label("上传进度").rect();
        let empty = harness.get_by_label("空目录").rect();
        assert!(
            progress.top() < empty.top(),
            "上传进度区域应在文件列表上方（原 ScrollArea 之后不可见）"
        );
        assert!(progress.top() >= 0.0 && progress.top() < 400.0);
    }

    /// 点击标题行徽标可收起/展开传输详情；收起后只剩徽标，文件列表
    /// 获得纵向空间；徽标按聚合状态区分（进行中/失败/完成）。
    #[test]
    fn 传输徽标点击收起展开详情() {
        use kittest::Queryable;

        let (_tx, rx) = tokio::sync::mpsc::channel(128);
        let (handle_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = SftpHandle::from_raw(handle_tx);
        let mut view = SftpView {
            host_name: "测试主机".into(),
            handle,
            rx,
            current_path: "/home/test".into(),
            sftp_home: "/home/test".into(),
            entries: Vec::new(),
            selected: vec![],
            selection_anchor: None,
            loading: false,
            transfers: vec![Transfer {
                id: 1,
                label: "上传 demo.bin".into(),
                done: 512,
                total: 1024,
                finished: false,
                failed: false,
            }],
            transfers_expanded: true,
            dialog: None,
            error: None,
            notice: None,
            closed: false,
            cell_width: 0.0,
            last_primary_click: None,
        };

        let mut harness = egui_kittest::Harness::new_ui(|ui| view.show(ui));
        harness.run_steps(2);
        // 展开态：徽标 + 详情卡片同时可见。
        harness.get_by_label("传输进度 · 进行中 1 项");
        harness.get_by_label("上传进度");

        // 点击徽标收起：详情消失，徽标保留。
        harness.get_by_label("传输进度 · 进行中 1 项").click();
        harness.run_steps(2);
        harness.get_by_label("传输进度 · 进行中 1 项");
        assert!(
            harness.root().query_by_label("上传进度").is_none(),
            "收起后不应继续显示传输详情卡片"
        );

        // 再次点击展开：详情恢复。
        harness.get_by_label("传输进度 · 进行中 1 项").click();
        harness.run_steps(2);
        harness.get_by_label("上传进度");
        harness.get_by_label("上传 demo.bin");
    }

    /// 无传输时不画徽标；完成后徽标为完成态、有失败时为失败态。
    ///
    /// 注意：harness 闭包会可变借用 `view`，中途不能再直接 push——
    /// 三个状态各自用独立 harness 验证（断言纯逻辑 + 渲染各一次）。
    #[test]
    fn 传输徽标按聚合状态区分() {
        use kittest::Queryable;

        fn make_view(transfers: Vec<Transfer>) -> SftpView {
            let (_tx, rx) = tokio::sync::mpsc::channel(128);
            let (handle_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
            SftpView {
                host_name: "测试主机".into(),
                handle: SftpHandle::from_raw(handle_tx),
                rx,
                current_path: "/home/test".into(),
                sftp_home: "/home/test".into(),
                entries: Vec::new(),
                selected: vec![],
                selection_anchor: None,
                loading: false,
                transfers,
                transfers_expanded: true,
                dialog: None,
                error: None,
                notice: None,
                closed: false,
                cell_width: 0.0,
                last_primary_click: None,
            }
        }
        fn done_transfer(id: u64, name: &str) -> Transfer {
            Transfer {
                id,
                label: format!("上传 {name}"),
                done: 1024,
                total: 1024,
                finished: true,
                failed: false,
            }
        }

        // 无传输：无状态、徽标不应出现。
        let view = make_view(Vec::new());
        assert_eq!(view.transfer_state(), None);
        let mut view = view;
        let mut harness = egui_kittest::Harness::new_ui(|ui| view.show(ui));
        harness.run_steps(2);
        assert!(
            harness
                .root()
                .query_all_by_label("传输进度 · 全部完成")
                .next()
                .is_none(),
            "无传输时不应出现传输徽标"
        );

        // 全部完成：完成态徽标。
        let view = make_view(vec![done_transfer(1, "done.bin")]);
        assert_eq!(view.transfer_state(), Some(TransferState::Done));
        let mut view = view;
        let mut harness = egui_kittest::Harness::new_ui(|ui| view.show(ui));
        harness.run_steps(2);
        harness.get_by_label("传输进度 · 全部完成");

        // 有失败：失败态徽标优先于完成。
        let view = make_view(vec![
            done_transfer(1, "done.bin"),
            Transfer {
                id: 2,
                label: "上传 bad.bin".into(),
                done: 0,
                total: 100,
                finished: false,
                failed: true,
            },
        ]);
        assert_eq!(
            view.transfer_state(),
            Some(TransferState::Failed { failed: 1 })
        );
        let mut view = view;
        let mut harness = egui_kittest::Harness::new_ui(|ui| view.show(ui));
        harness.run_steps(2);
        harness.get_by_label("传输进度 · 有 1 项失败");

        // 有进行中：进行中优先于失败（数量为未结束项数）。
        let view = make_view(vec![
            done_transfer(1, "done.bin"),
            Transfer {
                id: 2,
                label: "上传 bad.bin".into(),
                done: 0,
                total: 100,
                finished: false,
                failed: true,
            },
            Transfer {
                id: 3,
                label: "上传 ing.bin".into(),
                done: 10,
                total: 100,
                finished: false,
                failed: false,
            },
        ]);
        assert_eq!(
            view.transfer_state(),
            Some(TransferState::Uploading { active: 1 })
        );
        let mut view = view;
        let mut harness = egui_kittest::Harness::new_ui(|ui| view.show(ui));
        harness.run_steps(2);
        harness.get_by_label("传输进度 · 进行中 1 项");
    }

    /// 上传全部结束后，进度区域应提供关闭入口并清理记录。
    #[test]
    fn 上传完成后可关闭进度区域() {
        use kittest::Queryable;

        let (_tx, rx) = tokio::sync::mpsc::channel(128);
        let (handle_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = SftpHandle::from_raw(handle_tx);
        let mut view = SftpView {
            host_name: "测试主机".into(),
            handle,
            rx,
            current_path: "/home/test".into(),
            sftp_home: "/home/test".into(),
            entries: Vec::new(),
            selected: vec![],
            selection_anchor: None,
            loading: false,
            transfers: vec![Transfer {
                id: 1,
                label: "上传 done.bin".into(),
                done: 1024,
                total: 1024,
                finished: true,
                failed: false,
            }],
            transfers_expanded: true,
            dialog: None,
            error: None,
            notice: None,
            closed: false,
            cell_width: 0.0,
            last_primary_click: None,
        };

        let mut harness = egui_kittest::Harness::new_ui(|ui| view.show(ui));
        harness.run_steps(2);
        harness.get_by_label("上传进度");
        harness.get_by_label("关闭").click();
        harness.run_steps(2);
        assert!(
            harness.root().query_by_label("上传进度").is_none(),
            "关闭后不应继续显示上传进度区域"
        );
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
                refresh: true,
                path: None,
            })
            .unwrap();
        event_tx
            .try_send(SftpEvent::Error {
                id: Some(2),
                label: "上传 same.bin".into(),
                message: "失败".into(),
                path: None,
            })
            .unwrap();

        assert!(view.poll_events());
        assert_eq!(view.transfers.len(), 2);
        let first = view
            .transfers
            .iter()
            .find(|transfer| transfer.id == 1)
            .unwrap();
        assert!(first.finished && !first.failed && first.done == first.total);
        let second = view
            .transfers
            .iter()
            .find(|transfer| transfer.id == 2)
            .unwrap();
        assert!(!second.finished && second.failed && second.done == 20);
    }

    /// ⌘⇧L 只表达定位意图：面板向上传递请求，真正的目录导航由应用层
    /// 在终端 `pwd` 探测完成后经 `locate_terminal_directory` 完成。
    /// 回归：曾直接用跟踪器里的旧推测值导航，输入跟踪失效时（Tab/粘贴/
    /// 别名/函数）定位到的一直是旧目录，“只有 pwd 后才好用”。
    #[test]
    fn 快捷键定位终端目录() {
        let (_tx, rx) = tokio::sync::mpsc::channel(128);
        let (handle_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = SftpHandle::from_raw(handle_tx);
        let mut view = SftpView {
            host_name: "测试主机".into(),
            handle,
            rx,
            current_path: "/".into(),
            sftp_home: "/".into(),
            entries: Vec::new(),
            selected: vec![],
            selection_anchor: None,
            loading: false,
            transfers: Vec::new(),
            transfers_expanded: true,
            dialog: None,
            error: None,
            notice: None,
            closed: false,
            cell_width: 0.0,
            last_primary_click: None,
        };
        let locate_requested = std::rc::Rc::new(std::cell::Cell::new(false));
        let locate_flag = locate_requested.clone();
        let mut harness = egui_kittest::Harness::new_ui(|ui| {
            if view
                .show_with_terminal_cwd(ui, Some("/srv/project"))
                .is_some()
            {
                locate_flag.set(true);
            }
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

        assert!(
            locate_requested.get(),
            "快捷键应向上传递定位请求，由应用层探测后导航"
        );
        assert!(
            cmd_rx.try_recv().is_err(),
            "面板自身不应再直接发出列表请求，避免用旧推测值导航"
        );
    }

    /// `locate_terminal_directory` 负责真正的目录导航：目标与当前目录
    /// 不同时导航并提示"定位到…"；相同时不再刷新、提示"已在终端目录"。
    /// 回归：曾只做静默导航，路径相同时列表原地刷新看不出任何变化，
    /// 用户感觉"点击后无反应"。
    #[test]
    fn 定位终端目录有明确反馈() {
        use kittest::Queryable;
        use mino_core::ssh::sftp::SftpCmd;

        let (event_tx, rx) = tokio::sync::mpsc::channel(128);
        let (handle_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = SftpHandle::from_raw(handle_tx);
        let mut view = SftpView {
            host_name: "测试主机".into(),
            handle,
            rx,
            current_path: "/".into(),
            sftp_home: "/".into(),
            entries: Vec::new(),
            selected: vec![],
            selection_anchor: None,
            loading: false,
            transfers: Vec::new(),
            transfers_expanded: true,
            dialog: None,
            error: None,
            notice: None,
            closed: false,
            cell_width: 0.0,
            last_primary_click: None,
        };
        // 场景一：目标与当前不同 → 导航 + 提示。
        // 应用层在终端 `pwd` 探测完成后调用本方法（此处直接模拟调用）。
        view.locate_terminal_directory("/srv/project");
        let cmd = cmd_rx.try_recv().expect("定位应发出目录列表请求");
        assert!(matches!(&cmd, SftpCmd::List { path } if path == "/srv/project"));

        {
            let mut harness = egui_kittest::Harness::new_ui(|ui| {
                view.show(ui);
            });
            harness.run_steps(2);
            harness.get_by_label("定位到终端目录 /srv/project");

            // 模拟导航完成（面板当前目录变为 /srv/project）。
            event_tx
                .try_send(SftpEvent::Listed {
                    path: "/srv/project".into(),
                    entries: Vec::new(),
                })
                .unwrap();
            harness.run_steps(2);
        }

        // 场景二：目标与当前相同 → 不刷新，只提示"已在"。
        view.locate_terminal_directory("/srv/project");
        {
            let mut harness = egui_kittest::Harness::new_ui(|ui| {
                view.show(ui);
            });
            harness.run_steps(2);
            assert!(
                cmd_rx.try_recv().is_err(),
                "定位到相同目录不应重新发出列表请求"
            );
            harness.get_by_label("已在终端目录 /srv/project");
        }
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
            sftp_home: "/".into(),
            entries: Vec::new(),
            selected: vec![],
            selection_anchor: None,
            loading: false,
            transfers: Vec::new(),
            transfers_expanded: true,
            dialog: None,
            error: None,
            notice: None,
            closed: false,
            cell_width: 0.0,
            last_primary_click: None,
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
            sftp_home: "/very/long/remote/path/that/overflows/".into(),
            entries: Vec::new(),
            selected: vec![],
            selection_anchor: None,
            loading: false,
            transfers: Vec::new(),
            transfers_expanded: true,
            dialog: None,
            error: None,
            notice: None,
            closed: false,
            cell_width: 0.0,
            last_primary_click: None,
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

    /// 单击选中，双击目录进入下级目录（发 List 命令）。
    /// 第一次单击只选中，普通双击目录进入下级目录（发 List 命令）；
    /// 修饰键单击只选中，文件单击永远不导航。
    /// 回归：egui 全局 click_count 会被其它控件的点击污染，必须按行独立
    /// 识别双击。
    #[test]
    fn 单击选中双击目录进入() {
        use kittest::Queryable;
        use mino_core::ssh::sftp::SftpCmd;

        let (event_tx, rx) = tokio::sync::mpsc::channel(128);
        let (handle_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = SftpHandle::from_raw(handle_tx);
        let mut view = SftpView {
            host_name: "测试主机".into(),
            handle,
            rx,
            current_path: "/".into(),
            sftp_home: "/".into(),
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
            transfers_expanded: true,
            dialog: None,
            error: None,
            notice: None,
            closed: false,
            cell_width: 0.0,
            last_primary_click: None,
        };

        let mut harness = egui_kittest::Harness::builder()
            .with_step_dt(0.1)
            .build_ui(|ui| {
                view.show(ui);
            });
        harness.run();

        // 修饰键（⌘/Ctrl）单击目录：只选中，不导航（供批量操作）。
        harness
            .get_by_label("workspace/")
            .click_modifiers(egui::Modifiers::COMMAND);
        harness.run_steps(2);
        assert!(
            cmd_rx.try_recv().is_err(),
            "Cmd 单击目录只应选中，不应发出进入目录命令"
        );

        // 第一次普通单击目录 → 只选中，不进入目录。
        harness.get_by_label("workspace/").click();
        harness.run_steps(1);
        assert!(
            cmd_rx.try_recv().is_err(),
            "第一次单击只应选中，不应发出进入目录命令"
        );

        // 第二次在双击时间窗内点击同一目录 → 进入下级目录。
        harness.get_by_label("workspace/").click();
        harness.run_steps(6);
        let cmd = cmd_rx.try_recv().expect("双击目录应发出进入目录命令");
        assert!(
            matches!(&cmd, SftpCmd::List { path } if path == "/workspace"),
            "进入的路径应为 /workspace，收到 {cmd:?}"
        );

        event_tx
            .try_send(SftpEvent::Listed {
                path: "/workspace".into(),
                entries: vec![RemoteEntry {
                    name: "notes.txt".into(),
                    is_dir: false,
                    size: 12,
                    modified: None,
                    permissions: 0,
                }],
            })
            .unwrap();
        harness.run_steps(2);

        // 文件不受影响：单击两次也只是选中，不导航。
        harness.get_by_label("notes.txt").click();
        harness.run_steps(1);
        harness.get_by_label("notes.txt").click();
        harness.run_steps(1);
        assert!(
            cmd_rx.try_recv().is_err(),
            "文件不应发出任何命令（无目录可进）"
        );
    }

    /// ".." 行双击返回上级目录，单击不导航。
    #[test]
    fn 双击上级目录回退() {
        use kittest::Queryable;
        use mino_core::ssh::sftp::SftpCmd;

        let (_tx, rx) = tokio::sync::mpsc::channel(128);
        let (handle_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = SftpHandle::from_raw(handle_tx);
        let mut view = SftpView {
            host_name: "测试主机".into(),
            handle,
            rx,
            current_path: "/workspace/logs".into(),
            sftp_home: "/workspace/logs".into(),
            entries: Vec::new(),
            selected: vec![],
            selection_anchor: None,
            loading: false,
            transfers: Vec::new(),
            transfers_expanded: true,
            dialog: None,
            error: None,
            notice: None,
            closed: false,
            cell_width: 0.0,
            last_primary_click: None,
        };

        let mut harness = egui_kittest::Harness::builder()
            .with_step_dt(0.1)
            .build_ui(|ui| view.show(ui));
        harness.run();

        harness.get_by_label("..").click();
        harness.run_steps(1);
        assert!(cmd_rx.try_recv().is_err(), "单击 .. 不应回退目录");

        harness.get_by_label("..").click();
        harness.run_steps(4);
        let cmd = cmd_rx.try_recv().expect("双击 .. 应发出回退请求");
        assert!(
            matches!(&cmd, SftpCmd::List { path } if path == "/workspace"),
            "回退路径应为 /workspace，收到 {cmd:?}"
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
        assert_eq!(normalize_remote_path("/home/./user/../user/"), "/home/user");
        assert_eq!(normalize_remote_path("//home//user"), "/home/user");
    }

    #[test]
    fn 导航加载期间稳定列表并忽略旧结果() {
        let (event_tx, rx) = tokio::sync::mpsc::channel(128);
        let (handle_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = SftpHandle::from_raw(handle_tx);
        let mut view = SftpView {
            host_name: "测试主机".into(),
            handle,
            rx,
            current_path: "/home".into(),
            sftp_home: "/home".into(),
            entries: vec![RemoteEntry {
                name: "old.txt".into(),
                is_dir: false,
                size: 1,
                modified: None,
                permissions: 0,
            }],
            selected: vec!["old.txt".into()],
            selection_anchor: Some("old.txt".into()),
            loading: false,
            transfers: Vec::new(),
            transfers_expanded: true,
            dialog: None,
            error: None,
            notice: None,
            closed: false,
            cell_width: 0.0,
            last_primary_click: None,
        };

        view.navigate_to("/home/./workspace/../workspace");
        assert_eq!(view.current_path, "/home/workspace");
        assert_eq!(
            view.entries[0].name, "old.txt",
            "加载期间保留旧列表行作为占位"
        );
        assert!(view.selected.is_empty());
        assert!(view.loading);
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(mino_core::ssh::sftp::SftpCmd::List { path }) if path == "/home/workspace"
        ));

        // 旧目录中的删除/上传完成后不能触发当前目录的刷新，否则会把
        // 用户刚切换到的新目录重新置为加载中，并可能覆盖其列表结果。
        event_tx
            .try_send(SftpEvent::Done {
                id: None,
                label: "删除 old.txt".into(),
                refresh: true,
                path: Some("/home".into()),
            })
            .unwrap();
        assert!(view.poll_events());
        assert!(view.loading, "当前目录的列表请求仍应保持加载状态");
        assert!(cmd_rx.try_recv().is_err(), "旧目录完成事件不应追加刷新请求");

        event_tx
            .try_send(SftpEvent::Listed {
                path: "/home".into(),
                entries: vec![RemoteEntry {
                    name: "stale.txt".into(),
                    is_dir: false,
                    size: 1,
                    modified: None,
                    permissions: 0,
                }],
            })
            .unwrap();
        assert!(view.poll_events());
        assert_eq!(
            view.entries[0].name, "old.txt",
            "旧目录结果不能回写当前页面"
        );
        assert!(view.loading);

        event_tx
            .try_send(SftpEvent::Error {
                id: None,
                label: "列出目录".into(),
                message: "No such file: No such file".into(),
                path: Some("/home".into()),
            })
            .unwrap();
        assert!(view.poll_events());
        assert!(view.error.is_none(), "旧目录错误不能污染当前页面");
        assert!(view.loading);

        event_tx
            .try_send(SftpEvent::Listed {
                path: "/home/workspace".into(),
                entries: vec![RemoteEntry {
                    name: "new.txt".into(),
                    is_dir: false,
                    size: 2,
                    modified: None,
                    permissions: 0,
                }],
            })
            .unwrap();
        assert!(view.poll_events());
        assert_eq!(view.entries[0].name, "new.txt");
        assert!(!view.loading);
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
            sftp_home: "/".into(),
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
            transfers_expanded: true,
            dialog: None,
            error: None,
            notice: None,
            closed: false,
            cell_width: 0.0,
            last_primary_click: None,
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
