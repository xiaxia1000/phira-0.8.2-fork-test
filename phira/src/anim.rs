//! 对 prpr 补间原语的轻量封装，供 UI 层做「一次性过渡」动画。
//!
//! 不直接使用 prpr 的动画（`Anim`）系统，是因为那套系统面向关键帧轨道：需要注册轨道、
//! 维护关键帧序列，适合谱面演出这类复杂时间线；而 UI 层的过渡只需要「起止值 + 时长 + 缓动」
//! 三要素，故直接基于 `easing_from` / `StaticTween` / `TweenFunction` / `Tweenable`
//! 这几个原语组装出极简结构，省去轨道注册与每帧遍历关键帧的开销，也让调用点更直白。

use prpr::core::{easing_from, StaticTween, TweenFunction, Tweenable};
use std::rc::Rc;

/// 一段从 `from` 到 `to` 的补间，以绝对时间轴 `[start_time, end_time]` 描述进度。
///
/// 所有字段公开：除用 `begin`/`goto`/`start` 驱动外，也可直接改写 `to`（配合 `alter_to`），
/// 让「动画目标值随布局持续变化」而不必重启动画。
pub struct Anim<T: Tweenable> {
    /// 补间起点值（进度为 0 时取该值）。
    pub from: T,
    /// 补间终点值（进度为 1 时取该值）。
    pub to: T,
    /// 本次补间的起始绝对时间；初始为 `NEG_INFINITY` 使起始进度即视作完成（不做入场动画）。
    pub start_time: f32,
    /// 本次补间的结束绝对时间；初始为极小正值，与 `start_time` 共同构成退化的时间区间。
    pub end_time: f32,
    /// 缓动函数（把线性进度重映射为缓动进度），用 `Rc` 共享以降低每帧克隆成本。
    pub interpolator: Rc<dyn TweenFunction>,
}

// 为补间提供 `Default`：等价于「静止在 `T::default()`」，便于放入以 `Default` 派生的 UI 结构体。
impl<T: Tweenable + Default> Default for Anim<T> {
    /// 以 `T::default()` 作为静止值，构造一段不处于过渡中的动画。
    fn default() -> Self {
        Self::new(T::default())
    }
}

// 补间的构造与驱动方法。核心约定：所有方法都接收「当前绝对时间 t」，
// 状态只由 `from/to/start_time/end_time/interpolator` 决定，因此无隐藏的每帧状态，
// 调用方可以随时改写字段（例如只更新 `to` 而让动画继续朝新目标收敛）。
impl<T: Tweenable> Anim<T> {
    /// 构造一段静止在 `init` 的补间：起止值相同，故任何时刻取值都等于 `init`。
    /// 默认缓动为 Cubic-Out（先快后慢的减速曲线），符合 UI 位移/淡入的常见手感。
    pub fn new(init: T) -> Self {
        Self {
            from: init.clone(),
            to: init,
            start_time: f32::NEG_INFINITY,
            end_time: 1e-3,
            interpolator: StaticTween::get_rc(easing_from(prpr::core::TweenMajor::Cubic, prpr::core::TweenMinor::Out)),
        }
    }

    /// 时刻 `t` 是否落在过渡区间内（左闭右开）。
    /// 调用方据此判断「动画进行中」，例如过渡期间吞掉输入或额外绘制旧状态。
    #[inline]
    pub fn transiting(&self, t: f32) -> bool {
        (self.start_time..self.end_time).contains(&t)
    }

    /// 按当前进度对起止值做插值，得到 `t` 时刻的显示值。
    pub fn now(&self, t: f32) -> T {
        T::tween(&self.from, &self.to, self.progress(t))
    }

    /// 归一化进度：把绝对时间映射为 `[0, 1]` 后再经缓动函数整形。
    /// 仅用 `.min(1.)` 钳制上界（动画结束后稳定为终点值），下界不钳制，
    /// 因此起点之前可能出现负进度，缓动函数需能接受 `x < 0` 的输入。
    pub fn progress(&self, t: f32) -> f32 {
        self.interpolator.y(((t - self.start_time) / (self.end_time - self.start_time)).min(1.))
    }

    /// 显式指定起止值与时间区间并重启动画，用于两端状态已知的过渡。
    pub fn start(&mut self, from: T, to: T, t: f32, duration: f32) {
        self.from = from;
        self.to = to;
        self.start_time = t;
        self.end_time = t + duration;
    }

    /// 以「当前显示值」为起点平滑过渡到新目标值：打断中的动画不会跳变，而是从当前位置续接。
    #[inline]
    pub fn goto(&mut self, to: T, t: f32, duration: f32) {
        self.start(self.now(t), to, t, duration)
    }

    /// 保留既有 `to`，仅以当前显示值作为起点重新计时。
    /// 适用于目标值已被 `alter_to` 更新、需要以新时长为本次过渡重新起算的场景。
    pub fn begin(&mut self, t: f32, duration: f32) {
        self.start(self.now(t), self.to.clone(), t, duration)
    }

    /// 只改写终点值、不重启计时：若调用方每帧更新目标（如跟随布局），动画会自动朝最新目标收敛。
    #[inline]
    pub fn alter_to(&mut self, to: T) {
        self.to = to;
    }

    /// 立即落到 `value`：同时把起止值都设为该值，等价于跳过动画。
    pub fn set(&mut self, value: T) {
        self.from = value.clone();
        self.to = value;
    }
}
