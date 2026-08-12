//! kun 应用主体：布局与状态管理。




use std::sync::Arc;

use eframe::egui;
use kun_core::terminal::{Session, SessionEvent, SessionOptions};
use crate::views::terminal_view::TerminalView;

/// 应用状态。
pub struct KunApp {
    /// 终端视图（M1 为本地会话，后续支持远程）。
    terminal: Option<TerminalView>,
    dark_mode: bool,
}

impl KunApp {
    /// 创建应用（启动本地终端会话）。
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        crate::theme::apply_dark(&cc.egui_ctx);
        let ctx = cc.egui_ctx.clone();

        // PTY 数据到达时请求重绘。
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

        Self { terminal, dark_mode: true }
    }
}

impl eframe::App for KunApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // ==================== 顶部工具栏 ====================
        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add_space(4.0);
                ui.label(egui::RichText::new("kun").strong());
                ui.separator();
                ui.label("本地终端");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let text = if self.dark_mode { "☀ 浅色" } else { "🌙 深色" };
                    if ui.button(text).clicked() {
                        self.dark_mode = !self.dark_mode;
                        if self.dark_mode {
                            crate::theme::apply_dark(&ctx);
                        } else {
                            ctx.set_visuals(egui::Visuals::light());
                        }
                    }
                });
            });
        });

        // ==================== 状态栏 ====================
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add_space(4.0);
                if let Some(terminal) = &self.terminal {
                    let session = terminal.session();
                    ui.label(session.title());
                    ui.separator();
                    if session.has_exited() {
                        ui.colored_label(egui::Color32::from_rgb(0xff, 0x55, 0x55), "会话已退出");
                    }
                }
            });
        });

        // ==================== 中央终端区 ====================
        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(terminal) = &mut self.terminal {
                terminal.show(ui);
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label("本地终端启动失败，请查看日志。");
                });
            }
        });
    }
}
