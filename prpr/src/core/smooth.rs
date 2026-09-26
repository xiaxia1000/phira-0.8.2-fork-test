//! 一次性时间过渡：`Smooth<T>` 表示“在给定起止时间内从 `from` 过渡到 `to`”。
//!
//! 与 [`Anim`](super::Anim) 的分工：`Anim` 描述谱面时间轴上的多关键帧曲线，必须
//! 在任意 seek（可前可后）下都能正确求值，因此维护关键帧数组与搜索游标；`Smooth`
//! 只服务 UI/场景层的一次性进出场（例如侧栏进度从 0 滑到 1），只需两个端点加一段
//! 时间区间，结构更轻、查询是纯函数。
//!
//! 默认插值器为 ease-out-cubic `p ↦ 1 - (1 - p)^3`：UI 动画惯例是“快起慢收”，
//! 起始响应快、结束平稳；该式在 `p = 1` 处导数为 0，收尾速度连续，不会显得突兀。
use super::Tweenable;

/// 给定起止时间内的一次性插值：不保存关键帧，只保存两个端点与时间区间。
pub struct Smooth<T: Tweenable> {
    /// 过渡起点值，由 [`Smooth::start`] / [`Smooth::goto`] 确定。
    from: T,
    /// 过渡终点值，可被 [`Smooth::alter_to`] 在过渡途中改写。
    to: T,
    /// 过渡开始的绝对时刻，与调用方传入的 `t` 使用同一条时间轴（单位秒）。
    start_time: f32,
    /// 过渡结束的绝对时刻；`end_time - start_time` 为时长。
    end_time: f32,
    /// 归一化进度 `p ∈ [0, 1]` 到缓动输出的映射，默认 ease-out-cubic。
    ///
    /// 用裸函数指针而非闭包：缓动都是无捕获的纯函数，指针不占额外空间，
    /// 也避免 `Smooth` 携带装箱对象。
    interpolator: fn(f32) -> f32,
}

// 默认构造：以 `T::default()` 为起止值，区间取 [0, 1] 的“静止”过渡。
impl<T: Tweenable + Default> Default for Smooth<T> {
    /// 以 `T::default()` 作为起止值构造一个“尚未启动”的过渡。
    fn default() -> Self {
        Self::new(T::default())
    }
}

// 过渡的构造与查询：全部为纯读写，不依赖任何外部状态，故可在 `&self` 下求值。
impl<T: Tweenable> Smooth<T> {
    /// 以初值 `init` 构造：起点与终点都等于 `init`，时间区间为 `[0, 1]`。
    ///
    /// 这是“静止”状态——区间内 [`Smooth::now`] 恒返回 `init`，因为 `from` 与
    /// `to` 相同；区间本身只作为默认值存在，实际使用时会被 `start`/`goto` 覆盖。
    pub fn new(init: T) -> Self {
        Self {
            from: init.clone(),
            to: init,
            start_time: 0.,
            end_time: 1.,
            interpolator: |p| 1. - (1. - p).powi(3),
        }
    }

    /// 替换缓动函数，链式返回自身（构造期使用）。
    ///
    /// # Arguments
    /// * `f` - 定义在 `[0, 1]` 上的缓动；应满足 `f(0) = 0`、`f(1) = 1` 才能保证
    ///   过渡起点与终点等于 `from`/`to`，否则会平移整体取值。
    #[inline]
    pub fn with_interpolator(mut self, f: fn(f32) -> f32) -> Self {
        self.interpolator = f;
        self
    }

    /// 时间 `t` 是否处于过渡区间内（左闭右开 `[start_time, end_time)`）。
    ///
    /// 右端取的语义：`t == end_time` 时进度已达 1、过渡结束，调用方据此判断
    /// “是否还需要继续重绘/查询”而无需再调用 [`Smooth::now`]。
    /// 默认构造下的 `[0, 1)` 区间仅在 `new` 之后、真正 `start` 之前有效。
    #[inline]
    pub fn transiting(&self, t: f32) -> bool {
        (self.start_time..self.end_time).contains(&t)
    }

    /// 读取过渡终点值（不推进时间）。
    ///
    /// 典型用途：用“终点是否越过阈值”判断 UI 当前处于展开还是收起，
    /// 而不必依赖时间进度（例如面板滑动到 0.5 以上即视为展开）。
    pub fn to(&self) -> &T {
        &self.to
    }

    /// 求时间 `t` 处的插值结果。
    ///
    /// 归一化进度先钳到 `1.` 再喂给插值器：这样 `t` 越过 `end_time` 后结果稳定
    /// 收敛到 `to`，不会继续向外外插。下界未做钳制，`t < start_time` 时进度为负，
    /// 结果会低于 `from`（默认 ease-out-cubic 在 `p = 0` 处导数为 3，外插斜率不小），
    /// 因此调用方应先用 [`Smooth::transiting`] 判断是否需要求值。
    pub fn now(&self, t: f32) -> T {
        T::tween(&self.from, &self.to, (self.interpolator)(((t - self.start_time) / (self.end_time - self.start_time)).min(1.)))
    }

    /// 以显式端点与时长启动一次过渡。
    ///
    /// # Arguments
    /// * `from` / `to` - 起点与终点取值
    /// * `t` - 当前时刻，作为区间左端
    /// * `duration` - 时长；`end_time = t + duration`
    ///
    /// 只记录时间端点，进度每次查询时现算，因此帧率变化、暂停后恢复都不会累积误差。
    /// 注意 `duration` 必须为正，否则 [`Smooth::now`] 的归一化分母为 0 或负，
    /// 语义失去意义。
    pub fn start(&mut self, from: T, to: T, t: f32, duration: f32) {
        self.from = from;
        self.to = to;
        self.start_time = t;
        self.end_time = t + duration;
    }

    /// 从“当前值”平滑过渡到 `to`：起点取 `t` 时刻的 [`Smooth::now`]。
    ///
    /// 与 [`Smooth::start`] 的差别在于起点是当前进度而非调用方给的常量，因此可在
    /// 上一次过渡尚未结束时再次调用——新过渡从视觉上的当前位置接着走，不会跳变。
    /// 这正是 UI 里“反复点击开关”仍能保持连续的实现方式。
    #[inline]
    pub fn goto(&mut self, to: T, t: f32, duration: f32) {
        self.start(self.now(t), to, t, duration)
    }

    /// 只改写终点，不重启过渡。
    ///
    /// 适用场景：过渡进行中目标值发生变化，但希望保留当前时间进度与缓动曲线
    /// （不重置 `start_time`）。代价是起点 `from` 仍是旧值，轨迹可能出现拐点；
    /// 若要求视觉连续应改用 [`Smooth::goto`]。
    pub fn alter_to(&mut self, to: T) {
        self.to = to;
    }
}
