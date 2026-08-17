//! 性能测量 HUD：帧耗时 / FPS / 终端渲染分段耗时。
//!
//! 用调用方每帧传入的耗时打点（无锁刷新），渲染时以滑动窗口平均展示。
//! 纯 UI 线程使用，不做跨线程共享——HUD 本身不引入性能损耗。

use std::time::Instant;

/// 单条耗时指标的滑动平均。
struct Sliding {
    /// 历史样本（最大长度）。
    samples: Vec<f32>,
}

impl Sliding {
    fn new() -> Self {
        Self {
            samples: Vec::with_capacity(128),
        }
    }

    fn push(&mut self, v: f32) {
        self.samples.push(v);
        if self.samples.len() > 128 {
            self.samples.remove(0);
        }
    }

    /// 平均耗时（毫秒）。
    fn avg_ms(&self) -> Option<f32> {
        if self.samples.is_empty() {
            return None;
        }
        let sum: f32 = self.samples.iter().sum();
        Some(sum / self.samples.len() as f32)
    }
}

/// 全局性能统计（单实例，UI 线程使用）。
pub struct PerfStats {
    frame_times: Sliding,
    build_times: Sliding,
    layout_times: Sliding,
    paint_times: Sliding,
    /// 上一帧结束时间（计算 FPS）。
    last_frame: Option<Instant>,
    /// 每秒帧数滑动平均。
    fps: Sliding,
}

impl Default for PerfStats {
    fn default() -> Self {
        Self::new()
    }
}

impl PerfStats {
    pub fn new() -> Self {
        Self {
            frame_times: Sliding::new(),
            build_times: Sliding::new(),
            layout_times: Sliding::new(),
            paint_times: Sliding::new(),
            last_frame: None,
            fps: Sliding::new(),
        }
    }

    /// 帧开始（记录起始时间）。
    pub fn begin_frame(&mut self) {
        let now = Instant::now();
        if let Some(last) = self.last_frame {
            let dt = now.duration_since(last).as_secs_f32();
            if dt > 0.0 {
                self.fps.push(1.0 / dt);
            }
        }
        self.last_frame = Some(now);
    }

    /// 帧结束（记录整帧耗时，毫秒）。
    pub fn end_frame(&mut self) {
        if let Some(last) = self.last_frame {
            let t = last.elapsed().as_secs_f32() * 1000.0;
            self.frame_times.push(t);
        }
    }

    /// 记录终端锁内构建耗时（毫秒）。
    pub fn add_build(&mut self, ms: f32) {
        self.build_times.push(ms);
    }

    /// 记录终端文本布局耗时（毫秒）。
    pub fn add_layout(&mut self, ms: f32) {
        self.layout_times.push(ms);
    }

    /// 记录绘制耗时（毫秒）。
    pub fn add_paint(&mut self, ms: f32) {
        self.paint_times.push(ms);
    }

    /// 汇总一行展示文本。
    pub fn summary(&self) -> String {
        let f = |v: Option<f32>| match v {
            Some(v) => format!("{v:.2}"),
            None => "—".to_string(),
        };
        let fps = self.fps.avg_ms().map(|v| v as u32);
        let fps = fps.map(|v| v.to_string()).unwrap_or_else(|| "—".into());
        format!(
            "帧 {}ms | FPS {} | 构建 {}ms | 布局 {}ms | 绘制 {}ms",
            f(self.frame_times.avg_ms()),
            fps,
            f(self.build_times.avg_ms()),
            f(self.layout_times.avg_ms()),
            f(self.paint_times.avg_ms()),
        )
    }
}
