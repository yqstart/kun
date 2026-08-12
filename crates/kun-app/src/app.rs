//! kun 应用主体：布局与状态管理。

use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui;
use kun_core::config::{default_config_path, Auth, HostConfig, HostProfile};
use kun_core::ssh::{connect_remote, ConnectResult};
use kun_core::terminal::{Session, SessionEvent, SessionOptions};
use kun_core::updater::{check_for_update, UpdateInfo};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::views::sftp_view::SftpView;
use crate::views::terminal_view::TerminalView;
use kun_core::ssh::sftp::{connect_sftp, SftpEvent, SftpHandle};

/// 新建连接表单状态。
#[derive(Default)]
struct ConnectForm {
    name: String,
    host: String,
    port: String,
    user: String,
    auth_kind: usize, // 0=密码 1=私钥
    password: String,
    key_path: String,
    passphrase: String,
    /// 名称输入框是否已聚焦（对话框打开时自动聚焦）。
    name_focused: bool,
}

/// 更新检查状态。
enum UpdateState {
    /// 未检查。
    Idle,
    /// 检查中。
    Checking,
    /// 发现新版本。
    Available(UpdateInfo),
    /// 已是最新。
    UpToDate,
    /// 检查失败（静默，不打扰用户）。
    Failed,
}

/// 单个终端标签页（本地或远程会话）。
struct TerminalTab {
    /// 标签标题（远程=主机名；本地=动态会话标题）。
    label: String,
    terminal: TerminalView,
    /// 远程会话的 SFTP 面板（本地标签页为 None）。
    sftp: Option<SftpView>,
}

impl TerminalTab {
    fn new(label: String, terminal: TerminalView) -> Self {
        Self {
            label,
            terminal,
            sftp: None,
        }
    }

    /// 标签显示标题（跟随会话标题变化）。
    fn title(&self) -> String {
        let t = self.terminal.session().title();
        if t.is_empty() {
            self.label.clone()
        } else {
            t
        }
    }
}

/// 应用状态。
pub struct KunApp {
    /// 终端标签页列表。
    tabs: Vec<TerminalTab>,
    /// 当前激活的标签页索引。
    active_tab: usize,
    /// 连接成功后创建的标签页索引（用于挂载 SFTP）。
    pending_tab: Option<usize>,
    /// 左侧主机列表是否展开（默认收起，直接进入终端）。
    sidebar_open: bool,
    /// 主机行最近一次点击（时间, 行索引），用于自实现双击检测。
    /// （egui 的多击计数会把无关点击（如折叠按钮）计入序列，导致 count=3
    /// 而 double_clicked（count==2）失效，故自行记录）
    last_row_click: Option<(f64, usize)>,
    /// 主机配置。
    config: HostConfig,
    config_path: PathBuf,
    /// 是否显示新建连接对话框。
    show_new_conn: bool,
    /// 连接表单。
    form: ConnectForm,
    /// 进行中的远程连接。
    pending: Option<UnboundedReceiver<ConnectResult>>,
    pending_label: String,
    /// 提示消息（消息, 是否错误）。
    toast: Option<(String, bool)>,
    /// 进行中的 SFTP 连接。
    pending_sftp: Option<(SftpHandle, UnboundedReceiver<SftpEvent>)>,
    /// 远程连接的主机名（用于 SFTP 面板标题）。
    sftp_host: String,
    /// 更新检查状态。
    update_state: UpdateState,
    /// 更新检查结果接收端。
    update_rx: Option<std::sync::mpsc::Receiver<Result<Option<UpdateInfo>, String>>>,
}

/// 本地终端会话选项：默认工作目录为 home。
/// （Finder/Dock 启动时进程 cwd 为 `/`，不指定会导致终端落在根目录）
fn local_session_options() -> SessionOptions {
    SessionOptions {
        working_directory: std::env::var("HOME").ok().map(PathBuf::from),
        ..Default::default()
    }
}

impl KunApp {
    /// 创建应用（启动本地终端会话）。
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        crate::theme::set_theme(&cc.egui_ctx, 0);
        let ctx = cc.egui_ctx.clone();

        // 加载主机配置。
        let config_path = default_config_path();
        let config = HostConfig::load(&config_path).unwrap_or_default();

        let mut app = Self {
            tabs: Vec::new(),
            active_tab: 0,
            pending_tab: None,
            sidebar_open: false,
            last_row_click: None,
            config,
            config_path,
            show_new_conn: false,
            form: ConnectForm::default(),
            pending: None,
            pending_label: String::new(),
            toast: None,
            pending_sftp: None,
            sftp_host: String::new(),
            update_state: UpdateState::Idle,
            update_rx: None,
        };
        // 启动时自动检查更新（后台线程，延迟 3 秒）。
        app.start_update_check(true);
        // 初始打开一个本地终端标签页。
        app.new_local_tab(&ctx);
        app
    }

    /// 新建本地终端标签页并激活。
    fn new_local_tab(&mut self, ctx: &egui::Context) {
        let ctx = ctx.clone();
        let on_event = Arc::new(move |_ev: &SessionEvent| {
            ctx.request_repaint();
        });
        match Session::spawn_local(local_session_options(), 80, 24, on_event) {
            Ok(session) => {
                self.tabs.push(TerminalTab::new(
                    "本地终端".into(),
                    TerminalView::new(session),
                ));
                self.active_tab = self.tabs.len() - 1;
            }
            Err(e) => {
                log::error!("启动本地终端失败：{e}");
                self.toast = Some((format!("启动本地终端失败：{e}"), true));
            }
        }
    }

    /// 关闭指定标签页（会话随之 Drop 优雅关闭）。
    fn close_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        self.tabs.remove(index);
        if self.tabs.is_empty() {
            // 全部关闭后重置激活索引（可再新建）。
            self.active_tab = 0;
        } else if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len() - 1;
        } else if self.active_tab > index {
            self.active_tab -= 1;
        }
    }

    /// 启动后台更新检查（delay=true 时延迟 3 秒，避免影响启动）。
    fn start_update_check(&mut self, delay: bool) {
        let (tx, rx) = std::sync::mpsc::channel();
        let current = env!("CARGO_PKG_VERSION").to_string();
        std::thread::spawn(move || {
            if delay {
                std::thread::sleep(std::time::Duration::from_secs(3));
            }
            let result = check_for_update(&current, kun_core::updater::DEFAULT_REPO);
            let _ = tx.send(result);
        });
        self.update_rx = Some(rx);
        self.update_state = UpdateState::Checking;
    }

    /// 处理更新检查结果。
    fn poll_update(&mut self) {
        let mut result = None;
        if let Some(rx) = &self.update_rx {
            while let Ok(r) = rx.try_recv() {
                result = Some(r);
            }
        }
        if let Some(result) = result {
            self.update_rx = None;
            self.update_state = match result {
                Ok(Some(info)) => UpdateState::Available(info),
                Ok(None) => UpdateState::UpToDate,
                Err(e) => {
                    log::debug!("检查更新失败：{e}");
                    UpdateState::Failed
                }
            };
        }
    }

    /// 保存主机配置到磁盘。
    fn save_config(&self) {
        if let Err(e) = self.config.save(&self.config_path) {
            log::warn!("保存主机配置失败：{e}");
        }
    }

    /// 处理进行中的连接结果。
    fn poll_connection(&mut self, ctx: &egui::Context) {
        let mut result = None;
        if let Some(rx) = &mut self.pending {
            while let Ok(r) = rx.try_recv() {
                result = Some(r);
            }
        }
        if let Some(result) = result {
            self.pending = None;
            match result {
                ConnectResult::Connected(session) => {
                    let view = TerminalView::new(session);
                    // 新建远程标签页并激活（SFTP 就绪后挂载到该页）。
                    self.tabs
                        .push(TerminalTab::new(self.pending_label.clone(), view));
                    self.active_tab = self.tabs.len() - 1;
                    self.pending_tab = Some(self.active_tab);
                    self.toast = Some((format!("已连接到 {}", self.pending_label), false));
                    ctx.request_repaint();
                }
                ConnectResult::Failed(e) => {
                    log::error!("连接失败：{e}");
                    self.toast = Some((format!("连接失败：{e}"), true));
                    ctx.request_repaint();
                }
            }
        }
    }

    /// 发起远程连接（同时启动 SFTP 连接）。
    fn start_connect(&mut self, ctx: &egui::Context, profile: HostProfile) {
        let label = profile.name.clone();
        let ctx = ctx.clone();
        let on_event = Arc::new(move |_ev: &SessionEvent| {
            ctx.request_repaint();
        });
        let (_thread, rx) = connect_remote(&profile, 80, 24, on_event);
        self.pending = Some(rx);
        self.pending_label = label.clone();

        // 并行建立 SFTP 连接。
        let (_sftp_thread, sftp_handle, sftp_rx) = connect_sftp(&profile);
        self.sftp_host = label;
        self.pending_sftp = Some((sftp_handle, sftp_rx));
        self.pending_tab = None;
    }

    /// 处理 SFTP 连接结果。
    fn poll_sftp(&mut self) {
        let mut ready = false;
        let mut failed: Option<String> = None;
        if let Some((_handle, rx)) = &mut self.pending_sftp {
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    SftpEvent::Ready => ready = true,
                    SftpEvent::Failed(e) => failed = Some(e),
                    _ => {}
                }
            }
        }
        if ready {
            if let Some((handle, rx)) = self.pending_sftp.take() {
                // 挂载到连接成功后创建的标签页（用户可能已切换标签页）。
                if let Some(idx) = self.pending_tab {
                    if let Some(tab) = self.tabs.get_mut(idx) {
                        tab.sftp = Some(SftpView::new(&self.sftp_host, handle, rx));
                    }
                }
            }
        }
        if let Some(e) = failed {
            self.pending_sftp = None;
            self.toast = Some((format!("SFTP 连接失败：{e}"), true));
        }
    }

    /// 渲染顶部工具栏（左侧：主机列表折叠开关；右侧：主题切换 + 检查更新）。
    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            // 主机列表折叠开关（默认收起）。
            let theme = crate::theme::current_theme();
            let sidebar_btn =
                egui::Button::new(egui::RichText::new("◧").color(if self.sidebar_open {
                    theme.text_primary
                } else {
                    theme.text_muted
                }))
                .fill(if self.sidebar_open {
                    theme.accent_soft
                } else {
                    egui::Color32::TRANSPARENT
                })
                .stroke(egui::Stroke::NONE)
                .corner_radius(crate::theme::miro::RADIUS_ITEM);
            if ui
                .add(sidebar_btn)
                .on_hover_text("主机列表（⌘B）")
                .clicked()
            {
                self.sidebar_open = !self.sidebar_open;
            }
            ui.separator();
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // 手动检查更新。
                let update_btn = match &self.update_state {
                    UpdateState::Checking => "检查中…".to_string(),
                    UpdateState::Available(_) => "新版本可用".to_string(),
                    UpdateState::UpToDate => "已是最新".to_string(),
                    UpdateState::Failed => "检查失败".to_string(),
                    UpdateState::Idle => "检查更新".to_string(),
                };
                if ui.button(update_btn).clicked() {
                    self.start_update_check(false);
                }
                ui.separator();
                // 主题切换（四套皮肤循环）。
                let themes = &crate::theme::THEMES;
                let current = crate::theme::current_theme().name;
                egui::ComboBox::from_id_salt("theme_switcher")
                    .selected_text(current)
                    .show_ui(ui, |ui| {
                        for (i, theme) in themes.iter().enumerate() {
                            if ui
                                .selectable_label(
                                    crate::theme::current_theme().name == theme.name,
                                    theme.name,
                                )
                                .clicked()
                            {
                                crate::theme::set_theme(ui.ctx(), i);
                            }
                        }
                    });
            });
        });
    }

    /// 渲染左侧主机列表。
    fn host_sidebar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(2.0);
        if ui
            .add(
                egui::Button::new("新建连接")
                    .fill(crate::theme::current_theme().accent)
                    .stroke(egui::Stroke::NONE)
                    .corner_radius(crate::theme::miro::RADIUS_SM),
            )
            .clicked()
        {
            self.show_new_conn = true;
        }
        ui.add_space(2.0);
        ui.separator();

        if self.config.hosts.is_empty() {
            ui.label(
                egui::RichText::new("暂无已保存主机")
                    .color(crate::theme::current_theme().text_muted),
            );
        }
        let mut remove_idx: Option<usize> = None;
        let mut connect_idx: Option<usize> = None;
        let mut selected_host: Option<usize> = None;
        for (i, host) in self.config.hosts.iter().enumerate() {
            // 主机行：头像 + 名称 + 删除图标；整行可点击（单击选中，双击连接）。
            let row_id = egui::Id::new(("host_row", i));
            let theme = crate::theme::current_theme();
            // 行内容（头像 + 名称）。
            let inner = ui
                .scope_builder(egui::UiBuilder::new().id_salt(row_id), |ui| {
                    ui.horizontal(|ui| {
                        // 头像：accent 圆底 + 主机名首字符。
                        let avatar_size = 30.0;
                        let (avatar_rect, _) = ui.allocate_exact_size(
                            egui::vec2(avatar_size, avatar_size),
                            egui::Sense::hover(),
                        );
                        ui.painter().circle_filled(
                            avatar_rect.center(),
                            avatar_size / 2.0,
                            theme.accent,
                        );
                        let initial = host.name.chars().next().unwrap_or('?');
                        ui.painter().text(
                            avatar_rect.center(),
                            egui::Align2::CENTER_CENTER,
                            initial.to_string(),
                            egui::FontId::proportional(14.0),
                            egui::Color32::WHITE,
                        );
                        ui.add_space(6.0);
                        ui.vertical(|ui| {
                            ui.label(egui::RichText::new(&host.name).color(theme.text_primary));
                            ui.weak(format!("{}@{}", host.user, host.host));
                        });
                    });
                })
                .response;
            // 整行点击区（显式 id + rect：Response::interact 的响应链在
            // egui 0.36 下不可靠，点击无法命中，必须用 ui.interact 注册）。
            let row_response = ui.interact(inner.rect.expand(2.0), row_id, egui::Sense::click());
            // 删除图标（行右侧，最后注册保证可点）。
            let del_rect = egui::Rect::from_min_size(
                egui::pos2(inner.rect.right() - 26.0, inner.rect.top()),
                egui::vec2(24.0, inner.rect.height()),
            );
            let del_resp = ui.interact(del_rect, row_id.with("del"), egui::Sense::click());
            if del_resp.hovered() {
                ui.painter().rect_filled(
                    del_rect,
                    crate::theme::miro::RADIUS_ITEM,
                    egui::Color32::from_rgba_unmultiplied_const(0xff, 0x55, 0x55, 0x26),
                );
            }
            ui.painter().text(
                del_rect.center(),
                egui::Align2::CENTER_CENTER,
                "🗑",
                egui::FontId::proportional(14.0),
                theme.text_muted,
            );
            if del_resp.clicked() {
                remove_idx = Some(i);
            }
            if row_response.clicked() {
                selected_host = Some(i);
                // 自实现双击检测（egui 多击计数会被无关点击污染）：
                // 0.3s 内再次点击同一行 → 连接。
                let now = ui.input(|i| i.time);
                if let Some((t, idx)) = self.last_row_click {
                    if idx == i && now - t < 0.3 {
                        connect_idx = Some(i);
                    }
                }
                self.last_row_click = Some((now, i));
            }
            // 选中/hover 行：accent-soft 底 + 圆角；选中时左侧 2px accent 指示条。
            if selected_host == Some(i) || row_response.hovered() {
                let rect = row_response.rect.expand(2.0);
                ui.painter().rect_filled(
                    rect,
                    crate::theme::miro::RADIUS_ITEM,
                    crate::theme::current_theme().accent_soft,
                );
                if selected_host == Some(i) {
                    ui.painter().rect_filled(
                        egui::Rect::from_min_max(
                            egui::pos2(rect.left(), rect.top() + 3.0),
                            egui::pos2(rect.left() + 2.0, rect.bottom() - 3.0),
                        ),
                        2.0,
                        crate::theme::current_theme().accent,
                    );
                }
            }
        }
        if let Some(i) = connect_idx {
            let profile = self.config.hosts[i].clone();
            self.start_connect(ui.ctx(), profile);
        }
        if let Some(i) = remove_idx {
            self.config.hosts.remove(i);
            self.save_config();
        }
    }

    /// 渲染新建连接对话框。
    fn connect_dialog(&mut self, ctx: &egui::Context) {
        let mut open = self.show_new_conn;
        egui::Window::new("新建连接")
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .show(ctx, |ui| {
                egui::Grid::new("conn_form")
                    .num_columns(2)
                    .spacing([8.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("名称");
                        // 对话框打开时自动聚焦名称输入框。
                        let name_id = egui::Id::new("conn_form_name");
                        ui.add(egui::TextEdit::singleline(&mut self.form.name).id(name_id));
                        if !self.form.name_focused {
                            ui.memory_mut(|m| m.request_focus(name_id));
                            self.form.name_focused = true;
                        }
                        ui.end_row();
                        ui.label("主机");
                        ui.text_edit_singleline(&mut self.form.host);
                        ui.end_row();
                        ui.label("端口");
                        ui.text_edit_singleline(&mut self.form.port);
                        ui.end_row();
                        ui.label("用户名");
                        ui.text_edit_singleline(&mut self.form.user);
                        ui.end_row();
                        ui.label("认证方式");
                        ui.horizontal(|ui| {
                            ui.selectable_value(&mut self.form.auth_kind, 0, "密码");
                            ui.selectable_value(&mut self.form.auth_kind, 1, "私钥");
                        });
                        ui.end_row();
                        if self.form.auth_kind == 0 {
                            ui.label("密码");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.form.password).password(true),
                            );
                            ui.end_row();
                        } else {
                            ui.label("私钥路径");
                            ui.text_edit_singleline(&mut self.form.key_path);
                            ui.end_row();
                            ui.label("口令（可选）");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.form.passphrase)
                                    .password(true),
                            );
                            ui.end_row();
                        }
                    });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("连接").clicked() {
                        let port: u16 = self.form.port.trim().parse().unwrap_or(22);
                        let auth = if self.form.auth_kind == 0 {
                            Auth::Password(self.form.password.clone())
                        } else {
                            Auth::Key {
                                path: PathBuf::from(self.form.key_path.trim()),
                                passphrase: if self.form.passphrase.is_empty() {
                                    None
                                } else {
                                    Some(self.form.passphrase.clone())
                                },
                            }
                        };
                        let profile = HostProfile {
                            name: if self.form.name.trim().is_empty() {
                                self.form.host.clone()
                            } else {
                                self.form.name.trim().to_string()
                            },
                            host: self.form.host.trim().to_string(),
                            port,
                            user: self.form.user.trim().to_string(),
                            auth,
                        };
                        if !profile.host.is_empty() && !profile.user.is_empty() {
                            self.config.hosts.push(profile.clone());
                            self.save_config();
                            self.show_new_conn = false;
                            self.form.name_focused = false;
                            self.start_connect(ctx, profile);
                        } else {
                            self.toast = Some(("请填写主机与用户名".into(), true));
                        }
                    }
                    if ui.button("取消").clicked() {
                        self.show_new_conn = false;
                        self.form.name_focused = false;
                    }
                });
            });
        self.show_new_conn = open;
    }

    /// 渲染标签页栏（新建/切换/关闭）。
    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        let theme = crate::theme::current_theme();
        ui.horizontal(|ui| {
            ui.add_space(2.0);
            let mut switch_to: Option<usize> = None;
            let mut close_idx: Option<usize> = None;
            for (i, tab) in self.tabs.iter().enumerate() {
                // 每帧同步标签标题（会话标题可能变化）。
                let title = tab.title();
                let selected = i == self.active_tab;
                let row = ui
                    .scope_builder(egui::UiBuilder::new().id_salt(("tab", i)), |ui| {
                        ui.horizontal(|ui| {
                            // 标签主体：点击切换。
                            if ui
                                .selectable_label(
                                    selected,
                                    egui::RichText::new(&title).color(if selected {
                                        theme.text_primary
                                    } else {
                                        theme.text_muted
                                    }),
                                )
                                .clicked()
                            {
                                switch_to = Some(i);
                            }
                            // 关闭按钮。
                            if ui.small_button("×").on_hover_text("关闭标签页").clicked() {
                                close_idx = Some(i);
                            }
                        });
                    })
                    .response;
                // 未选中标签 hover 高亮。
                if !selected && row.hovered() {
                    ui.painter().rect_filled(
                        row.rect.expand(2.0),
                        crate::theme::miro::RADIUS_ITEM,
                        theme.accent_soft,
                    );
                }
            }
            // 新建本地终端标签。
            if ui
                .add(
                    egui::Button::new("＋")
                        .fill(egui::Color32::TRANSPARENT)
                        .stroke(egui::Stroke::NONE)
                        .corner_radius(crate::theme::miro::RADIUS_ITEM),
                )
                .on_hover_text("新建本地终端（⌘T）")
                .clicked()
            {
                self.new_local_tab(ui.ctx());
            }
            if let Some(i) = switch_to {
                self.active_tab = i;
            }
            if let Some(i) = close_idx {
                self.close_tab(i);
            }
        });
    }

    /// 渲染状态栏。
    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            if let Some(tab) = self.tabs.get(self.active_tab) {
                let session = tab.terminal.session();
                let title = session.title();
                ui.label(
                    egui::RichText::new(if title.is_empty() {
                        tab.label.clone()
                    } else {
                        title
                    })
                    .color(crate::theme::current_theme().text_secondary),
                );
                if session.has_exited() {
                    ui.colored_label(egui::Color32::from_rgb(0xff, 0x55, 0x55), "会话已退出");
                }
                if tab.sftp.is_some() {
                    ui.separator();
                    ui.colored_label(
                        egui::Color32::from_rgb(0x50, 0xfa, 0x7b),
                        format!("SFTP · {}", self.sftp_host),
                    );
                }
            }
            if self.pending_sftp.is_some() {
                ui.separator();
                ui.weak("SFTP 连接中…");
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.weak("⌘B 主机  ⌘T 新建终端  ⌘W 关闭  ⌘1-9 切换  ⌘N 连接  ⌥1-4 主题");
            });
        });
    }
}

impl eframe::App for KunApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // ==================== 快捷键 ====================
        // ⌘N 新建连接、⌘T 新建本地终端、⌘W 关闭标签、⌘1-9 切换标签、⌥1-4 切换主题。
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::N)) {
            self.show_new_conn = true;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::T)) {
            self.new_local_tab(&ctx);
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::B)) {
            self.sidebar_open = !self.sidebar_open;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::W))
            && !self.tabs.is_empty()
        {
            self.close_tab(self.active_tab);
        }
        for (key, theme_idx) in [
            (egui::Key::Num1, 0),
            (egui::Key::Num2, 1),
            (egui::Key::Num3, 2),
            (egui::Key::Num4, 3),
        ] {
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::ALT, key)) {
                crate::theme::set_theme(&ctx, theme_idx);
                self.toast = Some((
                    format!("主题：{}", crate::theme::current_theme().name),
                    false,
                ));
            }
        }
        // ⌘1-9 切换到对应标签页。
        for (key, idx) in [
            (egui::Key::Num1, 0usize),
            (egui::Key::Num2, 1),
            (egui::Key::Num3, 2),
            (egui::Key::Num4, 3),
            (egui::Key::Num5, 4),
            (egui::Key::Num6, 5),
            (egui::Key::Num7, 6),
            (egui::Key::Num8, 7),
            (egui::Key::Num9, 8),
        ] {
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, key))
                && idx < self.tabs.len()
            {
                self.active_tab = idx;
            }
        }

        // ==================== 处理连接结果 ====================
        self.poll_connection(&ctx);
        self.poll_sftp();
        self.poll_update();

        // ==================== 左侧主机栏（可折叠，默认收起） ====================
        if self.sidebar_open {
            let sidebar_frame = egui::Frame::new()
                .fill(crate::theme::current_theme().bg_panel)
                .inner_margin(egui::Margin::symmetric(10, 10))
                .stroke(egui::Stroke::new(1.0, crate::theme::current_theme().border));
            egui::Panel::left("hosts")
                .default_size(220.0)
                .resizable(true)
                .frame(sidebar_frame)
                .show(ui, |ui| {
                    self.host_sidebar(ui);
                });
        }

        // ==================== 顶部工具栏 ====================
        let toolbar_frame = egui::Frame::new()
            .fill(crate::theme::current_theme().bg_header)
            .inner_margin(egui::Margin::symmetric(10, 6))
            .stroke(egui::Stroke::new(1.0, crate::theme::current_theme().border));
        egui::Panel::top("toolbar")
            .frame(toolbar_frame)
            .show(ui, |ui| {
                self.toolbar(ui);
            });

        // ==================== 标签页栏 ====================
        let tab_frame = egui::Frame::new()
            .fill(crate::theme::current_theme().bg_header)
            .inner_margin(egui::Margin::symmetric(10, 3))
            .stroke(egui::Stroke::new(1.0, crate::theme::current_theme().border));
        egui::Panel::top("tabs").frame(tab_frame).show(ui, |ui| {
            self.tab_bar(ui);
        });

        // ==================== 状态栏 ====================
        let status_frame = egui::Frame::new()
            .fill(crate::theme::current_theme().bg_panel)
            .inner_margin(egui::Margin::symmetric(10, 4))
            .stroke(egui::Stroke::new(1.0, crate::theme::current_theme().border));
        egui::Panel::bottom("status")
            .frame(status_frame)
            .show(ui, |ui| {
                self.status_bar(ui);
            });

        // ==================== 中央区：当前标签页（终端 | SFTP 分栏） ====================
        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(_pending) = &self.pending {
                ui.centered_and_justified(|ui| {
                    ui.spinner();
                    ui.label(format!("正在连接 {} …", self.pending_label));
                });
            } else if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                // 该标签页存在 SFTP 面板时水平分栏（终端左，SFTP 右，可拖拽）。
                if let Some(sftp) = &mut tab.sftp {
                    let sftp_frame = egui::Frame::new()
                        .fill(crate::theme::current_theme().bg_panel)
                        .inner_margin(egui::Margin::symmetric(10, 8))
                        .stroke(egui::Stroke::new(1.0, crate::theme::current_theme().border));
                    egui::Panel::right("sftp_panel")
                        .default_size(360.0)
                        .resizable(true)
                        .frame(sftp_frame)
                        .show(ui, |ui| {
                            sftp.show(ui);
                        });
                }
                tab.terminal.show(ui);
            } else {
                ui.centered_and_justified(|ui| {
                    if ui.button("新建本地终端").clicked() {
                        self.new_local_tab(&ctx);
                    }
                });
            }
        });

        // ==================== SFTP 对话框（当前标签页） ====================
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            if let Some(sftp) = &mut tab.sftp {
                sftp.show_dialog(&ctx);
            }
        }

        // ==================== 新建连接对话框 ====================
        self.connect_dialog(&ctx);

        // ==================== 更新提示弹窗 ====================
        if let UpdateState::Available(info) = &self.update_state {
            let mut dismiss = false;
            egui::Window::new("发现新版本")
                .collapsible(false)
                .resizable(false)
                .show(&ctx, |ui| {
                    ui.label(
                        egui::RichText::new(format!(
                            "kun {} 已发布（当前 v{}）",
                            info.version,
                            env!("CARGO_PKG_VERSION")
                        ))
                        .strong(),
                    );
                    ui.add_space(6.0);
                    if !info.notes.is_empty() {
                        let preview: String = info.notes.chars().take(400).collect();
                        ui.label(
                            egui::RichText::new(preview)
                                .color(crate::theme::current_theme().text_secondary),
                        );
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add(
                                egui::Button::new("前往下载")
                                    .fill(crate::theme::current_theme().accent)
                                    .stroke(egui::Stroke::NONE),
                            )
                            .clicked()
                        {
                            ctx.open_url(egui::OpenUrl::same_tab(info.url.clone()));
                            dismiss = true;
                        }
                        if ui.button("稍后").clicked() {
                            dismiss = true;
                        }
                    });
                });
            if dismiss {
                self.update_state = UpdateState::Idle;
            }
        }

        // ==================== 提示消息 ====================
        if let Some((message, is_error)) = &self.toast {
            let color = if *is_error {
                egui::Color32::from_rgb(0xff, 0x55, 0x55)
            } else {
                egui::Color32::from_rgb(0x50, 0xfa, 0x7b)
            };
            let mut dismiss = false;
            let mut confirmed = false;
            egui::Window::new("提示")
                .open(&mut dismiss)
                .collapsible(false)
                .resizable(false)
                .show(&ctx, |ui| {
                    ui.colored_label(color, message);
                    ui.add_space(4.0);
                    if ui.button("确定").clicked() {
                        confirmed = true;
                    }
                });
            if confirmed || !dismiss {
                self.toast = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use eframe::egui;
    use egui_kittest::Harness;
    use kittest::Queryable;

    /// 验证侧栏、工具栏与中央面板在 root Ui 上正常渲染。
    #[test]
    fn 面板渲染完整() {
        let mut harness = Harness::new_ui(|ui| {
            egui::Panel::left("hosts")
                .default_size(220.0)
                .resizable(true)
                .show(ui, |ui| {
                    ui.heading("主机");
                    let _ = ui.button("新建连接");
                    ui.weak("暂无已保存主机");
                });
            egui::Panel::top("toolbar").show(ui, |ui| {
                ui.label(egui::RichText::new("kun").strong());
                let _ = ui.button("本地终端");
            });
            egui::Panel::bottom("status").show(ui, |ui| {
                ui.label("状态栏");
            });
            egui::CentralPanel::default().show(ui, |ui| {
                ui.label("终端区域");
            });
        });
        harness.run();

        harness.get_by_label("主机");
        harness.get_by_label("新建连接");
        harness.get_by_label("暂无已保存主机");
        harness.get_by_label("kun");
        harness.get_by_label("本地终端");
        harness.get_by_label("状态栏");
        harness.get_by_label("终端区域");
    }
}

#[cfg(test)]
mod dialog_tests {
    use super::*;

    /// 完整渲染"新建连接"对话框（含密码/私钥切换、自动聚焦），不应崩溃。
    #[test]
    fn 新建连接对话框完整渲染() {
        use kittest::Queryable;
        let mut form = ConnectForm {
            name: "测试".into(),
            host: "127.0.0.1".into(),
            port: "22".into(),
            user: "root".into(),
            auth_kind: 0,
            password: String::new(),
            key_path: String::new(),
            passphrase: String::new(),
            name_focused: false,
        };
        let mut show = true;
        let mut harness = egui_kittest::Harness::new_ui(|ui| {
            let mut open = show;
            egui::Window::new("新建连接")
                .open(&mut open)
                .resizable(false)
                .collapsible(false)
                .show(ui, |ui| {
                    egui::Grid::new("conn_form")
                        .num_columns(2)
                        .spacing([8.0, 6.0])
                        .show(ui, |ui| {
                            ui.label("名称");
                            let name_id = egui::Id::new("conn_form_name");
                            ui.add(egui::TextEdit::singleline(&mut form.name).id(name_id));
                            if !form.name_focused {
                                ui.memory_mut(|m| m.request_focus(name_id));
                                form.name_focused = true;
                            }
                            ui.end_row();
                            ui.label("主机");
                            ui.text_edit_singleline(&mut form.host);
                            ui.end_row();
                            ui.label("端口");
                            ui.text_edit_singleline(&mut form.port);
                            ui.end_row();
                            ui.label("用户名");
                            ui.text_edit_singleline(&mut form.user);
                            ui.end_row();
                            ui.label("认证方式");
                            ui.horizontal(|ui| {
                                ui.selectable_value(&mut form.auth_kind, 0, "密码");
                                ui.selectable_value(&mut form.auth_kind, 1, "私钥");
                            });
                            ui.end_row();
                            if form.auth_kind == 0 {
                                ui.label("密码");
                                ui.add(
                                    egui::TextEdit::singleline(&mut form.password).password(true),
                                );
                                ui.end_row();
                            } else {
                                ui.label("私钥路径");
                                ui.text_edit_singleline(&mut form.key_path);
                                ui.end_row();
                                ui.label("口令（可选）");
                                ui.add(
                                    egui::TextEdit::singleline(&mut form.passphrase).password(true),
                                );
                                ui.end_row();
                            }
                        });
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        let _ = ui.button("连接");
                        let _ = ui.button("取消");
                    });
                });
            show = open;
        });
        harness.run();

        // 渲染完成，切换到私钥模式再渲染一帧。
        harness.get_by_label("私钥").click();
        harness.run();
        harness.get_by_label("私钥路径");
    }
}

#[cfg(test)]
mod app_tests {
    use super::*;

    /// 完整应用：点击"新建连接"按钮打开对话框，不应崩溃。
    /// （回归测试：用户报告点击新建连接直接闪退）
    #[test]
    fn 点击新建连接不崩溃() {
        use kittest::Queryable;

        let mut harness = egui_kittest::Harness::new_eframe(|cc| KunApp::new(cc));
        harness.run();
        // 展开侧栏（默认折叠）。
        harness.get_by_label("◧").click();
        for _ in 0..3 {
            harness.step();
        }

        // 点击侧栏"新建连接"按钮（工具栏按钮已移除，侧栏为唯一入口）。
        harness.get_by_label("新建连接").click();
        // 终端持续请求重绘（光标闪烁），用 step 代替 run。
        for _ in 0..8 {
            harness.step();
        }

        // 对话框应出现且可交互（多个同名节点时取任意一个）。
        assert!(
            harness.query_all_by_label("名称").next().is_some(),
            "名称字段缺失"
        );
        assert!(
            harness.query_all_by_label("端口").next().is_some(),
            "端口字段缺失"
        );
        assert!(
            harness.query_all_by_label("用户名").next().is_some(),
            "用户名字段缺失"
        );
        assert!(
            harness.query_all_by_label("认证方式").next().is_some(),
            "认证方式缺失"
        );
        assert!(
            harness.query_all_by_label("连接").next().is_some(),
            "连接按钮缺失"
        );
        assert!(
            harness.query_all_by_label("取消").next().is_some(),
            "取消按钮缺失"
        );
    }
}

#[cfg(test)]
mod connect_tests {
    use super::*;
    use kun_core::config::Auth;

    /// 测试 sshd 是否可达（127.0.0.1:2222）；不可达时跳过网络测试。
    fn sshd_available() -> bool {
        use std::net::TcpStream;
        use std::time::Duration;
        TcpStream::connect_timeout(
            &"127.0.0.1:2222".parse().unwrap(),
            Duration::from_millis(500),
        )
        .is_ok()
    }

    /// 测试主机（需本地测试 sshd 运行，见 scripts/test-sshd.sh）。
    fn test_profile() -> HostProfile {
        let key_path = std::env::var("KUN_TEST_KEY").unwrap_or_else(|_| {
            format!(
                "{}/.ssh/id_ed25519",
                std::env::var("HOME").unwrap_or_default()
            )
        });
        HostProfile {
            name: "链路测试".into(),
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

    /// 端到端：点击侧栏"连接"按钮 → 远程终端 + SFTP 面板出现。
    /// （回归测试：用户报告点击后崩溃闪退，此前为 ui.input 闭包内 request_repaint 死锁）
    #[test]
    fn 点击连接建立远程会话() {
        use kittest::Queryable;
        use std::time::{Duration, Instant};

        if !sshd_available() {
            eprintln!("跳过：测试 sshd 未运行（scripts/test-sshd.sh start）");
            return;
        }

        // 预写主机配置（测试后清理）。
        let profile = test_profile();
        if let Auth::Key { path, .. } = &profile.auth {
            if !path.exists() {
                eprintln!("跳过：测试私钥不存在");
                return;
            }
        }
        let config_path = default_config_path();
        let config = HostConfig {
            hosts: vec![profile],
        };
        config.save(&config_path).expect("写入测试配置失败");

        // 步长 1/60s：两次点击间隔需小于双击检测窗口（0.3s），默认 step_dt=0.25s 会超时。
        let mut harness = egui_kittest::Harness::builder()
            .with_step_dt(1.0 / 60.0)
            .build_eframe(|cc| KunApp::new(cc));
        harness.run();
        // 展开侧栏（默认折叠）。
        harness.get_by_label("◧").click();
        for _ in 0..3 {
            harness.step();
        }

        // 双击主机条目触发连接（单击选中，双击连接；两次点击间需跨帧）。
        {
            let host_row = harness.get_by_label("链路测试");
            host_row.click();
        }
        harness.step();
        {
            let host_row = harness.get_by_label("链路测试");
            host_row.click();
        }

        // 轮询等待 SFTP 面板出现（连接完成）。
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut connected = false;
        while Instant::now() < deadline {
            for _ in 0..5 {
                harness.step();
            }
            if harness
                .root()
                .query_all_by_label("SFTP · 链路测试")
                .next()
                .is_some()
            {
                connected = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        // 清理测试配置。
        std::fs::remove_file(&config_path).ok();

        assert!(connected, "点击连接后未出现 SFTP 面板（连接失败或崩溃）");
        // 远程终端 + SFTP 面板并存。
        harness.get_by_label("上传");
        harness.get_by_label("刷新");
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    /// 测试 sshd 是否可达（127.0.0.1:2222）；不可达时跳过网络测试。
    fn sshd_available() -> bool {
        use std::net::TcpStream;
        use std::time::Duration;
        TcpStream::connect_timeout(
            &"127.0.0.1:2222".parse().unwrap(),
            Duration::from_millis(500),
        )
        .is_ok()
    }

    /// 连接测试 sshd 后渲染应用界面并保存截图（供视觉验证）。
    #[test]
    fn 生成连接后样式截图() {
        use kittest::Queryable;
        use kun_core::config::Auth;
        use std::time::{Duration, Instant};

        if !sshd_available() {
            eprintln!("跳过：测试 sshd 未运行");
            return;
        }

        let key_path = std::env::var("KUN_TEST_KEY").unwrap_or_else(|_| {
            format!(
                "{}/.ssh/id_ed25519",
                std::env::var("HOME").unwrap_or_default()
            )
        });
        let profile = HostProfile {
            // 与连接链路测试同名，避免并发写 hosts.toml 相互覆盖。
            name: "链路测试".into(),
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
        };
        if let Auth::Key { path, .. } = &profile.auth {
            if !path.exists() {
                eprintln!("跳过：测试私钥不存在");
                return;
            }
        }
        let config_path = default_config_path();
        let config = HostConfig {
            hosts: vec![profile],
        };
        config.save(&config_path).expect("写入测试配置失败");

        // 步长 1/60s：保证双击检测窗口（0.3s）内完成两次点击。
        let mut harness = egui_kittest::Harness::builder()
            .with_step_dt(1.0 / 60.0)
            .build_eframe(|cc| KunApp::new(cc));
        harness.run();
        // 展开侧栏（默认折叠）。
        harness.get_by_label("◧").click();
        for _ in 0..3 {
            harness.step();
        }
        // 双击主机条目触发连接（两次点击间需跨帧）。
        {
            let host_row = harness.get_by_label("链路测试");
            host_row.click();
        }
        harness.step();
        {
            let host_row = harness.get_by_label("链路测试");
            host_row.click();
        }

        // 等待 SFTP 面板出现（并发测试可能竞争配置文件，失败时重写重试）。
        let mut connected = false;
        for _attempt in 0..3 {
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                for _ in 0..5 {
                    harness.step();
                }
                if harness
                    .root()
                    .query_all_by_label("SFTP · 链路测试")
                    .next()
                    .is_some()
                {
                    connected = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            if connected {
                break;
            }
            // 配置可能被并发测试删除，重新写入并重启应用。
            config.save(&config_path).ok();
            harness = egui_kittest::Harness::builder()
                .with_step_dt(1.0 / 60.0)
                .build_eframe(|cc| KunApp::new(cc));
            harness.run();
            // 展开侧栏（默认折叠）。
            harness.get_by_label("◧").click();
            for _ in 0..3 {
                harness.step();
            }
            // 双击主机条目触发连接（两次点击间需跨帧）。
            {
                let host_row = harness.get_by_label("链路测试");
                host_row.click();
            }
            harness.step();
            {
                let host_row = harness.get_by_label("链路测试");
                host_row.click();
            }
        }
        assert!(connected, "连接失败，无法生成截图");

        // 渲染并保存。
        let img = harness.render().expect("渲染失败");
        let out = "/tmp/kun_style_sftp.png";
        img.save(out).expect("保存截图失败");
        eprintln!("样式截图已保存：{out}");
    }
}

#[cfg(test)]
mod theme_tests {
    use super::*;

    /// 四套主题切换并渲染截图（视觉验证用）。
    #[test]
    fn 四套主题渲染截图() {
        use kittest::Queryable;

        let mut harness = egui_kittest::Harness::new_eframe(|cc| KunApp::new(cc));
        harness.run();

        // 逐套主题：通过 UI 下拉切换并渲染。
        for theme_name in ["Miro 深色", "Dawn 浅色", "Midnight 深蓝", "Cyberpunk 霓虹"] {
            // 点击主题下拉（ComboBox 节点），再选择目标主题。
            let combo = harness
                .root()
                .query_by_role(accesskit::Role::ComboBox)
                .expect("主题下拉不存在");
            combo.click();
            for _ in 0..2 {
                harness.step();
            }
            harness.get_by_label(theme_name).click();
            for _ in 0..3 {
                harness.step();
            }
            assert_eq!(
                crate::theme::current_theme().name,
                theme_name,
                "主题切换失败"
            );
            // 渲染保存截图。
            let img = harness.render().expect("渲染失败");
            let out = format!("/tmp/kun_theme_{}.png", theme_name);
            img.save(&out).expect("保存截图失败");
            eprintln!("已保存：{out}");
        }
    }
}

#[cfg(test)]
mod tab_tests {
    use super::*;

    /// 标签栏"＋"按钮应新建标签页（而非替换）：关闭按钮数量 1 → 2。
    #[test]
    fn 新建本地终端标签页() {
        use kittest::Queryable;

        let mut harness = egui_kittest::Harness::new_eframe(|cc| KunApp::new(cc));
        harness.run();
        assert_eq!(
            harness.query_all_by_label("×").count(),
            1,
            "初始应有一个标签页"
        );

        // 标签栏"＋"按钮（工具栏"本地终端"按钮已移除）。
        harness.get_by_label("＋").click();
        for _ in 0..4 {
            harness.step();
        }
        assert_eq!(
            harness.query_all_by_label("×").count(),
            2,
            "点击后应有 2 个标签页"
        );
    }

    /// ⌘T 新建标签页、⌘W 关闭当前标签页。
    #[test]
    fn 快捷键新建与关闭标签页() {
        use kittest::Queryable;

        let mut harness = egui_kittest::Harness::new_eframe(|cc| KunApp::new(cc));
        harness.run();

        // ⌘T 新建。
        harness.event(egui::Event::Key {
            key: egui::Key::T,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        for _ in 0..4 {
            harness.step();
        }
        assert_eq!(
            harness.query_all_by_label("×").count(),
            2,
            "⌘T 后应有 2 个标签页"
        );

        // ⌘W 关闭当前。
        harness.event(egui::Event::Key {
            key: egui::Key::W,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        for _ in 0..4 {
            harness.step();
        }
        assert_eq!(
            harness.query_all_by_label("×").count(),
            1,
            "⌘W 后应回到 1 个标签页"
        );
        // 全部关闭后不应崩溃。
        harness.event(egui::Event::Key {
            key: egui::Key::W,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        for _ in 0..4 {
            harness.step();
        }
        assert_eq!(
            harness.query_all_by_label("×").count(),
            0,
            "全部关闭后应无标签页"
        );
    }

    /// 本地会话默认工作目录为 home（Finder 启动 cwd=/ 时终端应落在 ~）。
    #[test]
    fn 本地会话默认home目录() {
        let opts = local_session_options();
        assert_eq!(
            opts.working_directory,
            std::env::var("HOME").ok().map(PathBuf::from),
            "本地会话应默认在 home 目录"
        );
    }
}

#[cfg(test)]
mod dblclick_probe {
    use eframe::egui;

    /// 最小复现：kittest 两次 click 是否触发 double_clicked。
    #[test]
    fn 双击检测探针() {
        use kittest::Queryable;

        let mut harness = egui_kittest::Harness::builder()
            .with_step_dt(1.0 / 60.0)
            .build_ui(|ui| {
                let btn = ui.button("目标");
                if btn.double_clicked() {
                    ui.label("双击了");
                }
            });
        harness.step();
        {
            let b = harness.get_by_label("目标");
            b.click();
        }
        harness.step();
        {
            let b = harness.get_by_label("目标");
            b.click();
        }
        harness.step();
        assert!(
            harness.root().query_all_by_label("双击了").next().is_some(),
            "双击未触发"
        );
    }

    /// 方案 G：显式 ui.interact(rect, id) 的单击/双击。
    #[test]
    fn 显式interact探针() {
        use kittest::Queryable;

        let mut harness = egui_kittest::Harness::builder()
            .with_step_dt(1.0 / 60.0)
            .build_ui(|ui| {
                let inner = ui.scope_builder(egui::UiBuilder::new().id_salt("row"), |ui| {
                    ui.horizontal(|ui| {
                        ui.label("行内标签");
                        let _ = ui.button("其他");
                    });
                });
                let row = ui.interact(
                    inner.response.rect,
                    egui::Id::new("row"),
                    egui::Sense::click(),
                );
                if row.clicked() {
                    ui.label("单击了");
                }
                if row.double_clicked() {
                    ui.label("双击了");
                }
            });
        harness.step();
        {
            let b = harness.get_by_label("行内标签");
            b.click();
        }
        harness.step();
        {
            let b = harness.get_by_label("行内标签");
            b.click();
        }
        harness.step();
        assert!(
            harness.root().query_all_by_label("双击了").next().is_some(),
            "显式 interact 双击未触发"
        );
    }
}

#[cfg(test)]
mod sidebar_tests {
    use super::*;

    /// 侧栏默认折叠（"主机"标题不可见），点 ◧ 展开，再点收起。
    #[test]
    fn 侧栏默认折叠可展开收起() {
        use kittest::Queryable;

        let mut harness = egui_kittest::Harness::new_eframe(|cc| KunApp::new(cc));
        harness.run();

        // 默认折叠：无"新建连接"侧栏按钮。
        assert!(
            harness
                .root()
                .query_all_by_label("新建连接")
                .next()
                .is_none(),
            "侧栏默认应折叠（新建连接按钮不可见）"
        );

        // 点击 ◧ 展开。
        harness.get_by_label("◧").click();
        for _ in 0..3 {
            harness.step();
        }
        assert!(
            harness
                .root()
                .query_all_by_label("新建连接")
                .next()
                .is_some(),
            "展开后新建连接按钮应可见"
        );
        harness.get_by_label("新建连接");

        // 再点 ◧ 收起。
        harness.get_by_label("◧").click();
        for _ in 0..3 {
            harness.step();
        }
        assert!(
            harness
                .root()
                .query_all_by_label("新建连接")
                .next()
                .is_none(),
            "收起后新建连接按钮应不可见"
        );
    }

    /// ⌘B 切换侧栏折叠。
    #[test]
    fn 快捷键切换侧栏() {
        use kittest::Queryable;

        let mut harness = egui_kittest::Harness::new_eframe(|cc| KunApp::new(cc));
        harness.run();
        assert!(
            harness
                .root()
                .query_all_by_label("新建连接")
                .next()
                .is_none(),
            "默认折叠"
        );

        harness.event(egui::Event::Key {
            key: egui::Key::B,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        for _ in 0..3 {
            harness.step();
        }
        assert!(
            harness
                .root()
                .query_all_by_label("新建连接")
                .next()
                .is_some(),
            "⌘B 应展开侧栏"
        );
    }
}
