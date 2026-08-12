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

    // ==================== 等宽主字体与中文 fallback（按平台） ====================
    #[cfg(target_os = "macos")]
    let mono_candidates = [
        "/System/Library/Fonts/SFNSMono.ttf", // SF Mono
        "/System/Library/Fonts/Menlo.ttc",    // Menlo
        "/System/Library/Fonts/Supplemental/Menlo.ttc",
    ];
    #[cfg(target_os = "windows")]
    let mono_candidates = [
        "C:\\Windows\\Fonts\\CascadiaMono.ttf", // Cascadia Code
        "C:\\Windows\\Fonts\\consola.ttf",      // Consolas
    ];
    #[cfg(all(unix, not(target_os = "macos")))]
    let mono_candidates = [
        "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf", // DejaVu Sans Mono
        "/usr/share/fonts/truetype/noto/NotoSansMono-Regular.ttf",
        "/usr/share/fonts/noto/NotoSansMono-Regular.ttf",
    ];

    #[cfg(target_os = "macos")]
    let cjk_candidates = [
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/STHeiti Light.ttc",
    ];
    #[cfg(target_os = "windows")]
    let cjk_candidates = [
        "C:\\Windows\\Fonts\\msyh.ttc", // 微软雅黑
        "C:\\Windows\\Fonts\\msyhbd.ttc",
    ];
    #[cfg(all(unix, not(target_os = "macos")))]
    let cjk_candidates = [
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", // Noto Sans CJK
        "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",         // 文泉驿微米黑
        "/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf",
    ];

    let mut loaded_mono = false;
    for path in mono_candidates {
        if let Ok(bytes) = std::fs::read(path) {
            fonts.font_data.insert(
                "kun_mono".to_owned(),
                std::sync::Arc::new(FontData::from_owned(bytes)),
            );
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
            fonts.font_data.insert(
                "kun_cjk".to_owned(),
                std::sync::Arc::new(FontData::from_owned(bytes)),
            );
            for family in [FontFamily::Proportional, FontFamily::Monospace] {
                fonts
                    .families
                    .get_mut(&family)
                    .unwrap()
                    .push("kun_cjk".to_owned());
            }
            log::info!("加载中文 fallback 字体：{path}");
            break;
        }
    }

    ctx.set_fonts(fonts);
}

fn main() -> eframe::Result {
    env_logger::init();

    // ==================== 崩溃日志捕获 ====================
    // panic 信息写入文件，便于排查闪退问题（闪退时 stderr 不可见）。
    let panic_log = std::env::temp_dir().join("kun-panic.log");
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "未知位置".into());
        let backtrace = std::backtrace::Backtrace::force_capture();
        let msg = format!(
            "=== kun 崩溃时间：{} ===\n位置：{location}\n信息：{info}\n堆栈：\n{backtrace}\n",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs().to_string())
                .unwrap_or_else(|_| "未知".into())
        );
        let _ = std::fs::write(&panic_log, &msg);
        eprintln!("kun 发生崩溃，详情已写入 {}", panic_log.display());
    }));

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
