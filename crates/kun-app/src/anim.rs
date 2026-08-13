//! 轻量动效工具：缓动曲线、平滑插值、渐变/辉光绘制。
//!
//! 不引入额外依赖：所有状态存于 `egui::Context` 的临时数据中，
//! 以 `Id` 区分不同控件，帧间按真实时间差平滑收敛。

use egui::{
    epaint::Vertex, pos2, Color32, Context, CornerRadius, Id, Mesh, Painter, Pos2, Rect, Shape,
    Vec2,
};

/// 指数平滑的收敛速率（1/秒，越大越快）。
pub const SPEED_FAST: f32 = 34.0;
/// 常规悬停/切换的收敛速率。
pub const SPEED_NORMAL: f32 = 20.0;
/// 舒缓的大幅动画速率。
pub const SPEED_SLOW: f32 = 10.0;

/// 当前帧时间（秒）。
pub fn now(ctx: &Context) -> f64 {
    ctx.input(|i| i.time)
}

/// 将值限制在 [0, 1]。
pub fn clamp01(x: f32) -> f32 {
    x.clamp(0.0, 1.0)
}

/// 三次缓出（动画快进慢停）。
pub fn ease_out_cubic(t: f32) -> f32 {
    let t = clamp01(t);
    1.0 - (1.0 - t).powi(3)
}

/// 回弹缓出（用于 Toast 滑入的轻微过冲）。
pub fn ease_out_back(t: f32) -> f32 {
    let t = clamp01(t);
    const C1: f32 = 1.70158;
    const C3: f32 = C1 + 1.0;
    1.0 + C3 * (t - 1.0).powi(3) + C1 * (t - 1.0).powi(2)
}

/// 线性插值。
pub fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * clamp01(t)
}

/// RGBA 线性插值。
pub fn mix_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = clamp01(t);
    Color32::from_rgba_unmultiplied(
        lerp(a.r() as f32, b.r() as f32, t).round() as u8,
        lerp(a.g() as f32, b.g() as f32, t).round() as u8,
        lerp(a.b() as f32, b.b() as f32, t).round() as u8,
        lerp(a.a() as f32, b.a() as f32, t).round() as u8,
    )
}

/// 平滑动画的结果：当前值 + 是否仍在收敛。
#[derive(Debug, Clone, Copy)]
pub struct AnimValue {
    pub value: f32,
    /// 仍与目标有肉眼可见差距时为 true（调用方据此持续请求重绘）。
    pub settling: bool,
}

/// 值随时间向目标指数平滑收敛（状态存于 ctx 临时数据）。
///
/// `speed` 为收敛速率（1/秒），`eps` 为视为已到达的阈值。
pub fn smooth_state(ctx: &Context, id: Id, target: f32, speed: f32, eps: f32) -> AnimValue {
    let time = now(ctx);
    let (last_time, previous) = ctx.data_mut(|d| {
        (
            d.get_temp::<f64>(id.with("t")).unwrap_or(time),
            d.get_temp::<f32>(id).unwrap_or(target),
        )
    });
    // 时间跳跃（如休眠恢复）不应产生一步到位的大步长。
    let dt = ((time - last_time).clamp(0.0, 0.05)) as f32;
    let alpha = 1.0 - (-speed * dt).exp();
    let value = previous + (target - previous) * alpha;
    ctx.data_mut(|d| {
        d.insert_temp(id, value);
        d.insert_temp(id.with("t"), time);
    });
    AnimValue {
        value,
        settling: (value - target).abs() > eps,
    }
}

/// `smooth_state` 的简写，仅返回值。
pub fn smooth(ctx: &Context, id: Id, target: f32, speed: f32) -> f32 {
    smooth_state(ctx, id, target, speed, 0.01).value
}

/// 布尔目标（0/1）的平滑值。
pub fn smooth_bool(ctx: &Context, id: Id, target: bool, speed: f32) -> f32 {
    smooth(ctx, id, if target { 1.0 } else { 0.0 }, speed)
}

/// 正弦脉冲（0..1..0），用于"新版本可用"呼吸灯等。
pub fn pulse(ctx: &Context, period_secs: f32) -> f32 {
    let t = now(ctx) as f32;
    let phase = (t / period_secs).fract();
    0.5 + 0.5 * (phase * std::f32::consts::TAU).sin()
}

/// 循环相位 [0, 1)，用于扫光/渐变流动。
pub fn sweep(ctx: &Context, period_secs: f32) -> f32 {
    ((now(ctx) as f32 / period_secs) % 1.0 + 1.0) % 1.0
}

/// 绘制垂直渐变（两色）。
pub fn paint_v_gradient(painter: &Painter, rect: Rect, top: Color32, bottom: Color32) {
    let mesh = Mesh {
        vertices: vec![
            Vertex {
                pos: rect.left_top(),
                uv: Pos2::ZERO,
                color: top,
            },
            Vertex {
                pos: rect.right_top(),
                uv: Pos2::ZERO,
                color: top,
            },
            Vertex {
                pos: rect.right_bottom(),
                uv: Pos2::ZERO,
                color: bottom,
            },
            Vertex {
                pos: rect.left_bottom(),
                uv: Pos2::ZERO,
                color: bottom,
            },
        ],
        indices: vec![0, 1, 2, 0, 2, 3],
        ..Default::default()
    };
    painter.add(Shape::mesh(mesh));
}

/// 绘制水平渐变（两色）。
pub fn paint_h_gradient(painter: &Painter, rect: Rect, left: Color32, right: Color32) {
    let mesh = Mesh {
        vertices: vec![
            Vertex {
                pos: rect.left_top(),
                uv: Pos2::ZERO,
                color: left,
            },
            Vertex {
                pos: rect.right_top(),
                uv: Pos2::ZERO,
                color: right,
            },
            Vertex {
                pos: rect.right_bottom(),
                uv: Pos2::ZERO,
                color: right,
            },
            Vertex {
                pos: rect.left_bottom(),
                uv: Pos2::ZERO,
                color: left,
            },
        ],
        indices: vec![0, 1, 2, 0, 2, 3],
        ..Default::default()
    };
    painter.add(Shape::mesh(mesh));
}

/// 绘制"彗星扫光"线条：一段 `c0→c1` 的渐变色带沿水平方向循环掠过。
///
/// `phase` 为当前相位 [0,1)，`band` 为色带占整宽的比例。
pub fn paint_shimmer_line(
    painter: &Painter,
    rect: Rect,
    c0: Color32,
    c1: Color32,
    phase: f32,
    band: f32,
) {
    const SEGMENTS: usize = 28;
    let mut mesh = Mesh::default();
    for i in 0..SEGMENTS {
        let t0 = i as f32 / SEGMENTS as f32;
        let t1 = (i + 1) as f32 / SEGMENTS as f32;
        let x0 = rect.left() + rect.width() * t0;
        let x1 = rect.left() + rect.width() * t1;
        let color0 = shimmer_color(t0, phase, band, c0, c1);
        let color1 = shimmer_color(t1, phase, band, c0, c1);
        let base = mesh.vertices.len() as u32;
        mesh.vertices.extend([
            Vertex {
                pos: pos2(x0, rect.top()),
                uv: Pos2::ZERO,
                color: color0,
            },
            Vertex {
                pos: pos2(x1, rect.top()),
                uv: Pos2::ZERO,
                color: color1,
            },
            Vertex {
                pos: pos2(x1, rect.bottom()),
                uv: Pos2::ZERO,
                color: color1,
            },
            Vertex {
                pos: pos2(x0, rect.bottom()),
                uv: Pos2::ZERO,
                color: color0,
            },
        ]);
        mesh.indices
            .extend([base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    painter.add(Shape::mesh(mesh));
}

/// 相位对应的色带颜色（色带尾部拖出 `c0→c1` 渐变并渐隐）。
fn shimmer_color(t: f32, phase: f32, band: f32, c0: Color32, c1: Color32) -> Color32 {
    let d = (t - phase).rem_euclid(1.0);
    if d > band {
        return Color32::TRANSPARENT;
    }
    let k = d / band;
    // 二次缓入让色带头部更饱和，尾部渐隐。
    let base = mix_color(c0, c1, k * k);
    base.gamma_multiply(1.0 - k)
}

/// 绘制柔光（同心圆叠加近似径向辉光，中心亮、边缘淡）。
pub fn paint_glow(painter: &Painter, center: Pos2, radius: f32, color: Color32) {
    const STEPS: usize = 10;
    for i in 0..STEPS {
        let t = i as f32 / STEPS as f32;
        let r = radius * (1.0 - t);
        let alpha = 0.02 + 0.18 * (1.0 - t).powi(2);
        painter.circle_filled(center, r.max(0.5), color.gamma_multiply(alpha));
    }
}

/// 绘制圆角矩形的垂直渐变填充。
pub fn paint_rounded_gradient(
    painter: &Painter,
    rect: Rect,
    radius: f32,
    top: Color32,
    bottom: Color32,
) {
    let radius = radius.min(rect.width() * 0.5).min(rect.height() * 0.5);
    let points = rounded_rect_points(rect, radius);
    let mut mesh = Mesh::default();
    let center = rect.center();
    mesh.vertices.push(Vertex {
        pos: center,
        uv: Pos2::ZERO,
        color: mix_color(top, bottom, 0.5),
    });
    for p in &points {
        let t = ((p.y - rect.top()) / rect.height().max(1.0)).clamp(0.0, 1.0);
        mesh.vertices.push(Vertex {
            pos: *p,
            uv: Pos2::ZERO,
            color: mix_color(top, bottom, t),
        });
    }
    for i in 0..points.len() {
        let j = (i + 1) % points.len();
        mesh.indices.extend([0, (i + 1) as u32, (j + 1) as u32]);
    }
    painter.add(Shape::mesh(mesh));
}

/// 采样圆角矩形轮廓点（顺时针，从左上圆弧的起点开始）。
fn rounded_rect_points(rect: Rect, radius: f32) -> Vec<Pos2> {
    const ARC_SEGMENTS: usize = 7;
    let mut points = Vec::with_capacity(4 + 4 * ARC_SEGMENTS);
    let min = rect.min;
    let max = rect.max;

    fn push_arc(points: &mut Vec<Pos2>, center: Pos2, radius: f32, start_deg: f32, end_deg: f32) {
        for i in 1..=ARC_SEGMENTS {
            let deg = start_deg + (end_deg - start_deg) * (i as f32 / ARC_SEGMENTS as f32);
            let rad = deg.to_radians();
            points.push(center + Vec2::new(radius * rad.cos(), radius * rad.sin()));
        }
    }

    // 上边（左上角圆弧终点 → 右上角圆弧起点）。
    points.push(pos2(min.x + radius, min.y));
    points.push(pos2(max.x - radius, min.y));
    // 右上角圆弧（-90° → 0°）。
    push_arc(
        &mut points,
        pos2(max.x - radius, min.y + radius),
        radius,
        -90.0,
        0.0,
    );
    // 右边（右上角 → 右下角）。
    points.push(pos2(max.x, max.y - radius));
    // 右下角圆弧（0° → 90°）。
    push_arc(
        &mut points,
        pos2(max.x - radius, max.y - radius),
        radius,
        0.0,
        90.0,
    );
    // 下边（右下角 → 左下角）。
    points.push(pos2(min.x + radius, max.y));
    // 左下角圆弧（90° → 180°）。
    push_arc(
        &mut points,
        pos2(min.x + radius, max.y - radius),
        radius,
        90.0,
        180.0,
    );
    // 左边（左下角 → 左上角）。
    points.push(pos2(min.x, min.y + radius));
    // 左上角圆弧（180° → 270°）。
    push_arc(
        &mut points,
        pos2(min.x + radius, min.y + radius),
        radius,
        180.0,
        270.0,
    );

    points
}

/// 绘制圆角矩形（带统一圆角），参数顺序对齐 painter 惯例。
pub fn rect_filled(painter: &Painter, rect: Rect, radius: impl Into<CornerRadius>, color: Color32) {
    painter.rect_filled(rect, radius, color);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 缓出曲线端点() {
        assert!((ease_out_cubic(0.0) - 0.0).abs() < f32::EPSILON);
        assert!((ease_out_cubic(1.0) - 1.0).abs() < f32::EPSILON);
        assert!(ease_out_cubic(0.5) > 0.5);
        assert!(ease_out_back(0.0) <= 0.0 + f32::EPSILON);
        assert!((ease_out_back(1.0) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn 颜色插值端点() {
        let a = Color32::from_rgb(0, 0, 0);
        let b = Color32::from_rgb(255, 255, 255);
        assert_eq!(mix_color(a, b, 0.0), a);
        assert_eq!(mix_color(a, b, 1.0), b);
        assert_eq!(mix_color(a, b, 0.5), Color32::from_rgb(128, 128, 128));
    }

    #[test]
    fn 扫光带外透明() {
        assert_eq!(
            shimmer_color(0.9, 0.2, 0.2, Color32::WHITE, Color32::WHITE),
            Color32::TRANSPARENT
        );
        assert_ne!(
            shimmer_color(0.3, 0.2, 0.2, Color32::WHITE, Color32::WHITE),
            Color32::TRANSPARENT
        );
    }
}
