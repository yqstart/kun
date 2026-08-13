//! kun 应用入口。

pub mod anim;
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

    // ==================== 等宽符号 fallback（Menlo） ====================
    // SF Mono 缺少 ➜（U+279C）、❯（U+276F）等常用 zsh 提示符符号，
    // 缺字形会被 egui 渲染为 `?` 替换符（用户反馈提示符显示错乱）。
    // Menlo 同为等宽字体且完整覆盖这些符号（宽度一致，行内布局不会错位），
    // 追加到等宽族、排在中文 fallback 之前。
    #[cfg(target_os = "macos")]
    let symbol_candidates = [
        "/System/Library/Fonts/Menlo.ttc",
        "/System/Library/Fonts/Supplemental/Menlo.ttc",
    ];
    #[cfg(not(target_os = "macos"))]
    let symbol_candidates: [&str; 0] = [];

    for path in symbol_candidates {
        if let Ok(bytes) = std::fs::read(path) {
            fonts.font_data.insert(
                "kun_mono_sym".to_owned(),
                std::sync::Arc::new(FontData::from_owned(bytes)),
            );
            fonts
                .families
                .get_mut(&FontFamily::Monospace)
                .unwrap()
                .push("kun_mono_sym".to_owned());
            log::info!("加载等宽符号 fallback 字体：{path}");
            break;
        }
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

/// 加载应用图标（assets/icon.png → IconData）。
/// eframe 在 macOS 上会通过 NSApp 将其设置为 Dock 图标（运行时设置，
/// 无 .app bundle 的 debug 构建也能生效）。
fn load_icon() -> Option<egui::IconData> {
    let bytes = include_bytes!("../assets/icon.png");
    match image::load_from_memory_with_format(bytes, image::ImageFormat::Png) {
        Ok(img) => {
            let rgba = img.to_rgba8();
            let (width, height) = rgba.dimensions();
            Some(egui::IconData {
                rgba: rgba.into_raw(),
                width,
                height,
            })
        }
        Err(e) => {
            log::warn!("加载应用图标失败：{e}");
            None
        }
    }
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

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([960.0, 640.0])
        .with_min_inner_size([400.0, 300.0])
        .with_title("kun");
    // 设置应用图标（macOS Dock 图标由 eframe 运行时写入 NSApp）。
    if let Some(icon) = load_icon() {
        viewport = viewport.with_icon(icon);
    }
    let native_options = eframe::NativeOptions {
        viewport,
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

#[cfg(test)]
mod font_tests {
    use super::*;
    use egui::FontFamily;

    /// 等宽字体链应包含符号 fallback（Menlo），
    /// 否则 ➜/❯ 等 zsh 提示符符号会渲染为 `?` 替换符（回归测试）。
    #[test]
    fn 等宽字体链含符号fallback() {
        let ctx = egui::Context::default();
        // 先跑一帧初始化字体系统（Context::fonts 在首次 run 前不可用）；
        // set_fonts 延迟到下一帧 begin_pass 生效，因此跑两帧。
        let mut output = ctx.run_ui(egui::RawInput::default(), |ctx| {
            setup_fonts(ctx);
        });
        output.textures_delta.clear();
        let mut output = ctx.run_ui(egui::RawInput::default(), |_| {});
        output.textures_delta.clear();
        let definitions = ctx.fonts(|f| f.definitions().clone());
        let mono = definitions
            .families
            .get(&FontFamily::Monospace)
            .expect("Monospace 族缺失");
        assert!(
            mono.iter().any(|f| f == "kun_mono"),
            "Monospace 族应包含主等宽字体 kun_mono，实际：{mono:?}"
        );
        // Menlo 符号 fallback 仅 macOS 加载（SF Mono 缺 ➜/❯ 等字形）；
        // Linux/Windows 使用自带等宽字体，不适用该断言。
        #[cfg(target_os = "macos")]
        {
            assert!(
                mono.iter().any(|f| f == "kun_mono_sym"),
                "Monospace 族应包含 kun_mono_sym，实际：{mono:?}"
            );
            // 符号 fallback 必须排在中文 fallback 之前（缺字形时优先命中等宽符号）。
            let pos_sym = mono.iter().position(|f| f == "kun_mono_sym");
            let pos_cjk = mono.iter().position(|f| f == "kun_cjk");
            if let (Some(s), Some(c)) = (pos_sym, pos_cjk) {
                assert!(s < c, "kun_mono_sym 应排在 kun_cjk 之前，实际：{mono:?}");
            }
        }
    }
}
