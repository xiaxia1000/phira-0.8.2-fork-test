use std::sync::atomic::{AtomicUsize, Ordering};

use macroquad::color::Color;

// ---------------------------------------------------------------------------
// 内部工具函数
// ---------------------------------------------------------------------------

/// 在两个颜色之间做线性插值。
///
/// `t` 会被自动夹到 `[0, 1]`，因此调用方无需担心越界。
/// 当 `t = 0` 返回 `a`，`t = 1` 返回 `b`。
#[inline]
fn lerp_color(a: Color, b: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    Color {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    }
}

/// HSV → RGB 转换。
///
/// * `h` — 色相，任意实数，内部会通过 `rem_euclid(1.0)` 归一化。
/// * `s` — 饱和度 `[0, 1]`。
/// * `v` — 明度   `[0, 1]`。
///
/// 返回颜色 alpha 固定为 `1.0`。
#[inline]
fn hsv_to_rgb(h: f32, s: f32, v: f32) -> Color {
    let h = h.rem_euclid(1.0) * 6.0;
    let i = h.floor() as i32;
    let f = h - i as f32;

    let p = v * (1.0 - s);
    let q = v * (1.0 - s * f);
    let t = v * (1.0 - s * (1.0 - f));

    let (r, g, b) = match i.rem_euclid(6) {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    };

    Color { r, g, b, a: 1.0 }
}

// ---------------------------------------------------------------------------
// 渐变类型
// ---------------------------------------------------------------------------

/// 静态颜色渐变的具体策略。
///
/// 所有变体共用同一个"时钟"：内部维护一个单调递增的 `step` 计数器
/// （由 [`StaticColorGradient::next_step`] 推进）。每个变体通过参数 `s`
/// （speed，相位步长）把 `step` 换算成自身的相位。
///
/// 通用约定：
/// * `s` 表示"每推进一步，相位前进多少"。值越大颜色变化越快。
///   例如 `s = 0.005` 意味着大约 200 步走完一个单位相位。
/// * 所有周期性计算都使用 `rem_euclid`，保证相位始终非负，避免
///   `usize` 溢出或负相位带来的取模异常。
/// * 该枚举实现了 [`Copy`]，因此可以随意按值传递 / 匹配。
#[derive(Debug, Clone, Copy)]
pub enum StaticColorGradientType {
    /// 经典 RGB 色轮：在 R→G→B 六个色相区间上循环滚动。
    ///
    /// 相位周期为 `6 * (high - low)`，`low` / `high` 决定每个通道的
    /// 取值范围，从而间接控制饱和度与亮度。
    ///
    /// * `s`    — 每步推进的相位量。
    /// * `low`  — 通道下限（推荐 `0.0 ~ 1.0`）。
    /// * `high` — 通道上限（推荐 `0.0 ~ 1.0`，应大于 `low`）。
    ///
    /// 备注：若 `high <= low`，内部会退化为 `span = f32::EPSILON`，
    /// 此时颜色基本保持不变（不再 panic）。
    RGBTurning { s: f32, low: f32, high: f32 },

    /// 在两色之间来回渐变（三角波），不会出现首尾颜色跳变。
    ///
    /// 相位周期为 `2`：
    /// * `[0, 1)` — 从 `from` 平滑过渡到 `to`；
    /// * `[1, 2)` — 从 `to` 平滑过渡回 `from`。
    ///
    /// * `s`    — 每步推进的相位量。
    /// * `from` — 起点颜色。
    /// * `to`   — 终点颜色。
    BounceBetween { s: f32, from: Color, to: Color },

    /// 沿一组关键帧颜色循环插值。
    ///
    /// 数组首尾会自动衔接（最后一个颜色会过渡回第一个颜色），
    /// 因此可以无缝循环播放。
    ///
    /// * `s`      — 每步推进的相位量。
    /// * `colors` — 关键帧列表，使用 `'static` 切片以便保持 `const` 构造。
    ///
    /// 边界情况：
    /// * 长度为 `0` 时返回白色；
    /// * 长度为 `1` 时恒定返回该颜色。
    LinearCycle { s: f32, colors: &'static [Color] },

    /// 在基准颜色上叠加正弦波亮度调制（呼吸 / 脉动效果）。
    ///
    /// 计算方式：`m = 1 + amplitude * sin(phase)`，各 RGB 通道乘以 `m`
    /// 并夹到 `[0, 1]`。alpha 保持不变。
    ///
    /// * `s`         — 每步推进的相位量（正弦周期为 `2π / s`）。
    /// * `base`      — 基准颜色。
    /// * `amplitude` — 调制幅度，推荐 `0.0 ~ 1.0`。
    Sine { s: f32, base: Color, amplitude: f32 },

    /// 保持饱和度与明度不变，仅旋转色相（HSV 空间的彩虹）。
    ///
    /// 与 [`RGBTurning`](Self::RGBTurning) 的区别在于：这里走的是真正的
    /// HSV 色相环，颜色更鲜艳、通道间过渡更均匀。
    ///
    /// * `s`   — 每步推进的色相量（`s = 0.001` 时约 1000 步绕色相环一周）。
    /// * `sat` — 饱和度（`0.0 ~ 1.0`）。
    /// * `val` — 明度（`0.0 ~ 1.0`）。
    HueRotate { s: f32, sat: f32, val: f32 },

    /// 固定单色，不随时间变化。
    ///
    /// 主要用于调试 / 占位，或者在动态切换渐变类型时保持接口统一。
    Solid { color: Color },
}

impl Default for StaticColorGradientType {
    /// 默认值：经典的 RGB 色轮，慢速旋转、中等饱和度。
    fn default() -> Self {
        Self::RGBTurning {
            s: 0.005,
            low: 0.2,
            high: 0.8,
        }
    }
}

impl StaticColorGradientType {
    /// 纯函数：给定步进值，计算对应颜色。不修改任何内部状态。
    ///
    /// 这是所有渐变类型的具体实现入口；[`StaticColorGradient::get_color`]
    /// 只是把它和内部计数器串起来。
    pub fn color_at(&self, step: usize) -> Color {
        // step 是 usize，用 f32 表示足够支撑极长时间的运行。
        let p = step as f32;

        match *self {
            // ---------------------------------------------------------------
            // RGB 色轮
            // ---------------------------------------------------------------
            Self::RGBTurning { s, low, high } => {
                // 每个色相区间的高度。用 max 防止 high <= low 时除零 / 空区间。
                let span = (high - low).max(f32::EPSILON);
                let t = (p * s).rem_euclid(6.0 * span);

                let (r, g, b) = if t < span {
                    // 红不变，绿上升，蓝保持低
                    (high, t + low, low)
                } else if t < span * 2.0 {
                    // 红下降，绿不变，蓝保持低
                    (high - (t - span), high, low)
                } else if t < span * 3.0 {
                    // 红保持低，绿不变，蓝上升
                    (low, high, (t - span * 2.0) + low)
                } else if t < span * 4.0 {
                    // 红保持低，绿下降，蓝不变
                    (low, high - (t - span * 3.0), high)
                } else if t < span * 5.0 {
                    // 红上升，绿保持低，蓝不变
                    ((t - span * 4.0) + low, low, high)
                } else {
                    // 红不变，绿保持低，蓝下降
                    (high, low, high - (t - span * 5.0))
                };

                Color { r, g, b, a: 1.0 }
            }

            // ---------------------------------------------------------------
            // 双色往返
            // ---------------------------------------------------------------
            Self::BounceBetween { s, from, to } => {
                let t = (p * s).rem_euclid(2.0);
                // 三角波：0 → 1 → 0
                let k = if t < 1.0 { t } else { 2.0 - t };
                lerp_color(from, to, k)
            }

            // ---------------------------------------------------------------
            // 多关键帧循环
            // ---------------------------------------------------------------
            Self::LinearCycle { s, colors } => match colors.len() {
                0 => Color::new(1.0, 1.0, 1.0, 1.0),
                1 => colors[0],
                n => {
                    // 把相位限制在 [0, n)，其中整数部分对应关键帧索引。
                    let t = (p * s).rem_euclid(n as f32);
                    let i = t.floor() as usize;
                    let k = t - i as f32;

                    let a = colors[i % n];
                    // 最后一个区间从 colors[n-1] 平滑回到 colors[0]
                    let b = colors[(i + 1) % n];
                    lerp_color(a, b, k)
                }
            },

            // ---------------------------------------------------------------
            // 正弦亮度调制
            // ---------------------------------------------------------------
            Self::Sine {
                s,
                base,
                amplitude,
            } => {
                let phase = (p * s).rem_euclid(std::f32::consts::TAU);
                let m = 1.0 + amplitude * phase.sin();
                Color {
                    r: (base.r * m).clamp(0.0, 1.0),
                    g: (base.g * m).clamp(0.0, 1.0),
                    b: (base.b * m).clamp(0.0, 1.0),
                    a: base.a,
                }
            }

            // ---------------------------------------------------------------
            // HSV 色相旋转
            // ---------------------------------------------------------------
            Self::HueRotate { s, sat, val } => hsv_to_rgb(p * s, sat, val),

            // ---------------------------------------------------------------
            // 固定颜色
            // ---------------------------------------------------------------
            Self::Solid { color } => color,
        }
    }
}

// ---------------------------------------------------------------------------
// 运行时状态
// ---------------------------------------------------------------------------

/// 静态颜色渐变的运行时状态（调试时快速实现临时颜色特效的便捷工具）。
///
/// 内部只包含一个原子步进计数器，因此：
/// * 只需要 `&self` 就能推进（`next_color` / `next_step`）；
/// * 可以安全地在多个线程之间共享（`Arc<StaticColorGradient>`）；
/// * 由于使用 [`Ordering::Relaxed`]，开销极小，适合每帧调用。
///
/// # 示例
/// ```ignore
/// use std::sync::atomic::Ordering;
///
/// // 默认：慢速 RGB 色轮
/// let gradient = StaticColorGradient::default();
/// let c1 = gradient.next_color();
/// let c2 = gradient.next_color();
///
/// // 自定义：在红色与蓝色之间往返
/// let grad2 = StaticColorGradient::new(StaticColorGradientType::BounceBetween {
///     s: 0.01,
///     from: RED,
///     to: BLUE,
/// });
/// ```
#[derive(Debug, Default)]
pub struct StaticColorGradient {
    ty: StaticColorGradientType,
    step: AtomicUsize,
}

impl StaticColorGradient {
    /// 使用指定渐变类型创建一个新的渐变实例，步进从 `0` 开始。
    ///
    /// 因为 `step` 使用 `AtomicUsize::new`，所以这是 `const fn`，
    /// 可以在 `static` / `const` 上下文中直接构造。
    pub const fn new(ty: StaticColorGradientType) -> Self {
        Self {
            ty,
            step: AtomicUsize::new(0),
        }
    }

    /// 返回当前步进对应的颜色，并把步进推进 1。
    ///
    /// 这是最常用的调用方式：每帧调用一次即可获得连续变化的颜色。
    pub fn next_color(&self) -> Color {
        let color = self.get_color();
        self.next_step();
        color
    }

    /// 读取当前步进值（`Relaxed` 顺序，仅用于显示 / 调试）。
    pub fn get_step(&self) -> usize {
        self.step.load(Ordering::Relaxed)
    }

    /// 把步进值加 1。
    ///
    /// 一般无需手动调用，[`next_color`](Self::next_color) 已经包含此操作。
    pub fn next_step(&self) {
        self.step.fetch_add(1, Ordering::Relaxed);
    }

    /// 把步进值重置为 0（下一次取色会回到渐变起点）。
    pub fn reset(&self) {
        self.step.store(0, Ordering::Relaxed);
    }

    /// 直接设置步进值，可用于把渐变"跳转"到任意位置。
    pub fn set_step(&self, step: usize) {
        self.step.store(step, Ordering::Relaxed);
    }

    /// 计算当前步进对应的颜色，**不**推进内部计数器。
    ///
    /// 适合需要多次读取同一帧颜色，或希望自行控制推进时机的场景。
    pub fn get_color(&self) -> Color {
        self.ty.color_at(self.get_step())
    }

    /// 获取当前的渐变类型（按值返回，因为该枚举是 `Copy`）。
    pub fn ty(&self) -> StaticColorGradientType {
        self.ty
    }
}

pub fn mix_color(a: Color, b: Color, t: f32) -> Color {
    Color::new(
        a.r * (1.0 - t) + b.r * t,
        a.g * (1.0 - t) + b.g * t,
        a.b * (1.0 - t) + b.b * t,
        a.a * (1.0 - t) + b.a * t,
    )
}