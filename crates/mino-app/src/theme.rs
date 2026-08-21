//! 主题与配色。
//!
//! 视觉体系参照 Tabby 终端：深蓝灰分层背景、极淡半透明边线、扁平控件、
//! 低对比 hover 高亮、小圆角与紧凑排版。三套深色皮肤共用同一套结构。

use alacritty_terminal::vte::ansi::Rgb;
use egui::{Color32, Context, CornerRadius, Stroke, Visuals};

/// 设计 token（默认深色主题基准）。
pub mod tokens {
    use egui::Color32;

    // ==================== 背景分层（Mino 深空控制台） ====================
    // 视觉层级从“灰色面板堆叠”改为深空底 + 蓝紫表面 + 细微青色边界，
    // 让设置、主机卡片与终端属于同一套界面语言。
    pub const BG_APP: Color32 = Color32::from_rgb(0x07, 0x10, 0x1d);
    pub const BG_HEADER: Color32 = Color32::from_rgb(0x06, 0x0d, 0x18);
    pub const BG_PANEL: Color32 = Color32::from_rgb(0x0d, 0x1a, 0x2b);
    pub const BG_ELEVATED: Color32 = Color32::from_rgb(0x12, 0x24, 0x3a);
    pub const BG_TERMINAL: Color32 = Color32::from_rgb(0x06, 0x0e, 0x1a);

    pub const BORDER_SUBTLE: Color32 =
        Color32::from_rgba_unmultiplied_const(0x72, 0xc9, 0xe8, 0x25);
    pub const GRID_LINE: Color32 = Color32::from_rgba_unmultiplied_const(0x68, 0xc8, 0xe8, 0x07);

    pub const TEXT_PRIMARY: Color32 = Color32::from_rgb(0xe6, 0xf4, 0xff);
    pub const TEXT_SECONDARY: Color32 = Color32::from_rgb(0xa5, 0xbd, 0xd2);
    pub const TEXT_MUTED: Color32 = Color32::from_rgb(0x62, 0x7d, 0x98);

    // Mino 品牌强调色：冷青作为主动作，紫色作为状态/层级辅助。
    pub const ACCENT: Color32 = Color32::from_rgb(0x65, 0xe4, 0xff);
    pub const ACCENT_2: Color32 = Color32::from_rgb(0xa7, 0x8b, 0xfa);
    pub const ACCENT_SOFT: Color32 = Color32::from_rgba_unmultiplied_const(0x65, 0xe4, 0xff, 0x25);
    pub const ACCENT_FG: Color32 = Color32::from_rgb(0xff, 0xff, 0xff);
    pub const FOCUS_RING: Color32 = Color32::from_rgba_unmultiplied_const(0x65, 0xe4, 0xff, 0x90);

    pub const SUCCESS: Color32 = Color32::from_rgb(0x5c, 0xb8, 0x5c);
    pub const WARNING: Color32 = Color32::from_rgb(0xf0, 0xad, 0x4e);
    pub const DANGER: Color32 = Color32::from_rgb(0xd9, 0x53, 0x4f);

    pub const RADIUS_SM: f32 = 9.0;
    pub const RADIUS_ITEM: f32 = 7.0;
}

// ==================== 终端调色板（Catppuccin Mocha 基准） ====================
pub const TERM_PALETTE_16: [Rgb; 16] = [
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

pub const TERM_FG: Rgb = Rgb {
    r: 0xd5,
    g: 0xdc,
    b: 0xe4,
};
pub const TERM_BG: Rgb = Rgb {
    r: 0x0b,
    g: 0x10,
    b: 0x16,
};
pub const TERM_CURSOR: Rgb = Rgb {
    r: 0xf2,
    g: 0xf5,
    b: 0xf8,
};

/// xterm 256 色表固定部分（index ≥ 16：216 立方色 + 24 灰阶，与主题调色板无关）。
/// 一次计算后查表，避免逐 cell 现算乘除（全彩色屏时每帧上万次）。
static XTERM_FIXED: std::sync::OnceLock<[Rgb; 240]> = std::sync::OnceLock::new();

/// xterm 256 色表（16 基本 + 216 立方色 + 24 灰阶）。
pub fn xterm256(index: u8, palette: [Rgb; 16]) -> Rgb {
    if index < 16 {
        palette[index as usize]
    } else {
        let table = XTERM_FIXED.get_or_init(|| {
            std::array::from_fn(|i| {
                let n = i as u8 + 16;
                if n < 232 {
                    let n2 = n - 16;
                    let r = n2 / 36;
                    let g = (n2 % 36) / 6;
                    let b = n2 % 6;
                    let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
                    Rgb {
                        r: level(r),
                        g: level(g),
                        b: level(b),
                    }
                } else {
                    let v = 8 + (n - 232) * 10;
                    Rgb { r: v, g: v, b: v }
                }
            })
        });
        table[(index - 16) as usize]
    }
}

// ==================== 多主题 ====================

pub struct Theme {
    pub name: &'static str,
    pub light: bool,
    pub bg_app: Color32,
    pub bg_header: Color32,
    pub bg_panel: Color32,
    pub bg_elevated: Color32,
    pub border: Color32,
    pub text_primary: Color32,
    pub text_secondary: Color32,
    pub text_muted: Color32,
    pub accent: Color32,
    pub accent2: Color32,
    pub accent_soft: Color32,
    pub success: Color32,
    pub warning: Color32,
    pub danger: Color32,
    pub term_bg: Rgb,
    pub term_fg: Rgb,
    pub term_cursor: Rgb,
    pub term_palette: [Rgb; 16],
}

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
static CURRENT_THEME: AtomicUsize = AtomicUsize::new(0);
static THEME_REVISION: AtomicU64 = AtomicU64::new(0);

pub fn current_theme() -> &'static Theme {
    &THEMES[CURRENT_THEME.load(Ordering::Relaxed)]
}

/// 当前主题修订号；主题改变时递增，供终端等缓存消费者失效。
pub fn theme_revision() -> u64 {
    THEME_REVISION.load(Ordering::Acquire)
}

pub fn set_theme(ctx: &Context, index: usize) {
    let index = index.min(THEMES.len() - 1);
    if CURRENT_THEME.swap(index, Ordering::AcqRel) != index {
        THEME_REVISION.fetch_add(1, Ordering::AcqRel);
    }
    apply_theme(ctx, current_theme());
}

pub fn apply_theme(ctx: &Context, theme: &Theme) {
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
        w.corner_radius = CornerRadius::same(tokens::RADIUS_SM as u8);
    }
    visuals.widgets.inactive.bg_fill = theme.bg_panel;
    visuals.widgets.inactive.weak_bg_fill = theme.bg_panel;
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, theme.text_primary);
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, theme.border);

    // 科技感 hover：青色边界的低透明度提亮，避免所有控件都变成一块白底。
    let hover_overlay =
        Color32::from_rgba_unmultiplied(theme.accent.r(), theme.accent.g(), theme.accent.b(), 16);
    visuals.widgets.hovered.bg_fill = hover_overlay;
    visuals.widgets.hovered.weak_bg_fill = hover_overlay;
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, theme.text_primary);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::TRANSPARENT);

    visuals.widgets.active.bg_fill = theme.accent_soft;
    visuals.widgets.active.weak_bg_fill = theme.accent_soft;
    visuals.widgets.active.fg_stroke = Stroke::new(1.0, theme.text_primary);
    visuals.widgets.active.bg_stroke = Stroke::new(1.0, theme.accent);

    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, theme.text_secondary);
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, theme.border);

    style.spacing.item_spacing = egui::vec2(9.0, 7.0);
    style.spacing.button_padding = egui::vec2(11.0, 5.0);
    style.spacing.interact_size = egui::vec2(30.0, 26.0);

    visuals.window_corner_radius = CornerRadius::same(14);
    visuals.window_shadow = egui::Shadow {
        offset: [0, 6],
        blur: 24,
        spread: 0,
        color: Color32::from_rgba_unmultiplied(0x00, 0x04, 0x10, 0xd0),
    };
    visuals.popup_shadow = egui::Shadow {
        offset: [0, 4],
        blur: 16,
        spread: 0,
        color: Color32::from_rgba_unmultiplied(0x00, 0x04, 0x10, 0xb0),
    };

    // 所有可点击控件 hover 时显示小手光标（egui 默认不设置；
    // Button/SelectableLabel/ComboBox 等标准控件读取该字段）。
    // 注意：TextEdit 内部显式设置 I-beam，不受影响。
    visuals.interact_cursor = Some(egui::CursorIcon::PointingHand);

    style.visuals = visuals;
    ctx.set_style_of(egui::Theme::Dark, style);
}

// ---- 终端调色板：三套 ----

const PALETTE_WARP: [Rgb; 16] = TERM_PALETTE_16;

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

pub static THEMES: [Theme; 3] = [
    Theme {
        name: "深色",
        light: false,
        bg_app: tokens::BG_APP,
        bg_header: tokens::BG_HEADER,
        bg_panel: tokens::BG_PANEL,
        bg_elevated: tokens::BG_ELEVATED,
        border: tokens::BORDER_SUBTLE,
        text_primary: tokens::TEXT_PRIMARY,
        text_secondary: tokens::TEXT_SECONDARY,
        text_muted: tokens::TEXT_MUTED,
        accent: tokens::ACCENT,
        accent2: tokens::ACCENT_2,
        accent_soft: tokens::ACCENT_SOFT,
        success: tokens::SUCCESS,
        warning: tokens::WARNING,
        danger: tokens::DANGER,
        term_bg: TERM_BG,
        term_fg: TERM_FG,
        term_cursor: TERM_CURSOR,
        term_palette: PALETTE_WARP,
    },
    Theme {
        name: "深蓝",
        light: false,
        bg_app: Color32::from_rgb(0x06, 0x0a, 0x14),
        bg_header: Color32::from_rgb(0x0c, 0x12, 0x20),
        bg_panel: Color32::from_rgb(0x10, 0x18, 0x2b),
        bg_elevated: Color32::from_rgb(0x18, 0x23, 0x3d),
        border: Color32::from_rgba_unmultiplied_const(0xff, 0xff, 0xff, 0x10),
        text_primary: Color32::from_rgb(0xe8, 0xee, 0xf8),
        text_secondary: Color32::from_rgb(0xa9, 0xb6, 0xce),
        text_muted: Color32::from_rgb(0x6e, 0x7c, 0x99),
        accent: Color32::from_rgb(0x3b, 0x82, 0xf6),
        accent2: Color32::from_rgb(0x22, 0xd3, 0xee),
        accent_soft: Color32::from_rgba_unmultiplied_const(0x3b, 0x82, 0xf6, 0x22),
        success: Color32::from_rgb(0x2d, 0xd4, 0xbf),
        warning: Color32::from_rgb(0xfb, 0xbf, 0x24),
        danger: Color32::from_rgb(0xf4, 0x71, 0x71),
        term_bg: Rgb {
            r: 0x07,
            g: 0x0c,
            b: 0x17,
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
        name: "霓虹",
        light: false,
        bg_app: Color32::from_rgb(0x0a, 0x04, 0x0f),
        bg_header: Color32::from_rgb(0x14, 0x0a, 0x1d),
        bg_panel: Color32::from_rgb(0x1b, 0x0d, 0x28),
        bg_elevated: Color32::from_rgb(0x29, 0x15, 0x40),
        border: Color32::from_rgba_unmultiplied_const(0xff, 0x71, 0xce, 0x20),
        text_primary: Color32::from_rgb(0xf2, 0xe9, 0xff),
        text_secondary: Color32::from_rgb(0xcd, 0xb6, 0xe8),
        text_muted: Color32::from_rgb(0x96, 0x7f, 0xb0),
        accent: Color32::from_rgb(0xff, 0x4d, 0xcd),
        accent2: Color32::from_rgb(0x00, 0xf0, 0xff),
        accent_soft: Color32::from_rgba_unmultiplied_const(0xff, 0x4d, 0xcd, 0x22),
        success: Color32::from_rgb(0x00, 0xff, 0x9f),
        warning: Color32::from_rgb(0xff, 0xd3, 0x00),
        danger: Color32::from_rgb(0xff, 0x5c, 0x8a),
        term_bg: Rgb {
            r: 0x0f,
            g: 0x06,
            b: 0x17,
        },
        term_fg: Rgb {
            r: 0xe2,
            g: 0xe2,
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
