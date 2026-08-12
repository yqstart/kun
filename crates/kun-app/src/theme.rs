//! 主题与配色（参照 Dracula 系深色主题）。

use egui::{Context, Visuals};

/// Dracula 配色。
pub mod dracula {
    use egui::Color32;

    pub const BG: Color32 = Color32::from_rgb(0x28, 0x2a, 0x36);
    pub const BG_DARK: Color32 = Color32::from_rgb(0x21, 0x23, 0x2e);
    pub const BG_LIGHT: Color32 = Color32::from_rgb(0x44, 0x47, 0x5a);
    pub const FG: Color32 = Color32::from_rgb(0xf8, 0xf8, 0xf2);
    pub const COMMENT: Color32 = Color32::from_rgb(0x62, 0x72, 0xa4);
    pub const CYAN: Color32 = Color32::from_rgb(0x8b, 0xe9, 0xfd);
    pub const GREEN: Color32 = Color32::from_rgb(0x50, 0xfa, 0x7b);
    pub const ORANGE: Color32 = Color32::from_rgb(0xff, 0xb8, 0x6c);
    pub const PINK: Color32 = Color32::from_rgb(0xff, 0x79, 0xc6);
    pub const PURPLE: Color32 = Color32::from_rgb(0xbd, 0x93, 0xf9);
    pub const RED: Color32 = Color32::from_rgb(0xff, 0x55, 0x55);
    pub const YELLOW: Color32 = Color32::from_rgb(0xf1, 0xfa, 0x8c);
}

/// 应用深色主题。
pub fn apply_dark(ctx: &Context) {
    let mut style = (*ctx.style_of(egui::Theme::Dark)).clone();
    let visuals = Visuals::dark();

    style.visuals = visuals;
    style.visuals.panel_fill = dracula::BG;
    style.visuals.window_fill = dracula::BG;
    style.visuals.extreme_bg_color = dracula::BG_DARK;
    style.visuals.faint_bg_color = dracula::BG_DARK;
    style.visuals.widgets.inactive.bg_fill = dracula::BG_LIGHT;
    style.visuals.widgets.hovered.bg_fill = dracula::BG_LIGHT;
    style.visuals.widgets.active.bg_fill = dracula::BG_LIGHT;
    style.visuals.selection.bg_fill = dracula::PURPLE;
    style.visuals.selection.stroke.color = dracula::FG;
    style.visuals.hyperlink_color = dracula::CYAN;
    style.visuals.override_text_color = Some(dracula::FG);
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(8.0, 4.0);

    ctx.set_style_of(egui::Theme::Dark, style);
}
