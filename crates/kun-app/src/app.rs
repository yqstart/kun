//! kun 应用主体：布局与状态管理。

use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui;
use kun_core::config::{Auth, HostConfig, HostProfile, default_config_path};
use kun_core::ssh::{ConnectResult, connect_remote};
use kun_core::terminal::{Session, SessionEvent, SessionOptions};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::views::terminal_view::TerminalView;

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
    dark_mode: bool,
}

impl KunApp {
    /// 创建应用（启动本地终端会话）。
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        crate::theme::apply_dark(&cc.egui_ctx);
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
            dark_mode: true,
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

    /// 发起远程连接。
    fn start_connect(&mut self, ctx: &egui::Context, profile: HostProfile) {
        let label = profile.name.clone();
        let ctx = ctx.clone();
        let on_event = Arc::new(move |_ev: &SessionEvent| {
            ctx.request_repaint();
        });
        let (_thread, rx) = connect_remote(&profile, 80, 24, on_event);
        self.pending = Some(rx);
        self.pending_label = label;
    }

    /// 渲染顶部工具栏。
    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            ui.label(egui::RichText::new("kun").strong());
            ui.separator();
            if ui.button("新建连接").clicked() {
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
                let text = if self.dark_mode { "浅色模式" } else { "深色模式" };
                if ui.button(text).clicked() {
                    self.dark_mode = !self.dark_mode;
                    if self.dark_mode {
                        crate::theme::apply_dark(ui.ctx());
                    } else {
                        ui.ctx().set_visuals(egui::Visuals::light());
                    }
                }
            });
        });
    }

    /// 渲染左侧主机列表。
    fn host_sidebar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.heading("主机");
        ui.add_space(2.0);
        if ui.button("新建连接").clicked() {
            self.show_new_conn = true;
        }
        ui.separator();

        if self.config.hosts.is_empty() {
            ui.weak("暂无已保存主机");
        }
        let mut remove_idx: Option<usize> = None;
        let mut connect_idx: Option<usize> = None;
        for (i, host) in self.config.hosts.iter().enumerate() {
            ui.horizontal(|ui| {
                if ui.button("连接").on_hover_text("连接到该主机").clicked() {
                    connect_idx = Some(i);
                }
                ui.vertical(|ui| {
                    ui.label(&host.name);
                    ui.weak(format!("{}@{}", host.user, host.host));
                });
                if ui.button("删除").on_hover_text("删除该主机").clicked() {
                    remove_idx = Some(i);
                }
            });
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
                        ui.text_edit_singleline(&mut self.form.name);
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
                            ui.add(egui::TextEdit::singleline(&mut self.form.password).password(true));
                            ui.end_row();
                        } else {
                            ui.label("私钥路径");
                            ui.text_edit_singleline(&mut self.form.key_path);
                            ui.end_row();
                            ui.label("口令（可选）");
                            ui.add(egui::TextEdit::singleline(&mut self.form.passphrase).password(true));
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
                            self.start_connect(ctx, profile);
                        } else {
                            self.toast = Some(("请填写主机与用户名".into(), true));
                        }
                    }
                    if ui.button("取消").clicked() {
                        self.show_new_conn = false;
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
                ui.label(if title.is_empty() { "本地终端".to_string() } else { title });
                ui.separator();
                if session.has_exited() {
                    ui.colored_label(egui::Color32::from_rgb(0xff, 0x55, 0x55), "会话已退出");
                }
            }
        });
    }
}

impl eframe::App for KunApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // ==================== 处理连接结果 ====================
        self.poll_connection(&ctx);

        // ==================== 左侧主机栏 ====================
        egui::Panel::left("hosts").default_size(220.0).resizable(true).show(ui, |ui| {
            self.host_sidebar(ui);
        });

        // ==================== 顶部工具栏 ====================
        egui::Panel::top("toolbar").show(ui, |ui| {
            self.toolbar(ui);
        });

        // ==================== 状态栏 ====================
        egui::Panel::bottom("status").show(ui, |ui| {
            self.status_bar(ui);
        });

        // ==================== 中央终端区 ====================
        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(_pending) = &self.pending {
                ui.centered_and_justified(|ui| {
                    ui.spinner();
                    ui.label(format!("正在连接 {} …", self.pending_label));
                });
            } else if let Some(terminal) = &mut self.terminal {
                terminal.show(ui);
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label("本地终端启动失败，请查看日志。");
                });
            }
        });

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
            egui::Panel::left("hosts").default_size(220.0).resizable(true).show(ui, |ui| {
                ui.heading("主机");
                ui.button("新建连接");
                ui.weak("暂无已保存主机");
            });
            egui::Panel::top("toolbar").show(ui, |ui| {
                ui.label(egui::RichText::new("kun").strong());
                ui.button("本地终端");
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
