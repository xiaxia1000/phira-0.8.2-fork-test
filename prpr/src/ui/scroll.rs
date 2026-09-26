//! 可滚动容器。
//!
//! 本模块解决三件事：
//! 1. **手势判定**：把「点击」与「拖动」区分开（[`Scroller::touch`] 的 `unlock` 机制），
//!    滚动与点击互斥——一旦判定为拖动，本次触摸就不再交给内容处理；
//! 2. **物理手感**：惯性滑行、越界回弹、步长吸附都在 [`Scroller::update`] 里用
//!    「一阶弹簧 + 指数衰减」的近似实现，不需要真正的物理引擎；
//! 3. **裁剪**：[`ClipType`] 提供三种裁剪方式，因为有的滚动区需要圆角、
//!    有的不需要，而不同方式的 GPU 代价差别很大。
//!
//! 坐标约定：`Scroll` 在第一次 `render` 时记录当前 UI 变换的**逆矩阵**，
//! 之后所有触摸/鼠标输入都先经它反变换回 UI 的局部设计坐标再做命中检测。
//! 因此 `render` 之前 `touch`/`contains` 一律返回「不命中」，这是刻意的退化行为。

use super::{clip_rounded_rect, Ui};
use crate::{
    core::{Matrix, Point, Vector},
    judge::take_wheel,
};
use macroquad::{
    input::mouse_position,
    prelude::{Rect, Touch, TouchPhase, Vec2},
    window::screen_height,
};
use nalgebra::Translation2;
use std::collections::VecDeque;

/// 判定「按下后确实在拖动」的位移阈值（设计坐标单位）。
///
/// 手指按下时总会有几像素抖动，若不加阈值，几乎每次点击都会被判成拖动，
/// 从而吞掉内容自身的点击事件；0.03 是「足够大以滤掉抖动、又足够小以感觉即时」的取值。
const THRESHOLD: f32 = 0.03;
/// 鼠标滚轮每一格刻度对应的滚动距离（设计坐标单位）。
/// 之所以要在滚轮与拖动之间做单位换算，是因为两者物理量不同（格 vs 距离）。
const WHEEL_STEP: f32 = 0.1;

/// 滚动速度估计器：记录最近若干次触摸采样，并拟合出松手瞬间的速度。
pub struct VelocityTracker {
    /// 最近 [`VelocityTracker::RECORD_MAX`] 个 `(时间, 位置)` 采样，按时间递增。
    /// 位置用 [`Point`] 承载是为了复用向量/矩阵数学（`y` 恒为 0），
    /// 一维水平滚动也走同一套代码。
    movements: VecDeque<(f32, Point)>,
}

impl VelocityTracker {
    /// 参与拟合的最大采样数。
    ///
    /// 只保留最近 10 次是有意为之：速度只应反映「松手瞬间」的趋势，
    /// 样本过多会把松手前的减速历史也平均进去，导致甩动速度被低估、惯性显得迟钝。
    pub const RECORD_MAX: usize = 10;

    /// 创建空的速度估计器（还没有任何采样）。
    pub fn empty() -> Self {
        Self {
            movements: VecDeque::with_capacity(Self::RECORD_MAX),
        }
    }

    /// 丢弃全部历史采样。用于每次新的触摸按下时重置，避免上一次拖动的速度泄漏到本次。
    pub fn reset(&mut self) {
        self.movements.clear();
    }

    /// 追加一次采样，超过 [`VelocityTracker::RECORD_MAX`] 时丢弃最旧的一条。
    ///
    /// 这里用「长度到上限再弹出」而不是 `VecDeque::pop_front` 的批量裁剪，
    /// 是因为 `touch` 可能在一帧内被多次调用（多指/补帧），逐条维护最直观；
    /// 源码中的 `TODO optimize` 即指此处每次可能有一次多余的长度比较。
    pub fn push(&mut self, time: f32, position: Point) {
        if self.movements.len() == Self::RECORD_MAX {
            // TODO optimize
            self.movements.pop_front();
        }
        self.movements.push_back((time, position));
    }

    /// 估计当前速度（设计坐标 / 秒）。
    ///
    /// 算法：以最后一次采样的时间为原点，对 `(Δt, 位置)` 做**二次多项式最小二乘拟合**
    /// `s(Δt) = a·Δt² + b·Δt + c`，返回一次项系数 `b`——即原点处的瞬时速度。
    ///
    /// 为什么不用最简单的前后两点差分：触摸事件的采样间隔不均匀且有量化噪声，
    /// 两点差分会把噪声直接放大成速度尖峰；二次拟合同时吸收了加速度项，
    /// 相当于对采样做了平滑，因此甩动速度稳定得多，也不会因为最后一帧移动很小而突然「急停」。
    ///
    /// 实现上把原点平移到 `lst`（`t - lst`），使 `c = sum_y / n` 成为已知量，
    /// 于是只需解 `a`、`b` 两个未知数；下面的 `s_xx` / `s_xy` 等是正规方程
    /// `n·Σx² - (Σx)²` 形式的中心化量，避免大数相减造成精度损失。
    /// 源码中被注释掉的 `a` 与 `c` 是完整解的另外两个系数，此处只需要 `b`。
    ///
    /// # Returns
    /// 与采样轨迹同维度的速度向量；无采样或所有采样时间相同时（`denom == 0`，
    /// 会导致除零）返回零向量。
    pub fn speed(&self) -> Vector {
        if self.movements.is_empty() {
            return Vector::default();
        }
        let n = self.movements.len() as f32;
        let lst = self.movements.back().unwrap().0;
        // 累加 Σt、Σt²、Σt³、Σt⁴ 与对应的 Σ(t^k · y)，即最小二乘正规方程的输入。
        let mut sum_x = 0.;
        let mut sum_x2 = 0.;
        let mut sum_x3 = 0.;
        let mut sum_x4 = 0.;
        let mut sum_y = Point::new(0., 0.);
        let mut sum_x_y = Point::new(0., 0.);
        let mut sum_x2_y = Point::new(0., 0.);
        for (t, pt) in &self.movements {
            let t = t - lst;
            let v = pt.coords;
            let mut w = t;
            sum_y += v;
            sum_x += w;
            sum_x_y += w * v;
            w *= t;
            sum_x2 += w;
            sum_x2_y += w * v;
            w *= t;
            sum_x3 += w;
            sum_x4 += w * t;
        }
        // 中心化（把原点移回样本均值），构成仅含 a、b 的二元正规方程。
        let s_xx = sum_x2 - sum_x * sum_x / n;
        let s_xy = sum_x_y - sum_y * (sum_x / n);
        let s_xx2 = sum_x3 - sum_x * sum_x2 / n;
        let s_x2y = sum_x2_y - sum_y * (sum_x2 / n);
        let s_x2x2 = sum_x4 - sum_x2 * sum_x2 / n;
        let denom = s_xx * s_x2x2 - s_xx2 * s_xx2;
        if denom == 0.0 {
            return Vector::default();
        }
        // let a = (s_x2y * s_xx - s_xy * s_xx2) / denom;
        let b = (s_xy * s_x2x2 - s_x2y * s_xx2) / denom;
        // let c = (sum_y - b * sum_x - a * sum_x2) / n;
        #[allow(clippy::let_and_return)]
        b
    }
}

/// 单轴滚动状态机：负责触摸跟踪、惯性、回弹与吸附。
///
/// 一个 [`Scroll`] 里同时存在 x、y 两个 `Scroller`，但同一时刻只有一个在接收输入
/// （由 [`Scroll::horizontal`] 决定），另一个仅保持状态。
pub struct Scroller {
    /// 当前按住的触摸：`(触点 id, 起始位置, 起始 offset, 是否已解锁为拖动)`。
    ///
    /// 记录 `id` 是为了多指触摸时只跟踪最初按下的那根手指——
    /// 否则第二根手指滑过会让内容「跳」到它的位置；记录起始 offset 则是为了
    /// 拖动位移始终相对按下时刻计算，避免累计误差。
    touch: Option<(u64, f32, f32, bool)>,
    /// 当前滚动偏移：可视区左上角在内容坐标系中的位置，0 表示贴住内容起点。
    pub offset: f32,
    /// 可视区在本轴上的长度（设计坐标）。
    /// 仅用于触摸命中检测（按下点必须落在可视区内），不参与滚动范围计算。
    bound: f32,
    /// 可滚动余量 = 内容长度 - 可视区长度。即 `offset` 的合法上限。
    size: f32,
    /// 当前速度（设计坐标 / 秒），惯性滑行与回弹共用同一个量。
    speed: f32,
    /// 上一次 `update` 的时间戳；用于计算 `dt` 与指数衰减的时长。
    last_time: f32,
    /// 速度采样器，仅在按住期间喂数据。
    tracker: VelocityTracker,
    /// 本帧是否「拉出顶端/左端越界」超过了阈值，供上层做下拉刷新一类交互。
    /// 它在每帧末尾被清空，因此上层必须在收到该标记的同一帧内消费。
    pub pulled: bool,
    /// 本帧是否「拉出底端/右端越界」超过了阈值。
    pub pulled_down: bool,
    /// 本帧是否被触摸过；用于区分「用户松手导致的结束」与「状态被程序清掉」。
    frame_touched: bool,
    /// 吸附步长（如分页、列表项对齐）。`f32::NAN` 表示关闭吸附——
    /// 用 NaN 而不是 `Option<f32>` 只是为了让判断少一层包装，`is_nan()` 即为开关。
    pub step: f32,
    /// 平滑跳转的目标 offset（由 [`Scroller::goto_step`] 设置）。
    /// 与 `step` 的区别是：`step` 是「松手后对齐到网格」，`goto` 是「主动动画到某一页」。
    pub goto: Option<f32>,
}

// 实现语义：默认值与 `new()` 完全一致，提供 Default 只是为了能在
// `#[derive(Default)]` 的结构体里直接放一个 `Scroller` 字段。
impl Default for Scroller {
    fn default() -> Self {
        Self::new()
    }
}

impl Scroller {
    /// 允许越界拉出的最大距离（设计坐标）。0.33 是手感调校值：
    /// 太小会让人觉得「拽不动」，太大则会把内容整段拖出屏幕；
    /// 它同时被 [`Scroller::touch`] 用来做 clamp，以及配合 0.7 / 0.4 两个比例
    /// 推导「是否算拉到底」的阈值。
    pub const EXTEND: f32 = 0.33;

    /// 创建复位到初始状态的滚动器：`offset = 0`、无速度、无吸附（`step = NaN`）。
    ///
    /// `frame_touched` 初值取 `true` 是保守选择——首帧尚未收到任何触摸信息时，
    /// 宁可认为「刚被触摸过」，也不要误判成空闲而触发某些基于该标记的逻辑。
    pub fn new() -> Self {
        Self {
            touch: None,
            offset: 0.,
            bound: 0.,
            size: 0.,
            speed: 0.,
            last_time: 0.,
            tracker: VelocityTracker::empty(),
            pulled: false,
            pulled_down: false,
            frame_touched: true,
            step: f32::NAN,
            goto: None,
        }
    }

    /// 中止当前的触摸跟踪（不触发惯性）。
    ///
    /// 用于外部强制夺回控制权，例如弹窗弹出、触摸被上层拦截时，
    /// 避免残留的 `touch` 状态下一次 `Moved` 事件继续拖动内容。
    pub fn halt(&mut self) {
        self.touch = None;
    }

    /// 平滑滚动到第 `index` 个吸附格。
    ///
    /// 只是设置目标值，真正的动画在 `update` 里由弹簧完成；
    /// 因此若 `step` 为 `NaN`（未启用吸附），这里算出的目标也会是 `NaN`，
    /// 调用方必须先把 `step` 设成有效值。
    pub fn goto_step(&mut self, index: usize) {
        self.goto = Some(self.step * index as f32);
    }

    /// 回到内容起点并清空速度（不改变 `size`/`bound`/`step` 等布局信息）。
    pub fn reset(&mut self) {
        self.offset = 0.;
        self.speed = 0.;
    }

    /// 处理一个触摸事件，返回「本次触摸是否已被判定为拖动」。
    ///
    /// 这个返回值就是「滚动与点击互斥」的实现基础：调用方看到 `true` 就应把这次触摸
    /// 视为滚动、不再把点击交给内容；看到 `false` 则说明按下的手指一直没怎么动，
    /// 应当继续按普通点击处理。
    ///
    /// # Arguments
    /// * `id` — 触点 id，用于在多指场景下只跟踪同一根手指
    /// * `val` — 本轴上的位置（已由 [`Scroll`] 反变换到局部坐标系）
    /// * `t` — 事件时间戳，供速度拟合使用
    ///
    /// # Returns
    /// `true` 表示该触摸已解锁为拖动（越过了 `THRESHOLD`）；
    /// 未跟踪到该 `id` 的触摸一律返回 `false`。
    pub fn touch(&mut self, id: u64, phase: TouchPhase, val: f32, t: f32) -> bool {
        match phase {
            // 阶段一：按下。只有落在可视区内才接管，否则完全不动状态，
            // 让上层有机会把它当作别处的点击处理。
            TouchPhase::Started => {
                if 0. <= val && val < self.bound {
                    self.goto = None;
                    self.tracker.reset();
                    self.tracker.push(t, Point::new(val, 0.));
                    self.speed = 0.;
                    self.touch = Some((id, val, self.offset, false));
                    self.frame_touched = true;
                }
            }
            // 阶段二：按住移动。累积采样，并在位移超过阈值后把 `unlock` 置真；
            // 未解锁前**不移动内容**，这是「点击」与「拖动」互斥的关键——
            // 轻微抖动不会让内容偏移，于是点击不会被误判成拖动。
            TouchPhase::Stationary | TouchPhase::Moved => {
                if let Some((sid, st, st_off, unlock)) = &mut self.touch {
                    if *sid == id {
                        self.tracker.push(t, Point::new(val, 0.));
                        if (*st - val).abs() > THRESHOLD {
                            *unlock = true;
                        }
                        if *unlock {
                            self.offset = (*st_off + (*st - val)).clamp(-Self::EXTEND, self.size + Self::EXTEND);
                        }
                        self.frame_touched = true;
                    }
                }
            }
            // 阶段三：松手/取消。把最后一次位置补进采样，用拟合速度决定是否产生惯性，
            // 再判定是否已被拉到两端尽头，最后清空触摸状态并返回解锁标记。
            TouchPhase::Ended | TouchPhase::Cancelled => {
                if matches!(self.touch, Some((sid, ..)) if sid == id) {
                    self.tracker.push(t, Point::new(val, 0.));
                    let speed = self.tracker.speed().x;
                    // 0.2 的死区：慢速松手不应该产生甩动，否则会出现「明明停住了却自己滑走」。
                    // 乘 0.4 是额外的衰减系数——最小二乘拟合出的是松手瞬间的速度，
                    // 直接采用会让惯性滑得过远，0.4 是让滑行距离接近用户直觉的调校值。
                    // 速度取反是因为 `offset` 与手指位移方向相反（手指上滑 = offset 增大）。
                    if speed.abs() > 0.2 {
                        self.speed = -speed * 0.4;
                        self.last_time = t;
                    }
                    // 两端用不同比例（0.7 / 0.4）判「拉到底」：拉出方向的阈值更高，
                    // 避免用户随手拖一下就被当成「拉到尽头」而触发上层的下拉刷新类交互。
                    if self.offset <= -Self::EXTEND * 0.7 {
                        self.pulled = true;
                    }
                    if self.offset >= self.size + Self::EXTEND * 0.4 {
                        self.pulled_down = true;
                    }
                    let res = self.touch.map(|it| it.3).unwrap_or_default();
                    self.touch = None;
                    self.frame_touched = true;
                    return res;
                }
            }
        }
        self.touch.map(|it| it.3).unwrap_or_default()
    }

    /// 推进一帧的滚动物理。
    ///
    /// # Arguments
    /// * `t` — 当前时间（秒），与 `touch` 使用同一时钟
    /// * `extra_scroll` — 本帧的外部滚动量（滚轮格数），由 [`Scroll::update`] 从
    ///   [`take_wheel`] 取出并换算方向后传入；0 表示没有滚轮输入
    ///
    /// 整体是一套「一阶弹簧 + 指数衰减」的近似物理：每帧先把上一帧算好的速度积分成位移，
    /// 再根据当前状态重新计算速度。这样无需引入真实物理引擎，也能得到惯性滑行、
    /// 越界回弹和吸附三种手感。
    pub fn update(&mut self, t: f32, extra_scroll: f32) {
        // 阶段一：滚轮输入。直接叠加位移，并清零惯性/动画——
        // 若不清理，滚轮与惯性会叠加成不可预期的速度，出现「滚一下却飞很远」。
        self.offset += extra_scroll * WHEEL_STEP;
        if extra_scroll.abs() > 1e-5 {
            self.speed = 0.;
            self.goto = None;
        }

        // 阶段二：用上一帧的速度做一次积分。注意此时 `last_time` 还是上一帧的时间，
        // 所以后面所有 `(t - self.last_time)` 都等于本帧耗时 dt。
        let dt = t - self.last_time;
        self.offset += self.speed * dt;
        // 一阶弹簧的刚度：速度正比于「目标 - 当前位置」。K 越大收敛越快，
        // 4.0 配合下面的半衰期衰减，得到的是一条「越接近越慢」的收敛曲线，
        // 观感上是缓停而不是硬生生的瞬移。
        const K: f32 = 4.;
        let unlock = self.touch.is_some_and(|it| it.3);
        if unlock {
            // 阶段三（手指仍按住）：位置完全由手指决定，速度只做衰减不做弹簧，
            // 否则松手前的采样速度会干扰拖动中的跟随手感。
            self.speed *= (0.5_f32).powf((t - self.last_time) / 0.4);
        } else {
            // 阶段三（无拖动）：先算出「应该停在哪儿」的目标位置。
            // 越界优先停在边界上。
            let mut to = None;
            if self.offset < 0. {
                to = Some(0.);
            }
            if self.offset > self.size {
                to = Some(self.size);
            }
            // 吸附：把 offset 对齐到最近的 step 整数倍。`range` 上加了 1e-3 的容差，
            // 因为首尾两个吸附点在浮点误差下可能刚好算出略微越界的值，
            // 没有容差会导致边界页永远吸不上、来回抖动。
            if !self.step.is_nan() {
                let lower = (self.offset / self.step).floor() * self.step;
                let upper = lower + self.step;
                let range = -1e-3..(self.size + 1e-3);
                if range.contains(&lower) && to.is_none_or(|it| (it - self.offset).abs() >= (lower - self.offset).abs()) {
                    to = Some(lower);
                }
                if range.contains(&upper) && to.is_none_or(|it| (it - self.offset).abs() >= (upper - self.offset).abs()) {
                    to = Some(upper);
                }
            }
            // 主动跳转（`goto`）优先级高于吸附，且用双倍刚度让翻页动画更快；
            // 距离小于 0.01 时立即结束动画，避免为无限接近的尾数持续计算。
            if let Some(to) = self.goto {
                self.speed = (to - self.offset) * K * 2.;
                if (to - self.offset).abs() < 0.01 {
                    self.goto = None;
                }
            } else if let Some(to) = to {
                self.speed = (to - self.offset) * K;
            }
        }
        // 阶段四：决定最终速度。仍处于越界状态就强制回弹并取消动画（此时手指若还按着
        // 也允许越界显示，但一旦松手立刻弹回）；否则（界内且非拖动）让惯性的速度
        // 按半衰期 0.4s 指数衰减——半衰期取 0.4 是因为它既能明显滑行一段，
        // 又能在不到一秒内停下，不会显得"刹不住"。
        if !unlock && self.offset < -1e-3 {
            self.speed = -self.offset * K;
            self.goto = None;
        } else if !unlock && self.offset > self.size + 1e-3 {
            self.speed = (self.size - self.offset) * K;
            self.goto = None;
        } else {
            self.speed *= (0.5_f32).powf((t - self.last_time) / 0.4);
        }
        self.last_time = t;
        // 阶段五：`pulled`/`pulled_down` 只在「松手的那一帧」有效，
        // 必须在这里清掉，否则上层会反复看到读取请求。
        self.pulled = false;
        self.pulled_down = false;
        self.frame_touched = false;
    }

    /// 设置本轴可视区的长度，用于判定触摸是否落在滚动区域内。
    ///
    /// 命名容易误解：`bound` 存的其实是「可视区尺寸」而不是「滚动边界」，
    /// 滚动范围由 [`Scroller::size`] 决定。
    pub fn bound(&mut self, bound: f32) {
        self.bound = bound;
    }

    /// 设置可滚动余量（内容长度 - 可视区长度），即 `offset` 的合法上限。
    ///
    /// 由 [`Scroll::render`] 在量出内容尺寸后写入；内容比可视区短时应当传 0 而非负数，
    /// 否则回弹的目标会变成反向越界。
    pub fn size(&mut self, size: f32) {
        self.size = size;
    }
}

/// 滚动内容的裁剪方式。
///
/// 三种方式对应不同场景，之所以都要保留而不是只留一种：
/// - `None`：内容本来就不会超出可视区（或溢出可以接受）时完全不裁剪，
///   省掉一次 scissor 设置/材质切换，成本最低；
/// - `Scissor`：交给 GPU 的 scissor 矩形裁剪，直角、零着色器开销，是默认值
///   （绝大多数滚动列表就是直角矩形）；但它会打断绘制批次，且无法表现圆角；
/// - `Clip`：走 [`clip_rounded_rect`] 材质做逐像素 SDF 裁剪，能裁出圆角、
///   也能保持原有的绘制顺序，代价是每个像素一次距离计算。
///
/// 需要用圆角时只能选 `Clip`——固定管线的 scissor 在硬件层面就只支持矩形。
pub enum ClipType {
    /// 不裁剪。
    None,
    /// 用 GPU scissor 做矩形裁剪（直角、免费，但可能打断批处理）。
    Scissor,
    /// 用自定义材质做圆角矩形裁剪（可表现圆角，逐像素开销）。
    Clip,
}

/// 可滚动容器：把内容画在可平移的坐标系里，并负责输入命中与裁剪。
pub struct Scroll {
    /// 水平滚动器。即使 [`Scroll::horizontal`] 为 `false` 也始终存在，
    /// 只是不接收输入；`pub` 是因为存在需要直接读写其 `offset` 的调用方
    /// （例如把曲线图定位到某个关注点）。
    pub x_scroller: Scroller,
    /// 垂直滚动器，语义同上。
    pub y_scroller: Scroller,
    /// 可视区尺寸 `(宽, 高)`，由调用方通过 [`Scroll::size`] 在 `render` 前设置。
    size: (f32, f32),
    /// 当前 UI 变换矩阵的**逆**：把输入坐标（与 `mouse_position` 同空间）
    /// 映射回本容器的局部设计坐标。`None` 表示还没有渲染过、区域未知，
    /// 此时一切输入都判定为不命中。
    matrix: Option<Matrix>,
    /// 是否为水平单轴滚动。`true` 时只有 x 轴生效、只取输入的 x 分量；
    /// `false`（默认）则只取 y 分量。不存在「同时两轴滚动」的模式。
    horizontal: bool,
    /// 裁剪方式，见 [`ClipType`]。
    clip: ClipType,
}

// 实现语义：与 `new()` 一致，仅用于让包含 `Scroll` 字段的结构体能 `#[derive(Default)]`。
impl Default for Scroll {
    fn default() -> Self {
        Self::new()
    }
}

impl Scroll {
    /// 创建默认的滚动容器：尺寸暂定 `(2., 2.)`、垂直滚动、scissor 裁剪。
    ///
    /// 初始尺寸取 2 是「先假设占满一屏」的占位值——真实的可视区尺寸要等调用方
    /// 在布局完成后调用 `size()` 才知道；给一个足够大的初值可以避免首帧
    /// 因为尺寸为 0 而把所有输入判定为不命中。
    pub fn new() -> Self {
        Self {
            x_scroller: Scroller::new(),
            y_scroller: Scroller::new(),
            size: (2., 2.),
            matrix: None,
            horizontal: false,
            clip: ClipType::Scissor,
        }
    }

    /// builder 风格地设置裁剪方式。
    pub fn use_clip(mut self, clip: ClipType) -> Self {
        self.clip = clip;
        self
    }

    /// builder 风格地切换为水平滚动模式。
    pub fn horizontal(mut self) -> Self {
        self.horizontal = true;
        self
    }

    /// 直接设置两轴的偏移，并**不**触发惯性或吸附动画。
    /// 用于程序化定位（例如分析面板把相关系数曲线居中到推荐值）。
    pub fn set_offset(&mut self, x: f32, y: f32) {
        self.x_scroller.offset = x;
        self.y_scroller.offset = y;
    }

    /// 把一次触摸交给本容器处理，返回「是否已判定为滚动拖动」。
    ///
    /// 流程：先用保存的逆矩阵把触摸的屏幕坐标变换到局部坐标系，
    /// 再交给对应轴的 [`Scroller`]。三处判定值得注意：
    /// - `matrix` 为 `None`（尚未渲染过）时直接返回 `false`，不做任何猜测；
    /// - 只对 `Started` 做区域命中检测，后续 `Moved`/`Ended` 一律转发——
    ///   否则手指滑出区域边界就会中断拖动，手感非常突兀；
    /// - 命中判定用 `>` 而非 `>=`，与 [`Scroll::contains`] 的闭开区间一致，
    ///   避免相邻两个区域在边界线上同时命中。
    ///
    /// # Returns
    /// 该触摸是否已经被判定为拖动（即是否应当吞掉、不再作为点击传给内容）。
    pub fn touch(&mut self, touch: &Touch, t: f32) -> bool {
        let Some(matrix) = self.matrix else {
            return false;
        };
        let pt = touch.position;
        let pt = matrix.transform_point(&Point::new(pt.x, pt.y));
        if touch.phase == TouchPhase::Started && (pt.x < 0. || pt.y < 0. || pt.x > self.size.0 || pt.y > self.size.1) {
            return false;
        }
        if self.horizontal {
            self.x_scroller.touch(touch.id, touch.phase, pt.x, t)
        } else {
            self.y_scroller.touch(touch.id, touch.phase, pt.y, t)
        }
    }

    /// 推进一帧：处理滚轮输入并更新滚动器的物理状态。
    ///
    /// 滚轮只在该容器「包含当前鼠标位置」时才被消费，否则滚轮会同时被页面上所有
    /// `Scroll` 抢走，出现滚动多个列表的 bug。
    ///
    /// 坐标换算链条（容易看错，故写明）：`mouse_position()` 给的是窗口像素坐标
    /// （原点左上、y 向下），`get_viewport()` 给的是 OpenGL 视口 `(x, y, w, h)`
    /// （原点左下），所以第二行用 `screen_height() - (vp.1 + vp.3)` 做一次 y 翻转；
    /// 再除以视口尺寸乘 2 减 1，得到与 UI 变换同一套的归一化坐标
    /// （x ∈ [-1, 1]，y 再除以宽高比做非等比修正），最后乘逆矩阵回到局部坐标。
    /// 这条链条与 `Scroll::touch` 中的变换必须保持一致，否则鼠标与手指的命中区域会错位。
    pub fn update(&mut self, t: f32) {
        let extra_scroll = if let Some(matrix) = self.matrix {
            let (mx, my) = mouse_position();
            let vp = crate::ext::get_viewport();
            let pt = Point::new(
                (mx - vp.0 as f32) / vp.2 as f32 * 2. - 1.,
                ((my - (screen_height() - (vp.1 + vp.3) as f32)) / vp.3 as f32 * 2. - 1.) / (vp.2 as f32 / vp.3 as f32),
            );
            let pt = matrix.transform_point(&pt);
            if pt.x < 0. || pt.y < 0. || pt.x > self.size.0 || pt.y > self.size.1 {
                0.
            } else {
                // 滚轮方向与内容移动方向相反：向下滚（y 为正）时内容应上移、offset 增大。
                let (x, y) = take_wheel();
                if self.horizontal {
                    -x
                } else {
                    -y
                }
            }
        } else {
            0.
        };
        (if self.horizontal { &mut self.x_scroller } else { &mut self.y_scroller }).update(t, extra_scroll)
    }

    /// 判断一个触摸位置是否落在本容器的可视区内（不做任何状态修改）。
    ///
    /// 与 [`Scroll::touch`] 的命中条件一致：闭开区间 `[0, size)`，
    /// 用于上层在不消费事件的前提下「预检」某个坐标属于哪个区域（例如决定
    /// 手势应交给谁）。`matrix` 为 `None` 时恒为 `false`。
    pub fn contains(&self, touch: &Touch) -> bool {
        self.matrix.is_some_and(|mat| {
            let Vec2 { x, y } = touch.position;
            let p = mat.transform_point(&Point::new(x, y));
            !(p.x < 0. || p.x >= self.size.0 || p.y < 0. || p.y >= self.size.1)
        })
    }

    /// 渲染内容并把内容量出的尺寸反馈给滚动器。
    ///
    /// `content` 闭包在**已被平移过**的坐标系里绘制，其返回值应当是内容自身的
    /// `(宽, 高)`（而不是可视区尺寸）——这是本函数最重要也最容易被误用的隐式约定：
    /// 返回值决定「还能滚多远」，返回可视区尺寸等于不可滚动，返回 0 会得到负的余量。
    ///
    /// 流程：先记录当前 UI 变换的逆矩阵（供后续输入命中使用），
    /// 再按 [`ClipType`] 选择裁剪方式执行闭包，最后用内容尺寸减可视区尺寸得到
    /// 可滚动余量。两次 `.max(0.)` 保证内容比可视区短时余量为 0 而不是负数——
    /// 负数会让越界回弹的目标跑到反向去，表现为内容被顶到屏幕外。
    ///
    /// # Panics
    /// 若当前 `ui.transform` 不可逆（退化的缩放/投影矩阵）会在 `unwrap` 处 panic；
    /// 正常布局下变换始终是可逆的仿射变换。
    pub fn render(&mut self, ui: &mut Ui, content: impl FnOnce(&mut Ui) -> (f32, f32)) {
        self.matrix = Some(ui.transform.try_inverse().unwrap());
        let func = |ui: &mut Ui| ui.with(Translation2::new(-self.x_scroller.offset, -self.y_scroller.offset).to_homogeneous(), content);
        let s = match self.clip {
            ClipType::None => func(ui),
            ClipType::Scissor => ui.scissor(self.rect(), func),
            ClipType::Clip => clip_rounded_rect(ui, self.rect(), 0., func),
        };
        self.x_scroller.size((s.0 - self.size.0).max(0.));
        self.y_scroller.size((s.1 - self.size.1).max(0.));
    }

    /// 返回上一次 `render` 记录的逆变换，供外部做坐标换算（如判断点击落在哪个条目上）。
    pub fn matrix(&self) -> Option<Matrix> {
        self.matrix
    }

    /// 覆盖逆变换矩阵；传 `None` 可让容器暂时「失活」（所有输入判定为不命中）。
    ///
    /// 用于外层容器在自身被遮挡/禁用时屏蔽内部滚动区域，比逐个禁用更省事。
    pub fn set_matrix(&mut self, matrix: Option<Matrix>) {
        self.matrix = matrix;
    }

    /// 设置可视区尺寸，并同步给两个滚动器作为触摸命中范围。
    ///
    /// 必须在 `render` 之前调用，否则本帧的 `content` 会按旧尺寸被裁剪；
    /// 注意这里只影响**命中范围**，可滚动余量仍由 `render` 按内容实测值计算。
    pub fn size(&mut self, size: (f32, f32)) {
        self.size = size;
        self.x_scroller.bound(size.0);
        self.y_scroller.bound(size.1);
    }

    /// 可视区矩形，坐标以容器自身左上角为原点（用于 scissor 与材质裁剪）。
    pub fn rect(&self) -> Rect {
        Rect::new(0., 0., self.size.0, self.size.1)
    }
}
