//! kun 应用入口。

mod app;
pub mod theme;
pub mod views;

use app::KunApp;
use eframe::egui;

/// 加载等宽主字体与中文 fallback（macOS 系统字体）。
fn setup_fonts(ctx: &egui::Context) {
    use egui::{FontData, FontDefinitions, FontFamily};

    let mut fonts = FontDefinitions::default();

    // 等宽主字体候选（依次尝试）。
    let mono_candidates = [
        "/System/Library/Fonts/SFNSMono.ttf", // SF Mono
        "/System/Library/Fonts/Menlo.ttc", // Menlo
        "/System/Library/Fonts/Supplemental/Menlo.ttc",
    ];
    // 中文 fallback 候选（用于中文等宽显示）。
    let cjk_candidates = [
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/STHeiti Light.ttc",
    ];

    let mut loaded_mono = false;
    for path in mono_candidates {
        if let Ok(bytes) = std::fs::read(path) {
            fonts.font_data.insert("kun_mono".to_owned(), std::sync::Arc::new(FontData::from_owned(bytes)));
            fonts
                .families
                .get_mut(&FontFamily::Monospace)
                .unwrap()
                .insert(0, "kun_mono".to_owned());
            loaded_mono = true;
            log::info!("加载等宽字体：{path}");
            break;
        }
    }
    if !loaded_mono {
        log::warn!("未找到系统等宽字体，使用默认字体");
    }

    // 中文 fallback 追加到等宽族末尾。
    for path in cjk_candidates {
        if let Ok(bytes) = std::fs::read(path) {
            fonts.font_data.insert("kun_cjk".to_owned(), std::sync::Arc::new(FontData::from_owned(bytes)));
            fonts.families.get_mut(&FontFamily::Monospace).unwrap().push("kun_cjk".to_owned());
            log::info!("加载中文 fallback 字体：{path}");
            break;
        }
    }

    ctx.set_fonts(fonts);
}

fn main() -> eframe::Result {
    env_logger::init();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([960.0, 640.0])
            .with_min_inner_size([400.0, 300.0])
            .with_title("kun"),
        ..Default::default()
    };

    eframe::run_native(
        "kun",
        native_options,
        Box::new(|cc| {
            setup_fonts(&cc.egui_ctx);
            Ok(Box::new(KunApp::new(cc)))
        }),
    )
}
