//! macOS 原生窗口定制（非 macOS 平台为空实现）。
//!
//! 关闭系统标题栏（`with_decorations(false)`）后窗口是方角矩形，
//! 这里通过 AppKit 给窗口 contentView 的 layer 设置圆角 + 裁切，
//! 并将窗口背景设为透明，使圆角外侧露出桌面，形成整体圆角观感。
//!
//! 只应在真实 AppKit 窗口环境调用（eframe 主线程创建期）；
//! kittest 测试环境没有真实 NSWindow（句柄匹配失败），静默跳过。

use raw_window_handle::HasWindowHandle as _;

#[cfg(target_os = "macos")]
use objc2_app_kit::NSView;

/// 窗口四角圆角半径（逻辑像素，与 `theme.rs` 的大圆角面板观感一致）。
const WINDOW_CORNER_RADIUS: f64 = 12.0;

/// 给无边框窗口应用整体圆角。失败（非 macOS / 无 AppKit 句柄）时静默忽略。
pub fn apply_rounded_window(cc: &eframe::CreationContext<'_>) {
    #[cfg(target_os = "macos")]
    apply_rounded_window_macos(cc);
    #[cfg(not(target_os = "macos"))]
    let _ = cc;
}

/// macOS 实现：拿到 contentView → 打开 layer 支持 → 设圆角 + 裁切 → 窗口背景透明。
#[cfg(target_os = "macos")]
fn apply_rounded_window_macos(cc: &eframe::CreationContext<'_>) {
    use raw_window_handle::RawWindowHandle;

    // 从 eframe 创建的窗口拿 AppKit 句柄；失败说明不是真实窗口（如测试环境）。
    let Ok(handle) = cc.window_handle() else {
        return;
    };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return;
    };
    let ns_view_ptr = handle.ns_view.as_ptr().cast::<NSView>();
    if ns_view_ptr.is_null() {
        return;
    }
    // SAFETY: 句柄来自窗口系统，指针在窗口生命周期内有效（与 eframe 内部一致）。
    let ns_view: &NSView = unsafe { ns_view_ptr.as_ref() }.unwrap();

    apply_corner_radius(ns_view);

    // 让窗口背景透明（圆角外侧露出桌面），并关闭不透明标记。
    if let Some(ns_window) = ns_view.window() {
        set_window_transparent(&ns_window);
    }
}

/// 给 view 的 backing layer 设圆角并裁切越界内容。
#[cfg(target_os = "macos")]
fn apply_corner_radius(ns_view: &NSView) {
    // 打开 layer 支持（`wantsLayer`），随后 `layer()` 才有值。
    ns_view.setWantsLayer(true);
    if let Some(layer) = ns_view.layer() {
        layer.setCornerRadius(WINDOW_CORNER_RADIUS);
        layer.setMasksToBounds(true);
    }
}

/// 窗口背景设为透明并标记非不透明（否则圆角外侧是黑/白方块）。
#[cfg(target_os = "macos")]
fn set_window_transparent(ns_window: &objc2_app_kit::NSWindow) {
    use objc2_app_kit::NSColor;
    ns_window.setOpaque(false);
    ns_window.setBackgroundColor(Some(&NSColor::clearColor()));
}
