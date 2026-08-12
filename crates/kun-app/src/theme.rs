//! 主题与配色。
//!
//! 借鉴 MiroCode（Tauri + Vue 代码编辑器）的视觉体系：
//! macOS 原生风深色主题——分层明度差、半透明极淡边线、紫罗兰强调色。

use alacritty_terminal::vte::ansi::Rgb;
use egui::{Color32, Context, CornerRadius, Stroke, Visuals};

/// 设计 token（对齐 MiroCode `themes.css` / `tokens.css`）。
pub mod miro {
    use egui::Color32;

    // ==================== 背景分层（每层 +12 明度阶） ====================
    /// 应用底 / 编辑器背景。
    pub const BG_APP: Color32 = Color32::from_rgb(0x0a, 0x0a, 0x0d);
    /// 标题栏（+10 阶）。
    pub const BG_HEADER: Color32 = Color32::from_rgb(0x14, 0x14, 0x18);
    /// 侧栏 / 面板 / 标签栏 / 状态栏（+12 阶）。
    pub const BG_PANEL: Color32 = Color32::from_rgb(0x1c, 0x1c, 0x22);
    /// 卡片 / 弹层 / 菜单（+12 阶）。
    pub const BG_ELEVATED: Color32 = Color32::from_rgb(0x28, 0x28, 0x2f);
    /// 终端区域（紫调深色，配合 Catppuccin 调色板）。
    pub const BG_TERMINAL: Color32 = Color32::from_rgb(0x1e, 0x1e, 0x2e);

    // ==================== 边框（5% 半透明白） ====================
    /// 全部边框线。
    pub const BORDER_SUBTLE: Color32 =
        Color32::from_rgba_unmultiplied_const(0xff, 0xff, 0xff, 0x0d);

    // ==================== 文字三阶 ====================
    /// 主文本。
    pub const TEXT_PRIMARY: Color32 = Color32::from_rgb(0xf5, 0xf5, 0xf7);
    /// 次级文本。
    pub const TEXT_SECONDARY: Color32 = Color32::from_rgb(0xc7, 0xc7, 0xcc);
    /// 弱化文本。
    pub const TEXT_MUTED: Color32 = Color32::from_rgb(0x8e, 0x8e, 0x93);

    // ==================== 强调色（紫罗兰） ====================
    /// 强调色。
    pub const ACCENT: Color32 = Color32::from_rgb(0x8b, 0x5c, 0xf6);
    /// accent 16% 透明（hover/选中底色）。
    pub const ACCENT_SOFT: Color32 = Color32::from_rgba_unmultiplied_const(0x8b, 0x5c, 0xf6, 0x29);
    /// accent 上的文字。
    pub const ACCENT_FG: Color32 = Color32::from_rgb(0xff, 0xff, 0xff);
    /// focus 轮廓。
    pub const FOCUS_RING: Color32 = Color32::from_rgba_unmultiplied_const(0x8b, 0x5c, 0xf6, 0x80);

    // ==================== 状态色 ====================
    /// 成功。
    pub const SUCCESS: Color32 = Color32::from_rgb(0x34, 0xd3, 0x99);
    /// 警告。
    pub const WARNING: Color32 = Color32::from_rgb(0xfb, 0xbf, 0x24);
    /// 危险。
    pub const DANGER: Color32 = Color32::from_rgb(0xf8, 0x71, 0x71);

    // ==================== 圆角 ====================
    /// 按钮 / 输入框 / tab。
    pub const RADIUS_SM: f32 = 10.0;
    /// 列表项内部圆角。
    pub const RADIUS_ITEM: f32 = 6.0;
}

// ==================== 终端调色板（Catppuccin Mocha） ====================
/// 终端 16 色（基本色 + 亮色），按 NamedColor 判别值 0-15 排列。
pub const TERM_PALETTE_16: [Rgb; 16] = [
    // 基本色
    Rgb {
        r: 0x45,
        g: 0x47,
        b: 0x5a,
    }, // Black
    Rgb {
        r: 0xf3,
        g: 0x8b,
        b: 0xa8,
    }, // Red
    Rgb {
        r: 0xa6,
        g: 0xe3,
        b: 0xa1,
    }, // Green
    Rgb {
        r: 0xf9,
        g: 0xe2,
        b: 0xaf,
    }, // Yellow
    Rgb {
        r: 0x89,
        g: 0xb4,
        b: 0xfa,
    }, // Blue
    Rgb {
        r: 0xf5,
        g: 0xc2,
        b: 0xe7,
    }, // Magenta（粉）
    Rgb {
        r: 0x94,
        g: 0xe2,
        b: 0xd5,
    }, // Cyan
    Rgb {
        r: 0xba,
        g: 0xc2,
        b: 0xde,
    }, // White
    // 亮色
    Rgb {
        r: 0x58,
        g: 0x5b,
        b: 0x70,
    }, // BrightBlack
    Rgb {
        r: 0xf3,
        g: 0x8b,
        b: 0xa8,
    }, // BrightRed
    Rgb {
        r: 0xa6,
        g: 0xe3,
        b: 0xa1,
    }, // BrightGreen
    Rgb {
        r: 0xf9,
        g: 0xe2,
        b: 0xaf,
    }, // BrightYellow
    Rgb {
        r: 0x89,
        g: 0xb4,
        b: 0xfa,
    }, // BrightBlue
    Rgb {
        r: 0xf5,
        g: 0xc2,
        b: 0xe7,
    }, // BrightMagenta
    Rgb {
        r: 0x94,
        g: 0xe2,
        b: 0xd5,
    }, // BrightCyan
    Rgb {
        r: 0xa6,
        g: 0xad,
        b: 0xc8,
    }, // BrightWhite
];

/// 终端前景。
pub const TERM_FG: Rgb = Rgb {
    r: 0xcd,
    g: 0xd6,
    b: 0xf4,
};
/// 终端背景（紫调深色）。
pub const TERM_BG: Rgb = Rgb {
    r: 0x1e,
    g: 0x1e,
    b: 0x2e,
};
/// 终端光标色。
pub const TERM_CURSOR: Rgb = Rgb {
    r: 0xf5,
    g: 0xc2,
    b: 0xe7,
};

/// xterm 256 色表（16 基本 + 216 立方色 + 24 灰阶）。
pub fn xterm256(index: u8) -> Rgb {
    if index < 16 {
        TERM_PALETTE_16[index as usize]
    } else if index < 232 {
        let n = index - 16;
        let r = n / 36;
        let g = (n % 36) / 6;
        let b = n % 6;
        let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
        Rgb {
            r: level(r),
            g: level(g),
            b: level(b),
        }
    } else {
        let v = 8 + (index - 232) * 10;
        Rgb { r: v, g: v, b: v }
    }
}

/// 应用深色主题（MiroCode 风格）。
pub fn apply_dark(ctx: &Context) {
    let mut style = (*ctx.style_of(egui::Theme::Dark)).clone();
    let mut visuals = Visuals::dark();

    // ==================== 背景分层 ====================
    visuals.panel_fill = miro::BG_APP;
    visuals.window_fill = miro::BG_ELEVATED;
    visuals.extreme_bg_color = miro::BG_PANEL;
    visuals.faint_bg_color = miro::BG_HEADER;

    // ==================== 文字 ====================
    visuals.override_text_color = Some(miro::TEXT_PRIMARY);
    visuals.hyperlink_color = miro::ACCENT;

    // ==================== 选中 ====================
    visuals.selection.bg_fill = miro::ACCENT_SOFT;
    visuals.selection.stroke = Stroke::new(1.0, miro::ACCENT);

    // ==================== 控件（按钮/输入框等） ====================
    visuals.widgets.inactive.corner_radius = CornerRadius::same(miro::RADIUS_SM as u8);
    visuals.widgets.hovered.corner_radius = CornerRadius::same(miro::RADIUS_SM as u8);
    visuals.widgets.active.corner_radius = CornerRadius::same(miro::RADIUS_SM as u8);
    visuals.widgets.noninteractive.corner_radius = CornerRadius::same(miro::RADIUS_SM as u8);

    // 按钮：面板色底 + 细边框；hover 用 accent-soft。
    visuals.widgets.inactive.bg_fill = miro::BG_PANEL;
    visuals.widgets.inactive.weak_bg_fill = miro::BG_PANEL;
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, miro::TEXT_PRIMARY);
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, miro::BORDER_SUBTLE);

    visuals.widgets.hovered.bg_fill = miro::ACCENT_SOFT;
    visuals.widgets.hovered.weak_bg_fill = miro::ACCENT_SOFT;
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, miro::TEXT_PRIMARY);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::TRANSPARENT);

    visuals.widgets.active.bg_fill = miro::ACCENT_SOFT;
    visuals.widgets.active.weak_bg_fill = miro::ACCENT_SOFT;
    visuals.widgets.active.fg_stroke = Stroke::new(1.0, miro::TEXT_PRIMARY);
    visuals.widgets.active.bg_stroke = Stroke::new(1.0, miro::ACCENT);

    // 非交互控件（label 等）。
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, miro::TEXT_SECONDARY);
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, miro::BORDER_SUBTLE);

    // ==================== 间距 ====================
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 4.0);
    style.spacing.interact_size = egui::vec2(28.0, 24.0);

    // ==================== 窗口（对话框）圆角与阴影 ====================
    visuals.window_corner_radius = CornerRadius::same(12);
    visuals.window_shadow = egui::Shadow {
        offset: [0, 8],
        blur: 24,
        spread: 0,
        color: Color32::from_rgba_unmultiplied(0x00, 0x00, 0x00, 0xb0),
    };

    style.visuals = visuals;
    ctx.set_style_of(egui::Theme::Dark, style);
}
