//! kun 应用主体：布局与状态管理。

use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui;
use kun_core::config::{default_config_path, Auth, HostConfig, HostProfile};
use kun_core::ssh::{connect_remote, ConnectResult};
use kun_core::terminal::{Session, SessionEvent, SessionOptions};
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

/// 应用状态。
pub struct KunApp {
    /// 当前活动终端视图（本地或远程）。
    terminal: Option<TerminalView>,
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
    /// SFTP 面板（远程连接时存在）。
    sftp: Option<SftpView>,
    /// 进行中的 SFTP 连接。
    pending_sftp: Option<(SftpHandle, UnboundedReceiver<SftpEvent>)>,
    /// 远程连接的主机名（用于 SFTP 面板标题）。
    sftp_host: String,
}

impl KunApp {
    /// 创建应用（启动本地终端会话）。
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        crate::theme::set_theme(&cc.egui_ctx, 0);
        let ctx = cc.egui_ctx.clone();

        // 加载主机配置。
        let config_path = default_config_path();
        let config = HostConfig::load(&config_path).unwrap_or_default();

        // 启动本地终端会话。
        let on_event = Arc::new(move |_ev: &SessionEvent| {
            ctx.request_repaint();
        });
        let terminal = match Session::spawn_local(SessionOptions::default(), 80, 24, on_event) {
            Ok(session) => Some(TerminalView::new(session)),
            Err(e) => {
                log::error!("启动本地终端失败：{e}");
                None
            }
        };

        Self {
            terminal,
            config,
            config_path,
            show_new_conn: false,
            form: ConnectForm::default(),
            pending: None,
            pending_label: String::new(),
            toast: None,
            sftp: None,
            pending_sftp: None,
            sftp_host: String::new(),
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
                    self.terminal = Some(view);
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
        self.sftp = None;
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
                self.sftp = Some(SftpView::new(&self.sftp_host, handle, rx));
            }
        }
        if let Some(e) = failed {
            self.pending_sftp = None;
            self.toast = Some((format!("SFTP 连接失败：{e}"), true));
        }
    }

    /// 渲染顶部工具栏。
    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new("kun")
                    .strong()
                    .color(crate::theme::current_theme().accent),
            );
            ui.separator();
            let new_conn_btn = egui::Button::new("新建连接")
                .fill(crate::theme::current_theme().accent)
                .stroke(egui::Stroke::NONE)
                .corner_radius(crate::theme::miro::RADIUS_SM);
            if ui.add(new_conn_btn).clicked() {
                self.show_new_conn = true;
            }
            if ui.button("本地终端").clicked() {
                // 重新挂载本地终端（若当前是远程会话则替换）。
                let ctx = ui.ctx().clone();
                let on_event = Arc::new(move |_ev: &SessionEvent| {
                    ctx.request_repaint();
                });
                if let Ok(session) =
                    Session::spawn_local(SessionOptions::default(), 80, 24, on_event)
                {
                    self.terminal = Some(TerminalView::new(session));
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
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
        ui.label(
            egui::RichText::new("主机")
                .size(13.0)
                .color(crate::theme::current_theme().text_secondary)
                .strong(),
        );
        ui.add_space(4.0);
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
            // 主机行：整行可点击（单击选中，双击连接），hover 显示 accent-soft 底。
            let row_id = egui::Id::new(("host_row", i));
            let row_response = ui
                .scope_builder(egui::UiBuilder::new().id_salt(row_id), |ui| {
                    ui.horizontal(|ui| {
                        if ui.button("连接").on_hover_text("连接到该主机").clicked() {
                            connect_idx = Some(i);
                        }
                        ui.vertical(|ui| {
                            ui.label(
                                egui::RichText::new(&host.name)
                                    .color(crate::theme::current_theme().text_primary),
                            );
                            ui.weak(format!("{}@{}", host.user, host.host));
                        });
                        if ui.button("删除").on_hover_text("删除该主机").clicked() {
                            remove_idx = Some(i);
                        }
                    });
                })
                .response
                .interact(egui::Sense::click());
            if row_response.clicked() {
                selected_host = Some(i);
            }
            // 双击条目直接连接（常见交互，也便于鼠标操作）。
            if row_response.double_clicked() {
                connect_idx = Some(i);
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

    /// 渲染状态栏。
    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            if let Some(terminal) = &self.terminal {
                let session = terminal.session();
                let title = session.title();
                ui.label(
                    egui::RichText::new(if title.is_empty() {
                        "本地终端".to_string()
                    } else {
                        title
                    })
                    .color(crate::theme::current_theme().text_secondary),
                );
                if session.has_exited() {
                    ui.colored_label(egui::Color32::from_rgb(0xff, 0x55, 0x55), "会话已退出");
                }
            }
            if self.sftp.is_some() {
                ui.separator();
                ui.colored_label(
                    egui::Color32::from_rgb(0x50, 0xfa, 0x7b),
                    format!("SFTP · {}", self.sftp_host),
                );
            } else if self.pending_sftp.is_some() {
                ui.separator();
                ui.weak("SFTP 连接中…");
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.weak("⌘N 新建连接  ⌘1 本地终端");
            });
        });
    }
}

impl eframe::App for KunApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // ==================== 快捷键 ====================
        // ⌘N 新建连接、⌘1 本地终端、⌥1-4 切换主题。
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::N)) {
            self.show_new_conn = true;
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
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Num1)) {
            // 切换本地终端。
            let repaint_ctx = ctx.clone();
            let on_event = Arc::new(move |_ev: &SessionEvent| {
                repaint_ctx.request_repaint();
            });
            if let Ok(session) = Session::spawn_local(SessionOptions::default(), 80, 24, on_event) {
                self.terminal = Some(TerminalView::new(session));
                self.sftp = None;
            }
        }

        // ==================== 处理连接结果 ====================
        self.poll_connection(&ctx);
        self.poll_sftp();

        // ==================== 左侧主机栏 ====================
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

        // ==================== 中央区：终端 | SFTP 分栏 ====================
        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(_pending) = &self.pending {
                ui.centered_and_justified(|ui| {
                    ui.spinner();
                    ui.label(format!("正在连接 {} …", self.pending_label));
                });
            } else if let Some(terminal) = &mut self.terminal {
                // SFTP 面板存在时水平分栏（终端左，SFTP 右，可拖拽）。
                if self.sftp.is_some() {
                    let sftp_frame = egui::Frame::new()
                        .fill(crate::theme::current_theme().bg_panel)
                        .inner_margin(egui::Margin::symmetric(10, 8))
                        .stroke(egui::Stroke::new(1.0, crate::theme::current_theme().border));
                    egui::Panel::right("sftp_panel")
                        .default_size(360.0)
                        .resizable(true)
                        .frame(sftp_frame)
                        .show(ui, |ui| {
                            if let Some(sftp) = &mut self.sftp {
                                sftp.show(ui);
                            }
                        });
                }
                terminal.show(ui);
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label("本地终端启动失败，请查看日志。");
                });
            }
        });

        // ==================== SFTP 对话框 ====================
        if let Some(sftp) = &mut self.sftp {
            sftp.show_dialog(&ctx);
        }

        // ==================== 新建连接对话框 ====================
        self.connect_dialog(&ctx);

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

        // 点击工具栏"新建连接"按钮（工具栏与侧栏各有一个同名按钮，取第一个）。
        let buttons: Vec<_> = harness.query_all_by_label("新建连接").collect();
        assert!(buttons.len() >= 2, "工具栏与侧栏应各有一个新建连接按钮");
        buttons[0].click();
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

        let mut harness = egui_kittest::Harness::new_eframe(|cc| KunApp::new(cc));
        harness.run();

        // 点击侧栏条目的"连接"按钮。
        harness.get_by_label("连接").click();

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

        let mut harness = egui_kittest::Harness::new_eframe(|cc| KunApp::new(cc));
        harness.run();
        harness.get_by_label("连接").click();

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
            harness = egui_kittest::Harness::new_eframe(|cc| KunApp::new(cc));
            harness.run();
            harness.get_by_label("连接").click();
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
