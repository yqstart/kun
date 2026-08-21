//! Mino 应用主体：布局、连接管理与状态。
//!
//! 视觉参照 Warp：分层深色背景、品牌紫青渐变、圆角幽灵按钮、
//! 标签页底部指示条与扫光动效（动效细节见 `crate::anim`）。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use eframe::egui;
use mino_core::config::{Auth, HostConfig, HostProfile};
use mino_core::ssh::sftp::{connect_sftp, SftpEvent, SftpHandle};
use mino_core::ssh::{connect_remote, ConnectResult};
use mino_core::terminal::{Session, SessionEvent, SessionOptions};
use mino_core::updater::{check_for_update, UpdateInfo};
use tokio::sync::mpsc::{Receiver, UnboundedReceiver};

use crate::anim;
use crate::views::sftp_view::SftpView;
use crate::views::terminal_view::TerminalView;

/// 应用对外展示名称。
pub const PRODUCT_NAME: &str = "Mino";

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
    /// 标签稳定身份，不随 Vec 中的插入、删除或移动变化。
    id: u64,
    label: String,
    terminal: TerminalView,
    sftp: Option<SftpView>,
    /// SFTP 面板是否展开（tabby 形式：默认收起，终端右上角悬浮按钮切换）。
    sftp_open: bool,
}

impl TerminalTab {
    fn new(id: u64, label: String, terminal: TerminalView) -> Self {
        Self {
            id,
            label,
            terminal,
            sftp: None,
            sftp_open: false,
        }
    }

    /// 标签显示标题（跟随会话标题变化；读缓存避免每帧 Mutex + clone）。
    fn title(&self) -> String {
        let t = self.terminal.session_title();
        if t.is_empty() {
            self.label.clone()
        } else {
            t.to_string()
        }
    }
}

/// 一次 SSH/SFTP 连接的身份与状态。
struct SftpConnection {
    connection_id: u64,
    handle: SftpHandle,
    rx: Receiver<SftpEvent>,
    host: String,
    home: Option<String>,
}

/// 标签页：普通终端（含本地/远程）。设置改为独立弹窗（`show_settings`），
/// 不再作为 tab。
///
/// `Vec<Box<TerminalTab>>` 用 Box 包裹：TerminalTab 体积大，
/// Box 避免 Vec 各槽位按最大元素对齐造成内存浪费
/// （clippy `large_enum_variant` 等价警告——`Tab` enum 之前也是用 Box）。
pub type Tab = Box<TerminalTab>;

/// 应用状态。
pub struct MinoApp {
    tabs: Vec<Tab>,
    active_tab: usize,
    /// 连接成功后创建的标签稳定身份（用于挂载 SFTP）。
    pending_tab: Option<u64>,
    /// 当前等待 SSH 结果的连接身份。
    pending_connection_id: Option<u64>,
    /// 标签与连接身份分配器。
    next_id: u64,
    /// 主机行最近一次点击（时间, 行索引），自实现双击检测。
    last_row_click: Option<(f64, usize)>,
    /// 设置中的当前主机焦点（单击后保持，双击连接）。
    selected_host: Option<usize>,
    /// 设置弹窗是否打开（`⌘,` 或齿轮按钮切换；Esc/× 关闭）。
    show_settings: bool,
    /// 最近一帧的 egui::Context（`new_local_tab` 等非 UI 闭包内构造时使用）。
    last_ctx: egui::Context,
    config: HostConfig,
    config_path: PathBuf,
    /// 原配置无法读取且备份也失败时，禁止用空配置覆盖原文件。
    config_write_blocked: bool,
    show_new_conn: bool,
    form: ConnectForm,
    pending: Option<UnboundedReceiver<ConnectResult>>,
    pending_label: String,
    toast: Option<Toast>,
    /// 进行中的 SFTP 连接（句柄 + 事件流 + 主机名——主机名随连接绑定，
    /// 多连接并发时不会串到别的标签页上）。
    pending_sftp: Option<SftpConnection>,
    /// SFTP 已就绪但 SSH 标签尚未创建，等待挂载。
    ready_sftp: Option<SftpConnection>,
    /// SFTP 连接错误（状态栏持久显示，toast 易被忽略）。
    sftp_error: Option<String>,
    update_state: UpdateState,
    update_rx: Option<std::sync::mpsc::Receiver<Result<Option<UpdateInfo>, String>>>,
    download_rx: Option<std::sync::mpsc::Receiver<DownloadEvent>>,
    /// 当前下载的取消标记；取消动作会通知后台线程停止读取并清理临时文件。
    download_cancel: Option<Arc<AtomicBool>>,
    /// 当前下载的独立临时路径，用于取消时立即清理。
    download_path: Option<PathBuf>,
    /// 下载路径序号，避免重试与旧线程共享同一文件。
    download_sequence: u64,
    /// 本次检查是否为用户手动触发（决定是否弹提示）。
    manual_update: bool,
    /// 安装脚本已启动，到该时间点关闭应用重启。
    restart_at: Option<f64>,
    /// 安装脚本状态文件路径（原子写入后由 UI 轮询）。
    install_result_path: Option<PathBuf>,
    /// 安装脚本启动时间，用于检测脚本无响应。
    install_started_at: Option<f64>,
    /// 性能 HUD 是否显示（`⌥P` 切换；默认关闭，不影响测试截图）。
    show_perf_hud: bool,
    /// 帧耗时统计（UI 线程打点）。
    perf: crate::perf::PerfStats,
}

/// 本地终端会话选项：默认工作目录为 home，注入 TERM 与颜色环境变量。
fn local_session_options() -> SessionOptions {
    SessionOptions {
        working_directory: std::env::var("HOME").ok().map(PathBuf::from),
        // TERM 必须显式注入：从 GUI/Finder/Dock 启动的进程继承 `TERM=dumb`，
        // alacritty 的 `setup_env()` 只在其主应用入口调用，mino 未调用 →
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

/// 标签栏最右侧设置图标。22×22 纯图标按钮，无 unicode 齿轮字形依赖。
///
/// 用矢量圆环与八根短齿绘制，跨平台字体不会出现方框，视觉上也更贴近
/// Mino 的控制台/仪表盘语气。
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
    let rect = response.rect;
    if ui.is_rect_visible(rect) {
        if response.hovered() {
            // hover：白色 8% 圆角底（与全局控件 hover 一致）。
            ui.painter().rect_filled(
                rect,
                crate::theme::tokens::RADIUS_ITEM,
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 18),
            );
        }
        let icon_color = if response.hovered() {
            theme.accent
        } else {
            theme.text_secondary
        };
        let center = rect.center();
        let core = theme.bg_panel;
        for i in 0..8 {
            let angle = i as f32 * std::f32::consts::TAU / 8.0;
            let dir = egui::vec2(angle.cos(), angle.sin());
            ui.painter().line_segment(
                [center + dir * 5.2, center + dir * 7.0],
                egui::Stroke::new(1.5, icon_color),
            );
        }
        ui.painter()
            .circle_stroke(center, 5.0, egui::Stroke::new(1.5, icon_color));
        ui.painter().circle_filled(center, 2.0, core);
    }
    response.clicked()
}

/// 标签栏快速 SSH 连接按钮。22×22 纯图标按钮（">_" 终端符号，业界通用的
/// "命令行/终端"标识），风格与齿轮一致：次要色、hover 白 8% 圆角底。
/// 点击弹出已保存主机列表（`Popup::menu` 自行管理开关状态，Id 需稳定）。
fn ssh_quick_button(ui: &mut egui::Ui) -> egui::Response {
    let theme = crate::theme::current_theme();
    let btn_size = 22.0;
    let btn = egui::Button::new(
        egui::RichText::new(">_")
            .monospace()
            .size(11.0)
            .color(theme.text_secondary),
    )
    .fill(egui::Color32::TRANSPARENT)
    .stroke(egui::Stroke::NONE)
    .min_size(egui::vec2(btn_size, btn_size));
    let response = ui
        .add(btn)
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("快速连接已保存主机");
    if response.hovered() && ui.is_rect_visible(response.rect) {
        // hover：白色 8% 圆角底（与全局控件 hover 一致）。
        ui.painter().rect_filled(
            response.rect,
            crate::theme::tokens::RADIUS_ITEM,
            egui::Color32::from_rgba_unmultiplied(255, 255, 255, 18),
        );
    }
    response
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
fn temp_dmg_path(file_name: &str, sequence: u64) -> PathBuf {
    std::env::temp_dir()
        .join("mino-update")
        .join(format!("{file_name}.{sequence}.part"))
}

/// 安装脚本：挂载 dmg → 与应用握手 → 等待主程序退出 → 替换 .app → 重启。
/// 优先安装到 /Applications，失败回退 ~/Applications。第三个参数是状态文件，
/// 使用临时文件 + mv 原子写入，避免 UI 读到半行状态。
const INSTALL_SCRIPT: &str = r#"#!/bin/sh
set -u
DMG="$1"
MOUNT="$2"
RESULT="$3"
FINAL=""
RESULT_TMP="$RESULT.$$"
write_result() {
  printf '%s\n' "$1" > "$RESULT_TMP" 2>/dev/null || return 0
  mv -f "$RESULT_TMP" "$RESULT" 2>/dev/null || true
}
fail() {
  write_result "error:$1"
  exit 1
}
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
mkdir -p "$MOUNT" || fail "创建挂载目录失败"
hdiutil attach "$DMG" -nobrowse -readonly -mountpoint "$MOUNT" >/dev/null 2>&1 || fail "挂载 DMG 失败"
SRC="$MOUNT/Mino.app"
[ -d "$SRC" ] || { hdiutil detach "$MOUNT" -quiet >/dev/null 2>&1 || true; fail "DMG 中未找到 Mino.app"; }
# 先通知应用已完成挂载和源文件校验；应用收到后退出，脚本再替换正在运行的旧版本。
write_result "ready"
i=0
while [ $i -lt 50 ]; do
  if ! pgrep -x mino-app >/dev/null 2>&1; then break; fi
  sleep 0.2
  i=$((i+1))
done
if pgrep -x mino-app >/dev/null 2>&1; then
  hdiutil detach "$MOUNT" -quiet >/dev/null 2>&1 || true
  fail "等待旧版本退出超时"
fi
install "/Applications/Mino.app" || install "$HOME/Applications/Mino.app" || { hdiutil detach "$MOUNT" -quiet >/dev/null 2>&1 || true; fail "替换应用失败"; }
hdiutil detach "$MOUNT" -quiet >/dev/null 2>&1 || true
rmdir "$MOUNT" 2>/dev/null || true
rm -f "$DMG" 2>/dev/null
open "$FINAL" || fail "启动新版本失败"
write_result "success"
"#;

impl MinoApp {
    /// 创建应用（启动本地终端会话）。
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        #[cfg(test)]
        {
            return Self::new_with_config_inner(cc, test_config_path("default"), false);
        }
        #[cfg(not(test))]
        {
            Self::new_with_config(cc, mino_core::config::default_config_path())
        }
    }

    /// 指定配置文件路径创建应用。
    ///
    /// 测试必须走这里传入隔离路径——曾发生测试直接读写并删除用户真实的
    /// `~/.config/mino/hosts.toml`（default_config_path），运行一次测试
    /// 主机列表就丢一次（表现为"更新后主机全部消失"）。
    pub fn new_with_config(cc: &eframe::CreationContext<'_>, config_path: PathBuf) -> Self {
        Self::new_with_config_inner(cc, config_path, cfg!(not(test)))
    }

    /// 构造应用的内部实现；测试构建关闭自动更新，避免网络与后台线程污染 UI 测试。
    fn new_with_config_inner(
        cc: &eframe::CreationContext<'_>,
        config_path: PathBuf,
        auto_update: bool,
    ) -> Self {
        crate::theme::set_theme(&cc.egui_ctx, 0);
        let ctx = cc.egui_ctx.clone();

        // 加载失败不能静默按空配置启动：文件存在但解析失败时先备份原文，
        // 避免后续保存把用户主机列表覆盖掉。
        let (config, load_message, config_write_blocked) = match HostConfig::load(&config_path) {
            Ok(c) => (c, None, false),
            Err(e) => {
                let existed = config_path.exists();
                if !existed && e.kind() == std::io::ErrorKind::NotFound {
                    (HostConfig::default(), None, false)
                } else if existed {
                    let bak = config_path.with_extension("toml.bak");
                    match std::fs::copy(&config_path, &bak) {
                        Ok(_) => {
                            log::error!("主机配置加载失败（原文已备份为 {bak:?}）：{e}");
                            (
                                HostConfig::default(),
                                Some(format!("主机配置读取失败，原文已备份为 {}", bak.display())),
                                false,
                            )
                        }
                        Err(be) => {
                            log::error!("备份主机配置到 {bak:?} 失败：{be}");
                            log::error!("主机配置加载失败，已禁止覆盖原文件：{e}");
                            (
                                HostConfig::default(),
                                Some(format!(
                                    "主机配置读取失败且备份失败，已禁止覆盖原文件：{be}"
                                )),
                                true,
                            )
                        }
                    }
                } else {
                    log::error!("主机配置加载失败，已禁止覆盖原文件：{e}");
                    (
                        HostConfig::default(),
                        Some(format!("主机配置读取失败，已禁止覆盖原文件：{e}")),
                        true,
                    )
                }
            }
        };

        let mut app = Self {
            tabs: Vec::new(),
            active_tab: 0,
            pending_tab: None,
            pending_connection_id: None,
            next_id: 1,
            last_row_click: None,
            selected_host: None,
            show_settings: false,
            last_ctx: cc.egui_ctx.clone(),
            config,
            config_path,
            config_write_blocked,
            show_new_conn: false,
            form: ConnectForm::default(),
            pending: None,
            pending_label: String::new(),
            toast: None,
            pending_sftp: None,
            ready_sftp: None,
            sftp_error: None,
            update_state: UpdateState::Idle,
            update_rx: None,
            download_rx: None,
            download_cancel: None,
            download_path: None,
            download_sequence: 0,
            manual_update: false,
            restart_at: None,
            install_result_path: None,
            install_started_at: None,
            show_perf_hud: false,
            perf: crate::perf::PerfStats::new(),
        };
        // 启动时自动检查更新（后台线程，延迟 3 秒，静默）。
        if let Some(message) = load_message {
            app.show_toast(message, true);
        }
        if auto_update {
            app.start_update_check(true, &ctx);
        }
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
                let id = self.allocate_id();
                let tab = Box::new(TerminalTab::new(
                    id,
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
        let tab_id = self.tabs[index].id;
        if self.pending_tab == Some(tab_id) {
            self.pending_tab = None;
            self.close_ready_sftp(tab_id);
        }
        if self
            .pending_sftp
            .as_ref()
            .is_some_and(|connection| connection.connection_id == tab_id)
        {
            self.close_pending_sftp(tab_id);
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

    /// 分配不会随标签列表变化的身份。
    fn allocate_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }

    /// 关闭指定连接的待处理 SFTP 会话。
    fn close_pending_sftp(&mut self, connection_id: u64) {
        if self
            .pending_sftp
            .as_ref()
            .is_some_and(|connection| connection.connection_id == connection_id)
        {
            if let Some(connection) = self.pending_sftp.take() {
                connection.handle.close();
            }
        }
    }

    /// 关闭指定连接的已就绪但尚未挂载的 SFTP 会话。
    fn close_ready_sftp(&mut self, connection_id: u64) {
        if self
            .ready_sftp
            .as_ref()
            .is_some_and(|connection| connection.connection_id == connection_id)
        {
            if let Some(connection) = self.ready_sftp.take() {
                connection.handle.close();
            }
        }
    }

    /// 启动后台更新检查（delay=true 时延迟 3 秒，避免影响启动）。
    fn start_update_check(&mut self, delay: bool, ctx: &egui::Context) {
        let (tx, rx) = std::sync::mpsc::channel();
        let current = env!("CARGO_PKG_VERSION").to_string();
        let arch = macos_arch().to_string();
        let repaint_ctx = ctx.clone();
        std::thread::spawn(move || {
            if delay {
                std::thread::sleep(Duration::from_secs(3));
            }
            let result = check_for_update(&current, mino_core::updater::DEFAULT_REPO, &arch);
            let _ = tx.send(result);
            repaint_ctx.request_repaint();
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
    fn start_download(&mut self, info: UpdateInfo, ctx: &egui::Context) {
        self.cancel_download();
        let (tx, rx) = std::sync::mpsc::channel();
        let url = info.asset_url.clone();
        let sequence = self.download_sequence;
        self.download_sequence = self.download_sequence.wrapping_add(1);
        let dest = temp_dmg_path(&info.asset_name, sequence);
        let cancel = Arc::new(AtomicBool::new(false));
        let thread_cancel = cancel.clone();
        let thread_dest = dest.clone();
        let repaint_ctx = ctx.clone();
        std::thread::spawn(move || {
            let mut last_repaint = Instant::now() - Duration::from_secs(1);
            let result = mino_core::updater::download_asset_with_cancel(
                &url,
                &thread_dest,
                &thread_cancel,
                |done, total| {
                    let _ = tx.send(DownloadEvent::Progress {
                        downloaded: done,
                        total,
                    });
                    if last_repaint.elapsed() >= Duration::from_millis(50) {
                        repaint_ctx.request_repaint();
                        last_repaint = Instant::now();
                    }
                },
            );
            if thread_cancel.load(Ordering::Relaxed) {
                let _ = std::fs::remove_file(&thread_dest);
                return;
            }
            repaint_ctx.request_repaint();
            match result {
                Ok(()) => {
                    let _ = tx.send(DownloadEvent::Done(thread_dest));
                }
                Err(e) => {
                    let _ = tx.send(DownloadEvent::Error(e));
                }
            }
        });
        self.download_rx = Some(rx);
        self.download_cancel = Some(cancel);
        self.download_path = Some(dest);
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
                    self.download_cancel = None;
                    self.download_path = None;
                    self.update_state = UpdateState::Downloaded {
                        info,
                        dmg_path: path,
                    };
                }
                DownloadEvent::Error(e) => {
                    self.download_rx = None;
                    self.download_cancel = None;
                    self.download_path = None;
                    self.update_state = UpdateState::Error(e);
                }
            }
        }
    }

    /// 取消下载并清理当前临时文件。
    fn cancel_download(&mut self) {
        if let Some(cancel) = self.download_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        if let Some(path) = self.download_path.take() {
            let _ = std::fs::remove_file(path);
        }
        self.download_rx = None;
    }

    /// 启动安装脚本并安排重启。
    fn install_update(&mut self, ctx: &egui::Context, dmg_path: PathBuf) {
        let UpdateState::Downloaded { info, .. } = &self.update_state else {
            return;
        };
        let info = info.clone();
        let dir = std::env::temp_dir().join("mino-update");
        let sequence = self.download_sequence;
        self.download_sequence = self.download_sequence.wrapping_add(1);
        let mount = dir.join(format!("mount-{}-{sequence}", std::process::id()));
        let result_path = dir.join(format!(
            "install-result-{}-{sequence}.txt",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&result_path);
        match launch_installer(&dmg_path, &mount, &result_path) {
            Ok(()) => {
                self.update_state = UpdateState::Installing(info);
                self.install_result_path = Some(result_path);
                self.install_started_at = Some(anim::now(ctx));
                self.restart_at = None;
                ctx.request_repaint_after(Duration::from_millis(100));
            }
            Err(e) => {
                self.update_state = UpdateState::Error(format!("启动安装脚本失败：{e}"));
            }
        }
    }

    /// 轮询安装脚本状态；仅收到握手或最终成功状态后才安排退出。
    fn poll_install(&mut self, ctx: &egui::Context) {
        if !matches!(self.update_state, UpdateState::Installing(_)) {
            return;
        }
        let Some(result_path) = self.install_result_path.clone() else {
            self.update_state = UpdateState::Error("安装脚本缺少状态文件".into());
            return;
        };
        let status = match std::fs::read_to_string(result_path) {
            Ok(status) => status.trim().to_string(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => {
                self.update_state = UpdateState::Error(format!("读取安装状态失败：{e}"));
                self.install_result_path = None;
                self.install_started_at = None;
                return;
            }
        };
        if status == "ready" || status == "success" {
            // ready 表示脚本已完成挂载与校验，应用退出后脚本才会继续替换旧版本。
            self.update_state = UpdateState::Installed;
            self.restart_at = Some(anim::now(ctx) + 0.9);
            self.install_started_at = None;
            ctx.request_repaint_after(Duration::from_millis(50));
            return;
        }
        if let Some(message) = status.strip_prefix("error:") {
            self.update_state = UpdateState::Error(format!("安装失败：{message}"));
            self.install_result_path = None;
            self.install_started_at = None;
            return;
        }
        if self
            .install_started_at
            .is_some_and(|started| anim::now(ctx) - started > 120.0)
        {
            self.update_state = UpdateState::Error("安装脚本 120 秒内未返回状态".into());
            self.install_result_path = None;
            self.install_started_at = None;
            return;
        }
        ctx.request_repaint_after(Duration::from_millis(100));
    }

    /// 保存主机配置到磁盘，并把失败反馈给用户。
    fn save_config(&mut self) -> bool {
        if self.config_write_blocked {
            self.show_toast("主机配置保存已阻止：原文件未能备份", true);
            return false;
        }
        match self.config.save(&self.config_path) {
            Ok(()) => true,
            Err(e) => {
                log::warn!("保存主机配置失败：{e}");
                self.show_toast(format!("保存主机配置失败：{e}"), true);
                false
            }
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
                    let connection_id = self
                        .pending_connection_id
                        .take()
                        .unwrap_or_else(|| self.allocate_id());
                    let view = TerminalView::new(session);
                    self.tabs.push(Box::new(TerminalTab::new(
                        connection_id,
                        self.pending_label.clone(),
                        view,
                    )));
                    self.active_tab = self.tabs.len() - 1;
                    self.pending_tab = Some(connection_id);
                    self.mount_ready_sftp();
                    self.show_toast(format!("已连接到 {}", self.pending_label), false);
                    ctx.request_repaint();
                }
                ConnectResult::Failed(e) => {
                    self.pending_connection_id = None;
                    if let Some(connection) = self.pending_sftp.take() {
                        connection.handle.close();
                    }
                    if let Some(connection) = self.ready_sftp.take() {
                        connection.handle.close();
                    }
                    log::error!("连接失败：{e}");
                    self.show_toast(format!("连接失败：{e}"), true);
                    ctx.request_repaint();
                }
            }
        }
    }

    /// 发起远程连接（同时启动 SFTP 连接）。
    fn start_connect(&mut self, ctx: &egui::Context, profile: HostProfile) {
        if let Some(connection) = self.pending_sftp.take() {
            connection.handle.close();
        }
        if let Some(connection) = self.ready_sftp.take() {
            connection.handle.close();
        }
        self.pending = None;
        self.pending_tab = None;
        let connection_id = self.allocate_id();
        self.pending_connection_id = Some(connection_id);
        let label = profile.name.clone();
        let ctx = ctx.clone();
        let on_event = Arc::new(move |_ev: &SessionEvent| {
            ctx.request_repaint();
        });
        let (_thread, rx) = connect_remote(&profile, 80, 24, on_event);
        self.pending = Some(rx);
        self.pending_label = label.clone();

        let (_sftp_thread, sftp_handle, sftp_rx) = connect_sftp(&profile);
        // 主机名随本连接绑定，避免与并发连接串台。
        self.pending_sftp = Some(SftpConnection {
            connection_id,
            handle: sftp_handle,
            rx: sftp_rx,
            host: label,
            home: None,
        });
        self.sftp_error = None;
    }

    /// 将已就绪的 SFTP 会话挂载到 SSH 标签页。
    fn mount_ready_sftp(&mut self) {
        let Some(connection_id) = self.pending_tab else {
            return;
        };
        let Some(connection) = self.ready_sftp.take() else {
            return;
        };
        if connection.connection_id != connection_id {
            connection.handle.close();
            return;
        }
        let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == connection_id) else {
            connection.handle.close();
            self.pending_tab = None;
            return;
        };
        {
            let home = connection.home.unwrap_or_else(|| "/".to_string());
            tab.terminal.set_remote_current_directory(&home);
            tab.sftp = Some(SftpView::new_at_path(
                &connection.host,
                connection.handle,
                connection.rx,
                &home,
            ));
        }
    }

    /// 处理 SFTP 连接结果。
    fn poll_sftp(&mut self) {
        let mut ready = false;
        let mut failed: Option<String> = None;
        let mut closed = false;
        if let Some(connection) = &mut self.pending_sftp {
            while let Ok(ev) = connection.rx.try_recv() {
                match ev {
                    SftpEvent::Ready { home: path } => {
                        ready = true;
                        connection.home = Some(path);
                    }
                    SftpEvent::Failed(e) => failed = Some(e),
                    // 连接中途关闭（如被服务器断开）：不能继续等待，
                    // 否则状态栏会永远停在"SFTP 连接中…"。
                    SftpEvent::Closed => closed = true,
                    _ => {}
                }
            }
        }
        let err = failed.or(closed.then(|| "连接中断".to_string()));
        if err.is_none() && ready {
            self.ready_sftp = self.pending_sftp.take();
            self.mount_ready_sftp();
        }
        if let Some(e) = err {
            if let Some(connection) = self.pending_sftp.take() {
                connection.handle.close();
            }
            if let Some(connection) = self.ready_sftp.take() {
                connection.handle.close();
            }
            // 状态栏持久显示（toast 一闪而过容易忽略）。
            self.sftp_error = Some(format!("SFTP 连接失败：{e}"));
            self.show_toast(self.sftp_error.clone().unwrap(), true);
        }
    }

    /// 设置弹窗：Mino 控制台风格的统一外壳。
    ///
    /// 不使用 egui 默认标题栏，标题、关闭按钮与内容卡片共用同一个内边距
    /// 基线，避免出现“系统头部一套边距、弹窗内容另一套边距”的错位。
    fn settings_panel(&mut self, ctx: &egui::Context) {
        let theme = crate::theme::current_theme();
        let mut open = self.show_settings;
        let mut close_requested = false;
        egui::Window::new("settings_panel")
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, -20.0])
            .default_size([620.0, 560.0])
            .min_size([520.0, 420.0])
            .max_size([700.0, 580.0])
            .resizable(true)
            .collapsible(false)
            .title_bar(false)
            .frame(
                egui::Frame::new()
                    .fill(theme.bg_app)
                    .corner_radius(egui::CornerRadius::same(14))
                    .stroke(egui::Stroke::new(1.0, theme.border))
                    .inner_margin(egui::Margin::same(0)),
            )
            .show(ctx, |ui| {
                // ==================== 对齐的自绘头部 ====================
                let header_h = 78.0;
                let (header_rect, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), header_h),
                    egui::Sense::hover(),
                );
                ui.painter().rect_filled(
                    header_rect,
                    egui::CornerRadius {
                        nw: 14,
                        ne: 14,
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
                header.add_space(20.0);
                draw_logo_mark(&mut header, 46.0);
                header.add_space(12.0);
                header.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    ui.label(
                        egui::RichText::new("WORKSPACE CONTROL")
                            .monospace()
                            .size(9.0)
                            .color(theme.accent),
                    );
                    ui.label(
                        egui::RichText::new("设置")
                            .strong()
                            .size(19.0)
                            .color(theme.text_primary),
                    );
                });
                header.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(16.0);
                    let close = ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new("×")
                                    .size(21.0)
                                    .color(theme.text_secondary),
                            )
                            .fill(egui::Color32::TRANSPARENT)
                            .stroke(egui::Stroke::NONE)
                            .min_size(egui::vec2(30.0, 30.0))
                            .corner_radius(crate::theme::tokens::RADIUS_ITEM),
                        )
                        .on_hover_text("关闭设置（Esc）")
                        .on_hover_cursor(egui::CursorIcon::PointingHand);
                    if close.clicked() {
                        close_requested = true;
                    }
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new("ESC")
                            .monospace()
                            .size(9.0)
                            .color(theme.text_muted),
                    );
                });

                ui.add_space(14.0);

                egui::ScrollArea::vertical()
                    .id_salt("settings_scroll")
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width() - 32.0);
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
                            ui.add_space(4.0);
                            ui.label(
                                egui::RichText::new("⌥1 — ⌥3 快速切换主题")
                                    .monospace()
                                    .size(10.0)
                                    .color(theme.text_muted),
                            );
                        });
                        ui.add_space(10.0);

                        // ============ 关于 ============
                        Self::settings_card(ui, "关于", |ui| {
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "{PRODUCT_NAME} v{}",
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
                                    self.start_update_check(false, ctx);
                                }
                            });
                            ui.add_space(8.0);
                            ui.separator();
                            ui.add_space(4.0);
                            // 性能 HUD 开关（调试用）。
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new("性能 HUD")
                                        .size(12.0)
                                        .color(theme.text_secondary),
                                );
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        let mut enabled = self.show_perf_hud;
                                        if ui
                                            .checkbox(&mut enabled, "")
                                            .on_hover_text("显示帧耗时 / FPS（⌥P 切换）")
                                            .changed()
                                        {
                                            self.show_perf_hud = enabled;
                                        }
                                    },
                                );
                            });
                        });
                    });
            });
        if close_requested {
            open = false;
        }
        // Esc 关闭 / × 关闭 / 闭包内主动关闭（如双击主机行连接成功）都生效：
        // open 由 egui 回写用户关闭动作，self.show_settings 记录闭包内的主动关闭。
        self.show_settings = open && self.show_settings;
    }

    /// 设置弹窗的统一内容卡片：标题、编号、分隔线和内容使用同一条水平基线。
    fn settings_card(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
        let theme = crate::theme::current_theme();
        let frame = egui::Frame::new()
            .fill(theme.bg_panel)
            .stroke(egui::Stroke::new(1.0, theme.border))
            .corner_radius(egui::CornerRadius::same(11))
            .inner_margin(egui::Margin::symmetric(16, 14));
        frame.show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                ui.label(
                    egui::RichText::new(title)
                        .strong()
                        .size(13.0)
                        .color(theme.text_primary),
                );
                ui.label(
                    egui::RichText::new(match title {
                        "主机管理" => "HOSTS / SSH",
                        "外观" => "APPEARANCE",
                        _ => "SYSTEM",
                    })
                    .monospace()
                    .size(9.0)
                    .color(theme.accent2),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(match title {
                            "主机管理" => "01",
                            "外观" => "02",
                            _ => "03",
                        })
                        .monospace()
                        .size(10.0)
                        .color(theme.text_muted),
                    );
                });
            });
            ui.add_space(9.0);
            ui.painter().line_segment(
                [
                    ui.cursor().left_top(),
                    egui::pos2(ui.max_rect().right(), ui.cursor().top()),
                ],
                egui::Stroke::new(1.0, theme.border),
            );
            ui.add_space(10.0);
            body(ui);
        });
        ui.add_space(12.0);
    }

    /// 标签栏 ">_" 快捷按钮弹出的主机菜单：单击主机行直接发起连接
    /// （与设置弹窗主机行的双击不同——快捷入口单击即连，无需二次确认）。
    /// 无已保存主机时提示并可一键打开新建连接对话框。
    fn host_quick_menu(&mut self, ui: &mut egui::Ui) {
        let theme = crate::theme::current_theme();
        const MENU_W: f32 = 256.0;
        const ROW_H: f32 = 40.0;
        const AVATAR: f32 = 22.0;
        ui.set_min_width(MENU_W);
        ui.set_max_width(MENU_W);
        ui.spacing_mut().item_spacing.y = 2.0;

        if self.config.hosts.is_empty() {
            ui.add_space(8.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new("暂无已保存主机")
                        .size(12.0)
                        .color(theme.text_muted),
                );
                ui.add_space(6.0);
                if ui
                    .button(
                        egui::RichText::new("新建连接")
                            .size(12.0)
                            .color(theme.text_primary),
                    )
                    .clicked()
                {
                    self.show_new_conn = true;
                    ui.close();
                }
            });
            ui.add_space(8.0);
            return;
        }

        ui.add_space(2.0);
        let mut connect: Option<HostProfile> = None;
        for (i, host) in self.config.hosts.iter().enumerate() {
            let row_id = egui::Id::new(("quick_host", i));
            // 先占满菜单宽度，hover 高亮才能贴齐左右内边距（曾按内容包围盒
            // expand，短名称行高亮左右留白、像一块浮岛）。
            let (row_rect, _) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), ROW_H),
                egui::Sense::hover(),
            );
            let highlight = row_rect.shrink2(egui::vec2(4.0, 1.0));
            let hovered = ui.rect_contains_pointer(highlight);
            if hovered {
                ui.painter().rect_filled(
                    highlight,
                    crate::theme::tokens::RADIUS_ITEM,
                    egui::Color32::from_rgba_unmultiplied(255, 255, 255, 16),
                );
            }

            // 行内容：头像垂直居中 + 名称/地址左对齐截断。
            // 不用 add_sized——其内部 Layout::centered_and_justified 会把短
            // 名称水平居中，和长地址错位（回归：ssh快捷菜单行左对齐）。
            let mut inner = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt(row_id)
                    .max_rect(row_rect)
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
            );
            inner.spacing_mut().item_spacing.x = 8.0;
            inner.add_space(10.0);
            let (avatar_rect, _) =
                inner.allocate_exact_size(egui::vec2(AVATAR, AVATAR), egui::Sense::hover());
            anim::paint_rounded_gradient(
                inner.painter(),
                avatar_rect,
                AVATAR * 0.5,
                theme.accent,
                theme.accent2,
            );
            let initial = host.name.chars().next().unwrap_or('?');
            inner.painter().text(
                avatar_rect.center(),
                egui::Align2::CENTER_CENTER,
                initial.to_string(),
                egui::FontId::proportional(11.0),
                egui::Color32::WHITE,
            );
            inner.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                ui.set_max_width(ui.available_width() - 10.0);
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(&host.name)
                            .size(12.5)
                            .color(theme.text_primary),
                    )
                    .truncate(),
                );
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(format!("{}@{}", host.user, host.host))
                            .size(10.5)
                            .color(theme.text_secondary),
                    )
                    .truncate(),
                );
            });

            // 整行点击区（显式 interact + 稳定 Id，注册在内容之后）。
            let resp = ui
                .interact(row_rect, row_id.with("click"), egui::Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            if resp.clicked() {
                connect = Some(host.clone());
                ui.close();
            }
        }
        ui.add_space(2.0);
        if let Some(profile) = connect {
            self.start_connect(ui.ctx(), profile);
        }
    }

    /// 渲染设置里的主机管理区。
    ///
    /// 每个主机使用独立的 endpoint 卡片：名称、地址、认证方式和操作按钮
    /// 有明确层级，点击区域覆盖整行，避免只点到文字才有反应。
    fn host_sidebar(&mut self, ui: &mut egui::Ui) {
        let theme = crate::theme::current_theme();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.label(
                egui::RichText::new("已保存主机")
                    .strong()
                    .size(12.5)
                    .color(theme.text_primary),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new(format!("{:02} ENDPOINTS", self.config.hosts.len()))
                        .monospace()
                        .size(9.0)
                        .color(theme.text_muted),
                );
            });
        });
        ui.add_space(9.0);

        // 新建连接：右侧动作按钮与主机卡片共享同一宽度节奏。
        let new_btn = egui::Button::new(
            egui::RichText::new("新建连接")
                .color(crate::theme::tokens::ACCENT_FG)
                .size(12.5),
        )
        .fill(theme.accent)
        .stroke(egui::Stroke::NONE)
        .corner_radius(egui::CornerRadius::same(7));
        if ui
            .add_sized(egui::vec2(ui.available_width(), 32.0), new_btn)
            .clicked()
        {
            self.show_new_conn = true;
        }
        ui.add_space(12.0);

        if self.config.hosts.is_empty() {
            let (empty_rect, _) = ui
                .allocate_exact_size(egui::vec2(ui.available_width(), 92.0), egui::Sense::hover());
            ui.painter().rect_filled(
                empty_rect,
                crate::theme::tokens::RADIUS_ITEM,
                theme.bg_elevated.gamma_multiply(0.55),
            );
            ui.painter().rect_stroke(
                empty_rect,
                crate::theme::tokens::RADIUS_ITEM,
                egui::Stroke::new(1.0, theme.border),
                egui::StrokeKind::Inside,
            );
            let mut empty = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt("empty_hosts")
                    .max_rect(empty_rect)
                    .layout(egui::Layout::top_down(egui::Align::Center)),
            );
            empty.add_space(22.0);
            empty.label(
                egui::RichText::new("暂无已保存主机")
                    .color(theme.text_secondary)
                    .size(12.5),
            );
            empty.add_space(2.0);
            empty.label(
                egui::RichText::new("⌘N 添加第一台主机")
                    .monospace()
                    .size(10.0)
                    .color(theme.text_muted),
            );
        }

        let mut remove_idx: Option<usize> = None;
        let mut connect_idx: Option<usize> = None;
        for (i, host) in self.config.hosts.iter().enumerate() {
            let row_id = egui::Id::new(("host_row", i));
            let (row_rect, _) = ui
                .allocate_exact_size(egui::vec2(ui.available_width(), 68.0), egui::Sense::hover());
            let row_response = ui
                .interact(row_rect, row_id, egui::Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand);

            let hover = row_response.hovered();
            let selected = self.selected_host == Some(i);
            let fill = if selected {
                theme.accent_soft
            } else if hover {
                theme.bg_elevated.gamma_multiply(1.12)
            } else {
                theme.bg_elevated.gamma_multiply(0.72)
            };
            ui.painter().rect_filled(row_rect, 8.0, fill);
            ui.painter().rect_stroke(
                row_rect,
                8.0,
                egui::Stroke::new(1.0, if selected { theme.accent } else { theme.border }),
                egui::StrokeKind::Inside,
            );
            if selected || hover {
                ui.painter().rect_filled(
                    egui::Rect::from_min_max(
                        egui::pos2(row_rect.left(), row_rect.top() + 9.0),
                        egui::pos2(row_rect.left() + 2.0, row_rect.bottom() - 9.0),
                    ),
                    1.0,
                    if selected {
                        theme.accent
                    } else {
                        theme.accent2
                    },
                );
            }

            let content_rect = row_rect.shrink2(egui::vec2(14.0, 8.0));
            let mut inner = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt(row_id.with("content"))
                    .max_rect(content_rect)
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
            );
            inner.spacing_mut().item_spacing.x = 10.0;
            let avatar_size = 38.0;
            let (avatar_rect, _) = inner
                .allocate_exact_size(egui::vec2(avatar_size, avatar_size), egui::Sense::hover());
            anim::paint_rounded_gradient(
                inner.painter(),
                avatar_rect,
                11.0,
                theme.accent2,
                theme.accent,
            );
            let initial = host.name.chars().next().unwrap_or('?');
            inner.painter().text(
                avatar_rect.center(),
                egui::Align2::CENTER_CENTER,
                initial.to_string(),
                egui::FontId::proportional(15.0),
                egui::Color32::WHITE,
            );
            inner.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                ui.set_max_width((content_rect.width() - avatar_size - 88.0).max(80.0));
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(&host.name)
                            .strong()
                            .size(13.0)
                            .color(theme.text_primary),
                    )
                    .truncate(),
                );
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(format!("{}@{}:{}", host.user, host.host, host.port))
                            .monospace()
                            .size(10.5)
                            .color(theme.text_secondary),
                    )
                    .truncate(),
                );
            });

            let auth_label = if matches!(host.auth, Auth::Key { .. }) {
                "SSH KEY"
            } else {
                "PASSWORD"
            };
            let auth_frame = egui::Frame::new()
                .fill(theme.bg_panel)
                .stroke(egui::Stroke::new(1.0, theme.border))
                .corner_radius(5.0)
                .inner_margin(egui::Margin::symmetric(6, 3));
            auth_frame.show(&mut inner, |ui| {
                ui.label(
                    egui::RichText::new(auth_label)
                        .monospace()
                        .size(8.5)
                        .color(theme.accent2),
                );
            });

            // 删除按钮最后注册，覆盖整行点击区，避免点击删除时先触发连接。
            let del_rect = egui::Rect::from_min_size(
                egui::pos2(row_rect.right() - 34.0, row_rect.center().y - 13.0),
                egui::vec2(26.0, 26.0),
            );
            let del_resp = ui
                .interact(del_rect, row_id.with("del"), egui::Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            if del_resp.hovered() {
                ui.painter()
                    .rect_filled(del_rect, 6.0, theme.danger.gamma_multiply(0.18));
            }
            ui.painter().text(
                del_rect.center(),
                egui::Align2::CENTER_CENTER,
                "×",
                egui::FontId::proportional(18.0),
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
                self.selected_host = Some(i);
                let now = ui.input(|i| i.time);
                if let Some((t, idx)) = self.last_row_click {
                    if idx == i && now - t < 0.3 {
                        connect_idx = Some(i);
                    }
                }
                self.last_row_click = Some((now, i));
            }
            ui.add_space(8.0);
        }
        if let Some(i) = connect_idx {
            let profile = self.config.hosts[i].clone();
            // 从设置弹窗双击连接成功后关闭弹窗，直接进入终端。
            self.show_settings = false;
            self.start_connect(ui.ctx(), profile);
        }
        if let Some(i) = remove_idx {
            let removed = self.config.hosts.remove(i);
            self.selected_host = None;
            if !self.save_config() {
                self.config.hosts.insert(i, removed);
            }
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
        egui::Window::new("connect_dialog")
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, -20.0])
            .resizable(false)
            .collapsible(false)
            .title_bar(false)
            .frame(
                egui::Frame::new()
                    .fill(theme.bg_app)
                    .stroke(egui::Stroke::new(1.0, theme.border))
                    .corner_radius(egui::CornerRadius::same(14))
                    .inner_margin(egui::Margin::same(18)),
            )
            .show(ctx, |ui| {
                // 与设置窗口共用同一套头部基线，标题不再依赖 egui 默认 title bar。
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    draw_logo_mark(ui, 38.0);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        ui.label(
                            egui::RichText::new("NEW CONNECTION")
                                .monospace()
                                .size(9.0)
                                .color(theme.accent),
                        );
                        ui.label(
                            egui::RichText::new("新建连接")
                                .strong()
                                .size(17.0)
                                .color(theme.text_primary),
                        );
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new("×")
                                        .size(20.0)
                                        .color(theme.text_secondary),
                                )
                                .fill(egui::Color32::TRANSPARENT)
                                .stroke(egui::Stroke::NONE)
                                .min_size(egui::vec2(28.0, 28.0))
                                .corner_radius(crate::theme::tokens::RADIUS_ITEM),
                            )
                            .clicked()
                        {
                            canceled = true;
                        }
                    });
                });
                ui.add_space(15.0);
                ui.painter().line_segment(
                    [
                        ui.cursor().left_top(),
                        egui::pos2(ui.max_rect().right(), ui.cursor().top()),
                    ],
                    egui::Stroke::new(1.0, theme.border),
                );
                ui.add_space(13.0);
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
            let saved = self.save_config();
            if !saved {
                self.config.hosts.pop();
            }
            self.show_new_conn = false;
            self.show_settings = false;
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
            // macOS traffic lights（隐藏系统按钮后由应用自绘）。
            let (tl, _) = draw_traffic_lights(ui);
            tl_clicked = tl;
            // 顶部栏保留交通灯与标签页，不再重复显示品牌图标和名称。
            ui.add_space(12.0);
            let mut switch_to: Option<usize> = None;
            let mut close_idx: Option<usize> = None;
            for (i, tab) in self.tabs.iter().enumerate() {
                let title = tab.title();
                let selected = i == self.active_tab;
                // 数字索引前缀：与终端主流 Tab 习惯一致（1, 2, ...）。
                let prefix = format!("{} ", i + 1);
                // 选中底必须画在内容之前（Frame 先铺底再放内容）：曾把不透明
                // 面板色画在内容之后，激活 tab 的文字被整块盖住、完全不可见。
                let sel_alpha = anim::smooth_bool(
                    ui.ctx(),
                    egui::Id::new(("tab_sel", i)),
                    selected,
                    anim::SPEED_FAST,
                );
                let row = ui
                    .scope_builder(egui::UiBuilder::new().id_salt(("tab", i)), |ui| {
                        egui::Frame::new()
                            .fill(egui::Color32::from_rgba_unmultiplied(
                                theme.bg_panel.r(),
                                theme.bg_panel.g(),
                                theme.bg_panel.b(),
                                (sel_alpha * 255.0) as u8,
                            ))
                            .corner_radius(crate::theme::tokens::RADIUS_ITEM)
                            .inner_margin(egui::Margin::symmetric(2, 3))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.add_space(4.0);
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
                                    // 与手绘高亮叠加会形成"双重框"）。
                                    if ui
                                        .add(
                                            egui::Button::new(
                                                egui::RichText::new(&title).size(12.5).color(
                                                    if selected {
                                                        theme.text_primary
                                                    } else {
                                                        theme.text_muted
                                                    },
                                                ),
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
                                    ui.add_space(2.0);
                                });
                            });
                    })
                    .response;

                // hover 底（动画过渡）：白色低透明度叠加画在内容之后，只是
                // 极淡提亮不遮文字；激活 tab 的凸起底已由上方 Frame 先铺好。
                let hover = row.hovered() && !selected;
                let hover_alpha = anim::smooth_bool(
                    ui.ctx(),
                    egui::Id::new(("tab_hover", i)),
                    hover,
                    anim::SPEED_FAST,
                );
                if hover_alpha > 0.01 {
                    ui.painter().rect_filled(
                        row.rect.expand2(egui::vec2(1.0, 2.0)),
                        crate::theme::tokens::RADIUS_ITEM,
                        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 12)
                            .gamma_multiply(hover_alpha),
                    );
                }
                // 底部指示条（宽度随选中状态动画；白色细线，Tabby current-tab-indicator）。
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
                        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 200)
                            .gamma_multiply(sel_alpha.max(0.25)),
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

            // 快速 SSH 连接（">_" 图标）：点击弹出已保存主机列表，单击主机行
            // 直接发起连接（不需要进设置弹窗双击）。push_id 固定按钮 Id——
            // Popup::menu 的开关状态按 Id 记忆，自动 Id 帧间漂移会让菜单闪断。
            let ssh_btn_resp = ui.push_id("ssh_quick", ssh_quick_button).inner;
            egui::Popup::menu(&ssh_btn_resp).show(|ui| self.host_quick_menu(ui));

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
                // 标题读缓存（避免每帧 Mutex + String clone）。
                let title = tab.terminal.session_title();
                let exited = session.has_exited();
                status_dot(ui, if exited { theme.danger } else { theme.success }, false);
                ui.label(
                    egui::RichText::new(if title.is_empty() {
                        tab.label.clone()
                    } else {
                        title.to_string()
                    })
                    .size(11.5)
                    .color(theme.text_secondary),
                );
                if exited {
                    ui.colored_label(theme.danger, "会话已退出");
                }
                // SFTP 主机名从标签页的 SFTP 视图读取（每个标签绑定自己的连接，
                // 多远程标签共存时不会显示成最后一次连接的主机名）。
                if let Some(sftp) = &tab.sftp {
                    ui.separator();
                    status_dot(ui, theme.accent2, false);
                    ui.colored_label(theme.accent2, format!("SFTP · {}", sftp.host_name()));
                }
            }
            if self.pending_sftp.is_some() {
                ui.separator();
                loading_hint(ui, "SFTP 连接中…");
            }
            if let Some(e) = &self.sftp_error {
                ui.separator();
                status_dot(ui, theme.danger, false);
                ui.label(egui::RichText::new(e).size(11.5).color(theme.danger))
                    .on_hover_text("重新连接主机可再次尝试");
            }
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
                            egui::RichText::new(format!("更新 {PRODUCT_NAME}"))
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
                    loading_hint(ui, "正在挂载并安装，请稍候…");
                    ui.add_space(8.0);
                } else if matches!(self.update_state, UpdateState::Installed) {
                    ui.horizontal(|ui| {
                        status_dot(ui, theme.success, false);
                        ui.label(
                            egui::RichText::new("安装准备完成，应用即将退出并重启")
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
            Some(UpdateAction::StartDownload(info)) => self.start_download(info, ctx),
            Some(UpdateAction::CancelDownload) => {
                self.cancel_download();
                self.update_state = UpdateState::Idle;
            }
            Some(UpdateAction::Install { dmg_path }) => self.install_update(ctx, dmg_path),
            Some(UpdateAction::Retry) => self.start_update_check(false, ctx),
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
        // 滑入动画期间持续重绘（约 0.32s）；动画结束后不再 60fps 循环，
        // 仅安排到期关闭帧——Toast 停留期终端无输出时整帧静止省电。
        let in_t = (elapsed / 0.32).clamp(0.0, 1.0) as f32;
        if in_t < 1.0 {
            ctx.request_repaint_after(Duration::from_millis(16));
        } else {
            ctx.request_repaint_after(Duration::from_millis(
                ((DURATION - elapsed) * 1000.0).max(16.0) as u64,
            ));
        }
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
                    egui::RichText::new(PRODUCT_NAME)
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

/// 隔离的测试配置路径（每个测试独享一份临时文件）。
///
/// 绝不触碰用户真实的 `~/.config/mino/hosts.toml`——曾发生测试直接
/// 覆盖并删除用户主机配置（运行一次测试丢一次主机列表）。
#[cfg(test)]
pub(crate) fn test_config_path(tag: &str) -> PathBuf {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "mino-test-config-{tag}-{}-{sequence}.toml",
        std::process::id(),
    ))
}

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

/// 品牌标记：紫青渐变圆角底 + 白色极简终端提示符 `>_`。
///
/// 与应用图标（scripts/make-icon.swift）同构图；hover 时带 accent 辉光。
fn draw_logo_mark(ui: &mut egui::Ui, size: f32) -> egui::Rect {
    let theme = crate::theme::current_theme();
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let center = rect.center();
        let tile = egui::Rect::from_center_size(center, egui::vec2(size * 0.80, size * 0.80));
        let radius = tile.width() * 0.22;
        // 圆角底：accent → accent2 垂直渐变。
        anim::paint_rounded_gradient(painter, tile, radius, theme.accent, theme.accent2);

        // `>_`：用线条绘制，避免依赖字体字形，在小尺寸下也保持清晰。
        let stroke = egui::Stroke::new((size * 0.075).max(1.5), egui::Color32::WHITE);
        let chevron_left = tile.left() + tile.width() * 0.29;
        let chevron_tip = tile.left() + tile.width() * 0.44;
        let chevron_half_height = tile.height() * 0.18;
        painter.line_segment(
            [
                egui::pos2(chevron_left, center.y - chevron_half_height),
                egui::pos2(chevron_tip, center.y),
            ],
            stroke,
        );
        painter.line_segment(
            [
                egui::pos2(chevron_tip, center.y),
                egui::pos2(chevron_left, center.y + chevron_half_height),
            ],
            stroke,
        );
        painter.line_segment(
            [
                egui::pos2(
                    tile.left() + tile.width() * 0.54,
                    center.y + tile.height() * 0.18,
                ),
                egui::pos2(
                    tile.left() + tile.width() * 0.73,
                    center.y + tile.height() * 0.18,
                ),
            ],
            stroke,
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

/// 静态加载指示（accent2 圆点 + 文字）。
///
/// 替代 egui `Spinner`：其内部每帧 `request_repaint` 强制 60fps 全帧重绘
/// （连接等待/安装期间终端无输出时整帧白烧 CPU），静态点零重绘。
fn loading_hint(ui: &mut egui::Ui, text: &str) {
    let theme = crate::theme::current_theme();
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(9.0, 9.0), egui::Sense::hover());
        ui.painter()
            .circle_filled(rect.center(), 3.2, theme.accent2);
        ui.label(
            egui::RichText::new(text)
                .size(if ui.available_height() > 24.0 {
                    12.5
                } else {
                    11.5
                })
                .color(theme.text_secondary),
        );
    });
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
fn launch_installer(dmg: &Path, mount: &Path, result_path: &Path) -> Result<(), String> {
    let dir = std::env::temp_dir().join("mino-update");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let script = dir.join("install.sh");
    std::fs::write(&script, INSTALL_SCRIPT).map_err(|e| e.to_string())?;
    let log_path = dir.join(format!("install-{}.log", std::process::id()));
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .map_err(|e| e.to_string())?;
    let log_err = log.try_clone().map_err(|e| e.to_string())?;
    Command::new("/bin/sh")
        .arg(&script)
        .arg(dmg)
        .arg(mount)
        .arg(result_path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

impl eframe::App for MinoApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // 缓存 ctx 供非 UI 回调使用（如复制 tab 时构造 Session）。
        self.last_ctx = ctx.clone();
        self.perf.begin_frame();

        // ==================== 快捷键 ====================
        // ⌥P：切换性能 HUD。
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::ALT, egui::Key::P)) {
            self.show_perf_hud = !self.show_perf_hud;
        }
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
        // 所有标签都轮询后台状态，非活动标签不会积压终端写回或 SFTP 事件。
        for tab in &mut self.tabs {
            tab.terminal.drain_background_events();
        }
        let mut sftp_event_received = false;
        for tab in &mut self.tabs {
            if let Some(sftp) = &mut tab.sftp {
                sftp_event_received |= sftp.poll_events();
            }
        }
        if sftp_event_received {
            ctx.request_repaint();
        }
        self.poll_connection(&ctx);
        self.poll_sftp();
        self.poll_update();
        self.poll_download(&ctx);
        self.poll_install(&ctx);

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
        // SFTP 面板宽度：默认约 40% 窗口宽、最宽 50%（曾固定 340px，
        // 大窗口下偏窄；用户要求默认 40%、上限 50%）。
        let viewport_w = ui.ctx().viewport_rect().width();
        let sftp_default_w = viewport_w * 0.40;
        let max_sftp_w = viewport_w * 0.50;
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            if let Some(sftp) = &mut tab.sftp {
                let terminal_cwd = tab.terminal.current_directory();
                let sftp_frame = egui::Frame::new()
                    .fill(theme.bg_panel)
                    .inner_margin(egui::Margin::symmetric(12, 10));
                egui::Panel::right("sftp_panel")
                    .default_size(sftp_default_w)
                    // 最小宽度保证面板始终可见可操作（此前可被拖到极窄）。
                    .min_size(260.0)
                    // 最宽限制（约窗口 50%）：防止拖宽挤压终端
                    // （曾拖到 ~70% 窗口宽把终端压成一条窄带）。
                    .max_size(max_sftp_w)
                    .resizable(true)
                    .frame(sftp_frame)
                    .show_collapsible(ui, &mut tab.sftp_open, |ui| {
                        sftp.show_with_terminal_cwd(ui, terminal_cwd.as_deref())
                    });
            }
        }
        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(_pending) = &self.pending {
                ui.centered_and_justified(|ui| {
                    loading_hint(ui, &format!("正在连接 {} …", self.pending_label));
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

        // ==================== 性能 HUD ====================
        self.perf.end_frame();
        // 活动标签页的终端分段耗时喂给统计（无标签页时为 0 不影响）。
        if let Some(tab) = self.tabs.get(self.active_tab) {
            let (build, layout, paint) = tab.terminal.last_timing();
            self.perf.add_build(build);
            self.perf.add_layout(layout);
            self.perf.add_paint(paint);
        }
        if self.show_perf_hud {
            render_perf_hud(&ctx, &self.perf);
        }
    }
}

/// 渲染性能 HUD（右上角半透明小面板，调试用）。
fn render_perf_hud(ctx: &egui::Context, perf: &crate::perf::PerfStats) {
    let theme = crate::theme::current_theme();
    egui::Area::new(egui::Id::new("perf_hud"))
        .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-12.0, 44.0))
        .order(egui::Order::Foreground)
        .interactable(false)
        .show(ctx, |ui| {
            let frame = egui::Frame::new()
                .fill(theme.bg_elevated.gamma_multiply(0.92))
                .corner_radius(6.0)
                .inner_margin(egui::Margin::symmetric(10, 6));
            frame.show(ui, |ui| {
                ui.label(
                    egui::RichText::new(perf.summary())
                        .monospace()
                        .size(10.5)
                        .color(theme.text_muted),
                );
            });
        });
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
                ui.label(egui::RichText::new(super::PRODUCT_NAME).strong());
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
        harness.get_by_label(super::PRODUCT_NAME);
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
            let mut app = MinoApp::new(cc);
            // 构造带 SFTP 会话的标签页（mock 通道，无需测试 sshd）。
            let session = Session::spawn_local(
                SessionOptions::default(),
                80,
                24,
                Arc::new(|_ev: &SessionEvent| {}),
            )
            .expect("创建本地终端失败");
            let id = app.allocate_id();
            let mut tab = TerminalTab::new(id, "测试主机".into(), TerminalView::new(session));
            let (_tx, rx) = tokio::sync::mpsc::channel(128);
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
            harness.root().query_all_by_label("..").next().is_none(),
            "SFTP 面板应默认收起"
        );
        harness.get_by_label("SFTP").click();
        harness.run_steps(6);
        // 点击悬浮按钮 → 面板展开。
        harness.get_by_label("..");
        // 再点 → 收起。
        harness.get_by_label("SFTP").click();
        harness.run_steps(6);
        assert!(
            harness.root().query_all_by_label("..").next().is_none(),
            "再次点击应收起面板"
        );
    }

    /// 回归：前面的标签被删除后，延迟到达的 SFTP 结果仍按稳定身份挂载，
    /// 不能按旧 Vec 下标落到另一标签。
    #[test]
    fn sftp按稳定标签身份挂载() {
        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            let mut app = MinoApp::new(cc);
            app.new_local_tab(&cc.egui_ctx);
            let target_id = app.tabs[1].id;
            app.tabs.remove(0);
            app.active_tab = 0;

            let (_event_tx, event_rx) = tokio::sync::mpsc::channel(128);
            let (command_tx, _command_rx) = tokio::sync::mpsc::unbounded_channel();
            app.pending_tab = Some(target_id);
            app.ready_sftp = Some(SftpConnection {
                connection_id: target_id,
                handle: SftpHandle::from_raw(command_tx),
                rx: event_rx,
                host: "稳定身份主机".into(),
                home: Some("/home/test".into()),
            });
            app.mount_ready_sftp();
            app
        });
        harness.run_steps(6);
        let tab = &harness.state().tabs[0];
        assert_eq!(tab.label, "本地终端");
        assert_eq!(
            tab.sftp.as_ref().map(SftpView::host_name),
            Some("稳定身份主机")
        );
    }

    #[test]
    fn ssh失败关闭已就绪的sftp连接() {
        let _harness = egui_kittest::Harness::new_eframe(|cc| {
            let mut app = MinoApp::new(cc);
            let (result_tx, result_rx) = tokio::sync::mpsc::unbounded_channel();
            result_tx
                .send(ConnectResult::Failed("SSH 失败".into()))
                .unwrap();
            app.pending = Some(result_rx);

            let (command_tx, mut command_rx) = tokio::sync::mpsc::unbounded_channel();
            app.ready_sftp = Some(SftpConnection {
                connection_id: 42,
                handle: SftpHandle::from_raw(command_tx),
                rx: tokio::sync::mpsc::channel(128).1,
                host: "待关闭主机".into(),
                home: None,
            });
            app.poll_connection(&cc.egui_ctx);
            assert!(matches!(
                command_rx.try_recv(),
                Ok(mino_core::ssh::sftp::SftpCmd::Shutdown)
            ));
            app
        });
    }

    /// 本地终端输入命令前缀 → 补全浮层出现；Esc 关闭；Tab 确认补全。
    #[test]
    fn 输入触发补全浮层() {
        use kittest::Queryable;

        let mut harness = egui_kittest::Harness::new_eframe(|cc| MinoApp::new(cc));
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
    fn app_grid_text(app: &MinoApp) -> String {
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

        let mut harness = egui_kittest::Harness::new_eframe(|cc| MinoApp::new(cc));
        harness.run_steps(6);

        // 等待 zsh 就绪（提示符出现）。用 `~`（home 缩写，zsh 在 home 目录的
        // 交互提示符必含）而非 `➜`（仅 oh-my-zsh robbyrussell 主题有）——
        // GitHub runner 默认 zsh 提示符是 `...:~ runner$`，不输出 `➜`，
        // 依赖 `➜` 会让该测试在 CI 上永远超时（此前 CI 失败根因）。
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut ready = false;
        while Instant::now() < deadline {
            harness.step();
            if app_grid_text(harness.state()).contains('~') {
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

        let mut harness = egui_kittest::Harness::new_eframe(|cc| MinoApp::new(cc));
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
    use mino_core::config::Auth;

    /// 将测试 sshd 的 known_hosts 记录与 hostkey 放在同一目录（/tmp/mino-test-sshd）：
    /// hostkey 随 /tmp 清理重建时指纹记录一并消失，避免旧指纹不匹配导致测试失败。
    /// `call_once` 保证进程内只设置一次（测试并行安全）。
    static KNOWN_HOSTS_INIT: std::sync::Once = std::sync::Once::new();
    fn init_test_env() {
        KNOWN_HOSTS_INIT.call_once(|| {
            std::env::set_var("MINO_KNOWN_HOSTS", "/tmp/mino-test-sshd/known_hosts.toml");
        });
    }

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
        let key_path = std::env::var("MINO_TEST_KEY").unwrap_or_else(|_| {
            format!(
                "{}/.ssh/id_ed25519",
                std::env::var("HOME").unwrap_or_default()
            )
        });
        HostProfile {
            name: "链路测试".into(),
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

    /// 端到端：点击侧栏主机条目 → 远程终端 + SFTP 面板出现。
    #[test]
    fn 点击连接建立远程会话() {
        use kittest::Queryable;
        use std::time::{Duration, Instant};

        init_test_env();
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
        // 隔离路径：绝不覆盖用户真实的 ~/.config/mino/hosts.toml
        //（曾直接覆盖并删除用户主机列表，运行一次测试丢一次配置）。
        let config_path = test_config_path("connect-e2e");
        let config = HostConfig {
            hosts: vec![profile],
        };
        config.save(&config_path).expect("写入测试配置失败");

        let mut harness = egui_kittest::Harness::builder()
            .with_step_dt(1.0 / 60.0)
            .build_eframe(|cc| MinoApp::new_with_config(cc, config_path.clone()));
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
        // 连接成功后设置弹窗应自动关闭（回归：曾保持打开遮住终端）。
        assert!(
            !harness.state().show_settings,
            "双击主机行连接成功后设置弹窗应自动关闭"
        );
        // SFTP 面板默认收起，需点击终端右上角悬浮按钮展开。
        harness.get_by_label("SFTP").click();
        for _ in 0..6 {
            harness.step();
        }
        assert!(
            harness
                .root()
                .query_all_by_label("SFTP · 链路测试")
                .next()
                .is_some(),
            "SFTP 标题应出现"
        );
        harness.get_by_label("..");
    }

    /// 回归：设置弹窗 → 新建连接 → 点"连接"后，新建连接对话框与设置弹窗
    /// 都应关闭（连接结果不影响关闭行为）。
    #[test]
    fn 新建连接后关闭设置弹窗() {
        use kittest::{NodeT, Queryable};

        let config_path = test_config_path("new-conn-dialog");
        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);

        // ⌘, 打开设置弹窗 → 点"新建连接"。
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
        assert!(harness.state().show_settings, "设置弹窗应打开");
        harness.get_by_label("新建连接").click();
        for _ in 0..3 {
            harness.step();
        }
        assert!(harness.state().show_new_conn, "新建连接对话框应打开");

        // 填主机（用户名默认 root、端口默认 22，无需改动）。
        // 输入框顺序：名称、用户名、主机、端口、密码。
        let inputs: Vec<_> = harness
            .root()
            .query_all_by_role(accesskit::Role::TextInput)
            .collect();
        assert!(inputs.len() >= 3, "表单应有名称/用户名/主机输入框");
        inputs[2].click();
        for _ in 0..2 {
            harness.step();
        }
        harness.event(egui::Event::Text("127.0.0.1".into()));
        for _ in 0..2 {
            harness.step();
        }

        // 点"连接" → 两个弹窗都应关闭（连接发起后失败与否不影响）。
        // 按钮文字会同时生成 Label 节点，需按 role 过滤出真正的 Button。
        let connect_btn = harness
            .root()
            .query_all_by_role(accesskit::Role::Button)
            .find(|n| n.accesskit_node().label() == Some("连接".to_string()))
            .expect("找不到连接按钮");
        connect_btn.click();
        for _ in 0..3 {
            harness.step();
        }
        assert!(
            !harness.state().show_new_conn,
            "点连接后新建连接对话框应关闭"
        );
        assert!(!harness.state().show_settings, "点连接后设置弹窗应关闭");
        // 清理测试写入的配置。
        std::fs::remove_file(&config_path).ok();
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    /// 与 connect_tests::init_test_env 相同（每模块独立 Once）。
    static KNOWN_HOSTS_INIT: std::sync::Once = std::sync::Once::new();
    fn init_test_env() {
        KNOWN_HOSTS_INIT.call_once(|| {
            std::env::set_var("MINO_KNOWN_HOSTS", "/tmp/mino-test-sshd/known_hosts.toml");
        });
    }

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
        use mino_core::config::Auth;
        use std::time::{Duration, Instant};

        init_test_env();
        if !sshd_available() {
            eprintln!("跳过：测试 sshd 未运行");
            return;
        }

        let key_path = std::env::var("MINO_TEST_KEY").unwrap_or_else(|_| {
            format!(
                "{}/.ssh/id_ed25519",
                std::env::var("HOME").unwrap_or_default()
            )
        });
        let profile = HostProfile {
            name: "链路测试".into(),
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
        };
        if let Auth::Key { path, .. } = &profile.auth {
            if !path.exists() {
                eprintln!("跳过：测试私钥不存在");
                return;
            }
        }
        let config_path = test_config_path("snapshot");
        let config = HostConfig {
            hosts: vec![profile],
        };
        config.save(&config_path).expect("写入测试配置失败");

        let mut harness = egui_kittest::Harness::builder()
            .with_step_dt(1.0 / 60.0)
            .build_eframe(|cc| MinoApp::new_with_config(cc, config_path.clone()));
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
                .build_eframe(|cc| MinoApp::new_with_config(cc, config_path.clone()));
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

        // 连接成功后设置弹窗自动关闭，SFTP 浮按钮可直接点击。
        harness.get_by_label("SFTP").click();
        for _ in 0..6 {
            harness.step();
        }
        // 布局断言：SFTP 面板应位于窗口右侧（终端 + 面板 + 侧栏三段式）。
        let panel_title = harness
            .root()
            .query_all_by_label("SFTP · 链路测试")
            .max_by(|a, b| a.rect().left().partial_cmp(&b.rect().left()).unwrap())
            .expect("SFTP 标题应出现");
        let r = panel_title.rect();
        assert!(
            r.left() > 400.0,
            "SFTP 面板应位于窗口右半侧，实际标题 x={}",
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
        let out = "/tmp/mino_style_sftp.png";
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

        let mut harness = egui_kittest::Harness::new_eframe(|cc| MinoApp::new(cc));
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
            for _ in 0..3 {
                harness.step();
            }
            harness
                .get_by_role_and_label(accesskit::Role::Button, theme_name)
                .click();
            for _ in 0..3 {
                harness.step();
            }
            assert_eq!(
                crate::theme::current_theme().name,
                theme_name,
                "主题切换失败"
            );
            let img = harness.render().expect("渲染失败");
            let out = format!("/tmp/mino_theme_{theme_name}.png");
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

        let mut harness = egui_kittest::Harness::new_eframe(|cc| MinoApp::new(cc));
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

        let mut harness = egui_kittest::Harness::new_eframe(|cc| MinoApp::new(cc));
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
        let mut harness = egui_kittest::Harness::new_eframe(|cc| MinoApp::new(cc));
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

    #[test]
    fn 测试构造器隔离配置并关闭自动更新() {
        let mut harness = egui_kittest::Harness::new_eframe(|cc| MinoApp::new(cc));
        harness.run_steps(2);
        let app = harness.state();
        assert!(matches!(app.update_state, UpdateState::Idle));
        assert_eq!(
            app.config_path.parent(),
            Some(std::env::temp_dir().as_path())
        );
        assert!(app
            .config_path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("mino-test-config-default-")));
    }

    /// ⌘, 快捷键切换设置弹窗（macOS 标准"应用偏好设置"）。
    #[test]
    fn 快捷键打开设置() {
        use kittest::Queryable;
        let mut harness = egui_kittest::Harness::new_eframe(|cc| MinoApp::new(cc));
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
        let mut harness = egui_kittest::Harness::new_eframe(|cc| MinoApp::new(cc));
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

    /// 标签栏 ">_" 快捷按钮：点击弹出已保存主机列表，单击主机行直接发起
    /// 连接（隔离配置路径，不碰用户真实 hosts.toml）。
    #[test]
    fn ssh快捷按钮连接主机() {
        use kittest::Queryable;

        let config_path = test_config_path("ssh-quick");
        let config = HostConfig {
            hosts: vec![HostProfile {
                name: "快捷主机".into(),
                host: "127.0.0.1".into(),
                // 端口 9（discard）必然拒绝连接：只验证"发起连接"，
                // 不依赖测试 sshd。
                port: 9,
                user: "root".into(),
                auth: Auth::Password("x".into()),
            }],
        };
        config.save(&config_path).expect("写入测试配置失败");

        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);

        // 点击 ">_" → 弹出主机菜单（主机名可见）。
        harness.get_by_label(">_").click();
        harness.run_steps(6);
        harness.get_by_label("快捷主机");

        // 单击主机行 → 直接发起连接（pending_label 记录目标主机）。
        harness.get_by_label("快捷主机").click();
        harness.run_steps(6);
        assert_eq!(
            harness.state().pending_label,
            "快捷主机",
            "单击主机行应直接发起连接"
        );
        // 弹出菜单应已关闭（user@host 行不再可见）。
        assert!(
            harness
                .root()
                .query_all_by_label("root@127.0.0.1")
                .next()
                .is_none(),
            "点击主机行后快捷菜单应关闭"
        );

        std::fs::remove_file(&config_path).ok();
    }

    /// 无已保存主机时，" >_" 快捷菜单应提示并引导新建连接。
    #[test]
    fn ssh快捷按钮无主机提示() {
        use kittest::Queryable;

        let config_path = test_config_path("ssh-quick-empty");
        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);

        harness.get_by_label(">_").click();
        harness.run_steps(6);
        harness.get_by_label("暂无已保存主机");
        harness.get_by_label("新建连接");

        std::fs::remove_file(&config_path).ok();
    }

    /// 快捷菜单行：短名称与长名称左缘对齐（不因 add_sized 居中），
    /// 名称与 user@host 同一列。
    #[test]
    fn ssh快捷菜单行左对齐() {
        use kittest::Queryable;

        let config_path = test_config_path("ssh-quick-align");
        let config = HostConfig {
            hosts: vec![
                HostProfile {
                    name: "短名".into(),
                    host: "10.0.0.1".into(),
                    port: 9,
                    user: "root".into(),
                    auth: Auth::Password("x".into()),
                },
                HostProfile {
                    name: "很长的主机名称对齐".into(),
                    host: "192.168.31.233".into(),
                    port: 9,
                    user: "ubuntu".into(),
                    auth: Auth::Password("x".into()),
                },
            ],
        };
        config.save(&config_path).expect("写入测试配置失败");

        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);
        harness.get_by_label(">_").click();
        harness.run_steps(6);

        let short = harness.get_by_label("短名").rect();
        let long = harness.get_by_label("很长的主机名称对齐").rect();
        let short_addr = harness.get_by_label("root@10.0.0.1").rect();
        let long_addr = harness.get_by_label("ubuntu@192.168.31.233").rect();

        assert!(
            (short.left() - long.left()).abs() < 1.0,
            "短名称与长名称应左对齐（曾被 add_sized 居中），短={:.1} 长={:.1}",
            short.left(),
            long.left()
        );
        assert!(
            (short.left() - short_addr.left()).abs() < 1.0,
            "名称与地址应同一列左对齐，名称={:.1} 地址={:.1}",
            short.left(),
            short_addr.left()
        );
        assert!(
            (long.left() - long_addr.left()).abs() < 1.0,
            "长名称与地址应同一列左对齐"
        );
        // 短名称若被居中，右缘会靠近/越过两列内容的水平中线。
        let menu_right = long.right().max(long_addr.right());
        let menu_center = (short.left() + menu_right) * 0.5;
        assert!(
            short.right() < menu_center,
            "短名称右缘应在内容中线左侧（居中时会越过中线），right={:.1} center={:.1}",
            short.right(),
            menu_center
        );

        std::fs::remove_file(&config_path).ok();
    }
}
