//! kun 应用主体：布局、连接管理与状态。
//!
//! 视觉参照 Warp：分层深色背景、品牌紫青渐变、圆角幽灵按钮、
//! 标签页底部指示条与扫光动效（动效细节见 `crate::anim`）。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use eframe::egui;
use kun_core::config::{default_config_path, Auth, HostConfig, HostProfile};
use kun_core::ssh::sftp::{connect_sftp, SftpEvent, SftpHandle};
use kun_core::ssh::{connect_remote, ConnectResult};
use kun_core::terminal::{Session, SessionEvent, SessionOptions};
use kun_core::updater::{check_for_update, UpdateInfo};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::anim;
use crate::views::sftp_view::SftpView;
use crate::views::terminal_view::TerminalView;

/// 新建连接表单状态。
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

impl Default for ConnectForm {
    fn default() -> Self {
        Self {
            name: String::new(),
            host: String::new(),
            // 默认 root / 22 端口，可修改（大多数服务器默认入口）。
            port: "22".into(),
            user: "root".into(),
            auth_kind: 0,
            password: String::new(),
            key_path: String::new(),
            passphrase: String::new(),
            name_focused: false,
        }
    }
}

/// 更新下载事件（后台线程 → UI）。
enum DownloadEvent {
    Progress { downloaded: u64, total: Option<u64> },
    Done(PathBuf),
    Error(String),
}

/// 下载中的状态。
struct DownloadState {
    info: UpdateInfo,
    downloaded: u64,
    total: Option<u64>,
}

/// 更新状态机。
enum UpdateState {
    Idle,
    Checking,
    Available(UpdateInfo),
    UpToDate,
    Failed,
    Downloading(DownloadState),
    Downloaded { info: UpdateInfo, dmg_path: PathBuf },
    Installing(UpdateInfo),
    Installed,
    Error(String),
}

/// 更新弹窗内产生的用户动作。
enum UpdateAction {
    Dismiss,
    StartDownload(UpdateInfo),
    CancelDownload,
    Install { dmg_path: PathBuf },
    Retry,
}

/// 轻提示（非模态 Toast）。
struct Toast {
    message: String,
    is_error: bool,
    /// 首次渲染时记录时间戳（NAN 表示尚未记录）。
    start: f64,
}

/// 单个终端标签页（本地或远程会话）。
pub struct TerminalTab {
    label: String,
    terminal: TerminalView,
    sftp: Option<SftpView>,
    /// SFTP 面板是否展开（tabby 形式：默认收起，终端右上角悬浮按钮切换）。
    sftp_open: bool,
}

impl TerminalTab {
    fn new(label: String, terminal: TerminalView) -> Self {
        Self {
            label,
            terminal,
            sftp: None,
            sftp_open: false,
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

/// 标签页：普通终端（含本地/远程）。设置改为独立弹窗（`show_settings`），
/// 不再作为 tab。
///
/// `Vec<Box<TerminalTab>>` 用 Box 包裹：TerminalTab 体积大，
/// Box 避免 Vec 各槽位按最大元素对齐造成内存浪费
/// （clippy `large_enum_variant` 等价警告——`Tab` enum 之前也是用 Box）。
pub type Tab = Box<TerminalTab>;

/// 应用状态。
pub struct KunApp {
    tabs: Vec<Tab>,
    active_tab: usize,
    /// 连接成功后创建的标签页索引（用于挂载 SFTP）。
    pending_tab: Option<usize>,
    /// 主机行最近一次点击（时间, 行索引），自实现双击检测。
    last_row_click: Option<(f64, usize)>,
    /// 设置弹窗是否打开（`⌘,` 或齿轮按钮切换；Esc/× 关闭）。
    show_settings: bool,
    /// 最近一帧的 egui::Context（`new_local_tab` 等非 UI 闭包内构造时使用）。
    last_ctx: egui::Context,
    config: HostConfig,
    config_path: PathBuf,
    show_new_conn: bool,
    form: ConnectForm,
    pending: Option<UnboundedReceiver<ConnectResult>>,
    pending_label: String,
    toast: Option<Toast>,
    pending_sftp: Option<(SftpHandle, UnboundedReceiver<SftpEvent>)>,
    /// SFTP 已就绪但 SSH 标签尚未创建，等待挂载。
    ready_sftp: Option<(SftpHandle, UnboundedReceiver<SftpEvent>)>,
    /// SFTP 连接错误（状态栏持久显示，toast 易被忽略）。
    sftp_error: Option<String>,
    sftp_host: String,
    update_state: UpdateState,
    update_rx: Option<std::sync::mpsc::Receiver<Result<Option<UpdateInfo>, String>>>,
    download_rx: Option<std::sync::mpsc::Receiver<DownloadEvent>>,
    /// 本次检查是否为用户手动触发（决定是否弹提示）。
    manual_update: bool,
    /// 安装脚本已启动，到该时间点关闭应用重启。
    restart_at: Option<f64>,
}

/// 本地终端会话选项：默认工作目录为 home，注入 TERM 与颜色环境变量。
fn local_session_options() -> SessionOptions {
    SessionOptions {
        working_directory: std::env::var("HOME").ok().map(PathBuf::from),
        // TERM 必须显式注入：从 GUI/Finder/Dock 启动的进程继承 `TERM=dumb`，
        // alacritty 的 `setup_env()` 只在其主应用入口调用，kun 未调用 →
        // zsh 的 zle 判定非交互终端，删除回显走「原地空格覆盖」（删不掉+冒空格）、
        // 行编辑/补全/回车行为异常。注入 xterm-256color 恢复完整交互（同 Miro Code 修复）。
        // 注意：勿注入 locale（LANG/LC_ALL），会引发回车不执行（实测回归）。
        // macOS `ls` 默认不输出颜色（无 CLICOLOR 环境变量），文件/目录全白；
        // 注入后按 LSCOLORS 着色区分（与 Terminal.app/iTerm2 行为一致，不篡改 shell）。
        // LSCOLORS 为深色终端优化：目录=亮青、符号链接=紫红、可执行=红、
        // socket=绿、管道=黄、块/字符设备=蓝（默认底色）。
        env: [
            ("TERM".to_string(), "xterm-256color".to_string()),
            ("CLICOLOR".to_string(), "1".to_string()),
            ("LSCOLORS".to_string(), "Gxfxcxdxbxegedabagacad".to_string()),
        ]
        .into(),
        ..Default::default()
    }
}

/// 终端右上角悬浮 SFTP 开关按钮（tabby 风格；纯函数避免借用冲突，
/// 返回是否被点击）。打开时 accent 填充，关闭时浮层底 + 边框。
fn sftp_floating_button(ui: &mut egui::Ui, open: bool) -> bool {
    let theme = crate::theme::current_theme();
    let area = ui.max_rect();
    let btn_size = egui::vec2(60.0, 24.0);
    // 与终端内容内边距一致（PADDING=10），悬浮于终端区域右上角。
    let btn_rect = egui::Rect::from_min_size(
        egui::pos2(area.right() - 10.0 - btn_size.x, area.top() + 10.0),
        btn_size,
    );
    let (fill, stroke, fg) = if open {
        (theme.accent, egui::Stroke::NONE, egui::Color32::WHITE)
    } else {
        (
            theme.bg_elevated,
            egui::Stroke::new(1.0, theme.border),
            theme.text_secondary,
        )
    };
    let button = egui::Button::new(egui::RichText::new("SFTP").size(12.0).color(fg))
        .fill(fill)
        .stroke(stroke)
        .corner_radius(crate::theme::tokens::RADIUS_SM)
        .min_size(btn_size);
    ui.put(btn_rect, button)
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(if open {
            "关闭 SFTP 面板"
        } else {
            "打开 SFTP 面板"
        })
        .clicked()
}

/// 自绘 macOS traffic lights：关闭 eframe 标题栏后顶部左缘显示三个圆点
/// （红 / 黄 / 绿），与 Terminal.app/iTerm2 视觉一致。
/// 返回 `(clicked_idx, hover_idx)` —— clicked_idx = Some(0/1/2) 表示被点击，
/// 闭包外发 ViewportCommand 避免借用冲突。
fn draw_traffic_lights(ui: &mut egui::Ui) -> (Option<usize>, Option<usize>) {
    let r = 6.0;
    ui.add_space(10.0);
    let y = ui.cursor().center().y;
    let colors = [
        (egui::Color32::from_rgb(0xff, 0x5f, 0x57), "关闭"),
        (egui::Color32::from_rgb(0xfe, 0xbc, 0x2e), "最小化"),
        (egui::Color32::from_rgb(0x28, 0xc8, 0x40), "全屏"),
    ];
    let mut clicked = None;
    let mut hovered = None;
    for (i, (color, hover_text)) in colors.iter().enumerate() {
        let center = egui::pos2(ui.cursor().min.x + r, y);
        let rect = egui::Rect::from_center_size(center, egui::vec2(r * 2.0, r * 2.0));
        let resp = ui.allocate_rect(rect, egui::Sense::click());
        let hov = resp.hovered();
        let clk = resp.clicked();
        ui.painter().circle_filled(center, r, *color);
        if hov {
            hovered = Some(i);
            ui.painter().text(
                center,
                egui::Align2::CENTER_CENTER,
                match i {
                    0 => "×",
                    1 => "−",
                    _ => "⤢",
                },
                egui::FontId::proportional(8.5),
                egui::Color32::from_rgb(0x4d, 0x00, 0x00),
            );
            resp.on_hover_text(*hover_text);
        }
        if clk {
            clicked = Some(i);
        }
        ui.add_space(6.0);
    }
    ui.add_space(6.0);
    (clicked, hovered)
}

/// 标签栏最右侧齿轮图标（设置入口）。22×22 纯图标按钮，无文字。
/// 渐变圆底 + ⚙ 符号，hover accent2 辉光。
/// 包装为 `egui::Button` 以便被 kittest 通过 `Role::Button` 找到。
/// 点击调用方负责打开设置弹窗。
fn settings_gear_button(ui: &mut egui::Ui) -> bool {
    let theme = crate::theme::current_theme();
    let btn_size = 22.0;
    // 无文字 Button（透明 fill 覆盖默认背景）——kittest 通过 Role::Button 找到此控件。
    let btn = egui::Button::new("")
        .fill(egui::Color32::TRANSPARENT)
        .stroke(egui::Stroke::NONE)
        .min_size(egui::vec2(btn_size, btn_size));
    let response = ui
        .add(btn)
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("设置（⌘,）");
    // 自绘渐变圆底 + ⚙ 符号（在 Button rect 上覆盖）。
    let rect = response.rect;
    if ui.is_rect_visible(rect) {
        anim::paint_rounded_gradient(
            ui.painter(),
            rect,
            btn_size * 0.5,
            theme.accent,
            theme.accent2,
        );
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "⚙",
            egui::FontId::proportional(12.0),
            egui::Color32::WHITE,
        );
        if response.hovered() {
            anim::paint_glow(ui.painter(), rect.center(), btn_size * 0.9, theme.accent2);
        }
    }
    response.clicked()
}

/// 当前 macOS 架构 → 发布产物命名（release.yml 约定）。
fn macos_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        other => other,
    }
}

/// 下载缓存目录下的 dmg 路径。
fn temp_dmg_path(file_name: &str) -> PathBuf {
    std::env::temp_dir().join("kun-update").join(file_name)
}

/// 安装脚本：挂载 dmg → 等待主程序退出 → 替换 .app → 重启。
/// 优先安装到 /Applications，失败回退 ~/Applications。
const INSTALL_SCRIPT: &str = r#"#!/bin/sh
set -u
DMG="$1"
MOUNT="$2"
FINAL=""
install() {
  TARGET="$1"
  OLD="$TARGET.old"
  rm -rf "$OLD" 2>/dev/null
  if [ -d "$TARGET" ] && ! mv "$TARGET" "$OLD" 2>/dev/null; then
    return 1
  fi
  if ! ditto "$SRC" "$TARGET" 2>/dev/null; then
    rm -rf "$TARGET" 2>/dev/null
    [ -d "$OLD" ] && mv "$OLD" "$TARGET" 2>/dev/null
    return 1
  fi
  rm -rf "$OLD" 2>/dev/null
  FINAL="$TARGET"
  return 0
}
mkdir -p "$MOUNT"
hdiutil attach "$DMG" -nobrowse -readonly -mountpoint "$MOUNT" >/dev/null 2>&1 || exit 1
SRC="$MOUNT/kun.app"
[ -d "$SRC" ] || { hdiutil detach "$MOUNT" -quiet >/dev/null 2>&1 || true; exit 2; }
i=0
while [ $i -lt 50 ]; do
  if ! pgrep -x kun-app >/dev/null 2>&1; then break; fi
  sleep 0.2
  i=$((i+1))
done
install "/Applications/kun.app" || install "$HOME/Applications/kun.app" || { hdiutil detach "$MOUNT" -quiet >/dev/null 2>&1 || true; exit 3; }
hdiutil detach "$MOUNT" -quiet >/dev/null 2>&1 || true
rmdir "$MOUNT" 2>/dev/null || true
rm -f "$DMG" 2>/dev/null
open "$FINAL"
"#;

impl KunApp {
    /// 创建应用（启动本地终端会话）。
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        crate::theme::set_theme(&cc.egui_ctx, 0);
        let ctx = cc.egui_ctx.clone();

        let config_path = default_config_path();
        let config = HostConfig::load(&config_path).unwrap_or_default();

        let mut app = Self {
            tabs: Vec::new(),
            active_tab: 0,
            pending_tab: None,
            last_row_click: None,
            show_settings: false,
            last_ctx: cc.egui_ctx.clone(),
            config,
            config_path,
            show_new_conn: false,
            form: ConnectForm::default(),
            pending: None,
            pending_label: String::new(),
            toast: None,
            pending_sftp: None,
            ready_sftp: None,
            sftp_error: None,
            sftp_host: String::new(),
            update_state: UpdateState::Idle,
            update_rx: None,
            download_rx: None,
            manual_update: false,
            restart_at: None,
        };
        // 启动时自动检查更新（后台线程，延迟 3 秒，静默）。
        app.start_update_check(true);
        // 初始一个本地终端 tab；设置改弹窗（`show_settings`）。
        app.new_local_tab(&ctx);
        app
    }

    /// 弹出轻提示（自动淡出）。
    fn show_toast(&mut self, message: impl Into<String>, is_error: bool) {
        self.toast = Some(Toast {
            message: message.into(),
            is_error,
            start: f64::NAN,
        });
    }

    /// 新建本地终端标签页并激活。
    fn new_local_tab(&mut self, ctx: &egui::Context) {
        let ctx = ctx.clone();
        let on_event = Arc::new(move |_ev: &SessionEvent| {
            ctx.request_repaint();
        });
        match Session::spawn_local(local_session_options(), 80, 24, on_event) {
            Ok(session) => {
                let tab = Box::new(TerminalTab::new(
                    "本地终端".into(),
                    TerminalView::new(session),
                ));
                self.tabs.push(tab);
                self.active_tab = self.tabs.len() - 1;
            }
            Err(e) => {
                log::error!("启动本地终端失败：{e}");
                self.show_toast(format!("启动本地终端失败：{e}"), true);
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
        let arch = macos_arch().to_string();
        std::thread::spawn(move || {
            if delay {
                std::thread::sleep(Duration::from_secs(3));
            }
            let result = check_for_update(&current, kun_core::updater::DEFAULT_REPO, &arch);
            let _ = tx.send(result);
        });
        self.update_rx = Some(rx);
        self.update_state = UpdateState::Checking;
        self.manual_update = !delay;
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
            match result {
                Ok(Some(info)) => self.update_state = UpdateState::Available(info),
                Ok(None) => {
                    self.update_state = UpdateState::UpToDate;
                    if self.manual_update {
                        self.show_toast(
                            format!("已是最新版本 v{}", env!("CARGO_PKG_VERSION")),
                            false,
                        );
                    }
                }
                Err(e) => {
                    self.update_state = UpdateState::Failed;
                    if self.manual_update {
                        self.show_toast(format!("检查更新失败：{e}"), true);
                    } else {
                        log::debug!("检查更新失败：{e}");
                    }
                }
            }
            self.manual_update = false;
        }
    }

    /// 开始下载更新资产。
    fn start_download(&mut self, info: UpdateInfo) {
        let (tx, rx) = std::sync::mpsc::channel();
        let url = info.asset_url.clone();
        let dest = temp_dmg_path(&info.asset_name);
        std::thread::spawn(move || {
            let result = kun_core::updater::download_asset(&url, &dest, |done, total| {
                let _ = tx.send(DownloadEvent::Progress {
                    downloaded: done,
                    total,
                });
            });
            match result {
                Ok(()) => {
                    let _ = tx.send(DownloadEvent::Done(dest));
                }
                Err(e) => {
                    let _ = tx.send(DownloadEvent::Error(e));
                }
            }
        });
        self.download_rx = Some(rx);
        self.update_state = UpdateState::Downloading(DownloadState {
            info,
            downloaded: 0,
            total: None,
        });
    }

    /// 处理下载进度/结果。
    fn poll_download(&mut self, ctx: &egui::Context) {
        let mut events = Vec::new();
        if let Some(rx) = &self.download_rx {
            while let Ok(ev) = rx.try_recv() {
                events.push(ev);
            }
        }
        for ev in events {
            let info = match &self.update_state {
                UpdateState::Downloading(s) => Some(s.info.clone()),
                _ => None,
            };
            let Some(info) = info else {
                continue;
            };
            match ev {
                DownloadEvent::Progress { downloaded, total } => {
                    self.update_state = UpdateState::Downloading(DownloadState {
                        info,
                        downloaded,
                        total,
                    });
                    ctx.request_repaint();
                }
                DownloadEvent::Done(path) => {
                    self.download_rx = None;
                    self.update_state = UpdateState::Downloaded {
                        info,
                        dmg_path: path,
                    };
                }
                DownloadEvent::Error(e) => {
                    self.download_rx = None;
                    self.update_state = UpdateState::Error(e);
                }
            }
        }
    }

    /// 启动安装脚本并安排重启。
    fn install_update(&mut self, ctx: &egui::Context, dmg_path: PathBuf) {
        // 先切到"正在安装"（若脚本启动失败会退回错误态）。
        if let UpdateState::Downloaded { info, .. } = &self.update_state {
            self.update_state = UpdateState::Installing(info.clone());
        }
        let mount = std::env::temp_dir().join("kun-update").join("mount");
        match launch_installer(&dmg_path, &mount) {
            Ok(()) => {
                self.update_state = UpdateState::Installed;
                self.restart_at = Some(anim::now(ctx) + 0.9);
                ctx.request_repaint_after(Duration::from_millis(50));
            }
            Err(e) => {
                self.update_state = UpdateState::Error(format!("启动安装脚本失败：{e}"));
            }
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
                    self.tabs
                        .push(Box::new(TerminalTab::new(self.pending_label.clone(), view)));
                    self.active_tab = self.tabs.len() - 1;
                    self.pending_tab = Some(self.active_tab);
                    self.mount_ready_sftp();
                    self.show_toast(format!("已连接到 {}", self.pending_label), false);
                    ctx.request_repaint();
                }
                ConnectResult::Failed(e) => {
                    log::error!("连接失败：{e}");
                    self.show_toast(format!("连接失败：{e}"), true);
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

        let (_sftp_thread, sftp_handle, sftp_rx) = connect_sftp(&profile);
        self.sftp_host = label;
        self.sftp_error = None;
        self.pending_sftp = Some((sftp_handle, sftp_rx));
        self.ready_sftp = None;
        self.pending_tab = None;
    }

    /// 将已就绪的 SFTP 会话挂载到 SSH 标签页。
    fn mount_ready_sftp(&mut self) {
        let Some(idx) = self.pending_tab else { return };
        let Some((handle, rx)) = self.ready_sftp.take() else {
            return;
        };
        if let Some(tab) = self.tabs.get_mut(idx) {
            tab.sftp = Some(SftpView::new(&self.sftp_host, handle, rx));
        }
    }

    /// 处理 SFTP 连接结果。
    fn poll_sftp(&mut self) {
        let mut ready = false;
        let mut failed: Option<String> = None;
        let mut closed = false;
        if let Some((_handle, rx)) = &mut self.pending_sftp {
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    SftpEvent::Ready => ready = true,
                    SftpEvent::Failed(e) => failed = Some(e),
                    // 连接中途关闭（如被服务器断开）：不能继续等待，
                    // 否则状态栏会永远停在"SFTP 连接中…"。
                    SftpEvent::Closed => closed = true,
                    _ => {}
                }
            }
        }
        if ready {
            self.ready_sftp = self.pending_sftp.take();
            self.mount_ready_sftp();
        }
        let err = failed.or(closed.then(|| "连接中断".to_string()));
        if let Some(e) = err {
            self.pending_sftp = None;
            self.ready_sftp = None;
            // 状态栏持久显示（toast 一闪而过容易忽略）。
            self.sftp_error = Some(format!("SFTP 连接失败：{e}"));
            self.show_toast(self.sftp_error.clone().unwrap(), true);
        }
    }

    /// 设置弹窗：macOS 系统偏好设置风格，分三组（主机 / 外观 / 关于）。
    /// 内部调用 `host_sidebar` 渲染主机列表；外观组合并主题 ComboBox；
    /// 关于组展示版本号与手动检查更新按钮。
    /// 卡片化分组（`bg_elevated` 底 + 边框 + 圆角 + 紧凑内边距）。
    fn settings_panel(&mut self, ctx: &egui::Context) {
        let theme = crate::theme::current_theme();
        let mut open = self.show_settings;
        egui::Window::new("设置")
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, -20.0])
            .default_size([520.0, 540.0])
            .min_size([460.0, 420.0])
            // 限制最大尺寸：避免主机行多时被内容撑大（保持固定外观）。
            .max_size([520.0, 540.0])
            .resizable(true)
            .collapsible(false)
            .frame(
                egui::Frame::new()
                    .fill(theme.bg_app)
                    .corner_radius(crate::theme::tokens::RADIUS_SM)
                    .stroke(egui::Stroke::new(1.0, theme.border))
                    .inner_margin(egui::Margin::same(18)),
            )
            .show(ctx, |ui| {
                // 唯一标题由 `egui::Window::new("设置")` 自带，
                // 这里不再重复加标题与副标题（之前导致双层标题问题）。
                ui.add_space(4.0);

                egui::ScrollArea::vertical()
                    .auto_shrink([false, true])
                    // 弹窗总高 540 - 标题 28 - 内边距 36 - 边距 ≈ 476，
                    // 留 6px 给三组间距，紧凑但不被主机列表撑大。
                    .max_height(470.0)
                    .show(ui, |ui| {
                        // ============ 主机管理 ============
                        Self::settings_card(ui, "主机管理", |ui| {
                            self.host_sidebar(ui);
                        });
                        ui.add_space(10.0);

                        // ============ 外观 ============
                        Self::settings_card(ui, "外观", |ui| {
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new("主题")
                                        .size(12.0)
                                        .color(theme.text_muted),
                                );
                                ui.add_space(8.0);
                                let current = crate::theme::current_theme().name;
                                egui::ComboBox::from_id_salt("settings_theme_switcher")
                                    .selected_text(
                                        egui::RichText::new(current).color(theme.text_primary),
                                    )
                                    .show_ui(ui, |ui| {
                                        for (i, t) in crate::theme::THEMES.iter().enumerate() {
                                            let selected = current == t.name;
                                            if ui
                                                .selectable_label(
                                                    selected,
                                                    egui::RichText::new(t.name).color(
                                                        if selected {
                                                            theme.accent
                                                        } else {
                                                            theme.text_primary
                                                        },
                                                    ),
                                                )
                                                .clicked()
                                            {
                                                crate::theme::set_theme(ui.ctx(), i);
                                                self.show_toast(format!("主题：{}", t.name), false);
                                            }
                                        }
                                    });
                            });
                        });
                        ui.add_space(10.0);

                        // ============ 关于 ============
                        Self::settings_card(ui, "关于", |ui| {
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "kun v{}",
                                        env!("CARGO_PKG_VERSION")
                                    ))
                                    .size(12.0)
                                    .color(theme.text_secondary),
                                );
                            });
                            ui.add_space(8.0);
                            let (label, dot, pulse) = match &self.update_state {
                                UpdateState::Idle => ("检查更新", None, false),
                                UpdateState::Checking => ("检查中…", Some(theme.text_muted), false),
                                UpdateState::Available(_) => {
                                    ("新版本可用", Some(theme.accent2), true)
                                }
                                UpdateState::UpToDate => ("已是最新", Some(theme.success), false),
                                UpdateState::Failed => ("检查失败", Some(theme.danger), false),
                                UpdateState::Downloading(_) => {
                                    ("正在下载", Some(theme.accent), false)
                                }
                                UpdateState::Downloaded { .. } => {
                                    ("准备安装", Some(theme.accent), true)
                                }
                                UpdateState::Installing(_) => {
                                    ("安装中…", Some(theme.accent), false)
                                }
                                UpdateState::Installed => ("已更新", Some(theme.success), false),
                                UpdateState::Error(_) => ("更新出错", Some(theme.danger), true),
                            };
                            ui.horizontal(|ui| {
                                if let Some(color) = dot {
                                    status_dot(ui, color, pulse);
                                    ui.add_space(6.0);
                                }
                                if ui
                                    .add(
                                        egui::Button::new(
                                            egui::RichText::new(label)
                                                .size(12.0)
                                                .color(theme.text_primary),
                                        )
                                        .fill(theme.bg_elevated)
                                        .stroke(egui::Stroke::new(1.0, theme.border))
                                        .corner_radius(crate::theme::tokens::RADIUS_ITEM),
                                    )
                                    .on_hover_text("检查更新")
                                    .clicked()
                                    && matches!(
                                        self.update_state,
                                        UpdateState::Idle
                                            | UpdateState::UpToDate
                                            | UpdateState::Failed
                                            | UpdateState::Error(_)
                                    )
                                {
                                    self.start_update_check(false);
                                }
                            });
                        });
                    });
            });
        // Esc 关闭 / × 关闭：open 同步回 self.show_settings。
        self.show_settings = open;
    }

    /// 设置弹窗的卡片化分组容器：`bg_elevated` 底 + 边框 + 圆角 + 紧凑内边距。
    /// 头部 accent2 颜色小标题 + 内容区。自由函数避免借用 self 与 ctx 冲突。
    fn settings_card(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
        let theme = crate::theme::current_theme();
        let frame = egui::Frame::new()
            .fill(theme.bg_elevated)
            .stroke(egui::Stroke::new(1.0, theme.border))
            .corner_radius(crate::theme::tokens::RADIUS_SM)
            .inner_margin(egui::Margin::symmetric(12, 10));
        frame.show(ui, |ui| {
            ui.set_min_width(ui.available_width() - 4.0);
            ui.label(
                egui::RichText::new(title)
                    .strong()
                    .size(11.5)
                    .color(theme.accent2),
            );
            ui.add_space(6.0);
            body(ui);
        });
    }

    /// 渲染左侧主机列表。
    fn host_sidebar(&mut self, ui: &mut egui::Ui) {
        let theme = crate::theme::current_theme();
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("主机")
                    .strong()
                    .size(13.0)
                    .color(theme.text_primary),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new(format!("{} 台", self.config.hosts.len()))
                        .size(11.0)
                        .color(theme.text_muted),
                );
            });
        });
        ui.add_space(8.0);

        // 新建连接（主按钮）。
        let new_btn = egui::Button::new(
            egui::RichText::new("新建连接")
                .color(crate::theme::tokens::ACCENT_FG)
                .size(13.0),
        )
        .fill(theme.accent)
        .stroke(egui::Stroke::NONE)
        .corner_radius(crate::theme::tokens::RADIUS_SM);
        if ui
            .add_sized(egui::vec2(ui.available_width(), 30.0), new_btn)
            .clicked()
        {
            self.show_new_conn = true;
        }
        ui.add_space(12.0);
        hairline(ui, theme);
        ui.add_space(6.0);

        if self.config.hosts.is_empty() {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new("暂无已保存主机")
                        .color(theme.text_muted)
                        .size(12.5),
                );
                ui.add_space(2.0);
                ui.label(
                    egui::RichText::new("⌘N 添加第一台主机")
                        .size(11.0)
                        .color(theme.text_muted),
                );
            });
        }

        let mut remove_idx: Option<usize> = None;
        let mut connect_idx: Option<usize> = None;
        let mut selected_host: Option<usize> = None;
        // 行内容可用宽度：面板宽 - 内边距（供文本截断用，避免长名称换行）。
        let row_avail_w = ui.available_width() - 34.0;
        for (i, host) in self.config.hosts.iter().enumerate() {
            let row_id = egui::Id::new(("host_row", i));
            // 行内容（头像 + 名称）。
            let inner = ui
                .scope_builder(egui::UiBuilder::new().id_salt(row_id), |ui| {
                    ui.horizontal(|ui| {
                        let avatar_size = 30.0;
                        let (avatar_rect, _) = ui.allocate_exact_size(
                            egui::vec2(avatar_size, avatar_size),
                            egui::Sense::hover(),
                        );
                        anim::paint_rounded_gradient(
                            ui.painter(),
                            avatar_rect,
                            avatar_size * 0.3,
                            theme.accent,
                            theme.accent2,
                        );
                        let initial = host.name.chars().next().unwrap_or('?');
                        ui.painter().text(
                            avatar_rect.center(),
                            egui::Align2::CENTER_CENTER,
                            initial.to_string(),
                            egui::FontId::proportional(13.0),
                            egui::Color32::WHITE,
                        );
                        ui.add_space(8.0);
                        // 文本宽度固定，超长截断不换行。
                        ui.vertical(|ui| {
                            ui.add_sized(
                                [row_avail_w, 18.0],
                                egui::Label::new(
                                    egui::RichText::new(&host.name)
                                        .size(13.0)
                                        .color(theme.text_primary),
                                )
                                .truncate(),
                            );
                            ui.add_sized(
                                [row_avail_w, 16.0],
                                egui::Label::new(
                                    egui::RichText::new(format!("{}@{}", host.user, host.host))
                                        .size(11.0)
                                        .color(theme.text_muted),
                                )
                                .truncate(),
                            );
                        });
                    });
                })
                .response;

            // 整行点击区（显式 interact：egui 0.36 响应链不可靠）。
            // 点击区横向扩展到面板可用宽度，保证短名称主机也能整行点击。
            let row_rect = egui::Rect::from_min_max(
                inner.rect.min,
                egui::pos2(ui.max_rect().right() - 4.0, inner.rect.bottom()),
            );
            let row_response = ui
                .interact(row_rect, row_id, egui::Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            // 删除图标（行右缘，最后注册保证可点）。
            let del_rect = egui::Rect::from_min_size(
                egui::pos2(row_rect.right() - 26.0, row_rect.top() + 3.0),
                egui::vec2(22.0, 22.0),
            );
            let del_resp = ui
                .interact(del_rect, row_id.with("del"), egui::Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            if del_resp.hovered() {
                ui.painter().rect_filled(
                    del_rect,
                    crate::theme::tokens::RADIUS_ITEM,
                    theme.danger.gamma_multiply(0.22),
                );
            }
            ui.painter().text(
                del_rect.center(),
                egui::Align2::CENTER_CENTER,
                "🗑",
                egui::FontId::proportional(13.0),
                if del_resp.hovered() {
                    theme.danger
                } else {
                    theme.text_muted
                },
            );
            if del_resp.clicked() {
                remove_idx = Some(i);
            }
            if row_response.clicked() {
                selected_host = Some(i);
                let now = ui.input(|i| i.time);
                if let Some((t, idx)) = self.last_row_click {
                    if idx == i && now - t < 0.3 {
                        connect_idx = Some(i);
                    }
                }
                self.last_row_click = Some((now, i));
            }

            // 选中/hover 高亮（动画过渡）。
            let hover = row_response.hovered();
            let selected = selected_host == Some(i);
            let hover_alpha =
                anim::smooth_bool(ui.ctx(), row_id.with("hover"), hover, anim::SPEED_FAST);
            let sel_alpha =
                anim::smooth_bool(ui.ctx(), row_id.with("sel"), selected, anim::SPEED_FAST);
            let alpha = (hover_alpha * 0.72).max(sel_alpha);
            if alpha > 0.01 {
                ui.painter().rect_filled(
                    row_rect.expand2(egui::vec2(0.0, 4.0)),
                    crate::theme::tokens::RADIUS_ITEM,
                    theme.accent_soft.gamma_multiply(alpha),
                );
            }
            if sel_alpha > 0.01 {
                ui.painter().rect_filled(
                    egui::Rect::from_min_max(
                        egui::pos2(row_rect.left() + 1.0, row_rect.top() + 4.0),
                        egui::pos2(row_rect.left() + 3.0, row_rect.bottom() - 4.0),
                    ),
                    1.5,
                    theme.accent.gamma_multiply(sel_alpha),
                );
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
        let theme = crate::theme::current_theme();
        let field_width = 300.0;
        let mut port_error = false;
        let mut open = self.show_new_conn;
        let mut to_connect: Option<HostProfile> = None;
        let mut canceled = false;
        egui::Window::new("新建连接")
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, -20.0])
            .resizable(false)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    draw_logo_mark(ui, 42.0);
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new("建立安全连接")
                            .strong()
                            .size(18.0)
                            .color(theme.text_primary),
                    );
                    ui.label(
                        egui::RichText::new("连接到你的远程工作空间")
                            .size(11.0)
                            .color(theme.text_muted),
                    );
                });
                ui.add_space(14.0);
                ui.label(
                    egui::RichText::new("连接身份")
                        .strong()
                        .color(theme.accent2),
                );
                ui.add_space(4.0);
                let name_id = egui::Id::new("conn_form_name");
                if !self.form.name_focused {
                    ui.memory_mut(|m| m.request_focus(name_id));
                    self.form.name_focused = true;
                }
                ui.label(
                    egui::RichText::new("名称")
                        .size(11.0)
                        .color(theme.text_muted),
                );
                form_input(
                    ui,
                    name_id,
                    &mut self.form.name,
                    "连接名称（可选）",
                    field_width,
                    false,
                );
                ui.add_space(5.0);
                ui.label(
                    egui::RichText::new("用户名")
                        .size(11.0)
                        .color(theme.text_muted),
                );
                form_input(
                    ui,
                    egui::Id::new("conn_form_user"),
                    &mut self.form.user,
                    "用户名，例如 root",
                    field_width,
                    false,
                );
                ui.add_space(12.0);
                ui.label(
                    egui::RichText::new("网络地址")
                        .strong()
                        .color(theme.accent2),
                );
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new("主机 / 端口")
                        .size(11.0)
                        .color(theme.text_muted),
                );
                ui.horizontal(|ui| {
                    form_input(
                        ui,
                        egui::Id::new("conn_form_host"),
                        &mut self.form.host,
                        "主机名或 IP",
                        field_width - 76.0,
                        false,
                    );
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new("端口")
                            .size(11.0)
                            .color(theme.text_muted),
                    );
                    form_input(
                        ui,
                        egui::Id::new("conn_form_port"),
                        &mut self.form.port,
                        "22",
                        68.0,
                        false,
                    );
                });
                ui.add_space(12.0);
                ui.label(
                    egui::RichText::new("认证方式")
                        .strong()
                        .color(theme.accent2),
                );
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.form.auth_kind, 0, "密码");
                    ui.selectable_value(&mut self.form.auth_kind, 1, "私钥");
                });
                ui.add_space(5.0);
                if self.form.auth_kind == 0 {
                    form_input(
                        ui,
                        egui::Id::new("conn_form_password"),
                        &mut self.form.password,
                        "密码",
                        field_width,
                        true,
                    );
                } else {
                    form_input(
                        ui,
                        egui::Id::new("conn_form_key"),
                        &mut self.form.key_path,
                        "私钥文件路径",
                        field_width,
                        false,
                    );
                    ui.add_space(5.0);
                    form_input(
                        ui,
                        egui::Id::new("conn_form_pass"),
                        &mut self.form.passphrase,
                        "私钥口令（可选）",
                        field_width,
                        true,
                    );
                }
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(6.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let connect = egui::Button::new(
                        egui::RichText::new("连接")
                            .color(crate::theme::tokens::ACCENT_FG)
                            .size(13.0),
                    )
                    .fill(theme.accent)
                    .stroke(egui::Stroke::NONE)
                    .corner_radius(crate::theme::tokens::RADIUS_SM);
                    if ui.add(connect).clicked() {
                        let port: u16 = match self.form.port.trim().parse() {
                            Ok(port @ 1..=65535) => port,
                            _ => {
                                port_error = true;
                                22
                            }
                        };
                        if port_error {
                            self.show_toast("端口必须是 1-65535 的数字", true);
                            return;
                        }
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
                            to_connect = Some(profile);
                        } else {
                            self.show_toast("请填写主机与用户名", true);
                        }
                    }
                    if ui.button("取消").clicked() {
                        canceled = true;
                    }
                });
            });
        if let Some(profile) = to_connect {
            self.config.hosts.push(profile.clone());
            self.save_config();
            self.show_new_conn = false;
            self.form.name_focused = false;
            self.start_connect(ctx, profile);
        } else if canceled || !open {
            self.show_new_conn = false;
            self.form.name_focused = false;
        } else {
            self.show_new_conn = true;
        }
    }

    /// 渲染标签页栏（Warp 风格圆角标签 + 底部指示条）。
    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        let theme = crate::theme::current_theme();
        // 无边框窗口拖拽：整行注册底层 drag 背景（先注册，被后注册的控件覆盖）。
        // egui 命中规则：后注册 widget 在顶层，控件上点击/拖拽优先命中控件，
        // 标签栏空白处按下拖动则命中此背景 → 发 StartDrag 让系统接管窗口移动。
        let drag_rect = ui.max_rect();
        let drag_resp = ui.interact(
            drag_rect,
            egui::Id::new("tab_bar_drag"),
            egui::Sense::drag(),
        );
        if drag_resp.drag_started() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        let mut tl_clicked: Option<usize> = None;
        ui.horizontal(|ui| {
            // macOS traffic lights（自绘，因为关闭了 eframe 标题栏）。
            let (tl, _) = draw_traffic_lights(ui);
            tl_clicked = tl;
            let mut switch_to: Option<usize> = None;
            let mut close_idx: Option<usize> = None;
            for (i, tab) in self.tabs.iter().enumerate() {
                let title = tab.title();
                let selected = i == self.active_tab;
                // 数字索引前缀：与终端主流 Tab 习惯一致（1, 2, ...）。
                let prefix = format!("{} ", i + 1);
                let row = ui
                    .scope_builder(egui::UiBuilder::new().id_salt(("tab", i)), |ui| {
                        ui.horizontal(|ui| {
                            ui.add_space(6.0);
                            // 数字索引（muted 色，提示序号）。
                            ui.label(
                                egui::RichText::new(&prefix)
                                    .size(11.5)
                                    .color(if selected {
                                        theme.text_muted
                                    } else {
                                        theme.text_muted.gamma_multiply(0.7)
                                    })
                                    .monospace(),
                            );
                            // 无边框透明按钮（selectable_label 选中自带边框，
                            // 与下方手绘高亮叠加会形成"双重框"）。
                            if ui
                                .add(
                                    egui::Button::new(
                                        egui::RichText::new(&title).size(12.5).color(if selected {
                                            theme.text_primary
                                        } else {
                                            theme.text_muted
                                        }),
                                    )
                                    .fill(egui::Color32::TRANSPARENT)
                                    .stroke(egui::Stroke::NONE)
                                    .corner_radius(crate::theme::tokens::RADIUS_ITEM),
                                )
                                .clicked()
                            {
                                switch_to = Some(i);
                            }
                            ui.add_space(1.0);
                            if ui
                                .add(
                                    egui::Button::new("×")
                                        .fill(egui::Color32::TRANSPARENT)
                                        .stroke(egui::Stroke::NONE)
                                        .min_size(egui::vec2(18.0, 18.0))
                                        .corner_radius(4.0),
                                )
                                .on_hover_text("关闭标签页")
                                .clicked()
                            {
                                close_idx = Some(i);
                            }
                            ui.add_space(4.0);
                        });
                    })
                    .response;

                // 选中底 / hover 底（动画过渡，唯一的高亮层）。
                let hover = row.hovered() && !selected;
                let hover_alpha = anim::smooth_bool(
                    ui.ctx(),
                    egui::Id::new(("tab_hover", i)),
                    hover,
                    anim::SPEED_FAST,
                );
                let sel_alpha = anim::smooth_bool(
                    ui.ctx(),
                    egui::Id::new(("tab_sel", i)),
                    selected,
                    anim::SPEED_FAST,
                );
                if sel_alpha > 0.01 {
                    ui.painter().rect_filled(
                        row.rect.expand2(egui::vec2(2.0, 3.0)),
                        crate::theme::tokens::RADIUS_ITEM,
                        theme.accent_soft.gamma_multiply(sel_alpha),
                    );
                } else if hover_alpha > 0.01 {
                    ui.painter().rect_filled(
                        row.rect.expand2(egui::vec2(1.0, 2.0)),
                        crate::theme::tokens::RADIUS_ITEM,
                        theme.accent_soft.gamma_multiply(hover_alpha * 0.65),
                    );
                }
                // 底部指示条（宽度随选中状态动画）。
                let bar_w = anim::smooth(
                    ui.ctx(),
                    egui::Id::new(("tab_bar_w", i)),
                    if selected {
                        row.rect.width() * 0.58
                    } else {
                        0.0
                    },
                    anim::SPEED_NORMAL,
                );
                if bar_w > 0.5 {
                    let bar = egui::Rect::from_center_size(
                        egui::pos2(row.rect.center().x, row.rect.bottom() - 1.0),
                        egui::vec2(bar_w, 2.0),
                    );
                    ui.painter().rect_filled(
                        bar,
                        1.0,
                        theme.accent.gamma_multiply(sel_alpha.max(0.25)),
                    );
                }
            }
            // 新建本地终端标签（＋）。
            if ui
                .add(
                    egui::Button::new(egui::RichText::new("＋").color(theme.text_muted))
                        .fill(egui::Color32::TRANSPARENT)
                        .stroke(egui::Stroke::NONE)
                        .corner_radius(crate::theme::tokens::RADIUS_ITEM),
                )
                .on_hover_text("新建本地终端（⌘T）")
                .clicked()
            {
                self.new_local_tab(ui.ctx());
            }

            // 顶到右侧：设置齿轮（设置弹窗入口）。
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if settings_gear_button(ui) {
                    self.show_settings = true;
                }
            });

            if let Some(i) = switch_to {
                self.active_tab = i;
            }
            if let Some(i) = close_idx {
                self.close_tab(i);
            }
        });
        // traffic lights 点击 → ViewportCommand（闭包外避免借用冲突）。
        if let Some(idx) = tl_clicked {
            match idx {
                0 => ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close),
                1 => ui
                    .ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Minimized(true)),
                _ => ui
                    .ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Fullscreen(true)),
            }
        }
    }

    /// 切换设置弹窗开关状态。
    fn toggle_settings(&mut self) {
        self.show_settings = !self.show_settings;
    }

    /// 渲染状态栏。
    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let theme = crate::theme::current_theme();
        ui.horizontal(|ui| {
            ui.add_space(6.0);
            if let Some(tab) = self.tabs.get(self.active_tab) {
                let session = tab.terminal.session();
                let title = session.title();
                let exited = session.has_exited();
                status_dot(ui, if exited { theme.danger } else { theme.success }, false);
                ui.label(
                    egui::RichText::new(if title.is_empty() {
                        tab.label.clone()
                    } else {
                        title
                    })
                    .size(11.5)
                    .color(theme.text_secondary),
                );
                if exited {
                    ui.colored_label(theme.danger, "会话已退出");
                }
                if tab.sftp.is_some() {
                    ui.separator();
                    status_dot(ui, theme.accent2, false);
                    ui.colored_label(theme.accent2, format!("SFTP · {}", self.sftp_host));
                }
            }
            if self.pending_sftp.is_some() {
                ui.separator();
                ui.spinner();
                ui.label(
                    egui::RichText::new("SFTP 连接中…")
                        .size(11.5)
                        .color(theme.text_muted),
                );
            }
            if let Some(e) = &self.sftp_error {
                ui.separator();
                status_dot(ui, theme.danger, false);
                ui.label(egui::RichText::new(e).size(11.5).color(theme.danger))
                    .on_hover_text("重新连接主机可再次尝试");
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.available_width() > 460.0 {
                    // 快捷键提示：圆角深色块，按键 accent 色 + 动作次要色。
                    for (key, action) in [
                        ("⌘B", "主机"),
                        ("⌘T", "终端"),
                        ("⌘W", "关闭"),
                        ("⌘N", "连接"),
                        ("⌥1-3", "主题"),
                    ] {
                        egui::Frame::new()
                            .fill(theme.bg_elevated)
                            .stroke(egui::Stroke::new(1.0, theme.border))
                            .corner_radius(4.0)
                            .inner_margin(egui::Margin::symmetric(7, 2))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label(
                                        egui::RichText::new(key)
                                            .monospace()
                                            .size(10.5)
                                            .color(theme.accent),
                                    );
                                    ui.add_space(2.0);
                                    ui.label(
                                        egui::RichText::new(action)
                                            .size(10.5)
                                            .color(theme.text_secondary),
                                    );
                                });
                            });
                        ui.add_space(2.0);
                    }
                }
            });
        });
    }

    /// 更新弹窗（下载进度 / 安装 / 错误统一入口）。
    fn update_dialog(&mut self, ctx: &egui::Context) {
        let theme = crate::theme::current_theme();
        let (version, notes, url, downloaded, total, error, is_downloading, is_downloaded) =
            match &self.update_state {
                UpdateState::Available(info) => (
                    Some(info.version.clone()),
                    Some(info.notes.clone()),
                    Some(info.url.clone()),
                    None,
                    None,
                    None,
                    false,
                    false,
                ),
                UpdateState::Downloading(s) => (
                    Some(s.info.version.clone()),
                    Some(s.info.notes.clone()),
                    None,
                    Some(s.downloaded),
                    s.total,
                    None,
                    true,
                    false,
                ),
                UpdateState::Downloaded { info, .. } => (
                    Some(info.version.clone()),
                    Some(info.notes.clone()),
                    None,
                    None,
                    None,
                    None,
                    false,
                    true,
                ),
                UpdateState::Installing(info) => (
                    Some(info.version.clone()),
                    None,
                    None,
                    None,
                    None,
                    None,
                    false,
                    false,
                ),
                UpdateState::Installed => (None, None, None, None, None, None, false, false),
                UpdateState::Error(e) => {
                    (None, None, None, None, None, Some(e.clone()), false, false)
                }
                _ => return,
            };
        let dmg_path = match &self.update_state {
            UpdateState::Downloaded { dmg_path, .. } => Some(dmg_path.clone()),
            _ => None,
        };
        let available_info = match &self.update_state {
            UpdateState::Available(info) => Some(info.clone()),
            _ => None,
        };

        let mut action: Option<UpdateAction> = None;
        egui::Window::new("发现新版本")
            .anchor(egui::Align2::CENTER_CENTER, [0.0, -20.0])
            .default_width(460.0)
            .resizable(false)
            .collapsible(false)
            .show(ctx, |ui| {
                // 头部。
                ui.horizontal(|ui| {
                    draw_logo_mark(ui, 28.0);
                    ui.add_space(8.0);
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new("更新 kun")
                                .strong()
                                .size(16.0)
                                .color(theme.text_primary),
                        );
                        if let Some(v) = &version {
                            ui.label(
                                egui::RichText::new(format!(
                                    "v{v} 已发布 · 当前 v{}",
                                    env!("CARGO_PKG_VERSION")
                                ))
                                .size(11.5)
                                .color(theme.text_secondary),
                            );
                        }
                    });
                });
                ui.add_space(10.0);

                if let Some(e) = &error {
                    ui.label(egui::RichText::new(e).color(theme.danger).size(12.5));
                    ui.add_space(8.0);
                } else if is_downloading {
                    // 下载进度。
                    let fraction = match total {
                        Some(t) if t > 0 => downloaded.unwrap_or(0) as f32 / t as f32,
                        _ => f32::NAN,
                    };
                    progress_bar(ui, fraction);
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(match total {
                            Some(t) => format!(
                                "{} / {}（{:.0}%）",
                                fmt_bytes(downloaded.unwrap_or(0)),
                                fmt_bytes(t),
                                fraction * 100.0
                            ),
                            None => fmt_bytes(downloaded.unwrap_or(0)),
                        })
                        .size(11.5)
                        .color(theme.text_secondary),
                    );
                    ui.add_space(8.0);
                } else if is_downloaded {
                    ui.horizontal(|ui| {
                        status_dot(ui, theme.success, false);
                        ui.label(
                            egui::RichText::new("下载完成，重启后即可生效")
                                .color(theme.success)
                                .size(12.5),
                        );
                    });
                    ui.add_space(8.0);
                } else if matches!(self.update_state, UpdateState::Installing(_)) {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(
                            egui::RichText::new("正在挂载并安装，请稍候…")
                                .size(12.5)
                                .color(theme.text_secondary),
                        );
                    });
                    ui.add_space(8.0);
                } else if matches!(self.update_state, UpdateState::Installed) {
                    ui.horizontal(|ui| {
                        status_dot(ui, theme.success, false);
                        ui.label(
                            egui::RichText::new("安装已启动，应用即将重启")
                                .color(theme.success)
                                .size(12.5),
                        );
                    });
                    ui.add_space(8.0);
                } else if let Some(notes) = &notes {
                    if !notes.is_empty() {
                        egui::ScrollArea::vertical()
                            .id_salt("update_notes")
                            .max_height(170.0)
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                // release 说明逐行渲染：空行留段落间距，普通行紧凑排列。
                                let mut first = true;
                                let mut blank = false;
                                for line in notes.lines() {
                                    let text = line.trim();
                                    if text.is_empty() {
                                        blank = true;
                                        continue;
                                    }
                                    if !first {
                                        ui.add_space(if blank { 8.0 } else { 2.0 });
                                    }
                                    first = false;
                                    blank = false;
                                    ui.label(
                                        egui::RichText::new(text)
                                            .size(12.5)
                                            .color(theme.text_secondary),
                                    );
                                }
                            });
                        ui.add_space(8.0);
                    }
                }

                ui.separator();
                ui.add_space(6.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let primary = |label: &str| {
                        egui::Button::new(
                            egui::RichText::new(label)
                                .color(crate::theme::tokens::ACCENT_FG)
                                .size(13.0),
                        )
                        .fill(theme.accent)
                        .stroke(egui::Stroke::NONE)
                        .corner_radius(crate::theme::tokens::RADIUS_SM)
                    };
                    if is_downloaded {
                        if let Some(dmg) = &dmg_path {
                            if ui.add(primary("安装并重启")).clicked() {
                                action = Some(UpdateAction::Install {
                                    dmg_path: dmg.clone(),
                                });
                            }
                        }
                        if ui.button("取消").clicked() {
                            action = Some(UpdateAction::Dismiss);
                        }
                    } else if is_downloading {
                        if ui.button("取消").clicked() {
                            action = Some(UpdateAction::CancelDownload);
                        }
                    } else if error.is_some() {
                        if ui.add(primary("重试")).clicked() {
                            action = Some(UpdateAction::Retry);
                        }
                        if ui.button("关闭").clicked() {
                            action = Some(UpdateAction::Dismiss);
                        }
                    } else if let Some(info) = &available_info {
                        if ui.add(primary("下载并安装")).clicked() {
                            action = Some(UpdateAction::StartDownload(info.clone()));
                        }
                        if ui.button("稍后").clicked() {
                            action = Some(UpdateAction::Dismiss);
                        }
                        if let Some(url) = &url {
                            if ui.hyperlink_to("查看完整更新说明", url).clicked() {
                                action = Some(UpdateAction::Dismiss);
                            }
                        }
                    }
                });
            });

        match action {
            Some(UpdateAction::Dismiss) => self.update_state = UpdateState::Idle,
            Some(UpdateAction::StartDownload(info)) => self.start_download(info),
            Some(UpdateAction::CancelDownload) => {
                self.download_rx = None;
                self.update_state = UpdateState::Idle;
            }
            Some(UpdateAction::Install { dmg_path }) => self.install_update(ctx, dmg_path),
            Some(UpdateAction::Retry) => self.start_update_check(false),
            None => {}
        }
    }

    /// 渲染 Toast（右下角滑入，自动淡出）。
    fn render_toast(&mut self, ctx: &egui::Context) {
        let theme = crate::theme::current_theme();
        let Some(toast) = self.toast.as_mut() else {
            return;
        };
        let now = anim::now(ctx);
        if toast.start.is_nan() {
            toast.start = now;
        }
        let elapsed = now - toast.start;
        const DURATION: f64 = 4.0;
        if elapsed >= DURATION {
            self.toast = None;
            return;
        }
        // 动画期间持续重绘。
        ctx.request_repaint_after(Duration::from_millis(16));

        let in_t = (elapsed / 0.32).clamp(0.0, 1.0) as f32;
        let out_t = ((DURATION - elapsed) / 0.35).clamp(0.0, 1.0) as f32;
        let alpha = anim::ease_out_cubic(in_t) * anim::ease_out_cubic(out_t);
        let slide = (1.0 - anim::ease_out_back(in_t)) * 24.0;
        let accent = if toast.is_error {
            theme.danger
        } else {
            theme.success
        };

        let mut dismiss = false;
        egui::Area::new(egui::Id::new("toast"))
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-16.0, -16.0 + slide))
            .order(egui::Order::Foreground)
            .interactable(true)
            .show(ctx, |ui| {
                ui.set_opacity(alpha);
                let frame = egui::Frame::new()
                    .fill(theme.bg_elevated)
                    .corner_radius(9.0)
                    .inner_margin(egui::Margin::symmetric(13, 9))
                    .stroke(egui::Stroke::new(1.0, accent.gamma_multiply(0.45 * alpha)));
                let response = frame.show(ui, |ui| {
                    ui.horizontal(|ui| {
                        status_dot(ui, accent, false);
                        ui.label(
                            egui::RichText::new(&toast.message)
                                .size(12.5)
                                .color(theme.text_primary),
                        );
                    });
                });
                if ui
                    .interact(
                        response.response.rect,
                        egui::Id::new("toast_click"),
                        egui::Sense::click(),
                    )
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                {
                    dismiss = true;
                }
            });
        if dismiss {
            self.toast = None;
        }
    }

    /// 无标签页时的空状态。
    fn empty_state(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let theme = crate::theme::current_theme();
        ui.centered_and_justified(|ui| {
            ui.vertical_centered(|ui| {
                draw_logo_mark(ui, 48.0);
                ui.add_space(12.0);
                ui.label(
                    egui::RichText::new("kun")
                        .strong()
                        .size(22.0)
                        .color(theme.text_primary),
                );
                ui.add_space(2.0);
                ui.label(
                    egui::RichText::new("⌘T 新建本地终端 · ⌘N 新建连接")
                        .size(12.0)
                        .color(theme.text_muted),
                );
                ui.add_space(14.0);
                let btn = egui::Button::new(
                    egui::RichText::new("新建本地终端")
                        .color(crate::theme::tokens::ACCENT_FG)
                        .size(13.0),
                )
                .fill(theme.accent)
                .stroke(egui::Stroke::NONE)
                .corner_radius(crate::theme::tokens::RADIUS_SM);
                if ui.add(btn).clicked() {
                    self.new_local_tab(ctx);
                }
            });
        });
    }
}

// ==================== 绘制辅助 ====================

/// 新建连接对话框的输入框统一样式：圆角深色底、垂直居中、焦点 accent 边框。
///
/// TextEdit 默认 `Align2::LEFT_TOP`（单行输入框文字偏上），这里改为垂直居中；
/// 焦点时边框切换 accent 色高亮。
///
/// **egui 0.36 坑：提供自定义 frame 时 `.margin()` 被整体丢弃**
/// （`frame.unwrap_or_else(|| Frame::new().inner_margin(margin))`），
/// 而 `Frame::new()` 默认 `inner_margin` 为 ZERO——内边距必须挂在自定义 frame 上。
fn form_input(
    ui: &mut egui::Ui,
    id: egui::Id,
    value: &mut String,
    hint: &str,
    width: f32,
    password: bool,
) -> egui::Response {
    let theme = crate::theme::current_theme();
    let focused = ui.memory(|m| m.has_focus(id));
    let frame = egui::Frame::new()
        .fill(theme.bg_elevated)
        .stroke(egui::Stroke::new(
            if focused { 1.5 } else { 1.0 },
            if focused { theme.accent } else { theme.border },
        ))
        .corner_radius(crate::theme::tokens::RADIUS_SM)
        .inner_margin(egui::Margin::symmetric(10, 5));
    let mut edit = egui::TextEdit::singleline(value)
        .id(id)
        .hint_text(hint)
        .vertical_align(egui::Align::Center)
        .frame(frame)
        .text_color(theme.text_primary);
    if password {
        edit = edit.password(true);
    }
    ui.add_sized([width, 30.0], edit)
}

/// 品牌标记：紫金渐变圆底 + 白色粗体 "K" 字（居中）。
///
/// 与应用图标（scripts/make-icon.swift）同构图；hover 时带 accent 辉光。
fn draw_logo_mark(ui: &mut egui::Ui, size: f32) -> egui::Rect {
    let theme = crate::theme::current_theme();
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let center = rect.center();
        let radius = size * 0.40;
        // 圆底：accent → accent2 垂直渐变。
        anim::paint_rounded_gradient(
            painter,
            egui::Rect::from_center_size(center, egui::vec2(radius * 2.0, radius * 2.0)),
            radius,
            theme.accent,
            theme.accent2,
        );

        // K 字：白色粗体，居中。
        painter.text(
            center,
            egui::Align2::CENTER_CENTER,
            "K",
            egui::FontId::proportional(size * 0.46),
            egui::Color32::WHITE,
        );

        if response.hovered() {
            anim::paint_glow(painter, center, size * 0.9, theme.accent2);
        }
    }
    rect
}

/// 状态圆点（可选呼吸光圈）。
fn status_dot(ui: &mut egui::Ui, color: egui::Color32, pulse: bool) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(9.0, 9.0), egui::Sense::hover());
    let center = rect.center();
    ui.painter().circle_filled(center, 3.2, color);
    if pulse {
        let p = anim::pulse(ui.ctx(), 1.6);
        ui.painter().circle_stroke(
            center,
            4.0 + p * 2.4,
            egui::Stroke::new(1.0, color.gamma_multiply(0.8 - p * 0.8)),
        );
    }
}

/// 面板内的细分隔线。
fn hairline(ui: &mut egui::Ui, theme: &crate::theme::Theme) {
    let rect = ui.max_rect();
    ui.painter().hline(
        rect.left() + 6.0..=rect.right() - 6.0,
        rect.top(),
        egui::Stroke::new(1.0, theme.border),
    );
}

/// 自定义渐变进度条（未知总量时显示流动光带）。
fn progress_bar(ui: &mut egui::Ui, fraction: f32) {
    let theme = crate::theme::current_theme();
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 8.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 4.0, theme.bg_panel);
    let fill_w = if fraction.is_finite() {
        rect.width() * fraction.clamp(0.0, 1.0)
    } else {
        rect.width() * 0.28
    };
    let fill = egui::Rect::from_min_size(rect.min, egui::vec2(fill_w.max(4.0), rect.height()));
    if fill_w > 0.0 {
        anim::paint_rounded_gradient(
            ui.painter(),
            fill,
            fill.height() * 0.5,
            theme.accent,
            theme.accent2,
        );
        // 填充上的扫光。
        let phase = anim::sweep(ui.ctx(), 1.7);
        let band_w = fill.width() * 0.4;
        let band_x = fill.left() + (fill.width() - band_w).max(0.0) * phase;
        anim::paint_h_gradient(
            ui.painter(),
            egui::Rect::from_min_size(
                egui::pos2(band_x, fill.top()),
                egui::vec2(band_w.min(fill.width()), fill.height()),
            ),
            egui::Color32::from_white_alpha(0),
            egui::Color32::from_white_alpha(48),
        );
    }
}

/// 字节数友好显示。
fn fmt_bytes(n: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let n = n as f64;
    if n >= GB {
        format!("{:.1} GB", n / GB)
    } else if n >= MB {
        format!("{:.1} MB", n / MB)
    } else if n >= KB {
        format!("{:.1} KB", n / KB)
    } else {
        format!("{n:.0} B")
    }
}

/// 写安装脚本并启动（独立进程，应用退出后继续运行）。
fn launch_installer(dmg: &Path, mount: &Path) -> Result<(), String> {
    let dir = std::env::temp_dir().join("kun-update");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let script = dir.join("install.sh");
    std::fs::write(&script, INSTALL_SCRIPT).map_err(|e| e.to_string())?;
    Command::new("/bin/sh")
        .arg(&script)
        .arg(dmg)
        .arg(mount)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

impl eframe::App for KunApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // 缓存 ctx 供非 UI 回调使用（如复制 tab 时构造 Session）。
        self.last_ctx = ctx.clone();

        // ==================== 快捷键 ====================
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::N)) {
            self.show_new_conn = true;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::T)) {
            self.new_local_tab(&ctx);
        }
        // ⌘, 切换设置弹窗（macOS 标准"应用偏好设置"快捷键）。
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Comma)) {
            self.toggle_settings();
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
        ] {
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::ALT, key)) {
                crate::theme::set_theme(&ctx, theme_idx);
                self.show_toast(
                    format!("主题：{}", crate::theme::current_theme().name),
                    false,
                );
            }
        }
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

        // ==================== 处理异步结果 ====================
        self.poll_connection(&ctx);
        self.poll_sftp();
        self.poll_update();
        self.poll_download(&ctx);

        // ==================== 顶部标签页栏（合并了原 toolbar：齿轮入口在最右） ====================
        let theme = crate::theme::current_theme();
        // ==================== 标签页栏 ====================
        let tab_frame = egui::Frame::new()
            .fill(theme.bg_header)
            .inner_margin(egui::Margin {
                left: 8,
                right: 8,
                top: 2,
                bottom: 2,
            });
        egui::Panel::top("tabs").frame(tab_frame).show(ui, |ui| {
            self.tab_bar(ui);
        });

        // ==================== 状态栏 ====================
        let status_frame = egui::Frame::new()
            .fill(theme.bg_panel)
            .inner_margin(egui::Margin {
                left: 4,
                right: 10,
                top: 4,
                bottom: 4,
            });
        egui::Panel::bottom("status")
            .frame(status_frame)
            .show(ui, |ui| {
                self.status_bar(ui);
            });

        // ==================== 中央区：当前标签页 ====================
        // SFTP 面板必须是顶层面板（先于 CentralPanel 注册）：
        // egui 0.36 嵌套在 CentralPanel 内的 `Panel::right` 会把面板状态
        // 计入顶层布局，导致面板错位覆盖终端（表现为"SFTP 面板打不开"）。
        // tabby 形式：面板默认收起，只显示终端；终端右上角悬浮按钮切换
        // 展开/收起（`show_collapsible` 官方滑动动画，收起后右缘保留
        // 细拖拽把手，拖动也可重新打开）。
        let max_sftp_w = ui.ctx().viewport_rect().width() * 0.45;
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            if let Some(sftp) = &mut tab.sftp {
                let sftp_frame = egui::Frame::new()
                    .fill(theme.bg_panel)
                    .inner_margin(egui::Margin::symmetric(10, 8));
                egui::Panel::right("sftp_panel")
                    .default_size(340.0)
                    // 最小宽度保证面板始终可见可操作（此前可被拖到极窄）。
                    .min_size(260.0)
                    // 最宽限制（约窗口 45%）：防止拖宽挤压终端
                    // （曾拖到 ~70% 窗口宽把终端压成一条窄带）。
                    .max_size(max_sftp_w)
                    .resizable(true)
                    .frame(sftp_frame)
                    .show_collapsible(ui, &mut tab.sftp_open, |ui| sftp.show(ui));
            }
        }
        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(_pending) = &self.pending {
                ui.centered_and_justified(|ui| {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(
                            egui::RichText::new(format!("正在连接 {} …", self.pending_label))
                                .size(13.0)
                                .color(theme.text_secondary),
                        );
                    });
                });
            } else if self.tabs.is_empty() {
                self.empty_state(ui, &ctx);
            } else if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                tab.terminal.show(ui);
                // tabby 风格：远程标签页终端右上角悬浮 SFTP 开关按钮。
                if tab.sftp.is_some() && sftp_floating_button(ui, tab.sftp_open) {
                    tab.sftp_open = !tab.sftp_open;
                }
            }
        });

        // ==================== 对话框与 Toast ====================
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            if let Some(sftp) = &mut tab.sftp {
                sftp.show_dialog(&ctx);
            }
        }
        self.connect_dialog(&ctx);
        self.update_dialog(&ctx);
        if self.show_settings {
            self.settings_panel(&ctx);
        }
        self.render_toast(&ctx);

        // 安装完成 → 关闭应用（脚本会拉起新版本）。
        if matches!(self.update_state, UpdateState::Installed) {
            if let Some(t) = self.restart_at {
                if anim::now(&ctx) >= t {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                } else {
                    ctx.request_repaint_after(Duration::from_millis(50));
                }
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
        harness.run_steps(6);

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
        harness.run_steps(6);

        // 渲染完成，切换到私钥模式再渲染一帧。
        harness.get_by_label("私钥").click();
        harness.run_steps(6);
        harness.get_by_label("私钥路径");
    }
}

#[cfg(test)]
mod app_tests {
    use super::*;

    /// tabby 形式：远程标签页 SFTP 面板默认收起，仅显示终端；
    /// 终端右上角悬浮 SFTP 按钮点击切换面板开/关
    /// （回归测试：曾无条件显示右侧面板，宽度失控挤压终端成窄条）。
    #[test]
    fn sftp面板默认收起悬浮按钮切换() {
        use kittest::Queryable;

        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            let mut app = KunApp::new(cc);
            // 构造带 SFTP 会话的标签页（mock 通道，无需测试 sshd）。
            let session = Session::spawn_local(
                SessionOptions::default(),
                80,
                24,
                Arc::new(|_ev: &SessionEvent| {}),
            )
            .expect("创建本地终端失败");
            let mut tab = TerminalTab::new("测试主机".into(), TerminalView::new(session));
            let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let (handle_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
            tab.sftp = Some(SftpView::new(
                "测试主机",
                SftpHandle::from_raw(handle_tx),
                rx,
            ));
            app.tabs.push(Box::new(tab));
            app.active_tab = app.tabs.len() - 1;
            app
        });
        harness.run_steps(6);

        // 默认：面板收起（面板内控件不可见），悬浮按钮可见。
        assert!(
            harness.root().query_all_by_label("刷新").next().is_none(),
            "SFTP 面板应默认收起"
        );
        harness.get_by_label("SFTP").click();
        harness.run_steps(6);
        // 点击悬浮按钮 → 面板展开。
        harness.get_by_label("刷新");
        harness.get_by_label("上传");
        // 再点 → 收起。
        harness.get_by_label("SFTP").click();
        harness.run_steps(6);
        assert!(
            harness.root().query_all_by_label("刷新").next().is_none(),
            "再次点击应收起面板"
        );
    }

    /// 本地终端输入命令前缀 → 补全浮层出现；Esc 关闭；Tab 确认补全。
    #[test]
    fn 输入触发补全浮层() {
        use kittest::Queryable;

        let mut harness = egui_kittest::Harness::new_eframe(|cc| KunApp::new(cc));
        harness.run_steps(6);

        // 输入 "ca"（本地终端，初始即聚焦）。
        harness.event(egui::Event::Text("ca".into()));
        for _ in 0..6 {
            harness.step();
        }

        // 浮层应出现命令候选（cat 一定在 PATH）。
        let has_candidate = harness.root().query_all_by_label("cat").next().is_some();
        assert!(has_candidate, "输入 ca 后应出现补全浮层（cat 候选）");

        // Esc 关闭浮层。
        harness.event(egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            modifiers: egui::Modifiers::NONE,
            repeat: false,
            pressed: true,
        });
        for _ in 0..6 {
            harness.step();
        }
        assert!(
            harness.root().query_all_by_label("cat").next().is_none(),
            "Esc 应关闭补全浮层"
        );

        // 再次输入 "ca"，Tab 确认补全（浮层关闭）。
        // 先退格清空当前输入行（Esc 只关浮层不清输入；
        // Ctrl+C 在 kittest 事件模拟中可能被 egui 消费为复制命令）。
        for _ in 0..2 {
            harness.event(egui::Event::Key {
                key: egui::Key::Backspace,
                physical_key: None,
                modifiers: egui::Modifiers::NONE,
                repeat: false,
                pressed: true,
            });
        }
        for _ in 0..3 {
            harness.step();
        }
        harness.event(egui::Event::Text("ca".into()));
        for _ in 0..6 {
            harness.step();
        }
        assert!(
            harness.root().query_all_by_label("cat").next().is_some(),
            "重新输入后浮层应再次出现"
        );
        harness.event(egui::Event::Key {
            key: egui::Key::Tab,
            physical_key: None,
            modifiers: egui::Modifiers::NONE,
            repeat: false,
            pressed: true,
        });
        for _ in 0..6 {
            harness.step();
        }
        assert!(
            harness.root().query_all_by_label("cat").next().is_none(),
            "Tab 确认补全后浮层应关闭"
        );
    }

    /// 读当前标签页终端可见文本（kittest 断言用）。
    fn app_grid_text(app: &KunApp) -> String {
        use alacritty_terminal::term::cell::Flags;
        let tab = app.tabs.get(app.active_tab).expect("无标签页");
        let term_arc = tab.terminal.session().term();
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

    /// 完整应用（工具栏/标签栏/悬浮按钮共存）下回车应执行命令
    /// （回归测试：用户报告英文输入法下命令能输入但回车不执行）。
    #[test]
    fn 完整应用回车执行命令() {
        use std::time::{Duration, Instant};

        let mut harness = egui_kittest::Harness::new_eframe(|cc| KunApp::new(cc));
        harness.run_steps(6);

        // 等待 zsh 就绪（提示符出现）。
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut ready = false;
        while Instant::now() < deadline {
            harness.step();
            if app_grid_text(harness.state()).contains('➜') {
                ready = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        assert!(
            ready,
            "zsh 未就绪，终端内容：\n{}",
            app_grid_text(harness.state())
        );

        // 输入 echo hello。
        harness.event(egui::Event::Text("echo hello".into()));
        for _ in 0..6 {
            harness.step();
        }

        // 按回车。
        harness.event(egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            modifiers: egui::Modifiers::NONE,
            repeat: false,
            pressed: true,
        });

        // 等待 hello 输出出现（命令被执行）。
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut executed = false;
        while Instant::now() < deadline {
            harness.step();
            if app_grid_text(harness.state()).contains("hello") {
                executed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        assert!(
            executed,
            "回车未执行命令，终端内容：\n{}",
            app_grid_text(harness.state())
        );
    }

    /// 新建连接表单默认值：用户名 root、端口 22（可修改）。
    #[test]
    fn 表单默认root与22端口() {
        let form = ConnectForm::default();
        assert_eq!(form.user, "root");
        assert_eq!(form.port, "22");
        assert!(form.name.is_empty());
        assert!(form.host.is_empty());
        assert_eq!(form.port.trim().parse::<u16>().unwrap(), 22);
    }

    /// 完整应用：⌘, 打开设置弹窗后点"新建连接"按钮打开对话框，不应崩溃。
    #[test]
    fn 点击新建连接不崩溃() {
        use kittest::Queryable;

        let mut harness = egui_kittest::Harness::new_eframe(|cc| KunApp::new(cc));
        harness.run_steps(6);
        // ⌘, 打开设置弹窗（含"主机管理"分组与"新建连接"按钮）。
        harness.event(egui::Event::Key {
            key: egui::Key::Comma,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        for _ in 0..3 {
            harness.step();
        }

        harness.get_by_label("新建连接").click();
        for _ in 0..8 {
            harness.step();
        }

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

    fn sshd_available() -> bool {
        use std::net::TcpStream;
        use std::time::Duration;
        TcpStream::connect_timeout(
            &"127.0.0.1:2222".parse().unwrap(),
            Duration::from_millis(500),
        )
        .is_ok()
    }

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

    /// 端到端：点击侧栏主机条目 → 远程终端 + SFTP 面板出现。
    #[test]
    fn 点击连接建立远程会话() {
        use kittest::Queryable;
        use std::time::{Duration, Instant};

        if !sshd_available() {
            eprintln!("跳过：测试 sshd 未运行（scripts/test-sshd.sh start）");
            return;
        }

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

        let mut harness = egui_kittest::Harness::builder()
            .with_step_dt(1.0 / 60.0)
            .build_eframe(|cc| KunApp::new(cc));
        harness.run_steps(6);
        // 主机行从设置弹窗里取：⌘, 打开设置弹窗。
        harness.event(egui::Event::Key {
            key: egui::Key::Comma,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        for _ in 0..3 {
            harness.step();
        }

        // 双击设置 tab 内的主机行（自实现 0.3s 双击检测）。
        {
            let host_row = harness.get_by_label("链路测试");
            host_row.click();
        }
        harness.step();
        {
            let host_row = harness.get_by_label("链路测试");
            host_row.click();
        }

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
        std::fs::remove_file(&config_path).ok();

        assert!(connected, "点击连接后未出现 SFTP 面板（连接失败或崩溃）");
        // 连接成功后设置弹窗仍打开，关闭弹窗（⌘, toggle）让 SFTP 浮按钮可点。
        harness.event(egui::Event::Key {
            key: egui::Key::Comma,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        for _ in 0..3 {
            harness.step();
        }
        // SFTP 面板默认收起，需点击终端右上角悬浮按钮展开。
        harness.get_by_label("SFTP").click();
        for _ in 0..6 {
            harness.step();
        }
        harness.get_by_label("上传");
        harness.get_by_label("刷新");
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;

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

        let mut harness = egui_kittest::Harness::builder()
            .with_step_dt(1.0 / 60.0)
            .build_eframe(|cc| KunApp::new(cc));
        harness.run_steps(6);
        // 主机行从设置弹窗里取：⌘, 打开设置弹窗。
        harness.event(egui::Event::Key {
            key: egui::Key::Comma,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        for _ in 0..3 {
            harness.step();
        }
        {
            let host_row = harness.get_by_label("链路测试");
            host_row.click();
        }
        harness.step();
        {
            let host_row = harness.get_by_label("链路测试");
            host_row.click();
        }

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
            config.save(&config_path).ok();
            harness = egui_kittest::Harness::builder()
                .with_step_dt(1.0 / 60.0)
                .build_eframe(|cc| KunApp::new(cc));
            harness.run_steps(6);
            // 主机行从设置弹窗里取：⌘, 打开设置弹窗。
            harness.event(egui::Event::Key {
                key: egui::Key::Comma,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::COMMAND,
            });
            for _ in 0..3 {
                harness.step();
            }
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

        // 连接成功后设置弹窗仍打开，关闭弹窗（⌘, toggle）让 SFTP 浮按钮可点。
        harness.event(egui::Event::Key {
            key: egui::Key::Comma,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        for _ in 0..3 {
            harness.step();
        }
        // SFTP 面板默认收起，需点击终端右上角悬浮按钮展开。
        harness.get_by_label("SFTP").click();
        for _ in 0..6 {
            harness.step();
        }
        // 布局断言：SFTP 面板应位于窗口右侧（终端 + 面板 + 侧栏三段式）。
        let up = harness.get_by_label("上传");
        let r = up.rect();
        assert!(
            r.left() > 400.0,
            "SFTP 面板应位于窗口右半侧，实际上传按钮 x={}",
            r.left()
        );
        // 设置入口齿轮按钮应在标签栏最右侧（> 700 视口），用 Button role 查找
        // （齿轮纯图标无文字 label）。取所有 button 中 right 最大的。
        let gear = harness
            .root()
            .query_all_by_role(accesskit::Role::Button)
            .max_by(|a, b| a.rect().right().partial_cmp(&b.rect().right()).unwrap())
            .expect("应找到按钮");
        assert!(
            gear.rect().right() > 700.0,
            "齿轮按钮应在标签栏最右侧（视口宽 800），实际 right={}",
            gear.rect().right()
        );
        // 多跑几帧让面板状态稳定后再截图（kittest 渲染器对首帧 shapes 输出有延迟）。
        for _ in 0..30 {
            harness.step();
        }

        let img = harness.render().expect("渲染失败");
        let out = "/tmp/kun_style_sftp.png";
        img.save(out).expect("保存截图失败");
        eprintln!("样式截图已保存：{out}");
    }
}

#[cfg(test)]
mod theme_tests {
    use super::*;

    /// 三套主题切换并渲染截图（视觉验证用）。
    #[test]
    fn 三套主题渲染截图() {
        use kittest::Queryable;

        let mut harness = egui_kittest::Harness::new_eframe(|cc| KunApp::new(cc));
        harness.run_steps(6);
        // 主题下拉现在在设置弹窗里，先用 ⌘, 打开弹窗。
        harness.event(egui::Event::Key {
            key: egui::Key::Comma,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        for _ in 0..3 {
            harness.step();
        }

        for theme_name in ["深色", "深蓝", "霓虹"] {
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
            let img = harness.render().expect("渲染失败");
            let out = format!("/tmp/kun_theme_{theme_name}.png");
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
        harness.run_steps(6);
        assert_eq!(
            harness.query_all_by_label("×").count(),
            1,
            "初始应有一个标签页"
        );

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
        harness.run_steps(6);

        // 启动时：[本地终端]（设置改弹窗，不再是 tab）→ 1 个 ×。
        assert_eq!(
            harness.query_all_by_label("×").count(),
            1,
            "启动时只有 1 个本地终端标签"
        );

        // ⌘T 新建本地终端 → [本地终端, 新的本地终端] → 2 个 ×。
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
            "⌘T 后应有 2 个 × 按钮"
        );

        // ⌘W 关闭当前 → [本地终端] → 1 个 ×。
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
            "⌘W 后应回到 1 个 × 按钮"
        );

        // 再次 ⌘W → [] → 0 个 ×。
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
            "全部关闭后应无 × 按钮"
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
        // 必须注入 TERM：GUI 启动继承 TERM=dumb 会致删除回显异常/回车不执行（回归测试）。
        assert_eq!(
            opts.env.get("TERM").map(String::as_str),
            Some("xterm-256color"),
            "本地会话必须注入 TERM=xterm-256color（避免继承 TERM=dumb）"
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
mod settings_tests {
    use super::*;

    /// 设置弹窗默认关闭，tabs 不含设置 tab（设置改弹窗后无 Tab::Settings 概念）。
    #[test]
    fn 设置弹窗默认关闭() {
        let mut harness = egui_kittest::Harness::new_eframe(|cc| KunApp::new(cc));
        harness.run_steps(6);
        let app = harness.state();
        assert!(!app.show_settings, "启动时设置弹窗应默认关闭");
        assert_eq!(
            app.active_tab, 0,
            "默认激活第一个 tab（本地终端），不打扰用户"
        );
        // 启动时"主机管理"分组不应渲染。
        use kittest::Queryable;
        assert!(
            harness
                .root()
                .query_all_by_label("主机管理")
                .next()
                .is_none(),
            "设置弹窗未打开时不应渲染主机管理"
        );
    }

    /// ⌘, 快捷键切换设置弹窗（macOS 标准"应用偏好设置"）。
    #[test]
    fn 快捷键打开设置() {
        use kittest::Queryable;
        let mut harness = egui_kittest::Harness::new_eframe(|cc| KunApp::new(cc));
        harness.run_steps(6);
        assert!(!harness.state().show_settings, "默认关闭");

        harness.event(egui::Event::Key {
            key: egui::Key::Comma,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        for _ in 0..3 {
            harness.step();
        }
        // 打开后渲染"主机管理"分组。
        assert!(harness.state().show_settings, "⌘, 应打开设置弹窗");
        harness.get_by_label("主机管理");

        // 再按 ⌘, 关闭。
        harness.event(egui::Event::Key {
            key: egui::Key::Comma,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        for _ in 0..3 {
            harness.step();
        }
        assert!(!harness.state().show_settings, "⌘, 应再次关闭设置弹窗");
    }

    /// 标签栏齿轮按钮存在且在标签栏最右侧（> 200 px）。
    /// 齿轮纯图标无文字 label（`on_hover_text` 不被 kittest 识别为 label），
    /// 通过 `by_role(Button)` 查找；点击验证 ⌘, 等价路径。
    #[test]
    fn 齿轮存在并能打开设置() {
        use kittest::Queryable;
        let mut harness = egui_kittest::Harness::new_eframe(|cc| KunApp::new(cc));
        harness.run_steps(6);
        // 齿轮是标签栏最右侧的 Button（视口宽 800，齿轮 right > 700）。
        let gear = harness
            .root()
            .query_all_by_role(accesskit::Role::Button)
            .max_by(|a, b| a.rect().right().partial_cmp(&b.rect().right()).unwrap())
            .expect("应找到按钮");
        assert!(
            gear.rect().right() > 700.0,
            "齿轮按钮应在标签栏最右侧（> 700），实际 right={}",
            gear.rect().right()
        );
        gear.click();
        for _ in 0..3 {
            harness.step();
        }
        assert!(harness.state().show_settings, "齿轮点击应打开设置弹窗");
        harness.get_by_label("主机管理");
    }
}
