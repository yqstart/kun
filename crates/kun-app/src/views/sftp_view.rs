//! SFTP 面板：远程文件浏览、传输进度与文件操作。

use egui::{Color32, RichText, Ui};
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

        // ==================== 工具栏 ====================
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("SFTP · {}", self.host_name)).strong());
            ui.separator();
            if ui.button("上传").on_hover_text("上传文件").clicked() {
                self.upload_dialog();
            }
            if ui.button("下载").on_hover_text("下载选中文件").clicked() {
                self.download_selected();
            }
            ui.separator();
            if ui.button("新建目录").clicked() {
                self.dialog = Some(ConfirmDialog::Mkdir {
                    path: self.join(""),
                    input: String::new(),
                });
            }
            if ui.button("重命名").clicked() {
                if let Some(name) = self.selected.clone() {
                    self.dialog = Some(ConfirmDialog::Rename {
                        from: name.clone(),
                        path: self.join(&name),
                        input: name,
                    });
                }
            }
            if ui.button("删除").clicked() {
                if let Some(name) = self.selected.clone() {
                    let entry = self.entries.iter().find(|e| e.name == name);
                    self.dialog = Some(ConfirmDialog::Delete {
                        name: name.clone(),
                        path: self.join(&name),
                        is_dir: entry.map(|e| e.is_dir).unwrap_or(false),
                    });
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("刷新").clicked() {
                    self.handle.list(&self.current_path);
                    self.loading = true;
                }
            });
        });

        // ==================== 路径栏 ====================
        ui.horizontal(|ui| {
            if ui.button("上级").on_hover_text("返回上级目录").clicked() {
                let parent = Self::parent_of(&self.current_path);
                self.handle.list(&parent);
                self.loading = true;
            }
            ui.label(
                RichText::new(&self.current_path)
                    .monospace()
                    .color(crate::theme::current_theme().text_muted),
            );
        });

        // ==================== 错误提示 ====================
        if let Some(err) = &self.error {
            ui.colored_label(Color32::from_rgb(0xff, 0x55, 0x55), err);
        }
        if self.closed {
            ui.colored_label(Color32::from_rgb(0xff, 0x55, 0x55), "连接已关闭");
        }

        // ==================== 文件列表 ====================
        let cell_width = ui.fonts_mut(|f| {
            let font = egui::FontId::monospace(13.0);
            f.glyph_width(&font, ' ')
        });
        let table_width = ui.available_width();

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if self.loading {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("加载中…");
                    });
                }
                // 表头。
                ui.horizontal(|ui| {
                    ui.label(RichText::new("名称").strong());
                    ui.add_space(table_width - cell_width * 38.0);
                    ui.label(RichText::new("大小").strong());
                    ui.add_space(cell_width * 10.0);
                    ui.label(RichText::new("修改时间").strong());
                });
                ui.separator();

                let mut open_dir: Option<String> = None;
                let mut select: Option<String> = None;
                for entry in &self.entries {
                    let selected = self.selected.as_deref() == Some(entry.name.as_str());
                    let label = if entry.is_dir {
                        format!("{}/", entry.name)
                    } else {
                        entry.name.clone()
                    };
                    let mut name_text = RichText::new(label).monospace();
                    if selected {
                        name_text = name_text.background_color(Color32::from_rgb(0x44, 0x47, 0x5a));
                    }
                    let response = ui.horizontal(|ui| {
                        ui.label(name_text);
                        ui.add_space(table_width - cell_width * 38.0 - 2.0 * cell_width);
                        if entry.is_dir {
                            ui.weak("—");
                        } else {
                            ui.weak(Self::format_size(entry.size));
                        }
                        ui.add_space(cell_width * 10.0);
                        if let Some(modified) = entry.modified {
                            ui.weak(Self::format_time(modified));
                        }
                    });
                    if response.response.clicked() {
                        select = Some(entry.name.clone());
                    }
                    if response.response.double_clicked() && entry.is_dir {
                        open_dir = Some(entry.name.clone());
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
                    ui.label(
                        RichText::new("空目录").color(crate::theme::current_theme().text_muted),
                    );
                }
            });

        // ==================== 传输进度 ====================
        if !self.transfers.is_empty() {
            ui.separator();
            ui.label(RichText::new("传输").strong());
            // 只保留最近的传输记录（上限 12 条）。
            if self.transfers.len() > 12 {
                self.transfers.drain(..self.transfers.len() - 12);
            }
            for transfer in &self.transfers {
                ui.horizontal(|ui| {
                    let (color, text) = if transfer.failed {
                        (Color32::from_rgb(0xff, 0x55, 0x55), "失败")
                    } else if transfer.finished {
                        (Color32::from_rgb(0x50, 0xfa, 0x7b), "完成")
                    } else {
                        (Color32::from_rgb(0x8b, 0xe9, 0xfd), "进行中")
                    };
                    ui.colored_label(color, text);
                    ui.label(&transfer.label);
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

    /// 格式化时间戳。
    fn format_time(ts: u64) -> String {
        // 简化：显示 UTC 日期（完整时区处理后续优化）。
        let days = ts / 86400;
        let years = 1970 + days / 365;
        let month = (days % 365) / 30 + 1;
        let day = (days % 365) % 30 + 1;
        format!("{years}-{month:02}-{day:02}")
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

    #[test]
    fn 路径拼接与上级目录() {
        assert_eq!(join_path("/home/user", "file.txt"), "/home/user/file.txt");
        assert_eq!(join_path("/", "etc"), "/etc");
        assert_eq!(join_path("/home/user/", "a"), "/home/user/a");
        assert_eq!(parent_of("/home/user"), "/home");
        assert_eq!(parent_of("/home"), "/");
        assert_eq!(parent_of("/"), "/");
    }

    #[test]
    fn 大小格式化() {
        assert_eq!(SftpView::format_size(0), "0 B");
        assert_eq!(SftpView::format_size(1024), "1.0 KB");
        assert_eq!(SftpView::format_size(1024 * 1024), "1.0 MB");
        assert_eq!(SftpView::format_size(1024 * 1024 * 1024), "1.0 GB");
    }
}
