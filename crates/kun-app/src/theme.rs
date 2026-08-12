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
pub fn xterm256(index: u8, palette: [Rgb; 16]) -> Rgb {
    if index < 16 {
        palette[index as usize]
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

// ==================== 多主题（对齐 MiroCode 四套皮肤） ====================

/// 一套完整主题（UI token + 终端调色板）。
pub struct Theme {
    /// 显示名称。
    pub name: &'static str,
    /// 是否为浅色主题（用于系统标题栏/滚动条跟随）。
    pub light: bool,
    // ---- UI token ----
    pub bg_app: Color32,
    pub bg_header: Color32,
    pub bg_panel: Color32,
    pub bg_elevated: Color32,
    pub border: Color32,
    pub text_primary: Color32,
    pub text_secondary: Color32,
    pub text_muted: Color32,
    pub accent: Color32,
    pub accent_soft: Color32,
    pub success: Color32,
    pub danger: Color32,
    // ---- 终端调色板 ----
    pub term_bg: Rgb,
    pub term_fg: Rgb,
    pub term_cursor: Rgb,
    pub term_palette: [Rgb; 16],
}

/// 当前主题索引（静态，view 层按需读取）。
use std::sync::atomic::{AtomicUsize, Ordering};
static CURRENT_THEME: AtomicUsize = AtomicUsize::new(0);

/// 当前主题。
pub fn current_theme() -> &'static Theme {
    &THEMES[CURRENT_THEME.load(Ordering::Relaxed)]
}

/// 设置当前主题索引并应用。
pub fn set_theme(ctx: &Context, index: usize) {
    CURRENT_THEME.store(index.min(THEMES.len() - 1), Ordering::Relaxed);
    apply_theme(ctx, current_theme());
}

/// 应用指定主题。
pub fn apply_theme(ctx: &Context, theme: &Theme) {
    // 同步系统主题（macOS 标题栏/滚动条跟随浅色/深色）。
    ctx.set_theme(if theme.light {
        egui::ThemePreference::Light
    } else {
        egui::ThemePreference::Dark
    });
    let mut style = (*ctx.style_of(egui::Theme::Dark)).clone();
    let mut visuals = Visuals::dark();

    visuals.panel_fill = theme.bg_app;
    visuals.window_fill = theme.bg_elevated;
    visuals.extreme_bg_color = theme.bg_panel;
    visuals.faint_bg_color = theme.bg_header;
    visuals.override_text_color = Some(theme.text_primary);
    visuals.hyperlink_color = theme.accent;
    visuals.selection.bg_fill = theme.accent_soft;
    visuals.selection.stroke = Stroke::new(1.0, theme.accent);

    for w in [
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.noninteractive,
    ] {
        w.corner_radius = CornerRadius::same(miro::RADIUS_SM as u8);
    }
    visuals.widgets.inactive.bg_fill = theme.bg_panel;
    visuals.widgets.inactive.weak_bg_fill = theme.bg_panel;
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, theme.text_primary);
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, theme.border);
    visuals.widgets.hovered.bg_fill = theme.accent_soft;
    visuals.widgets.hovered.weak_bg_fill = theme.accent_soft;
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, theme.text_primary);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::TRANSPARENT);
    visuals.widgets.active.bg_fill = theme.accent_soft;
    visuals.widgets.active.weak_bg_fill = theme.accent_soft;
    visuals.widgets.active.fg_stroke = Stroke::new(1.0, theme.text_primary);
    visuals.widgets.active.bg_stroke = Stroke::new(1.0, theme.accent);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, theme.text_secondary);
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, theme.border);

    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 4.0);
    style.spacing.interact_size = egui::vec2(28.0, 24.0);
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

// ---- 终端调色板：四套 ----

/// miro-dark：Catppuccin Mocha（默认，紫调）。
const PALETTE_MIRO: [Rgb; 16] = [
    Rgb {
        r: 0x45,
        g: 0x47,
        b: 0x5a,
    },
    Rgb {
        r: 0xf3,
        g: 0x8b,
        b: 0xa8,
    },
    Rgb {
        r: 0xa6,
        g: 0xe3,
        b: 0xa1,
    },
    Rgb {
        r: 0xf9,
        g: 0xe2,
        b: 0xaf,
    },
    Rgb {
        r: 0x89,
        g: 0xb4,
        b: 0xfa,
    },
    Rgb {
        r: 0xf5,
        g: 0xc2,
        b: 0xe7,
    },
    Rgb {
        r: 0x94,
        g: 0xe2,
        b: 0xd5,
    },
    Rgb {
        r: 0xba,
        g: 0xc2,
        b: 0xde,
    },
    Rgb {
        r: 0x58,
        g: 0x5b,
        b: 0x70,
    },
    Rgb {
        r: 0xf3,
        g: 0x8b,
        b: 0xa8,
    },
    Rgb {
        r: 0xa6,
        g: 0xe3,
        b: 0xa1,
    },
    Rgb {
        r: 0xf9,
        g: 0xe2,
        b: 0xaf,
    },
    Rgb {
        r: 0x89,
        g: 0xb4,
        b: 0xfa,
    },
    Rgb {
        r: 0xf5,
        g: 0xc2,
        b: 0xe7,
    },
    Rgb {
        r: 0x94,
        g: 0xe2,
        b: 0xd5,
    },
    Rgb {
        r: 0xa6,
        g: 0xad,
        b: 0xc8,
    },
];

/// midnight：深蓝（Catppuccin 蓝调）。
const PALETTE_MIDNIGHT: [Rgb; 16] = [
    Rgb {
        r: 0x3b,
        g: 0x42,
        b: 0x68,
    },
    Rgb {
        r: 0xf7,
        g: 0x76,
        b: 0x8e,
    },
    Rgb {
        r: 0x9e,
        g: 0xce,
        b: 0x6a,
    },
    Rgb {
        r: 0xe0,
        g: 0xaf,
        b: 0x68,
    },
    Rgb {
        r: 0x7a,
        g: 0xa2,
        b: 0xf7,
    },
    Rgb {
        r: 0xbb,
        g: 0x9a,
        b: 0xf7,
    },
    Rgb {
        r: 0x7d,
        g: 0xcf,
        b: 0xff,
    },
    Rgb {
        r: 0xa9,
        g: 0xb1,
        b: 0xd6,
    },
    Rgb {
        r: 0x41,
        g: 0x48,
        b: 0x68,
    },
    Rgb {
        r: 0xf7,
        g: 0x76,
        b: 0x8e,
    },
    Rgb {
        r: 0x9e,
        g: 0xce,
        b: 0x6a,
    },
    Rgb {
        r: 0xe0,
        g: 0xaf,
        b: 0x68,
    },
    Rgb {
        r: 0x7a,
        g: 0xa2,
        b: 0xf7,
    },
    Rgb {
        r: 0xbb,
        g: 0x9a,
        b: 0xf7,
    },
    Rgb {
        r: 0x7d,
        g: 0xcf,
        b: 0xff,
    },
    Rgb {
        r: 0xc0,
        g: 0xca,
        b: 0xf5,
    },
];

/// cyberpunk：霓虹（紫粉 + 青）。
const PALETTE_CYBER: [Rgb; 16] = [
    Rgb {
        r: 0x2a,
        g: 0x1a,
        b: 0x3e,
    },
    Rgb {
        r: 0xff,
        g: 0x5c,
        b: 0x8a,
    },
    Rgb {
        r: 0x00,
        g: 0xff,
        b: 0x9f,
    },
    Rgb {
        r: 0xff,
        g: 0xd3,
        b: 0x00,
    },
    Rgb {
        r: 0x00,
        g: 0xd4,
        b: 0xff,
    },
    Rgb {
        r: 0xff,
        g: 0x71,
        b: 0xce,
    },
    Rgb {
        r: 0x00,
        g: 0xff,
        b: 0xe0,
    },
    Rgb {
        r: 0xe0,
        g: 0xe0,
        b: 0xff,
    },
    Rgb {
        r: 0x4a,
        g: 0x2a,
        b: 0x6e,
    },
    Rgb {
        r: 0xff,
        g: 0x5c,
        b: 0x8a,
    },
    Rgb {
        r: 0x00,
        g: 0xff,
        b: 0x9f,
    },
    Rgb {
        r: 0xff,
        g: 0xd3,
        b: 0x00,
    },
    Rgb {
        r: 0x00,
        g: 0xd4,
        b: 0xff,
    },
    Rgb {
        r: 0xff,
        g: 0x71,
        b: 0xce,
    },
    Rgb {
        r: 0x00,
        g: 0xff,
        b: 0xe0,
    },
    Rgb {
        r: 0xff,
        g: 0xff,
        b: 0xff,
    },
];

/// dawn：浅色（日间）。
const PALETTE_DAWN: [Rgb; 16] = [
    Rgb {
        r: 0x8a,
        g: 0x8a,
        b: 0x8a,
    },
    Rgb {
        r: 0xd2,
        g: 0x4d,
        b: 0x3d,
    },
    Rgb {
        r: 0x2e,
        g: 0x8b,
        b: 0x57,
    },
    Rgb {
        r: 0xb5,
        g: 0x8f,
        b: 0x00,
    },
    Rgb {
        r: 0x25,
        g: 0x63,
        b: 0xeb,
    },
    Rgb {
        r: 0xa5,
        g: 0x5e,
        b: 0xe0,
    },
    Rgb {
        r: 0x0e,
        g: 0x9f,
        b: 0x9a,
    },
    Rgb {
        r: 0x4a,
        g: 0x4a,
        b: 0x4a,
    },
    Rgb {
        r: 0x9a,
        g: 0x9a,
        b: 0x9a,
    },
    Rgb {
        r: 0xd2,
        g: 0x4d,
        b: 0x3d,
    },
    Rgb {
        r: 0x2e,
        g: 0x8b,
        b: 0x57,
    },
    Rgb {
        r: 0xb5,
        g: 0x8f,
        b: 0x00,
    },
    Rgb {
        r: 0x25,
        g: 0x63,
        b: 0xeb,
    },
    Rgb {
        r: 0xa5,
        g: 0x5e,
        b: 0xe0,
    },
    Rgb {
        r: 0x0e,
        g: 0x9f,
        b: 0x9a,
    },
    Rgb {
        r: 0x4a,
        g: 0x4a,
        b: 0x4a,
    },
];

/// 四套主题（对齐 MiroCode：miro-dark / dawn / midnight / cyberpunk）。
pub static THEMES: [Theme; 4] = [
    Theme {
        name: "Miro 深色",
        light: false,
        bg_app: Color32::from_rgb(0x0a, 0x0a, 0x0d),
        bg_header: Color32::from_rgb(0x14, 0x14, 0x18),
        bg_panel: Color32::from_rgb(0x1c, 0x1c, 0x22),
        bg_elevated: Color32::from_rgb(0x28, 0x28, 0x2f),
        border: Color32::from_rgba_unmultiplied_const(0xff, 0xff, 0xff, 0x0d),
        text_primary: Color32::from_rgb(0xf5, 0xf5, 0xf7),
        text_secondary: Color32::from_rgb(0xc7, 0xc7, 0xcc),
        text_muted: Color32::from_rgb(0x8e, 0x8e, 0x93),
        accent: Color32::from_rgb(0x8b, 0x5c, 0xf6),
        accent_soft: Color32::from_rgba_unmultiplied_const(0x8b, 0x5c, 0xf6, 0x29),
        success: Color32::from_rgb(0x34, 0xd3, 0x99),
        danger: Color32::from_rgb(0xf8, 0x71, 0x71),
        term_bg: Rgb {
            r: 0x11,
            g: 0x11,
            b: 0x1b,
        }, // crust（更深的紫调）
        term_fg: Rgb {
            r: 0xcd,
            g: 0xd6,
            b: 0xf4,
        },
        term_cursor: Rgb {
            r: 0xf5,
            g: 0xc2,
            b: 0xe7,
        },
        term_palette: PALETTE_MIRO,
    },
    Theme {
        name: "Dawn 浅色",
        light: true,
        bg_app: Color32::from_rgb(0xf5, 0xf5, 0xf7),
        bg_header: Color32::from_rgb(0xec, 0xec, 0xf0),
        bg_panel: Color32::from_rgb(0xe8, 0xe8, 0xed),
        bg_elevated: Color32::from_rgb(0xff, 0xff, 0xff),
        border: Color32::from_rgba_unmultiplied_const(0x00, 0x00, 0x00, 0x14),
        text_primary: Color32::from_rgb(0x1d, 0x1d, 0x1f),
        text_secondary: Color32::from_rgb(0x3f, 0x3f, 0x46),
        text_muted: Color32::from_rgb(0x71, 0x71, 0x7a),
        accent: Color32::from_rgb(0x25, 0x63, 0xeb),
        accent_soft: Color32::from_rgba_unmultiplied_const(0x25, 0x63, 0xeb, 0x1f),
        success: Color32::from_rgb(0x16, 0xa3, 0x4a),
        danger: Color32::from_rgb(0xd2, 0x4d, 0x3d),
        term_bg: Rgb {
            r: 0xff,
            g: 0xff,
            b: 0xff,
        },
        term_fg: Rgb {
            r: 0x1d,
            g: 0x1d,
            b: 0x1f,
        },
        term_cursor: Rgb {
            r: 0x25,
            g: 0x63,
            b: 0xeb,
        },
        term_palette: PALETTE_DAWN,
    },
    Theme {
        name: "Midnight 深蓝",
        light: false,
        bg_app: Color32::from_rgb(0x07, 0x0b, 0x16),
        bg_header: Color32::from_rgb(0x0d, 0x13, 0x22),
        bg_panel: Color32::from_rgb(0x11, 0x18, 0x2b),
        bg_elevated: Color32::from_rgb(0x1a, 0x24, 0x3d),
        border: Color32::from_rgba_unmultiplied_const(0xff, 0xff, 0xff, 0x0d),
        text_primary: Color32::from_rgb(0xe6, 0xed, 0xf7),
        text_secondary: Color32::from_rgb(0xb4, 0xc0, 0xd6),
        text_muted: Color32::from_rgb(0x7f, 0x8d, 0xa8),
        accent: Color32::from_rgb(0x38, 0xbd, 0xf8),
        accent_soft: Color32::from_rgba_unmultiplied_const(0x38, 0xbd, 0xf8, 0x26),
        success: Color32::from_rgb(0x2d, 0xd4, 0xbf),
        danger: Color32::from_rgb(0xf4, 0x71, 0x71),
        term_bg: Rgb {
            r: 0x0a,
            g: 0x0f,
            b: 0x1e,
        },
        term_fg: Rgb {
            r: 0xc0,
            g: 0xca,
            b: 0xf5,
        },
        term_cursor: Rgb {
            r: 0x7d,
            g: 0xcf,
            b: 0xff,
        },
        term_palette: PALETTE_MIDNIGHT,
    },
    Theme {
        name: "Cyberpunk 霓虹",
        light: false,
        bg_app: Color32::from_rgb(0x0b, 0x05, 0x0f),
        bg_header: Color32::from_rgb(0x16, 0x0a, 0x1e),
        bg_panel: Color32::from_rgb(0x1e, 0x0e, 0x2a),
        bg_elevated: Color32::from_rgb(0x2c, 0x16, 0x3e),
        border: Color32::from_rgba_unmultiplied_const(0xff, 0x71, 0xce, 0x2e),
        text_primary: Color32::from_rgb(0xf0, 0xe6, 0xff),
        text_secondary: Color32::from_rgb(0xcf, 0xb8, 0xe8),
        text_muted: Color32::from_rgb(0x9a, 0x7f, 0xb8),
        accent: Color32::from_rgb(0x22, 0xd3, 0xee),
        accent_soft: Color32::from_rgba_unmultiplied_const(0x22, 0xd3, 0xee, 0x29),
        success: Color32::from_rgb(0x00, 0xff, 0x9f),
        danger: Color32::from_rgb(0xff, 0x5c, 0x8a),
        term_bg: Rgb {
            r: 0x12,
            g: 0x08,
            b: 0x1a,
        },
        term_fg: Rgb {
            r: 0xe0,
            g: 0xe0,
            b: 0xff,
        },
        term_cursor: Rgb {
            r: 0x00,
            g: 0xff,
            b: 0xe0,
        },
        term_palette: PALETTE_CYBER,
    },
];
